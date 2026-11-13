//! Audit trail: the writer every mutating route calls, and `GET /api/audit` (the active tenant's rows; `?job=<key>`
//! narrows them to one job through the job GUC, without it every job's and the tenant-level rows are listed).
use axum::extract::State;
use axum::{Json, Router, routing::get};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;

use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::page::{Page, Paged};
use crate::state::AppState;
use crate::tenancy::Member;

/// Writes one audit row on a scoped connection (`TenantCtx::begin` / `JobCtx::begin`): `tenant_id` and `job_id`
/// come from the column defaults, i.e. the transaction's GUCs (job empty = a tenant-level action).
#[allow(clippy::too_many_arguments)]
pub async fn record(
    conn: &mut PgConnection,
    actor: &str,
    actor_type: &str,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    ok: bool,
    details: Value,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO audit_events (actor_id, actor_type, action_type, resource_type, resource_id, status, details)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(actor)
    .bind(actor_type)
    .bind(action)
    .bind(resource_type)
    .bind(resource_id)
    .bind(if ok { "success" } else { "failure" })
    .bind(details)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AuditRow {
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub actor_id: String,
    pub actor_type: String,
    pub action_type: String,
    pub resource_type: String,
    pub resource_id: String,
    pub status: String,
    pub details: Value,
    pub request_id: Option<String>,
    pub ip_address: Option<String>,
}

#[derive(Deserialize)]
pub struct AuditQuery {
    q: Option<String>,
    actor_type: Option<String>,
    status: Option<String>,
    resource_type: Option<String>,
    limit: Option<i64>,
    before: Option<String>,
    /// A job key of the tenant; 404 when it has none.
    job: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/audit", get(list))
}

async fn list(
    Member(ctx): Member,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<AuditQuery>,
) -> ApiResult<Json<Paged<AuditRow>>> {
    let page = Page::new(q.limit, q.before);
    let limit = page.limit();
    let before = page.before_i64()?;
    let mut tx = ctx.begin(&s.db).await?;
    if let Some(key) = &q.job {
        let job: Option<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
        let job = job.ok_or_else(|| ApiError::NotFound("job".into()))?;
        sqlx::query("SELECT set_config('app.current_job', $1, true)")
            .bind(job.to_string())
            .execute(&mut *tx)
            .await?;
    }
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT id, timestamp, actor_id, actor_type, action_type, resource_type, resource_id, status, details, request_id, ip_address
         FROM audit_events
         WHERE ($1::text IS NULL OR actor_id ILIKE '%'||$1||'%' OR resource_id ILIKE '%'||$1||'%' OR action_type ILIKE '%'||$1||'%')
           AND ($2::text IS NULL OR actor_type = $2)
           AND ($3::text IS NULL OR status = $3)
           AND ($4::text IS NULL OR resource_type = $4)
           AND ($5::bigint IS NULL OR id < $5)
         ORDER BY id DESC LIMIT $6",
    )
    .bind(q.q)
    .bind(q.actor_type)
    .bind(q.status)
    .bind(q.resource_type)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Paged::new(rows, limit, |r| r.id.to_string())))
}
