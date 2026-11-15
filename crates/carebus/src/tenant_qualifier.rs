//! The interaction daemon's qualifier: the tenant's own `qualify` provider, else the platform
//! default, else the keyword rules.
//!
//! The row is read per turn (one indexed lookup) so a rotation made at `/admin/providers` takes
//! effect on the next message; the built rig client is cached per `(provider, version)`, because it
//! owns a reqwest client and a DNS pin and rebuilding it per turn would discard keep-alive and redo
//! the lookup for every message.
//!
//! A tenant provider's base_url passed `netguard` on save and is pinned again here before every
//! build (public hosts only; a name that resolves private is refused, not called). The platform
//! default is deployment configuration — its base_url is the internal inference gateway — so it is
//! built without the guard, exactly as `carebus-api` does, and it carries care-bus' identity
//! headers through [`RigQualifier::with_service_actor`].

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::interaction::{
    DEFAULT_MODEL, Draft, Extraction, LLM_TIMEOUT, Qualifier, RigQualifier, RuleQualifier,
    StartData,
};
use crate::netguard;
use crate::provider_db::{self, BoundProvider};
use crate::provider_secret::SecretsOwned;
use crate::spaces::SpaceCatalog;

/// What a cached client was built from. `version` is monotonic per row (0032), so a row read before
/// a concurrent admin's write can never displace the newer one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CacheKey {
    id: Uuid,
    version: i64,
    base_url: String,
    model: String,
}

struct Cached {
    key: CacheKey,
    qualifier: Arc<RigQualifier>,
}

pub struct TenantQualifier {
    /// `None` when `DATABASE_APP_URL` is unset (a local REPL): keyword rules only.
    pool: Option<sqlx::PgPool>,
    tenant: Uuid,
    secrets: SecretsOwned,
    /// `SERVICE_ACTOR_ID`, sent as `X-Actor-Id` on platform-model calls (never on a tenant's own).
    service_actor: Option<Uuid>,
    cache: Mutex<Option<Cached>>,
}

impl TenantQualifier {
    pub fn new(
        pool: Option<sqlx::PgPool>,
        tenant: Uuid,
        secrets: SecretsOwned,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            pool,
            tenant,
            secrets,
            service_actor: crate::identity::service_actor_from_env()?,
            cache: Mutex::new(None),
        })
    }

    /// The tenant's binding for `qualify`, else the platform default; `None` when neither exists.
    /// An `Err` is a binding that exists but cannot be used — never a silent fallback.
    async fn bound(&self) -> anyhow::Result<Option<BoundProvider>> {
        let Some(pool) = &self.pool else {
            return Ok(None);
        };
        let secrets = self.secrets.borrow();
        match provider_db::resolve(pool, self.tenant, "qualify", secrets).await? {
            Some(p) => Ok(Some(p)),
            None => provider_db::platform_default(pool, secrets).await,
        }
    }

    async fn client_for(&self, p: &BoundProvider) -> anyhow::Result<Arc<RigQualifier>> {
        let model = p.model.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let key = CacheKey {
            id: p.id,
            version: p.version,
            base_url: p.base_url.clone(),
            model: model.clone(),
        };
        if let Some(hit) = self
            .cache
            .lock()
            .expect("qualifier cache lock")
            .as_ref()
            .filter(|c| c.key == key)
        {
            return Ok(hit.qualifier.clone());
        }
        let built = if p.id.is_nil() {
            // The platform default (0033): internal gateway, care-bus' identity headers.
            RigQualifier::with_service_actor(
                p.token.clone(),
                p.base_url.clone(),
                model,
                self.service_actor,
            )?
        } else {
            // A tenant's provider: public host only, resolved and pinned now, no redirects/proxy.
            let builder = netguard::check_and_pin(&p.base_url, false)
                .await
                .map(|(_, b)| b)
                .map_err(|r| {
                    anyhow::anyhow!("the qualify provider's base_url is not callable: {}", r.0)
                })?;
            let http = builder
                .timeout(LLM_TIMEOUT)
                .build()
                .map_err(|e| anyhow::anyhow!("building the qualify client: {e}"))?;
            RigQualifier::with_client(p.token.clone(), p.base_url.clone(), model, http)
        };
        let q = Arc::new(built);
        {
            let mut cache = self.cache.lock().expect("qualifier cache lock");
            // Never replace a newer build of the same row with an older read; a different provider
            // (a rebind) wins, because it was resolved later than the entry it replaces.
            let stale = cache
                .as_ref()
                .is_some_and(|c| c.key.id == key.id && c.key.version > key.version);
            if !stale {
                *cache = Some(Cached {
                    key,
                    qualifier: q.clone(),
                });
            }
        }
        Ok(q)
    }
}

impl Qualifier for TenantQualifier {
    async fn extract(
        &self,
        text: &str,
        draft: Option<&Draft>,
        start: &StartData,
        spaces: &SpaceCatalog,
    ) -> anyhow::Result<Extraction> {
        match self.bound().await? {
            Some(p) => {
                self.client_for(&p)
                    .await?
                    .extract(text, draft, start, spaces)
                    .await
            }
            None => RuleQualifier.extract(text, draft, start, spaces).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cache_key_orders_by_version_within_one_row() {
        let id = Uuid::new_v4();
        let k = |version| CacheKey {
            id,
            version,
            base_url: "https://p.example/v1".into(),
            model: "m".into(),
        };
        assert_ne!(k(1), k(2));
        assert_eq!(k(2), k(2));
    }
}
