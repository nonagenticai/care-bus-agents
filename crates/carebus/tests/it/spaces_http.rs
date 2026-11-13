//! `HttpSpaces`: one scope's context spaces from carebus-api (plan 12 Task 3).
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use carebus::spaces::{HttpSpaces, SpaceSource};
use serde_json::{Value, json};

/// What the stub API saw on one request.
#[derive(Clone, Debug, Default)]
struct Seen {
    path: String,
    tenant: Option<String>,
    auth: Option<String>,
}

type Log = Arc<std::sync::Mutex<Vec<Seen>>>;

async fn api(status: u16, reply: Value) -> (String, Arc<AtomicU32>, Log) {
    let hits = Arc::new(AtomicU32::new(0));
    let seen: Log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (h, q) = (hits.clone(), seen.clone());
    let app = axum::Router::new().route(
        "/api/jobs/{job}/context-spaces",
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
                        tenant: header("x-active-tenant-id"),
                        auth: header("authorization"),
                    });
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        axum::Json(reply),
                    )
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, seen)
}

/// Two items as carebus-api returns them (extra fields included), plus a space in an area no code knows.
fn listed() -> Value {
    json!({"items": [
        {"key": "retail:order-support:returns", "area": "order-support", "label": "Returns", "request_type": "return",
         "required": ["order_id", "reason"], "entities": ["Order"], "procedure": "return", "origin": "base",
         "overridden": false, "hidden": false, "topic_prefix": "order-support.returns",
         "streams": {"customer": "bus:customer:order-support", "worker_results": "bus:worker:order-support:results", "worker": []}},
        {"key": "retail:returns-desk:returns-b", "area": "returns-desk", "label": "Returns desk", "request_type": "returns_b",
         "required": ["order_id"], "entities": ["Order"], "procedure": "return", "origin": "tenant",
         "overridden": false, "hidden": false, "topic_prefix": "returns-desk.returns-b",
         "streams": {"customer": "bus:customer:returns-desk", "worker_results": "bus:worker:returns-desk:results", "worker": []}}
    ], "next": null})
}

#[tokio::test]
async fn requests_the_scope_path_and_tenant() {
    let (url, hits, seen) = api(200, listed()).await;
    let root = HttpSpaces::new(url, Some("svc-token".into()));
    let b = crate::common::scope_b();
    let src = root.for_scope(&b);
    assert_eq!(src.scope(), Some(&b));
    src.catalog().await.unwrap();
    let s = seen.lock().unwrap()[0].clone();
    assert_eq!(s.path, "/api/jobs/customer_care_agent/context-spaces");
    assert_eq!(s.tenant.as_deref(), Some(crate::common::OTHER_TENANT));
    assert_eq!(s.auth.as_deref(), Some("Bearer svc-token"));

    let err = root.catalog().await.unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the unscoped lookup made no request"
    );
}

#[tokio::test]
async fn parses_the_paged_route() {
    let (url, _, _) = api(200, listed()).await;
    let cat = HttpSpaces::new(url, None)
        .for_scope(&crate::common::scope_a())
        .catalog()
        .await
        .unwrap();
    assert_eq!(cat.areas(), ["order-support", "returns-desk"]);
    let s = cat.by_request_type("returns_b").unwrap();
    assert_eq!(s.topic_prefix(), "returns-desk.returns-b");
    assert_eq!(s.required, ["order_id"]);
}

#[tokio::test]
async fn refused_lookup_is_an_error() {
    let (url, _, _) = api(500, json!({"error": {"code": "internal"}})).await;
    let err = HttpSpaces::new(url, None)
        .for_scope(&crate::common::scope_a())
        .catalog()
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("refused"), "{err:#}");
}
