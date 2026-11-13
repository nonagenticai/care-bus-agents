//! MCP Tasks (SEP-2663, extension `io.modelcontextprotocol/tasks`), client side.
//!
//! nonagenticai_mcp_server answers a `tools/call` for long-running work (an n8n workflow, an fr project) with a durable
//! task instead of a result, but only to a client that declares the extension ([`client_config`], sent on every
//! session). This module starts such calls, polls them, relays a HUMAN's answer to their questions and cancels them.
//! The durable bookkeeping (table `mcp_tasks`, the poller, the staff routes) lives in carebus-api; this file depends on
//! the rest of care-bus only through [`HttpMcp`] and [`ToolError`], so it can be lifted into the agent platform.
//!
//! Agents never answer task questions: [`HttpMcp::answer_task`] is called only from the human-authenticated answer
//! route (the [`StaffActor`] is the answering person), and no agent tool set offers it. A task is STARTED on behalf of
//! the person who started the research run ([`HttpMcp::start_task`]'s actor), which is who fr records as its owner.

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::model::ProtocolVersion;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams, ClientCapabilities,
    ClientConfig, ExtensionCapabilities, GetTaskParams, Implementation, JsonObject,
    RequestMetaObject, TASKS_EXTENSION_ID, UpdateTaskParams,
};
use rmcp::service::{ClientLifecycleMode, ServiceError};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::mcp_http::{HttpMcp, tool_error_from_text, tool_result_to_value};
use crate::tools::ToolError;

/// `_meta` key naming the human who answers a task's question (resolved by fr through `user_channel_prefs`).
pub const ACTOR_META: &str = "ai.nonagentic/actor";
/// JSON-RPC error of an unknown, expired or other-tenant `taskId`: the task is gone for good.
pub const UNKNOWN_TASK: i32 = -32602;
/// Poll interval bounds and the default when the server names none.
pub const POLL_FLOOR: Duration = Duration::from_secs(1);
pub const POLL_CAP: Duration = Duration::from_secs(60);
pub const POLL_DEFAULT: Duration = Duration::from_secs(5);
/// Env var listing the MCP tools that are started as tasks (comma-separated). Configuration decides, never the model.
pub const TASK_TOOLS_VAR: &str = "MCP_TASK_TOOLS";

/// The client config every care-bus MCP session is opened with: the tasks extension declared.
pub fn client_config() -> ClientConfig {
    let mut capabilities = ClientCapabilities::default();
    capabilities.extensions = Some(ExtensionCapabilities::from([(
        TASKS_EXTENSION_ID.to_string(),
        JsonObject::new(),
    )]));
    ClientConfig::new(
        capabilities,
        Implementation::new("care-bus", env!("CARGO_PKG_VERSION")),
    )
}

/// How every care-bus MCP session starts: `server/discover` at `2026-07-28` first (stateless, per-request `_meta`,
/// which is where nonagenticai_mcp_server reads the tasks declaration and advertises the extension), falling back to
/// `initialize` for a server that predates it. An `initialize` at `2026-07-28` would be answered with the older
/// `2025-11-25`, without the extension and without per-request client capabilities: no tasks.
pub fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: None,
    }
}

/// The long-running tools of `MCP_TASK_TOOLS`: trimmed, empty entries dropped.
pub fn task_tools(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// The server's `pollIntervalMs`, held to [`POLL_FLOOR`]..[`POLL_CAP`].
pub fn poll_delay(poll_interval_ms: Option<u64>) -> Duration {
    poll_interval_ms
        .map(Duration::from_millis)
        .unwrap_or(POLL_DEFAULT)
        .clamp(POLL_FLOOR, POLL_CAP)
}

/// What a task-capable call returned.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskStart {
    /// The call finished at once (or the server does not offer tasks): the same value `call_tool` returns.
    Completed(Value),
    /// The server materialized a task; poll it with [`HttpMcp::get_task`].
    Task(TaskHandle),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskHandle {
    pub task_id: String,
    pub status: String,
    pub status_message: Option<String>,
    pub poll_interval_ms: Option<u64>,
    pub ttl_ms: Option<u64>,
}

/// Why a task operation did not get an answer.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskError {
    /// `-32602`: the server no longer knows the task (expired, unknown, another tenant's). Stop polling.
    Lost(String),
    /// Anything else, classified as for tool calls (retryable transport failures, permanent protocol errors).
    Tool(ToolError),
}

impl TaskError {
    pub fn message(&self) -> &str {
        match self {
            TaskError::Lost(m) => m,
            TaskError::Tool(e) => &e.message,
        }
    }
}

/// One `tasks/get` answer: the status, its text, the next poll interval, and the whole wire object (`inputRequests`,
/// `result` or `error` inlined) for storage.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskSnapshot {
    pub status: String,
    pub status_message: Option<String>,
    pub poll_interval_ms: Option<u64>,
    pub detail: Value,
}

impl TaskSnapshot {
    /// From the `DetailedTask` wire object (`{taskId, status, statusMessage?, pollIntervalMs?, ...}`).
    pub fn from_detail(detail: Value) -> Self {
        Self {
            status: detail["status"].as_str().unwrap_or("working").to_string(),
            status_message: detail["statusMessage"].as_str().map(String::from),
            poll_interval_ms: detail["pollIntervalMs"].as_u64(),
            detail,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
}

/// How a finished task ends up, as a tool result would.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskOutcome {
    /// `completed`: the tool's own result, mapped like `call_tool` (an `isError` result is a [`ToolError`]).
    Completed(Result<Value, ToolError>),
    /// `failed`: the JSON-RPC error of the task. Permanent.
    Failed(ToolError),
    /// `cancelled`: stopped at a staff user's request.
    Cancelled,
    /// The server lost the task (`-32602`). Permanent.
    Lost(ToolError),
}

impl TaskOutcome {
    /// The stored form (`mcp_tasks.result`): `{"outcome", "ok", "result" | "error", "retryable"}`.
    pub fn to_json(&self) -> Value {
        let err = |outcome: &str, e: &ToolError| json!({"outcome": outcome, "ok": false, "error": e.message, "retryable": e.retryable});
        match self {
            TaskOutcome::Completed(Ok(v)) => {
                json!({"outcome": "completed", "ok": true, "result": v})
            }
            TaskOutcome::Completed(Err(e)) => err("completed", e),
            TaskOutcome::Failed(e) => err("failed", e),
            TaskOutcome::Lost(e) => err("lost", e),
            TaskOutcome::Cancelled => json!({"outcome": "cancelled", "ok": false}),
        }
    }

    /// The `mcp_tasks.status` this outcome ends the row in.
    pub fn status(&self) -> &'static str {
        match self {
            TaskOutcome::Completed(_) => "completed",
            TaskOutcome::Failed(_) => "failed",
            TaskOutcome::Cancelled => "cancelled",
            TaskOutcome::Lost(_) => "lost",
        }
    }
}

/// The outcome of a terminal snapshot; `None` while it is still `working` / `input_required`.
pub fn outcome_of(s: &TaskSnapshot) -> Option<TaskOutcome> {
    match s.status.as_str() {
        "completed" => Some(TaskOutcome::Completed(completed_value(&s.detail["result"]))),
        "failed" => {
            let e = &s.detail["error"];
            let code = e["code"]
                .as_i64()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            let message = e["message"].as_str().unwrap_or("the task failed");
            Some(TaskOutcome::Failed(ToolError::fatal(format!(
                "task failed{code}: {message}"
            ))))
        }
        "cancelled" => Some(TaskOutcome::Cancelled),
        _ => None,
    }
}

/// A `completed` task's `result` (a `CallToolResult` dump) as a tool result.
fn completed_value(result: &Value) -> Result<Value, ToolError> {
    match serde_json::from_value::<CallToolResult>(result.clone()) {
        Ok(r) => tool_result_to_value(r),
        // Not a CallToolResult: report its text as the tool would, permanent.
        Err(_) => Err(tool_error_from_text(&format!(
            "[permanent] task result is not a tool result: {}",
            truncate(&result.to_string(), 500)
        ))),
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// One open question of an `input_required` task: its key, the text shown to staff, and the schema of the answer.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct InputQuestion {
    pub key: String,
    pub message: String,
    pub requested_schema: Value,
}

/// The `elicitation/create` entries of `inputRequests`; anything else (sampling, roots) is not shown to staff.
pub fn input_questions(detail: &Value) -> Vec<InputQuestion> {
    let Some(map) = detail["inputRequests"].as_object() else {
        return vec![];
    };
    map.iter()
        .filter(|(_, r)| r["method"] == "elicitation/create")
        .map(|(key, r)| InputQuestion {
            key: key.clone(),
            message: r["params"]["message"].as_str().unwrap_or("").to_string(),
            requested_schema: r["params"]["requestedSchema"].clone(),
        })
        .collect()
}

/// The human answering a task question: tenant + their care-bus subject. Built only by a human-authenticated route.
#[derive(Clone, Debug, PartialEq)]
pub struct StaffActor {
    pub tenant: Uuid,
    pub staff_user: String,
}

impl StaffActor {
    /// `care-bus:<tenant_id>:<staff_user_id>`, mapped in fr to an fr user of the tenant.
    pub fn address(&self) -> String {
        format!("care-bus:{}:{}", self.tenant, self.staff_user)
    }

    pub fn meta(&self) -> Value {
        json!({"channel": "mcp", "address": self.address()})
    }
}

/// Check a staff answer against the question's `requestedSchema` and build the `ElicitResult` to send.
/// `accept` needs `content` that meets the schema (required keys present and non-empty, enum values, strings only,
/// no keys the schema does not name) and a `reason` when the decision is `rejected`; `decline` / `cancel` carry no
/// content. The error names what is wrong, for the UI.
pub fn validate_answer(
    schema: &Value,
    action: &str,
    content: Option<&Value>,
) -> Result<Value, String> {
    match action {
        "decline" | "cancel" => return Ok(json!({"action": action})),
        "accept" => {}
        other => {
            return Err(format!(
                "unknown action {other:?}: accept, decline or cancel"
            ));
        }
    }
    let Some(content) = content.and_then(Value::as_object) else {
        return Err("an accepted answer needs content".into());
    };
    let props = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut clean = Map::new();
    for (k, v) in content {
        let Some(p) = props.get(k) else {
            return Err(format!("{k} is not a field of this question"));
        };
        let Some(s) = v.as_str() else {
            if v.is_null() {
                continue;
            }
            return Err(format!("{k} must be text"));
        };
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if let Some(allowed) = p["enum"].as_array()
            && !allowed.iter().any(|a| a.as_str() == Some(s))
        {
            return Err(format!(
                "{k} must be one of {}",
                Value::Array(allowed.clone())
            ));
        }
        if p["type"].as_str().is_some_and(|t| t != "string") {
            return Err(format!("{k} has an unsupported type"));
        }
        clean.insert(k.clone(), Value::String(s.to_string()));
    }
    for r in schema["required"].as_array().into_iter().flatten() {
        if let Some(r) = r.as_str()
            && !clean.contains_key(r)
        {
            return Err(format!("{r} is required"));
        }
    }
    if clean.get("decision").and_then(Value::as_str) == Some("rejected")
        && !clean.contains_key("reason")
    {
        return Err("reason is required when rejecting".into());
    }
    Ok(json!({"action": "accept", "content": clean}))
}

// `tasks/update` and `tasks/cancel` answer with an ack, `{"resultType":"complete","_meta":{...}}`. rmcp 3.5 classifies
// an ack that carries `_meta` as another result kind and reports `UnexpectedResponse` (measured against
// nonagenticai_mcp_server 21e19e8). A JSON-RPC *result* is the ack either way: only a JSON-RPC error is a failure.

fn task_error(e: &ServiceError) -> Option<TaskError> {
    match e {
        ServiceError::McpError(d) if d.code.0 == UNKNOWN_TASK => Some(TaskError::Lost(format!(
            "task unknown to the server: {}",
            d.message
        ))),
        _ => None,
    }
}

impl HttpMcp {
    /// C2: a `tools/call` that may come back as a task. A server without the extension, or an immediate result, gives
    /// [`TaskStart::Completed`] (today's envelope, e.g. `{run_id, status:"queued"}`). An MRTR `input_required` at
    /// call time is not something this server sends: permanent error. `actor` is the person the task starts for (C9,
    /// sent as `_meta["ai.nonagentic/actor"]`); the plain-call fallback sends none, since no task is started there.
    pub async fn start_task(
        &self,
        actor: &StaffActor,
        name: &str,
        arguments: Value,
    ) -> Result<TaskStart, ToolError> {
        let tenant = actor.tenant;
        let (generation, svc, auth, tasks) = self.session_for_tasks(tenant).await?;
        if !tasks {
            return self
                .call_tool_for(tenant, name, arguments)
                .await
                .map(TaskStart::Completed);
        }
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Value::Object(m) = arguments {
            params = params.with_arguments(m);
        }
        // C9: the person on whose behalf the task starts. The server stores it on the run; an fr project run without
        // one fails with `actor_required`.
        params.meta = Some(actor_meta(actor));
        match svc.peer().call_tool_once(params).await {
            Ok(CallToolResponse::Complete(r)) => tool_result_to_value(r).map(TaskStart::Completed),
            Ok(CallToolResponse::Task(t)) => Ok(TaskStart::Task(TaskHandle {
                task_id: t.task.task_id,
                status: status_str(&t.task.status),
                status_message: t.task.status_message,
                poll_interval_ms: t.task.poll_interval_ms,
                ttl_ms: t.task.ttl_ms,
            })),
            Ok(CallToolResponse::InputRequired(_)) => Err(ToolError::fatal(format!(
                "mcp call {name}: the server asked for input at call time, which care-bus does not answer"
            ))),
            Ok(_) => Err(ToolError::fatal(format!(
                "mcp call {name}: unexpected response"
            ))),
            Err(e) => Err(self.fail(tenant, generation, &auth, &e, name).await),
        }
    }

    /// C3: `tasks/get`.
    pub async fn get_task(&self, tenant: Uuid, task_id: &str) -> Result<TaskSnapshot, TaskError> {
        let (generation, svc, auth, _) = self
            .session_for_tasks(tenant)
            .await
            .map_err(TaskError::Tool)?;
        match svc.peer().get_task(GetTaskParams::new(task_id)).await {
            Ok(r) => Ok(TaskSnapshot::from_detail(
                serde_json::to_value(&r.task).unwrap_or(Value::Null),
            )),
            Err(e) => Err(match task_error(&e) {
                Some(t) => t,
                None => {
                    TaskError::Tool(self.fail(tenant, generation, &auth, &e, "tasks/get").await)
                }
            }),
        }
    }

    /// C4: `tasks/update` with one human answer (an `ElicitResult` from [`validate_answer`]) and the actor `_meta`.
    /// The ack is eventually consistent: keep polling.
    pub async fn answer_task(
        &self,
        actor: &StaffActor,
        task_id: &str,
        key: &str,
        response: Value,
    ) -> Result<(), TaskError> {
        let (generation, svc, auth, _) = self
            .session_for_tasks(actor.tenant)
            .await
            .map_err(TaskError::Tool)?;
        let mut params =
            UpdateTaskParams::new(task_id, BTreeMap::from([(key.to_string(), response)]));
        params.meta = Some(actor_meta(actor));
        match svc.peer().update_task(params).await {
            Ok(()) | Err(ServiceError::UnexpectedResponse) => Ok(()),
            Err(e) => Err(match task_error(&e) {
                Some(t) => t,
                None => TaskError::Tool(
                    self.fail(actor.tenant, generation, &auth, &e, "tasks/update")
                        .await,
                ),
            }),
        }
    }

    /// C5: `tasks/cancel`, a cooperative ack: the task may still complete. Keep polling.
    pub async fn cancel_task(&self, tenant: Uuid, task_id: &str) -> Result<(), TaskError> {
        let (generation, svc, auth, _) = self
            .session_for_tasks(tenant)
            .await
            .map_err(TaskError::Tool)?;
        match svc.peer().cancel_task(CancelTaskParams::new(task_id)).await {
            Ok(()) | Err(ServiceError::UnexpectedResponse) => Ok(()),
            Err(e) => Err(match task_error(&e) {
                Some(t) => t,
                None => TaskError::Tool(
                    self.fail(tenant, generation, &auth, &e, "tasks/cancel")
                        .await,
                ),
            }),
        }
    }
}

/// The request `_meta` naming `actor` (`ai.nonagentic/actor`).
fn actor_meta(actor: &StaffActor) -> RequestMetaObject {
    let mut meta = RequestMetaObject::new();
    meta.insert(ACTOR_META.to_string(), actor.meta());
    meta
}

fn status_str(s: &rmcp::model::TaskStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "working".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(v: Value) -> TaskSnapshot {
        TaskSnapshot::from_detail(v)
    }

    #[test]
    fn the_client_config_declares_the_tasks_extension() {
        let v = serde_json::to_value(client_config()).unwrap();
        assert_eq!(
            v["capabilities"]["extensions"][TASKS_EXTENSION_ID],
            json!({})
        );
        assert_eq!(v["clientInfo"]["name"], "care-bus");
    }

    #[test]
    fn task_tools_come_from_configuration() {
        assert_eq!(
            task_tools(Some(" n8n_run , ,fr_project_start")),
            ["n8n_run", "fr_project_start"]
        );
        assert!(task_tools(None).is_empty());
    }

    #[test]
    fn poll_interval_is_floored_and_capped() {
        assert_eq!(poll_delay(Some(10)), POLL_FLOOR);
        assert_eq!(poll_delay(Some(600_000)), POLL_CAP);
        assert_eq!(poll_delay(Some(2_500)), Duration::from_millis(2_500));
        assert_eq!(poll_delay(None), POLL_DEFAULT);
    }

    #[test]
    fn each_terminal_status_maps_to_its_outcome() {
        let ok = snap(json!({"taskId": "t", "status": "completed", "result": {
            "content": [], "isError": false,
            "structuredContent": {"run_id": "r", "status": "done", "result": {"x": 1}, "error": null}}}));
        assert_eq!(
            outcome_of(&ok),
            Some(TaskOutcome::Completed(Ok(
                json!({"run_id": "r", "status": "done", "result": {"x": 1}, "error": null})
            )))
        );
        let text = snap(json!({"status": "completed", "result": {
            "content": [{"type": "text", "text": "{\"a\":2}"}], "isError": false}}));
        assert_eq!(
            outcome_of(&text),
            Some(TaskOutcome::Completed(Ok(json!({"a": 2}))))
        );
        let tool_err = snap(json!({"status": "completed", "result": {
            "content": [{"type": "text", "text": "[retryable] n8n timed out"}], "isError": true}}));
        let Some(TaskOutcome::Completed(Err(e))) = outcome_of(&tool_err) else {
            panic!("isError is a tool error")
        };
        assert!(e.retryable);
        assert_eq!(e.message, "n8n timed out");
        let failed =
            snap(json!({"status": "failed", "error": {"code": -32603, "message": "boom"}}));
        let Some(TaskOutcome::Failed(e)) = outcome_of(&failed) else {
            panic!()
        };
        assert!(!e.retryable);
        assert!(e.message.contains("-32603") && e.message.contains("boom"));
        assert_eq!(
            outcome_of(&snap(json!({"status": "cancelled"}))),
            Some(TaskOutcome::Cancelled)
        );
        assert_eq!(outcome_of(&snap(json!({"status": "working"}))), None);
        assert_eq!(outcome_of(&snap(json!({"status": "input_required"}))), None);
        assert_eq!(TaskOutcome::Cancelled.to_json()["outcome"], "cancelled");
        assert_eq!(TaskOutcome::Lost(ToolError::fatal("gone")).status(), "lost");
    }

    fn approval_schema() -> Value {
        json!({"type": "object", "required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"]},
            "reason": {"type": "string"}, "comment": {"type": "string"}}})
    }

    #[test]
    fn input_questions_are_the_elicitations_only() {
        let d = json!({"status": "input_required", "inputRequests": {
            "t:00aa": {"method": "elicitation/create", "params": {"message": "Ship it?", "requestedSchema": approval_schema()}},
            "t:00bb": {"method": "sampling/createMessage", "params": {}}}});
        let q = input_questions(&d);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].key, "t:00aa");
        assert_eq!(q[0].message, "Ship it?");
        assert_eq!(q[0].requested_schema, approval_schema());
        assert!(input_questions(&json!({"status": "working"})).is_empty());
    }

    #[test]
    fn a_rejection_needs_a_reason_and_the_schema_is_enforced() {
        let s = approval_schema();
        assert_eq!(
            validate_answer(&s, "accept", Some(&json!({"decision": "rejected"}))).unwrap_err(),
            "reason is required when rejecting"
        );
        assert_eq!(
            validate_answer(
                &s,
                "accept",
                Some(&json!({"decision": "rejected", "reason": " too risky "}))
            )
            .unwrap(),
            json!({"action": "accept", "content": {"decision": "rejected", "reason": "too risky"}})
        );
        assert!(validate_answer(&s, "accept", Some(&json!({"decision": "maybe"}))).is_err());
        assert!(
            validate_answer(&s, "accept", Some(&json!({"reason": "x"}))).is_err(),
            "decision required"
        );
        assert!(
            validate_answer(
                &s,
                "accept",
                Some(&json!({"decision": "approved", "extra": "x"}))
            )
            .is_err()
        );
        assert!(validate_answer(&s, "accept", None).is_err());
        assert_eq!(
            validate_answer(&s, "decline", None).unwrap(),
            json!({"action": "decline"})
        );
        assert!(validate_answer(&s, "approve", None).is_err());
    }

    #[test]
    fn the_actor_is_the_care_bus_staff_address() {
        let a = StaffActor {
            tenant: Uuid::from_u128(1),
            staff_user: "u-1".into(),
        };
        assert_eq!(
            a.address(),
            "care-bus:00000000-0000-0000-0000-000000000001:u-1"
        );
        assert_eq!(a.meta(), json!({"channel": "mcp", "address": a.address()}));
    }
}
