//! Refinement actions on any overlay object, by natural key (W3 plan 11): `POST /refinements/{revert,rebase,hide,
//! unhide}` with `{"object": "<kind>", "key": {…}}` → 204. Revert deletes the tenant row (back to the base), rebase
//! accepts the current base under it (both resolve the key's open release conflicts), hide writes a tombstone, unhide
//! removes it. `tenant_admin` (or a super_admin in the tenant) only.
use axum::{Router, extract::State, http::StatusCode, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    audit,
    error::{ApiJson, ApiResult},
    refine::{self, ObjectKind},
    state::AppState,
    tenancy::{JobAdmin, JobCtx},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    object: String,
    key: Value,
}

#[derive(Clone, Copy)]
enum Op {
    Revert,
    Rebase,
    Hide,
    Unhide,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Revert => "revert",
            Op::Rebase => "rebase",
            Op::Hide => "hide",
            Op::Unhide => "unhide",
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/refinements/revert", post(revert))
        .route("/refinements/rebase", post(rebase))
        .route("/refinements/hide", post(hide))
        .route("/refinements/unhide", post(unhide))
}

async fn run(op: Op, ctx: JobCtx, s: AppState, b: Body) -> ApiResult<StatusCode> {
    let kind = ObjectKind::parse(&b.object)?;
    let key = kind.key(&b.key)?;
    let actor = ctx.t.caller.actor.clone();
    let mut tx = ctx.begin(&s.db).await?;
    if kind == ObjectKind::JobSetting {
        // the setting lives on the job row itself, which these writes update
        sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(ctx.job_id)
            .execute(&mut *tx)
            .await?;
    } else {
        refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    }
    match op {
        Op::Revert => refine::revert(&mut tx, kind, &key, &actor).await?,
        Op::Rebase => refine::rebase(&mut tx, kind, &key, &actor).await?,
        Op::Hide => refine::hide(&mut tx, kind, &key).await?,
        Op::Unhide => refine::unhide(&mut tx, kind, &key).await?,
    }
    let id = kind
        .key_columns()
        .iter()
        .map(|c| key.get(*c).and_then(Value::as_str).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("/");
    audit::record(
        &mut tx,
        &actor,
        ctx.t.caller.actor_type(),
        op.as_str(),
        kind.as_str(),
        &id,
        true,
        json!({"key": key}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revert(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(b): ApiJson<Body>,
) -> ApiResult<StatusCode> {
    run(Op::Revert, ctx, s, b).await
}

async fn rebase(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(b): ApiJson<Body>,
) -> ApiResult<StatusCode> {
    run(Op::Rebase, ctx, s, b).await
}

async fn hide(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(b): ApiJson<Body>,
) -> ApiResult<StatusCode> {
    run(Op::Hide, ctx, s, b).await
}

async fn unhide(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(b): ApiJson<Body>,
) -> ApiResult<StatusCode> {
    run(Op::Unhide, ctx, s, b).await
}
