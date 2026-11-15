//! The platform default's row (migration 0033), read by the library without HTTP.
//!
//! The row is deployment configuration, not tenant data, and carebus-api seeds it once from the
//! bootstrap env at boot. `carebus-api` reads it for chat's fallback qualifier and the researcher
//! for its `DefaultLlm`, so both get the same value a super_admin edits at
//! `PUT /api/providers/platform`.

use uuid::Uuid;

use crate::provider_secret::{self, Secrets};

/// The platform default as a caller uses it. `Debug` never prints the token.
#[derive(Clone)]
pub struct BoundProvider {
    /// Always nil: the platform row has no provider id (a tenant provider does).
    pub id: Uuid,
    pub base_url: String,
    pub model: Option<String>,
    /// The usable token: empty when the row stores none.
    pub token: String,
    /// The row's monotonic version (0033): a client built for an older version is stale.
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

/// The platform default (0033's singleton row), or `None` when it has not been seeded.
///
/// The platform model is deployment configuration: its `base_url` is the internal inference
/// gateway, so it is **not** run through carebus-api's `netguard` (which refuses every
/// cluster-internal name). A tenant's provider is.
///
/// A missing table is `None`: carebus-api runs the migrations and the researcher may start first,
/// so a database without 0033 is a rolling-deploy state, not an error. Any other error is real.
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
            id: Uuid::nil(),
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
