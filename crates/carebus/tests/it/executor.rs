use std::sync::Arc;
use std::time::Duration;

use crate::common::{bus, entries, executor_fast, fast, wait_entries};
use carebus::bus::{Bus, Envelope};
use carebus::domain::{RequestType, TOOL_CALL, TOOL_RESPONSE, ToolCall, ToolResponse, WorkRequest};
use carebus::executor::{ToolExecutor, tool_timeout};
use carebus::mcp::{FakeMcp, mcp_arguments, mcp_tool_name};
use carebus::spaces::SpaceCatalog;
use carebus::tools::FakeTools;
use serde_json::{Value, json};

fn call(id: &str, tool: &str, method: &str, args: Value) -> ToolCall {
    call_within(id, tool, method, args, 3000)
}

/// Calls run in the bootstrap job's delivery-status space.
fn call_topic() -> String {
    SpaceCatalog::bootstrap()
        .topic(&RequestType::delivery_status(), TOOL_CALL)
        .expect("bootstrap space")
}

fn call_within(id: &str, tool: &str, method: &str, args: Value, timeout_ms: u64) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        mcp_tool: mcp_tool_name(tool, method),
        arguments: mcp_arguments(tool, method, &args, id, None),
        timeout_ms,
        work: WorkRequest {
            work_id: id.into(),
            idempotency_key: id.into(),
            case_id: "CASE-X".into(),
            phase: 0,
            as_name: "x".into(),
            worker: tool.into(),
            action: method.into(),
            args,
            attempt: 1,
            request_type: RequestType::delivery_status(),
            side_effect: None,
        },
    }
}

async fn send(bus: &Bus, c: &ToolCall) -> Envelope {
    let env = Envelope::new(&call_topic(), "test", &c.work.case_id, c).unwrap();
    bus.publish(&bus.streams.tool(&c.work.worker), &env)
        .await
        .unwrap();
    env
}

fn spawn(
    bus: &Bus,
    mcp: FakeMcp,
    consumer: &'static str,
    timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    let mut ex = ToolExecutor::new(bus.clone(), mcp);
    ex.timeout = timeout;
    ex.consume = executor_fast(&ex);
    let ex = Arc::new(ex);
    tokio::spawn(async move { ex.run(consumer).await.unwrap() })
}

fn is_response(e: &Envelope) -> bool {
    e.event() == Some(TOOL_RESPONSE)
}

#[tokio::test]
async fn tool_call_gets_one_tool_response_on_the_same_stream() {
    let bus = bus("ex-roundtrip").await;
    let task = spawn(
        &bus,
        FakeMcp::new(FakeTools::default()),
        "e-1",
        Duration::from_secs(3),
    );
    send(
        &bus,
        &call(
            "C-1",
            "order",
            "get_order",
            json!({"order_id": "ORD-58790"}),
        ),
    )
    .await;
    let got = wait_entries(&bus, &bus.streams.tool("order"), 1, is_response).await;
    task.abort();
    assert_eq!(got[0].topic, "logistics.delivery-status.tool.response");
    assert_eq!(got[0].from, "mcp:order");
    let r: ToolResponse = got[0].payload_as().unwrap();
    assert_eq!(r.call_id, "C-1");
    assert_eq!(r.mcp_tool, "order_get_order");
    assert!(r.ok, "{r:?}");
    assert_eq!(r.result.unwrap()["order_id"], "ORD-58790");
}

#[tokio::test]
async fn executor_times_out_a_slow_tool_as_retryable() {
    let bus = bus("ex-timeout").await;
    let mcp = FakeMcp::new(FakeTools::default());
    mcp.delay_next("order_get_order", Duration::from_secs(2));
    let task = spawn(&bus, mcp, "e-1", Duration::from_millis(200));
    send(
        &bus,
        &call(
            "C-2",
            "order",
            "get_order",
            json!({"order_id": "ORD-58790"}),
        ),
    )
    .await;
    let got = wait_entries(&bus, &bus.streams.tool("order"), 1, is_response).await;
    task.abort();
    let r: ToolResponse = got[0].payload_as().unwrap();
    assert!(!r.ok);
    assert!(r.retryable);
    assert_eq!(r.error.as_deref(), Some("timeout after 200 ms"));
}

#[tokio::test]
async fn faq_gets_the_longer_timeout_budget() {
    let bus = bus("ex-faq-budget").await;
    let mcp = FakeMcp::new(FakeTools::default());
    mcp.delay_next("faq_ask", Duration::from_millis(400));
    let mut ex = ToolExecutor::new(bus.clone(), mcp);
    ex.timeout = Duration::from_millis(200); // would fail any other tool
    ex.faq_timeout = Duration::from_secs(2);
    ex.consume = executor_fast(&ex);
    let ex = Arc::new(ex);
    let task = tokio::spawn(async move { ex.run("e-1").await.unwrap() });
    send(
        &bus,
        &call(
            "C-F",
            "faq",
            "ask",
            json!({"question": "How long does a refund take?"}),
        ),
    )
    .await;
    let got = wait_entries(&bus, &bus.streams.tool("faq"), 1, is_response).await;
    task.abort();
    let r: ToolResponse = got[0].payload_as().unwrap();
    assert!(
        r.ok,
        "faq must not be cut off at the 200 ms default budget: {r:?}"
    );
}

#[test]
fn tool_timeout_defaults_and_overrides() {
    let none = |_: &str| None;
    assert_eq!(tool_timeout("order", none), Duration::from_millis(3_000));
    assert_eq!(tool_timeout("faq", none), Duration::from_millis(20_000));
    let set = |k: &str| match k {
        "TOOL_TIMEOUT_MS" => Some("1500".to_string()),
        "TOOL_TIMEOUT_MS_FAQ" => Some("9000".to_string()),
        _ => None,
    };
    assert_eq!(tool_timeout("crm", set), Duration::from_millis(1_500));
    assert_eq!(tool_timeout("faq", set), Duration::from_millis(9_000));
    assert_eq!(
        tool_timeout("faq", |_: &str| Some("not-a-number".to_string())),
        Duration::from_millis(20_000)
    );
}

#[tokio::test]
async fn duplicate_tool_call_calls_mcp_once() {
    let bus = bus("ex-dup").await;
    let tools = FakeTools::default();
    let task = spawn(
        &bus,
        FakeMcp::new(tools.clone()),
        "e-1",
        Duration::from_secs(3),
    );
    let c = call(
        "C-3",
        "order",
        "create_rma",
        json!({"order_id": "ORD-58213", "reason": "x"}),
    );
    let env = send(&bus, &c).await;
    bus.publish(&bus.streams.tool("order"), &env).await.unwrap(); // redelivered duplicate
    let got = wait_entries(&bus, &bus.streams.tool("order"), 2, is_response).await;
    task.abort();
    assert_eq!(tools.calls.lock().unwrap().len(), 1, "MCP called once");
    assert_eq!(got[0].payload, got[1].payload, "same response both times");
}

#[tokio::test]
async fn two_executor_replicas_call_each_entry_once() {
    let bus = bus("ex-replicas").await;
    let tools = FakeTools::default();
    let a = spawn(
        &bus,
        FakeMcp::new(tools.clone()),
        "e-1",
        Duration::from_secs(3),
    );
    let b = spawn(
        &bus,
        FakeMcp::new(tools.clone()),
        "e-2",
        Duration::from_secs(3),
    );
    for i in 0..20 {
        send(
            &bus,
            &call(
                &format!("C-{i}"),
                "order",
                "get_order",
                json!({"order_id": "ORD-58790"}),
            ),
        )
        .await;
    }
    let got = wait_entries(&bus, &bus.streams.tool("order"), 20, is_response).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.abort();
    b.abort();
    let ids: std::collections::HashSet<String> = got
        .iter()
        .map(|e| e.payload["call_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 20);
    assert_eq!(
        tools.calls.lock().unwrap().len(),
        20,
        "each entry processed by exactly one replica"
    );
}

#[tokio::test]
async fn call_timeout_ms_shorter_than_tool_budget_wins() {
    let bus = bus("ex-call-budget").await;
    let mcp = FakeMcp::new(FakeTools::default());
    mcp.delay_next("order_get_order", Duration::from_millis(400));
    let task = spawn(&bus, mcp, "e-1", Duration::from_secs(3));
    send(
        &bus,
        &call_within(
            "C-4",
            "order",
            "get_order",
            json!({"order_id": "ORD-58790"}),
            100,
        ),
    )
    .await;
    let got = wait_entries(&bus, &bus.streams.tool("order"), 1, is_response).await;
    task.abort();
    let r: ToolResponse = got[0].payload_as().unwrap();
    assert!(!r.ok, "{r:?}");
    assert!(r.retryable, "{r:?}");
    assert_eq!(r.error.as_deref(), Some("timeout after 100 ms"));
}

#[tokio::test]
async fn concurrent_deliveries_of_one_call_call_mcp_once() {
    let bus = bus("ex-inflight").await;
    let tools = FakeTools::default();
    let mcp = FakeMcp::new(tools.clone());
    mcp.delay_next("order_get_order", Duration::from_millis(300));
    let mut ex = ToolExecutor::new(bus.clone(), mcp);
    ex.consume = executor_fast(&ex);
    let c = call(
        "C-I",
        "order",
        "get_order",
        json!({"order_id": "ORD-58790"}),
    );
    let env = Envelope::new(&call_topic(), "test", &c.work.case_id, &c).unwrap();
    // Two replicas hold the same entry (XAUTOCLAIM handed it over while the first still runs).
    let (a, b) = tokio::join!(ex.on_call(env.clone()), ex.on_call(env.clone()));
    assert_eq!(tools.calls.lock().unwrap().len(), 1, "MCP called once");
    let (ok, err): (Vec<_>, Vec<_>) = [a, b].into_iter().partition(|r| r.is_ok());
    assert_eq!(ok.len(), 1, "exactly one delivery executes");
    assert_eq!(err.len(), 1, "the other stays pending for redelivery");
    let e = err[0].as_ref().unwrap_err();
    assert!(format!("{e:#}").contains("in flight"), "{e:#}");
    let responses = entries(&bus, &bus.streams.tool("order"))
        .await
        .iter()
        .filter(|e| is_response(e))
        .count();
    assert_eq!(responses, 1, "one tool.response published");
}

#[tokio::test]
async fn executor_reads_one_entry_at_a_time_by_default() {
    // A batch would sit read-but-unprocessed behind slow calls until other replicas reclaim it.
    let ex = ToolExecutor::new(bus("ex-batch").await, FakeMcp::new(FakeTools::default()));
    assert_eq!(ex.consume.batch, 1);
    assert!(ex.check_reclaim().is_ok(), "defaults are consistent");
}

#[tokio::test]
async fn executor_refuses_to_start_when_entries_could_be_reclaimed_mid_call() {
    let mut ex = ToolExecutor::new(bus("ex-reclaim").await, FakeMcp::new(FakeTools::default()));
    ex.timeout = Duration::from_secs(3);
    ex.faq_timeout = Duration::from_secs(20);
    ex.consume = fast(); // reclaim after 300 ms, far inside a 20 s call
    let res = tokio::time::timeout(Duration::from_secs(2), Arc::new(ex).run("e-1"))
        .await
        .expect("run must refuse at once, not start consuming");
    let err = res.unwrap_err();
    assert!(format!("{err:#}").contains("reclaim_idle"), "{err:#}");
}

#[test]
fn reclaim_window_follows_the_budgets_and_always_passes_its_own_check() {
    use carebus::executor::{check_reclaim, reclaim_idle_for};
    let s = Duration::from_secs;
    // Today's budgets keep the bus default: nothing changes for a 20 s faq.
    assert_eq!(reclaim_idle_for(s(3), s(20)), s(30));
    // A slow local-llm: faq raised to 330 s must not make the executor refuse to start.
    assert_eq!(reclaim_idle_for(s(3), s(330)), s(340));
    assert_eq!(
        reclaim_idle_for(s(400), s(20)),
        s(410),
        "the larger budget counts"
    );
    for faq in [1, 20, 25, 26, 60, 330, 900] {
        let idle = reclaim_idle_for(s(3), s(faq));
        assert!(
            check_reclaim(idle, s(3), s(faq)).is_ok(),
            "faq {faq}s → reclaim {idle:?} must pass check_reclaim"
        );
    }
}

#[test]
fn reclaim_window_must_exceed_the_largest_budget_plus_slack() {
    use carebus::executor::check_reclaim;
    let s = Duration::from_secs;
    assert!(check_reclaim(s(30), s(3), s(20)).is_ok());
    assert!(
        check_reclaim(s(25), s(3), s(20)).is_err(),
        "equal is not enough"
    );
    assert!(check_reclaim(s(25) + Duration::from_millis(1), s(3), s(20)).is_ok());
    assert!(
        check_reclaim(s(30), s(26), s(1)).is_err(),
        "the larger budget counts"
    );
    let err = check_reclaim(Duration::from_millis(300), s(3), s(20)).unwrap_err();
    assert!(
        format!("{err:#}").contains("reclaim_idle 300 ms")
            && format!("{err:#}").contains("20000 ms"),
        "{err:#}"
    );
}
