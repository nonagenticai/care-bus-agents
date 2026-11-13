//! Releases, read-only: care-bus-agents is a standalone demo whose base is seeded by migrations, so nothing pushes
//! a release any more. Tenant members read the current (bootstrap) release (`GET …/releases/current`) and its
//! conflicts (`GET …/release-conflicts`). Paths are relative to `/api/jobs/{job}`.
use axum::extract::State;
use axum::{Json, Router, routing::get};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::release::report::{self, ReleaseReport};
use crate::state::AppState;
use crate::tenancy::JobMember;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/releases/{release_id}", get(get_release))
        .route("/release-conflicts", get(list_conflicts))
}

/// `{release_id}` under the nested `/api/jobs/{job}`.
#[derive(Deserialize)]
pub struct ReleasePath {
    pub job: String,
    pub release_id: String,
}

/// `GET …/releases/current`: the current release with its open conflicts (`applied: true`); 404 `release` for a
/// job that never had one. The bootstrap release reports `hash = "sha256:bootstrap-0009"`.
pub async fn get_release(
    JobMember(ctx): JobMember,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<ReleasePath>,
) -> ApiResult<Json<ReleaseReport>> {
    if p.release_id != "current" {
        return Err(ApiError::NotFound("release".into()));
    }
    let mut tx = ctx.begin(&st.db).await?;
    let current = report::current_report(&mut tx)
        .await?
        .ok_or_else(|| ApiError::NotFound("release".into()))?;
    Ok(Json(current))
}

#[derive(Deserialize)]
pub struct ConflictsQuery {
    status: Option<String>,
}

/// A conflict with its lifecycle.
#[derive(Serialize, sqlx::FromRow)]
pub struct ConflictItem {
    pub id: i64,
    pub kind: String,
    pub object: String,
    pub key: Value,
    pub tenant_row: String,
    pub base_hash_before: Option<String>,
    pub base_hash_after: Option<String>,
    pub release_id: String,
    pub detected_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolution: Option<String>,
    pub resolved_by: Option<String>,
}

#[derive(Serialize)]
pub struct ConflictList {
    pub items: Vec<ConflictItem>,
    pub next: Option<String>,
}

/// `GET …/release-conflicts?status=open|all` (default `open`), newest first; never paged (`next` is null).
pub async fn list_conflicts(
    JobMember(ctx): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ConflictsQuery>,
) -> ApiResult<Json<ConflictList>> {
    let open_only = match q.status.as_deref() {
        None | Some("open") => true,
        Some("all") => false,
        Some(_) => {
            return Err(ApiError::BadRequest(
                "invalid_query",
                "status must be open or all.".into(),
            ));
        }
    };
    let mut tx = ctx.begin(&st.db).await?;
    let items: Vec<ConflictItem> = sqlx::query_as(
        "SELECT id, kind, object, key, tenant_row, base_hash_before, base_hash_after, release_id, detected_at,
                resolved_at, resolution, resolved_by
           FROM release_conflicts
          WHERE NOT $1 OR resolved_at IS NULL
          ORDER BY id DESC",
    )
    .bind(open_only)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(ConflictList { items, next: None }))
}
