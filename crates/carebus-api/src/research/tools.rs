//! The tools a run may call ([`ToolSet`]) and the source re-resolution the verifier uses ([`Resolver`]).
//!
//! [`LiveTools`] serves both: the native `strata_search` (strata_rag `POST /api/query`, only the run's stores) and
//! the MCP tools of nonagenticai_mcp_server (`tools/list` filtered by the profile's allow-list). `submit_report`
//! is the runner's own and never reaches a tool set.
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carebus::mcp_http::{HttpMcp, McpTool};
use carebus::mcp_tasks::{StaffActor, TaskStart};
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::RunScope;
use super::{Profile, SourceKind};
use crate::upstream::{self, Auth};

pub const SUBMIT_REPORT: &str = "submit_report";
pub const STRATA_SEARCH: &str = "strata_search";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Ok,
    Error,
    Denied,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Denied => "denied",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    pub status: StepStatus,
    pub text: String,
    /// Set when the call started a long-running MCP task: the run parks as `waiting` until it finishes.
    pub task_id: Option<String>,
}

impl ToolOutcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            status: StepStatus::Ok,
            text: text.into(),
            task_id: None,
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            status: StepStatus::Error,
            text: text.into(),
            task_id: None,
        }
    }
    pub fn denied(text: impl Into<String>) -> Self {
        Self {
            status: StepStatus::Denied,
            text: text.into(),
            task_id: None,
        }
    }
}

/// What a tool call may act on.
pub struct ToolCtx<'a> {
    pub tenant: Uuid,
    pub profile: Profile,
    /// strata store ids the run may search (`research_runs.store_ids`).
    pub store_ids: &'a [String],
    /// The person who started the run (`research_runs.created_by` as a uuid subject), the `X-Actor-Id` of its
    /// strata_rag calls; `None` = care-bus' service account ([`StrataCfg::service_actor`]).
    pub actor: Option<Uuid>,
    /// The run making the call: a long-running MCP tool (crate::mcp_tasks) records its task under it. `None` outside a
    /// run (such a tool is then refused).
    pub run: Option<RunScope>,
    /// `research_runs.created_by`, kept on a task the run starts.
    pub created_by: &'a str,
}

pub trait ToolSet: Send + Sync + 'static {
    /// OpenAI `tools` entries offered to the model (without `submit_report`).
    fn specs(&self, ctx: &ToolCtx<'_>) -> impl Future<Output = Vec<Value>> + Send;
    /// One call; only names from `specs` reach it.
    fn call(
        &self,
        ctx: &ToolCtx<'_>,
        name: &str,
        args: &Value,
    ) -> impl Future<Output = ToolOutcome> + Send;
    /// strata_rag's `GET /api/stores` body for `tenant`, for the wall re-check at claim (crate::walls); `Ok(None)`
    /// when this tool set cannot search that tenant's stores at all (no `strata_search` is offered), `Err` when the
    /// tree cannot be read (the run is not continued).
    fn store_tree(
        &self,
        _tenant: Uuid,
    ) -> impl Future<Output = Result<Option<Value>, String>> + Send {
        async { Ok(None) }
    }
}

/// What the register of a cited source says.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    /// The source exists; `in_force` when the register says.
    Found { in_force: Option<bool> },
    /// The register does not know it.
    Missing(String),
    /// The register could not be asked (MCP down, tool absent).
    Unavailable(String),
}

pub trait Resolver: Send + Sync + 'static {
    /// Only called for `eli`, `saos` and `eurlex`; asked as the run's `tenant` (its own MCP credentials).
    fn resolve(
        &self,
        tenant: Uuid,
        kind: SourceKind,
        locator: &str,
    ) -> impl Future<Output = Resolution> + Send;
}

/// One OpenAI function-tool entry.
pub fn function_spec(name: &str, description: &str, parameters: Value) -> Value {
    json!({"type": "function", "function": {"name": name, "description": description, "parameters": parameters}})
}

pub fn submit_report_spec() -> Value {
    function_spec(
        SUBMIT_REPORT,
        "Submit the final report. Ends the research. The report is built from `claims` only; every claim needs at \
         least one citation whose quote is copied verbatim from the tool result of step `step_ordinal`, the step of \
         that source's own call.",
        json!({
            "type": "object",
            "properties": {
                "summary_markdown": {"type": "string",
                    "description": "Optional scratch notes; never shown in the report."},
                "claims": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                            "citations": {
                                "type": "array",
                                "minItems": 1,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "source_kind": {"type": "string",
                                            "enum": ["strata", "web", "eli", "saos", "eurlex", "krs",
                                                     "literature", "stats", "other"]},
                                        "locator": {"type": "string"},
                                        "title": {"type": "string"},
                                        "quote": {"type": "string"},
                                        "step_ordinal": {"type": "integer"}
                                    },
                                    "required": ["source_kind", "locator", "quote", "step_ordinal"]
                                }
                            }
                        },
                        "required": ["text", "citations"]
                    }
                }
            },
            "required": ["claims"]
        }),
    )
}

pub fn strata_search_spec() -> Value {
    function_spec(
        STRATA_SEARCH,
        "Search the organisation's document stores. Returns chunks with their chunk_id (cite strata sources by \
         chunk_id).",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "store_id": {"type": "string", "description": "One of the run's stores; omit to search all of them."},
                "top_k": {"type": "integer", "minimum": 1, "maximum": 20}
            },
            "required": ["query"]
        }),
    )
}

/// A tool result as text: a JSON string as is, anything else as pretty JSON.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// strata_rag (`STRATA_RAG_URL`). Every call acts for the run's tenant with THAT tenant's strata-rag licence token,
/// the one its MCP credentials carry (carebus-api `POST /api/internal/mcp-credentials`, carebus::mcp_http): there is
/// no shared or pinned strata token. Without MCP there is no token, so no `strata_search`.
#[derive(Clone)]
pub struct StrataCfg {
    pub url: String,
    /// `SERVICE_ACTOR_ID`: the `X-Actor-Id` of a strata_rag call no person is behind.
    pub service_actor: Option<Uuid>,
}

/// One strata_rag attempt: answered, refused with 401 (the token is renewed and the call made once more), or failed.
enum StrataAttempt {
    Ok(Value),
    Unauthorized,
    Failed(String),
}

const MCP_LIST_TTL: Duration = Duration::from_secs(300);

pub struct LiveTools {
    pub mcp: Option<Arc<HttpMcp>>,
    pub strata: Option<StrataCfg>,
    pub http: reqwest::Client,
    /// Long-running MCP tools (`MCP_TASK_TOOLS`) and the database their tasks are kept in; `None` = none.
    pub tasks: Option<TaskTools>,
    listed: Mutex<Option<(Instant, Vec<McpTool>)>>,
    warned: Mutex<HashSet<String>>,
}

/// Long-running MCP tools and where their tasks are kept.
pub struct TaskTools {
    pub db: PgPool,
    pub tools: Vec<String>,
    /// The binding caller (crate::mcp_binding): catalog tools, which these long-running ones are, answer only it.
    pub mcp: Arc<HttpMcp>,
}

impl LiveTools {
    pub fn new(
        mcp: Option<Arc<HttpMcp>>,
        strata: Option<StrataCfg>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            mcp,
            strata,
            http,
            listed: Mutex::new(None),
            warned: Mutex::new(HashSet::new()),
            tasks: None,
        }
    }

    /// Start the configured long-running tools as MCP tasks kept in `db` (crate::mcp_tasks).
    pub fn with_tasks(mut self, db: PgPool, tools: Vec<String>, mcp: Arc<HttpMcp>) -> Self {
        if !tools.is_empty() {
            self.tasks = Some(TaskTools { db, tools, mcp });
        }
        self
    }

    /// A long-running tool: started as a task and recorded; the model gets the handle, never the task's questions
    /// (staff answer those in the Tasks inbox).
    async fn start_task(
        &self,
        t: &TaskTools,
        ctx: &ToolCtx<'_>,
        name: &str,
        args: &Value,
    ) -> ToolOutcome {
        let Some(run) = ctx.run else {
            return ToolOutcome::denied(format!("{name} is long-running and needs a research run"));
        };
        // The task starts on behalf of the person who started the run (C9): fr refuses a project without an actor.
        if ctx.created_by.trim().is_empty() {
            return ToolOutcome::denied(format!("{name} needs the person who started this run"));
        }
        let actor = StaffActor {
            tenant: ctx.tenant,
            staff_user: ctx.created_by.to_string(),
        };
        match t.mcp.start_task(&actor, name, args.clone()).await {
            Ok(TaskStart::Completed(v)) => ToolOutcome::ok(value_text(&v)),
            Ok(TaskStart::Task(h)) => {
                let origin = crate::mcp_tasks::Origin {
                    tenant: run.tenant,
                    job: run.job,
                    run: Some(run.run),
                    case: None,
                    created_by: ctx.created_by,
                };
                if let Err(e) = crate::mcp_tasks::record_started(&t.db, &origin, name, &h).await {
                    tracing::error!(task_id = %h.task_id, error = %e, "research: started MCP task not recorded");
                    return ToolOutcome::error(format!(
                        "{name} started task {} but it could not be recorded",
                        h.task_id
                    ));
                }
                // The run parks until the task finishes; the model then sees the task's result as this call's result.
                let mut o = ToolOutcome::ok(format!(
                    "waiting for MCP task {}: {name} is long-running; this run resumes with its result",
                    h.task_id
                ));
                o.task_id = Some(h.task_id);
                o
            }
            Err(e) => ToolOutcome::error(e.message),
        }
    }

    /// strata_rag and MCP (where the tenant's strata token comes from) are configured, and the run has stores.
    fn strata_for(&self, ctx: &ToolCtx<'_>) -> Option<&StrataCfg> {
        self.strata
            .as_ref()
            .filter(|_| self.mcp.is_some() && !ctx.store_ids.is_empty())
    }

    /// One strata_rag call as `tenant`, with `token`.
    #[allow(clippy::too_many_arguments)]
    async fn strata_once(
        &self,
        s: &StrataCfg,
        token: &str,
        tenant: Uuid,
        actor: Option<Uuid>,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> StrataAttempt {
        let name = crate::strata::NAME;
        let url = format!("{}{path}", s.url.trim_end_matches('/'));
        // `X-App-Id: care-bus` and `X-Actor-Id` (the run's creator, else the service account): strata relays both
        // to its metering and to inference-router.
        let mut req = Auth {
            token: Some(token),
            tenant: Some(tenant),
            app: Some(carebus::identity::APP_ID),
            actor: carebus::identity::pick_actor(actor, s.service_actor),
        }
        .apply(self.http.request(method, url));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let refused = crate::error::ApiError::Upstream(name).to_string();
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(upstream = name, error = %e, "upstream unreachable");
                return StrataAttempt::Failed(refused);
            }
        };
        let raw = resp.status();
        if raw == reqwest::StatusCode::UNAUTHORIZED {
            return StrataAttempt::Unauthorized;
        }
        if raw.is_server_error() || raw == reqwest::StatusCode::FORBIDDEN {
            tracing::warn!(
                upstream = name,
                status = raw.as_u16(),
                "upstream refused or failed"
            );
            return StrataAttempt::Failed(refused);
        }
        let body: Option<Value> = resp.json().await.ok();
        match (raw.is_success(), body) {
            (true, Some(v)) => StrataAttempt::Ok(v),
            (true, None) => StrataAttempt::Failed(refused),
            (false, body) => match axum::http::StatusCode::from_u16(raw.as_u16()) {
                Ok(status) => StrataAttempt::Failed(upstream::message(
                    name,
                    status,
                    &body.unwrap_or(Value::Null),
                )),
                Err(_) => StrataAttempt::Failed(refused),
            },
        }
    }

    /// A strata_rag call as `tenant` with its own strata-rag token (from its MCP credentials); on a 401 the
    /// credentials are renewed and the call made once more.
    async fn strata_call(
        &self,
        s: &StrataCfg,
        tenant: Uuid,
        actor: Option<Uuid>,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let Some(mcp) = &self.mcp else {
            return Err("strata_search needs MCP (the tenant's strata token comes with it)".into());
        };
        let creds = mcp
            .credentials(tenant)
            .await
            .map_err(|e| format!("the tenant's strata credentials are unavailable: {e:#}"))?;
        match self
            .strata_once(
                s,
                creds.strata_token(),
                tenant,
                actor,
                method.clone(),
                path,
                body.clone(),
            )
            .await
        {
            StrataAttempt::Ok(v) => return Ok(v),
            StrataAttempt::Failed(e) => return Err(e),
            StrataAttempt::Unauthorized => {
                tracing::warn!(%tenant, "research: strata refused the tenant's token (401); renewing once");
            }
        }
        let fresh = mcp
            .renew_credentials(tenant, &creds)
            .await
            .map_err(|e| format!("the tenant's strata credentials are unavailable: {e:#}"))?;
        match self
            .strata_once(s, fresh.strata_token(), tenant, actor, method, path, body)
            .await
        {
            StrataAttempt::Ok(v) => Ok(v),
            StrataAttempt::Failed(e) => Err(e),
            StrataAttempt::Unauthorized => {
                Err(crate::error::ApiError::Upstream(crate::strata::NAME).to_string())
            }
        }
    }

    /// The server's tool list, read as `tenant` and cached for every tenant: the MCP server offers every tenant-level
    /// care-bus licence the same set, and a call is still made as its own run's tenant.
    async fn mcp_tools(&self, tenant: Uuid) -> Vec<McpTool> {
        let cached: Option<Vec<McpTool>> = {
            let guard = self.listed.lock().expect("mcp list lock");
            guard
                .as_ref()
                .filter(|(at, _)| at.elapsed() < MCP_LIST_TTL)
                .map(|(_, t)| t.clone())
        };
        if let Some(t) = cached {
            return t;
        }
        let Some(mcp) = &self.mcp else {
            return vec![];
        };
        match mcp.list_tools(tenant).await {
            Ok(t) => {
                *self.listed.lock().expect("mcp list lock") = Some((Instant::now(), t.clone()));
                t
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "research: MCP tools/list failed; no MCP tools offered");
                vec![]
            }
        }
    }

    async fn strata_search(&self, ctx: &ToolCtx<'_>, args: &Value) -> ToolOutcome {
        let Some(s) = self.strata_for(ctx) else {
            return ToolOutcome::denied("strata_search is not available for this run");
        };
        let Some(query) = args["query"].as_str().filter(|q| !q.trim().is_empty()) else {
            return ToolOutcome::error("`query` is required");
        };
        let stores: Vec<&String> = match args["store_id"].as_str() {
            Some(id) => match ctx.store_ids.iter().find(|s| s.as_str() == id) {
                Some(s) => vec![s],
                None => return ToolOutcome::denied("store_id is not one of this run's stores"),
            },
            None => ctx.store_ids.iter().collect(),
        };
        let root_ids: Vec<Uuid> = stores.iter().filter_map(|s| s.parse().ok()).collect();
        if root_ids.is_empty() {
            return ToolOutcome::error("the run has no valid store ids");
        }
        let top_k = args["top_k"].as_u64().unwrap_or(8).clamp(1, 20);
        let body = json!({"query": query, "top_k": top_k, "root_ids": root_ids});
        match self
            .strata_call(
                s,
                ctx.tenant,
                ctx.actor,
                Method::POST,
                "/api/query",
                Some(body),
            )
            .await
        {
            Ok(body) => ToolOutcome::ok(format_strata_results(&body)),
            Err(e) => ToolOutcome::error(e),
        }
    }
}

/// strata_rag `/api/query` results as the tool result: JSON `{"results": [{chunk_id, document_id, title, source,
/// text}]}`, so the ids a citation may name are structured fields ([`strata_result_ids`]), never free text.
pub fn format_strata_results(body: &Value) -> String {
    let Some(results) = body["results"].as_array().filter(|a| !a.is_empty()) else {
        return "no results".into();
    };
    let out: Vec<Value> = results
        .iter()
        .map(|r| {
            let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
            json!({
                "chunk_id": s("chunk_id"),
                "document_id": s("document_id"),
                "title": s("title"),
                "source": s("source"),
                "text": s("text"),
            })
        })
        .collect();
    serde_json::to_string_pretty(&json!({"results": out})).unwrap_or_default()
}

/// The chunk and document ids of a stored `strata_search` result ([`format_strata_results`]); empty when it is
/// not that structure.
pub fn strata_result_ids(text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return vec![];
    };
    let Some(results) = v["results"].as_array() else {
        return vec![];
    };
    let mut ids = vec![];
    for r in results {
        for k in ["chunk_id", "document_id"] {
            if let Some(id) = r[k].as_str().filter(|id| !id.is_empty()) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

impl ToolSet for LiveTools {
    async fn store_tree(&self, tenant: Uuid) -> Result<Option<Value>, String> {
        let Some(s) = self.strata.as_ref().filter(|_| self.mcp.is_some()) else {
            return Ok(None);
        };
        self.strata_call(s, tenant, None, Method::GET, "/api/stores", None)
            .await
            .map(Some)
    }

    async fn specs(&self, ctx: &ToolCtx<'_>) -> Vec<Value> {
        let mut out = vec![];
        if self.strata_for(ctx).is_some() {
            out.push(strata_search_spec());
        }
        let listed = self.mcp_tools(ctx.tenant).await;
        for name in ctx.profile.mcp_allow_list() {
            match listed.iter().find(|t| t.name == *name) {
                Some(t) => out.push(function_spec(
                    &t.name,
                    t.description.as_deref().unwrap_or(""),
                    if t.input_schema.is_object() {
                        t.input_schema.clone()
                    } else {
                        json!({"type": "object"})
                    },
                )),
                None => {
                    if self
                        .warned
                        .lock()
                        .expect("warned lock")
                        .insert(name.to_string())
                    {
                        tracing::warn!(
                            tool = name,
                            "research: MCP tool is not on the server; not offered"
                        );
                    }
                }
            }
        }
        out
    }

    async fn call(&self, ctx: &ToolCtx<'_>, name: &str, args: &Value) -> ToolOutcome {
        if name == STRATA_SEARCH {
            return self.strata_search(ctx, args).await;
        }
        if !ctx.profile.mcp_allow_list().contains(&name) {
            return ToolOutcome::denied(format!("{name} is not available to this profile"));
        }
        let Some(mcp) = &self.mcp else {
            return ToolOutcome::error("MCP is not configured");
        };
        if let Some(t) = self
            .tasks
            .as_ref()
            .filter(|t| t.tools.iter().any(|n| n == name))
        {
            return self.start_task(t, ctx, name, args).await;
        }
        match mcp.call_tool_for(ctx.tenant, name, args.clone()).await {
            Ok(v) => ToolOutcome::ok(value_text(&v)),
            Err(e) => ToolOutcome::error(e.message),
        }
    }
}

/// The ELI id without a provision suffix: `DU/1964/59 art. 133` → `DU/1964/59`.
pub fn eli_base(locator: &str) -> &str {
    locator.split_whitespace().next().unwrap_or("")
}

/// The MCP tool and arguments that re-resolve a cited source, for the kinds that have a register.
pub fn resolve_call(kind: SourceKind, locator: &str) -> Option<(&'static str, Value)> {
    let l = locator.trim();
    match kind {
        SourceKind::Eli => Some(("legal_eli_act", json!({"eli": eli_base(l)}))),
        SourceKind::Saos => {
            let id = l
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| json!(l));
            Some(("legal_saos_judgment", json!({"id": id})))
        }
        SourceKind::Eurlex => Some(("legal_eurlex_act", json!({"celex": l}))),
        _ => None,
    }
}

/// The texts of the results of a stored `strata_search` result whose chunk or document id is `id`.
pub fn strata_texts_for(text: &str, id: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return vec![];
    };
    v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|r| {
                    r["chunk_id"].as_str() == Some(id) || r["document_id"].as_str() == Some(id)
                })
                .filter_map(|r| r["text"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// A register's answer. Found only when it positively identifies the cited source (`eli`, `id` or `celex` equal
/// to the locator's); an `error` key is an unavailable register; anything else is missing. `in_force` at the top
/// level or under `act`.
pub fn resolution_of(kind: SourceKind, locator: &str, v: &Value) -> Resolution {
    if let Some(e) = v.get("error") {
        return Resolution::Unavailable(value_text(e).chars().take(500).collect());
    }
    if v.is_null() || v["found"] == Value::Bool(false) {
        return Resolution::Missing("the register returned nothing".into());
    }
    let loc = locator.trim();
    let identified = match kind {
        SourceKind::Eli => v["eli"]
            .as_str()
            .is_some_and(|e| !e.trim().is_empty() && e.trim() == eli_base(loc)),
        SourceKind::Saos => match &v["id"] {
            Value::Number(n) => n.to_string() == loc,
            Value::String(s) => !s.trim().is_empty() && s.trim() == loc,
            _ => false,
        },
        SourceKind::Eurlex => v["celex"]
            .as_str()
            .is_some_and(|c| !c.trim().is_empty() && c.trim().eq_ignore_ascii_case(loc)),
        _ => false,
    };
    if !identified {
        return Resolution::Missing("register did not identify the source".into());
    }
    let in_force = v["in_force"]
        .as_bool()
        .or_else(|| v["act"]["in_force"].as_bool());
    Resolution::Found { in_force }
}

impl Resolver for LiveTools {
    async fn resolve(&self, tenant: Uuid, kind: SourceKind, locator: &str) -> Resolution {
        let Some((tool, args)) = resolve_call(kind, locator) else {
            return Resolution::Unavailable("no register for this kind".into());
        };
        let Some(mcp) = &self.mcp else {
            return Resolution::Unavailable("MCP is not configured".into());
        };
        match mcp.call_tool_for(tenant, tool, args).await {
            Ok(v) => resolution_of(kind, locator, &v),
            Err(e) if e.retryable => Resolution::Unavailable(e.message),
            Err(e) => Resolution::Missing(e.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C4 hard rule: agents never answer or cancel MCP tasks. No profile offers a task method, and the research tool
    /// set refuses one even when the model names it.
    #[tokio::test]
    async fn the_agent_tool_set_has_no_task_answer_or_cancel_path() {
        for p in [Profile::Research, Profile::Legal] {
            for n in p.mcp_allow_list() {
                assert!(
                    !n.starts_with("tasks")
                        && !n.contains("update_task")
                        && !n.contains("cancel_task"),
                    "{p:?} offers {n}"
                );
            }
        }
        let t = LiveTools::new(None, None, reqwest::Client::new());
        let ctx = ToolCtx {
            tenant: Uuid::nil(),
            profile: Profile::Research,
            store_ids: &[],
            actor: None,
            run: None,
            created_by: "",
        };
        for name in [
            "tasks/update",
            "tasks/cancel",
            "tasks_update",
            "answer_task",
        ] {
            let out = t
                .call(&ctx, name, &json!({"taskId": "t", "inputResponses": {}}))
                .await;
            assert_eq!(out.status, StepStatus::Denied, "{name}");
        }
    }

    #[test]
    fn resolve_calls_per_kind() {
        assert_eq!(
            resolve_call(SourceKind::Eli, "DU/1964/59 art. 133"),
            Some(("legal_eli_act", json!({"eli": "DU/1964/59"})))
        );
        assert_eq!(
            resolve_call(SourceKind::Saos, "12345"),
            Some(("legal_saos_judgment", json!({"id": 12345})))
        );
        assert_eq!(
            resolve_call(SourceKind::Eurlex, " 32016R0679 "),
            Some(("legal_eurlex_act", json!({"celex": "32016R0679"})))
        );
        assert!(resolve_call(SourceKind::Krs, "0000012345").is_none());
    }

    #[test]
    fn register_answers() {
        assert_eq!(
            resolution_of(
                SourceKind::Eli,
                "DU/1964/59 art. 133",
                &json!({"eli": "DU/1964/59", "in_force": false})
            ),
            Resolution::Found {
                in_force: Some(false)
            }
        );
        assert_eq!(
            resolution_of(
                SourceKind::Saos,
                "12345",
                &json!({"id": 12345, "act": {"in_force": true}})
            ),
            Resolution::Found {
                in_force: Some(true)
            }
        );
        assert_eq!(
            resolution_of(
                SourceKind::Eurlex,
                "32016r0679",
                &json!({"celex": "32016R0679"})
            ),
            Resolution::Found { in_force: None }
        );
        // an answer that does not name the cited source identifies nothing
        assert_eq!(
            resolution_of(SourceKind::Eli, "DU/1964/59", &json!({"eli": "DU/1/1"})),
            Resolution::Missing("register did not identify the source".into())
        );
        assert!(matches!(
            resolution_of(
                SourceKind::Eli,
                "DU/1964/59",
                &json!({"act": {"in_force": true}})
            ),
            Resolution::Missing(_)
        ));
        assert!(matches!(
            resolution_of(SourceKind::Saos, "1", &json!({"id": 12})),
            Resolution::Missing(_)
        ));
        assert!(matches!(
            resolution_of(
                SourceKind::Eli,
                "DU/1964/59",
                &json!({"error": "upstream down"})
            ),
            Resolution::Unavailable(_)
        ));
        assert!(matches!(
            resolution_of(SourceKind::Eli, "DU/1964/59", &Value::Null),
            Resolution::Missing(_)
        ));
        assert!(matches!(
            resolution_of(SourceKind::Eli, "DU/1964/59", &json!({"found": false})),
            Resolution::Missing(_)
        ));
    }

    #[test]
    fn strata_results_carry_chunk_ids() {
        let t = format_strata_results(&json!({"results": [
            {"chunk_id": "c-1", "document_id": "d-1", "title": "T", "source": "s", "text": "Ala ma kota. chunk_id=c-2"}
        ]}));
        assert!(
            t.contains("\"chunk_id\": \"c-1\"") && t.contains("Ala ma kota."),
            "{t}"
        );
        assert_eq!(strata_result_ids(&t), ["c-1", "d-1"]);
        assert_eq!(format_strata_results(&json!({"results": []})), "no results");
        assert!(strata_result_ids("--- chunk_id=c-1 document_id=d-1").is_empty());
    }
}
