//! Job settings overlay (W3 plan 11): `jobs.settings` is the base (the release's `job_settings`, or what
//! provisioning wrote), `jobs.settings_override` the tenant's refinement. `PUT /settings` sets a key to a string
//! (override) or to null (hide the base value) and leaves absent keys unchanged; `DELETE /settings/{key}` reverts a
//! key to the base; `GET /settings` → `{"base", "override", "effective"}`.
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{delete, get},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    audit,
    error::{ApiError, ApiJson, ApiPath, ApiResult},
    refine::{self, JOB_SETTINGS, ObjectKind},
    secrets,
    state::AppState,
    tenancy::{JobAdmin, JobMember},
};

#[derive(Deserialize)]
struct SettingPath {
    key: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings", get(read).put(write))
        .route("/settings/{key}", delete(revert))
}

/// Base, override and the effective settings (`value: null` in the override removes the base value).
async fn view(conn: &mut PgConnection, job_id: Uuid) -> ApiResult<Value> {
    let (base, over): (Value, Value) =
        sqlx::query_as("SELECT settings, settings_override FROM jobs WHERE id = $1")
            .bind(job_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| ApiError::NotFound("job".into()))?;
    let mut effective: Map<String, Value> = base.as_object().cloned().unwrap_or_default();
    for (k, o) in over.as_object().into_iter().flatten() {
        match o.get("value") {
            Some(Value::String(v)) => {
                effective.insert(k.clone(), Value::String(v.clone()));
            }
            _ => {
                effective.remove(k);
            }
        }
    }
    Ok(json!({"base": base, "override": over, "effective": effective}))
}

async fn read(JobMember(ctx): JobMember, State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&s.db).await?;
    Ok(Json(view(&mut tx, ctx.job_id).await?))
}

fn setting_key(k: &str) -> ApiResult<()> {
    if JOB_SETTINGS.contains(&k) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "invalid_setting",
            format!("a job setting is one of {}.", JOB_SETTINGS.join(", ")),
        ))
    }
}

async fn lock_job(conn: &mut PgConnection, job_id: Uuid) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job_id)
        .fetch_optional(&mut *conn)
        .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::NotFound("job".into()))
}

async fn write(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let Some(obj) = raw.as_object() else {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "The request body must be a JSON object.".into(),
        ));
    };
    secrets::reject_secret_fields(&raw, &[])?;
    for (k, v) in obj {
        setting_key(k)?;
        match v {
            Value::Null => {}
            Value::String(t) if !t.trim().is_empty() && t.chars().count() <= 4000 => {}
            _ => {
                return Err(ApiError::BadRequest(
                    "invalid_body",
                    format!(
                        "{k} must be a string of 1 to 4000 characters, or null to hide the base value."
                    ),
                ));
            }
        }
    }
    let mut tx = ctx.begin(&s.db).await?;
    // FOR UPDATE, not the refinements' FOR SHARE: this write updates the job row itself.
    lock_job(&mut tx, ctx.job_id).await?;
    for (k, v) in obj {
        refine::set_setting(&mut tx, k, v.as_str()).await?;
        refine::resolve_conflicts(
            &mut tx,
            ObjectKind::JobSetting,
            &ObjectKind::JobSetting.key_of(&[k.as_str()]),
            "rebased",
            &ctx.t.caller.actor,
        )
        .await?;
    }
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "update",
        "job_settings",
        &ctx.job_key,
        true,
        json!({"refinement": "override", "keys": obj.keys().collect::<Vec<_>>()}),
    )
    .await?;
    let out = view(&mut tx, ctx.job_id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn revert(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(SettingPath { key }): ApiPath<SettingPath>,
) -> ApiResult<StatusCode> {
    setting_key(&key)?;
    let mut tx = ctx.begin(&s.db).await?;
    lock_job(&mut tx, ctx.job_id).await?;
    refine::revert(
        &mut tx,
        ObjectKind::JobSetting,
        &ObjectKind::JobSetting.key_of(&[key.as_str()]),
        &ctx.t.caller.actor,
    )
    .await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "revert",
        "job_settings",
        &ctx.job_key,
        true,
        json!({"key": key}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
