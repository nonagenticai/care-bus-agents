use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use carebus::domain::{ProcedureSource, static_plan};
use carebus::procedures::HttpProcedures;
use carebus::spaces::{ContextSpace, SpaceCatalog};
use serde_json::{Value, json};

/// What the stub API saw on one request.
#[derive(Clone, Debug, Default)]
struct Seen {
    path: String,
    query: String,
    tenant: Option<String>,
    auth: Option<String>,
}

type Log = Arc<std::sync::Mutex<Vec<Seen>>>;

async fn api(reply: Value) -> (String, Arc<AtomicU32>, Log) {
    let hits = Arc::new(AtomicU32::new(0));
    let seen: Log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (h, q) = (hits.clone(), seen.clone());
    let app = axum::Router::new().route(
        "/api/jobs/{job}/procedures",
        axum::routing::get(
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                let (h, q, reply) = (h.clone(), q.clone(), reply.clone());
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    let header = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(String::from)
                    };
                    q.lock().unwrap().push(Seen {
                        path: uri.path().to_string(),
                        query: uri.query().unwrap_or_default().to_string(),
                        tenant: header("x-active-tenant-id"),
                        auth: header("authorization"),
                    });
                    axum::Json(reply)
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, seen)
}

/// The client scope A's orchestrator uses.
fn scoped(url: String, token: Option<String>) -> HttpProcedures {
    HttpProcedures::new(url, token).for_scope(&crate::common::scope_a())
}

/// The bootstrap job's returns space.
fn returns() -> ContextSpace {
    SpaceCatalog::bootstrap()
        .by_key("retail:order-support:returns")
        .expect("bootstrap space")
        .clone()
}

fn return_plan() -> carebus::domain::Plan {
    static_plan("return").expect("built-in return plan")
}

fn published() -> Value {
    // plan 05's Paged list envelope, exactly as carebus-api returns it
    json!({"items": [{"name": "return", "version": "1.3.0", "version_status": "PUBLISHED", "plan": return_plan()}], "next": null})
}

#[tokio::test]
async fn fetches_the_published_plan_for_the_context_space_and_caches_it() {
    let (url, hits, seen) = api(published()).await;
    let src = scoped(url, Some("t".into()));
    let a = src.plan(&returns()).await.unwrap();
    let b = src.plan(&returns()).await.unwrap();
    assert_eq!(a.phases.len(), 3);
    assert_eq!(a.procedure.as_deref(), Some("return@1.3.0"));
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(&b).unwrap()
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "second lookup served from cache"
    );
    assert_eq!(
        seen.lock().unwrap()[0].query,
        "context_space=retail%3Aorder-support%3Areturns&status=PUBLISHED"
    );
}

#[tokio::test]
async fn cache_expires_after_ttl() {
    let (url, hits, _) = api(published()).await;
    let mut src = scoped(url, None);
    src.ttl = Duration::ZERO;
    src.plan(&returns()).await.unwrap();
    src.plan(&returns()).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn no_published_version_is_a_clear_error() {
    let (url, _, _) = api(json!({"items": [], "next": null})).await;
    let err = scoped(url, None).plan(&returns()).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("no PUBLISHED procedure for retail:order-support:returns"),
        "{err:#}"
    );
}

#[test]
fn from_vars_rejects_an_unset_or_empty_url() {
    let with_url = |url: Option<&'static str>| {
        move |k: &str| match k {
            "CAREBUS_API_URL" => url.map(String::from),
            _ => None,
        }
    };
    assert!(HttpProcedures::from_vars(with_url(Some("http://api.example"))).is_ok());
    let err = HttpProcedures::from_vars(with_url(Some(""))).err().unwrap();
    assert!(
        format!("{err:#}").contains("CAREBUS_API_URL is set but empty"),
        "{err:#}"
    );
    let err = HttpProcedures::from_vars(with_url(None)).err().unwrap();
    assert!(
        format!("{err:#}").contains("CAREBUS_API_URL not set"),
        "{err:#}"
    );
}

fn published_plan(plan: Value) -> Value {
    json!({"items": [{"name": "return", "version": "9.0.0", "version_status": "PUBLISHED", "plan": plan}], "next": null})
}

#[tokio::test]
async fn plan_with_a_worker_nobody_consumes_is_rejected_and_not_cached() {
    let mut plan = serde_json::to_value(return_plan()).unwrap();
    plan["phases"][0]["steps"][0]["worker"] = json!("warehouse");
    let (url, hits, _) = api(published_plan(plan)).await;
    let src = scoped(url, None);
    for _ in 0..2 {
        let err = src.plan(&returns()).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown worker warehouse"),
            "{err:#}"
        );
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "a rejected plan is not cached"
    );

    let mut plan = serde_json::to_value(return_plan()).unwrap();
    plan["phases"][0]["steps"][0]["action"] = json!("teleport");
    let (url, _, _) = api(published_plan(plan)).await;
    let err = scoped(url, None).plan(&returns()).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("has no action teleport"),
        "{err:#}"
    );
}

#[tokio::test]
async fn plan_with_no_phases_is_rejected() {
    let (url, _, _) = api(published_plan(json!({"phases": []}))).await;
    let err = scoped(url, None).plan(&returns()).await.unwrap_err();
    assert!(format!("{err:#}").contains("has no phases"), "{err:#}");
}

#[test]
fn every_built_in_plan_passes_validation() {
    use carebus::procedures::validate_plan;
    for name in [
        "return",
        "warranty_claim",
        "delivery_status",
        "duplicate_charge_refund",
        "faq_answer",
    ] {
        let plan = static_plan(name).unwrap_or_else(|| panic!("{name} is built in"));
        validate_plan(&plan).unwrap_or_else(|e| panic!("{name}: {e:#}"));
    }
}

/// The version comes from the lookup, never from a `procedure` key an author stored inside the plan.
#[tokio::test]
async fn the_procedure_version_is_the_one_looked_up_not_one_in_the_plan() {
    let mut plan = serde_json::to_value(return_plan()).unwrap();
    plan["procedure"] = json!("spoofed@9.9.9");
    let (url, _, _) = api(json!({"items": [{"name": "return", "version": "1.4.0", "version_status": "PUBLISHED", "plan": plan}], "next": null})).await;
    let p = scoped(url, None).plan(&returns()).await.unwrap();
    assert_eq!(p.procedure.as_deref(), Some("return@1.4.0"));
}

/// Scope B's client asks for B's job path and names B's tenant; the service token rides along. An unscoped
/// client never reaches the API.
#[tokio::test]
async fn requests_the_scope_path_and_tenant() {
    let (url, hits, seen) = api(published()).await;
    let root = HttpProcedures::new(url, Some("svc-token".into()));
    let b = crate::common::scope_b();
    let src = root.for_scope(&b);
    assert_eq!(src.scope(), Some(&b));
    src.plan(&returns()).await.unwrap();
    let s = seen.lock().unwrap()[0].clone();
    assert_eq!(s.path, "/api/jobs/customer_care_agent/procedures");
    assert_eq!(s.tenant.as_deref(), Some(crate::common::OTHER_TENANT));
    assert_eq!(s.auth.as_deref(), Some("Bearer svc-token"));

    let err = root.plan(&returns()).await.unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the unscoped plan made no request"
    );
}

/// Each scope's client has its own cache: B's lookup is not served from A's.
#[tokio::test]
async fn scopes_do_not_share_the_cache() {
    let (url, hits, seen) = api(published()).await;
    let root = HttpProcedures::new(url, None);
    root.for_scope(&crate::common::scope_a())
        .plan(&returns())
        .await
        .unwrap();
    root.for_scope(&crate::common::scope_b())
        .plan(&returns())
        .await
        .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let tenants: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.tenant.clone())
        .collect();
    assert_eq!(
        tenants,
        vec![
            Some(crate::common::BOOT_TENANT.to_string()),
            Some(crate::common::OTHER_TENANT.to_string())
        ]
    );
}
