//! `POST /api/internal/answer-context {tenant_id, job}` — the effective job settings and the model
//! provider the website answer stage must use. Service token only, as the faq-scope route. The
//! response carries a provider token, so it is `no-store` and nothing here logs it.
//!
//! The provider is the tenant's `answer` binding when one exists, else the deployment's platform
//! default (0033). A tenant provider's host is resolved and checked here, as the qualifier does;
//! the platform row is deployment configuration and deliberately skips that guard.

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{Caller, CallerKind};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::provider_secret;
use crate::routes::settings;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// Absolute path; merged by `routes::all`.
pub fn internal_routes() -> Router<AppState> {
    Router::new().route("/api/internal/answer-context", post(answer_context))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    tenant_id: Uuid,
    job: String,
}

async fn answer_context(
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
    let provider = answer_provider(&s, i.tenant_id).await?;
    let settings = json!({
        "preamble": effective.get("preamble").and_then(Value::as_str).unwrap_or_default(),
        "actor": effective.get("actor").and_then(Value::as_str).unwrap_or_default(),
        "stance": effective.get("stance").and_then(Value::as_str).unwrap_or_default(),
        "max_tokens": effective.get("max_tokens").and_then(Value::as_str),
    });
    let mut resp = Json(json!({"settings": settings, "provider": provider})).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

#[derive(sqlx::FromRow)]
struct BoundProvider {
    id: Uuid,
    base_url: String,
    secret_enc: Option<String>,
    secret_sealed: Option<String>,
    model: Option<String>,
}

async fn answer_provider(s: &AppState, tenant: Uuid) -> ApiResult<Option<Value>> {
    let mut tx = begin_scoped(&s.db, tenant, None).await?;
    let bound: Option<BoundProvider> = sqlx::query_as(
        "SELECT p.id, p.base_url, p.secret_enc, p.secret_sealed, u.model
           FROM llm_provider_uses u
           JOIN llm_providers p ON p.tenant_id = u.tenant_id AND p.id = u.provider_id
          WHERE u.\"use\" = 'answer' AND p.kind = 'openai' AND p.deleted_at IS NULL",
    )
    .fetch_optional(&mut *tx)
    .await?;
    drop(tx);
    if let Some(BoundProvider {
        id,
        base_url,
        secret_enc,
        secret_sealed,
        model,
    }) = bound
    {
        let token = provider_secret::open(
            s.cfg.secrets(),
            tenant,
            id,
            secret_sealed.as_deref(),
            secret_enc.as_deref(),
        )?
        .unwrap_or_default();
        // The stored base_url passed the literal guard on save; resolve and check it here too,
        // exactly as the qualifier's tenant provider is checked.
        crate::netguard::check_and_pin(&base_url, s.cfg.source_fetch_allow_loopback)
            .await
            .map(|_| ())
            .map_err(|r| {
                ApiError::Unprocessable(
                    "invalid_provider",
                    format!("the answer provider's base_url is not callable: {}", r.0),
                )
            })?;
        return Ok(Some(json!({
            "base_url": base_url,
            "model": model.unwrap_or_else(|| carebus::interaction::DEFAULT_MODEL.to_string()),
            "token": token,
        })));
    }
    // The platform default (0033). No row (or a database that predates it) is no provider.
    let row: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT base_url, model, secret_sealed FROM platform_llm_default WHERE id")
            .fetch_optional(&s.db)
            .await
            .or_else(|e| {
                if matches!(&e, sqlx::Error::Database(d) if d.code().as_deref() == Some("42P01")) {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;
    let Some((base_url, model, sealed)) = row else {
        return Ok(None);
    };
    let token =
        provider_secret::open_platform(s.cfg.secrets(), sealed.as_deref())?.unwrap_or_default();
    Ok(Some(json!({
        "base_url": base_url,
        "model": model,
        "token": token,
    })))
}
