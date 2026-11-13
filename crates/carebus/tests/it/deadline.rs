//! Case deadline sweeper over a real Redis (:6391). Each test has its own key prefix.
//! Nothing consumes the work streams, so dispatched work stays `outstanding`.

use std::sync::Arc;
use std::time::Duration;

use carebus::bus::{Bus, Envelope};
use carebus::domain::{
    AREAS, Outcome, Phase, Plan, ProcedureSource, QualifiedRequest, REQUEST_QUALIFIED,
    REQUEST_RESOLVED, RequestType, Resolution, Slots, Step, WORK_SUCCEEDED, WorkOutcome,
};
use carebus::orchestrator::Orchestrator;
use carebus::spaces::{ContextSpace, SpaceCatalog};
use redis::AsyncCommands;

use crate::common::{bus, entries};

/// One phase, one step: `order.get_order`.
struct OneStep;
impl ProcedureSource for OneStep {
    async fn plan(&self, _space: &ContextSpace) -> anyhow::Result<Plan> {
        Ok(Plan {
            phases: vec![Phase {
                steps: vec![Step {
                    worker: "order".into(),
                    action: "get_order".into(),
                    as_name: "order".into(),
                    args: Default::default(),
                    side_effect: None,
                }],
                gate: None,
            }],
            procedure: None,
        })
    }
}

fn orch(bus: &Bus, deadline_ms: u64) -> Orchestrator<OneStep> {
    Orchestrator::new(bus.clone(), OneStep).with_case_deadline(Duration::from_millis(deadline_ms))
}

/// A delivery-status request and its (bootstrap) context space.
fn request(case: &str) -> (ContextSpace, QualifiedRequest) {
    let sp = SpaceCatalog::bootstrap()
        .by_request_type("delivery_status")
        .expect("bootstrap space")
        .clone();
    let req = QualifiedRequest {
        case_id: case.into(),
        customer_id: "C-10482".into(),
        request_type: RequestType::delivery_status(),
        area: sp.area.clone(),
        context_space: sp.key.clone(),
        slots: Slots::of(&[("order_id", "ORD-58790")]),
        utterance: "where is it".into(),
        gates: None,
    };
    (sp, req)
}

async fn qualify(o: &Orchestrator<OneStep>, case: &str) -> QualifiedRequest {
    let (sp, req) = request(case);
    o.on_qualified(Envelope::new(&sp.topic(REQUEST_QUALIFIED), "test", case, &req).unwrap())
        .await
        .unwrap();
    req
}

async fn resolutions(bus: &Bus, case: &str) -> Vec<Resolution> {
    let mut out = Vec::new();
    for area in AREAS {
        for e in entries(bus, &bus.streams.customer(area)).await {
            if e.correlation_id == case && e.event() == Some(REQUEST_RESOLVED) {
                out.push(e.payload_as().unwrap());
            }
        }
    }
    out
}

async fn in_zset(bus: &Bus, case: &str) -> bool {
    let s: Option<f64> = bus
        .conn()
        .zscore(bus.streams.key("cases:deadline"), case)
        .await
        .unwrap();
    s.is_some()
}

fn ok_outcome(req: &QualifiedRequest) -> Envelope {
    let o = WorkOutcome {
        work_id: format!("{}:0:order:1", req.case_id),
        case_id: req.case_id.clone(),
        phase: 0,
        as_name: "order".into(),
        worker: "order".into(),
        action: "get_order".into(),
        attempt: 1,
        ok: true,
        result: Some(serde_json::json!({"status": "shipped"})),
        error: None,
        retryable: false,
        request_type: req.request_type.clone(),
    };
    Envelope::new(&req.topic(WORK_SUCCEEDED), "test", &req.case_id, &o).unwrap()
}

#[tokio::test]
async fn unanswered_work_fails_the_case_at_the_deadline() {
    let bus = bus("deadline-fail").await;
    let o = orch(&bus, 300);
    qualify(&o, "CASE-D1").await;
    assert!(in_zset(&bus, "CASE-D1").await);
    assert_eq!(o.sweep_once().await.unwrap(), 0, "not overdue yet");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(o.sweep_once().await.unwrap(), 1);
    let rs = resolutions(&bus, "CASE-D1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, Outcome::Failed);
    assert_eq!(
        rs[0].reason.as_deref(),
        Some("order did not answer in time")
    );
    let err = o.cases().error("CASE-D1").await.unwrap().unwrap();
    assert!(
        err.contains("deadline exceeded") && err.contains("order"),
        "{err}"
    );
    assert!(!in_zset(&bus, "CASE-D1").await);
    assert_eq!(o.sweep_once().await.unwrap(), 0, "swept once only");
}

#[tokio::test]
async fn a_case_that_finishes_in_time_is_never_swept() {
    let bus = bus("deadline-ok").await;
    let o = orch(&bus, 300);
    let req = qualify(&o, "CASE-D2").await;
    assert!(in_zset(&bus, "CASE-D2").await, "created with a deadline");
    o.on_result(ok_outcome(&req)).await.unwrap();
    assert_eq!(resolutions(&bus, "CASE-D2").await.len(), 1);
    assert!(
        !in_zset(&bus, "CASE-D2").await,
        "resolution clears the deadline"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(o.sweep_once().await.unwrap(), 0);
    let rs = resolutions(&bus, "CASE-D2").await;
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].outcome, Outcome::Completed);
}

#[tokio::test]
async fn two_replicas_sweeping_one_case_publish_one_resolution() {
    let bus = bus("deadline-race").await;
    let a = Arc::new(orch(&bus, 200));
    let b = Arc::new(orch(&bus, 200));
    qualify(&a, "CASE-D3").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (x, y) = tokio::join!(
        {
            let a = a.clone();
            async move { a.sweep_once().await.unwrap() }
        },
        {
            let b = b.clone();
            async move { b.sweep_once().await.unwrap() }
        }
    );
    assert_eq!(x + y, 1, "exactly one replica claims the case");
    assert_eq!(resolutions(&bus, "CASE-D3").await.len(), 1);
}

#[tokio::test]
async fn a_late_result_for_a_swept_case_changes_nothing() {
    let bus = bus("deadline-late").await;
    let o = orch(&bus, 100);
    let req = qualify(&o, "CASE-D4").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(o.sweep_once().await.unwrap(), 1);
    o.on_result(ok_outcome(&req)).await.unwrap();
    let rs = resolutions(&bus, "CASE-D4").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, Outcome::Failed);
}

#[tokio::test]
async fn an_expired_case_hash_is_dropped_from_the_deadline_set() {
    let bus = bus("deadline-gone").await;
    let o = orch(&bus, 100);
    qualify(&o, "CASE-D5").await;
    let _: i64 = bus
        .conn()
        .del(bus.streams.key("case:CASE-D5"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(o.sweep_once().await.unwrap(), 0);
    assert!(!in_zset(&bus, "CASE-D5").await);
    assert!(resolutions(&bus, "CASE-D5").await.is_empty());
}

#[tokio::test]
async fn a_claimed_case_stays_leased_and_is_retried_after_the_lease() {
    let bus = bus("deadline-lease").await;
    let o = orch(&bus, 50);
    qualify(&o, "CASE-D6").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cases = o.cases();
    let now = cases.now_ms().await.unwrap();
    assert!(cases.claim_overdue("CASE-D6", now).await.unwrap());
    assert!(
        !cases.claim_overdue("CASE-D6", now + 1).await.unwrap(),
        "leased: nobody else sweeps it"
    );
    assert!(
        in_zset(&bus, "CASE-D6").await,
        "a claim never deletes the entry"
    );
    let after = now + carebus::case::SWEEP_LEASE.as_millis() as i64 + 1;
    assert!(
        cases.claim_overdue("CASE-D6", after).await.unwrap(),
        "retried once the lease runs out"
    );
}

#[tokio::test]
async fn redelivering_a_created_case_keeps_one_deadline_entry() {
    let bus = bus("deadline-create").await;
    let o = orch(&bus, 60_000);
    let req = qualify(&o, "CASE-D7").await;
    assert!(!o.cases().create(&req).await.unwrap());
    let n: i64 = bus
        .conn()
        .zcard(bus.streams.key("cases:deadline"))
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn a_sweep_racing_the_final_result_publishes_one_outcome() {
    for i in 0..25 {
        let bus = bus("deadline-boundary").await;
        let (a, b) = (orch(&bus, 40), orch(&bus, 40));
        let case = format!("CASE-R{i}");
        let req = qualify(&a, &case).await;
        tokio::time::sleep(Duration::from_millis(45)).await;
        let (_, _) = tokio::join!(async { b.sweep_once().await.unwrap() }, async {
            a.on_result(ok_outcome(&req)).await.unwrap()
        });
        let rs = resolutions(&bus, &case).await;
        assert!(!rs.is_empty(), "round {i}: no resolution");
        assert!(
            rs.iter().all(|r| r.outcome == rs[0].outcome),
            "round {i}: split outcomes {rs:?}"
        );
        let stored: String = bus
            .conn()
            .hget(bus.streams.key(&format!("case:{case}")), "resolved")
            .await
            .unwrap();
        assert_eq!(
            stored,
            serde_json::to_string(&rs[0].outcome).unwrap(),
            "round {i}"
        );
    }
}

/// `OneStep` as the procedure registry would hand it out: stamped with the version it chose.
struct Versioned;
impl ProcedureSource for Versioned {
    async fn plan(&self, space: &ContextSpace) -> anyhow::Result<Plan> {
        let mut p = OneStep.plan(space).await?;
        p.procedure = Some("delivery@2.1.0".into());
        Ok(p)
    }
}

/// The orchestrator stamps the chosen procedure version on the work it dispatches and on the resolution, so the
/// recorder can attribute the case to a version.
#[tokio::test]
async fn work_and_resolution_carry_the_procedure_version() {
    let bus = bus("deadline-procedure").await;
    let o = Orchestrator::new(bus.clone(), Versioned);
    let (sp, req) = request("CASE-P1");
    o.on_qualified(Envelope::new(&sp.topic(REQUEST_QUALIFIED), "test", "CASE-P1", &req).unwrap())
        .await
        .unwrap();
    let work = entries(&bus, &bus.streams.worker(&sp.area, "order")).await;
    assert_eq!(work.len(), 1, "{work:?}");
    assert_eq!(work[0].procedure.as_deref(), Some("delivery@2.1.0"));
    o.on_result(ok_outcome(&req)).await.unwrap();
    let mut resolved = Vec::new();
    for e in entries(&bus, &bus.streams.customer(&sp.area)).await {
        if e.correlation_id == "CASE-P1" && e.event() == Some(REQUEST_RESOLVED) {
            resolved.push(e);
        }
    }
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].procedure.as_deref(), Some("delivery@2.1.0"));
    assert_eq!(
        resolved[0].payload_as::<Resolution>().unwrap().outcome,
        Outcome::Completed
    );
}

/// A built-in plan has no version: nothing is stamped, and the field is absent from the stored JSON.
#[tokio::test]
async fn a_plan_without_a_version_stamps_nothing() {
    let bus = bus("deadline-noprocedure").await;
    let o = orch(&bus, 60_000);
    let req = qualify(&o, "CASE-P2").await;
    let work = entries(&bus, &bus.streams.worker(&req.area, "order")).await;
    assert_eq!(work[0].procedure, None);
    let raw = serde_json::to_value(&work[0]).unwrap();
    assert!(raw.get("procedure").is_none(), "{raw}");
}

/// Entries written before the field existed still deserialize.
#[test]
fn an_envelope_without_procedure_deserializes() {
    let e: Envelope = serde_json::from_value(serde_json::json!({
        "id": "x", "topic": "t", "from": "f", "correlation_id": "c", "published_at_ms": 1, "payload": null
    }))
    .unwrap();
    assert_eq!(e.procedure, None);
}
