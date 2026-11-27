//! `handover.request` hands over the case's own conversation: the id comes from the case link
//! (`chat::case_conv_key`), never from a plan slot the model fills; a case with no link fails before any tool
//! call. Redis is real.
use carebus::bus::Envelope;
use carebus::chat;
use carebus::domain::{
    RequestType, TOOL_CALL, WORK_FAILED, WORK_REQUESTED, WorkOutcome, WorkRequest,
};
use carebus::spaces::{ContextSpace, SpaceCatalog};
use carebus::worker::{HANDOVER_NO_CONVERSATION, Worker};
use serde_json::json;

use crate::common::{bus, entries, wait_entries};

fn handover_space() -> ContextSpace {
    ContextSpace {
        key: "website:handover".into(),
        area: "handover".into(),
        request_type: "handover".into(),
        required: vec!["reason".into()],
        procedure: "website_handover".into(),
    }
}

fn request(case_id: &str) -> WorkRequest {
    WorkRequest {
        work_id: format!("{case_id}:0:handover:1"),
        idempotency_key: format!("{case_id}:0:handover"),
        case_id: case_id.into(),
        phase: 0,
        as_name: "handover".into(),
        worker: "handover".into(),
        action: "request".into(),
        args: json!({"reason": "I want to talk to George"}),
        attempt: 1,
        request_type: RequestType::new("handover").unwrap(),
        side_effect: None,
    }
}

async fn work(w: &Worker, sp: &ContextSpace, req: &WorkRequest) {
    w.on_work(Envelope::new(&sp.topic(WORK_REQUESTED), "orchestrator", &req.case_id, req).unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn the_tool_call_carries_the_cases_own_conversation() {
    let bus = bus("handover-scope").await;
    let _: () = redis::cmd("SET")
        .arg(chat::case_conv_key(&bus, "CASE-h1"))
        .arg("conv-42")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let sp = handover_space();
    let w = Worker::new(bus.clone(), "handover").with_spaces(SpaceCatalog::new(vec![sp.clone()]));
    work(&w, &sp, &request("CASE-h1")).await;
    let calls: Vec<carebus::domain::ToolCall> = entries(&bus, &bus.streams.tool("handover"))
        .await
        .iter()
        .filter(|e| e.event() == Some(TOOL_CALL))
        .map(|e| e.payload_as().unwrap())
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].mcp_tool, "handover_request");
    assert_eq!(calls[0].arguments["conversation_id"], "conv-42");
    assert_eq!(calls[0].arguments["reason"], "I want to talk to George");
    assert_eq!(calls[0].arguments["idempotency_key"], "CASE-h1:0:handover");
}

#[tokio::test]
async fn a_case_without_a_conversation_fails_before_any_call() {
    let bus = bus("handover-nolink").await;
    let sp = handover_space();
    let w = Worker::new(bus.clone(), "handover").with_spaces(SpaceCatalog::new(vec![sp.clone()]));
    work(&w, &sp, &request("CASE-h2")).await;
    let failed: Vec<WorkOutcome> =
        wait_entries(&bus, &bus.streams.worker_results(&sp.area), 1, |e| {
            e.event() == Some(WORK_FAILED)
        })
        .await
        .iter()
        .map(|e| e.payload_as().unwrap())
        .collect();
    assert_eq!(failed[0].error.as_deref(), Some(HANDOVER_NO_CONVERSATION));
    assert!(!failed[0].retryable);
    assert!(
        entries(&bus, &bus.streams.tool("handover"))
            .await
            .is_empty(),
        "no handover_request"
    );
}
