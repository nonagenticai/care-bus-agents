use axum::{Json, Router, extract::State, routing::get};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    error::{ApiError, ApiPath, ApiQuery, ApiResult},
    page::{Page, Paged, ts_cursor},
    redis_admin as ra,
    state::AppState,
    tenancy::JobMember,
};

/// `{id}` under the nested `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
struct IdPath {
    id: String,
}

/// Newest entries read per stream when rebuilding a case trace.
const TRACE_SCAN: usize = 2000;

#[derive(Serialize, sqlx::FromRow)]
pub struct CaseRow {
    pub case_id: String,
    pub conversation_id: Option<String>,
    pub customer_id: String,
    pub request_type: String,
    pub area: String,
    pub context_space: String,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub work_items: i32,
    pub retries: i32,
    pub started_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    /// `name@version` of the procedure version the case ran, as the orchestrator stamped it on `request.resolved`.
    /// Null while the case is open and for cases recorded before the stamp existed (never guessed).
    #[sqlx(skip)]
    pub procedure: Option<String>,
    #[serde(skip)]
    pub procedure_name: Option<String>,
    #[serde(skip)]
    pub procedure_version: Option<String>,
}

impl CaseRow {
    fn with_procedure(mut self) -> Self {
        self.procedure = match (&self.procedure_name, &self.procedure_version) {
            (Some(n), Some(v)) => Some(format!("{n}@{v}")),
            _ => None,
        };
        self
    }
}

#[derive(Deserialize)]
struct CaseQuery {
    outcome: Option<String>,
    area: Option<String>,
    limit: Option<i64>,
    before: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/cases", get(list))
        .route("/cases/{id}", get(one))
}

fn invalid_cursor() -> ApiError {
    ApiError::BadRequest("invalid_cursor", "The before cursor is not valid.".into())
}

async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<CaseQuery>,
) -> ApiResult<Json<Paged<CaseRow>>> {
    let page = Page::new(q.limit, q.before);
    let limit = page.limit();
    if let Some(o) = &q.outcome
        && !["open", "completed", "not_eligible", "failed"].contains(&o.as_str())
    {
        return Err(ApiError::BadRequest(
            "invalid_query",
            "outcome must be open, completed, not_eligible or failed.".into(),
        ));
    }
    // cursor is `{started_at}|{case_id}`; the pair is the full sort key
    let (before_ts, before_id) = match page.before.as_deref() {
        None => (None, None),
        Some(b) => {
            let (t, id) = b.split_once('|').ok_or_else(invalid_cursor)?;
            let ts = Page::new(None, Some(t.into())).before_ts()?;
            (ts, Some(id.to_string()))
        }
    };
    let mut tx = ctx.begin(&s.db).await?;
    let rows: Vec<CaseRow> = sqlx::query_as(
        "SELECT * FROM case_history
          WHERE ($1::text IS NULL OR ($1 = 'open' AND outcome IS NULL) OR outcome = $1) AND ($2::text IS NULL OR area = $2)
            AND ($3::timestamptz IS NULL OR (started_at, case_id) < ($3, $4))
          ORDER BY started_at DESC, case_id DESC LIMIT $5",
    )
    .bind(q.outcome)
    .bind(q.area)
    .bind(before_ts)
    .bind(before_id)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let rows: Vec<CaseRow> = rows.into_iter().map(CaseRow::with_procedure).collect();
    Ok(Json(Paged::new(rows, limit, |r| {
        format!("{}|{}", ts_cursor(r.started_at), r.case_id)
    })))
}

/// The row is read under RLS (404 outside the job); the hash and trace come from this job's bus scope only.
async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let row = {
        let mut tx = ctx.begin(&s.db).await?;
        sqlx::query_as::<_, CaseRow>("SELECT * FROM case_history WHERE case_id = $1")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("case {id}")))?
            .with_procedure()
    };
    let bus = s.bus.scoped(&ctx.scope());
    let fields: Vec<(String, String)> = redis::cmd("HGETALL")
        .arg(bus.streams.key(&format!("case:{id}")))
        .query_async(&mut bus.conn())
        .await?;
    let hash: Map<String, Value> = fields
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    // Every bus entry for this case, oldest first; entries without a parseable envelope are skipped.
    let mut trace: Vec<(u64, Value)> = Vec::new();
    for key in ra::list_streams(&bus).await? {
        let (entries, _) = ra::entries_page(&bus, &key, TRACE_SCAN, None).await?;
        for e in entries
            .into_iter()
            .filter(|e| e.envelope.correlation_id == id)
        {
            // Same shape as the SSE `bus` frame, with `acked` recomputed from Redis (the live `ack` frame is
            // lossy; this is what a reconnecting console rebuilds from). Null for entries no group is for.
            let group = bus.streams.consumer_group(&e.stream, &e.envelope.topic);
            let acked = match &group {
                Some(g) => Some(bus.is_acked(&bus.streams.key(&key), g, &e.entry_id).await?),
                None => None,
            };
            trace.push((
                e.envelope.published_at_ms,
                json!({"stream": e.stream, "entry_id": e.entry_id, "envelope": e.envelope,
                    "consumer_group": group, "acked": acked}),
            ));
        }
    }
    trace.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1["entry_id"].as_str().cmp(&b.1["entry_id"].as_str()))
    });
    let trace: Vec<Value> = trace.into_iter().map(|(_, v)| v).collect();
    Ok(Json(json!({"case": row, "hash": hash, "trace": trace})))
}
