//! End-to-end over a real Redis (`REDIS_URL`, default the compose one on
//! :6391). Each test uses its own key prefix, so tests run in parallel.
//! No Redis = test failure, not a skip.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use carebus::bus::{Bus, Envelope};
use carebus::domain::{
    AREAS, Outcome, REQUEST_QUALIFIED, REQUEST_RESOLVED, RequestType, Resolution, StaticProcedures,
    TOOL_CALL, TOOL_RESPONSE, ToolResponse, WORK_FAILED, WORK_SUCCEEDED,
};
use carebus::executor::ToolExecutor;
use carebus::interaction::{Interaction, RuleQualifier, Turn};
use carebus::mcp::FakeMcp;
use carebus::orchestrator::Orchestrator;
use carebus::spaces::{ContextSpace, SpaceCatalog};
use carebus::tools::FakeTools;
use carebus::worker::Worker;
use tokio::task::JoinHandle;

use crate::common::{bus, entries, executor_fast, fast};

const WORKERS: [&str; 6] = [
    "crm",
    "order",
    "delivery",
    "faq",
    "business-rules",
    "procedures",
];

struct Harness {
    bus: Bus,
    tools: FakeTools,
    mcp: FakeMcp,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.tasks.iter().for_each(JoinHandle::abort);
    }
}

async fn harness(test: &str) -> Harness {
    harness_with(test, Duration::from_secs(3)).await
}

async fn harness_with(test: &str, executor_timeout: Duration) -> Harness {
    harness_in(test, executor_timeout, SpaceCatalog::bootstrap()).await
}

/// The agents of one scope serving `spaces` as its context spaces.
async fn harness_in(test: &str, executor_timeout: Duration, spaces: SpaceCatalog) -> Harness {
    let bus = bus(test).await;
    let tools = FakeTools::default();
    let mcp = FakeMcp::new(tools.clone());
    let mut tasks = Vec::new();
    let mut orch = Orchestrator::new(bus.clone(), StaticProcedures).with_spaces(spaces.clone());
    orch.consume = fast();
    let orch = Arc::new(orch);
    tasks.push(tokio::spawn(
        async move { orch.run("orch-1").await.unwrap() },
    ));
    for tool in WORKERS {
        let mut w = Worker::new(bus.clone(), tool).with_spaces(spaces.clone());
        w.consume = fast();
        let w = Arc::new(w);
        tasks.push(tokio::spawn(async move { w.run("w-1").await.unwrap() }));
    }
    let mut ex = ToolExecutor::new(bus.clone(), mcp.clone());
    ex.timeout = executor_timeout;
    ex.consume = executor_fast(&ex);
    let ex = Arc::new(ex);
    tasks.push(tokio::spawn(async move { ex.run("x-1").await.unwrap() }));
    Harness {
        bus,
        tools,
        mcp,
        tasks,
    }
}

/// Every `request.resolved` for `case_id`, from all departments' customer streams.
async fn resolutions(bus: &Bus, case_id: &str) -> Vec<Envelope> {
    let mut out = Vec::new();
    for area in AREAS {
        out.extend(
            entries(bus, &bus.streams.customer(area))
                .await
                .into_iter()
                .filter(|e| e.correlation_id == case_id && e.event() == Some(REQUEST_RESOLVED)),
        );
    }
    out
}

async fn resolution(bus: &Bus, case_id: &str) -> Resolution {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(e) = resolutions(bus, case_id).await.into_iter().next() {
            return e.payload_as().unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "no resolution for {case_id} within 10s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The bootstrap job's space of request type `rt`.
fn space(rt: &str) -> ContextSpace {
    SpaceCatalog::bootstrap()
        .by_request_type(rt)
        .expect("a bootstrap request type")
        .clone()
}

async fn say(
    agent: &Interaction<RuleQualifier>,
    draft: &mut Option<carebus::interaction::Draft>,
    text: &str,
) -> Turn {
    agent.handle(draft, text).await.unwrap()
}

#[tokio::test]
async fn delivery_status_is_qualified_from_start_data_and_completes() {
    let h = harness("delivery").await;
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    let Turn::Published(case) = say(&agent, &mut draft, "Where is my airfryer?").await else {
        panic!("should qualify in one turn")
    };
    let r = resolution(&h.bus, &case).await;
    assert_eq!(r.outcome, Outcome::Completed);
    assert_eq!(r.facts["shipment"]["status"], "in_transit");
    assert_eq!(r.facts["order"]["order_id"], "ORD-58790");
}

#[tokio::test]
async fn resolution_comes_back_on_the_departments_customer_bus_and_reaches_the_customer() {
    let h = harness("resolved-path").await;
    let agent = Arc::new(Interaction::new(h.bus.clone(), RuleQualifier));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let replies = agent.clone();
    let task = tokio::spawn(async move {
        replies
            .run_replies("i-1", move |case, text| {
                let _ = tx.send((case, text));
            })
            .await
            .unwrap()
    });
    let mut draft = None;
    let Turn::Published(case) = agent
        .handle(&mut draft, "Where is my airfryer?")
        .await
        .unwrap()
    else {
        panic!("should qualify")
    };
    let (got_case, text) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("reply within 10s")
        .expect("channel open");
    task.abort();
    assert_eq!(got_case, case);
    assert!(text.contains("PostNL"), "{text}");
    let resolved = resolutions(&h.bus, &case).await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].topic,
        "logistics.delivery-status.request.resolved"
    );
    let replies_stream_exists: bool = redis::cmd("EXISTS")
        .arg(h.bus.streams.key("bus:customer:replies"))
        .query_async(&mut h.bus.conn())
        .await
        .unwrap();
    assert!(!replies_stream_exists, "bus:customer:replies is gone");
}

/// CASE-42b46a3b: faq_ask answered with a citation, the case resolved `completed`,
/// and the customer was still told "I couldn't find an answer".
#[tokio::test]
async fn faq_answer_reaches_the_customer() {
    let h = harness("faq-reply").await;
    let agent = Arc::new(Interaction::new(h.bus.clone(), RuleQualifier));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let replies = agent.clone();
    let task = tokio::spawn(async move {
        replies
            .run_replies("i-1", move |case, text| {
                let _ = tx.send((case, text));
            })
            .await
            .unwrap()
    });
    let mut draft = None;
    let Turn::Published(case) = agent
        .handle(&mut draft, "What is your warranty policy?")
        .await
        .unwrap()
    else {
        panic!("should qualify as faq")
    };
    let (got_case, text) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("reply within 10s")
        .expect("channel open");
    task.abort();
    assert_eq!(got_case, case);
    assert_eq!(
        text,
        "Every product has a 2-year warranty from delivery. [1]"
    );
    let resolved = resolutions(&h.bus, &case).await;
    assert_eq!(resolved[0].topic, "faq.general.request.resolved");
}

#[tokio::test]
async fn return_is_held_until_reason_then_creates_one_rma() {
    let h = harness("return").await;
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    assert!(matches!(
        say(&agent, &mut draft, "I want to return my headphones").await,
        Turn::Ask(_)
    ));
    assert!(
        entries(&h.bus, &h.bus.streams.customer("order-support"))
            .await
            .is_empty(),
        "nothing on customer-bus while unqualified"
    );
    let Turn::Published(case) = say(&agent, &mut draft, "they are too uncomfortable").await else {
        panic!("should qualify")
    };
    let r = resolution(&h.bus, &case).await;
    assert_eq!(r.outcome, Outcome::Completed, "{r:?}");
    assert!(
        r.facts["rma"]["rma_id"]
            .as_str()
            .unwrap()
            .starts_with("RMA-")
    );
    assert_eq!(r.facts["rules"]["eligible"], true);
}

#[tokio::test]
async fn retryable_tool_failure_is_retried_and_case_completes() {
    let h = harness("retry").await;
    h.tools.fail_next("order.get_order");
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    let Turn::Published(case) = say(&agent, &mut draft, "Where is my airfryer?").await else {
        panic!()
    };
    assert_eq!(resolution(&h.bus, &case).await.outcome, Outcome::Completed);
    let results = entries(&h.bus, &h.bus.streams.worker_results("logistics")).await;
    let order_attempts: Vec<(String, u64)> = results
        .iter()
        .filter(|e| e.payload["action"] == "get_order")
        .map(|e| {
            (
                e.event().unwrap().to_string(),
                e.payload["attempt"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order_attempts,
        vec![(WORK_FAILED.into(), 1), (WORK_SUCCEEDED.into(), 2)]
    );
}

#[tokio::test]
async fn old_order_needs_order_number_and_return_window_gate_stops_it() {
    let h = harness("gate").await;
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    assert!(matches!(
        say(&agent, &mut draft, "I want to return my drill because it is too heavy").await,
        Turn::Ask(q) if q.contains("order")
    ));
    let Turn::Published(case) = say(&agent, &mut draft, "ORD-57102").await else {
        panic!()
    };
    let r = resolution(&h.bus, &case).await;
    assert_eq!(r.outcome, Outcome::NotEligible);
    assert!(r.facts.get("rma").is_none(), "no RMA after a failed gate");
    assert!(
        !h.tools
            .calls
            .lock()
            .unwrap()
            .contains(&"order.create_rma".to_string())
    );
}

#[tokio::test]
async fn duplicate_qualified_message_resolves_once_and_refunds_once() {
    let h = harness("dup").await;
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    let Turn::Published(case) = say(&agent, &mut draft, "I think I was charged twice").await else {
        panic!()
    };
    let original = entries(&h.bus, &h.bus.streams.customer("payments"))
        .await
        .into_iter()
        .find(|e| e.event() == Some(REQUEST_QUALIFIED))
        .unwrap();
    assert_eq!(original.topic, "payments.payment-errors.request.qualified");
    h.bus
        .publish(&h.bus.streams.customer("payments"), &original)
        .await
        .unwrap();

    let r = resolution(&h.bus, &case).await;
    assert_eq!(r.outcome, Outcome::Completed, "{r:?}");
    assert_eq!(r.facts["refund"]["payment_id"], "PAY-3399");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        resolutions(&h.bus, &case).await.len(),
        1,
        "one resolution per case"
    );
    let refunds = h
        .tools
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| *c == "order.issue_refund")
        .count();
    assert_eq!(refunds, 1, "refund tool called once");
}

#[tokio::test]
async fn failing_handler_is_redelivered_then_dead_lettered() {
    let bus = bus("dlq").await;
    let stream = bus.streams.key("bus:probe");
    for i in 0..2 {
        bus.publish(
            &stream,
            &Envelope::new(
                "probe",
                "test",
                &format!("C-{i}"),
                serde_json::json!({"i": i}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    }
    let seen = Arc::new(AtomicU32::new(0));
    let (b, s, st) = (bus.clone(), seen.clone(), stream.clone());
    let task = tokio::spawn(async move {
        b.consume(&st, "g", "c1", fast(), move |env| {
            let s = s.clone();
            async move {
                s.fetch_add(1, Ordering::SeqCst);
                let n = s.load(Ordering::SeqCst);
                if env.correlation_id == "C-1" || n <= 2 {
                    anyhow::bail!("boom")
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let dead = loop {
        let dead = entries(&bus, &bus.streams.dead()).await;
        if !dead.is_empty() {
            break dead;
        }
        assert!(Instant::now() < deadline, "nothing dead-lettered");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].correlation_id, "C-1");
    // The dead-letter entry is published before the source entry is acknowledged.
    // Wait for both effects before aborting the consumer, or the test can interrupt its XACK.
    let pending = loop {
        let pending: (u64,) = redis::cmd("XPENDING")
            .arg(&stream)
            .arg("g")
            .query_async::<redis::Value>(&mut bus.conn())
            .await
            .map(|v| match v {
                redis::Value::Array(a) => (match &a[0] {
                    redis::Value::Int(n) => *n as u64,
                    _ => 99,
                },),
                _ => (99,),
            })
            .unwrap();
        if pending.0 == 0 || Instant::now() >= deadline {
            break pending;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    task.abort();
    assert_eq!(
        pending.0, 0,
        "C-0 acked after redelivery, C-1 acked into the dead-letter stream"
    );
}

#[tokio::test]
async fn duplicate_result_does_not_advance_phase_while_a_sibling_is_open() {
    use carebus::domain::{QualifiedRequest, RequestType, Slots, WorkOutcome};
    let bus = bus("dupresult").await;
    let orch = Orchestrator::new(bus.clone(), StaticProcedures);
    let rt = RequestType::r#return();
    let sp = space("return");
    let req = QualifiedRequest {
        case_id: "CASE-T1".into(),
        customer_id: "C-10482".into(),
        request_type: rt.clone(),
        area: sp.area.clone(),
        context_space: sp.key.clone(),
        slots: Slots::of(&[("order_id", "ORD-58213"), ("reason", "too big")]),
        utterance: "return".into(),
        gates: None,
    };
    orch.on_qualified(
        Envelope::new(&sp.topic(REQUEST_QUALIFIED), "test", "CASE-T1", &req).unwrap(),
    )
    .await
    .unwrap();
    let outcome = |as_name: &str, worker: &str, action: &str, result: serde_json::Value| {
        let o = WorkOutcome {
            work_id: format!("CASE-T1:0:{as_name}:1"),
            case_id: "CASE-T1".into(),
            phase: 0,
            as_name: as_name.into(),
            worker: worker.into(),
            action: action.into(),
            attempt: 1,
            ok: true,
            result: Some(result),
            error: None,
            retryable: false,
            request_type: rt.clone(),
        };
        Envelope::new(&sp.topic(WORK_SUCCEEDED), "test", "CASE-T1", &o).unwrap()
    };
    let crm = outcome(
        "customer",
        "crm",
        "verify_customer",
        serde_json::json!({"verified": true}),
    );
    orch.on_result(crm.clone()).await.unwrap();
    orch.on_result(crm).await.unwrap(); // redelivered duplicate
    assert_eq!(
        orch.cases().phase("CASE-T1").await.unwrap(),
        0,
        "order result still open"
    );
    let rules_stream = bus.streams.worker("order-support", "business-rules");
    assert!(
        entries(&bus, &rules_stream).await.is_empty(),
        "phase 1 must not start"
    );

    let order = carebus::tools::orders()
        .into_iter()
        .find(|o| o["order_id"] == "ORD-58213")
        .unwrap();
    orch.on_result(outcome("order", "order", "get_order", order))
        .await
        .unwrap();
    assert_eq!(orch.cases().phase("CASE-T1").await.unwrap(), 1);
    let rules = entries(&bus, &rules_stream).await;
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].topic, "order-support.returns.work.requested");
    assert_eq!(
        rules[0].payload["args"]["category"], "audio",
        "phase 1 args resolved from the order result"
    );
}

#[tokio::test]
async fn tool_calls_round_trip_over_the_tool_bus() {
    let h = harness("toolbus").await;
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    let Turn::Published(case) = say(&agent, &mut draft, "Where is my airfryer?").await else {
        panic!()
    };
    assert_eq!(resolution(&h.bus, &case).await.outcome, Outcome::Completed);
    let on_delivery = entries(&h.bus, &h.bus.streams.tool("delivery")).await;
    let topics: Vec<&str> = on_delivery.iter().map(|e| e.topic.as_str()).collect();
    assert_eq!(
        topics,
        vec![
            "logistics.delivery-status.tool.call",
            "logistics.delivery-status.tool.response"
        ]
    );
    assert_eq!(
        on_delivery[0].payload["call_id"],
        on_delivery[1].payload["call_id"]
    );
    assert_eq!(
        on_delivery[0].payload["mcp_tool"],
        "delivery_track_shipment"
    );
    assert!(on_delivery.iter().all(|e| e.correlation_id == case));
}

#[tokio::test]
async fn executor_timeout_is_retried_and_the_case_completes() {
    let h = harness_with("ex-retry", Duration::from_millis(300)).await;
    h.mcp.delay_next("order_get_order", Duration::from_secs(2));
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    let Turn::Published(case) = say(&agent, &mut draft, "Where is my airfryer?").await else {
        panic!()
    };
    assert_eq!(resolution(&h.bus, &case).await.outcome, Outcome::Completed);
    let results = entries(&h.bus, &h.bus.streams.worker_results("logistics")).await;
    let attempts: Vec<(String, u64)> = results
        .iter()
        .filter(|e| e.payload["action"] == "get_order")
        .map(|e| {
            (
                e.event().unwrap().to_string(),
                e.payload["attempt"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        attempts,
        vec![(WORK_FAILED.into(), 1), (WORK_SUCCEEDED.into(), 2)]
    );
    let first_failure = results
        .into_iter()
        .find(|e| e.event() == Some(WORK_FAILED))
        .unwrap();
    assert_eq!(first_failure.payload["error"], "timeout after 300 ms");
}

#[tokio::test]
async fn response_for_unknown_call_is_dropped() {
    let bus = bus("unknown-call").await;
    let w = Worker::new(bus.clone(), "order");
    let sp = space("delivery_status");
    let resp = ToolResponse {
        call_id: "never-dispatched".into(),
        mcp_tool: "order_get_order".into(),
        ok: true,
        result: Some(serde_json::json!({})),
        error: None,
        retryable: false,
        latency_ms: 1,
    };
    w.on_response(Envelope::new(&sp.topic(TOOL_RESPONSE), "mcp:order", "CASE-Z", &resp).unwrap())
        .await
        .expect("dropped, not an error");
    assert!(
        entries(&bus, &bus.streams.worker_results("logistics"))
            .await
            .is_empty()
    );
    // a tool.call on the same stream is the executor's business, not the worker's
    w.on_response(
        Envelope::new(
            &sp.topic(TOOL_CALL),
            "worker:order",
            "CASE-Z",
            serde_json::json!({}),
        )
        .unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn duplicate_tool_response_republishes_the_stored_outcome() {
    use carebus::domain::{ToolCall, WORK_REQUESTED, WorkRequest};
    let bus = bus("dup-response").await;
    let w = Worker::new(bus.clone(), "order");
    let sp = space("delivery_status");
    let req = WorkRequest {
        work_id: "CASE-D:0:order:1".into(),
        case_id: "CASE-D".into(),
        phase: 0,
        as_name: "order".into(),
        worker: "order".into(),
        action: "get_order".into(),
        args: serde_json::json!({"order_id": "ORD-58790"}),
        idempotency_key: "k".into(),
        attempt: 1,
        request_type: RequestType::delivery_status(),
        side_effect: None,
    };
    w.on_work(Envelope::new(&sp.topic(WORK_REQUESTED), "orch", "CASE-D", &req).unwrap())
        .await
        .unwrap();
    let call: ToolCall = entries(&bus, &bus.streams.tool("order")).await[0]
        .payload_as()
        .unwrap();
    let mk = |ok: bool| {
        let r = ToolResponse {
            call_id: call.call_id.clone(),
            mcp_tool: call.mcp_tool.clone(),
            ok,
            result: ok.then(|| serde_json::json!({"order_id": "ORD-1"})),
            error: (!ok).then(|| "late conflicting".to_string()),
            retryable: false,
            latency_ms: 1,
        };
        Envelope::new(&sp.topic(TOOL_RESPONSE), "mcp:order", "CASE-D", &r).unwrap()
    };
    w.on_response(mk(true)).await.unwrap();
    w.on_response(mk(false)).await.unwrap(); // duplicate id, different content
    let results = entries(&bus, &bus.streams.worker_results("logistics")).await;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|e| e.event() == Some(WORK_SUCCEEDED)));
    assert_eq!(results[0].payload, results[1].payload);
}

/// What a customer may read: no URLs, no host:port, no raw error chain.
fn assert_customer_safe(text: &str) {
    assert!(!text.contains("http"), "URL leaked to the customer: {text}");
    let b = text.as_bytes();
    assert!(
        !b.windows(2).any(|w| w[0] == b':' && w[1].is_ascii_digit()),
        "host:port leaked to the customer: {text}"
    );
}

#[tokio::test]
async fn procedure_lookup_failure_fails_the_case_with_a_reason() {
    use carebus::domain::{Plan, ProcedureSource, QualifiedRequest, Slots};
    struct Down;
    impl ProcedureSource for Down {
        async fn plan(&self, _space: &ContextSpace) -> anyhow::Result<Plan> {
            Err(anyhow::anyhow!(
                "error sending request for url (http://carebus-api.test:9917/api/jobs/customer_care_agent/procedures)"
            )
            .context("procedure registry unreachable"))
        }
    }
    let bus = bus("no-procedure").await;
    let orch = Orchestrator::new(bus.clone(), Down);
    let sp = space("delivery_status");
    let req = QualifiedRequest {
        case_id: "CASE-NP".into(),
        customer_id: "C-10482".into(),
        request_type: RequestType::delivery_status(),
        area: sp.area.clone(),
        context_space: sp.key.clone(),
        slots: Slots::of(&[("order_id", "ORD-58790")]),
        utterance: "where is it".into(),
        gates: None,
    };
    orch.on_qualified(
        Envelope::new(&sp.topic(REQUEST_QUALIFIED), "test", "CASE-NP", &req).unwrap(),
    )
    .await
    .expect("a missing procedure fails the case; it is not a handler error");
    let r = resolution(&bus, "CASE-NP").await;
    assert_eq!(r.outcome, Outcome::Failed);
    let reason = r.reason.clone().unwrap();
    assert_eq!(
        reason,
        format!("the procedure registry is unavailable for {}", sp.key)
    );
    assert_customer_safe(&reason);
    assert_customer_safe(&carebus::interaction::compose_reply(&r));
    // Operators still get the raw detail, on the case.
    let detail = orch.cases().error("CASE-NP").await.unwrap().unwrap();
    assert!(
        detail.contains("procedure registry unreachable")
            && detail.contains("http://carebus-api.test:9917"),
        "{detail}"
    );
}

#[tokio::test]
async fn mcp_down_times_out_twice_and_fails_the_case_with_a_safe_reason() {
    let h = harness_with("mcp-down", Duration::from_millis(300)).await;
    h.mcp.delay_all("order_get_order", Duration::from_secs(2));
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    let Turn::Published(case) = say(&agent, &mut draft, "Where is my airfryer?").await else {
        panic!("should qualify")
    };
    let r = resolution(&h.bus, &case).await;
    assert_eq!(r.outcome, Outcome::Failed, "{r:?}");
    let results = entries(&h.bus, &h.bus.streams.worker_results("logistics")).await;
    let attempts: Vec<(String, u64, String)> = results
        .iter()
        .filter(|e| e.payload["action"] == "get_order")
        .map(|e| {
            (
                e.event().unwrap().to_string(),
                e.payload["attempt"].as_u64().unwrap(),
                e.payload["error"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        attempts,
        vec![
            (WORK_FAILED.into(), 1, "timeout after 300 ms".into()),
            (WORK_FAILED.into(), 2, "timeout after 300 ms".into()),
        ],
        "retried once, then failed"
    );
    let reason = r.reason.clone().unwrap();
    assert_eq!(reason, "order did not answer in time");
    assert_customer_safe(&reason);
    let reply = carebus::interaction::compose_reply(&r);
    assert!(reply.contains("order did not answer in time"), "{reply}");
    assert!(!reply.contains("timeout after"), "{reply}");
    // The raw error stays on the case for operators.
    let work = carebus::case::CaseStore::new(h.bus.clone())
        .work(&case)
        .await
        .unwrap();
    assert!(
        work.iter()
            .any(|w| w["action"] == "get_order" && w["error"] == "timeout after 300 ms"),
        "{work:?}"
    );
}

#[tokio::test]
async fn permanent_tool_failure_is_reported_as_unavailable_without_internals() {
    let h = harness("tool-down").await;
    h.mcp.fail_all(
        "order_get_order",
        "upstream http://demo-backends.test:9931/orders refused",
    );
    let agent = Interaction::new(h.bus.clone(), RuleQualifier);
    let mut draft = None;
    let Turn::Published(case) = say(&agent, &mut draft, "Where is my airfryer?").await else {
        panic!("should qualify")
    };
    let r = resolution(&h.bus, &case).await;
    assert_eq!(r.outcome, Outcome::Failed, "{r:?}");
    let reason = r.reason.clone().unwrap();
    assert_eq!(reason, "order is unavailable");
    assert_customer_safe(&carebus::interaction::compose_reply(&r));
}

#[tokio::test]
async fn pending_call_record_is_written_with_its_expiry() {
    use carebus::domain::{WORK_REQUESTED, WorkRequest};
    let bus = bus("call-ttl").await;
    let w = Worker::new(bus.clone(), "order");
    let sp = space("delivery_status");
    let req = WorkRequest {
        work_id: "CASE-T:0:order:1".into(),
        case_id: "CASE-T".into(),
        phase: 0,
        as_name: "order".into(),
        worker: "order".into(),
        action: "get_order".into(),
        args: serde_json::json!({"order_id": "ORD-58790"}),
        idempotency_key: "k".into(),
        attempt: 1,
        request_type: RequestType::delivery_status(),
        side_effect: None,
    };
    w.on_work(Envelope::new(&sp.topic(WORK_REQUESTED), "orch", "CASE-T", &req).unwrap())
        .await
        .unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg(bus.streams.key("call:CASE-T:0:order:1"))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert!(
        (86_000..=86_400).contains(&ttl),
        "call record expires in a day: {ttl}"
    );
}

/// Plan 13 Review Focus 1: with two scopes registered and every agent started through the per-scope supervisor,
/// a return on scope A resolves on A, and scope B's streams and console see nothing of it.
#[tokio::test]
async fn two_scopes_do_not_cross() {
    use carebus::supervisor::run_per_scope;
    use futures_util::StreamExt;

    let root = crate::common::root("two-scopes").await;
    let (a, b) = (crate::common::scope_a(), crate::common::scope_b());
    carebus::scopes::add(&root, &a).await.unwrap();
    carebus::scopes::add(&root, &b).await.unwrap();
    let (bus_a, bus_b) = (root.scoped(&a), root.scoped(&b));
    // Subscribed before any agent runs, so every frame published for B is buffered here.
    let mut ps = root.client().get_async_pubsub().await.unwrap();
    ps.subscribe(bus_b.streams.console()).await.unwrap();

    let tools = FakeTools::default();
    let mcp = FakeMcp::new(tools.clone());
    let mut tasks = Vec::new();
    let r = root.clone();
    tasks.push(tokio::spawn(async move {
        let _ = run_per_scope(r, "orchestrator", |bus, _s| async move {
            let mut o = Orchestrator::new(bus, StaticProcedures);
            o.consume = fast();
            Arc::new(o).run("orch-1").await
        })
        .await;
    }));
    for tool in WORKERS {
        let r = root.clone();
        tasks.push(tokio::spawn(async move {
            let _ = run_per_scope(r, tool, move |bus, _s| async move {
                let mut w = Worker::new(bus, tool);
                w.consume = fast();
                Arc::new(w).run("w-1").await
            })
            .await;
        }));
    }
    let (r, m) = (root.clone(), mcp.clone());
    tasks.push(tokio::spawn(async move {
        let _ = run_per_scope(r, "tool-executor", move |bus, _s| {
            let mut ex = ToolExecutor::new(bus, m.clone());
            ex.timeout = Duration::from_secs(3);
            ex.consume = executor_fast(&ex);
            async move { Arc::new(ex).run("x-1").await }
        })
        .await;
    }));
    // Aborting the supervisors (on drop) aborts their scope tasks.
    let h = Harness {
        bus: bus_a.clone(),
        tools,
        mcp,
        tasks,
    };

    let agent = Interaction::new(bus_a.clone(), RuleQualifier);
    let mut draft = None;
    assert!(matches!(
        say(&agent, &mut draft, "I want to return my headphones").await,
        Turn::Ask(_)
    ));
    let Turn::Published(case) = say(&agent, &mut draft, "they are too uncomfortable").await else {
        panic!("should qualify")
    };
    let r = resolution(&h.bus, &case).await;
    assert_eq!(r.outcome, Outcome::Completed, "{r:?}");
    assert!(
        !entries(&bus_a, &bus_a.streams.tool("order"))
            .await
            .is_empty(),
        "A's tool bus carried the order calls"
    );

    for area in AREAS {
        let mut streams = vec![
            bus_b.streams.customer(area),
            bus_b.streams.worker_results(area),
        ];
        streams.extend(WORKERS.iter().map(|t| bus_b.streams.worker(area, t)));
        for s in streams {
            assert!(entries(&bus_b, &s).await.is_empty(), "{s} has entries");
        }
    }
    for tool in WORKERS {
        let s = bus_b.streams.tool(tool);
        assert!(entries(&bus_b, &s).await.is_empty(), "{s} has entries");
    }
    let mut frames = ps.into_on_message();
    let frame = tokio::time::timeout(Duration::from_millis(500), frames.next()).await;
    assert!(frame.is_err(), "B's console received a frame");
}

/// The bootstrap catalog plus a space in a new area with a request type that is not one of the built-in five,
/// running the `return` procedure: what a release can add without a redeploy.
fn with_returns_desk() -> SpaceCatalog {
    let mut all = SpaceCatalog::bootstrap().spaces().to_vec();
    all.push(ContextSpace {
        key: "retail:returns-desk:returns-b".into(),
        area: "returns-desk".into(),
        request_type: "returns_b".into(),
        required: vec!["order_id".into(), "reason".into()],
        procedure: "return".into(),
    });
    SpaceCatalog::new(all)
}

/// Plan 12 live check: a context space with a NEW request type is qualified by the interaction agent, gets its own
/// area streams, and is routed by the orchestrator and the workers to a resolution, all from the catalog.
#[tokio::test]
async fn new_space_is_routed() {
    let spaces = with_returns_desk();
    let h = harness_in("new-space", Duration::from_secs(3), spaces.clone()).await;
    let agent = Interaction::new(h.bus.clone(), RuleQualifier).with_spaces(spaces);
    let mut draft = None;
    assert_eq!(
        say(&agent, &mut draft, "I need returns b for my headphones").await,
        Turn::Ask("What's the reason you'd like to return it?".into())
    );
    assert_eq!(
        draft.as_ref().map(|d| d.request_type.as_str()),
        Some("returns_b")
    );
    let Turn::Published(case) = say(&agent, &mut draft, "they are too uncomfortable").await else {
        panic!("should qualify")
    };
    let customer = h.bus.streams.customer("returns-desk");
    let deadline = Instant::now() + Duration::from_secs(10);
    let resolved = loop {
        let found = entries(&h.bus, &customer)
            .await
            .into_iter()
            .find(|e| e.correlation_id == case && e.event() == Some(REQUEST_RESOLVED));
        if let Some(e) = found {
            break e;
        }
        assert!(
            Instant::now() < deadline,
            "no resolution for {case} within 10s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(resolved.topic, "returns-desk.returns-b.request.resolved");
    let r: Resolution = resolved.payload_as().unwrap();
    assert_eq!(r.outcome, Outcome::Completed, "{r:?}");
    assert_eq!(r.request_type, "returns_b");
    assert!(r.facts.contains_key("rma"), "{r:?}");
    let on_customer = entries(&h.bus, &customer).await;
    assert_eq!(
        on_customer[0].topic,
        "returns-desk.returns-b.request.qualified"
    );
    let work = entries(&h.bus, &h.bus.streams.worker("returns-desk", "order")).await;
    assert!(
        work.iter()
            .any(|e| e.topic == "returns-desk.returns-b.work.requested"),
        "the work went to the new area's worker stream"
    );
    assert!(
        !entries(&h.bus, &h.bus.streams.worker_results("returns-desk"))
            .await
            .is_empty()
    );
    // nothing of it reached the bootstrap areas
    assert!(resolutions(&h.bus, &case).await.is_empty());
}

/// A work item whose request type the scope does not have is a handler error (redelivered, then dead-lettered),
/// never a panic and never routed somewhere else.
#[tokio::test]
async fn work_for_an_unknown_request_type_is_refused() {
    use carebus::domain::WorkRequest;
    let bus = bus("unknown-rt").await;
    let w = Worker::new(bus.clone(), "order");
    let req = WorkRequest {
        work_id: "CASE-U:0:order:1".into(),
        case_id: "CASE-U".into(),
        phase: 0,
        as_name: "order".into(),
        worker: "order".into(),
        action: "get_order".into(),
        args: serde_json::json!({"order_id": "ORD-58790"}),
        idempotency_key: "k".into(),
        attempt: 1,
        request_type: RequestType::new("gift_card").unwrap(),
        side_effect: None,
    };
    let err = w
        .on_work(
            Envelope::new("vouchers.gift-cards.work.requested", "orch", "CASE-U", &req).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("gift_card"), "{err:#}");
    assert!(entries(&bus, &bus.streams.tool("order")).await.is_empty());
}
