//! The job's context spaces as the agents see them, per scope (tenant, job).
//!
//! A context space is configuration (the effective view of the job's release plus tenant refinements, plan 11):
//! its `area` names the department streams (`bus:customer:{area}`, `bus:worker:{area}:{tool}`), its key's last
//! segment the topic prefix (`order-support.returns`). `HttpSpaces::for_scope` reads them from
//! `GET {base}/api/jobs/{job}/context-spaces` the way `HttpProcedures::for_scope` reads procedures; `StaticSpaces`
//! serves the bootstrap job's five seed spaces for tests and local runs without carebus-api.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::RequestType;
use crate::procedures::ACTIVE_TENANT_HEADER;
use crate::scope::Scope;

/// One context space; the fields the agents route with (the route serves more, which is ignored).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSpace {
    /// Ontology key, e.g. `retail:order-support:returns`.
    pub key: String,
    /// Department, the `{area}` in stream keys.
    pub area: String,
    pub request_type: String,
    #[serde(default)]
    pub required: Vec<String>,
    /// Name of the procedure the space runs.
    #[serde(default)]
    pub procedure: String,
}

impl ContextSpace {
    /// The space inside the area: the key's last segment (`returns`).
    pub fn space(&self) -> &str {
        self.key.rsplit(':').next().unwrap_or_default()
    }

    /// Topic prefix, e.g. `order-support.returns` (topics are `{prefix}.{event}`).
    pub fn topic_prefix(&self) -> String {
        format!("{}.{}", self.area, self.space())
    }

    /// Topic for `event` in this space, e.g. `order-support.returns.work.requested`.
    pub fn topic(&self, event: &str) -> String {
        format!("{}.{event}", self.topic_prefix())
    }
}

/// The context spaces of one job, sorted by key, one entry per key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpaceCatalog {
    spaces: Vec<ContextSpace>,
}

impl SpaceCatalog {
    pub fn new(mut spaces: Vec<ContextSpace>) -> Self {
        spaces.sort_by(|a, b| a.key.cmp(&b.key));
        spaces.dedup_by(|a, b| a.key == b.key);
        Self { spaces }
    }

    /// The bootstrap job's seed spaces (carebus-api `0002_seed.sql`), i.e. today's four areas.
    pub fn bootstrap() -> Self {
        fn sp(key: &str, area: &str, rt: &str, required: &[&str], procedure: &str) -> ContextSpace {
            ContextSpace {
                key: key.into(),
                area: area.into(),
                request_type: rt.into(),
                required: required.iter().map(|s| (*s).to_string()).collect(),
                procedure: procedure.into(),
            }
        }
        Self::new(vec![
            sp(
                "retail:order-support:returns",
                "order-support",
                "return",
                &["order_id", "reason"],
                "return",
            ),
            sp(
                "retail:order-support:warranty",
                "order-support",
                "warranty_claim",
                &["order_id", "defect"],
                "warranty_claim",
            ),
            sp(
                "retail:logistics:delivery-status",
                "logistics",
                "delivery_status",
                &["order_id"],
                "delivery_status",
            ),
            sp(
                "retail:payments:payment-errors",
                "payments",
                "payment_error",
                &["order_id", "error_type"],
                "duplicate_charge_refund",
            ),
            sp(
                "retail:faq:general",
                "faq",
                "faq",
                &["question"],
                "faq_answer",
            ),
        ])
    }

    pub fn spaces(&self) -> &[ContextSpace] {
        &self.spaces
    }

    pub fn by_request_type(&self, rt: &str) -> Option<&ContextSpace> {
        self.spaces.iter().find(|s| s.request_type == rt)
    }

    /// The space of request type `rt`, if this job has one.
    pub fn space_of(&self, rt: &RequestType) -> Option<&ContextSpace> {
        self.by_request_type(rt.as_str())
    }

    pub fn by_key(&self, key: &str) -> Option<&ContextSpace> {
        self.spaces.iter().find(|s| s.key == key)
    }

    /// Topic for `event` in the space of `rt`, e.g. `order-support.returns.work.requested`.
    pub fn topic(&self, rt: &RequestType, event: &str) -> Option<String> {
        self.space_of(rt).map(|s| s.topic(event))
    }

    /// The job's request types that are valid `RequestType`s, in key order.
    pub fn request_types(&self) -> Vec<RequestType> {
        self.spaces
            .iter()
            .filter_map(|s| RequestType::new(s.request_type.as_str()).ok())
            .collect()
    }

    /// Distinct areas, in order of first appearance by space key.
    pub fn areas(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in &self.spaces {
            if !out.contains(&s.area) {
                out.push(s.area.clone());
            }
        }
        out
    }
}

/// Where an agent gets its scope's context spaces.
pub trait SpaceSource: Send + Sync + 'static {
    fn catalog(&self) -> impl Future<Output = Result<SpaceCatalog>> + Send;
}

/// The bootstrap spaces (tests, local runs without carebus-api).
pub struct StaticSpaces;

impl SpaceSource for StaticSpaces {
    async fn catalog(&self) -> Result<SpaceCatalog> {
        Ok(SpaceCatalog::bootstrap())
    }
}

/// plan 05's list envelope for every list route.
#[derive(Deserialize)]
struct Paged<T> {
    items: Vec<T>,
}

/// Context spaces from carebus-api for one scope. Built unscoped from the environment, then `for_scope` per scope;
/// an unscoped client refuses to look anything up.
pub struct HttpSpaces {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
    scope: Option<Scope>,
}

impl HttpSpaces {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            scope: None,
        }
    }

    pub fn from_env() -> Result<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// `CAREBUS_API_URL` (required) and `CAREBUS_API_TOKEN` (empty means none), as `HttpProcedures::from_vars`.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let url = crate::required_var("CAREBUS_API_URL", get("CAREBUS_API_URL"))?;
        let token = get("CAREBUS_API_TOKEN").filter(|t| !t.is_empty());
        Ok(Self::new(url, token))
    }

    /// The same API, URL and token for scope `s`.
    pub fn for_scope(&self, s: &Scope) -> HttpSpaces {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            token: self.token.clone(),
            scope: Some(s.clone()),
        }
    }

    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }
}

impl SpaceSource for HttpSpaces {
    async fn catalog(&self) -> Result<SpaceCatalog> {
        let scope = self.scope.as_ref().context("spaces client is not scoped")?;
        let mut req = self
            .http
            .get(format!(
                "{}/api/jobs/{}/context-spaces",
                self.base_url, scope.job
            ))
            .header(ACTIVE_TENANT_HEADER, scope.tenant.to_string());
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let page: Paged<ContextSpace> = req
            .send()
            .await
            .context("carebus-api unreachable")?
            .error_for_status()
            .context("carebus-api refused the context-space lookup")?
            .json()
            .await
            .context("context-space list is not the expected shape")?;
        Ok(SpaceCatalog::new(page.items))
    }
}

/// From carebus-api when `CAREBUS_API_URL` is set, otherwise the bootstrap spaces (local runs, and deployments
/// that do not give the agent the API yet: they keep serving the bootstrap job's areas).
pub enum EnvSpaces {
    Http(HttpSpaces),
    Static,
}

impl EnvSpaces {
    pub fn from_env() -> Result<Self> {
        Ok(match crate::optional_var("CAREBUS_API_URL") {
            Some(_) => Self::Http(HttpSpaces::from_env()?),
            None => Self::Static,
        })
    }

    pub fn for_scope(&self, s: &Scope) -> EnvSpaces {
        match self {
            Self::Http(h) => Self::Http(h.for_scope(s)),
            Self::Static => Self::Static,
        }
    }
}

impl SpaceSource for EnvSpaces {
    async fn catalog(&self) -> Result<SpaceCatalog> {
        match self {
            Self::Http(h) => h.catalog().await,
            Self::Static => Ok(SpaceCatalog::bootstrap()),
        }
    }
}

/// How often a running scope task re-reads its context spaces.
pub const REFRESH: Duration = Duration::from_secs(30);

/// Run `run(areas)` on the scope's current areas. The source is re-read every `refresh`; when the area set changes
/// this returns `Ok`, and the supervisor restarts the scope task (after 5 s) on the new streams, so a context space
/// added in a release is consumed without a redeploy. With no areas (a job without context spaces yet) nothing is
/// consumed until some appear. A failed first read is an error (the supervisor retries); a failed re-read keeps the
/// current streams.
pub async fn run_with_areas<S, F, Fut>(source: &S, refresh: Duration, run: F) -> Result<()>
where
    S: SpaceSource,
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    run_while_same(source, refresh, SpaceCatalog::areas, |c| run(c.areas())).await
}

/// `run_with_areas` for agents that route by the spaces themselves (request type → area, topic): `run(catalog)`
/// on the scope's current context spaces, returning when any space changes (a new request type in a known area
/// included), so the restarted task routes with the new catalog.
pub async fn run_with_catalog<S, F, Fut>(source: &S, refresh: Duration, run: F) -> Result<()>
where
    S: SpaceSource,
    F: FnOnce(SpaceCatalog) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    run_while_same(source, refresh, SpaceCatalog::clone, run).await
}

/// Run `run(catalog)` until `view` of a re-read catalog differs from the first one's; an empty catalog idles.
async fn run_while_same<S, V, K, F, Fut>(
    source: &S,
    refresh: Duration,
    view: V,
    run: F,
) -> Result<()>
where
    S: SpaceSource,
    V: Fn(&SpaceCatalog) -> K,
    K: PartialEq + std::fmt::Debug,
    F: FnOnce(SpaceCatalog) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let catalog = source
        .catalog()
        .await
        .context("reading the scope's context spaces")?;
    let current = view(&catalog);
    let changed = async {
        loop {
            tokio::time::sleep(refresh).await;
            match source.catalog().await {
                Ok(c) => {
                    let next = view(&c);
                    if next != current {
                        tracing::info!(old = ?current, new = ?next, "context spaces changed; restarting on the new streams");
                        return;
                    }
                }
                Err(e) => tracing::warn!(
                    "re-reading the scope's context spaces failed; keeping the current streams: {e:#}"
                ),
            }
        }
    };
    if catalog.spaces().is_empty() {
        tracing::info!("no context spaces in this scope yet; idle until some appear");
        changed.await;
        return Ok(());
    }
    tokio::select! {
        r = run(catalog) => r,
        () = changed => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AREAS, WORK_REQUESTED};

    /// Today's wire: the five built-in request types keep their spaces, areas, required fields and topics.
    #[test]
    fn bootstrap_agrees_with_the_built_in_request_types() {
        let cat = SpaceCatalog::bootstrap();
        assert_eq!(cat.spaces().len(), 5);
        for (rt, key, area, required, topic) in [
            (
                RequestType::r#return(),
                "retail:order-support:returns",
                "order-support",
                &["order_id", "reason"][..],
                "order-support.returns.work.requested",
            ),
            (
                RequestType::warranty_claim(),
                "retail:order-support:warranty",
                "order-support",
                &["order_id", "defect"][..],
                "order-support.warranty.work.requested",
            ),
            (
                RequestType::delivery_status(),
                "retail:logistics:delivery-status",
                "logistics",
                &["order_id"][..],
                "logistics.delivery-status.work.requested",
            ),
            (
                RequestType::payment_error(),
                "retail:payments:payment-errors",
                "payments",
                &["order_id", "error_type"][..],
                "payments.payment-errors.work.requested",
            ),
            (
                RequestType::faq(),
                "retail:faq:general",
                "faq",
                &["question"][..],
                "faq.general.work.requested",
            ),
        ] {
            let s = cat
                .space_of(&rt)
                .unwrap_or_else(|| panic!("{rt} has a bootstrap space"));
            assert_eq!(s.key, key);
            assert_eq!(s.area, area);
            assert_eq!(s.required, required);
            assert_eq!(s.topic(WORK_REQUESTED), topic);
            assert_eq!(cat.topic(&rt, WORK_REQUESTED).as_deref(), Some(topic));
            assert_eq!(cat.by_key(key), Some(s));
        }
        assert_eq!(cat.request_types().len(), 5);
        assert_eq!(
            cat.areas(),
            ["faq", "logistics", "order-support", "payments"]
        );
        let mut built_in: Vec<&str> = AREAS.to_vec();
        built_in.sort_unstable();
        assert_eq!(cat.areas(), built_in);
    }

    #[test]
    fn new_sorts_and_drops_duplicate_keys() {
        let mut a = SpaceCatalog::bootstrap().spaces()[0].clone();
        a.key = "z:new:space".into();
        a.area = "new-area".into();
        let mut spaces = SpaceCatalog::bootstrap().spaces().to_vec();
        spaces.insert(0, a.clone());
        spaces.push(a);
        let cat = SpaceCatalog::new(spaces);
        assert_eq!(cat.spaces().len(), 6);
        assert_eq!(
            cat.spaces().last().unwrap().topic_prefix(),
            "new-area.space"
        );
        assert_eq!(cat.areas().last().map(String::as_str), Some("new-area"));
    }

    /// Serves the catalogs in order; the last one keeps being served.
    struct Seq(std::sync::Mutex<Vec<SpaceCatalog>>);

    impl SpaceSource for Seq {
        async fn catalog(&self) -> Result<SpaceCatalog> {
            let mut v = self.0.lock().unwrap();
            Ok(if v.len() > 1 {
                v.remove(0)
            } else {
                v[0].clone()
            })
        }
    }

    fn with_extra_area() -> SpaceCatalog {
        let mut extra = SpaceCatalog::bootstrap().spaces()[0].clone();
        extra.key = "retail:returns-desk:returns-b".into();
        extra.area = "returns-desk".into();
        extra.request_type = "returns_b".into();
        let mut all = SpaceCatalog::bootstrap().spaces().to_vec();
        all.push(extra);
        SpaceCatalog::new(all)
    }

    #[tokio::test]
    async fn run_with_areas_returns_when_the_areas_change() {
        let src = Seq(std::sync::Mutex::new(vec![
            SpaceCatalog::bootstrap(),
            SpaceCatalog::bootstrap(),
            with_extra_area(),
        ]));
        let got = std::sync::Mutex::new(Vec::new());
        let res = tokio::time::timeout(
            Duration::from_secs(2),
            run_with_areas(&src, Duration::from_millis(10), |areas| {
                let got = &got;
                async move {
                    *got.lock().unwrap() = areas;
                    std::future::pending::<()>().await;
                    Ok(())
                }
            }),
        )
        .await;
        assert!(matches!(res, Ok(Ok(()))), "returned on the change");
        assert_eq!(
            *got.lock().unwrap(),
            ["faq", "logistics", "order-support", "payments"]
        );
    }

    #[tokio::test]
    async fn run_with_catalog_returns_when_a_request_type_is_added_in_a_known_area() {
        let mut extra = SpaceCatalog::bootstrap().spaces()[0].clone();
        extra.key = "retail:order-support:exchanges".into();
        extra.request_type = "exchange".into();
        let mut all = SpaceCatalog::bootstrap().spaces().to_vec();
        all.push(extra);
        let grown = SpaceCatalog::new(all);
        assert_eq!(grown.areas(), SpaceCatalog::bootstrap().areas());
        let src = Seq(std::sync::Mutex::new(vec![
            SpaceCatalog::bootstrap(),
            grown,
        ]));
        let res = tokio::time::timeout(
            Duration::from_secs(2),
            run_with_catalog(&src, Duration::from_millis(10), |cat| async move {
                assert!(cat.by_request_type("exchange").is_none());
                std::future::pending::<()>().await;
                Ok(())
            }),
        )
        .await;
        assert!(matches!(res, Ok(Ok(()))), "returned on the change");
    }

    #[tokio::test]
    async fn empty_catalog_idles_until_areas_change() {
        let src = Seq(std::sync::Mutex::new(vec![
            SpaceCatalog::default(),
            SpaceCatalog::default(),
            SpaceCatalog::bootstrap(),
        ]));
        let called = std::sync::atomic::AtomicBool::new(false);
        let res = tokio::time::timeout(
            Duration::from_secs(2),
            run_with_areas(&src, Duration::from_millis(10), |_| {
                let called = &called;
                async move {
                    called.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            }),
        )
        .await;
        assert!(matches!(res, Ok(Ok(()))));
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "nothing consumed without areas"
        );
    }
}
