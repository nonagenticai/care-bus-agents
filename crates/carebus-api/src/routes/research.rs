//! Research runs of one job (plan 18, `/api/jobs/{job}/research-runs`). A run is queued here and executed by the
//! `researcher` bin, which claims it with `carebus_research_claim` and checkpoints every step; these routes only
//! create, read and cancel. Every query runs under the job's RLS GUCs, so another tenant's or job's run is a 404.
//! Any member of the tenant may create, read and cancel (a service token may not: `JobMember` needs a tenant role).
//!
//! Legal matters (migration 0026): a `legal` run must be filed under a matter the caller is a member of, and may
//! search only that matter's stores. A run under a matter is visible to the matter's members only (RLS `matter_wall`;
//! a tenant_admin who is not a member gets 404 like any other non-member). A store attached to a matter is walled
//! (crate::walls): only runs under THAT matter may search it, or any store above it in the strata tree.
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    audit,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    page::{Page, Paged, ts_cursor},
    state::AppState,
    tenancy::JobMember,
    walls,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/research-runs", get(list).post(create))
        .route("/research-runs/{id}", get(one))
        .route("/research-runs/{id}/cancel", post(cancel))
}

/// `{id}` under the nested `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
struct IdPath {
    id: Uuid,
}

const PROFILES: [&str; 2] = ["research", "legal"];
const STATUSES: [&str; 7] = [
    "queued",
    "running",
    "waiting",
    "verifying",
    "done",
    "failed",
    "cancelled",
];
const TERMINAL: [&str; 3] = ["done", "failed", "cancelled"];
const QUESTION_MAX: usize = 8000;
const STORES_MAX: usize = 20;
/// The list shows the start of each question only.
const QUESTION_PREVIEW: i32 = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    profile: String,
    question: String,
    #[serde(default)]
    store_ids: Option<Vec<String>>,
    #[serde(default)]
    budget: Option<BudgetInput>,
    /// The legal matter the run is filed under: required for `legal`, optional for `research`.
    #[serde(default)]
    matter_id: Option<Uuid>,
}

/// Every key optional: the runner fills defaults for the missing ones, so only what was given is stored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetInput {
    max_steps: Option<i64>,
    max_tool_calls: Option<i64>,
    max_wall_s: Option<i64>,
    max_tokens: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct RunView {
    id: Uuid,
    profile: String,
    matter_id: Option<Uuid>,
    question: String,
    store_ids: Vec<String>,
    status: String,
    budget: Value,
    steps_used: i32,
    tokens_used: i64,
    attempts: i32,
    report: Option<String>,
    error: Option<String>,
    cancel_requested: bool,
    created_by: String,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

const RUN_COLUMNS: &str =
    "id, profile, matter_id, question, store_ids, status, budget, steps_used, tokens_used, attempts, report, error,
     cancel_requested, created_by, created_at, started_at, finished_at, updated_at";

#[derive(Serialize, sqlx::FromRow)]
struct RunSummary {
    id: Uuid,
    profile: String,
    matter_id: Option<Uuid>,
    question: String,
    status: String,
    steps_used: i32,
    tokens_used: i64,
    created_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, sqlx::FromRow)]
struct StepView {
    ordinal: i32,
    kind: String,
    status: String,
    duration_ms: Option<i32>,
    /// The tool's name for a `tool` step (`request->>'name'`); null for a model turn.
    tool: Option<String>,
    created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<Value>,
}

#[derive(Serialize, sqlx::FromRow)]
struct ClaimView {
    id: Uuid,
    ordinal: i32,
    text: String,
    status: String,
    citation_ids: Vec<Uuid>,
}

#[derive(Serialize, sqlx::FromRow)]
struct CitationView {
    id: Uuid,
    source_kind: String,
    locator: String,
    title: Option<String>,
    quote: String,
    step_ordinal: Option<i32>,
    verify_status: String,
    verify_detail: Option<Value>,
    verified_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.into())
}

fn bad_query(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest("invalid_query", msg.into())
}

fn invalid_cursor() -> ApiError {
    ApiError::BadRequest("invalid_cursor", "The before cursor is not valid.".into())
}

/// `{key: value}` for each given budget key, or the 400 naming the first one out of range.
fn budget_json(b: Option<BudgetInput>) -> ApiResult<Value> {
    let Some(b) = b else {
        return Ok(json!({}));
    };
    let mut m = Map::new();
    for (key, v, lo, hi) in [
        ("max_steps", b.max_steps, 1, 200),
        ("max_tool_calls", b.max_tool_calls, 1, 400),
        ("max_wall_s", b.max_wall_s, 60, 21_600),
        ("max_tokens", b.max_tokens, 1000, 5_000_000),
    ] {
        if let Some(v) = v {
            if !(lo..=hi).contains(&v) {
                return Err(bad(format!("budget.{key} must be {lo} to {hi}.")));
            }
            m.insert(key.into(), json!(v));
        }
    }
    Ok(Value::Object(m))
}

/// A strata store id in canonical (hyphenated, lower-case) form, or `None` when it is not a uuid.
pub(crate) fn canonical_store(s: &str) -> Option<String> {
    s.trim()
        .parse::<Uuid>()
        .ok()
        .map(|u| u.hyphenated().to_string())
}

/// Canonical (hyphenated, lower-case) store ids, de-duplicated in order.
fn store_ids(ids: Option<Vec<String>>) -> ApiResult<Vec<String>> {
    let ids = ids.unwrap_or_default();
    if ids.len() > STORES_MAX {
        return Err(bad(format!("store_ids takes at most {STORES_MAX} stores.")));
    }
    let mut out: Vec<String> = Vec::with_capacity(ids.len());
    for s in &ids {
        let c = canonical_store(s).ok_or_else(|| bad(format!("store_ids: {s} is not a uuid.")))?;
        if !out.contains(&c) {
            out.push(c);
        }
    }
    Ok(out)
}

/// A run may search only the strata stores bound to this job's knowledge stores (the effective layout, read under
/// RLS); anything else is 422 `unknown_store`.
pub(crate) async fn check_stores(conn: &mut PgConnection, ids: &[String]) -> ApiResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let known: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT strata_store_id::text FROM eff_knowledge_stores
          WHERE strata_store_id::text = ANY($1)",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    if let Some(missing) = ids.iter().find(|i| !known.contains(*i)) {
        return Err(ApiError::Unprocessable(
            "unknown_store",
            format!("store {missing} is not a knowledge store of this job."),
        ));
    }
    Ok(())
}

fn walled(store: &str) -> ApiError {
    ApiError::Unprocessable(
        "store_walled",
        format!(
            "store {store} belongs to (or holds a store of) a legal matter; file the run under that matter."
        ),
    )
}

/// The walls of OTHER matters (every job of the tenant) a run's stores may not reach: a store of another matter, or
/// one above such a store (a run routed from it would search its subtree). The tree is read only when such a wall
/// exists.
async fn check_reach(
    s: &AppState,
    conn: &mut PgConnection,
    tenant: Uuid,
    matter: Option<Uuid>,
    stores: &[String],
) -> ApiResult<()> {
    if stores.is_empty() {
        return Ok(());
    }
    let foreign: Vec<String> = walls::walls(conn)
        .await?
        .into_iter()
        .filter(|w| Some(w.matter_id) != matter)
        .map(|w| w.store_id)
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    if let Some(w) = stores.iter().find(|st| foreign.contains(*st)) {
        return Err(walled(w));
    }
    let tree = walls::Tree::load(s, tenant).await?;
    for st in stores {
        let reach = tree.subtree(st);
        if foreign.iter().any(|f| reach.contains(f)) {
            return Err(walled(st));
        }
    }
    Ok(())
}

/// The matter rules of a new run (migration 0026):
/// - a matter is optional for every profile (owner 2026-10-25: matters are hidden in the web UI); without one only
///   the walls apply ([`check_reach`]);
/// - the matter must be one the caller is a MEMBER of (422 `unknown_matter`; a tenant_admin who is not a member is
///   refused too) and open (409 `matter_closed`);
/// - every store must be attached to that matter (422 `store_not_in_matter`);
/// - the stores' reach is [`check_reach`].
async fn check_matter(
    conn: &mut PgConnection,
    sub: &str,
    matter: Option<Uuid>,
    stores: &[String],
) -> ApiResult<()> {
    let Some(m) = matter else {
        // the walls themselves: check_reach (tenant-wide)
        return Ok(());
    };
    let closed: Option<bool> = sqlx::query_scalar(
        "SELECT m.closed_at IS NOT NULL FROM legal_matters m
          WHERE m.id = $1
            AND EXISTS (SELECT 1 FROM legal_matter_members mm WHERE mm.matter_id = m.id AND mm.user_sub = $2)",
    )
    .bind(m)
    .bind(sub)
    .fetch_optional(&mut *conn)
    .await?;
    match closed {
        None => {
            return Err(ApiError::Unprocessable(
                "unknown_matter",
                format!("matter {m} is not one of your matters."),
            ));
        }
        Some(true) => {
            return Err(ApiError::Conflict(
                "matter_closed",
                "This matter is closed.".into(),
            ));
        }
        Some(false) => {}
    }
    let attached: Vec<String> =
        sqlx::query_scalar("SELECT store_id FROM legal_matter_stores WHERE matter_id = $1")
            .bind(m)
            .fetch_all(&mut *conn)
            .await?;
    if let Some(missing) = stores.iter().find(|s| !attached.contains(*s)) {
        return Err(ApiError::Unprocessable(
            "store_not_in_matter",
            format!("store {missing} is not attached to this matter."),
        ));
    }
    Ok(())
}

async fn load(conn: &mut PgConnection, id: Uuid) -> ApiResult<RunView> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM research_runs WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("research run {id}")))
}

async fn create(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiJson(i): ApiJson<RunInput>,
) -> ApiResult<(StatusCode, Json<RunView>)> {
    if !PROFILES.contains(&i.profile.as_str()) {
        return Err(bad("profile must be research or legal."));
    }
    let question = i.question.trim();
    let n = question.chars().count();
    if n == 0 || n > QUESTION_MAX {
        return Err(bad(format!(
            "question must be 1 to {QUESTION_MAX} characters."
        )));
    }
    let stores = store_ids(i.store_ids)?;
    let budget = budget_json(i.budget)?;
    let id = Uuid::new_v4();
    let mut tx = ctx.begin(&s.db).await?;
    check_stores(&mut tx, &stores).await?;
    check_matter(&mut tx, &ctx.t.caller.sub, i.matter_id, &stores).await?;
    check_reach(&s, &mut tx, ctx.t.tenant, i.matter_id, &stores).await?;
    let run: RunView = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO research_runs (id, profile, question, store_ids, budget, created_by, matter_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {RUN_COLUMNS}"
    )))
    .bind(id)
    .bind(&i.profile)
    .bind(question)
    .bind(&stores)
    .bind(&budget)
    .bind(&ctx.t.caller.sub)
    .bind(i.matter_id)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "create",
        "research_run",
        &id.to_string(),
        true,
        json!({"profile": run.profile, "matter_id": run.matter_id, "stores": stores.len(), "budget": budget}),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(run)))
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    /// Only the runs of this matter (still only those the caller may see).
    matter_id: Option<Uuid>,
    limit: Option<i64>,
    before: Option<String>,
}

/// Newest first; the cursor is `{created_at}|{id}`, the full sort key. A matter's runs appear only to its members
/// (RLS `matter_wall`).
async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Paged<RunSummary>>> {
    if let Some(st) = &q.status
        && !STATUSES.contains(&st.as_str())
    {
        return Err(bad_query(format!(
            "status must be one of {}.",
            STATUSES.join(", ")
        )));
    }
    let page = Page::new(q.limit, q.before);
    let limit = page.limit();
    let (before_ts, before_id) = match page.before.as_deref() {
        None => (None, None),
        Some(b) => {
            let (t, id) = b.split_once('|').ok_or_else(invalid_cursor)?;
            let ts = Page::new(None, Some(t.into())).before_ts()?;
            let id: Uuid = id.parse().map_err(|_| invalid_cursor())?;
            (ts, Some(id))
        }
    };
    let mut tx = ctx.begin(&s.db).await?;
    let rows: Vec<RunSummary> = sqlx::query_as(
        "SELECT id, profile, matter_id, left(question, $1) AS question, status, steps_used, tokens_used, created_at,
                finished_at
           FROM research_runs
          WHERE ($2::text IS NULL OR status = $2)
            AND ($3::timestamptz IS NULL OR (created_at, id) < ($3, $4))
            AND ($6::uuid IS NULL OR matter_id = $6)
          ORDER BY created_at DESC, id DESC LIMIT $5",
    )
    .bind(QUESTION_PREVIEW)
    .bind(&q.status)
    .bind(before_ts)
    .bind(before_id)
    .bind(limit)
    .bind(q.matter_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Paged::new(rows, limit, |r| {
        format!("{}|{}", ts_cursor(r.created_at), r.id)
    })))
}

#[derive(Deserialize)]
struct OneQuery {
    include: Option<String>,
}

/// The run with its steps (no tool results unless `?include=steps_full`), claims and citations.
async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiQuery(q): ApiQuery<OneQuery>,
) -> ApiResult<Json<Value>> {
    let full = match q.include.as_deref() {
        None | Some("") => false,
        Some("steps_full") => true,
        Some(_) => return Err(bad_query("include must be steps_full.")),
    };
    let mut tx = ctx.begin(&s.db).await?;
    let run = load(&mut tx, id).await?;
    let steps: Vec<StepView> = sqlx::query_as(
        "SELECT ordinal, kind, status, duration_ms, CASE WHEN kind = 'tool' THEN request->>'name' END AS tool,
                created_at,
                CASE WHEN $2 THEN request END AS request, CASE WHEN $2 THEN response END AS response
           FROM research_steps WHERE run_id = $1 ORDER BY ordinal",
    )
    .bind(id)
    .bind(full)
    .fetch_all(&mut *tx)
    .await?;
    let claims: Vec<ClaimView> = sqlx::query_as(
        "SELECT id, ordinal, text, status, citation_ids FROM research_claims WHERE run_id = $1 ORDER BY ordinal",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let citations: Vec<CitationView> = sqlx::query_as(
        "SELECT id, source_kind, locator, title, quote, step_ordinal, verify_status, verify_detail, verified_at,
                created_at
           FROM research_citations WHERE run_id = $1 ORDER BY created_at, id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(
        json!({"run": run, "steps": steps, "claims": claims, "citations": citations}),
    ))
}

/// Asks the runner to stop: `cancel_requested` is set; a run nobody holds (queued, or waiting on an MCP task, whose tasks are cancelled too) is cancelled at once. A finished
/// run is 409 `run_finished`.
async fn cancel(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<RunView>> {
    let mut tx = ctx.begin(&s.db).await?;
    let status: String =
        sqlx::query_scalar("SELECT status FROM research_runs WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("research run {id}")))?;
    if TERMINAL.contains(&status.as_str()) {
        return Err(ApiError::Conflict(
            "run_finished",
            format!("This run is already {status}."),
        ));
    }
    let run: RunView = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE research_runs
            SET cancel_requested = true,
                status = CASE WHEN status IN ('queued', 'waiting') THEN 'cancelled' ELSE status END,
                finished_at = CASE WHEN status IN ('queued', 'waiting') THEN now() ELSE finished_at END,
                updated_at = now()
          WHERE id = $1
         RETURNING {RUN_COLUMNS}"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if status == "waiting" {
        // Nobody holds a waiting run: it is cancelled above, and the task it waits on is cancelled too (the poller
        // sends tasks/cancel). When the task ends, finish() requeues only a run that is still waiting.
        sqlx::query(
            "UPDATE mcp_tasks SET cancel_requested = true, updated_at = now()
              WHERE run_id = $1 AND terminal_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "cancel",
        "research_run",
        &id.to_string(),
        true,
        json!({"from": status, "to": run.status}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(run))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_keeps_given_keys_and_checks_ranges() {
        assert_eq!(budget_json(None).unwrap(), json!({}));
        let b = BudgetInput {
            max_steps: Some(10),
            max_tool_calls: None,
            max_wall_s: Some(60),
            max_tokens: None,
        };
        assert_eq!(
            budget_json(Some(b)).unwrap(),
            json!({"max_steps": 10, "max_wall_s": 60})
        );
        let b = BudgetInput {
            max_steps: None,
            max_tool_calls: None,
            max_wall_s: Some(59),
            max_tokens: None,
        };
        assert!(budget_json(Some(b)).is_err());
    }

    #[test]
    fn store_ids_are_canonical_and_bounded() {
        let u = "6F9619FF-8B86-D011-B42D-00CF4FC964FF";
        assert_eq!(
            store_ids(Some(vec![u.into(), u.to_lowercase()])).unwrap(),
            vec![u.to_lowercase()]
        );
        assert!(store_ids(Some(vec!["nope".into()])).is_err());
        let many = (0..21).map(|_| Uuid::new_v4().to_string()).collect();
        assert!(store_ids(Some(many)).is_err());
    }
}
