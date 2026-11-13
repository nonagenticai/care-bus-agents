//! The qualifier carebus-api uses for chat, resolved per tenant on every turn ([`qualifier_for`]):
//! 1. the tenant's `qualify` binding (`llm_provider_uses` → an `openai` provider in `llm_providers`, token decrypted
//!    with `CAREBUS_SOURCE_KEY`), cached per tenant and keyed by (provider id, provider `updated_at`, model);
//! 2. no binding: the platform default, the rig extractor from env when `VLLM_LICENSE_TOKEN` is set;
//! 3. no binding and no env: keyword rules.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use carebus::interaction::{
    DEFAULT_LLM_BASE_URL, DEFAULT_MODEL, Draft, Extraction, Qualifier, RigQualifier, RuleQualifier,
    StartData,
};
use carebus::spaces::SpaceCatalog;
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use uuid::Uuid;

use crate::crypto;
use crate::error::{ApiError, ApiResult};
use crate::routes::llm_providers::{PlatformDefaultView, open_token};
use crate::state::AppState;

/// The platform default qualifier LLM as configured by env, for `GET /api/providers` (`platform_default`).
#[derive(Clone, Debug, Default)]
pub struct PlatformLlm {
    /// `Some((base_url, model))` when `VLLM_LICENSE_TOKEN` is set; the token itself is never kept here.
    pub endpoint: Option<(String, String)>,
}

impl PlatformLlm {
    /// Mirrors [`AnyQualifier::from_env`] and `RigQualifier::from_env`.
    pub fn from_env() -> Self {
        if std::env::var("VLLM_LICENSE_TOKEN").is_err() {
            return Self::default();
        }
        let base_url =
            std::env::var("CAREBUS_LLM_BASE_URL").unwrap_or_else(|_| DEFAULT_LLM_BASE_URL.into());
        let model = std::env::var("CAREBUS_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        Self {
            endpoint: Some((base_url, model)),
        }
    }

    pub fn view(&self) -> PlatformDefaultView {
        match &self.endpoint {
            Some((base_url, model)) => PlatformDefaultView {
                base_url: Some(base_url.clone()),
                model: Some(model.clone()),
                configured: true,
            },
            None => PlatformDefaultView {
                base_url: None,
                model: None,
                configured: false,
            },
        }
    }
}

/// What a cached tenant qualifier was built from; any change rebuilds it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BuiltFrom {
    provider: Uuid,
    updated_at: DateTime<Utc>,
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

/// The qualifier for one turn in `tenant`: its `qualify` binding, else the platform default (`st.qualifier`).
/// A bound provider whose token cannot be used is an error, never a silent fallback to another model.
pub async fn qualifier_for(st: &AppState, tenant: Uuid) -> ApiResult<Arc<AnyQualifier>> {
    type Bound = (Uuid, String, Option<String>, DateTime<Utc>, Option<String>);
    let row: Option<Bound> = {
        let mut tx = crate::tenancy::begin_scoped(&st.db, tenant, None).await?;
        sqlx::query_as(
            "SELECT p.id, p.base_url, p.secret_enc, p.updated_at, u.model
               FROM llm_provider_uses u
               JOIN llm_providers p ON p.tenant_id = u.tenant_id AND p.id = u.provider_id
              WHERE u.\"use\" = 'qualify' AND p.kind = 'openai'",
        )
        .fetch_optional(&mut *tx)
        .await?
    };
    let Some((provider, base_url, secret_enc, updated_at, model)) = row else {
        st.qualifiers.forget(tenant);
        return Ok(st.qualifier.clone());
    };
    let from = BuiltFrom {
        provider,
        updated_at,
        model: model.clone(),
    };
    if let Some(q) = st.qualifiers.get(tenant, &from) {
        return Ok(q);
    }
    let token = match secret_enc {
        None => String::new(),
        Some(enc) => open_token(crypto::require(st.cfg.source_key.as_ref())?, provider, &enc)
            .map_err(|_| {
                ApiError::Internal(anyhow::anyhow!(
                    "the qualify provider's stored token cannot be decrypted"
                ))
            })?,
    };
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

impl AnyQualifier {
    pub fn from_env() -> Result<Self> {
        Ok(if std::env::var("VLLM_LICENSE_TOKEN").is_ok() {
            AnyQualifier::Rig(Box::new(RigQualifier::from_env()?))
        } else {
            AnyQualifier::Rules(RuleQualifier)
        })
    }
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
