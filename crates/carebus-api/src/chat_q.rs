//! The qualifier carebus-api uses for chat, resolved per tenant on every turn ([`qualifier_for`]):
//! 1. the tenant's `qualify` binding (`llm_provider_uses` → an `openai` provider in `llm_providers`, token decrypted
//!    with `CAREBUS_SOURCE_KEY`), cached per tenant and keyed by (provider id, provider `version`, model);
//! 2. no binding: [`PlatformQualifier`] — migration 0033's singleton row, seeded once from the old env;
//! 3. no binding and no row: keyword rules.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use carebus::interaction::{
    DEFAULT_MODEL, Draft, Extraction, Qualifier, RigQualifier, RuleQualifier, StartData,
};
use carebus::spaces::SpaceCatalog;

use futures_util::future::BoxFuture;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The platform default (0033), built per row version and cached: a super_admin's PUT is picked up
/// on the next turn without a restart.
///
/// The platform model is deployment configuration, not tenant input: its base_url is the internal
/// inference gateway, so it is deliberately **not** run through `crate::netguard` (which refuses
/// every cluster-internal name). A tenant's provider is. `fixed` is the test override — when set,
/// the row is never read and this is exactly the qualifier returned.
pub struct PlatformQualifier {
    fixed: Option<Arc<AnyQualifier>>,
    cache: Mutex<Option<(i64, Arc<AnyQualifier>)>>,
    service_actor: Option<Uuid>,
}

impl PlatformQualifier {
    pub fn new(service_actor: Option<Uuid>) -> Self {
        Self {
            fixed: None,
            cache: Mutex::new(None),
            service_actor,
        }
    }

    /// Tests: a fixed qualifier instead of the row (rules, a recording stub, …).
    pub fn fixed(q: Arc<AnyQualifier>) -> Self {
        Self {
            fixed: Some(q),
            cache: Mutex::new(None),
            service_actor: None,
        }
    }

    /// `AppState::platform` is an `Arc`; tests install a fixed qualifier by replacing that Arc.
    pub fn test_fixed(q: AnyQualifier) -> Arc<Self> {
        Arc::new(Self::fixed(Arc::new(q)))
    }

    /// The fixed qualifier, when this was built for a test (pointer-identity assertions).
    pub fn fixed_qualifier(&self) -> Option<Arc<AnyQualifier>> {
        self.fixed.clone()
    }

    /// Drop the cached build: the row changed under us (`PUT /api/providers/platform`).
    pub fn forget(&self) {
        *self.cache.lock().expect("platform qualifier cache") = None;
    }

    /// The platform default for the next turn. No row (or a database that predates 0033) is the
    /// keyword rules, never an error.
    pub async fn resolve(&self, st: &AppState) -> ApiResult<Arc<AnyQualifier>> {
        if let Some(q) = &self.fixed {
            return Ok(q.clone());
        }
        let row: Option<(String, String, Option<String>, i64)> = sqlx::query_as(
            "SELECT base_url, model, secret_sealed, version FROM platform_llm_default WHERE id",
        )
        .fetch_optional(&st.db)
        .await
        .or_else(|e| {
            if matches!(&e, sqlx::Error::Database(d) if d.code().as_deref() == Some("42P01")) {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
        let Some((base_url, model, sealed, version)) = row else {
            return Ok(rules());
        };
        if let Some((_, q)) = self
            .cache
            .lock()
            .expect("platform qualifier cache")
            .as_ref()
            .filter(|(v, _)| *v == version)
        {
            return Ok(q.clone());
        }
        let token = crate::provider_secret::open_platform(st.cfg.secrets(), sealed.as_deref())?
            .unwrap_or_default();
        let q = Arc::new(AnyQualifier::Rig(Box::new(
            RigQualifier::with_service_actor(token, base_url, model, self.service_actor)
                .map_err(ApiError::Internal)?,
        )));
        let mut cache = self.cache.lock().expect("platform qualifier cache");
        if cache.as_ref().is_none_or(|(v, _)| version >= *v) {
            *cache = Some((version, q.clone()));
        }
        Ok(q)
    }
}

fn rules() -> Arc<AnyQualifier> {
    Arc::new(AnyQualifier::Rules(RuleQualifier))
}

/// What a cached tenant qualifier was built from; any change rebuilds it. `version` (0032) is
/// bumped inside every provider UPDATE, so it orders two concurrent admins' writes correctly;
/// `updated_at` is the transaction's start time and can commit out of order.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BuiltFrom {
    provider: Uuid,
    version: i64,
    model: Option<String>,
}

/// Per-tenant qualifiers built from a `qualify` binding.
#[derive(Default)]
pub struct TenantQualifiers {
    cache: Mutex<HashMap<Uuid, (BuiltFrom, Arc<AnyQualifier>)>>,
}

impl TenantQualifiers {
    fn get(&self, tenant: Uuid, from: &BuiltFrom) -> Option<Arc<AnyQualifier>> {
        let c = self.cache.lock().expect("qualifier cache lock");
        c.get(&tenant)
            .filter(|(f, _)| f == from)
            .map(|(_, q)| q.clone())
    }
    fn put(&self, tenant: Uuid, from: BuiltFrom, q: Arc<AnyQualifier>) {
        self.cache
            .lock()
            .expect("qualifier cache lock")
            .insert(tenant, (from, q));
    }
    fn forget(&self, tenant: Uuid) {
        self.cache
            .lock()
            .expect("qualifier cache lock")
            .remove(&tenant);
    }
}

/// The qualifier for one turn in `tenant`: its `qualify` binding, else [`PlatformQualifier`].
/// A bound provider whose token cannot be used is an error, never a silent fallback to another model.
pub async fn qualifier_for(st: &AppState, tenant: Uuid) -> ApiResult<Arc<AnyQualifier>> {
    type Bound = (
        Uuid,
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
    );
    let row: Option<Bound> = {
        let mut tx = crate::tenancy::begin_scoped(&st.db, tenant, None).await?;
        sqlx::query_as(
            "SELECT p.id, p.base_url, p.secret_enc, p.secret_sealed, p.version, u.model
               FROM llm_provider_uses u
               JOIN llm_providers p ON p.tenant_id = u.tenant_id AND p.id = u.provider_id
              WHERE u.\"use\" = 'qualify' AND p.kind = 'openai' AND p.deleted_at IS NULL",
        )
        .fetch_optional(&mut *tx)
        .await?
    };
    let Some((provider, base_url, secret_enc, secret_sealed, version, model)) = row else {
        st.qualifiers.forget(tenant);
        return st.platform.resolve(st).await;
    };
    let from = BuiltFrom {
        provider,
        version,
        model: model.clone(),
    };
    if let Some(q) = st.qualifiers.get(tenant, &from) {
        return Ok(q);
    }
    let token = crate::provider_secret::open(
        st.cfg.secrets(),
        tenant,
        provider,
        secret_sealed.as_deref(),
        secret_enc.as_deref(),
    )?
    .unwrap_or_default();
    let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    // The stored base_url passed the literal guard on save; per cache entry its host is resolved, every answer
    // checked and the client pinned to the checked address, with no redirects and no proxy (crate::netguard).
    // The pin lives as long as the cache entry (until the provider or binding changes).
    let builder = crate::netguard::check_and_pin(&base_url, st.cfg.source_fetch_allow_loopback)
        .await
        .map(|(_, b)| b)
        .map_err(|r| {
            ApiError::Internal(anyhow::anyhow!(
                "the qualify provider's base_url is not callable: {}",
                r.0
            ))
        })?;
    let http = builder
        .timeout(carebus::interaction::LLM_TIMEOUT)
        .build()
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("building the qualify client: {e}")))?;
    let q = Arc::new(AnyQualifier::Rig(Box::new(RigQualifier::with_client(
        token, base_url, model, http,
    ))));
    st.qualifiers.put(tenant, from, q.clone());
    Ok(q)
}

/// A side effect run by [`AnyQualifier::Then`] (test hook).
pub type Hook = std::sync::Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

pub enum AnyQualifier {
    Rules(RuleQualifier),
    Rig(Box<RigQualifier>),
    /// Test hook: rules after a delay, so a request can be cancelled mid-qualification.
    Slow(std::time::Duration),
    /// Test hook: the model is down; every extraction fails.
    Failing,
    /// Test hook: rules, then a side effect (e.g. break a Redis key mid-turn).
    Then(Hook),
    /// Test hook: records every text it is asked to qualify, then rules after a delay.
    Record(
        std::time::Duration,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ),
    /// Test hook: rules, recording this model usage as the rig extractor would (plan 15 metering).
    Metered(carebus::interaction::LlmUsage),
    /// Test hook: rules after a delay for the first `n` calls, then every extraction fails.
    FailAfter(
        usize,
        std::time::Duration,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ),
}

impl Qualifier for AnyQualifier {
    async fn extract(
        &self,
        text: &str,
        draft: Option<&Draft>,
        start: &StartData,
        spaces: &SpaceCatalog,
    ) -> Result<Extraction> {
        match self {
            AnyQualifier::Rules(q) => q.extract(text, draft, start, spaces).await,
            AnyQualifier::Rig(q) => q.extract(text, draft, start, spaces).await,
            AnyQualifier::Slow(d) => {
                tokio::time::sleep(*d).await;
                RuleQualifier.extract(text, draft, start, spaces).await
            }
            AnyQualifier::Failing => anyhow::bail!("the model endpoint did not answer"),
            AnyQualifier::Metered(u) => {
                carebus::interaction::record_llm_usage(u.clone());
                RuleQualifier.extract(text, draft, start, spaces).await
            }
            AnyQualifier::Then(hook) => {
                let ex = RuleQualifier.extract(text, draft, start, spaces).await;
                hook().await;
                ex
            }
            AnyQualifier::FailAfter(n, d, calls) => {
                tokio::time::sleep(*d).await;
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= *n {
                    anyhow::bail!("the model endpoint did not answer");
                }
                RuleQualifier.extract(text, draft, start, spaces).await
            }
            AnyQualifier::Record(d, seen) => {
                seen.lock().expect("record lock").push(text.to_string());
                tokio::time::sleep(*d).await;
                RuleQualifier.extract(text, draft, start, spaces).await
            }
        }
    }
}
