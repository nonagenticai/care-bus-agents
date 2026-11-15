//! A tenant's own LLM provider, read from the database by the bus binaries.
//!
//! The interaction daemon runs the customer-care turns, so it has to honour the provider the tenant
//! configured at `/admin/providers` — the same rows `carebus-api` reads in `chat_q::qualifier_for`,
//! reached the same way: one transaction with `app.current_tenant` set transaction-locally
//! (`set_config(…, true)`), so row-level security confines every row to that tenant and nothing
//! survives the commit on the pooled connection.
//!
//! The connection must be the RLS-enforced `carebus_app` role (`DATABASE_APP_URL`), never the owner.

use uuid::Uuid;

use crate::provider_secret::{self, Secrets};

/// A provider the caller can call. `Debug` never prints the token.
#[derive(Clone)]
pub struct BoundProvider {
    pub id: Uuid,
    pub base_url: String,
    /// The model the binding names, if any; the caller applies its own default.
    pub model: Option<String>,
    /// The usable token: empty when the row stores none (a provider that needs no bearer).
    pub token: String,
    /// The row's monotonic version (0032): a client built for an older version is stale.
    pub version: i64,
}

impl std::fmt::Debug for BoundProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundProvider")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("version", &self.version)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// The tenant's binding for `use_name` (`qualify`, `gates`, `research`, `research_write`), or `None`
/// when it has none, the provider is soft-deleted, or the binding names no provider.
///
/// An `Err` means a binding exists but cannot be used (its secret does not open): the caller must
/// fail rather than silently fall back to another model, exactly as `carebus-api` does.
pub async fn resolve(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    use_name: &str,
    secrets: Secrets<'_>,
) -> anyhow::Result<Option<BoundProvider>> {
    type Row = (
        Uuid,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
    );
    let row: Option<Row> = {
        let mut tx = pool.begin().await?;
        sqlx::query(
            "SELECT set_config('app.current_tenant', $1, true), set_config('app.current_job', '', true)",
        )
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query_as(
            "SELECT p.id, p.base_url, p.secret_enc, p.secret_sealed, u.model, p.version
               FROM llm_provider_uses u
               JOIN llm_providers p ON p.tenant_id = u.tenant_id AND p.id = u.provider_id
              WHERE u.\"use\" = $1 AND p.kind = 'openai' AND p.deleted_at IS NULL",
        )
        .bind(use_name)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row
    };
    let Some((id, base_url, secret_enc, secret_sealed, model, version)) = row else {
        return Ok(None);
    };
    let token = provider_secret::open(
        secrets,
        tenant,
        id,
        secret_sealed.as_deref(),
        secret_enc.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?
    .unwrap_or_default();
    Ok(Some(BoundProvider {
        id,
        base_url,
        model,
        token,
        version,
    }))
}

/// The platform default (0033's singleton row), or `None` when it has not been seeded.
///
/// The platform model is deployment configuration, not tenant input: its `base_url` is the internal
/// inference gateway, so it is **not** run through [`crate::netguard`] (which refuses every
/// cluster-internal name). A tenant provider is.
///
/// A missing table is `None`: carebus-api runs the migrations and the two images roll
/// independently, so the daemon may briefly see a database without 0033. Any other error is real.
pub async fn platform_default(
    pool: &sqlx::PgPool,
    secrets: Secrets<'_>,
) -> anyhow::Result<Option<BoundProvider>> {
    type Row = (String, String, Option<String>, i64);
    let row: Option<Row> = sqlx::query_as(
        "SELECT base_url, model, secret_sealed, version FROM platform_llm_default WHERE id",
    )
    .fetch_optional(pool)
    .await
    .or_else(|e| if missing_table(&e) { Ok(None) } else { Err(e) })?;
    let Some((base_url, model, sealed, version)) = row else {
        return Ok(None);
    };
    let token = provider_secret::open_platform(secrets, sealed.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .unwrap_or_default();
    Ok(Some(BoundProvider {
        id: Uuid::nil(),
        base_url,
        model: Some(model),
        token,
        version,
    }))
}

/// SQLSTATE 42P01: the relation does not exist — a database older than the binary.
fn missing_table(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("42P01"))
}

/// The runtime pool from `DATABASE_APP_URL` (or `CAREBUS_DATABASE_APP_URL`), or `None` when neither
/// is set (a local REPL without a database: the qualifier then runs on keyword rules).
///
/// Refuses a role that bypasses RLS, exactly as `carebus-api` and the researcher do: every read
/// below relies on row-level security to confine it to one tenant.
pub async fn pool_from_env() -> anyhow::Result<Option<sqlx::PgPool>> {
    use anyhow::Context;
    let url = ["DATABASE_APP_URL", "CAREBUS_DATABASE_APP_URL"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()));
    let Some(url) = url else { return Ok(None) };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .context("connect DATABASE_APP_URL (runtime role)")?;
    let bypass: bool = sqlx::query_scalar(
        "SELECT r.rolsuper OR r.rolbypassrls OR EXISTS (
                    SELECT 1 FROM pg_tables t WHERE t.schemaname = 'public' AND t.tableowner = current_user)
           FROM pg_roles r WHERE r.rolname = current_user",
    )
    .fetch_one(&pool)
    .await
    .context("read the runtime role's attributes")?;
    if bypass {
        anyhow::bail!(
            "DATABASE_APP_URL connects as a role that bypasses RLS (superuser, BYPASSRLS or table owner); \
             it must be carebus_app"
        );
    }
    Ok(Some(pool))
}

/// Seed the platform default once, from the bootstrap env, when the row is absent.
///
/// `CAREBUS_LLM_BASE_URL` / `CAREBUS_MODEL` / `VLLM_LICENSE_TOKEN` are read here and nowhere else
/// after this: the row is the runtime source of truth. `ON CONFLICT DO NOTHING` makes a concurrent
/// boot harmless and can never overwrite a super_admin's edit.
pub async fn ensure_platform_default(
    pool: &sqlx::PgPool,
    secrets: Secrets<'_>,
) -> anyhow::Result<bool> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let Some(token) = get("VLLM_LICENSE_TOKEN") else {
        return Ok(false);
    };
    if secrets.keyring.is_none() {
        tracing::warn!(
            "VLLM_LICENSE_TOKEN is set but PROVIDER_SECRET_KEYS is not: the platform default cannot be seeded"
        );
        return Ok(false);
    }
    let base_url = get("CAREBUS_LLM_BASE_URL")
        .unwrap_or_else(|| crate::interaction::DEFAULT_LLM_BASE_URL.to_string());
    let model =
        get("CAREBUS_MODEL").unwrap_or_else(|| crate::interaction::DEFAULT_MODEL.to_string());
    let stored =
        provider_secret::seal_platform(secrets, &token).map_err(|e| anyhow::anyhow!("{e}"))?;
    let done = sqlx::query(
        "INSERT INTO platform_llm_default
             (base_url, model, secret_sealed, secret_fpr, secret_updated_at, updated_by)
         VALUES ($1, $2, $3, $4, now(), 'bootstrap')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&base_url)
    .bind(&model)
    .bind(&stored.sealed)
    .bind(&stored.fingerprint)
    .execute(pool)
    .await;
    match done {
        Ok(r) => {
            if r.rows_affected() == 1 {
                tracing::info!(
                    base_url,
                    model,
                    "platform default seeded from the bootstrap env"
                );
            }
            Ok(r.rows_affected() == 1)
        }
        // A database older than 0033 while the two images roll: not an error, seed later.
        Err(e) if missing_table(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_provider_never_prints_its_token() {
        let p = BoundProvider {
            id: Uuid::new_v4(),
            base_url: "https://gpu.example.com/v1".into(),
            model: Some("their-model".into()),
            token: "super-secret".into(),
            version: 3,
        };
        let shown = format!("{p:?}");
        assert!(!shown.contains("super-secret"));
        assert!(shown.contains("[redacted]"));
    }
}
