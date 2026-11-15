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
}

impl std::fmt::Debug for BoundProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundProvider")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
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
    type Row = (Uuid, String, Option<String>, Option<String>, Option<String>);
    let row: Option<Row> = {
        let mut tx = pool.begin().await?;
        sqlx::query(
            "SELECT set_config('app.current_tenant', $1, true), set_config('app.current_job', '', true)",
        )
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query_as(
            "SELECT p.id, p.base_url, p.secret_enc, p.secret_sealed, u.model
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
    let Some((id, base_url, secret_enc, secret_sealed, model)) = row else {
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
    }))
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
        };
        let shown = format!("{p:?}");
        assert!(!shown.contains("super-secret"));
        assert!(shown.contains("[redacted]"));
    }
}
