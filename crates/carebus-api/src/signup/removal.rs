//! Early-access REMOVAL: undo an APPROVED signup end to end (Studio w2v_orchestrator `signup/removal.py`,
//! CommonPractice #476/#477). The reject repair (crate::signup::provisioning) only cleans up after an ABORTED approve;
//! this removes a fully approved workspace, which until now had to be done by hand.
//!
//! THE ACTION ([`start`], `POST /api/signup-approvals/{id}/remove` `{confirm}`, super_admin): only an `approved` row
//! whose workspace is the one its own approve founded, whose provisioning steps are not being run right now, and only
//! with the workspace slug typed back as `confirm` ([`tenant_slug`]: the licences registry name, shown on the signup
//! as `tenant_slug`). In ONE transaction the row becomes `status = 'removed'`, `removal_state = 'pending'` (audited
//! `signup.remove`), its five steps are journaled `pending`, and then the steps run inline. From that moment the
//! provisioning tick ignores the row (it only runs `approved` ones), so nothing re-licenses the workspace mid-removal.
//!
//! THE STEPS, in order ([`STEPS`]), journaled in `signup_removal_steps` (migration 0028), each audited
//! `signup.remove.<step>` in the workspace's audit trail:
//! 1. `suspend` — the tenant `suspended` (`updated_by = signup:<id>:removed`), its scopes out of the registry: the
//!    workspace is not served any more (audited `tenant.offboard`).
//! 2. `members` — every `users` row of the tenant deleted (their `member_licences` rows go with them; audited
//!    `user.delete` each), so no login can be re-licensed on login.
//! 3. `licences` — `GET /licenses` for the tenant; every live licence that is not a shared dependency licence is
//!    revoked (member licences and the bundle licence; licences releases the dependency licences — mcp, strata-rag,
//!    strata-ingest — nothing requires any more); a fresh listing must hold no live own licence (else retried). A
//!    dependency licence still live afterwards gets one explicit revoke; one licences refuses is recorded as
//!    `kept_dependencies` (the archive below stops it authorizing). A listing that names another tenant's licence
//!    BLOCKS with zero writes.
//! 4. `identity` — the care-bus realm login this signup minted (the `keycloak_user` journal) is DELETED, but only when
//!    it carries this signup's stamp (`carebus_signup_id`); an unstamped one BLOCKS (`identity_not_owned`), one
//!    seated in another workspace is kept (`skipped`, `kept_in_use`), one already gone is ok (`absent`).
//! 5. `licences_tenant` — `PATCH /tenants/{id}/archive` in licences (404: never registered, `skipped`).
//!
//! Then `removal_state = 'done'`: with the login deleted the address can sign up again.
//!
//! FAILURE AND RETRY: one runner at a time (`removal_lease_until` + `removal_lease_token`, renewed before every step
//! and every licences revoke, released only by its owner). A transient failure
//! (transport, 5xx, 408, 429) leaves the step `pending` with the provisioning backoff and the tick runs it again (at
//! most [`MAX_ATTEMPTS`]); a refusal is `failed` (`removal_state = 'failed'`), a guard is `blocked`
//! (`removal_state = 'blocked'`); both stop the chain and stay visible until a super_admin's Retry ([`retry`],
//! `POST /api/signup-approvals/{id}/removal/retry`) sets them `pending` and runs again. Every step is idempotent, so
//! a re-run after a partial failure (or of a workspace someone already cleaned up by hand) only finishes what is left.
use carebus::Scope;
use carebus::scopes;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit;
use crate::error::{ApiError, ApiResult};
use crate::signup::Signup;
use crate::signup::licences::registry_name;
use crate::signup::provisioning::{
    MAX_ATTEMPTS, REJECTED_PREFIX, STAMP_ATTR, TICK_ACTOR, backoff_secs, founded_tenant, identity,
};
use crate::signup::store::{self, Approval, COLUMNS};
use crate::signup::upstream::UpstreamError;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

pub const STEPS: [&str; 5] = [
    "suspend",
    "members",
    "licences",
    "identity",
    "licences_tenant",
];
const LEASE_SECS: f64 = 300.0;
pub const TENANT_MISMATCH: &str = "tenant_mismatch";
pub const IDENTITY_NOT_OWNED: &str = "identity_not_owned";

/// Who runs the removal: the super_admin (request) or the tick, as the audit trail records it.
#[derive(Clone, Copy)]
pub struct Actor<'a> {
    pub id: &'a str,
    pub kind: &'static str,
}

pub const TICK: Actor<'static> = Actor {
    id: TICK_ACTOR,
    kind: "system",
};

/// The workspace slug a removal is confirmed with: its licences registry name (name as a slug + the id's last 8 hex).
pub fn tenant_slug(name: &str, tenant: Uuid) -> String {
    registry_name(name, tenant)
}

enum Outcome {
    Ok(Value),
    /// Nothing to do, and that is not a failure.
    Skip(&'static str, String, Value),
    Retry(&'static str, String),
    Fail(&'static str, String),
    /// A guard refused: nothing was touched by this step; an operator looks before Retry.
    Block(&'static str, String),
}

fn upstream(code: &'static str, e: &UpstreamError) -> Outcome {
    let msg: String = e.message.chars().take(400).collect();
    match e.status {
        None => Outcome::Retry(code, msg),
        Some(s) if s >= 500 || s == 408 || s == 429 => Outcome::Retry(code, msg),
        Some(_) => Outcome::Fail(code, msg),
    }
}

fn not_found(id: &str) -> ApiError {
    ApiError::Detailed(
        axum::http::StatusCode::NOT_FOUND,
        "signup_not_found",
        format!("no signup {id:?}"),
        json!({}),
    )
}

/// The workspace's current name, read inside its own context; `None` when the row is gone.
async fn tenant_name(st: &AppState, tenant: Uuid) -> ApiResult<Option<String>> {
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(name)
}

/// The slug of an approved / removed signup's workspace (shown on the signup so the screen can ask for it).
pub async fn slug_of(st: &AppState, a: &Approval) -> ApiResult<Option<String>> {
    let Some(t) = a
        .tenant_id
        .filter(|_| a.status == "approved" || a.status == "removed")
    else {
        return Ok(None);
    };
    Ok(tenant_name(st, t).await?.map(|n| tenant_slug(&n, t)))
}

/// Start removing the APPROVED signup `id`, then run its steps. Refusals: 404 `signup_not_found`, 409
/// `signup_not_approved` | `removal_exists` | `signup_no_workspace` | `tenant_mismatch` | `provisioning_in_progress`,
/// 422 `removal_confirm_mismatch`.
pub async fn start(
    st: &AppState,
    sg: &Signup,
    id: Uuid,
    confirm: &str,
    actor: Actor<'_>,
) -> ApiResult<()> {
    let mut tx = st.db.begin().await?;
    let a = store::get(&mut tx, id)
        .await?
        .ok_or_else(|| not_found(&id.to_string()))?;
    tx.commit().await?;
    let tenant = admissible(&a)?;
    // Everything again under the row lock, in the workspace's context (the audit row is its data).
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let a = store::lock(&mut tx, id)
        .await?
        .ok_or_else(|| not_found(&id.to_string()))?;
    if admissible(&a)? != tenant {
        return Err(ApiError::Conflict(
            "signup_not_approved",
            "this signup changed while the removal started; reload it".into(),
        ));
    }
    let rows = store::steps(&mut tx, id).await?;
    if founded_tenant(&rows) != Some(tenant) {
        return Err(ApiError::Conflict(
            TENANT_MISMATCH,
            "the signup's workspace is not the one its approve founded; remove it by hand".into(),
        ));
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM signup_provisioning_steps WHERE signup_id = $1 AND lease_until > now())",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(ApiError::Conflict(
            "provisioning_in_progress",
            "a provisioning step of this signup is running right now; try again in a few minutes"
                .into(),
        ));
    }
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(name) = name else {
        return Err(ApiError::Conflict(
            "signup_no_workspace",
            "the signup's workspace no longer exists".into(),
        ));
    };
    let slug = tenant_slug(&name, tenant);
    if confirm.trim() != slug {
        return Err(ApiError::Unprocessable(
            "removal_confirm_mismatch",
            "confirm must be the workspace slug (tenant_slug on the signup)".into(),
        ));
    }
    sqlx::query(
        "UPDATE signup_approvals \
            SET status = 'removed', removal_state = 'pending', removal_requested_by = $2, \
                removal_requested_at = now(), removal_next_at = now(), removal_lease_until = NULL, \
                removal_lease_token = NULL \
          WHERE id = $1 AND status = 'approved'",
    )
    .bind(id)
    .bind(actor.id)
    .execute(&mut *tx)
    .await?;
    for (i, step) in STEPS.iter().enumerate() {
        sqlx::query(
            "INSERT INTO signup_removal_steps (signup_id, step, ord) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(*step)
        .bind(i16::try_from(i + 1).unwrap_or(i16::MAX))
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        actor.id,
        actor.kind,
        "signup.remove",
        "signup",
        &id.to_string(),
        true,
        json!({"tenant": tenant, "slug": slug}),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(signup = %id, %tenant, "signup_removal_started");
    run(st, sg, id, actor).await
}

/// The workspace of a signup a removal may start on.
fn admissible(a: &Approval) -> ApiResult<Uuid> {
    match a.status.as_str() {
        "approved" => {}
        "removed" => {
            return Err(ApiError::Conflict(
                "removal_exists",
                "this signup is being removed or was removed already; use Retry on its removal"
                    .into(),
            ));
        }
        other => {
            return Err(ApiError::Conflict(
                "signup_not_approved",
                format!("only an approved signup can be removed; this one is {other}"),
            ));
        }
    }
    a.tenant_id.ok_or_else(|| {
        ApiError::Conflict(
            "signup_no_workspace",
            "the signup's workspace no longer exists".into(),
        )
    })
}

/// The super_admin Retry of a removal that is `pending`, `blocked` or `failed`: its failed and blocked steps go back
/// to `pending` and the run starts now. 409 `signup_not_removed` | `removal_done` | `removal_in_progress`.
pub async fn retry(st: &AppState, sg: &Signup, id: Uuid, actor: Actor<'_>) -> ApiResult<()> {
    let mut tx = st.db.begin().await?;
    let a = store::get(&mut tx, id)
        .await?
        .ok_or_else(|| not_found(&id.to_string()))?;
    tx.commit().await?;
    let tenant = retryable(&a)?;
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let a = store::lock(&mut tx, id)
        .await?
        .ok_or_else(|| not_found(&id.to_string()))?;
    retryable(&a)?;
    let leased: bool = sqlx::query_scalar(
        "SELECT removal_lease_until IS NOT NULL AND removal_lease_until > now() FROM signup_approvals WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if leased {
        return Err(ApiError::Conflict(
            "removal_in_progress",
            "this removal is running right now; reload in a moment".into(),
        ));
    }
    let requeued: Vec<String> = sqlx::query_scalar(
        "UPDATE signup_removal_steps \
            SET status = 'pending', attempts = 0, code = NULL, message = NULL, updated_at = now() \
          WHERE signup_id = $1 AND status IN ('failed', 'blocked') RETURNING step",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE signup_approvals SET removal_state = 'pending', removal_next_at = now() \
          WHERE id = $1 AND status = 'removed'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor.id,
        actor.kind,
        "signup.remove.retry",
        "signup",
        &id.to_string(),
        true,
        json!({"tenant": tenant, "requeued": requeued, "from_state": a.removal_state}),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(signup = %id, steps = ?requeued, "signup_removal_retry");
    run(st, sg, id, actor).await
}

fn retryable(a: &Approval) -> ApiResult<Uuid> {
    if a.status != "removed" {
        return Err(ApiError::Conflict(
            "signup_not_removed",
            format!("only a removal can be retried; this signup is {}", a.status),
        ));
    }
    if a.removal_state.as_deref() == Some("done") {
        return Err(ApiError::Conflict(
            "removal_done",
            "this removal is finished; there is nothing to retry".into(),
        ));
    }
    a.tenant_id.ok_or_else(|| {
        ApiError::Conflict(
            "signup_no_workspace",
            "the signup's workspace no longer exists".into(),
        )
    })
}

/// This runner's hold on a removal: the row's `removal_lease_token` while `removal_lease_until` is in the future.
pub(crate) struct Lease {
    id: Uuid,
    token: Uuid,
}

const LEASE_LOST: &str = "removal_lease_lost";

impl Lease {
    /// Extend the lease; 409 `removal_lease_lost` when it is no longer ours (lapsed, or taken by another runner).
    async fn hold(&self, st: &AppState) -> ApiResult<()> {
        let held = sqlx::query(
            "UPDATE signup_approvals SET removal_lease_until = now() + make_interval(secs => $3) \
              WHERE id = $1 AND removal_lease_token = $2 AND removal_lease_until > now()",
        )
        .bind(self.id)
        .bind(self.token)
        .bind(LEASE_SECS)
        .execute(&st.db)
        .await?
        .rows_affected()
            == 1;
        if held {
            Ok(())
        } else {
            Err(ApiError::Conflict(
                LEASE_LOST,
                "this run no longer holds the removal".into(),
            ))
        }
    }

    /// Release it, only while it is still ours.
    async fn release(&self, st: &AppState) {
        let res = sqlx::query(
            "UPDATE signup_approvals SET removal_lease_until = NULL, removal_lease_token = NULL \
              WHERE id = $1 AND removal_lease_token = $2",
        )
        .bind(self.id)
        .bind(self.token)
        .execute(&st.db)
        .await;
        if let Err(e) = res {
            tracing::warn!(signup = %self.id, error = %e, "signup_removal_lease_release_failed");
        }
    }
}

/// Claim the removal of `id` for one run: due, `pending`, lease free. `None` = nothing to run now.
async fn claim(st: &AppState, id: Uuid) -> ApiResult<Option<(Approval, Lease)>> {
    let token = Uuid::new_v4();
    let a: Option<Approval> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE signup_approvals \
            SET removal_lease_until = now() + make_interval(secs => $2), removal_lease_token = $3 \
          WHERE id = $1 AND status = 'removed' AND removal_state = 'pending' AND removal_next_at <= now() \
            AND (removal_lease_until IS NULL OR removal_lease_until < now()) \
          RETURNING {COLUMNS}"
    )))
    .bind(id)
    .bind(LEASE_SECS)
    .bind(token)
    .fetch_optional(&st.db)
    .await?;
    Ok(a.map(|a| (a, Lease { id, token })))
}

/// Run every outstanding step of the removal `id` when it is due (the request, Retry and the tick).
pub async fn run(st: &AppState, sg: &Signup, id: Uuid, actor: Actor<'_>) -> ApiResult<()> {
    let Some((a, lease)) = claim(st, id).await? else {
        return Ok(());
    };
    let out = run_claimed(st, sg, &a, actor, &lease).await;
    lease.release(st).await;
    match out {
        Err(e) if e.code() == LEASE_LOST => {
            tracing::warn!(signup = %id, "signup_removal_lease_lost; another runner continues");
            Ok(())
        }
        other => other,
    }
}

#[derive(sqlx::FromRow)]
struct StepRow {
    step: String,
    status: String,
    attempts: i32,
}

async fn run_claimed(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    actor: Actor<'_>,
    lease: &Lease,
) -> ApiResult<()> {
    let Some(tenant) = a.tenant_id else {
        return set_state(st, a.id, "blocked", None).await;
    };
    let mut conn = st.db.acquire().await?;
    let provisioning = store::steps(&mut conn, a.id).await?;
    drop(conn);
    for step in STEPS {
        let row: Option<StepRow> = sqlx::query_as(
            "SELECT step, status, attempts FROM signup_removal_steps WHERE signup_id = $1 AND step = $2",
        )
        .bind(a.id)
        .bind(step)
        .fetch_optional(&st.db)
        .await?;
        let Some(row) = row else {
            return set_state(st, a.id, "failed", None).await;
        };
        match row.status.as_str() {
            "ok" | "skipped" => continue,
            "failed" => return set_state(st, a.id, "failed", None).await,
            "blocked" => return set_state(st, a.id, "blocked", None).await,
            _ => {}
        }
        lease.hold(st).await?;
        let out = match run_step(st, sg, a, tenant, &row.step, &provisioning, actor, lease).await {
            Ok(o) => o,
            // Never record a step this run no longer owns.
            Err(e) if e.code() == LEASE_LOST => return Err(e),
            Err(e) => Outcome::Retry("internal", e.to_string().chars().take(400).collect()),
        };
        let attempts = row.attempts + 1;
        let (status, code, message, data, next) = match &out {
            Outcome::Ok(d) => ("ok", None, None, d.clone(), None),
            Outcome::Skip(c, m, d) => ("skipped", Some(*c), Some(m.clone()), d.clone(), None),
            Outcome::Retry(c, m) if attempts < MAX_ATTEMPTS => (
                "pending",
                Some(*c),
                Some(m.clone()),
                json!({}),
                Some(backoff_secs(attempts)),
            ),
            Outcome::Retry(c, m) | Outcome::Fail(c, m) => {
                ("failed", Some(*c), Some(m.clone()), json!({}), None)
            }
            Outcome::Block(c, m) => ("blocked", Some(*c), Some(m.clone()), json!({}), None),
        };
        let message = message.map(|m| m.chars().take(500).collect::<String>());
        let mut tx = begin_scoped(&st.db, tenant, None).await?;
        sqlx::query(
            "UPDATE signup_removal_steps \
                SET status = $3, code = $4, message = $5, data = data || $6, attempts = $7, \
                    last_attempt_at = now(), updated_at = now() \
              WHERE signup_id = $1 AND step = $2",
        )
        .bind(a.id)
        .bind(step)
        .bind(status)
        .bind(code)
        .bind(&message)
        .bind(&data)
        .bind(attempts)
        .execute(&mut *tx)
        .await?;
        let landed = matches!(status, "ok" | "skipped");
        audit::record(
            &mut tx,
            actor.id,
            actor.kind,
            &format!("signup.remove.{step}"),
            "signup",
            &a.id.to_string(),
            landed,
            json!({"tenant": tenant, "step_status": status, "code": code, "attempts": attempts, "result": data}),
        )
        .await?;
        tx.commit().await?;
        if landed {
            tracing::info!(signup = %a.id, step, status, "signup_removal_step");
            continue;
        }
        tracing::warn!(signup = %a.id, step, status, code, attempts, "signup_removal_step");
        return match status {
            "pending" => set_state(st, a.id, "pending", next).await,
            other => set_state(st, a.id, other, None).await,
        };
    }
    sqlx::query(
        "UPDATE signup_approvals SET removal_state = 'done', removal_done_at = now(), removal_next_at = NULL \
          WHERE id = $1 AND status = 'removed'",
    )
    .bind(a.id)
    .execute(&st.db)
    .await?;
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    audit::record(
        &mut tx,
        actor.id,
        actor.kind,
        "signup.remove.done",
        "signup",
        &a.id.to_string(),
        true,
        json!({"tenant": tenant}),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(signup = %a.id, %tenant, "signup_removal_done");
    Ok(())
}

/// The row's removal state; `next` (seconds) schedules the tick for a `pending` one.
async fn set_state(st: &AppState, id: Uuid, state: &str, next: Option<f64>) -> ApiResult<()> {
    sqlx::query(
        "UPDATE signup_approvals \
            SET removal_state = $2, \
                removal_next_at = CASE WHEN $3::float8 IS NULL THEN NULL \
                                       ELSE now() + make_interval(secs => $3::float8) END \
          WHERE id = $1 AND status = 'removed' AND removal_state <> 'done'",
    )
    .bind(id)
    .bind(state)
    .bind(next)
    .execute(&st.db)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_step(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    tenant: Uuid,
    step: &str,
    provisioning: &[store::StepRow],
    actor: Actor<'_>,
    lease: &Lease,
) -> ApiResult<Outcome> {
    Ok(match step {
        "suspend" => suspend(st, a, tenant, actor).await?,
        "members" => members(st, a, tenant, actor).await?,
        "licences" => licences(st, sg, tenant, lease).await?,
        "identity" => retire_login(st, sg, a, tenant, provisioning).await?,
        "licences_tenant" => match sg.licences.as_ref() {
            None => Outcome::Retry(
                "signup_not_configured",
                "licences admin is not configured".into(),
            ),
            Some(lic) => match lic.archive_tenant(tenant).await {
                Ok(true) => Outcome::Ok(json!({"archived": true})),
                Ok(false) => Outcome::Skip(
                    "not_registered",
                    "licences does not know this tenant".into(),
                    json!({"archived": false}),
                ),
                Err(e) => upstream("licences_archive_failed", &e),
            },
        },
        other => Outcome::Fail("unknown_step", format!("no such step {other}")),
    })
}

/// Step 1: the tenant suspended and out of the scope registry (idempotent).
async fn suspend(
    st: &AppState,
    a: &Approval,
    tenant: Uuid,
    actor: Actor<'_>,
) -> ApiResult<Outcome> {
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let prev: Option<String> =
        sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1 FOR UPDATE")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(prev) = prev else {
        tx.commit().await?;
        return Ok(Outcome::Block(
            "tenant_missing",
            "the workspace row no longer exists".into(),
        ));
    };
    if prev != "suspended" {
        sqlx::query(
            "UPDATE tenants SET status = 'suspended', updated_at = now(), updated_by = $2 WHERE id = $1",
        )
        .bind(tenant)
        .bind(format!("signup:{}:removed", a.id))
        .execute(&mut *tx)
        .await?;
        audit::record(
            &mut tx,
            actor.id,
            actor.kind,
            "tenant.offboard",
            "tenant",
            &tenant.to_string(),
            true,
            json!({"signup": a.id, "reason": "signup_removed", "from_status": prev}),
        )
        .await?;
    }
    let jobs: Vec<String> = sqlx::query_scalar("SELECT key FROM jobs ORDER BY key")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    for job in &jobs {
        if let Err(e) = scopes::remove(
            &st.bus,
            &Scope {
                tenant,
                job: job.clone(),
            },
        )
        .await
        {
            return Ok(Outcome::Retry(
                "scope_registry_failed",
                format!("{e:#}").chars().take(200).collect(),
            ));
        }
    }
    Ok(Outcome::Ok(
        json!({"from_status": prev, "jobs": jobs.len()}),
    ))
}

/// Step 2: every member of the tenant deleted (member_licences rows with them), each audited `user.delete`.
async fn members(
    st: &AppState,
    a: &Approval,
    tenant: Uuid,
    actor: Actor<'_>,
) -> ApiResult<Outcome> {
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT u.sub, u.tenant_role, m.license_id FROM users u LEFT JOIN member_licences m ON m.sub = u.sub \
          WHERE u.tenant_id = $1 ORDER BY u.created_at, u.sub FOR UPDATE OF u",
    )
    .bind(tenant)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM users WHERE tenant_id = $1")
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
    for (sub, role, licence) in &rows {
        audit::record(
            &mut tx,
            actor.id,
            actor.kind,
            "user.delete",
            "user",
            sub,
            true,
            json!({"tenant_role": role, "member_licence": licence, "reason": "signup_removed", "signup": a.id}),
        )
        .await?;
    }
    tx.commit().await?;
    let licences: Vec<&String> = rows.iter().filter_map(|r| r.2.as_ref()).collect();
    Ok(Outcome::Ok(
        json!({"deleted": rows.len(), "member_licences": licences}),
    ))
}

/// Step 3: every own live licence of the tenant revoked, then every dependency licence still live, each counted as
/// revoked only when a fresh listing no longer shows it live (licences answers an unknown or another tenant's id 200
/// `{revoked: false}`, so an answer alone proves nothing). The lease is renewed before every revoke.
async fn licences(st: &AppState, sg: &Signup, tenant: Uuid, lease: &Lease) -> ApiResult<Outcome> {
    let Some(lic) = sg.licences.as_ref() else {
        return Ok(Outcome::Retry(
            "signup_not_configured",
            "licences admin is not configured".into(),
        ));
    };
    let listed = match lic.tenant_licences(tenant).await {
        Ok(l) => l,
        Err(e) => return Ok(upstream("licences_list_failed", &e)),
    };
    let foreign: Vec<&str> = listed
        .iter()
        .filter(|l| {
            l.tenant_id
                .as_deref()
                .and_then(|t| Uuid::parse_str(t.trim()).ok())
                != Some(tenant)
        })
        .map(|l| l.id.as_str())
        .collect();
    if !foreign.is_empty() {
        return Ok(Outcome::Block(
            TENANT_MISMATCH,
            format!(
                "the licences listing for this tenant returned {} licence(s) of another tenant; nothing was revoked",
                foreign.len()
            ),
        ));
    }
    let already: Vec<&str> = listed
        .iter()
        .filter(|l| !l.live())
        .map(|l| l.id.as_str())
        .collect();
    let own: Vec<&str> = listed
        .iter()
        .filter(|l| l.live() && !l.dependency())
        .map(|l| l.id.as_str())
        .collect();
    let mut released: Vec<String> = Vec::new();
    let mut not_revoked: Vec<&str> = Vec::new();
    for id in &own {
        lease.hold(st).await?;
        match lic.revoke(tenant, id).await {
            Ok(Some(r)) => {
                for d in r {
                    if !released.contains(&d) {
                        released.push(d);
                    }
                }
            }
            Ok(None) => not_revoked.push(id),
            Err(e) => return Ok(upstream("licence_revoke_failed", &e)),
        }
    }
    let after = match lic.tenant_licences(tenant).await {
        Ok(l) => l,
        Err(e) => return Ok(upstream("licences_list_failed", &e)),
    };
    let still: Vec<&str> = after
        .iter()
        .filter(|l| l.live() && !l.dependency())
        .map(|l| l.id.as_str())
        .collect();
    if !still.is_empty() {
        return Ok(Outcome::Retry(
            "licence_still_live",
            format!(
                "{} licence(s) still live after the revoke ({} answered not revoked)",
                still.len(),
                not_revoked.len()
            ),
        ));
    }
    // A dependency licence still live: one explicit revoke each. A transient failure is retried (the step stays
    // pending); a refusal (409 `dependency_still_required`, 403) is recorded and confirmed by the listing below.
    let deps: Vec<&str> = after
        .iter()
        .filter(|l| l.live() && l.dependency())
        .map(|l| l.id.as_str())
        .collect();
    let mut refused: Vec<Value> = Vec::new();
    for id in &deps {
        lease.hold(st).await?;
        match lic.revoke(tenant, id).await {
            Ok(Some(_)) => {}
            Ok(None) => refused.push(json!({"license_id": id, "reason": "not_revoked"})),
            Err(e) if e.status.is_none_or(|s| s >= 500 || s == 408 || s == 429) => {
                return Ok(Outcome::Retry(
                    "dependency_revoke_failed",
                    e.message.chars().take(400).collect(),
                ));
            }
            Err(e) => refused.push(json!({"license_id": id, "status": e.status,
                                          "reason": e.message.chars().take(160).collect::<String>()})),
        }
    }
    let last = if deps.is_empty() {
        after.clone()
    } else {
        match lic.tenant_licences(tenant).await {
            Ok(l) => l,
            Err(e) => return Ok(upstream("licences_list_failed", &e)),
        }
    };
    let live_last = |id: &str| last.iter().any(|l| l.id == id && l.live());
    let revoked: Vec<&str> = own
        .iter()
        .chain(deps.iter())
        .copied()
        .filter(|id| !live_last(id))
        .collect();
    let kept: Vec<Value> = last
        .iter()
        .filter(|l| l.live() && l.dependency())
        .map(|l| {
            refused
                .iter()
                .find(|r| r["license_id"] == l.id.as_str())
                .cloned()
                .unwrap_or_else(|| json!({"license_id": l.id}))
        })
        .collect();
    if last.iter().any(|l| l.live() && !l.dependency()) {
        return Ok(Outcome::Retry(
            "licence_still_live",
            "an own licence became live again during the removal".into(),
        ));
    }
    tracing::info!(%tenant, revoked = revoked.len(), released = released.len(), kept = kept.len(),
        "signup_removal_licences_revoked");
    Ok(Outcome::Ok(json!({
        "listed": listed.len(),
        "already_revoked": already,
        "revoked": revoked,
        "released_dependency_ids": released,
        "kept_dependencies": kept,
    })))
}

/// Step 4: the login this signup minted, deleted when it carries this signup's stamp.
async fn retire_login(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    tenant: Uuid,
    provisioning: &[store::StepRow],
) -> ApiResult<Outcome> {
    let Some(sub) = identity(provisioning) else {
        return Ok(Outcome::Skip(
            "no_minted_login",
            "the approve recorded no login it created".into(),
            json!({"login": "none"}),
        ));
    };
    let Some(kc) = sg.kc.as_ref() else {
        return Ok(Outcome::Retry(
            "signup_not_configured",
            "keycloak admin is not configured".into(),
        ));
    };
    let home: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM carebus_auth_user($1)")
        .bind(&sub)
        .fetch_optional(&st.db)
        .await?;
    if home.is_some_and(|t| t != tenant) {
        return Ok(Outcome::Skip(
            "kept_in_use",
            "the login belongs to another care-bus workspace".into(),
            json!({"login": "kept_in_use"}),
        ));
    }
    let res: Result<Outcome, UpstreamError> = async {
        if !kc.exists(&sub).await? {
            return Ok(Outcome::Ok(json!({"login": "absent"})));
        }
        let stamp = kc.attribute(&sub, STAMP_ATTR).await?;
        let ours = stamp
            .as_deref()
            .is_some_and(|s| s == a.id.to_string() || s == format!("{REJECTED_PREFIX}{}", a.id));
        if !ours {
            return Ok(Outcome::Block(
                IDENTITY_NOT_OWNED,
                "the login does not carry this signup's stamp; it is never deleted by the removal"
                    .into(),
            ));
        }
        let login = if kc.delete_user(&sub).await? {
            "deleted"
        } else {
            "absent"
        };
        Ok(Outcome::Ok(json!({ "login": login })))
    }
    .await;
    Ok(res.unwrap_or_else(|e| upstream("keycloak_delete_failed", &e)))
}

/// The removals the tick runs: `pending`, due, lease free.
async fn due(st: &AppState) -> ApiResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM signup_approvals \
          WHERE status = 'removed' AND removal_state = 'pending' AND removal_next_at <= now() \
            AND (removal_lease_until IS NULL OR removal_lease_until < now()) \
          ORDER BY removal_next_at LIMIT 20",
    )
    .fetch_all(&st.db)
    .await?)
}

/// One tick: every due removal runs as the tick. Returns how many were due.
pub async fn tick(st: &AppState, sg: &Signup) -> ApiResult<usize> {
    if !sg.enabled() {
        return Ok(0);
    }
    let ids = due(st).await?;
    for id in &ids {
        if let Err(e) = run(st, sg, *id, TICK).await {
            tracing::warn!(signup = %id, error = %e, "signup_removal_tick_failed");
        }
    }
    Ok(ids.len())
}

/// The removal as the API shows it: state, who asked, and the journal (codes and licence ids, no personal data).
pub async fn view(st: &AppState, a: &Approval) -> ApiResult<Value> {
    if a.removal_state.is_none() {
        return Ok(Value::Null);
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        step: String,
        status: String,
        code: Option<String>,
        message: Option<String>,
        attempts: i32,
        data: Value,
        last_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT step, status, code, message, attempts, data, last_attempt_at \
           FROM signup_removal_steps WHERE signup_id = $1 ORDER BY ord",
    )
    .bind(a.id)
    .fetch_all(&st.db)
    .await?;
    Ok(json!({
        "state": a.removal_state,
        "requested_by": a.removal_requested_by,
        "requested_at": a.removal_requested_at,
        "done_at": a.removal_done_at,
        "steps": rows.iter().map(|r| json!({
            "step": r.step,
            "status": r.status,
            "code": r.code,
            "message": r.message,
            "attempts": r.attempts,
            "at": r.last_attempt_at,
            "result": r.data,
        })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slug_is_the_licences_registry_name() {
        let id = Uuid::parse_str("03533460-c986-4000-8000-0000deadbeef").unwrap();
        assert_eq!(tenant_slug("Globex Clinics", id), "globex-clinics-deadbeef");
    }

    #[test]
    fn upstream_failures_classify_by_status() {
        let retry = |s: Option<u16>| {
            matches!(
                upstream("x", &UpstreamError::new(s, "m")),
                Outcome::Retry(..)
            )
        };
        assert!(retry(None));
        assert!(retry(Some(502)));
        assert!(retry(Some(429)));
        assert!(!retry(Some(403)));
        assert!(!retry(Some(409)));
    }
}
