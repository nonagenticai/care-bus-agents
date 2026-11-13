use std::sync::Arc;

use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use carebus::mcp_http::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use serde_json::json;

async fn serve(app: axum::Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

/// The tenant each credentials fetch was for.
type Asked = Arc<std::sync::Mutex<Vec<uuid::Uuid>>>;

/// A minimal streamable-HTTP MCP server that lists two tools, only to `X-Tenant-Id: Bearer lic-<tenant>`, and
/// per-tenant credentials `lic-<tenant>` / `str-<tenant>` (recording which tenant each fetch was for).
async fn stub_mcp() -> (HttpMcp, Asked) {
    let mcp_base = serve(axum::Router::new().route(
        "/mcp",
        axum::routing::any(|method: Method, headers: axum::http::HeaderMap, body: String| async move {
            if method != Method::POST {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            let licence = headers
                .get("x-tenant-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer lic-"))
                .unwrap_or_default()
                .to_string();
            let strata = headers
                .get("x-strata-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if licence.is_empty() || strata != format!("str-{licence}") {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            let req: serde_json::Value = serde_json::from_str(&body).unwrap();
            let id = req["id"].clone();
            let reply = match req["method"].as_str().unwrap_or("") {
                // A server before 2026-07-28: no server/discover, so the client falls back to initialize.
                "server/discover" => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
                "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": req["params"]["protocolVersion"],
                    "capabilities": {"tools": {}}, "serverInfo": {"name": "stub", "version": "0"}}}),
                "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                    {"name": "order_get_order", "description": "Get one order.", "inputSchema": {"type": "object"}},
                    {"name": "faq_ask", "inputSchema": {"type": "object"}}]}}),
                _ => return StatusCode::ACCEPTED.into_response(),
            };
            let mut resp = axum::Json(reply).into_response();
            if req["method"] == "initialize" {
                resp.headers_mut().insert("mcp-session-id", "s".parse().unwrap());
            }
            resp
        }),
    ))
    .await;
    let asked: Asked = Arc::default();
    let log = asked.clone();
    let creds = TenantCredentials::new(FnSource(move |tenant: uuid::Uuid, _fresh: bool| {
        log.lock().unwrap().push(tenant);
        async move {
            McpCredentials::new(
                &format!("lic-{tenant}"),
                &format!("str-{tenant}"),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            )
        }
    }));
    (HttpMcp::new(format!("{mcp_base}/mcp"), creds), asked)
}

#[tokio::test]
async fn tools_503_when_mcp_is_not_configured() {
    let e = crate::common::env("tools-unset").await;
    assert!(
        e.state.mcp.is_none(),
        "test state never reads the developer's MCP env"
    );
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/tools"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["error"]["code"], "upstream_unavailable");
    assert_eq!(b["error"]["message"], "mcp is unavailable");
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j("/tools"), None)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn tools_lists_catalogue_with_24h_stats_from_audit_rows() {
    let mut e = crate::common::env("tools-ok").await;
    let (mcp, asked) = stub_mcp().await;
    e.state.mcp = Some(Arc::new(mcp));
    let app = carebus_api::router(e.state.clone());
    let db = crate::common::pool(&e);
    // what the recorder writes per tool response: resource_id = mcp tool, action execute
    for (status, ms, age) in [
        ("success", 100, "1 hour"),
        ("success", 300, "2 hours"),
        ("failure", 200, "3 hours"),
        ("success", 9999, "30 hours"),
    ] {
        sqlx::query("INSERT INTO audit_events (timestamp, actor_id, actor_type, action_type, resource_type, resource_id, status, details)
                     VALUES (now() - $1::interval, 'tool-executor', 'system', 'execute', 'tool', 'order_get_order', $2, $3)")
            .bind(age).bind(status).bind(json!({"latency_ms": ms})).execute(db).await.unwrap();
    }
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j("/tools"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["next"], serde_json::Value::Null);
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let o = items
        .iter()
        .find(|i| i["name"] == "order_get_order")
        .unwrap();
    assert_eq!(o["description"], "Get one order.");
    assert_eq!(o["calls_24h"], 3, "the 30 h old call is outside the window");
    assert!((o["error_rate"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-9);
    assert!((o["p95_ms"].as_f64().unwrap() - 290.0).abs() < 1e-6);
    let f = items.iter().find(|i| i["name"] == "faq_ask").unwrap();
    assert_eq!(f["calls_24h"], 0);
    assert!(
        f["error_rate"].is_null() && f["p95_ms"].is_null(),
        "no calls: null, not zero"
    );
    // listed with the caller's tenant's own credentials
    assert_eq!(
        *asked.lock().unwrap(),
        [crate::common::BOOT_TENANT.parse::<uuid::Uuid>().unwrap()]
    );
}
