//! MCP tasks against a REAL nonagenticai_mcp_server (tasks extension, `MCP_TASKS_ENABLED=true`) whose external driver
//! talks to fr: the MCP-tasks client spec (2026-10-30), acceptance step 2. A long-running tool
//! starts a task as one staff user, the task asks for an approval, a different staff user answers it, and it
//! completes; a second task is cancelled; an unknown task id is lost.
//!
//! Run (credentials as the server's licence introspection accepts them; `LIVE_FR_LOG_URL` optional, a fake fr's
//! received-message log):
//! MCP_URL=… LIVE_LICENCE=<mcp licence> LIVE_BINDING=<binding token> LIVE_TENANT_ID=<uuid> LIVE_FR_TOOL=<tool> [LIVE_FR_ARGS='{…}'] [LIVE_FR_LOG_URL=…] \
//!   cargo test -p carebus --test it live_mcp_tasks -- --ignored --nocapture
use std::time::{Duration, Instant};

use carebus::mcp_http::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use carebus::mcp_tasks::{
    StaffActor, TaskError, TaskOutcome, TaskSnapshot, TaskStart, input_questions, outcome_of,
    validate_answer,
};
use serde_json::{Value, json};
use uuid::Uuid;

struct Live {
    mcp: HttpMcp,
    tenant: Uuid,
    tool: String,
    args: Value,
    fr_log: Option<String>,
}

fn live() -> Option<Live> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (Some(url), Some(licence), Some(tenant), Some(tool)) = (
        var("MCP_URL"),
        var("LIVE_LICENCE"),
        var("LIVE_TENANT_ID"),
        var("LIVE_FR_TOOL"),
    ) else {
        eprintln!("MCP_URL, LIVE_LICENCE, LIVE_TENANT_ID or LIVE_FR_TOOL not set; skipping");
        return None;
    };
    let strata = var("LIVE_STRATA_TOKEN").unwrap_or_else(|| "str-e2e".into());
    // Tasks need a binding caller: the tenant's `mcp` licence plus `X-MCP-Binding` (LIVE_BINDING).
    let binding = var("LIVE_BINDING");
    let creds = TenantCredentials::new(FnSource(move |_tenant: Uuid, _fresh: bool| {
        let out = McpCredentials::new(&licence, &strata, Instant::now() + Duration::from_secs(300))
            .and_then(|c| match &binding {
                Some(b) => c.with_binding(b),
                None => Ok(c),
            });
        async move { out }
    }));
    Some(Live {
        mcp: HttpMcp::new(url, creds),
        tenant: tenant.parse().expect("LIVE_TENANT_ID is a uuid"),
        tool,
        args: var("LIVE_FR_ARGS")
            .map(|a| serde_json::from_str(&a).expect("LIVE_FR_ARGS is JSON"))
            .unwrap_or_else(|| json!({})),
        fr_log: var("LIVE_FR_LOG_URL"),
    })
}

/// Polls `task_id` until `done` holds for a snapshot (every 500 ms, at most `limit`).
async fn poll_until(
    l: &Live,
    task_id: &str,
    limit: Duration,
    done: impl Fn(&TaskSnapshot) -> bool,
) -> TaskSnapshot {
    let start = Instant::now();
    loop {
        let s = l.mcp.get_task(l.tenant, task_id).await.expect("tasks/get");
        eprintln!("tasks/get {task_id}: {} {:?}", s.status, s.status_message);
        if done(&s) {
            return s;
        }
        assert!(
            start.elapsed() < limit,
            "task {task_id} stuck at {}: {}",
            s.status,
            s.detail
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn started(l: &Live, s: TaskStart) -> String {
    match s {
        TaskStart::Task(h) => {
            eprintln!(
                "task {} status={} poll={:?}ms ttl={:?}ms",
                h.task_id, h.status, h.poll_interval_ms, h.ttl_ms
            );
            h.task_id
        }
        TaskStart::Completed(v) => panic!(
            "{} came back as a plain result, not a task (does the server advertise the tasks extension?): {v}",
            l.tool
        ),
    }
}

#[tokio::test]
#[ignore]
async fn live_task_is_answered_by_a_human_and_completes() {
    let Some(l) = live() else { return };
    let starter = StaffActor {
        tenant: l.tenant,
        staff_user: "staff-e2e-starter".into(),
    };
    let answerer = StaffActor {
        tenant: l.tenant,
        staff_user: "staff-e2e-answerer".into(),
    };

    let id = started(
        &l,
        l.mcp
            .start_task(&starter, &l.tool, l.args.clone())
            .await
            .expect("tools/call"),
    );

    let waiting = poll_until(&l, &id, Duration::from_secs(60), |s| {
        s.status == "input_required" || s.is_terminal()
    })
    .await;
    assert_eq!(
        waiting.status, "input_required",
        "the task asks for an approval: {}",
        waiting.detail
    );
    let questions = input_questions(&waiting.detail);
    assert!(
        !questions.is_empty(),
        "an approval is open: {}",
        waiting.detail
    );
    let q = &questions[0];
    eprintln!(
        "question {}: {}\nschema {}",
        q.key, q.message, q.requested_schema
    );

    // The UI's check runs against the server's own schema: a rejection without a reason is refused locally.
    assert!(
        validate_answer(
            &q.requested_schema,
            "accept",
            Some(&json!({"decision": "rejected"}))
        )
        .is_err()
    );
    let answer = validate_answer(
        &q.requested_schema,
        "accept",
        Some(&json!({"decision": "approved", "comment": "e2e"})),
    )
    .expect("an approval meets the schema");
    l.mcp
        .answer_task(&answerer, &id, &q.key, answer)
        .await
        .expect("tasks/update");

    let done = poll_until(&l, &id, Duration::from_secs(60), |s| s.is_terminal()).await;
    let outcome = outcome_of(&done).expect("terminal");
    eprintln!("outcome {}", outcome.to_json());
    assert!(
        matches!(outcome, TaskOutcome::Completed(Ok(_))),
        "completed: {}",
        done.detail
    );

    if let Some(url) = &l.fr_log {
        let log = reqwest::get(url).await.unwrap().text().await.unwrap();
        for who in [&starter, &answerer] {
            assert!(
                log.contains(&who.address()),
                "fr received {} : {log}",
                who.address()
            );
        }
    }
}

#[tokio::test]
#[ignore]
async fn live_task_is_cancelled_and_an_unknown_task_is_lost() {
    let Some(l) = live() else { return };
    let starter = StaffActor {
        tenant: l.tenant,
        staff_user: "staff-e2e-starter".into(),
    };
    let id = started(
        &l,
        l.mcp
            .start_task(&starter, &l.tool, l.args.clone())
            .await
            .expect("tools/call"),
    );
    l.mcp
        .cancel_task(l.tenant, &id)
        .await
        .expect("tasks/cancel");
    let done = poll_until(&l, &id, Duration::from_secs(60), |s| s.is_terminal()).await;
    let outcome = outcome_of(&done).expect("terminal");
    eprintln!("outcome after cancel {}", outcome.to_json());
    // Cancellation is cooperative: completed is allowed by the protocol; failed is not.
    assert!(
        matches!(outcome, TaskOutcome::Cancelled | TaskOutcome::Completed(_)),
        "{}",
        done.detail
    );

    let lost = l.mcp.get_task(l.tenant, &Uuid::new_v4().to_string()).await;
    assert!(matches!(lost, Err(TaskError::Lost(_))), "{lost:?}");
}
