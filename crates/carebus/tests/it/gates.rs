//! Laya gates on the bus side (spec 2026-10-18 §3): the worker asks carebus-api before a side-effect tool call, the
//! interaction daemon before sending a reply. carebus-api is a stub here; Redis is real.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::post;
use carebus::bus::Envelope;
use carebus::domain::{
    Outcome, REQUEST_RESOLVED, RequestType, Resolution, WORK_FAILED, WORK_REQUESTED, WorkRequest,
};
use carebus::gates::{DEFAULT_HANDOFF, GateClient};
use carebus::interaction::{Interaction, RuleQualifier};
use carebus::spaces::{ContextSpace, SpaceCatalog};
use carebus::worker::Worker;
use serde_json::{Value, json};

use crate::common::{bus, entries, wait_entries};

fn space(rt: &str) -> ContextSpace {
    SpaceCatalog::bootstrap()
        .by_request_type(rt)
        .expect("a bootstrap request type")
        .clone()
}

/// carebus-api's internal gate route answering `verdict`; records (Authorization, body) of every call.
async fn api_stub(verdict: Value) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().route(
        "/api/internal/gates/evaluate",
        post(move |h: HeaderMap, axum::Json(body): axum::Json<Value>| {
            let (s, verdict) = (s.clone(), verdict.clone());
            async move {
                let auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                s.lock().unwrap().push((auth, body));
                axum::Json(verdict)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

fn work(case: &str, action: &str, args: Value) -> WorkRequest {
    WorkRequest {
        work_id: format!("{case}:1:{action}:1"),
        idempotency_key: format!("{case}:1:{action}"),
        case_id: case.into(),
        phase: 1,
        as_name: action.into(),
        worker: "order".into(),
        action: action.into(),
        args,
        attempt: 1,
        request_type: RequestType::new("payment_error").unwrap(),
        side_effect: None,
    }
}

#[tokio::test]
async fn a_fired_tool_gate_fails_the_step_non_retryable_and_calls_no_tool() {
    let bus = bus("gate-tool").await;
    let (url, seen) = api_stub(json!({"outcome": "fired", "action": "block", "message": null,
                                      "fired_keys": ["customer_consent"], "answers": {}, "latency_ms": 7,
                                      "has_closed": true}))
    .await;
    let mut w = Worker::new(bus.clone(), "order");
    w.gates = Some(GateClient::new(url, Some("svc-token".into())));
    let sp = space("payment_error");
    // the orchestrator's case hash: the worker reads the customer's request text from it
    let _: () = redis::cmd("HSET")
        .arg(bus.streams.key("case:CASE-g1"))
        .arg("request")
        .arg(json!({"case_id": "CASE-g1", "customer_id": "C-1", "request_type": "payment_error",
                    "area": sp.area, "context_space": sp.key, "slots": {}, "utterance": "I was charged twice"})
            .to_string())
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let refund = work(
        "CASE-g1",
        "issue_refund",
        json!({"payment_id": "PAY-1", "amount": 49.0}),
    );
    w.on_work(
        Envelope::new(
            &sp.topic(WORK_REQUESTED),
            "orchestrator",
            "CASE-g1",
            &refund,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let failed = wait_entries(&bus, &bus.streams.worker_results(&sp.area), 1, |e| {
        e.event() == Some(WORK_FAILED)
    })
    .await;
    assert_eq!(failed[0].payload["retryable"], false);
    assert!(
        failed[0].payload["error"]
            .as_str()
            .unwrap()
            .starts_with("gate_blocked: customer_consent"),
        "{}",
        failed[0].payload
    );
    assert!(
        entries(&bus, &bus.streams.tool("order")).await.is_empty(),
        "no tool.call"
    );
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "Bearer svc-token");
    assert_eq!(calls[0].1["point"], "tool");
    assert_eq!(calls[0].1["tool"], "order_issue_refund");
    assert_eq!(calls[0].1["case_id"], "CASE-g1");
    assert_eq!(calls[0].1["job"], "customer_care_agent");
    assert_eq!(calls[0].1["tenant_id"], crate::common::BOOT_TENANT);
    // the customer's own words only: the tool is named in `tool`, never in the state (its name and arguments
    // biased laya, e.g. "refund" read as the customer asking for one)
    assert_eq!(calls[0].1["state"], "I was charged twice");

    // a read-only tool is never gated: no call, the tool.call goes out
    let lookup = work("CASE-g2", "get_order", json!({"order_id": "ORD-1"}));
    w.on_work(
        Envelope::new(
            &sp.topic(WORK_REQUESTED),
            "orchestrator",
            "CASE-g2",
            &lookup,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(entries(&bus, &bus.streams.tool("order")).await.len(), 1);
}

#[tokio::test]
async fn an_unreachable_api_blocks_a_side_effect_tool() {
    let bus = bus("gate-tool-down").await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    };
    let mut w = Worker::new(bus.clone(), "order");
    w.gates = Some(GateClient::new(dead, None));
    let sp = space("payment_error");
    let refund = work(
        "CASE-g3",
        "issue_refund",
        json!({"payment_id": "PAY-1", "amount": 49.0}),
    );
    w.on_work(
        Envelope::new(
            &sp.topic(WORK_REQUESTED),
            "orchestrator",
            "CASE-g3",
            &refund,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let failed = wait_entries(&bus, &bus.streams.worker_results(&sp.area), 1, |e| {
        e.event() == Some(WORK_FAILED)
    })
    .await;
    assert_eq!(
        failed[0].payload["error"],
        "gate_blocked: gates unavailable"
    );
    assert!(entries(&bus, &bus.streams.tool("order")).await.is_empty());
}

/// Runs the reply consumer with `gates` and returns the first reply to a resolved delivery case.
async fn reply_with(test: &str, gates: GateClient, case: &str) -> (carebus::Bus, String) {
    let bus = bus(test).await;
    let agent = Arc::new(Interaction::new(bus.clone(), RuleQualifier).with_gates(Some(gates)));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let task = tokio::spawn(async move {
        agent
            .run_replies("i-1", move |case, text| {
                let _ = tx.send((case, text));
            })
            .await
            .unwrap()
    });
    let sp = space("delivery_status");
    let mut facts = serde_json::Map::new();
    facts.insert(
        "shipment".into(),
        json!({"status": "in_transit", "carrier": "PostNL", "last_event": "sorted", "eta": "tomorrow"}),
    );
    facts.insert("order".into(), json!({"item": "airfryer"}));
    let res = Resolution {
        case_id: case.into(),
        request_type: RequestType::new("delivery_status").unwrap(),
        outcome: Outcome::Completed,
        reason: None,
        facts,
    };
    bus.publish(
        &bus.streams.customer(&sp.area),
        &Envelope::new(&sp.topic(REQUEST_RESOLVED), "orchestrator", case, &res).unwrap(),
    )
    .await
    .unwrap();
    let (got, text) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("reply within 10 s")
        .expect("channel open");
    task.abort();
    assert_eq!(got, case);
    (bus, text)
}

#[tokio::test]
async fn a_fired_response_gate_replaces_the_reply_and_its_verdict_is_kept() {
    let (url, seen) = api_stub(json!({"outcome": "fired", "action": "handoff",
                                      "message": "A colleague will check this and email you today.",
                                      "fired_keys": ["unbacked_claim"], "answers": {}, "latency_ms": 9,
                                      "has_closed": true}))
    .await;
    let (bus, text) = reply_with("gate-reply", GateClient::new(url, None), "CASE-r1").await;
    assert_eq!(text, "A colleague will check this and email you today.");
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["point"], "response");
    let state = calls[0].1["state"].as_str().unwrap();
    assert!(
        state.contains("PostNL") && state.contains("in_transit"),
        "{state}"
    );
    // stored under the case: a redelivery reuses it instead of asking again
    let stored: Option<String> = redis::cmd("GET")
        .arg(bus.streams.key("gate:CASE-r1"))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert!(stored.unwrap().contains("unbacked_claim"));
}

#[tokio::test]
async fn a_passing_response_gate_keeps_the_reply_and_an_unreachable_api_hands_off() {
    let (url, _) = api_stub(
        json!({"outcome": "pass", "action": null, "message": null, "fired_keys": [],
                                   "answers": {}, "latency_ms": 9, "has_closed": true}),
    )
    .await;
    let (_, text) = reply_with("gate-reply-pass", GateClient::new(url, None), "CASE-r2").await;
    assert!(text.contains("PostNL"), "{text}");

    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    };
    let (bus, text) = reply_with("gate-reply-down", GateClient::new(dead, None), "CASE-r3").await;
    assert_eq!(text, DEFAULT_HANDOFF);
    // a transport error is not carebus-api's verdict: nothing is stored, a redelivery asks again
    let stored: Option<String> = redis::cmd("GET")
        .arg(bus.streams.key("gate:CASE-r3"))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(stored, None);
}
