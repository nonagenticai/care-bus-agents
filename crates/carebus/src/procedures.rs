//! PUBLISHED procedures from carebus-api, cached per context space, for one scope (tenant, job).
//!
//! `from_env` builds the unscoped client (URL + token); `for_scope` derives the client one scope's orchestrator
//! uses. It calls `GET {base}/api/jobs/{job}/procedures` and names the tenant with `x-active-tenant-id`, which a
//! service token must send. An unscoped client refuses to plan, so no scope can plan with another's procedures.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::domain::{Plan, ProcedureSource, TOOLS};
use crate::scope::Scope;
use crate::spaces::ContextSpace;
use crate::tools::requirements;

#[derive(Deserialize)]
struct ProcedureVersion {
    name: String,
    version: String,
    version_status: String,
    plan: Plan,
}

/// The header a service token names its tenant with (carebus-api `ACTIVE_TENANT_HEADER`).
pub const ACTIVE_TENANT_HEADER: &str = "x-active-tenant-id";

/// plan 05's list envelope for every list route.
#[derive(Deserialize)]
struct Paged<T> {
    items: Vec<T>,
    #[allow(dead_code)]
    next: Option<String>,
}

pub struct HttpProcedures {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
    scope: Option<Scope>,
    pub ttl: Duration,
    /// By context-space key.
    cache: Mutex<HashMap<String, (Instant, Plan)>>,
}

impl HttpProcedures {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            scope: None,
            ttl: Duration::from_secs(60),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Result<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// `from_env` over any lookup. `CAREBUS_API_URL` unset or empty is an error;
    /// an empty `CAREBUS_API_TOKEN` means no token.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let url = crate::required_var("CAREBUS_API_URL", get("CAREBUS_API_URL"))?;
        let token = get("CAREBUS_API_TOKEN").filter(|t| !t.is_empty());
        Ok(Self::new(url, token))
    }

    /// The same API, URL, token and TTL for scope `s`, with its own (empty) cache.
    pub fn for_scope(&self, s: &Scope) -> HttpProcedures {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            token: self.token.clone(),
            scope: Some(s.clone()),
            ttl: self.ttl,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }
}

impl ProcedureSource for HttpProcedures {
    async fn plan(&self, space: &ContextSpace) -> Result<Plan> {
        let scope = self
            .scope
            .as_ref()
            .context("procedures client is not scoped")?;
        let key = space.key.as_str();
        if let Some((at, plan)) = self.cache.lock().await.get(key)
            && at.elapsed() < self.ttl
        {
            return Ok(plan.clone());
        }
        let mut req = self
            .http
            .get(format!(
                "{}/api/jobs/{}/procedures",
                self.base_url, scope.job
            ))
            .header(ACTIVE_TENANT_HEADER, scope.tenant.to_string())
            .query(&[("context_space", key), ("status", "PUBLISHED")]);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let versions: Paged<ProcedureVersion> = req
            .send()
            .await
            .context("carebus-api unreachable")?
            .error_for_status()
            .context("carebus-api refused the procedure lookup")?
            .json()
            .await
            .context("procedure list is not the expected shape")?;
        let v = versions
            .items
            .into_iter()
            .find(|v| v.version_status == "PUBLISHED")
            .with_context(|| format!("no PUBLISHED procedure for {key}"))?;
        validate_plan(&v.plan).with_context(|| {
            format!(
                "procedure {} {} for {key} is not runnable",
                v.name, v.version
            )
        })?;
        tracing::debug!(procedure = %v.name, version = %v.version, %key, %scope, "procedure loaded");
        // Always the version this lookup chose, never a `procedure` key an author put in the stored plan.
        let mut plan = v.plan;
        plan.procedure = Some(format!("{}@{}", v.name, v.version));
        self.cache
            .lock()
            .await
            .insert(key.to_string(), (Instant::now(), plan.clone()));
        Ok(plan)
    }
}

/// A plan the bus can run: at least one phase, and every step names a worker that has a
/// consumer group and an action that worker knows. Anything else would publish work nobody
/// answers (the case hangs) or complete a case with no facts.
pub fn validate_plan(plan: &Plan) -> Result<()> {
    anyhow::ensure!(!plan.phases.is_empty(), "plan has no phases");
    for (i, phase) in plan.phases.iter().enumerate() {
        for step in &phase.steps {
            anyhow::ensure!(
                TOOLS.contains(&step.worker.as_str()),
                "phase {i} step {}: unknown worker {}",
                step.as_name,
                step.worker
            );
            anyhow::ensure!(
                requirements(&step.worker, &step.action).is_some(),
                "phase {i} step {}: {} has no action {}",
                step.as_name,
                step.worker,
                step.action
            );
        }
    }
    Ok(())
}
