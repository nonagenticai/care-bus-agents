//! MCP tasks of one job (`/api/jobs/{job}/mcp-tasks`, crate::mcp_tasks): the staff inbox of long-running MCP work.
//! Staff see each task's status and its open questions, answer a question as themselves (the server is told
//! `care-bus:<tenant>:<sub>`), and cancel a task. Every route needs a PERSON in the tenant: a service token never
//! answers or cancels a task, and no agent tool reaches these routes (the agent never decides).
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use carebus::mcp_tasks::StaffActor;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    audit,
    auth::CallerKind,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    mcp_tasks::{AnswerInput, AnswerResult, answer, cancel as cancel_task, open_questions},
    state::AppState,
    tenancy::{JobCtx, JobMember},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/mcp-tasks", get(list))
        .route("/mcp-tasks/{id}", get(one))
        .route("/mcp-tasks/{id}/answers", post(answer_one))
        .route("/mcp-tasks/{id}/cancel", post(cancel))
}

#[derive(Deserialize)]
struct IdPath {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    /// `open` = working / input_required only.
    #[serde(default)]
    state: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Row {
    task_id: String,
    tool: String,
    status: String,
    status_message: Option<String>,
    run_id: Option<uuid::Uuid>,
    case_id: Option<uuid::Uuid>,
    last_detail: Option<Value>,
    result: Option<Value>,
    cancel_requested: bool,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    terminal_at: Option<DateTime<Utc>>,
    answered: Vec<String>,
}

#[derive(Serialize)]
struct TaskView {
    task_id: String,
    tool: String,
    status: String,
    status_message: Option<String>,
    run_id: Option<uuid::Uuid>,
    case_id: Option<uuid::Uuid>,
    /// Open questions nobody here answered yet: `{key, message, requested_schema}`.
    questions: Vec<Value>,
    result: Option<Value>,
    cancel_requested: bool,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    terminal_at: Option<DateTime<Utc>>,
}

impl From<Row> for TaskView {
    fn from(r: Row) -> Self {
        let questions = if r.status == "input_required" {
            open_questions(r.last_detail.as_ref(), &r.answered)
        } else {
            vec![]
        };
        Self {
            task_id: r.task_id,
            tool: r.tool,
            status: r.status,
            status_message: r.status_message,
            run_id: r.run_id,
            case_id: r.case_id,
            questions,
            result: r.result,
            cancel_requested: r.cancel_requested,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
            terminal_at: r.terminal_at,
        }
    }
}

const COLUMNS: &str = "t.task_id, t.tool, t.status, t.status_message, t.run_id, t.case_id, t.last_detail, t.result,
    t.cancel_requested, t.created_by, t.created_at, t.updated_at, t.terminal_at,
    coalesce((SELECT array_agg(a.input_key) FROM mcp_task_answers a WHERE a.task_id = t.task_id), '{}') AS answered";

/// A person in the tenant (a service token is 403).
fn staff(ctx: &JobCtx) -> ApiResult<StaffActor> {
    if !matches!(ctx.t.caller.kind, CallerKind::Person { .. }) {
        return Err(ApiError::Forbidden("person in the tenant"));
    }
    Ok(StaffActor {
        tenant: ctx.t.tenant,
        staff_user: ctx.t.caller.sub.clone(),
    })
}

async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    staff(&ctx)?;
    let open = match q.state.as_deref() {
        None | Some("all") => false,
        Some("open") => true,
        Some(_) => {
            return Err(ApiError::BadRequest(
                "invalid_query",
                "state must be open or all.".into(),
            ));
        }
    };
    let mut tx = ctx.begin(&s.db).await?;
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM mcp_tasks t
          WHERE NOT $1 OR t.status IN ('working', 'input_required')
          ORDER BY (t.status = 'input_required') DESC, t.created_at DESC
          LIMIT 200"
    )))
    .bind(open)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let items: Vec<TaskView> = rows.into_iter().map(TaskView::from).collect();
    Ok(Json(json!({ "items": items })))
}

async fn fetch(s: &AppState, ctx: &JobCtx, id: &str) -> ApiResult<TaskView> {
    let mut tx = ctx.begin(&s.db).await?;
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM mcp_tasks t WHERE t.task_id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    row.map(TaskView::from)
        .ok_or_else(|| ApiError::NotFound(format!("task {id}")))
}

async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<TaskView>> {
    staff(&ctx)?;
    Ok(Json(fetch(&s, &ctx, &id).await?))
}

#[derive(Serialize)]
struct AnswerView {
    #[serde(flatten)]
    outcome: AnswerResult,
    task: TaskView,
}

/// One human answer to an open question (crate::mcp_tasks::answer).
async fn answer_one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiJson(input): ApiJson<AnswerInput>,
) -> ApiResult<Json<AnswerView>> {
    let actor = staff(&ctx)?;
    let mcp = s.mcp_catalog.as_ref().ok_or(ApiError::Upstream("mcp"))?;
    let outcome = answer(&s.db, mcp, &actor, ctx.job_id, &id, &input).await?;
    let mut tx = ctx.begin(&s.db).await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "answer",
        "mcp_task",
        &id,
        true,
        json!({"key": input.key, "action": input.action, "sent": outcome.sent,
               "already_answered": outcome.already_answered}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(AnswerView {
        outcome,
        task: fetch(&s, &ctx, &id).await?,
    }))
}

/// Asks the server to cancel (crate::mcp_tasks::cancel). The task stays open until the server reports it cancelled.
async fn cancel(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    staff(&ctx)?;
    let mcp = s.mcp_catalog.as_ref().ok_or(ApiError::Upstream("mcp"))?;
    let sent = cancel_task(&s.db, mcp, ctx.t.tenant, ctx.job_id, &id).await?;
    let mut tx = ctx.begin(&s.db).await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "cancel",
        "mcp_task",
        &id,
        true,
        json!({"sent": sent}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"sent": sent, "task": fetch(&s, &ctx, &id).await?}),
    ))
}
