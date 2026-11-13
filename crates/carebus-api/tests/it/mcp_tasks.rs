//! MCP tasks (migration 0029, carebus_api::mcp_tasks) against a stub MCP server that offers the tasks extension: the
//! poller stores each status and honours `pollIntervalMs`, `-32602` ends a task `lost`, a claimed-but-abandoned row
//! is claimed again after its lease, and staff answer (as themselves, a rejection needs a reason, a double submit is a
//! no-op) and cancel through `/api/jobs/{job}/mcp-tasks`.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use carebus::mcp_http::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use carebus_api::mcp_tasks::TaskPoller;
use serde_json::{Value, json};

const TASKS: &str = "io.modelcontextprotocol/tasks";
const KEY: &str = "t-ask:00000000000000aa";

type Seen = Arc<Mutex<Vec<Value>>>;

fn task(id: &str, status: &str) -> Value {
    json!({"resultType": "complete", "taskId": id, "status": status, "createdAt": "2026-10-30T10:00:00Z",
           "lastUpdatedAt": "2026-10-30T10:00:01Z", "ttlMs": 86_400_000, "pollIntervalMs": 2000})
}

/// `tasks/get`: t-work working, t-ask input_required (one approval), t-done completed, anything else -32602.
async fn stub_mcp() -> (Arc<HttpMcp>, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::any(move |method: Method, body: String| {
            let log = log.clone();
            async move {
                if method != Method::POST {
                    return StatusCode::METHOD_NOT_ALLOWED.into_response();
                }
                let req: Value = serde_json::from_str(&body).unwrap();
                log.lock().unwrap().push(req.clone());
                let id = req["id"].clone();
                let ok = |r: Value| json!({"jsonrpc": "2.0", "id": id, "result": r});
                let tid = req["params"]["taskId"].as_str().unwrap_or("");
                let known = ["t-work", "t-ask", "t-done", "t-abandoned"].contains(&tid);
                let reply = match req["method"].as_str().unwrap_or("") {
                    "server/discover" => ok(json!({"supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}, "extensions": {TASKS: {}}}, "ttlMs": 0, "cacheScope": "private", "resultType": "complete", "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "stub", "version": "0"}}})),
                    "initialize" => ok(json!({"protocolVersion": req["params"]["protocolVersion"],
                        "capabilities": {"tools": {}, "extensions": {TASKS: {}}},
                        "serverInfo": {"name": "stub", "version": "0"}})),
                    "tasks/get" | "tasks/update" | "tasks/cancel" if !known => json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": "unknown task"}}),
                    "tasks/update" | "tasks/cancel" => ok(json!({"resultType": "complete"})),
                    "tools/call" if req["params"]["name"] == "web_scrape" => {
                        let mut t = task("t-new", "working");
                        t["resultType"] = json!("task");
                        ok(t)
                    }
                    "tasks/get" => ok(match tid {
                        "t-ask" => {
                            let mut t = task(tid, "input_required");
                            t["statusMessage"] = json!("waiting for approval in fr");
                            t["inputRequests"] = json!({KEY: {"method": "elicitation/create", "params": {
                                "message": "Merge the release branch?\n\n3 files changed",
                                "requestedSchema": {"type": "object", "required": ["decision"], "properties": {
                                    "decision": {"type": "string", "enum": ["approved", "rejected"]},
                                    "reason": {"type": "string"}, "comment": {"type": "string"}}}}}});
                            t
                        }
                        "t-done" => {
                            let mut t = task(tid, "completed");
                            t["result"] = json!({"content": [], "isError": false, "structuredContent":
                                {"run_id": "r-9", "status": "done", "result": {"pr": 12}, "error": null}});
                            t
                        }
                        _ => task(tid, "working"),
                    }),
                    _ => return StatusCode::ACCEPTED.into_response(),
                };
                axum::Json(reply).into_response()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let creds = TenantCredentials::new(FnSource(|tenant: uuid::Uuid, _fresh: bool| async move {
        McpCredentials::new(
            &format!("lic-{tenant}"),
            &format!("str-{tenant}"),
            Instant::now() + Duration::from_secs(300),
        )
    }));
    (
        Arc::new(HttpMcp::new(format!("http://{addr}/mcp"), creds)),
        seen,
    )
}

fn of(seen: &Seen, method: &str) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|b| b["method"] == method)
        .cloned()
        .collect()
}

/// Inserted as the bootstrap tenant/job (the owner pool's GUCs fill the defaults), due now.
async fn insert(e: &crate::common::TestEnv, task_id: &str, status: &str, detail: Option<Value>) {
    let terminal = matches!(status, "completed" | "cancelled" | "failed" | "lost");
    sqlx::query(
        "INSERT INTO mcp_tasks (task_id, tool, status, last_detail, created_by, poll_after, terminal_at)
         VALUES ($1, 'fr_project_start', $2, $3, 'u-admin', now() - interval '1 second',
                 CASE WHEN $4 THEN now() END)",
    )
    .bind(task_id)
    .bind(status)
    .bind(detail)
    .bind(terminal)
    .execute(crate::common::pool(e))
    .await
    .unwrap();
}

#[derive(sqlx::FromRow, Debug)]
struct Row {
    status: String,
    status_message: Option<String>,
    result: Option<Value>,
    last_detail: Option<Value>,
    terminal: bool,
    wait_s: f64,
}

async fn row(e: &crate::common::TestEnv, task_id: &str) -> Row {
    sqlx::query_as(
        "SELECT status, status_message, result, last_detail, terminal_at IS NOT NULL AS terminal,
                extract(epoch FROM poll_after - now())::float8 AS wait_s
           FROM mcp_tasks WHERE task_id = $1",
    )
    .bind(task_id)
    .fetch_one(crate::common::pool(e))
    .await
    .unwrap()
}

fn poller(e: &crate::common::TestEnv, mcp: Arc<HttpMcp>) -> TaskPoller {
    TaskPoller {
        db: e.state.db.clone(),
        mcp,
        lease_s: 30,
        batch: 20,
    }
}

#[tokio::test]
async fn the_poller_stores_each_status_and_honours_the_poll_interval() {
    let e = crate::common::env("mcp-tasks-poll").await;
    let (mcp, seen) = stub_mcp().await;
    for id in ["t-work", "t-ask", "t-done", "t-gone"] {
        insert(&e, id, "working", None).await;
    }
    let p = poller(&e, mcp);
    assert_eq!(p.tick().await.unwrap(), 4);

    let w = row(&e, "t-work").await;
    assert_eq!(w.status, "working");
    assert!(!w.terminal);
    assert!(
        (1.0..=3.0).contains(&w.wait_s),
        "pollIntervalMs 2000 → next poll in ~2 s: {w:?}"
    );

    let a = row(&e, "t-ask").await;
    assert_eq!(a.status, "input_required");
    assert_eq!(
        a.status_message.as_deref(),
        Some("waiting for approval in fr")
    );
    assert!(a.last_detail.unwrap()["inputRequests"][KEY].is_object());

    let d = row(&e, "t-done").await;
    assert_eq!(d.status, "completed");
    assert!(d.terminal);
    assert_eq!(
        d.result.unwrap(),
        json!({"outcome": "completed", "ok": true,
               "result": {"run_id": "r-9", "status": "done", "result": {"pr": 12}, "error": null}})
    );

    let g = row(&e, "t-gone").await;
    assert_eq!(g.status, "lost");
    assert!(g.terminal);
    assert_eq!(g.result.unwrap()["outcome"], "lost");

    // Nothing is due now; terminal rows are never polled again.
    assert_eq!(p.tick().await.unwrap(), 0);
    assert_eq!(of(&seen, "tasks/get").len(), 4);
}

/// A poller killed after claiming leaves the row leased; once the lease passes, another poller claims it again.
#[tokio::test]
async fn a_row_claimed_by_a_killed_poller_is_claimed_again_after_its_lease() {
    let e = crate::common::env("mcp-tasks-restart").await;
    let (mcp, _) = stub_mcp().await;
    insert(&e, "t-abandoned", "working", None).await;
    let claimed: Vec<(String,)> =
        sqlx::query_as("SELECT task_id FROM carebus_mcp_task_claim(5, 10)")
            .fetch_all(&e.state.db)
            .await
            .unwrap();
    assert_eq!(claimed, [("t-abandoned".to_string(),)]);
    // ...and the process dies here. Within the lease nobody else takes it.
    let p = poller(&e, mcp);
    assert_eq!(p.tick().await.unwrap(), 0);
    // The lease passes.
    sqlx::query("UPDATE mcp_tasks SET poll_after = now() - interval '1 second' WHERE task_id = 't-abandoned'")
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(p.tick().await.unwrap(), 1);
    assert_eq!(row(&e, "t-abandoned").await.status, "working");
}

#[tokio::test]
async fn staff_answer_as_themselves_once_and_cancel() {
    let mut e = crate::common::env("mcp-tasks-routes").await;
    let (mcp, seen) = stub_mcp().await;
    e.state.mcp_catalog = Some(mcp.clone());
    let app = carebus_api::router(e.state.clone());
    insert(&e, "t-ask", "working", None).await;
    insert(&e, "t-work", "working", None).await;
    insert(&e, "t-old", "completed", None).await;
    poller(&e, mcp).tick().await.unwrap();

    let user = crate::common::user();
    let (s, b) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j("/mcp-tasks?state=open"), Some(&user)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "open tasks only: {b}");
    assert_eq!(items[0]["task_id"], "t-ask", "waiting tasks first");
    assert_eq!(items[0]["questions"][0]["key"], KEY);
    assert_eq!(
        items[0]["questions"][0]["requested_schema"]["required"],
        json!(["decision"])
    );

    let answer = |body: Value| {
        crate::common::json(
            "POST",
            &crate::common::j("/mcp-tasks/t-ask/answers"),
            Some(&user),
            body,
        )
    };
    // A rejection without a reason never reaches the server.
    let (s, b) = crate::common::send(
        &app,
        answer(json!({"key": KEY, "action": "accept",
        "content": {"decision": "rejected"}})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_answer");
    assert!(of(&seen, "tasks/update").is_empty());

    let (s, b) = crate::common::send(&app, answer(json!({"key": KEY, "action": "accept",
        "content": {"decision": "rejected", "reason": "touches billing", "comment": "ask finance"}}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["sent"], true);
    assert_eq!(
        b["task"]["questions"],
        json!([]),
        "an answered question is no longer open here"
    );
    let upd = of(&seen, "tasks/update");
    assert_eq!(upd.len(), 1);
    assert_eq!(
        upd[0]["params"]["inputResponses"][KEY],
        json!({"action": "accept", "content": {"decision": "rejected", "reason": "touches billing", "comment": "ask finance"}})
    );
    assert_eq!(
        upd[0]["params"]["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("care-bus:{}:u-user", crate::common::BOOT_TENANT)})
    );

    // A double submit is a no-op.
    let (s, b) = crate::common::send(
        &app,
        answer(json!({"key": KEY, "action": "accept",
        "content": {"decision": "approved"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["already_answered"], true);
    assert_eq!(of(&seen, "tasks/update").len(), 1);

    // A question the task is not waiting on (answered on another channel) is 409.
    let (s, b) = crate::common::send(
        &app,
        answer(json!({"key": "t-ask:ffff", "action": "decline"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "answered_elsewhere");

    // Cancel: acknowledged, the row stays open until the server says cancelled.
    let (s, b) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/mcp-tasks/t-work/cancel"),
            Some(&user),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["sent"], true);
    assert_eq!(b["task"]["cancel_requested"], true);
    assert_eq!(b["task"]["status"], "working");
    assert_eq!(of(&seen, "tasks/cancel")[0]["params"]["taskId"], "t-work");
    let (s, b) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/mcp-tasks/t-old/cancel"),
            Some(&user),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "task_finished");

    // No caller without a person: anonymous is 401.
    let (s, _) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j("/mcp-tasks"), None),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

/// A run parked as `waiting` on `task_id`: the research run, its waiting tool step and the task row (due now).
async fn waiting_run(e: &crate::common::TestEnv, task_id: &str) -> uuid::Uuid {
    let run = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO research_runs (id, profile, question, created_by, status, started_at, updated_at)
         VALUES ($1, 'research', 'q', 'u-user', 'waiting', now() - interval '1 hour', now() - interval '30 minutes')",
    )
    .bind(run)
    .execute(crate::common::pool(e))
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO research_steps (run_id, ordinal, kind, request, response, status)
         VALUES ($1, 0, 'tool', jsonb_build_object('tool_call_id', 'c1', 'name', 'fr_project_start', 'arguments', '{}'::jsonb),
                 jsonb_build_object('ok', true, 'text', 'waiting', 'task_id', $2::text, 'waiting', true), 'ok')",
    )
    .bind(run)
    .bind(task_id)
    .execute(crate::common::pool(e))
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mcp_tasks (task_id, tool, run_id, created_by, poll_after)
         VALUES ($1, 'fr_project_start', $2, 'u-user', now() - interval '1 second')",
    )
    .bind(task_id)
    .bind(run)
    .execute(crate::common::pool(e))
    .await
    .unwrap();
    run
}

async fn run_and_step(e: &crate::common::TestEnv, run: uuid::Uuid) -> (String, String, Value) {
    let (status,): (String,) = sqlx::query_as("SELECT status FROM research_runs WHERE id = $1")
        .bind(run)
        .fetch_one(crate::common::pool(e))
        .await
        .unwrap();
    let (step_status, response): (String, Value) = sqlx::query_as(
        "SELECT status, response FROM research_steps WHERE run_id = $1 AND ordinal = 0",
    )
    .bind(run)
    .fetch_one(crate::common::pool(e))
    .await
    .unwrap();
    (status, step_status, response)
}

/// C6: the poller finishing the task writes its result into the waiting step and requeues the run.
#[tokio::test]
async fn a_finished_task_writes_its_result_into_the_waiting_step_and_requeues_the_run() {
    let e = crate::common::env("mcp-tasks-resume").await;
    let (mcp, _) = stub_mcp().await;
    let run = waiting_run(&e, "t-done").await;
    let (status, step_status, response) = run_and_step(&e, run).await;
    assert_eq!((status.as_str(), step_status.as_str()), ("waiting", "ok"));
    assert_eq!(response["waiting"], true);

    assert_eq!(poller(&e, mcp).tick().await.unwrap(), 1);

    let (status, step_status, response) = run_and_step(&e, run).await;
    assert_eq!(status, "queued", "the researcher claims it again");
    assert_eq!(step_status, "ok");
    assert_eq!(response["task_id"], "t-done");
    assert_eq!(response["ok"], true);
    assert!(response.get("waiting").is_none(), "{response}");
    let text = response["text"].as_str().unwrap();
    assert!(
        text.contains("\"pr\": 12"),
        "the result as a tool result: {text}"
    );
    // The half hour spent waiting is not counted against the wall-clock budget (1 h old, parked 30 min ago).
    let (elapsed_s,): (f64,) = sqlx::query_as(
        "SELECT extract(epoch FROM now() - started_at)::float8 FROM research_runs WHERE id = $1",
    )
    .bind(run)
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert!(
        (1700.0..1900.0).contains(&elapsed_s),
        "the waiting half hour was credited back: {elapsed_s}"
    );
}

/// A lost task ends the step as an error the model reads, and the run still resumes.
#[tokio::test]
async fn a_lost_task_resumes_the_run_with_an_error_step() {
    let e = crate::common::env("mcp-tasks-resume-lost").await;
    let (mcp, _) = stub_mcp().await;
    let run = waiting_run(&e, "t-gone").await;
    poller(&e, mcp).tick().await.unwrap();
    let (status, step_status, response) = run_and_step(&e, run).await;
    assert_eq!((status.as_str(), step_status.as_str()), ("queued", "error"));
    assert_eq!(response["ok"], false);
    assert!(
        response["text"]
            .as_str()
            .unwrap()
            .starts_with("task lost: "),
        "{response}"
    );
}

/// Cancelling a waiting run cancels it at once and flags its task; the poller then sends tasks/cancel, and the task
/// ending later does not resurrect the run.
#[tokio::test]
async fn cancelling_a_waiting_run_cancels_it_and_its_task() {
    let mut e = crate::common::env("mcp-tasks-cancel-run").await;
    let (mcp, seen) = stub_mcp().await;
    e.state.mcp_catalog = Some(mcp.clone());
    let app = carebus_api::router(e.state.clone());
    let run = waiting_run(&e, "t-done").await;

    let (s, b) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/research-runs/{run}/cancel")),
            Some(&crate::common::user()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "cancelled");
    let (flagged,): (bool,) =
        sqlx::query_as("SELECT cancel_requested FROM mcp_tasks WHERE task_id = 't-done'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert!(flagged, "the task is flagged so the poller cancels it");

    poller(&e, mcp).tick().await.unwrap();
    assert_eq!(of(&seen, "tasks/cancel")[0]["params"]["taskId"], "t-done");
    let (status, ..) = run_and_step(&e, run).await;
    assert_eq!(
        status, "cancelled",
        "a finished task never resurrects a cancelled run"
    );
}

/// C9: a research run's long-running tool starts its task on behalf of the person who started the run (fr refuses a
/// project without an actor), and the task is kept under that run.
#[tokio::test]
async fn a_research_tool_starts_its_task_as_the_person_who_started_the_run() {
    use carebus_api::research::Profile;
    use carebus_api::research::store::RunScope;
    use carebus_api::research::tools::{LiveTools, ToolCtx, ToolSet};

    let e = crate::common::env("mcp-tasks-start-actor").await;
    let (mcp, seen) = stub_mcp().await;
    let run = uuid::Uuid::new_v4();
    let (tenant, job): (uuid::Uuid, uuid::Uuid) = sqlx::query_as(
        "INSERT INTO research_runs (id, profile, question, created_by, status)
         VALUES ($1, 'research', 'q', 'u-user', 'running') RETURNING tenant_id, job_id",
    )
    .bind(run)
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    let tools = LiveTools::new(Some(mcp.clone()), None, reqwest::Client::new()).with_tasks(
        e.state.db.clone(),
        vec!["web_scrape".into()],
        mcp,
    );
    let ctx = ToolCtx {
        tenant,
        profile: Profile::Research,
        store_ids: &[],
        actor: None,
        run: Some(RunScope { tenant, job, run }),
        created_by: "u-user",
    };
    let out = tools
        .call(&ctx, "web_scrape", &json!({"url": "https://example.org"}))
        .await;
    assert_eq!(out.task_id.as_deref(), Some("t-new"), "{}", out.text);

    let call = &of(&seen, "tools/call")[0];
    assert_eq!(
        call["params"]["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("care-bus:{tenant}:u-user")})
    );
    let (run_id, created_by): (Option<uuid::Uuid>, String) =
        sqlx::query_as("SELECT run_id, created_by FROM mcp_tasks WHERE task_id = 't-new'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!((run_id, created_by.as_str()), (Some(run), "u-user"));
}
