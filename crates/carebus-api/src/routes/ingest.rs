//! `/api/ingest/jobs`: strata_ingest's jobs for the technical page. `before` (a job id) and `next` are
//! strata_ingest's own cursor, passed through; `limit` is clamped to 1..=200 (its maximum).
//!
//! Retrying a failed api/datatable job is a NEW re-sync of its care-bus source: strata_ingest keeps no
//! credentials, so its own retry of such a job is a 409.
//!
//! Cancel is `POST /api/ingest/jobs/{id}/cancel` (a running job goes `cancelling`, then `cancelled`); `DELETE
//! /api/ingest/jobs/{id}` deletes an ENDED job (strata_ingest PR #11), leaving its documents in strata_rag. A job that
//! a care-bus source run still follows (directly, or as a crawl/sitemap page job) is not deleted: 409
//! `owned_by_active_run` with `run_id`, so the run's outcome is not decided by a job vanishing under it. A page job no
//! running run follows is strata_ingest's 409 `child_of_parent_job` with `parent_job_id` (delete the parent, which
//! cascades). The delete may be 202 `{job_id, status: "deleting"}` (strata_ingest finishes it in the background).
//!
//! A job into a store walled for a matter the caller is not a member of (crate::walls) is left out of the list and
//! 404 for every other route: read, cancel, retry, delete.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit;
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::ingest::{self, IngestJob, JobDelete};
use crate::page::Paged;
use crate::state::AppState;
use crate::tenancy::{Member, TenantAdmin, TenantCtx};
use crate::{runs, sources, walls};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ingest/jobs", get(list))
        .route("/api/ingest/jobs/{id}", get(one).delete(delete_job))
        .route("/api/ingest/jobs/{id}/cancel", post(cancel))
        .route("/api/ingest/jobs/{id}/retry", post(retry))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    store_id: Option<Uuid>,
    status: Option<String>,
    limit: Option<i64>,
    before: Option<Uuid>,
}

async fn list(
    Member(ctx): Member,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Paged<IngestJob>>> {
    if let Some(st) = &q.status
        && !ingest::JOB_STATUSES.contains(&st.as_str())
    {
        return Err(ApiError::BadRequest(
            "invalid_status",
            format!("status must be one of {}.", ingest::JOB_STATUSES.join(", ")),
        ));
    }
    let mut path = format!(
        "/api/ingest/jobs?limit={}",
        q.limit.unwrap_or(50).clamp(1, 200)
    );
    if let Some(id) = q.store_id {
        path.push_str(&format!("&store_id={id}"));
    }
    if let Some(st) = &q.status {
        path.push_str(&format!("&status={st}"));
    }
    if let Some(b) = q.before {
        path.push_str(&format!("&before={b}"));
    }
    // A job into a store walled for a matter the caller is not a member of is not theirs to see (crate::walls).
    let (hidden, _) = walls::hidden(&s, &ctx).await?;
    if let Some(id) = q.store_id
        && hidden.contains(&id.to_string())
    {
        return Err(ApiError::NotFound(format!("store {id}")));
    }
    let (mut items, next) = ingest::list_jobs(&s, ctx.tenant, &path).await?;
    items.retain(|j| !hidden.contains(&walls::canonical(&j.store_id)));
    ingest::scrub_jobs(&s, &ctx, &mut items).await;
    Ok(Json(Paged { items, next }))
}

async fn one(
    Member(ctx): Member,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<IngestJob>> {
    let job = ingest::get_job(&s, ctx.tenant, &id.to_string())
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("ingest job {id}")))?;
    let (hidden, _) = walls::hidden(&s, &ctx).await?;
    if hidden.contains(&walls::canonical(&job.store_id)) {
        return Err(ApiError::NotFound(format!("ingest job {id}")));
    }
    Ok(Json(scrubbed(&s, &ctx, job).await))
}

/// 404 for a job into a store walled for a matter the caller is not a member of (crate::walls), as `one` answers.
/// The job is looked up only when such a wall exists; a failed lookup then refuses (never acts unchecked).
async fn guard_job(s: &AppState, ctx: &TenantCtx, id: Uuid) -> ApiResult<()> {
    let (hidden, _) = walls::hidden(s, ctx).await?;
    if hidden.is_empty() {
        return Ok(());
    }
    let job = ingest::get_job(s, ctx.tenant, &id.to_string())
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("ingest job {id}")))?;
    if hidden.contains(&walls::canonical(&job.store_id)) {
        return Err(ApiError::NotFound(format!("ingest job {id}")));
    }
    Ok(())
}

/// One audit row in the request's tenant (job GUC empty: ingest jobs are tenant-level).
async fn audit_job(
    s: &AppState,
    ctx: &TenantCtx,
    action: &str,
    id: Uuid,
    ok: bool,
    details: serde_json::Value,
) -> ApiResult<()> {
    let mut tx = ctx.begin(&s.db).await?;
    audit::record(
        &mut tx,
        &ctx.caller.actor,
        ctx.caller.actor_type(),
        action,
        "ingest_job",
        &id.to_string(),
        ok,
        details,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn act(
    s: &AppState,
    ctx: &TenantCtx,
    action: &str,
    method: Method,
    id: Uuid,
    suffix: &str,
) -> ApiResult<Json<IngestJob>> {
    let res = ingest::job_action(s, ctx.tenant, method, &id.to_string(), suffix).await;
    let details = match &res {
        Ok(j) => json!({"status": j.status}),
        Err(e) => {
            let (status, code) = e.status_and_code();
            json!({"error_code": code, "http_status": status.as_u16()})
        }
    };
    audit_job(s, ctx, action, id, res.is_ok(), details).await?;
    Ok(Json(scrubbed(s, ctx, res?).await))
}

async fn scrubbed(s: &AppState, ctx: &TenantCtx, job: IngestJob) -> IngestJob {
    let mut one = [job];
    ingest::scrub_jobs(s, ctx, &mut one).await;
    let [job] = one;
    job
}

async fn retry(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<IngestJob>> {
    ingest::provisioned_for(&s, ctx.tenant).await?;
    guard_job(&s, &ctx, id).await?;
    // A lookup that fails falls through: strata_ingest's own answer (and its audit row) decides.
    if let Ok(Some(job)) = ingest::get_job(&s, ctx.tenant, &id.to_string()).await
        && job.status == "failed"
        && job.is_structured_source()
    {
        return resubmit(&s, &ctx, id, &job).await;
    }
    act(&s, &ctx, "retry", Method::POST, id, "/retry").await
}

/// A failed api/datatable job re-submitted as a new re-sync of its source (with the credentials carebus-api
/// holds): the new job's view.
async fn resubmit(
    s: &AppState,
    ctx: &TenantCtx,
    id: Uuid,
    job: &IngestJob,
) -> ApiResult<Json<IngestJob>> {
    let res = resubmitted(s, ctx, job).await;
    let details = match &res {
        Ok((run, new)) => {
            json!({"status": new.status, "resubmitted_as": new.id, "run_id": run})
        }
        Err(e) => {
            let (status, code) = e.status_and_code();
            json!({"error_code": code, "http_status": status.as_u16(), "resubmit": true})
        }
    };
    audit_job(s, ctx, "retry", id, res.is_ok(), details).await?;
    Ok(Json(scrubbed(s, ctx, res?.1).await))
}

async fn resubmitted(
    s: &AppState,
    ctx: &TenantCtx,
    job: &IngestJob,
) -> ApiResult<(Uuid, IngestJob)> {
    let gone = || {
        ApiError::Conflict(
            "ingest_conflict",
            "strata_ingest keeps no credentials, so this source job can only be re-submitted from its care-bus \
             source, which no longer exists. Create the source again and re-sync it."
                .into(),
        )
    };
    let source = job.source_id().ok_or_else(gone)?;
    // The source's own job: the re-sync runs (and is recorded) in it.
    let jctx = ingest::source_job(s, ctx, source).await?.ok_or_else(gone)?;
    let src = match sources::record(s, &jctx, source).await {
        Ok(r) => r,
        Err(ApiError::NotFound(_)) => return Err(gone()),
        Err(e) => return Err(e),
    };
    let (_, run) = runs::start(s, &jctx, &src, &ctx.caller.actor, Vec::new()).await?;
    let Some(new_id) = run.ingest_job_ids.first() else {
        return Err(ApiError::Conflict(
            "ingest_conflict",
            format!(
                "The re-sync was refused: {}",
                run.error.as_deref().unwrap_or("no job was created")
            ),
        ));
    };
    let new = ingest::get_job(s, ctx.tenant, new_id)
        .await?
        .ok_or(ApiError::Upstream(ingest::NAME))?;
    Ok((run.id, new))
}

async fn cancel(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<IngestJob>> {
    ingest::provisioned_for(&s, ctx.tenant).await?;
    guard_job(&s, &ctx, id).await?;
    act(&s, &ctx, "cancel", Method::POST, id, "/cancel").await
}

/// The care-bus source run that follows job `id` (one of its jobs, or a page job of one) and is still running once
/// brought up to date. A finished run is never changed by a delete (its outcome is stored), so it does not count.
async fn active_owner(s: &AppState, ctx: &TenantCtx, id: &str) -> ApiResult<Option<Uuid>> {
    let Some(job) = ingest::get_job(s, ctx.tenant, id).await? else {
        return Ok(None);
    };
    let Some(source) = job.source_id() else {
        return Ok(None);
    };
    let Some(jctx) = ingest::source_job(s, ctx, source).await? else {
        return Ok(None);
    };
    let Some(run_id) = runs::running_of(s, &jctx, source).await? else {
        return Ok(None);
    };
    let run = runs::refresh(s, &jctx, run_id).await?;
    if run.status != "running" {
        return Ok(None);
    }
    if run.ingest_job_ids.iter().any(|j| j == id) {
        return Ok(Some(run.id));
    }
    for parent in &run.ingest_job_ids {
        if let Some(p) = ingest::get_job(s, ctx.tenant, parent).await?
            && p.child_job_ids.iter().any(|c| c == id)
        {
            return Ok(Some(run.id));
        }
    }
    Ok(None)
}

/// An ended job and its staged files in strata_ingest. Its documents stay searchable in strata_rag: the answer lists
/// them (`document_ids`, `documents: "retained"`). 202 `{job_id, status: "deleting"}` when strata_ingest finishes the
/// delete in the background: the job reads `deleting` until it is gone (404).
async fn delete_job(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    ingest::provisioned_for(&s, ctx.tenant).await?;
    guard_job(&s, &ctx, id).await?;
    let res = match active_owner(&s, &ctx, &id.to_string()).await {
        Ok(Some(run)) => Err(ApiError::ConflictWith(
            "owned_by_active_run",
            format!(
                "This job belongs to source run {run}, which is still running. Delete it once that run has finished."
            ),
            json!({"run_id": run}),
        )),
        Ok(None) => ingest::delete_job(&s, ctx.tenant, &id.to_string()).await,
        Err(e) => Err(e),
    };
    let details = match &res {
        Ok(JobDelete::Done(d)) => json!({"status": d.status, "deleted_jobs": d.deleted_jobs,
                                         "documents_retained": d.document_ids.len()}),
        Ok(JobDelete::Continuing) => json!({"status": "deleting", "accepted": true}),
        Err(e) => {
            let (status, code) = e.status_and_code();
            json!({"error_code": code, "http_status": status.as_u16()})
        }
    };
    audit_job(&s, &ctx, "delete", id, res.is_ok(), details).await?;
    Ok(match res? {
        JobDelete::Done(d) => (StatusCode::OK, Json(json!(d))),
        JobDelete::Continuing => (
            StatusCode::ACCEPTED,
            Json(json!({"job_id": id, "status": "deleting"})),
        ),
    })
}
