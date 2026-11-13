fn boot_scope() -> carebus::Scope {
    carebus::Scope {
        tenant: crate::common::BOOT_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    }
}

/// The bootstrap job's bus: the routes under `crate::common::j(..)` read exactly these keys.
fn boot_bus(e: &crate::common::TestEnv) -> carebus::Bus {
    e.state.bus.scoped(&boot_scope())
}

use std::sync::{Arc, Mutex};

use carebus::mcp_http::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use serde_json::Value;
use uuid::Uuid;

fn health(b: &Value, n: &str) -> Value {
    b["health"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == n)
        .unwrap_or_else(|| panic!("no health entry {n}"))
        .clone()
}

fn attention_pages(b: &Value) -> Vec<String> {
    b["attention"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["page"].as_str().unwrap().to_string())
        .collect()
}

/// A URL on loopback where nothing listens: bind to port 0, note the port, drop the listener.
fn dead_url() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn overview_is_deterministic_and_reports_seed_attention() {
    let e = crate::common::env("overview").await;
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/overview"), Some(&t)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(health(&b, "redis")["status"], "ok");
    assert_eq!(health(&b, "postgres")["status"], "ok");
    assert_eq!(health(&b, "strata_rag")["status"], "unconfigured");
    assert_eq!(health(&b, "demo-backends")["status"], "unconfigured");
    assert_eq!(health(&b, "mcp")["status"], "unconfigured");
    let pages = attention_pages(&b);
    assert!(
        pages.contains(&"procedures".into()),
        "return@1.4.0-beta is CANARY in the seed"
    );
    assert!(
        !pages.contains(&"models".into()),
        "model routes are gone (D38): nothing points at the deleted Models page"
    );
    assert_eq!(b["today"]["cases"], 0);
    assert_eq!(b["week_ago"]["cases"], 0);
    assert!(
        !b["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["severity"] == "err"),
        "no dependency is down and no dead letters exist: {b}"
    );
}

#[tokio::test]
async fn overview_requires_a_token() {
    let e = crate::common::env("overview_auth").await;
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/overview"), None),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn overview_reports_down_dependencies_without_failing() {
    let dead = dead_url();
    let e = crate::common::env_with("overview_down", |c| {
        c.strata_rag_url = Some(dead.clone());
        c.demo_backends_url = Some(dead.clone());
        c.mcp_url = Some(format!("{dead}/mcp"));
    })
    .await;
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/overview"), Some(&t)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    for n in ["strata_rag", "demo-backends", "mcp"] {
        assert_eq!(health(&b, n)["status"], "down", "{n}");
    }
    let detail = health(&b, "mcp")["detail"].to_string();
    assert!(
        !detail.contains("127.0.0.1"),
        "probe URL must not be echoed: {detail}"
    );
    let titles: Vec<String> = b["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["severity"] == "err")
        .map(|a| a["title"].as_str().unwrap().to_string())
        .collect();
    for n in ["strata_rag", "demo-backends", "mcp"] {
        assert!(
            titles.contains(&format!("{n} is down")),
            "{n} missing from attention: {titles:?}"
        );
    }
}

async fn serve(app: axum::Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

/// Stub of nonagenticai_mcp_server: `/api/health` answers 200 only to `X-Tenant-Id: Bearer good` with a strata token
/// (anything else is 401) and records every `X-Tenant-Id` it sees; an `Authorization` header is refused outright.
async fn stub_mcp_server() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    use axum::http::{HeaderMap, StatusCode};
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let seen2 = seen.clone();
    let base = serve(axum::Router::new().route(
        "/api/health",
        axum::routing::get(move |h: HeaderMap| {
            let seen = seen2.clone();
            async move {
                let licence = h
                    .get("x-tenant-id")
                    .map(|v| v.to_str().unwrap().to_string());
                let ok = licence.as_deref() == Some("Bearer good")
                    && h.get("x-strata-token").is_some()
                    && h.get("authorization").is_none();
                seen.lock().unwrap().push(licence);
                if ok {
                    StatusCode::OK
                } else {
                    StatusCode::UNAUTHORIZED
                }
            }
        }),
    ))
    .await;
    (base, seen)
}

/// The tenant each credentials fetch was for, and whether it was a fresh one.
type Asked = Arc<Mutex<Vec<(Uuid, bool)>>>;

/// Per-tenant credentials issuing `licences[0]`, then `licences[1]`, … (the last one repeats); `None` fails every
/// fetch (no licence can be minted).
fn credentials(licences: Option<&'static [&'static str]>) -> (TenantCredentials, Asked) {
    let asked: Asked = Arc::default();
    let log = asked.clone();
    let creds = TenantCredentials::new(FnSource(move |tenant: Uuid, fresh: bool| {
        let n = {
            let mut g = log.lock().unwrap();
            g.push((tenant, fresh));
            g.len()
        };
        async move {
            let Some(licences) = licences else {
                anyhow::bail!("not_licensed_for_tenant: no care-bus licence");
            };
            McpCredentials::new(
                licences[(n - 1).min(licences.len() - 1)],
                "strata-token",
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            )
        }
    }));
    (creds, asked)
}

async fn overview_against(
    test: &str,
    mcp_base: &str,
    licences: Option<Option<&'static [&'static str]>>,
) -> (Value, Asked) {
    let mut e =
        crate::common::env_with(test, |c| c.mcp_url = Some(format!("{mcp_base}/mcp"))).await;
    let mut asked = Asked::default();
    if let Some(l) = licences {
        let (creds, a) = credentials(l);
        asked = a;
        e.state.mcp = Some(Arc::new(HttpMcp::new(format!("{mcp_base}/mcp"), creds)));
    }
    let app = carebus_api::router(e.state.clone());
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j("/overview"), Some(&t)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    (b, asked)
}

const GOOD: &[&str] = &["good"];
const STALE: &[&str] = &["stale"];
const STALE_THEN_GOOD: &[&str] = &["stale", "good"];

fn boot() -> Uuid {
    crate::common::BOOT_TENANT.parse().unwrap()
}

/// The probe carries the caller's TENANT's licence (X-Tenant-Id), as its tool calls do, never a shared bearer.
#[tokio::test]
async fn overview_probes_mcp_health_with_the_tenants_licence() {
    let (base, seen) = stub_mcp_server().await;
    let (b, asked) = overview_against("overview_mcp", &base, Some(Some(GOOD))).await;
    assert_eq!(health(&b, "mcp")["status"], "ok", "{b}");
    assert_eq!(*seen.lock().unwrap(), vec![Some("Bearer good".to_string())]);
    assert_eq!(*asked.lock().unwrap(), [(boot(), false)]);
}

#[tokio::test]
async fn overview_reports_mcp_down_when_its_licence_is_refused() {
    let (base, seen) = stub_mcp_server().await;
    let (b, _) = overview_against("overview_mcp_401", &base, Some(Some(STALE))).await;
    assert_eq!(health(&b, "mcp")["status"], "down");
    assert_eq!(health(&b, "mcp")["detail"], "HTTP 401");
    assert_eq!(seen.lock().unwrap().len(), 2, "one retry, then down");
}

/// A refused licence is renewed once (a fresh mint) and the probe retried, instead of showing MCP down until the
/// cached licence expires.
#[tokio::test]
async fn overview_mcp_probe_renews_a_refused_licence_and_retries_once() {
    let (base, seen) = stub_mcp_server().await;
    let (b, asked) =
        overview_against("overview_mcp_refresh", &base, Some(Some(STALE_THEN_GOOD))).await;
    assert_eq!(health(&b, "mcp")["status"], "ok", "{b}");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            Some("Bearer stale".to_string()),
            Some("Bearer good".to_string())
        ]
    );
    assert_eq!(*asked.lock().unwrap(), [(boot(), false), (boot(), true)]);
}

#[tokio::test]
async fn overview_reports_mcp_down_without_credentials_instead_of_probing_anonymously() {
    let (base, seen) = stub_mcp_server().await;
    let (b, _) = overview_against("overview_mcp_nocreds", &base, None).await;
    assert_eq!(health(&b, "mcp")["status"], "down");
    assert_eq!(health(&b, "mcp")["detail"], "no MCP credentials");
    assert!(
        seen.lock().unwrap().is_empty(),
        "no anonymous request is sent"
    );
}

/// The tenant holds no care-bus licence (nothing can be minted): down, and nothing is sent.
#[tokio::test]
async fn overview_reports_mcp_down_when_no_licence_can_be_minted() {
    let (base, seen) = stub_mcp_server().await;
    let (b, _) = overview_against("overview_mcp_nolicence", &base, Some(None)).await;
    assert_eq!(health(&b, "mcp")["status"], "down");
    assert_eq!(health(&b, "mcp")["detail"], "credentials unavailable");
    assert!(seen.lock().unwrap().is_empty());
}

/// Since W3 the caller's tenant and job come from Postgres, so with the database down nobody can be resolved:
/// the overview is a 503 naming postgres, never a page of zeros.
#[tokio::test]
async fn overview_with_postgres_down_is_503_naming_postgres() {
    let e = crate::common::env("overview_pgdown").await;
    e.state.db.close().await;
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/overview"), Some(&t)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "upstream_unavailable", "{b}");
    assert_eq!(b["error"]["message"], "postgres is unavailable", "{b}");
}

#[tokio::test]
async fn overview_counts_dead_letters_and_todays_cases() {
    let e = crate::common::env("overview_counts").await;
    sqlx::query(
        "INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, outcome, started_at, resolved_at, duration_ms)
         VALUES ('c-1','cu','return','retail','retail:order-support','completed', now(), now(), 1000),
                ('c-2','cu','return','retail','retail:order-support', NULL, now(), NULL, NULL)",
    )
    .execute(crate::common::pool(&e))
    .await
    .unwrap();
    let key = boot_bus(&e).streams.key("bus:dead");
    let _: String = redis::cmd("XADD")
        .arg(&key)
        .arg("*")
        .arg("payload")
        .arg("{}")
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    let t = crate::common::user();
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/overview"), Some(&t)),
    )
    .await;
    assert_eq!(b["today"]["cases"], 2);
    assert_eq!(b["today"]["completed"], 1);
    assert_eq!(b["today"]["open"], 1);
    assert!(
        b["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["severity"] == "err"
                && a["page"] == "buses"
                && a["title"].as_str().unwrap().contains("dead-letter")),
        "{b}"
    );
}

/// A case-counter query that never answers (case_history locked by another transaction) must not hold the page:
/// each counter is bounded, comes back `null` (unknown, not zero), and the rest of the page is served. Postgres
/// itself has to answer since W3 (the caller's tenant and job are read from it).
#[tokio::test]
async fn overview_returns_promptly_when_the_case_counters_hang() {
    let e = crate::common::env("overview_pghang").await;
    let mut lock = crate::common::pool(&e).begin().await.unwrap();
    sqlx::query("LOCK TABLE case_history IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let t = crate::common::user();
    let started = std::time::Instant::now();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/overview"), Some(&t)),
    )
    .await;
    let took = started.elapsed();
    lock.rollback().await.unwrap();
    assert_eq!(s, axum::http::StatusCode::OK, "{b}");
    // two bounded counters (today, a week ago) of 2 s each, nothing waits for the lock beyond that
    assert!(took < std::time::Duration::from_secs(7), "took {took:?}");
    assert!(b["today"].is_null() && b["week_ago"].is_null(), "{b}");
    assert_eq!(health(&b, "postgres")["status"], "ok");
    assert_eq!(health(&b, "redis")["status"], "ok");
}
