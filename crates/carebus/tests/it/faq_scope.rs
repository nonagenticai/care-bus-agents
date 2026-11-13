//! The customer chat never searches a store walled for a legal matter (carebus-api crate::walls): the `faq` worker
//! asks carebus-api (`POST /api/internal/faq/scope`, a stub here) what chat may search and sends `faq_ask` with
//! exactly those `leaf_ids`; the executor drops an answer whose MCP server did not confirm the filter (an old server
//! that ignores it searched every store). Redis is real.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::post;
use carebus::bus::Envelope;
use carebus::domain::{
    RequestType, TOOL_CALL, TOOL_RESPONSE, ToolCall, ToolResponse, WORK_FAILED, WORK_REQUESTED,
    WorkRequest,
};
use carebus::executor::ToolExecutor;
use carebus::gates::GateClient;
use carebus::mcp::{FAQ_UNSCOPED, FakeMcp};
use carebus::spaces::{ContextSpace, SpaceCatalog};
use carebus::tools::FakeTools;
use carebus::worker::{FAQ_NO_STORES, FAQ_SCOPE_UNAVAILABLE, Worker};
use serde_json::{Value, json};

use crate::common::{bus, entries, executor_fast, wait_entries};

const A: &str = "7f1c9a2e-0000-4000-8000-0000000000a1";
const B: &str = "7f1c9a2e-0000-4000-8000-0000000000a2";

fn faq_space() -> ContextSpace {
    SpaceCatalog::bootstrap()
        .space_of(&RequestType::faq())
        .expect("the bootstrap faq space")
        .clone()
}

/// carebus-api's internal faq scope route answering `scope`; records (Authorization, body) of every call.
async fn scope_stub(scope: Value) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().route(
        "/api/internal/faq/scope",
        post(move |h: HeaderMap, axum::Json(body): axum::Json<Value>| {
            let (s, scope) = (s.clone(), scope.clone());
            async move {
                let auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                s.lock().unwrap().push((auth, body));
                axum::Json(scope)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

fn ask(case: &str, args: Value) -> WorkRequest {
    WorkRequest {
        work_id: format!("{case}:0:faq:1"),
        idempotency_key: format!("{case}:0:faq"),
        case_id: case.into(),
        phase: 0,
        as_name: "faq".into(),
        worker: "faq".into(),
        action: "ask".into(),
        args,
        attempt: 1,
        request_type: RequestType::faq(),
        side_effect: None,
    }
}

async fn work(w: &Worker, sp: &ContextSpace, req: &WorkRequest) {
    w.on_work(Envelope::new(&sp.topic(WORK_REQUESTED), "orchestrator", &req.case_id, req).unwrap())
        .await
        .unwrap();
}

fn calls(es: &[Envelope]) -> Vec<ToolCall> {
    es.iter()
        .filter(|e| e.event() == Some(TOOL_CALL))
        .map(|e| e.payload_as().unwrap())
        .collect()
}

#[tokio::test]
async fn a_tenant_with_walls_asks_only_the_leaves_outside_them() {
    let bus = bus("faq-scope-walled").await;
    let (url, seen) = scope_stub(json!({"restricted": true, "leaf_ids": [A, B]})).await;
    let mut w = Worker::new(bus.clone(), "faq");
    w.gates = Some(GateClient::new(url, Some("svc-token".into())));
    let sp = faq_space();
    // a store a plan step named is dropped: only carebus-api decides what chat searches
    let req = ask(
        "CASE-f1",
        json!({"question": "What is the return period?", "root_id": "x", "leaf_ids": ["y"], "root_ids": ["z"]}),
    );
    work(&w, &sp, &req).await;
    let sent = calls(&entries(&bus, &bus.streams.tool("faq")).await);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].mcp_tool, "faq_ask");
    assert_eq!(
        sent[0].arguments,
        json!({"question": "What is the return period?", "leaf_ids": [A, B]})
    );
    let asked = seen.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, "Bearer svc-token");
    assert_eq!(asked[0].1, json!({"tenant_id": crate::common::BOOT_TENANT}));
}

#[tokio::test]
async fn a_tenant_without_walls_keeps_the_default_routing() {
    let bus = bus("faq-scope-open").await;
    let (url, _) = scope_stub(json!({"restricted": false})).await;
    let mut w = Worker::new(bus.clone(), "faq");
    w.gates = Some(GateClient::new(url, None));
    let req = ask("CASE-f2", json!({"question": "q", "root_id": "x"}));
    work(&w, &faq_space(), &req).await;
    let sent = calls(&entries(&bus, &bus.streams.tool("faq")).await);
    assert_eq!(sent[0].arguments, json!({"question": "q"}));
}

#[tokio::test]
async fn nothing_searchable_or_no_answer_from_the_api_fails_the_step_without_a_call() {
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    };
    let (empty, _) = scope_stub(json!({"restricted": true, "leaf_ids": []})).await;
    for (test, url, want) in [
        ("faq-scope-empty", empty, FAQ_NO_STORES),
        ("faq-scope-down", dead, FAQ_SCOPE_UNAVAILABLE),
    ] {
        let bus = bus(test).await;
        let mut w = Worker::new(bus.clone(), "faq");
        w.gates = Some(GateClient::new(url, None));
        let sp = faq_space();
        work(&w, &sp, &ask("CASE-f3", json!({"question": "q"}))).await;
        let failed = wait_entries(&bus, &bus.streams.worker_results(&sp.area), 1, |e| {
            e.event() == Some(WORK_FAILED)
        })
        .await;
        assert_eq!(failed[0].payload["error"], want, "{test}");
        assert_eq!(failed[0].payload["retryable"], false, "{test}");
        assert!(
            calls(&entries(&bus, &bus.streams.tool("faq")).await).is_empty(),
            "{test}: no faq_ask"
        );
    }
}

/// The executor's answer to one `faq_ask` call with `leaf_ids` through `mcp`.
async fn execute(test: &str, mcp: FakeMcp) -> (ToolResponse, Option<String>) {
    let bus = bus(test).await;
    let mut ex = ToolExecutor::new(bus.clone(), mcp);
    ex.faq_timeout = Duration::from_secs(3);
    ex.consume = executor_fast(&ex);
    let ex = Arc::new(ex);
    let task = {
        let ex = ex.clone();
        tokio::spawn(async move { ex.run("e-1").await.unwrap() })
    };
    let req = ask("CASE-f4", json!({"question": "q", "leaf_ids": [A]}));
    let call = ToolCall {
        call_id: req.work_id.clone(),
        mcp_tool: "faq_ask".into(),
        arguments: json!({"question": "q", "leaf_ids": [A]}),
        timeout_ms: 3000,
        work: req,
    };
    let topic = faq_space().topic(TOOL_CALL);
    bus.publish(
        &bus.streams.tool("faq"),
        &Envelope::new(&topic, "worker:faq", "CASE-f4", &call).unwrap(),
    )
    .await
    .unwrap();
    let got = wait_entries(&bus, &bus.streams.tool("faq"), 1, |e| {
        e.event() == Some(TOOL_RESPONSE)
    })
    .await;
    task.abort();
    let done: Option<String> = redis::cmd("GET")
        .arg(bus.streams.key(&format!("done:call:{}", call.call_id)))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    (got[0].payload_as().unwrap(), done)
}

#[tokio::test]
async fn an_answer_is_kept_only_when_the_mcp_server_confirms_the_filter() {
    // a server that applies the filter echoes it
    let (r, _) = execute("faq-scope-confirmed", FakeMcp::new(FakeTools::default())).await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.result.unwrap()["scope"]["leaf_ids"], json!([A]));

    // an old server drops the unknown argument and searches every store: the answer never enters the bus
    let old = FakeMcp::new(FakeTools::default());
    old.drop_arg("faq_ask", "leaf_ids");
    let (r, done) = execute("faq-scope-unconfirmed", old).await;
    assert!(!r.ok);
    assert!(!r.retryable);
    assert!(r.result.is_none());
    assert_eq!(r.error.as_deref(), Some(FAQ_UNSCOPED));
    let done = done.expect("the response is stored for redelivery");
    assert!(!done.contains("I do not know"), "{done}");
}
