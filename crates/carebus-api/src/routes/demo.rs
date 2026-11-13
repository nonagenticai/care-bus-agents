//! demo-backends proxy (the demo-data UI). Response shapes pass through as demo-backends defines them.
//!
//! demo-backends is ONE shared fake enterprise for every tenant, so reads need a job member but writes (record
//! edit, reset, scenarios, faults) need a super_admin acting in the tenant: a tenant admin must not reset the demo
//! data of every tenant. Writes are audited in the job's scope.
use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use axum::{
    Json, Router,
    extract::{RawQuery, State},
    routing::{get, post},
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::audit;
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, SuperAdminIn};
use crate::upstream::{self, segment};

const NAME: &str = "demo-backends";

/// Relative paths; nested under `/api/jobs/{job}` by `routes::all`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/demo/records/{table}", get(records))
        .route("/demo/records/{table}/{id}", get(record).put(record_put))
        .route("/demo/reset", post(reset))
        .route("/demo/scenarios", get(scenarios))
        .route("/demo/scenarios/{name}", post(apply))
        .route("/demo/faults", get(faults).put(faults_put))
}

#[derive(Deserialize)]
struct TablePath {
    table: String,
}

#[derive(Deserialize)]
struct RecordPath {
    table: String,
    id: String,
}

#[derive(Deserialize)]
struct NamePath {
    name: String,
}

/// One audit row in the job's scope.
async fn audited(
    s: &AppState,
    ctx: &JobCtx,
    action: &str,
    id: &str,
    ok: bool,
    details: Value,
) -> ApiResult<()> {
    let mut tx = ctx.begin(&s.db).await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        action,
        "demo",
        id,
        ok,
        details,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Forward to demo-backends; an upstream 4xx keeps its status and body (validation messages reach the UI).
async fn demo(
    s: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> ApiResult<Response> {
    let r = upstream::call(
        &s.http,
        NAME,
        s.cfg.demo_backends_url.as_deref(),
        upstream::Auth {
            token: s.cfg.demo_backends_token.as_deref(),
            tenant: None, // demo-backends is not a strata service
            ..upstream::Auth::default()
        },
        method,
        path,
        body,
    )
    .await?;
    if r.body.is_null() && r.status.is_client_error() {
        // A non-JSON 4xx body: our envelope, not a bare null.
        return Ok((
            r.status,
            Json(json!({"error": {
                "code": "upstream_rejected",
                "message": format!("{NAME} rejected the request (HTTP {}).", r.status.as_u16()),
            }})),
        )
            .into_response());
    }
    Ok((r.status, Json(r.body)).into_response())
}

/// A demo write: forwarded, then audited. When the upstream is unavailable the outcome is unknown
/// (a reset may have run), so a `failure` row is still written before the error is returned.
#[allow(clippy::too_many_arguments)]
async fn write(
    s: &AppState,
    ctx: &JobCtx,
    action: &str,
    id: &str,
    mut details: Value,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> ApiResult<Response> {
    match demo(s, method, path, body).await {
        Ok(r) => {
            audited(s, ctx, action, id, r.status().is_success(), details).await?;
            Ok(r)
        }
        Err(e) => {
            if let Some(o) = details.as_object_mut() {
                o.insert("upstream".into(), json!("unavailable"));
            }
            if let Err(ae) = audited(s, ctx, action, id, false, details).await {
                // The upstream error is the one the caller needs; do not let the audit failure replace it.
                tracing::error!(action, id, error = %ae, "could not audit a failed demo write");
            }
            Err(e)
        }
    }
}

async fn records(
    _: JobMember,
    State(s): State<AppState>,
    ApiPath(TablePath { table: t }): ApiPath<TablePath>,
    RawQuery(q): RawQuery,
) -> ApiResult<Response> {
    let qs = q.map(|q| format!("?{q}")).unwrap_or_default();
    demo(
        &s,
        Method::GET,
        &format!("/api/admin/records/{}{qs}", segment(&t, false)?),
        None,
    )
    .await
}

async fn record(
    _: JobMember,
    State(s): State<AppState>,
    ApiPath(RecordPath { table: t, id }): ApiPath<RecordPath>,
) -> ApiResult<Response> {
    let path = format!(
        "/api/admin/records/{}/{}",
        segment(&t, false)?,
        segment(&id, true)?
    );
    demo(&s, Method::GET, &path, None).await
}

async fn record_put(
    _: SuperAdminIn,
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(RecordPath { table: t, id }): ApiPath<RecordPath>,
    ApiJson(b): ApiJson<Value>,
) -> ApiResult<Response> {
    let path = format!(
        "/api/admin/records/{}/{}",
        segment(&t, false)?,
        segment(&id, true)?
    );
    let fields: Vec<&String> = b
        .as_object()
        .map(|o| o.keys().collect())
        .unwrap_or_default();
    let details = json!({"table": t, "fields": fields});
    write(
        &s,
        &ctx,
        "update_record",
        &format!("{t}/{id}"),
        details,
        Method::PUT,
        &path,
        Some(b),
    )
    .await
}

/// Body optional: a body-less POST resets with demo-backends' default seed.
async fn reset(
    _: SuperAdminIn,
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    body: Bytes,
) -> ApiResult<Response> {
    let b = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        Some(serde_json::from_slice::<Value>(&body).map_err(|_| {
            ApiError::BadRequest("invalid_body", "The request body is not valid JSON.".into())
        })?)
    };
    let seed = b.as_ref().and_then(|v| v.get("seed")).cloned();
    write(
        &s,
        &ctx,
        "reset",
        "all",
        json!({"seed": seed}),
        Method::POST,
        "/api/admin/reset",
        b,
    )
    .await
}

async fn scenarios(_: JobMember, State(s): State<AppState>) -> ApiResult<Response> {
    demo(&s, Method::GET, "/api/admin/scenarios", None).await
}

async fn apply(
    _: SuperAdminIn,
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(NamePath { name }): ApiPath<NamePath>,
) -> ApiResult<Response> {
    let path = format!("/api/admin/scenarios/{}", segment(&name, false)?);
    write(
        &s,
        &ctx,
        "apply_scenario",
        &name,
        json!({}),
        Method::POST,
        &path,
        Some(json!({})),
    )
    .await
}

async fn faults(_: JobMember, State(s): State<AppState>) -> ApiResult<Response> {
    demo(&s, Method::GET, "/api/admin/faults", None).await
}

async fn faults_put(
    _: SuperAdminIn,
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiJson(b): ApiJson<Value>,
) -> ApiResult<Response> {
    let details = json!({"tool": b.get("tool"), "mode": b.get("mode"), "count": b.get("count")});
    write(
        &s,
        &ctx,
        "set_faults",
        "faults",
        details,
        Method::PUT,
        "/api/admin/faults",
        Some(b),
    )
    .await
}
