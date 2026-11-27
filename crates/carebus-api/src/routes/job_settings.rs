//! `POST /api/internal/job-settings {tenant_id, job}` — the effective job settings a `faq.ask`
//! carries to strata_rag (actor, stance, preamble, max_tokens). Service token only, as the
//! faq-scope route. The values are per-tenant configuration, so the response is `no-store`.

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{Caller, CallerKind};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::routes::settings;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// Absolute path; merged by `routes::all`.
pub fn internal_routes() -> Router<AppState> {
    Router::new().route("/api/internal/job-settings", post(job_settings))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    tenant_id: Uuid,
    job: String,
}

async fn job_settings(
    caller: Caller,
    State(s): State<AppState>,
    ApiJson(i): ApiJson<Input>,
) -> ApiResult<Response> {
    if !matches!(caller.kind, CallerKind::Service) {
        return Err(ApiError::Forbidden("service token"));
    }
    let job = i.job.trim();
    if i.tenant_id.is_nil() || job.is_empty() || job.len() > 63 {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "tenant_id (non-nil uuid) and job (the job key) are required.".into(),
        ));
    }
    let mut tx = begin_scoped(&s.db, i.tenant_id, None).await?;
    let job_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
            .bind(i.tenant_id)
            .bind(job)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(job_id) = job_id else {
        return Err(ApiError::NotFound("job".into()));
    };
    let view = settings::view(&mut tx, job_id).await?;
    let effective = view.get("effective").cloned().unwrap_or_else(|| json!({}));
    drop(tx);
    let settings = json!({
        "preamble": effective.get("preamble").and_then(Value::as_str).unwrap_or_default(),
        "actor": effective.get("actor").and_then(Value::as_str).unwrap_or_default(),
        "stance": effective.get("stance").and_then(Value::as_str).unwrap_or_default(),
        "max_tokens": effective.get("max_tokens").and_then(Value::as_str),
    });
    let mut resp = Json(json!({"settings": settings})).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}
