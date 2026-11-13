//! Plan 05 Task 15 (decision D2): one real JSON response per route, written into the web app's source tree.
//! `UPDATE_FIXTURES=1 cargo test -p carebus-api --test it contract_fixtures` rewrites web/src/lib/api/fixtures/*.json;
//! without it the test compares the SHAPE of every live response with the checked-in file.
//!
//! The demo state is built the way production builds it: cases go through the recorder, the dead letter comes out of
//! `Bus::consume`, tool latency is written by `recorder::handle_tool`, chat goes through the conversation routes and
//! the SSE frames are read off the real Pub/Sub channels. Only the three upstreams (strata_rag, demo-backends, MCP)
//! are stubs, with the real upstream shapes (plans 02, 03 and 04). Nothing here talks to a live service.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::{
    Json, Router,
    routing::{get, post},
};
use carebus::bus::ConsumeOptions;
use carebus::case::CaseStore;
use carebus::domain::{
    Outcome, QualifiedRequest, REQUEST_QUALIFIED, REQUEST_RESOLVED, RequestType, Resolution, Slots,
    TOOL_RESPONSE, ToolResponse, WORK_REQUESTED, WorkRequest, static_plan,
};
use carebus::mcp_http::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use carebus::{Bus, Envelope};
use futures_util::StreamExt;
use serde_json::{Value, json};

const CASE: &str = "CASE-0001";
const DEAD_CASE: &str = "CASE-0002";
const AREA: &str = "order-support";

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

/// Fake strata_rag + demo-backends with the REAL upstream shapes (plans 02 and 04).
async fn upstreams() -> String {
    let router = "7f1c9a2e-0000-4000-8000-000000000001";
    let leaf = "7f1c9a2e-0000-4000-8000-000000000002";
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        // strata_rag
        .route(
            "/api/stores",
            get(move || async move {
                Json(json!({"stores": [
                    {"id": router, "parent_id": null, "kind": "router", "name": "care",
                     "description": "All customer-care knowledge", "token_count": 0, "chunk_count": 0, "children": [leaf]},
                    {"id": leaf, "parent_id": router, "kind": "leaf", "name": "returns",
                     "description": "Returns and refunds", "token_count": 18400, "chunk_count": 62, "children": []}]}))
            })
            .post(move |Json(b): Json<Value>| async move {
                (StatusCode::CREATED, Json(json!({"id": "7f1c9a2e-0000-4000-8000-000000000003", "parent_id": b["parent_id"],
                    "kind": b["kind"], "name": b["name"], "description": b["description"],
                    "token_count": 0, "chunk_count": 0, "children": []})))
            }),
        )
        // the store kind is read when a source is saved
        .route(
            "/api/stores/{id}",
            get(move |axum::extract::Path(id): axum::extract::Path<String>| async move {
                if id == leaf {
                    Json(json!({"id": leaf, "parent_id": router, "kind": "leaf", "name": "returns",
                        "description": "Returns and refunds", "token_count": 18400, "chunk_count": 62, "children": []}))
                    .into_response()
                } else {
                    (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "not_found", "message": "no such store"}})))
                        .into_response()
                }
            }),
        )
        // Task 11b: strata_rag's own document shape and `{items, next}` paging.
        .route(
            "/api/stores/{id}/documents",
            get(move || async move {
                Json(json!({"items": [{
                    "id": "7f1c9a2e-0000-4000-8000-0000000000d1", "store_id": leaf, "source": "policies/returns.md",
                    "title": "Returns policy", "content_hash": "9b74c9897bac770ffc029102a200c5de",
                    "token_count": 1840, "chunk_count": 6, "created_at": "2026-10-10T09:00:00Z"}],
                    "next": "eyJvIjoxfQ"}))
            }),
        )
        // strata_rag origin/main `query::QueryResult`: citations (`ord`, pages, `source`, `title`) and provenance.
        .route(
            "/api/query",
            post(move |Json(b): Json<Value>| async move {
                assert!(b["query"].is_string(), "the proxy passes the body through");
                Json(json!({
                    "results": [{"chunk_id": "7f1c9a2e-0000-4000-8000-0000000000c1",
                        "document_id": "7f1c9a2e-0000-4000-8000-0000000000d1", "store_path": ["care", "returns"],
                        "ord": 3, "text": "Refunds are issued within 5 working days.",
                        "page_start": 2, "page_end": 3,
                        "source": "policies/returns.pdf", "title": "Returns policy",
                        "scores": {"dense": 0.81, "lexical": 0.44, "rerank": 0.93},
                        "source_id": "7f1c9a2e-0000-4000-8000-0000000000e1", "source_run": null,
                        "metadata": {}}],
                    "routing": {"mode_used": "hybrid", "leaves": [leaf], "laya_consulted": true, "laya_fallback": null},
                    "timings_ms": {"embed": 12, "route": 30, "search": 18, "rerank": 95, "total": 160}}))
            }),
        )
        // demo-backends (plan 02)
        .route(
            "/api/admin/records/{table}",
            get(|| async {
                Json(json!({"table": "orders", "primary_key": "order_id", "editable": ["status"],
                    "columns": ["order_id", "customer_id", "status"],
                    "rows": [{"order_id": "ORD-58790", "customer_id": "C-10482", "status": "in_transit"}], "total": 1}))
            }),
        )
        .route(
            "/api/admin/records/{table}/{id}",
            get(|| async {
                Json(json!({"order_id": "ORD-58790", "customer_id": "C-10482", "status": "in_transit"}))
            }),
        )
        .route(
            "/api/admin/scenarios",
            get(|| async {
                Json(json!({"scenarios": [{"name": "duplicate-charge",
                    "description": "Restore Anna's duplicate capture on ORD-58790"}]}))
            }),
        )
        .route(
            "/api/admin/faults",
            get(|| async {
                Json(json!({"faults": {"delivery_track_shipment": {"mode": "timeout", "remaining": 1}}}))
            }),
        );
    serve(app).await
}

/// A minimal streamable-HTTP MCP server (and its `/api/health`), called with fixed per-tenant credentials.
async fn stub_mcp() -> (HttpMcp, String) {
    let mcp_base = serve(
        Router::new()
            .route("/api/health", get(|| async { Json(json!({"status": "ok"})) }))
            .route(
                "/mcp",
                axum::routing::any(|method: Method, body: String| async move {
                    if method != Method::POST {
                        return StatusCode::METHOD_NOT_ALLOWED.into_response();
                    }
                    let req: Value = serde_json::from_str(&body).unwrap();
                    let id = req["id"].clone();
                    let reply = match req["method"].as_str().unwrap_or("") {
                        // A server before 2026-07-28: no server/discover, so the client falls back to initialize.
                        "server/discover" => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
                        "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": req["params"]["protocolVersion"],
                            "capabilities": {"tools": {}}, "serverInfo": {"name": "stub", "version": "0"}}}),
                        "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                            {"name": "order_get_order", "description": "Get one order with lines, status and shipment id.",
                             "inputSchema": {"type": "object", "required": ["order_id"],
                                             "properties": {"order_id": {"type": "string"}}}},
                            {"name": "faq_ask", "description": "Answer a customer question from the knowledge base.",
                             "inputSchema": {"type": "object"}}]}}),
                        _ => return StatusCode::ACCEPTED.into_response(),
                    };
                    let mut resp = Json(reply).into_response();
                    if req["method"] == "initialize" {
                        resp.headers_mut().insert("mcp-session-id", "s".parse().unwrap());
                    }
                    resp
                }),
            ),
    )
    .await;
    (
        HttpMcp::new(
            format!("{mcp_base}/mcp"),
            TenantCredentials::new(FnSource(|tenant: uuid::Uuid, _fresh: bool| async move {
                McpCredentials::new(
                    &format!("lic-{tenant}"),
                    &format!("str-{tenant}"),
                    std::time::Instant::now() + Duration::from_secs(300),
                )
            })),
        ),
        mcp_base,
    )
}

async fn until<F, Fut>(what: &str, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..150 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Produce a dead letter the real way: a handler that always fails, until `Bus::consume` gives up.
async fn dead_letter_via_consume(bus: &Bus) {
    let work = WorkRequest {
        work_id: format!("{DEAD_CASE}:0:order:1"),
        idempotency_key: format!("{DEAD_CASE}:0:order"),
        case_id: DEAD_CASE.into(),
        phase: 0,
        as_name: "order".into(),
        worker: "order".into(),
        action: "get_order".into(),
        args: json!({"order_id": "ORD-58213"}),
        attempt: 1,
        request_type: RequestType::r#return(),
        side_effect: None,
    };
    let origin = bus.streams.worker(AREA, "order");
    let env = Envelope::new(
        &format!("{AREA}.returns.{WORK_REQUESTED}"),
        "orchestrator",
        DEAD_CASE,
        &work,
    )
    .unwrap();
    bus.publish(&origin, &env).await.unwrap();
    let opts = ConsumeOptions {
        batch: 16,
        block: Duration::from_millis(100),
        reclaim_idle: Duration::from_millis(200),
        max_deliveries: 2,
    };
    let b = bus.clone();
    let task = tokio::spawn(async move {
        b.consume(
            &origin,
            "worker-order",
            "worker-order-1",
            opts,
            |_env| async { anyhow::bail!("always fails") },
        )
        .await
    });
    let dead = bus.streams.dead();
    until("Bus::consume to dead-letter", || async {
        let n: u64 = redis::cmd("XLEN")
            .arg(&dead)
            .query_async(&mut bus.conn())
            .await
            .unwrap_or(0);
        n == 1
    })
    .await;
    task.abort();
}

/// What the probes measure varies from run to run; the keys do not.
fn stable_latency(mut v: Value) -> Value {
    assert!(
        v["latency_ms"].is_number(),
        "probe result has latency_ms: {v}"
    );
    v["latency_ms"] = json!(42);
    v
}

fn urlencode(s: &str) -> String {
    s.replace(':', "%3A")
}

#[tokio::test]
async fn every_route_matches_its_checked_in_fixture() {
    let up = upstreams().await;
    let (mcp, mcp_base) = stub_mcp().await;
    let (ingest_url, ingest) = crate::common::fakes::start().await;
    let sample = crate::common::fakes::sample_api().await;
    let mut e = crate::common::env_with("fixtures", |c| {
        c.strata_rag_url = Some(up.clone());
        c.strata_ingest_url = Some(ingest_url.clone());
        c.strata_ingest_token = Some(crate::common::fakes::INGEST_TOKEN.into());
        c.source_fetch_allow_loopback = true; // the customer API stub listens on 127.0.0.1
        c.demo_backends_url = Some(up.clone());
        // never the developer's MCP env: the overview and /api/tools both talk to this stub
        c.mcp_url = Some(format!("{mcp_base}/mcp"));
    })
    .await;
    e.state.mcp = Some(Arc::new(mcp));
    let state = e.state.clone();
    let app = carebus_api::router(state.clone());
    // The bootstrap job's bus: every job route below reads exactly these keys. Its prefix (environment + scope)
    // is what the routes strip, so the fixtures stay production-shaped (`bus:customer:…`).
    let scope = carebus::Scope {
        tenant: crate::common::BOOT_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    };
    let bus = state.bus.scoped(&scope);
    // The recorder writes in the envelope's scope, exactly as its per-scope task does.
    let ids = carebus_api::recorder::ScopeIds::resolve(&state, bus.clone(), &scope)
        .await
        .unwrap();
    let prefix = bus.streams.key("");
    crate::common::FIXTURE_PREFIX.set(prefix).ok();
    let viewer = crate::common::user();
    let admin = crate::common::admin();

    // Pub/Sub taps first, so the SSE frames below are what bus.publish / chat really mirrored.
    let mut tap = bus.client().get_async_pubsub().await.unwrap();
    tap.subscribe(bus.streams.console()).await.unwrap();
    tap.subscribe(carebus::chat::chat_channel(&bus))
        .await
        .unwrap();
    let chat_channel = carebus::chat::chat_channel(&bus);
    let mut frames = tap.into_on_message();

    // The three consumer groups that read the customer stream in production. The recorder group runs the real
    // recorder handler, so case_history is written exactly as it is in production.
    let customer = bus.streams.customer(AREA);
    let mut consumers = Vec::new();
    for (group, record) in [
        (carebus::interaction::GROUP, false),
        (carebus::orchestrator::GROUP, false),
        (carebus_api::recorder::GROUP, true),
    ] {
        let (b, s, ids, stream) = (bus.clone(), state.clone(), ids.clone(), customer.clone());
        consumers.push(tokio::spawn(async move {
            let opts = ConsumeOptions {
                block: Duration::from_millis(100),
                ..ConsumeOptions::default()
            };
            b.consume(&stream, group, &format!("{group}-1"), opts, move |env| {
                let (s, ids) = (s.clone(), ids.clone());
                async move {
                    if record {
                        carebus_api::recorder::handle_customer(&s, &ids, &env)
                            .await
                            .map_err(anyhow::Error::from)?;
                    }
                    Ok(())
                }
            })
            .await
        }));
    }

    let get = |path: &str| {
        let (app, v, path) = (app.clone(), viewer.clone(), path.to_string());
        async move {
            let (s, b) = crate::common::send(&app, crate::common::get(&path, Some(&v))).await;
            assert!(s.is_success(), "{path}: {s} {b}");
            b
        }
    };

    // --- a conversation that really publishes a case (older than CASE-0001, still open) -----------------------
    let (s, conv) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&viewer),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{conv}");
    let conv_id = conv["conversation_id"].as_str().unwrap().to_string();
    let say = |text: &'static str| {
        let (app, v, id) = (app.clone(), viewer.clone(), conv_id.clone());
        async move {
            crate::common::send(
                &app,
                crate::common::json(
                    "POST",
                    &crate::common::j(&format!("/conversations/{id}/messages")),
                    Some(&v),
                    json!({"text": text}),
                ),
            )
            .await
        }
    };
    let (_, first) = say("I want to return ORD-58213").await;
    assert_eq!(first["turn"], "ask", "the reason is still missing: {first}");
    // The reply can arrive before the background drainer releases its lock. This fixture
    // needs a new synchronous turn, rather than the valid 202 response for a queued message.
    until("first conversation turn to finish", || async {
        let held: bool = redis::cmd("EXISTS")
            .arg(carebus::chat::lock_key(&bus, &conv_id))
            .query_async(&mut bus.conn())
            .await
            .unwrap();
        !held
    })
    .await;
    let (s, published) = say("It is too small").await;
    assert_eq!(s, StatusCode::OK, "{published}");
    assert_eq!(published["turn"], "published", "{published}");
    let conv_case = published["case_id"].as_str().unwrap().to_string();

    // --- a dead letter, through Bus::consume (another case, so CASE-0001's trace stays its own) ---------------
    dead_letter_via_consume(&bus).await;

    // --- one finished case: bus entries, orchestrator hash, and the recorder's case_history row ----------------
    let cases = CaseStore::new(bus.clone());
    let req = QualifiedRequest {
        case_id: CASE.into(),
        customer_id: "C-10482".into(),
        request_type: RequestType::r#return(),
        area: AREA.into(),
        context_space: "retail:order-support:returns".into(),
        slots: Slots::of(&[("order_id", "ORD-58213"), ("reason", "too uncomfortable")]),
        utterance: "I want to return my headphones".into(),
        gates: None,
    };
    assert!(cases.create(&req).await.unwrap());
    cases
        .set_plan(CASE, &static_plan("return").expect("built-in return plan"))
        .await
        .unwrap();
    cases.init_pending(CASE, 0, 2).await.unwrap();
    cases
        .record_result(
            CASE,
            0,
            "order",
            &json!({"order_id": "ORD-58213", "category": "audio", "status": "delivered"}),
        )
        .await
        .unwrap();
    for (work_id, action, attempt, status, error) in [
        (
            format!("{CASE}:0:customer:1"),
            "verify_customer",
            1,
            "succeeded",
            Value::Null,
        ),
        (
            format!("{CASE}:0:order:1"),
            "get_order",
            1,
            "failed",
            json!("timeout"),
        ),
        (
            format!("{CASE}:0:order:2"),
            "get_order",
            2,
            "succeeded",
            Value::Null,
        ),
    ] {
        cases
            .set_work_status(
                CASE,
                &work_id,
                &json!({"work_id": work_id, "worker": if action == "get_order" { "order" } else { "crm" },
                        "action": action, "attempt": attempt, "status": status, "error": error}),
            )
            .await
            .unwrap();
    }
    let resolution = Resolution {
        case_id: CASE.into(),
        request_type: RequestType::r#return(),
        outcome: Outcome::Completed,
        reason: None,
        facts: serde_json::from_value(json!({"rma_id": "RMA-1042"})).unwrap(),
    };
    cases.set_resolution(CASE, &resolution).await.unwrap();
    cases.mark_resolved(CASE, "completed").await.unwrap();
    // Newer than everything above, so CASE-0001 heads the (newest first) case list; resolved 4.2 s later, so the
    // duration and the median are exact. Both land a moment in the future of the wall clock, which nothing reads.
    let t0 = now_ms() + 1_000;
    let mut q = Envelope::new(&req.topic(REQUEST_QUALIFIED), "interaction", CASE, &req).unwrap();
    q.published_at_ms = t0;
    let mut r = Envelope::new(
        &req.topic(REQUEST_RESOLVED),
        "orchestrator",
        CASE,
        &resolution,
    )
    .unwrap();
    r.published_at_ms = t0 + 4_200;
    bus.publish(&customer, &q).await.unwrap();
    bus.publish(&customer, &r).await.unwrap();

    // --- audited admin writes -----------------------------------------------------------------------------------
    // A connector pointed at the stub, so its test is a real probe of something local.
    let (s, connector) = crate::common::send(
        &app,
        crate::common::json("POST", &crate::common::j("/connectors"), Some(&admin), json!({
            "id": "oms", "name": "Order management", "system": "OMS", "base_url": format!("{up}/api"),
            "auth_type": "api_key", "secret_ref": "carebus-connectors/oms-api-key", "timeout_ms": 2500,
            "rate_limit": "80 req/s", "mappings": [{"backend_field": "ord_no", "attribute": "Order.order_id"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{connector}");
    let (s, connector_test) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/connectors/oms/test"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{connector_test}");
    assert_eq!(
        connector_test["ok"], true,
        "the stub answers /health: {connector_test}"
    );

    // An LLM provider bound to `gates` (not `qualify`: a later chat turn would call it), so the interaction agent's
    // `model_routes` (its bound LLM uses) is not empty and the agent fixtures keep their array shape. Written by the
    // owner pool: no token, no probe.
    crate::common::bind_llm_use(crate::common::pool(&e), "gates").await;

    // Tool latency exactly as the recorder writes it (one `execute`/`tool` audit row with details.latency_ms).
    // Written last so the newest audit row is the one that carries a request_id.
    let tool = ToolResponse {
        call_id: format!("{CASE}:0:order:2"),
        mcp_tool: "order_get_order".into(),
        ok: true,
        result: Some(json!({"order_id": "ORD-58213"})),
        error: None,
        retryable: false,
        latency_ms: 240,
    };
    let env = Envelope::new("tool.response", "tool-executor", CASE, &tool).unwrap();
    assert_eq!(env.event(), Some(TOOL_RESPONSE));
    carebus_api::recorder::handle_tool(&state, &ids, &env)
        .await
        .unwrap();

    // --- wait until the consumer groups have caught up and the recorder has written both cases -----------------
    let info = |key: &'static str| {
        let bus = bus.clone();
        async move {
            carebus_api::redis_admin::stream_info(&bus, key)
                .await
                .unwrap()
        }
    };
    until("every group on the customer stream is drained", || async {
        let i = info("bus:customer:order-support").await;
        i.groups.len() == 3
            && i.groups
                .iter()
                .all(|g| g.consumers == 1 && g.pending == 0 && g.lag == Some(0))
    })
    .await;
    until("the recorder wrote both cases", || async {
        let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT case_id, outcome, conversation_id FROM case_history WHERE case_id = ANY($1)",
        )
        .bind(vec![CASE.to_string(), conv_case.clone()])
        .fetch_all(crate::common::pool(&e))
        .await
        .unwrap();
        rows.len() == 2
            && rows.iter().all(|(id, outcome, conv)| {
                if id == CASE {
                    outcome.is_some()
                } else {
                    conv.is_some()
                }
            })
    })
    .await;
    for c in &consumers {
        c.abort();
    }

    // --- capture --------------------------------------------------------------------------------------------------
    let key = urlencode("bus:customer:order-support");
    crate::common::fixture(
        "health",
        &crate::common::send(&app, crate::common::get("/health", None))
            .await
            .1,
    );
    crate::common::fixture("overview", &get(&crate::common::j("/overview")).await);
    crate::common::fixture("agents", &get(&crate::common::j("/agents")).await);
    crate::common::fixture(
        "agent",
        &get(&crate::common::j("/agents/interaction")).await,
    );
    crate::common::fixture("streams", &get(&crate::common::j("/streams")).await);
    crate::common::fixture(
        "stream",
        &get(&crate::common::j(&format!("/streams/{key}"))).await,
    );
    crate::common::fixture(
        "stream_entries",
        &get(&crate::common::j(&format!(
            "/streams/{key}/entries?count=5"
        )))
        .await,
    );
    crate::common::fixture(
        "dead_letters",
        &get(&crate::common::j("/dead-letters")).await,
    );
    crate::common::fixture("cases", &get(&crate::common::j("/cases")).await);
    crate::common::fixture(
        "case",
        &get(&crate::common::j(&format!("/cases/{CASE}"))).await,
    );
    crate::common::fixture("connectors", &get(&crate::common::j("/connectors")).await);
    crate::common::fixture(
        "connector",
        &get(&crate::common::j("/connectors/oms")).await,
    );
    crate::common::fixture("connector_test", &stable_latency(connector_test));
    crate::common::fixture(
        "context_spaces",
        &get(&crate::common::j("/context-spaces")).await,
    );
    crate::common::fixture(
        "ontology_entities",
        &get(&crate::common::j("/ontology/entities")).await,
    );
    crate::common::fixture(
        "ontology_relations",
        &get(&crate::common::j("/ontology/relations")).await,
    );
    crate::common::fixture("procedures", &get(&crate::common::j("/procedures")).await);
    crate::common::fixture(
        "procedure_version",
        &get(&crate::common::j("/procedures/return/versions/1.3.0")).await,
    );
    crate::common::fixture(
        "procedure_published",
        &get(&crate::common::j("/procedures/return?status=PUBLISHED")).await,
    );
    crate::common::fixture(
        "analytics",
        &get(&crate::common::j("/analytics?days=7")).await,
    );
    crate::common::fixture("audit", &get("/api/audit").await);
    crate::common::fixture("tools", &get(&crate::common::j("/tools")).await);
    crate::common::fixture("knowledge_stores", &get("/api/knowledge/stores").await);
    let store = "7f1c9a2e-0000-4000-8000-000000000002";
    crate::common::fixture(
        "knowledge_documents",
        &get(&format!("/api/knowledge/stores/{store}/documents?limit=1")).await,
    );
    let (s, query) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            "/api/knowledge/query",
            Some(&viewer),
            json!({"query": "how long does a refund take", "route_mode": "hybrid"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{query}");
    crate::common::fixture("knowledge_query", &query);
    crate::common::fixture(
        "demo_records",
        &get(&crate::common::j("/demo/records/orders?limit=50&offset=0")).await,
    );
    crate::common::fixture(
        "demo_record",
        &get(&crate::common::j("/demo/records/orders/ORD-58790")).await,
    );
    crate::common::fixture(
        "demo_scenarios",
        &get(&crate::common::j("/demo/scenarios")).await,
    );
    crate::common::fixture("demo_faults", &get(&crate::common::j("/demo/faults")).await);
    // Written after `ontology_relations` and `audit` are captured, so those fixtures keep the seeded state.
    let (s, relation) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/relations"),
            Some(&admin),
            json!({"subject": "Customer", "predicate": "files", "object": "WarrantyClaim", "cardinality": "1:n"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{relation}");
    crate::common::fixture("ontology_relation", &relation);
    // A tenant override of a base context space (after `context_spaces` is captured).
    let (s, space) = crate::common::send(
        &app,
        crate::common::json(
            "PUT",
            &crate::common::j("/context-spaces/retail:faq:general"),
            Some(&admin),
            json!({"label": "Frequently asked", "required": ["question", "order_id"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{space}");
    crate::common::fixture("context_space", &space);

    // --- Sources, ingest jobs and store writes (plan 2026-10-16) -------------------------------------------------
    // The shared normaliser may rewrite the `source` fixture's URL host to a private-looking one: fixtures pin shape, not values.
    let post = |uri: String, body: Value| {
        let (app, admin) = (app.clone(), admin.clone());
        async move {
            crate::common::send(&app, crate::common::json("POST", &uri, Some(&admin), body)).await
        }
    };
    let leaf = "7f1c9a2e-0000-4000-8000-000000000002";
    let (s, api_src) = post(
        crate::common::j("/sources"),
        json!({
            "name": "Customer API", "type": "api", "target_store_id": leaf, "schedule": "0 3 * * *",
            "config": {"url": format!("{}/customers", sample.url), "auth": {"type": "header", "header_name": "X-Api-Key"}},
            "mapping": [{"source_field": "customer_id", "entity": "Customer", "attribute": "customer_id"},
                        {"source_field": "email", "entity": "Customer", "attribute": "email"}],
            "credentials": {"secret": crate::common::fakes::API_KEY}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{api_src}");
    let api_id = api_src["id"].as_str().unwrap().to_string();
    let (s, web_src) = post(
        crate::common::j("/sources"),
        json!({
            "name": "Help centre", "type": "website", "target_store_id": leaf,
            "config": {"urls": ["https://help.example.nl/returns"]}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{web_src}");
    let web_id = web_src["id"].as_str().unwrap().to_string();

    // strata_ingest answers a source fetch failure with 202 and a `failed` job: the run is final at once
    ingest.lock().unwrap().source_failure =
        Some(("source_auth".into(), "the source answered HTTP 401".into()));
    let (s, refused) = post(
        crate::common::j(&format!("/sources/{api_id}/resync")),
        json!({}),
    )
    .await;
    assert_eq!(
        (s, refused["status"].clone()),
        (StatusCode::OK, json!("failed")),
        "{refused}"
    );
    ingest.lock().unwrap().source_failure = None;
    let (s, started) = post(
        crate::common::j(&format!("/sources/{web_id}/resync")),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{started}");
    crate::common::fixture("source_run", &started);
    let job = started["ingest_job_ids"][0].as_str().unwrap().to_string();
    crate::common::fakes::complete(&ingest, &job, &[crate::common::fakes::doc_id(1)]);
    let runs = get(&crate::common::j(&format!("/sources/{web_id}/runs"))).await;
    assert_eq!(runs["items"][0]["status"], "completed", "{runs}");
    crate::common::fixture("source_runs", &runs);
    let sources = get(&crate::common::j("/sources")).await;
    let last: Vec<&Value> = sources["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["last_run"]["status"])
        .collect();
    assert_eq!(last, [&json!("failed"), &json!("completed")], "{sources}");
    crate::common::fixture("sources", &sources);
    crate::common::fixture(
        "source",
        &get(&crate::common::j(&format!("/sources/{api_id}"))).await,
    );
    let (s, tested) = post(
        crate::common::j(&format!("/sources/{api_id}/test")),
        json!({}),
    )
    .await;
    assert_eq!(
        (s, tested["ok"].clone()),
        (StatusCode::OK, json!(true)),
        "{tested}"
    );
    crate::common::fixture("source_test", &tested);
    let csv = b"customer_id;name;email\nC-10482;Anna de Vries;anna@example.nl\n";
    let (s, preview) = crate::common::send(
        &app,
        crate::common::multipart(
            &crate::common::j("/sources/preview-columns"),
            Some(&admin),
            &[crate::common::Part {
                name: "file",
                file_name: Some("customers.csv"),
                content_type: Some("text/csv"),
                body: csv,
            }],
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{preview}");
    crate::common::fixture("source_preview", &preview);
    crate::common::fixture("ingest_jobs", &get("/api/ingest/jobs").await);
    crate::common::fixture("ingest_job", &get(&format!("/api/ingest/jobs/{job}")).await);
    let (s, store) = post(
        "/api/knowledge/stores".into(),
        json!({"parent_id": "7f1c9a2e-0000-4000-8000-000000000001", "kind": "leaf", "name": "warranty",
               "description": "Warranty terms"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{store}");
    crate::common::fixture("knowledge_store", &store);
    let del = axum::http::Request::delete(crate::common::j(&format!("/sources/{api_id}")))
        .header("authorization", format!("Bearer {admin}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let (s, deleted) = crate::common::send(&app, del).await;
    assert_eq!(s, StatusCode::OK, "{deleted}");
    crate::common::fixture("source_delete", &deleted);
    for body in [&api_src, &started, &runs, &tested, &sources] {
        assert!(
            !body.to_string().contains(crate::common::fakes::API_KEY),
            "a credential reached a fixture: {body}"
        );
    }
    crate::common::fixture("conversation_message", &published);
    let conversation = get(&crate::common::j(&format!("/conversations/{conv_id}"))).await;
    // shape() pins arrays by their first element (a customer message, which has no case_id), so pin the
    // published agent reply's exact keys here.
    let reply = conversation["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "agent" && m["case_id"] == conv_case.as_str())
        .expect("the agent reply carrying the published case");
    assert!(reply["case_id"].is_string());
    let mut keys: Vec<&String> = reply.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["at_ms", "case_id", "conversation_id", "role", "text"]
    );
    crate::common::fixture("conversation", &conversation);

    // SSE frame payloads: the raw Pub/Sub messages (what /api/events sends as `data:`).
    let mut bus_frame = None;
    let mut chat_frame = None;
    while let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(300), frames.next()).await {
        let v: Value = serde_json::from_str(&m.get_payload::<String>().unwrap()).unwrap();
        if m.get_channel_name() == chat_channel {
            if v["role"] == "agent" && v["case_id"].is_string() {
                chat_frame.get_or_insert(v);
            }
        } else if v["envelope"]["correlation_id"] == CASE
            && v["envelope"]["topic"]
                .as_str()
                .is_some_and(|t| t.ends_with("request.qualified"))
        {
            bus_frame.get_or_insert(v);
        }
    }
    crate::common::fixture(
        "event_bus",
        &bus_frame.expect("console frame for CASE-0001's request.qualified"),
    );
    crate::common::fixture(
        "event_chat",
        &chat_frame.expect("chat frame for the published reply"),
    );

    // the error envelope every route uses
    let (s, err) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j("/cases/CASE-NOPE"), Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    crate::common::fixture("error", &err);
}
