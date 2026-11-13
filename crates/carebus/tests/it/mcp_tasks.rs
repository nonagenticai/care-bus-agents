//! MCP tasks (crate::mcp_tasks) against a stub streamable-HTTP MCP server: the session declares the extension, a
//! long-running call comes back as a task, `tasks/get` / `tasks/update` / `tasks/cancel` reach the server with the
//! tenant's credentials, the answer carries the deciding human, an unknown task is `Lost`, and a server without the
//! extension gets a plain call.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carebus::mcp_http::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use carebus::mcp_tasks::{
    StaffActor, TaskError, TaskOutcome, TaskStart, input_questions, outcome_of,
};
use serde_json::{Value, json};
use uuid::Uuid;

const A: Uuid = Uuid::from_u128(0xa);
const TASKS: &str = "io.modelcontextprotocol/tasks";

fn creds() -> TenantCredentials {
    TenantCredentials::new(FnSource(|tenant: Uuid, _fresh: bool| {
        let out = McpCredentials::new(
            &format!("lic-{tenant}"),
            &format!("str-{tenant}"),
            Instant::now() + Duration::from_secs(300),
        );
        async move { out }
    }))
}

#[derive(Clone, Default)]
struct Log {
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Log {
    fn of(&self, method: &str) -> Vec<Value> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b["method"] == method)
            .cloned()
            .collect()
    }
}

fn task_wire(status: &str) -> Value {
    json!({"taskId": "t-1", "status": status, "createdAt": "2026-10-30T10:00:00Z",
           "lastUpdatedAt": "2026-10-30T10:00:01Z", "ttlMs": 86_400_000, "pollIntervalMs": 2000})
}

/// A stub MCP server. `with_tasks` decides whether `initialize` advertises the extension. `tools/call` of `long`
/// returns a task; `tasks/get` of `t-1` walks working → input_required → completed; any other task id is -32602.
async fn server(log: Log, with_tasks: bool) -> String {
    use axum::http::{Method, StatusCode};
    use axum::response::IntoResponse;
    let polls = Arc::new(Mutex::new(0usize));
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::any(move |method: Method, body: String| {
            let log = log.clone();
            let polls = polls.clone();
            async move {
                if method != Method::POST {
                    return StatusCode::METHOD_NOT_ALLOWED.into_response();
                }
                let req: Value = serde_json::from_str(&body).unwrap();
                log.bodies.lock().unwrap().push(req.clone());
                let id = req["id"].clone();
                let m = req["method"].as_str().unwrap_or("");
                let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
                let reply = match m {
                    // A 2026-07-28 server (nonagenticai_mcp_server since #62) answers server/discover and advertises
                    // the extension there; the one without tasks predates discover and falls back to initialize.
                    "server/discover" if with_tasks => ok(json!({"supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}, "extensions": {TASKS: {}}}, "ttlMs": 0, "cacheScope": "private", "resultType": "complete", "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "stub", "version": "0"}}})),
                    "server/discover" => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
                    "initialize" => {
                        let mut caps = json!({"tools": {}});
                        if with_tasks {
                            caps["extensions"] = json!({TASKS: {}});
                        }
                        ok(json!({"protocolVersion": req["params"]["protocolVersion"], "capabilities": caps,
                                  "serverInfo": {"name": "stub", "version": "0"}}))
                    }
                    "tools/call" if req["params"]["name"] == "long" && with_tasks => {
                        let mut t = task_wire("working");
                        t["resultType"] = json!("task");
                        ok(t)
                    }
                    "tools/call" => ok(json!({"content": [], "isError": false,
                        "structuredContent": {"run_id": "r-1", "status": "queued", "result": null, "error": null}})),
                    "tasks/get" if req["params"]["taskId"] == "t-1" => {
                        let n = {
                            let mut p = polls.lock().unwrap();
                            *p += 1;
                            *p
                        };
                        let mut t = task_wire(match n {
                            1 => "working",
                            2 => "input_required",
                            _ => "completed",
                        });
                        t["resultType"] = json!("complete");
                        if n == 2 {
                            t["inputRequests"] = json!({"t-1:00000000000000aa": {"method": "elicitation/create",
                                "params": {"message": "Deploy?", "requestedSchema": {"type": "object",
                                    "required": ["decision"], "properties": {"decision": {"type": "string",
                                    "enum": ["approved", "rejected"]}, "reason": {"type": "string"}}}}}});
                        }
                        if n >= 3 {
                            t["result"] = json!({"content": [], "isError": false,
                                "structuredContent": {"run_id": "r-1", "status": "done", "result": {"ok": 1}, "error": null}});
                        }
                        ok(t)
                    }
                    "tasks/get" | "tasks/update" | "tasks/cancel" if req["params"]["taskId"] != "t-1" => json!({
                        "jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "unknown task"}}),
                    "tasks/update" | "tasks/cancel" => ok(json!({"resultType": "complete"})),
                    _ => return StatusCode::ACCEPTED.into_response(),
                };
                axum::Json(reply).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

/// Whether a request declared the extension: in `initialize`'s capabilities, or in the per-request client
/// capabilities `_meta` when the protocol has no handshake.
fn declares_tasks(body: &Value) -> bool {
    body["params"]["capabilities"]["extensions"]
        .get(TASKS)
        .is_some()
        || body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
            .get(TASKS)
            .is_some()
}

#[tokio::test]
async fn a_long_running_call_is_a_task_polled_answered_by_a_human_and_cancelled() {
    let log = Log::default();
    let mcp = HttpMcp::new(server(log.clone(), true).await, creds());

    // C1: the session declares the extension.
    let starter = StaffActor {
        tenant: A,
        staff_user: "staff-1".into(),
    };
    let started = mcp
        .start_task(&starter, "long", json!({"x": 1}))
        .await
        .unwrap();
    // C9: the tools/call names the person the task starts for (fr refuses a project without one).
    assert_eq!(
        log.of("tools/call")[0]["params"]["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("care-bus:{A}:staff-1")})
    );
    assert!(
        log.of("initialize")
            .iter()
            .chain(log.of("tools/call").iter())
            .any(declares_tasks),
        "the tasks extension is declared: {:?}",
        log.bodies.lock().unwrap()
    );

    // C2: a task handle.
    let TaskStart::Task(h) = started else {
        panic!("expected a task, got {started:?}")
    };
    assert_eq!(h.task_id, "t-1");
    assert_eq!(h.status, "working");
    assert_eq!(h.poll_interval_ms, Some(2000));
    assert_eq!(h.ttl_ms, Some(86_400_000));

    // C3: polling walks the states.
    let s = mcp.get_task(A, "t-1").await.unwrap();
    assert_eq!(s.status, "working");
    assert!(outcome_of(&s).is_none());
    let s = mcp.get_task(A, "t-1").await.unwrap();
    assert_eq!(s.status, "input_required");
    let q = input_questions(&s.detail);
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].key, "t-1:00000000000000aa");

    // C4: the answer carries the deciding human in `_meta`.
    let actor = StaffActor {
        tenant: A,
        staff_user: "staff-7".into(),
    };
    mcp.answer_task(
        &actor,
        "t-1",
        &q[0].key,
        json!({"action": "accept", "content": {"decision": "approved"}}),
    )
    .await
    .unwrap();
    let upd = log.of("tasks/update");
    assert_eq!(upd.len(), 1);
    assert_eq!(upd[0]["params"]["taskId"], "t-1");
    assert_eq!(
        upd[0]["params"]["inputResponses"]["t-1:00000000000000aa"],
        json!({"action": "accept", "content": {"decision": "approved"}})
    );
    assert_eq!(
        upd[0]["params"]["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("care-bus:{A}:staff-7")})
    );

    // C5: cancel is an ack.
    mcp.cancel_task(A, "t-1").await.unwrap();
    assert_eq!(log.of("tasks/cancel")[0]["params"]["taskId"], "t-1");

    // C6: completed → the run envelope, as call_tool would give it.
    let s = mcp.get_task(A, "t-1").await.unwrap();
    assert_eq!(
        outcome_of(&s),
        Some(TaskOutcome::Completed(Ok(
            json!({"run_id": "r-1", "status": "done", "result": {"ok": 1}, "error": null})
        )))
    );

    // -32602: the task is lost, on every task method.
    assert!(matches!(
        mcp.get_task(A, "gone").await,
        Err(TaskError::Lost(_))
    ));
    assert!(matches!(
        mcp.cancel_task(A, "gone").await,
        Err(TaskError::Lost(_))
    ));
    assert!(matches!(
        mcp.answer_task(&actor, "gone", "k", json!({"action": "decline"}))
            .await,
        Err(TaskError::Lost(_))
    ));

    // A short call through start_task completes at once.
    let done = mcp.start_task(&starter, "quick", json!({})).await.unwrap();
    assert_eq!(
        done,
        TaskStart::Completed(
            json!({"run_id": "r-1", "status": "queued", "result": null, "error": null})
        )
    );
}

/// C7: a server that does not advertise the extension gets today's plain call and its queued envelope.
#[tokio::test]
async fn without_the_extension_a_long_running_call_is_a_plain_call() {
    let log = Log::default();
    let mcp = HttpMcp::new(server(log.clone(), false).await, creds());
    let starter = StaffActor {
        tenant: A,
        staff_user: "staff-1".into(),
    };
    let started = mcp.start_task(&starter, "long", json!({})).await.unwrap();
    assert_eq!(
        started,
        TaskStart::Completed(
            json!({"run_id": "r-1", "status": "queued", "result": null, "error": null})
        )
    );
    assert_eq!(log.of("tools/call").len(), 1);
}
