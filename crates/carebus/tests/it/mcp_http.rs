//! `HttpMcp` calls nonagenticai_mcp_server as ONE bus tenant per call: `X-Tenant-Id: Bearer <tenant's care-bus
//! licence>` and `X-Strata-Token: <tenant's strata-rag token>`, no `Authorization`, one session per tenant, and no
//! request at all when the tenant's credentials cannot be fetched.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carebus::mcp::McpClient;
use carebus::mcp_http::{
    AUTH_REJECTED, ApiCredentialSource, CredentialSource, FnSource, HttpMcp, McpCredentials,
    TenantCredentials,
};
use serde_json::json;
use uuid::Uuid;

const A: Uuid = Uuid::from_u128(0xa);
const B: Uuid = Uuid::from_u128(0xb);

fn short(t: Uuid) -> String {
    format!("{:x}", t.as_u128())
}

/// The n-th licence (1-based) the fake issuer hands `t`.
fn lic(t: Uuid, n: usize) -> String {
    format!("lic-{}-{n}", short(t))
}

fn strata(t: Uuid, n: usize) -> String {
    format!("str-{}-{n}", short(t))
}

/// A fake credential source: the n-th fetch for a tenant issues `lic(t, n)` / `strata(t, n)`, valid for `life(n)`.
/// Records (tenant, fresh) per fetch; a tenant in `fail` gets an error.
#[derive(Clone, Default)]
struct Issuer {
    fetches: Arc<Mutex<Vec<(Uuid, bool)>>>,
    fail: Arc<Mutex<Vec<Uuid>>>,
}

impl Issuer {
    fn hits(&self) -> usize {
        self.fetches.lock().unwrap().len()
    }

    fn hits_for(&self, t: Uuid) -> usize {
        self.fetches
            .lock()
            .unwrap()
            .iter()
            .filter(|(x, _)| *x == t)
            .count()
    }

    fn issue(
        &self,
        tenant: Uuid,
        fresh: bool,
        life: &dyn Fn(usize) -> Duration,
    ) -> anyhow::Result<McpCredentials> {
        if self.fail.lock().unwrap().contains(&tenant) {
            anyhow::bail!("carebus-api refused the MCP credentials: 409");
        }
        let n = {
            let mut f = self.fetches.lock().unwrap();
            f.push((tenant, fresh));
            f.iter().filter(|(x, _)| *x == tenant).count()
        };
        McpCredentials::new(
            &lic(tenant, n),
            &strata(tenant, n),
            Instant::now() + life(n),
        )
    }

    fn creds(&self, life: impl Fn(usize) -> Duration + Send + Sync + 'static) -> TenantCredentials {
        let me = self.clone();
        TenantCredentials::new(FnSource(move |tenant: Uuid, fresh: bool| {
            let out = me.issue(tenant, fresh, &life);
            async move { out }
        }))
    }
}

fn long(_: usize) -> Duration {
    Duration::from_secs(300)
}

#[test]
fn tools_list_result_is_parsed_into_mcp_tools() {
    use carebus::mcp_http::{McpTool, mcp_tools_from_list_result};
    let result = json!({"tools": [
        {"name": "order_get_order", "description": "Get one order.", "inputSchema": {"type": "object", "required": ["order_id"]}},
        {"name": "faq_ask", "inputSchema": {"type": "object"}},
        {"description": "no name: skipped"}
    ]});
    let tools = mcp_tools_from_list_result(&result);
    assert_eq!(tools.len(), 2);
    assert_eq!(
        tools[0],
        McpTool {
            name: "order_get_order".into(),
            description: Some("Get one order.".into()),
            input_schema: json!({"type": "object", "required": ["order_id"]}),
        }
    );
    assert_eq!(tools[1].description, None);
    assert!(mcp_tools_from_list_result(&json!({})).is_empty());
}

#[tokio::test]
async fn credentials_are_cached_per_tenant_until_close_to_expiry() {
    let issuer = Issuer::default();
    let creds = issuer.creds(long);
    assert_eq!(creds.get(A).await.unwrap().licence_token(), lic(A, 1));
    assert_eq!(creds.get(A).await.unwrap().licence_token(), lic(A, 1));
    assert_eq!(creds.get(B).await.unwrap().licence_token(), lic(B, 1));
    assert_eq!(issuer.hits_for(A), 1);
    assert_eq!(issuer.hits_for(B), 1);
}

#[tokio::test]
async fn expired_credentials_are_refetched() {
    let issuer = Issuer::default();
    let creds = issuer.creds(|_| Duration::ZERO);
    assert_eq!(creds.get(A).await.unwrap().licence_token(), lic(A, 1));
    assert_eq!(creds.get(A).await.unwrap().licence_token(), lic(A, 2));
    assert_eq!(issuer.hits(), 2);
}

#[tokio::test]
async fn renew_fetches_fresh_once_and_keeps_a_licence_another_request_already_renewed() {
    let issuer = Issuer::default();
    let creds = issuer.creds(long);
    let first = creds.get(A).await.unwrap();
    let renewed = creds.renew(A, first.licence_token()).await.unwrap();
    assert_eq!(renewed.licence_token(), lic(A, 2));
    // a second request refused with the same stale licence gets the renewed one, no third fetch
    let again = creds.renew(A, first.licence_token()).await.unwrap();
    assert_eq!(again.licence_token(), lic(A, 2));
    assert_eq!(*issuer.fetches.lock().unwrap(), [(A, false), (A, true)]);
}

#[test]
fn credentials_must_be_header_safe_and_never_print() {
    let at = Instant::now();
    assert!(McpCredentials::new("", "s", at).is_err());
    assert!(McpCredentials::new("l", "", at).is_err());
    assert!(McpCredentials::new("l\nX-Evil: 1", "s", at).is_err());
    let c = McpCredentials::new("secret-licence", "secret-strata", at).unwrap();
    let shown = format!("{c:?}");
    assert!(!shown.contains("secret"), "{shown}");
    let [(n1, v1), (n2, v2)] = c.headers();
    assert_eq!(n1.as_str(), "x-tenant-id");
    assert_eq!(v1.to_str().unwrap(), "Bearer secret-licence");
    assert_eq!(n2.as_str(), "x-strata-token");
    assert_eq!(v2.to_str().unwrap(), "secret-strata");
    assert!(v1.is_sensitive() && v2.is_sensitive());
    assert_eq!(c.strata_token(), "secret-strata");
}

#[test]
fn tool_error_prefix_sets_retryable_and_is_stripped() {
    use carebus::mcp_http::tool_error_from_text;
    let e = tool_error_from_text("[retryable] demo-backends timed out");
    assert!(e.retryable);
    assert_eq!(e.message, "demo-backends timed out");
    let e = tool_error_from_text("[permanent] order ORD-1 not found");
    assert!(!e.retryable);
    assert_eq!(e.message, "order ORD-1 not found");
}

#[test]
fn tool_error_without_prefix_is_permanent() {
    let e = carebus::mcp_http::tool_error_from_text("something broke");
    assert!(!e.retryable);
    assert_eq!(e.message, "something broke");
    assert_eq!(
        carebus::mcp_http::tool_error_from_text("").message,
        "tool error"
    );
}

#[test]
fn mcp_protocol_error_is_permanent_and_keeps_the_session() {
    use carebus::mcp_http::service_error_to_tool_error;
    use rmcp::service::ServiceError;
    let (e, reset) = service_error_to_tool_error(&ServiceError::McpError(
        rmcp::ErrorData::invalid_params("unknown tool", None),
    ));
    assert!(!e.retryable);
    assert!(!reset);
    let (e, reset) = service_error_to_tool_error(&ServiceError::TransportClosed);
    assert!(e.retryable);
    assert!(reset);
    let (e, reset) = service_error_to_tool_error(&ServiceError::Timeout {
        timeout: Duration::from_secs(1),
    });
    assert!(e.retryable);
    assert!(reset);
}

/// One POST the stub saw.
#[derive(Clone, Debug)]
struct Post {
    method: String,
    tenant: String,
    strata: String,
    authorization: Option<String>,
    session: Option<String>,
}

#[derive(Clone, Default)]
struct McpLog {
    posts: Arc<Mutex<Vec<Post>>>,
    /// An `initialize` carrying this `X-Tenant-Id` never gets an answer (a hung server, for that tenant).
    hang_initialize_for: Arc<Mutex<Option<String>>>,
}

impl McpLog {
    fn posts(&self) -> Vec<Post> {
        self.posts.lock().unwrap().clone()
    }

    /// The `X-Tenant-Id` of every POST of JSON-RPC `method`.
    fn tenants_of(&self, method: &str) -> Vec<String> {
        self.posts()
            .into_iter()
            .filter(|p| p.method == method)
            .map(|p| p.tenant)
            .collect()
    }
}

fn hdr(h: &axum::http::HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Minimal streamable-HTTP MCP server (JSON responses, no SSE) that records the credential headers per request. The
/// session id it hands out names the licence that opened the session (`sess-<licence>`).
async fn mcp_server(log: McpLog) -> String {
    use axum::http::{HeaderMap, Method, StatusCode};
    use axum::response::IntoResponse;
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::any(move |method: Method, headers: HeaderMap, body: String| {
            let log = log.clone();
            async move {
                if method != Method::POST {
                    return StatusCode::METHOD_NOT_ALLOWED.into_response();
                }
                let req: serde_json::Value = serde_json::from_str(&body).unwrap();
                let m = req["method"].as_str().unwrap_or("").to_string();
                let tenant = hdr(&headers, "x-tenant-id").unwrap_or_default();
                log.posts.lock().unwrap().push(Post {
                    method: m.clone(),
                    tenant: tenant.clone(),
                    strata: hdr(&headers, "x-strata-token").unwrap_or_default(),
                    authorization: hdr(&headers, "authorization"),
                    session: hdr(&headers, "mcp-session-id"),
                });
                let id = req["id"].clone();
                let hang = log.hang_initialize_for.lock().unwrap().clone();
                if m == "initialize" && hang.as_deref() == Some(tenant.as_str()) {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                if m == "tools/call" && req["params"]["name"] == "slow" {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                if m == "tools/call" && req["params"]["name"] == "slow-broken" {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                let reply = match m.as_str() {
                    // A server before 2026-07-28: no server/discover, so the client falls back to initialize.
                    "server/discover" => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
                    "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": req["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "stub", "version": "0"}}}),
                    "tools/call" if req["params"]["name"] == "nope" => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": "unknown tool nope"}}),
                    "tools/list" => {
                        tokio::time::sleep(Duration::from_secs(5)).await; // a hung server
                        json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}})
                    }
                    "tools/call" => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "content": [{"type": "text", "text": "{\"ok\":true}"}], "isError": false}}),
                    _ => return StatusCode::ACCEPTED.into_response(), // notifications
                };
                let mut resp = axum::Json(reply).into_response();
                if m == "initialize" {
                    let licence = tenant.strip_prefix("Bearer ").unwrap_or("none");
                    resp.headers_mut()
                        .insert("mcp-session-id", format!("sess-{licence}").parse().unwrap());
                }
                resp
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

/// The audit's hole: one shared identity for every tenant. Now each tenant's calls carry ITS licence and ITS strata
/// token, on its own session; tenant A's requests never carry B's token, and no request carries `Authorization`.
#[tokio::test]
async fn each_tenant_calls_with_its_own_licence_and_strata_token_on_its_own_session() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(long),
    ));
    let (a, b) = (mcp.for_tenant(A), mcp.for_tenant(B));
    for c in [&a, &b, &a, &b] {
        let v = c.call_tool("order_get_order", json!({})).await.unwrap();
        assert_eq!(v, json!({"ok": true}));
    }

    let bearer = |t: Uuid| format!("Bearer {}", lic(t, 1));
    assert_eq!(
        log.tenants_of("initialize"),
        [bearer(A), bearer(B)],
        "one session per tenant"
    );
    assert_eq!(
        log.tenants_of("tools/call"),
        [bearer(A), bearer(B), bearer(A), bearer(B)]
    );
    for p in log.posts() {
        assert_eq!(p.authorization, None, "no shared identity: {p:?}");
        let t = if p.tenant == bearer(A) { A } else { B };
        assert_eq!(p.tenant, bearer(t), "{p:?}");
        assert_eq!(
            p.strata,
            strata(t, 1),
            "the strata token of the same tenant: {p:?}"
        );
        if let Some(s) = &p.session {
            assert_eq!(
                s,
                &format!("sess-{}", lic(t, 1)),
                "a session is only ever used with the licence that opened it: {p:?}"
            );
        }
    }
    assert_eq!((issuer.hits_for(A), issuer.hits_for(B)), (1, 1));
}

/// No credentials for a tenant: the call errors (retryable, not `auth_rejected`) and nothing is sent for it; other
/// tenants are unaffected and never borrow anyone's credentials.
#[tokio::test]
async fn a_tenant_without_credentials_gets_no_call_and_borrows_none() {
    let issuer = Issuer::default();
    issuer.fail.lock().unwrap().push(B);
    let log = McpLog::default();
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(long),
    ));
    mcp.for_tenant(A)
        .call_tool("order_get_order", json!({}))
        .await
        .unwrap();
    let before = log.posts().len();
    let err = mcp
        .for_tenant(B)
        .call_tool("order_get_order", json!({}))
        .await
        .unwrap_err();
    assert!(err.retryable, "{}", err.message);
    assert!(!err.message.contains(AUTH_REJECTED), "{}", err.message);
    assert!(err.message.contains("mcp credentials"), "{}", err.message);
    assert_eq!(log.posts().len(), before, "nothing was sent for B");
    let err = mcp.list_tools(B).await.unwrap_err();
    assert!(format!("{err:#}").contains("mcp credentials"), "{err:#}");
    assert_eq!(log.posts().len(), before, "nothing was sent for B");
}

/// One tenant's hung `initialize` is bounded by `connect_timeout` and never holds up another tenant's calls.
#[tokio::test]
async fn a_hung_session_open_for_one_tenant_does_not_block_another() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    *log.hang_initialize_for.lock().unwrap() = Some(format!("Bearer {}", lic(A, 1)));
    let mut mcp = HttpMcp::new(mcp_server(log.clone()).await, issuer.creds(long));
    mcp.connect_timeout = Duration::from_millis(700);
    let mcp = Arc::new(mcp);
    let hung = {
        let a = mcp.for_tenant(A);
        tokio::spawn(async move { a.call_tool("order_get_order", json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await; // A is opening its session
    let started = Instant::now();
    mcp.for_tenant(B)
        .call_tool("order_get_order", json!({}))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "B waited on A: {:?}",
        started.elapsed()
    );
    let err = tokio::time::timeout(Duration::from_secs(3), hung)
        .await
        .expect("the connect timeout bounds A")
        .unwrap()
        .unwrap_err();
    assert!(err.retryable, "{}", err.message);
    assert!(
        err.message.contains("no answer within 700 ms"),
        "{}",
        err.message
    );
}

/// Concurrent cold starts of one tenant share one credentials fetch.
#[tokio::test]
async fn concurrent_cold_fetches_of_one_tenant_are_single_flight() {
    let calls = Arc::new(Mutex::new(0usize));
    let n = calls.clone();
    let creds = Arc::new(TenantCredentials::new(FnSource(
        move |t: Uuid, _fresh: bool| {
            *n.lock().unwrap() += 1;
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                McpCredentials::new(&lic(t, 1), &strata(t, 1), Instant::now() + long(1))
            }
        },
    )));
    let mut tasks = vec![];
    for _ in 0..8 {
        let c = creds.clone();
        tasks.push(tokio::spawn(async move { c.get(A).await.unwrap() }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().licence_token(), lic(A, 1));
    }
    creds.get(B).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 2, "one fetch for A, one for B");
}

#[tokio::test]
async fn http_mcp_reconnects_with_the_new_licence_and_classifies_protocol_errors() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    // credentials that are already due: every call fetches a new licence
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(|_| Duration::ZERO),
    ));
    let a = mcp.for_tenant(A);

    let v = a
        .call_tool("order_get_order", json!({"order_id": "X"}))
        .await
        .unwrap();
    assert_eq!(v, json!({"ok": true}));
    a.call_tool("order_get_order", json!({})).await.unwrap();

    let want = [
        format!("Bearer {}", lic(A, 1)),
        format!("Bearer {}", lic(A, 2)),
    ];
    assert_eq!(
        log.tenants_of("initialize"),
        want,
        "new initialize per rotated licence: {:?}",
        log.posts()
    );
    assert_eq!(log.tenants_of("tools/call"), want);

    let err = a.call_tool("nope", json!({})).await.unwrap_err();
    assert!(
        !err.retryable,
        "JSON-RPC error must be permanent: {}",
        err.message
    );
}

#[tokio::test]
async fn licence_rotation_keeps_in_flight_calls_and_the_fresh_session() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    // licence 1 is due at once (the next call rotates); later ones live 300 s
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(|n| {
            if n == 1 {
                Duration::ZERO
            } else {
                Duration::from_secs(300)
            }
        }),
    ));

    let slow = {
        let a = mcp.for_tenant(A);
        tokio::spawn(async move { a.call_tool("slow", json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await; // the slow call is in flight on licence 1
    let a = mcp.for_tenant(A);
    let rotated = a.call_tool("order_get_order", json!({})).await;
    let slow = slow.await.unwrap();
    assert!(
        slow.is_ok(),
        "in-flight call survives rotation: {:?}",
        slow.err().map(|e| e.message)
    );
    assert!(rotated.is_ok(), "{:?}", rotated.err().map(|e| e.message));

    // The fresh session and licence are still in place: no third fetch, no third initialize.
    a.call_tool("order_get_order", json!({})).await.unwrap();
    assert_eq!(
        log.tenants_of("initialize"),
        [
            format!("Bearer {}", lic(A, 1)),
            format!("Bearer {}", lic(A, 2))
        ],
        "{:?}",
        log.posts()
    );
    assert_eq!(issuer.hits(), 2, "fresh licence kept");
}

#[tokio::test]
async fn late_transport_failure_on_a_rotated_out_session_keeps_the_fresh_one() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(|n| {
            if n == 1 {
                Duration::ZERO
            } else {
                Duration::from_secs(300)
            }
        }),
    ));
    let broken = {
        let a = mcp.for_tenant(A);
        tokio::spawn(async move { a.call_tool("slow-broken", json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let a = mcp.for_tenant(A);
    a.call_tool("order_get_order", json!({})).await.unwrap(); // rotates to licence 2
    let err = broken.await.unwrap().unwrap_err();
    assert!(
        err.retryable,
        "transport failure is retryable: {}",
        err.message
    );

    a.call_tool("order_get_order", json!({})).await.unwrap();
    assert_eq!(
        log.tenants_of("initialize"),
        [
            format!("Bearer {}", lic(A, 1)),
            format!("Bearer {}", lic(A, 2))
        ],
        "{:?}",
        log.posts()
    );
    assert_eq!(issuer.hits(), 2, "fresh licence not invalidated");
}

#[tokio::test]
async fn list_tools_is_bounded_by_a_timeout() {
    let mut mcp = HttpMcp::new(
        mcp_server(McpLog::default()).await,
        Issuer::default().creds(long),
    );
    mcp.list_timeout = Duration::from_millis(200);
    let started = Instant::now();
    let err = mcp.list_tools(A).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        format!("{err:#}").contains("no answer within 200 ms"),
        "{err:#}"
    );
}

#[test]
fn http_mcp_from_vars_needs_the_mcp_url_and_carebus_api() {
    let vars = |url: &'static str| {
        move |k: &str| match k {
            "MCP_URL" => Some(url.to_string()),
            _ => Some("http://x".to_string()),
        }
    };
    assert!(HttpMcp::from_vars(vars("http://mcp.example/mcp")).is_ok());
    let err = HttpMcp::from_vars(vars("")).err().unwrap();
    assert!(
        format!("{err:#}").contains("MCP_URL is set but empty"),
        "{err:#}"
    );
    for missing in ["CAREBUS_API_URL", "CAREBUS_API_TOKEN"] {
        let err = HttpMcp::from_vars(|k: &str| (k != missing).then(|| "http://x".into()))
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains(&format!("{missing} not set")),
            "{err:#}"
        );
    }
    // the Keycloak client-credentials variables are gone: nothing else is required
    assert!(
        HttpMcp::from_vars(|k: &str| {
            ["MCP_URL", "CAREBUS_API_URL", "CAREBUS_API_TOKEN"]
                .contains(&k)
                .then(|| "http://x".into())
        })
        .is_ok()
    );
}

/// One request seen by `RotatingMcp`: (HTTP method, JSON-RPC method or "", X-Tenant-Id, status).
type Seen = (String, String, String, u16);

/// A streamable-HTTP MCP server whose accepted licence can be rotated (licences revoked one and care-bus minted
/// another). `GET /mcp` is a long-lived SSE stream that ends when `rotate_to` fires, so the client has to reconnect
/// it. A wrong licence is 401 with `WWW-Authenticate`, as nonagenticai_mcp_server answers.
struct RotatingMcp {
    accepted: Arc<Mutex<String>>,
    restart: tokio::sync::watch::Sender<u32>,
    seen: Arc<Mutex<Vec<Seen>>>,
    url: String,
}

impl RotatingMcp {
    async fn start(accepted: &str) -> Self {
        use axum::body::Body;
        use axum::http::{HeaderMap, Method, StatusCode, header};
        use axum::response::IntoResponse;
        let accepted = Arc::new(Mutex::new(accepted.to_string()));
        let (restart, restart_rx) = tokio::sync::watch::channel(0u32);
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let (acc, log) = (accepted.clone(), seen.clone());
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::any(move |method: Method, headers: HeaderMap, body: String| {
                let (acc, log, mut restart_rx) = (acc.clone(), log.clone(), restart_rx.clone());
                async move {
                    let tenant = hdr(&headers, "x-tenant-id").unwrap_or_default();
                    let req: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                    let m = req["method"].as_str().unwrap_or("").to_string();
                    let ok = tenant == format!("Bearer {}", acc.lock().unwrap())
                        && headers.get(header::AUTHORIZATION).is_none();
                    let status = if !ok {
                        401
                    } else if method == Method::GET || req.get("id").is_some() {
                        200
                    } else {
                        202
                    };
                    log.lock()
                        .unwrap()
                        .push((method.to_string(), m.clone(), tenant, status));
                    if !ok {
                        return (
                            StatusCode::UNAUTHORIZED,
                            [(header::WWW_AUTHENTICATE, "Bearer")],
                        )
                            .into_response();
                    }
                    if method == Method::GET {
                        restart_rx.borrow_and_update();
                        let stream = futures_util::stream::unfold(
                            (0u8, restart_rx),
                            |(n, mut rx)| async move {
                                if n == 0 {
                                    let ping: Result<_, std::convert::Infallible> =
                                        Ok(axum::body::Bytes::from_static(b": ping\n\n"));
                                    return Some((ping, (1, rx)));
                                }
                                let _ = rx.changed().await; // the licence rotated: end the stream
                                None
                            },
                        );
                        return (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            Body::from_stream(stream),
                        )
                            .into_response();
                    }
                    let id = req["id"].clone();
                    let reply = match m.as_str() {
                        // A server before 2026-07-28: no server/discover, so the client falls back to initialize.
                        "server/discover" => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
                        "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": req["params"]["protocolVersion"],
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "rotating", "version": "0"}}}),
                        "tools/call" => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "content": [{"type": "text", "text": "{\"ok\":true}"}], "isError": false}}),
                        _ => return StatusCode::ACCEPTED.into_response(),
                    };
                    let mut resp = axum::Json(reply).into_response();
                    if m == "initialize" {
                        resp.headers_mut()
                            .insert("mcp-session-id", "sess-1".parse().unwrap());
                    }
                    resp
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            accepted,
            restart,
            seen,
            url: format!("http://{addr}/mcp"),
        }
    }

    /// Only `licence` is accepted from now on, and open SSE streams end.
    fn rotate_to(&self, licence: &str) {
        *self.accepted.lock().unwrap() = licence.to_string();
        self.restart.send_modify(|g| *g += 1);
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// (X-Tenant-Id, status) of every `tools/call`.
    fn calls(&self) -> Vec<(String, u16)> {
        self.seen()
            .into_iter()
            .filter(|(_, m, _, _)| m == "tools/call")
            .map(|(_, _, t, s)| (t, s))
            .collect()
    }
}

/// Wait up to `limit` for `pred` over the requests the server has seen.
async fn wait_for(mcp: &RotatingMcp, limit: Duration, pred: impl Fn(&[Seen]) -> bool) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if pred(&mcp.seen()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred(&mcp.seen())
}

fn is_get_with(licence: String, status: u16) -> impl Fn(&Seen) -> bool {
    move |(m, _, tenant, st)| m == "GET" && tenant == &format!("Bearer {licence}") && *st == status
}

/// The SSE stream's reconnect after a 401 fetches fresh credentials for its tenant and recovers, with no restart
/// and no backoff loop on the stale licence.
#[tokio::test]
async fn sse_reconnect_after_a_401_fetches_fresh_credentials_and_recovers_without_a_restart() {
    let issuer = Issuer::default();
    let server = RotatingMcp::start(&lic(A, 1)).await;
    let mut mcp = HttpMcp::new(server.url.clone(), issuer.creds(long));
    mcp.sse_retry_base = Duration::from_millis(50);
    let mcp = Arc::new(mcp);
    let a = mcp.for_tenant(A);

    a.call_tool("order_get_order", json!({})).await.unwrap();
    assert!(
        wait_for(&server, Duration::from_secs(3), |s| s
            .iter()
            .any(is_get_with(lic(A, 1), 200)))
        .await,
        "the SSE stream opens with the licence: {:?}",
        server.seen()
    );

    server.rotate_to(&lic(A, 2));
    // No tool call in between: the stream's own reconnect has to recover.
    let recovered = wait_for(&server, Duration::from_secs(5), |s| {
        s.iter().any(is_get_with(lic(A, 2), 200))
    })
    .await;
    let seen = server.seen();
    assert!(
        recovered,
        "GET /mcp never reconnected with fresh credentials: {seen:?}"
    );
    let stale_401s = seen
        .iter()
        .filter(|s| is_get_with(lic(A, 1), 401)(s))
        .count();
    assert!(
        stale_401s <= 1,
        "one 401, then fresh credentials, not a backoff loop: {seen:?}"
    );

    let v = a.call_tool("order_get_order", json!({})).await;
    assert!(v.is_ok(), "{:?}", v.err().map(|e| e.message));
    assert_eq!(
        *issuer.fetches.lock().unwrap(),
        [(A, false), (A, true)],
        "exactly one fresh fetch"
    );
}

/// A POST answered 401 is retried once with fresh credentials: the first call after the rotation succeeds.
#[tokio::test]
async fn a_tool_call_answered_401_is_retried_once_with_fresh_credentials() {
    let issuer = Issuer::default();
    let server = RotatingMcp::start(&lic(A, 1)).await;
    let mut mcp = HttpMcp::new(server.url.clone(), issuer.creds(long));
    mcp.sse_retry_base = Duration::from_secs(60); // keep the stream's reconnect out of this test
    let a = Arc::new(mcp).for_tenant(A);

    a.call_tool("order_get_order", json!({})).await.unwrap();
    server.rotate_to(&lic(A, 2));
    let v = a.call_tool("order_get_order", json!({})).await;
    assert!(
        v.is_ok(),
        "first call after rotation: {:?}",
        v.err().map(|e| e.message)
    );
    let bearer = |n| format!("Bearer {}", lic(A, n));
    assert_eq!(
        server.calls(),
        [(bearer(1), 200), (bearer(1), 401), (bearer(2), 200)]
    );
    assert_eq!(issuer.hits(), 2);
}

/// The session open (`server/discover`) refused with the cached and with the fresh licence (the tenant holds no valid tenant-level care-bus
/// licence): the call fails at once with the non-retryable `auth_rejected`, after exactly one fresh fetch.
#[tokio::test]
async fn a_session_open_refused_with_fresh_credentials_too_is_a_non_retryable_auth_rejected_error()
{
    let issuer = Issuer::default();
    let server = RotatingMcp::start("a-licence-nobody-has").await;
    let mcp = Arc::new(HttpMcp::new(server.url.clone(), issuer.creds(long)));
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        mcp.for_tenant(A).call_tool("order_get_order", json!({})),
    )
    .await
    .expect("an auth rejection fails fast")
    .unwrap_err();
    assert!(!err.retryable, "{}", err.message);
    assert!(
        err.message.contains(AUTH_REJECTED) && err.message.contains("HTTP 401"),
        "{}",
        err.message
    );
    assert_eq!(*issuer.fetches.lock().unwrap(), [(A, false), (A, true)]);
    let inits: Vec<(String, u16)> = server
        .seen()
        .into_iter()
        // The session opens with server/discover (crate::mcp_tasks::lifecycle); that is what is refused.
        .filter(|(_, m, _, _)| m == "server/discover")
        .map(|(_, _, t, s)| (t, s))
        .collect();
    assert_eq!(
        inits,
        [
            (format!("Bearer {}", lic(A, 1)), 401),
            (format!("Bearer {}", lic(A, 2)), 401)
        ],
        "refused, then refused again with the fresh licence"
    );
}

/// The same on a `tools/call` of an open session: refused with the cached and the fresh licence ⇒ `auth_rejected`.
#[tokio::test]
async fn a_tool_call_refused_with_fresh_credentials_too_is_non_retryable() {
    let issuer = Issuer::default();
    let server = RotatingMcp::start(&lic(A, 1)).await;
    let mut mcp = HttpMcp::new(server.url.clone(), issuer.creds(long));
    mcp.sse_retry_base = Duration::from_secs(60); // keep the stream's reconnect out of this test
    let a = Arc::new(mcp).for_tenant(A);

    a.call_tool("order_get_order", json!({})).await.unwrap();
    server.rotate_to("a-licence-this-tenant-never-gets");
    let err = a.call_tool("order_get_order", json!({})).await.unwrap_err();
    assert!(!err.retryable, "{}", err.message);
    assert!(err.message.contains(AUTH_REJECTED), "{}", err.message);
    let bearer = |n| format!("Bearer {}", lic(A, n));
    assert_eq!(
        server.calls(),
        [(bearer(1), 200), (bearer(1), 401), (bearer(2), 401)]
    );
    // Not `== 2`: dropping the refused session may DELETE it with a further renewal in the background.
    assert!(issuer.hits() >= 2);
}

#[derive(Clone, Default)]
struct ApiLog {
    /// (Authorization, body) per request.
    seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

/// Stub carebus-api `POST /api/internal/mcp-credentials`: service token `svc` only; tenant B is answered with A's
/// credentials (a misbehaving server the client must not trust).
async fn credentials_api(log: ApiLog) -> String {
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    let app = axum::Router::new().route(
        "/api/internal/mcp-credentials",
        axum::routing::post(
            move |headers: HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                let log = log.clone();
                async move {
                    let auth = hdr(&headers, "authorization").unwrap_or_default();
                    log.seen.lock().unwrap().push((auth.clone(), body.clone()));
                    if auth != "Bearer svc" {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    let asked: Uuid = body["tenant_id"].as_str().unwrap().parse().unwrap();
                    let answered = if asked == B { A } else { asked };
                    axum::Json(json!({
                        "tenant_id": answered,
                        "licence_token": format!("lic-{}", short(answered)),
                        "strata_token": format!("str-{}", short(answered)),
                        "expires_at": "2099-01-01T00:00:00Z",
                        "expires_in": 900,
                    }))
                    .into_response()
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// The agents' source: carebus-api's service-token route, asked for exactly the tenant at hand; an answer naming
/// another tenant, or a refusal, is an error (no credentials, so no MCP call).
#[tokio::test]
async fn the_api_source_asks_carebus_api_for_the_tenant_and_trusts_only_its_own_answer() {
    let log = ApiLog::default();
    let base = credentials_api(log.clone()).await;
    let src = ApiCredentialSource::new(&format!("{base}/"), "svc".into());

    let c = src.fetch(A, false).await.unwrap();
    assert_eq!(c.licence_token(), "lic-a");
    let [(_, licence), (_, strata_token)] = c.headers();
    assert_eq!(licence.to_str().unwrap(), "Bearer lic-a");
    assert_eq!(strata_token.to_str().unwrap(), "str-a");
    let left = c.expires.saturating_duration_since(Instant::now());
    assert!(
        left > Duration::from_secs(890) && left <= Duration::from_secs(900),
        "{left:?}"
    );

    src.fetch(A, true).await.unwrap();
    let err = src.fetch(B, false).await.unwrap_err();
    assert!(format!("{err:#}").contains("another tenant"), "{err:#}");
    assert_eq!(
        *log.seen.lock().unwrap(),
        [
            (
                "Bearer svc".to_string(),
                json!({"tenant_id": A, "fresh": false})
            ),
            (
                "Bearer svc".to_string(),
                json!({"tenant_id": A, "fresh": true})
            ),
            (
                "Bearer svc".to_string(),
                json!({"tenant_id": B, "fresh": false})
            ),
        ]
    );

    let wrong = ApiCredentialSource::new(&base, "not-the-service-token".into());
    let err = wrong.fetch(A, false).await.unwrap_err();
    assert!(format!("{err:#}").contains("refused"), "{err:#}");
}
