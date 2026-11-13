//! What an approval does, step by step, journaled in `signup_provisioning_steps` (agent `signup/provisioning.rs`,
//! after commonpractice `approveApprovalRow`). The signup is marked `approved` LAST, only after its login, workspace
//! and seat landed; a failure before that leaves it `pending` and decides nothing, and the next approve resumes into
//! the same workspace.
//!
//! The approve takes a claim on the row first (`approving_until` / `approving_claim`, a fresh token per run, from the
//! claim's RETURNING row): one approver at a time across replicas, the screen and Telegram. Every step re-confirms and
//! extends the claim before it runs, and a run older than [`MAX_APPROVE`] stops, so a lost or lapsed claim stops the
//! run with 409 `approval_claim_lost` before anything further (lessons #2, "30-minute ceiling").
//!
//! Gating steps (they must land before the decision):
//! 1. `keycloak_user` — the care-bus login for the requester's email, created WITHOUT an email, names by fr's split
//!    rule, born with `carebus_signup_id=<signup id>` (its ownership stamp: a create whose answer was lost is
//!    recognised by it). A login that exists and this signup did not mint is refused (409 `identity_exists`, the
//!    fr/w2v rule: never write to, or mail, a login this signup did not create).
//! 2. `tenant` — the `tenants` row, founded under an id remembered BEFORE the insert and written by
//!    `updated_by = signup:<id>` in the same INSERT (lesson #3), plus its job (`SIGNUP_JOB_KEY`) and the scope registry.
//! 3. `membership` — the `users` row: the requester is the workspace's tenant_admin.
//!
//! Then, attempted inline right after the decision and finished by the tick:
//! 4. `base_release` — the workspace's job put on the current base release of its job key, copied from the source
//!    job (`SIGNUP_BASE_TENANT_ID`, the bootstrap tenant by default) by `carebus_adopt_base_release` (migration 0021):
//!    what an operator applying the release by hand did, so the new workspace serves the product content at once.
//!    Idempotent (a job already on a release is left alone). No base release to adopt is `skipped`
//!    (`no_base_release`): the workspace starts empty, the signup does not fail.
//! 5. `licences_tenant` — the tenant registered in licences (owner care-bus) and its licence set ensured (bundle).
//! 6. `member_licence` — the person's member licence (`POST /licenses/user/{sub}`, subject realm care-bus), recorded
//!    in `member_licences` inside the tenant context (lesson #1).
//! 7. `invite_email` — ONE email, after every earlier step is ok or skipped: `locale` + `signup_app` on the login,
//!    then execute-actions-email (UPDATE_PASSWORD, VERIFY_EMAIL; client `care-bus`, redirect the public URL). An answer
//!    that never came (timeout) is `failed` (`invite_email_uncertain`), never re-sent automatically: a resend would
//!    rotate the link and kill the first one (lesson #5).
//!
//! Each step is claimed (`lease_until`) by one runner at a time — the approve or the tick (lesson #4) — and every
//! action is idempotent or remembers its progress in `data` before the next upstream call. A transient failure is
//! `pending` with backoff (1, 2, 4 … min, 6 h cap, 10 attempts); a refusal is `failed` and stops the chain. The tick
//! only runs the steps of an APPROVED signup.
//!
//! Reject repair (CP `signup-reject-repair`): a reject of a signup whose aborted approve founded things suspends that
//! workspace (scopes removed, seat dropped) after revoking its licence set: every member licence recorded in the
//! tenant and the tenant's bundle licence, so licences releases the tool dependency licences (mcp, strata-rag,
//! strata-ingest) nothing requires any more. It also retires the login it minted (deleted when it holds no credential
//! and no IdP link, else disabled and marked `carebus_signup_id=rejected:<id>`). The outcome is kept in
//! `signup_approvals.reject_repair` (`workspace`, `licences`: the revoked and released ids, `identity`), and a repair
//! that did not finish is retried by the tick; every part is idempotent.
use std::time::{Duration, Instant};

use carebus::{Scope, scopes};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit;
use crate::error::{ApiError, ApiResult};
use crate::signup::keycloak_admin::{ActionsEmail, INVITE_ACTIONS};
use crate::signup::licences::{LicencesAdmin, TenantOwner};
use crate::signup::store::{self, Approval, StepRow};
use crate::signup::upstream::UpstreamError;
use crate::signup::{DEFAULT_LOCALE, Signup, default_tenant_name};
use crate::state::AppState;
use crate::tenancy::begin_scoped;

pub const STEPS: [&str; 7] = [
    "keycloak_user",
    "tenant",
    "membership",
    "base_release",
    "licences_tenant",
    "member_licence",
    "invite_email",
];
pub const GATING: usize = 3;
const LEASE_SECS: f64 = 300.0;
pub const CLAIM_SECS: f64 = 300.0;
/// An approve run never continues past this (CP: abort before anything irreversible).
pub const MAX_APPROVE: Duration = Duration::from_secs(30 * 60);
const BACKOFF_BASE_SECS: f64 = 60.0;
const BACKOFF_CAP_SECS: f64 = 6.0 * 3600.0;
pub const MAX_ATTEMPTS: i32 = 10;
/// The audit actor of the provisioning steps (whoever runs them).
pub const TICK_ACTOR: &str = "service:carebus-api-signup-tick";
/// The Keycloak attribute that stamps a login this service minted for signup `<id>` (`<id>`), and retired
/// (`rejected:<id>`). Admin-only in the realm's user profile (at least 45 characters).
pub const STAMP_ATTR: &str = "carebus_signup_id";
pub const REJECTED_PREFIX: &str = "rejected:";
pub const BELONGS_ELSEWHERE: &str = "user_belongs_to_another_workspace";
pub const IDENTITY_EXISTS: &str = "identity_exists";
const REPAIR_MAX: i32 = 10;

pub fn backoff_secs(attempts: i32) -> f64 {
    let exp = (attempts.max(1) - 1).min(30);
    (BACKOFF_BASE_SECS * 2f64.powi(exp)).min(BACKOFF_CAP_SECS)
}

enum Outcome {
    Ok(Value),
    /// Nothing to do, and that is not a failure (recorded `skipped` with its code; the chain goes on).
    Skip(&'static str, String),
    Retry(&'static str, String),
    Fail(&'static str, String),
}

impl Outcome {
    fn landed(&self) -> bool {
        matches!(self, Outcome::Ok(_) | Outcome::Skip(..))
    }
}

/// An upstream failure as a step outcome: transport, 5xx, 408 and 429 retry; any other status is a refusal.
fn upstream(code: &'static str, e: &UpstreamError) -> Outcome {
    let msg: String = e.message.chars().take(400).collect();
    match e.status {
        None => Outcome::Retry(code, msg),
        Some(s) if s >= 500 || s == 408 || s == 429 => Outcome::Retry(code, msg),
        Some(_) => Outcome::Fail(code, msg),
    }
}

/// A database error as an upstream-shaped error (the login licence path reports one error type).
fn db(e: sqlx::Error) -> UpstreamError {
    UpstreamError::new(
        None,
        format!("care-bus database: {}", ApiError::from(e).code()),
    )
}

pub fn signup_label(a: &Approval) -> String {
    default_tenant_name(a.company.as_deref(), &a.email, Some(a.name.as_str()))
}

pub enum Decision {
    Approved {
        approval: Box<Approval>,
        tenant: Uuid,
        tenant_name: String,
    },
    Rejected(Box<Approval>, Value),
    AlreadyDecided(Box<Approval>),
    InProgress(Box<Approval>),
    NotFound,
}

pub struct TenantInput<'a> {
    pub name: &'a str,
}

/// The approve run's claim on its row (CP `HeldApprovalClaim`).
pub struct Claim {
    id: Uuid,
    token: Uuid,
    started: Instant,
}

fn claim_lost(step: &str) -> ApiError {
    ApiError::Conflict(
        "approval_claim_lost",
        format!(
            "the approve stopped before '{step}': this run no longer holds the signup; nothing further was done; approve again to resume"
        ),
    )
}

impl Claim {
    /// Before every step: 409 `approval_claim_lost` unless the claim is still ours, live, the row pending and the run
    /// younger than [`MAX_APPROVE`]; afterwards it is live for a full window.
    async fn hold(&self, st: &AppState, step: &str) -> ApiResult<()> {
        if self.started.elapsed() > MAX_APPROVE {
            return Err(claim_lost(step));
        }
        let mut tx = st.db.begin().await?;
        let held = store::hold(&mut tx, self.id, self.token, CLAIM_SECS).await?;
        tx.commit().await?;
        if held { Ok(()) } else { Err(claim_lost(step)) }
    }

    async fn release(&self, st: &AppState) {
        let res = async {
            let mut tx = st.db.begin().await?;
            store::release(&mut tx, self.id, self.token).await?;
            tx.commit().await?;
            Ok::<_, ApiError>(())
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(signup = %self.id, error = %e, "signup_approve_claim_release_failed");
        }
    }
}

struct Ctx<'a> {
    input: Option<&'a TenantInput<'a>>,
    claim: Option<&'a Claim>,
}

/// Approve the PENDING signup `id`: claim it, run the gating steps, attempt the rest, then mark it approved
/// (conditional on the claim). A gating step that does not land is an error ([`not_landed`]); the row stays pending.
pub async fn approve(
    st: &AppState,
    sg: &Signup,
    id: Uuid,
    input: &TenantInput<'_>,
    by: &str,
    via: &str,
) -> ApiResult<Decision> {
    let token = Uuid::new_v4();
    let mut tx = st.db.begin().await?;
    let claimed = store::claim(&mut tx, id, token, CLAIM_SECS).await?;
    let current = if claimed.is_some() {
        None
    } else {
        store::get(&mut tx, id).await?
    };
    tx.commit().await?;
    let a = match (claimed, current) {
        (Some(a), _) => a,
        (None, None) => return Ok(Decision::NotFound),
        (None, Some(a)) if a.status != "pending" => {
            return Ok(Decision::AlreadyDecided(Box::new(a)));
        }
        (None, Some(a)) => return Ok(Decision::InProgress(Box::new(a))),
    };
    let claim = Claim {
        id,
        token,
        started: Instant::now(),
    };
    let out = approve_claimed(st, sg, &a, &claim, input, by, via).await;
    claim.release(st).await;
    out
}

async fn approve_claimed(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    claim: &Claim,
    input: &TenantInput<'_>,
    by: &str,
    via: &str,
) -> ApiResult<Decision> {
    let id = a.id;
    let mut tx = st.db.begin().await?;
    for (i, step) in STEPS.iter().enumerate() {
        sqlx::query(
            "INSERT INTO signup_provisioning_steps (signup_id, step, ord, status, attempts, data, next_attempt_at) \
             VALUES ($1, $2, $3, 'pending', 0, '{}', now()) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(*step)
        .bind(i16::try_from(i + 1).unwrap_or(i16::MAX))
        .execute(&mut *tx)
        .await?;
    }
    let gating: Vec<String> = STEPS[..GATING].iter().map(|s| s.to_string()).collect();
    sqlx::query(
        "UPDATE signup_provisioning_steps \
            SET status = 'pending', attempts = 0, next_attempt_at = now(), code = NULL, message = NULL, \
                lease_until = NULL, updated_at = now() \
          WHERE signup_id = $1 AND step = ANY($2) AND status IN ('pending', 'failed') \
            AND (lease_until IS NULL OR lease_until < now())",
    )
    .bind(id)
    .bind(&gating)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let ctx = Ctx {
        input: Some(input),
        claim: Some(claim),
    };
    if let Some((code, message)) = run_chain(st, sg, a, &STEPS[..GATING], &ctx).await? {
        let mut tx = st.db.begin().await?;
        let steps = store::steps(&mut tx, id).await?;
        tx.commit().await?;
        return Err(not_landed(code, message, &steps));
    }
    // Decide as soon as login, workspace and seat landed; the licence and email steps run after the decision (here,
    // then the tick), so an email never goes out for a signup that is still pending (a lost claim, a later reject).
    claim.hold(st, "decide").await?;
    let mut tx = st.db.begin().await?;
    let rows = store::steps(&mut tx, id).await?;
    let (Some(tenant), Some(tenant_name)) = (founded_tenant(&rows), founded_name(&rows)) else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "signup {id}: the gating steps landed without a workspace"
        )));
    };
    let Some(approval) = store::mark_approved(&mut tx, id, tenant, by, via, claim.token).await?
    else {
        return Err(claim_lost("decide"));
    };
    tx.commit().await?;
    tracing::info!(signup = %id, %tenant, via, "signup_approved");
    let after = Ctx {
        input: None,
        claim: None,
    };
    if let Err(e) = run_chain(st, sg, &approval, &STEPS[GATING..], &after).await {
        tracing::warn!(signup = %id, error = %e, "signup_post_decision_steps_failed; the tick retries them");
    }
    Ok(Decision::Approved {
        approval: Box::new(approval),
        tenant,
        tenant_name,
    })
}

/// A gating step that did not land: 409 for a refusal the super_admin resolves, else 502 with the step's code; the
/// error carries the journal (`steps`). Nothing was decided.
pub fn not_landed(code: &'static str, message: String, steps: &[StepRow]) -> ApiError {
    let status = match code {
        BELONGS_ELSEWHERE | IDENTITY_EXISTS | "user_in_other_tenant" | "step_in_progress" => {
            axum::http::StatusCode::CONFLICT
        }
        _ => axum::http::StatusCode::BAD_GATEWAY,
    };
    let steps: Vec<Value> = steps
        .iter()
        .map(|r| json!({"step": r.step, "status": r.status, "code": r.code}))
        .collect();
    ApiError::Detailed(
        status,
        code,
        format!("the approval did not complete; nothing was decided ({message})"),
        json!({"steps": steps}),
    )
}

/// Reject the PENDING signup `id`; refused while an approve holds its claim. What an aborted approve founded is
/// repaired after the decision commits ([`repair`]).
pub async fn reject(
    st: &AppState,
    sg: &Signup,
    id: Uuid,
    by: &str,
    via: &str,
    reason: Option<&str>,
) -> ApiResult<Decision> {
    let mut tx = st.db.begin().await?;
    let Some(a) = store::lock(&mut tx, id).await? else {
        return Ok(Decision::NotFound);
    };
    if a.status != "pending" {
        return Ok(Decision::AlreadyDecided(Box::new(a)));
    }
    if a.approving {
        return Ok(Decision::InProgress(Box::new(a)));
    }
    let Some(a) = store::mark_rejected(&mut tx, id, by, via, reason).await? else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "signup {id} stopped being pending under its row lock"
        )));
    };
    tx.commit().await?;
    tracing::info!(signup = %id, via, "signup_rejected");
    // The reject is committed: a repair error never turns into "nothing was decided"; the tick retries it.
    let repaired = match repair(st, sg, &a).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(signup = %id, error = %e, "signup_reject_repair_failed; the tick retries it");
            let _ = sqlx::query(
                "UPDATE signup_approvals SET reject_repair = '{\"repair\": \"failed\"}', \
                        repair_next_at = now() + interval '60 seconds' WHERE id = $1 AND status = 'rejected'",
            )
            .bind(id)
            .execute(&st.db)
            .await;
            json!({"repair": "failed"})
        }
    };
    Ok(Decision::Rejected(Box::new(a), repaired))
}

/// Undo what an aborted approve of the REJECTED signup `a` founded, record it in `reject_repair`, and schedule a
/// retry when a part failed. Returns the record (`{}` when there was nothing to undo).
pub async fn repair(st: &AppState, sg: &Signup, a: &Approval) -> ApiResult<Value> {
    let mut tx = st.db.begin().await?;
    let rows = store::steps(&mut tx, a.id).await?;
    tx.commit().await?;
    let planned = planned_tenant(&rows);
    let minted = identity(&rows);
    let mut out = serde_json::Map::new();
    let mut failed = false;
    if let Some(t) = planned {
        let r = offboard_aborted(st, sg, a, t, minted.as_deref()).await;
        let v = match r {
            Ok((s, licences)) => {
                if let Some(l) = licences {
                    out.insert("licences".into(), l);
                }
                s.to_string()
            }
            Err(e) => {
                failed = true;
                tracing::error!(signup = %a.id, tenant = %t, error = %e, "signup_reject_offboard_failed");
                format!(
                    "failed: {}",
                    e.to_string().chars().take(160).collect::<String>()
                )
            }
        };
        out.insert("workspace".into(), json!(v));
    }
    if let Some(sub) = &minted {
        let v = retire_minted(st, sg, a, sub, planned).await;
        failed |= v.starts_with("failed");
        out.insert("identity".into(), json!(v));
    }
    if out.is_empty() {
        return Ok(json!({}));
    }
    let record = Value::Object(out);
    sqlx::query(
        "UPDATE signup_approvals SET reject_repair = $2, repair_attempts = repair_attempts + 1, \
                repair_next_at = CASE WHEN $3 AND repair_attempts + 1 < $4 \
                                      THEN now() + make_interval(secs => $5::float8) ELSE NULL END \
          WHERE id = $1 AND status = 'rejected'",
    )
    .bind(a.id)
    .bind(&record)
    .bind(failed)
    .bind(REPAIR_MAX)
    .bind(backoff_secs(1))
    .execute(&st.db)
    .await?;
    if failed {
        tracing::warn!(signup = %a.id, "signup_reject_repair_incomplete; the tick retries it");
    }
    Ok(record)
}

/// Suspend the workspace an aborted approve founded (when it was founded at all), remove its scopes, drop the minted
/// login's seat, after revoking the tenant's licence set ([`revoke_licence_set`], with what it revoked and released).
/// `absent` when nothing was founded.
async fn offboard_aborted(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    tenant: Uuid,
    sub: Option<&str>,
) -> ApiResult<(&'static str, Option<Value>)> {
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let founder: Option<String> =
        sqlx::query_scalar("SELECT updated_by FROM tenants WHERE id = $1 FOR UPDATE")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(founder) = founder else {
        tx.commit().await?;
        return Ok(("absent", None));
    };
    let marker = format!("signup:{}", a.id);
    if !founder.starts_with(&marker) {
        // Somebody else wrote it since (a super_admin re-provisioned it): never offboard what this signup does not own.
        tx.commit().await?;
        return Ok(("kept_not_ours", None));
    }
    let others: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE sub IS DISTINCT FROM $1")
            .bind(sub)
            .fetch_one(&mut *tx)
            .await?;
    if others > 0 {
        tx.commit().await?;
        return Ok(("kept_other_members", None));
    }
    // Read before the seat goes: member_licences rows are deleted with their users row.
    let members: Vec<String> =
        sqlx::query_scalar("SELECT license_id FROM member_licences ORDER BY ensured_at, sub")
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    let licences = revoke_licence_set(sg, tenant, &members)
        .await
        .map_err(|e| ApiError::Rejected(axum::http::StatusCode::BAD_GATEWAY, e.message))?;
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    if let Some(s) = sub {
        sqlx::query("DELETE FROM users WHERE sub = $1")
            .bind(s)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE tenants SET status = 'suspended', updated_at = now(), updated_by = $2 WHERE id = $1",
    )
    .bind(tenant)
    .bind(format!("{marker}:rejected"))
    .execute(&mut *tx)
    .await?;
    let jobs: Vec<String> = sqlx::query_scalar("SELECT key FROM jobs")
        .fetch_all(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        TICK_ACTOR,
        "system",
        "tenant.offboard",
        "tenant",
        &tenant.to_string(),
        true,
        json!({"signup": a.id, "reason": "signup_rejected"}),
    )
    .await?;
    tx.commit().await?;
    for job in jobs {
        scopes::remove(&st.bus, &Scope { tenant, job }).await?;
    }
    Ok(("suspended", Some(licences)))
}

/// Revoke the tenant's licence set: every member licence in `members`, then the bundle licence (licences answers it
/// as the service subject's). Idempotent: an already revoked or unknown licence is not an error. The record:
/// `{"revoked": [ids], "released_dependency_ids": [ids]}` (licence ids only, no personal data).
pub async fn revoke_licence_set(
    sg: &Signup,
    tenant: Uuid,
    members: &[String],
) -> Result<Value, UpstreamError> {
    let lic = sg
        .licences
        .as_ref()
        .ok_or_else(|| UpstreamError::new(None, "licences admin is not configured"))?;
    let mut ids: Vec<String> = members.to_vec();
    if let Some(b) = lic.bundle_licence(tenant).await?
        && !ids.contains(&b)
    {
        ids.push(b);
    }
    let mut released: Vec<String> = Vec::new();
    for id in &ids {
        for r in lic.revoke(tenant, id).await?.unwrap_or_default() {
            if !released.contains(&r) {
                released.push(r);
            }
        }
    }
    tracing::info!(%tenant, revoked = ids.len(), released = released.len(), "signup_offboard_licences_revoked");
    Ok(json!({"revoked": ids, "released_dependency_ids": released}))
}

/// Retire the login an aborted approve minted (CP `retireRejectedIdentity`), unless it is seated in another tenant
/// (`kept_in_use`). No credential and no IdP link (positively) -> deleted; else disabled and marked. Never fails the
/// reject: an error is `failed: …`.
async fn retire_minted(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    sub: &str,
    tenant: Option<Uuid>,
) -> String {
    let home: Result<Option<Uuid>, sqlx::Error> =
        sqlx::query_scalar("SELECT tenant_id FROM carebus_auth_user($1)")
            .bind(sub)
            .fetch_optional(&st.db)
            .await;
    match home {
        Ok(Some(t)) if Some(t) != tenant => return "kept_in_use".into(),
        Ok(_) => {}
        Err(e) => {
            tracing::error!(signup = %a.id, error = %e, "signup_reject_identity_retire_failed");
            return "failed: could not check the login's tenant".into();
        }
    }
    let Some(kc) = sg.kc.as_ref() else {
        return "failed: keycloak admin is not configured".into();
    };
    let marker = format!("{REJECTED_PREFIX}{}", a.id);
    let res: Result<&'static str, UpstreamError> = async {
        if !kc.exists(sub).await? {
            return Ok("absent");
        }
        if kc.has_credential_or_link(sub).await? == Some(false) {
            kc.delete_user(sub).await?;
            return Ok("deleted");
        }
        Ok(if kc.disable_and_mark(sub, STAMP_ATTR, &marker).await? {
            "disabled"
        } else {
            "disabled_unmarked"
        })
    }
    .await;
    match res {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::error!(
                signup = %a.id,
                error = %e,
                remediation = "the login minted for this rejected signup may still be ENABLED; disable it by hand",
                "signup_reject_identity_retire_failed"
            );
            format!(
                "failed: {}",
                e.message.chars().take(160).collect::<String>()
            )
        }
    }
}

async fn claim_step(st: &AppState, id: Uuid, step: &str) -> ApiResult<Option<(i32, Value)>> {
    let claimed = sqlx::query_as(
        "UPDATE signup_provisioning_steps \
            SET lease_until = now() + make_interval(secs => $3), updated_at = now() \
          WHERE signup_id = $1 AND step = $2 AND status = 'pending' \
            AND (next_attempt_at IS NULL OR next_attempt_at <= now()) \
            AND (lease_until IS NULL OR lease_until < now()) \
          RETURNING attempts, data",
    )
    .bind(id)
    .bind(step)
    .bind(LEASE_SECS)
    .fetch_optional(&st.db)
    .await?;
    Ok(claimed)
}

/// Merge `patch` into the step's `data` (progress an upstream call must not repeat).
async fn remember(st: &AppState, id: Uuid, step: &str, patch: &Value) -> ApiResult<()> {
    sqlx::query(
        "UPDATE signup_provisioning_steps SET data = data || $3, updated_at = now() \
          WHERE signup_id = $1 AND step = $2",
    )
    .bind(id)
    .bind(step)
    .bind(patch)
    .execute(&st.db)
    .await?;
    Ok(())
}

async fn record_in(
    conn: &mut PgConnection,
    id: Uuid,
    step: &str,
    attempts: i32,
    out: &Outcome,
) -> ApiResult<()> {
    let attempts = attempts + 1;
    let (status, code, message, patch, next): (
        &str,
        Option<&str>,
        Option<String>,
        Value,
        Option<f64>,
    ) = match out {
        Outcome::Ok(d) => ("ok", None, None, d.clone(), None),
        Outcome::Skip(c, m) => ("skipped", Some(*c), Some(m.clone()), json!({}), None),
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
    };
    sqlx::query(
        "UPDATE signup_provisioning_steps \
            SET status = $3, code = $4, message = $5, data = data || $6, attempts = $7, \
                last_attempt_at = now(), lease_until = NULL, updated_at = now(), \
                next_attempt_at = CASE WHEN $8::float8 IS NULL THEN NULL \
                                       ELSE now() + make_interval(secs => $8::float8) END \
          WHERE signup_id = $1 AND step = $2",
    )
    .bind(id)
    .bind(step)
    .bind(status)
    .bind(code)
    .bind(message.map(|m| m.chars().take(500).collect::<String>()))
    .bind(patch)
    .bind(attempts)
    .bind(next)
    .execute(conn)
    .await?;
    if out.landed() {
        tracing::info!(signup = %id, step, status, "signup_provisioning_step")
    } else {
        tracing::warn!(signup = %id, step, status, code, attempts, "signup_provisioning_step")
    }
    Ok(())
}

async fn record(
    st: &AppState,
    id: Uuid,
    step: &str,
    attempts: i32,
    out: &Outcome,
) -> ApiResult<()> {
    let mut conn = st.db.acquire().await?;
    record_in(&mut conn, id, step, attempts, out).await
}

/// Run every outstanding step of the APPROVED signup `id` (the tick and Retry).
pub async fn run(st: &AppState, sg: &Signup, id: Uuid) -> ApiResult<()> {
    let mut tx = st.db.begin().await?;
    let Some(a) = store::get(&mut tx, id).await? else {
        return Ok(());
    };
    tx.commit().await?;
    if a.status != "approved" {
        return Ok(());
    }
    let ctx = Ctx {
        input: None,
        claim: None,
    };
    run_chain(st, sg, &a, &STEPS, &ctx).await?;
    Ok(())
}

/// Run `steps` of `a` in order. `None` when every one is ok or skipped; else the (code, note) of the first that is not.
async fn run_chain(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    steps: &[&str],
    ctx: &Ctx<'_>,
) -> ApiResult<Option<(&'static str, String)>> {
    for step in steps {
        let mut tx = st.db.begin().await?;
        let rows = store::steps(&mut tx, a.id).await?;
        tx.commit().await?;
        let Some(row) = rows.iter().find(|r| r.step == *step) else {
            return Ok(Some((
                "journal_missing",
                format!("the journal has no {step} step"),
            )));
        };
        match row.status.as_str() {
            "ok" | "skipped" => continue,
            "failed" => {
                return Ok(Some((
                    match row.code.as_deref() {
                        Some(IDENTITY_EXISTS) => IDENTITY_EXISTS,
                        Some(BELONGS_ELSEWHERE) => BELONGS_ELSEWHERE,
                        _ => "step_failed",
                    },
                    format!("the {step} step failed earlier"),
                )));
            }
            _ => {}
        }
        if let Some(c) = ctx.claim {
            c.hold(st, step).await?;
        }
        let Some((attempts, data)) = claim_step(st, a.id, step).await? else {
            return Ok(Some((
                "step_in_progress",
                format!("the {step} step is waiting or being run by another runner"),
            )));
        };
        let out = match run_step(st, sg, a, step, &data, &rows, ctx).await {
            Ok(o) => o,
            Err(e) => Outcome::Retry("internal", e.to_string().chars().take(400).collect()),
        };
        record(st, a.id, step, attempts, &out).await?;
        if let Outcome::Retry(c, m) | Outcome::Fail(c, m) = out {
            return Ok(Some((c, m)));
        }
    }
    Ok(None)
}

fn data_of<'a>(rows: &'a [StepRow], step: &str) -> Option<&'a Value> {
    rows.iter().find(|r| r.step == step).map(|r| &r.data)
}

/// The login the `keycloak_user` step minted (only minted logins are ever recorded).
pub(crate) fn identity(rows: &[StepRow]) -> Option<String> {
    let d = data_of(rows, "keycloak_user")?;
    if d.get("existing").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    d.get("sub")?.as_str().map(str::to_string)
}

fn planned_tenant(rows: &[StepRow]) -> Option<Uuid> {
    data_of(rows, "tenant")?
        .get("tenant_id")?
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
}

pub(crate) fn founded_tenant(rows: &[StepRow]) -> Option<Uuid> {
    rows.iter()
        .find(|r| r.step == "tenant" && r.status == "ok")
        .and_then(|_| planned_tenant(rows))
}

fn founded_name(rows: &[StepRow]) -> Option<String> {
    rows.iter()
        .find(|r| r.step == "tenant" && r.status == "ok")?
        .data
        .get("name")?
        .as_str()
        .map(str::to_string)
}

async fn run_step(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    step: &str,
    data: &Value,
    rows: &[StepRow],
    ctx: &Ctx<'_>,
) -> ApiResult<Outcome> {
    let tenant = founded_tenant(rows);
    let missing =
        |what: &'static str| Outcome::Fail(what, format!("an earlier step recorded no {what}"));
    let (Some(kc), Some(lic)) = (sg.kc.as_ref(), sg.licences.as_ref()) else {
        return Ok(Outcome::Retry(
            "signup_not_configured",
            "signup is not configured".into(),
        ));
    };
    Ok(match step {
        "keycloak_user" => keycloak_user(st, kc, a).await?,
        "tenant" => match ctx.input {
            Some(input) => found_tenant(st, sg, a, input, data).await?,
            None => Outcome::Fail(
                "tenant_input_missing",
                "only an approve founds the workspace".into(),
            ),
        },
        "membership" => match (tenant, identity(rows)) {
            (Some(t), Some(sub)) => membership(st, a, t, &sub).await?,
            (None, _) => missing("tenant_missing"),
            (_, None) => missing("identity_missing"),
        },
        "base_release" => match tenant {
            Some(t) => adopt_base_release(st, sg, a, t).await?,
            None => missing("tenant_missing"),
        },
        "licences_tenant" => match tenant {
            Some(t) => match tenant_licences(st, lic, t).await {
                Ok(()) => Outcome::Ok(json!({})),
                Err(e) => upstream("licences_tenant_failed", &e),
            },
            None => missing("tenant_missing"),
        },
        "member_licence" => match (tenant, identity(rows)) {
            (Some(t), Some(sub)) => match record_member_licence(st, lic, t, &sub).await {
                Ok(id) => Outcome::Ok(json!({"license_id": id})),
                Err(e) => upstream("member_licence_failed", &e),
            },
            (None, _) => missing("tenant_missing"),
            (_, None) => missing("identity_missing"),
        },
        "invite_email" => {
            let Some(sub) = identity(rows) else {
                return Ok(missing("identity_missing"));
            };
            // Defence in depth: the email only after every earlier step is ok or skipped (lesson: licences first).
            let all_landed = rows
                .iter()
                .filter(|r| r.step != "invite_email")
                .all(|r| matches!(r.status.as_str(), "ok" | "skipped"));
            if !all_landed {
                return Ok(Outcome::Retry(
                    "earlier_steps_pending",
                    "the email waits for every earlier step".into(),
                ));
            }
            // A send that may have happened (a crash or a lapsed lease mid-call) is never repeated automatically.
            if data.get("email_attempted").and_then(Value::as_bool) == Some(true) {
                return Ok(Outcome::Fail(
                    "invite_email_uncertain",
                    "an earlier attempt may have sent the email: check the inbox before Retry"
                        .into(),
                ));
            }
            remember(st, a.id, "invite_email", &json!({"email_attempted": true})).await?;
            invite_email(sg, kc, a, &sub).await
        }
        other => Outcome::Fail("unknown_step", format!("no such step {other}")),
    })
}

/// fr's rule: several words -> the first, then the rest; ONE word -> that word as both; empty -> neither.
pub fn split_name(name: &str) -> (Option<&str>, Option<String>) {
    let mut parts = name.split_whitespace();
    let Some(first) = parts.next() else {
        return (None, None);
    };
    let rest = parts.collect::<Vec<_>>().join(" ");
    if rest.is_empty() {
        return (Some(first), Some(first.to_string()));
    }
    (Some(first), Some(rest))
}

/// The `keycloak_user` step: find the login by email; one stamped by this signup is ours, any other is refused; none
/// -> create it with names and the stamp (a 409 means it appeared concurrently: look again next attempt).
async fn keycloak_user(
    st: &AppState,
    kc: &crate::signup::keycloak_admin::KeycloakAdmin,
    a: &Approval,
) -> ApiResult<Outcome> {
    let stamp = a.id.to_string();
    Ok(match kc.find_by_email(&a.email).await {
        Err(e) => upstream("keycloak_unavailable", &e),
        Ok(Some(sub)) => match kc.attribute(&sub, STAMP_ATTR).await {
            Err(e) => upstream("keycloak_unavailable", &e),
            Ok(Some(s)) if s == stamp => Outcome::Ok(json!({"sub": sub, "existing": false})),
            Ok(_) => Outcome::Fail(
                IDENTITY_EXISTS,
                "a care-bus login already exists for this address and this signup did not create it".into(),
            ),
        },
        Ok(None) => {
            remember(st, a.id, "keycloak_user", &json!({"create_attempted": true})).await?;
            let (first, last) = split_name(&a.name);
            match kc
                .create_user(&a.email, first, last.as_deref(), &[(STAMP_ATTR, stamp.as_str())])
                .await
            {
                Err(e) => upstream("keycloak_create_failed", &e),
                Ok(Some(sub)) => Outcome::Ok(json!({"sub": sub, "existing": false})),
                Ok(None) => Outcome::Retry(
                    "keycloak_user_conflict",
                    "Keycloak answered 409 to the create; looking it up again".into(),
                ),
            }
        }
    })
}

/// The `tenant` step: the workspace under an id remembered first, `updated_by = signup:<id>` in its own INSERT, plus
/// its job, then the scope registry after COMMIT.
async fn found_tenant(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    input: &TenantInput<'_>,
    data: &Value,
) -> ApiResult<Outcome> {
    let remembered = data
        .get("tenant_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok());
    let tenant = match remembered {
        Some(t) => t,
        None => {
            let t = Uuid::new_v4();
            remember(st, a.id, "tenant", &json!({"tenant_id": t})).await?;
            t
        }
    };
    let marker = format!("signup:{}", a.id);
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let existing: Option<(String, String)> =
        sqlx::query_as("SELECT name, updated_by FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await?;
    let name = match existing {
        Some((name, by)) if by.starts_with(&marker) => name,
        Some(_) => {
            tx.commit().await?;
            return Ok(Outcome::Fail(
                "tenant_not_ours",
                "the remembered workspace id belongs to a workspace this signup did not found"
                    .into(),
            ));
        }
        None => {
            let name: String = input.name.chars().take(120).collect();
            sqlx::query(
                "INSERT INTO tenants (id, name, status, updated_by) VALUES ($1, $2, 'active', $3)",
            )
            .bind(tenant)
            .bind(&name)
            .bind(&marker)
            .execute(&mut *tx)
            .await?;
            audit::record(
                &mut tx,
                TICK_ACTOR,
                "system",
                "tenant.upsert",
                "tenant",
                &tenant.to_string(),
                true,
                json!({"signup": a.id, "status": "active", "via": "early_access"}),
            )
            .await?;
            name
        }
    };
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label) VALUES ($1, $2, 'Customer care') ON CONFLICT (tenant_id, key) DO NOTHING",
    )
    .bind(tenant)
    .bind(&sg.cfg.job_key)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if let Err(e) = scopes::add(
        &st.bus,
        &Scope {
            tenant,
            job: sg.cfg.job_key.clone(),
        },
    )
    .await
    {
        return Ok(Outcome::Retry(
            "scope_registry_failed",
            format!("{e:#}").chars().take(200).collect(),
        ));
    }
    Ok(Outcome::Ok(json!({"tenant_id": tenant, "name": name})))
}

/// The `users` row of the minted login: the workspace's tenant_admin. A login seated elsewhere is refused.
async fn membership(st: &AppState, a: &Approval, tenant: Uuid, sub: &str) -> ApiResult<Outcome> {
    let home: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM carebus_auth_user($1)")
        .bind(sub)
        .fetch_optional(&st.db)
        .await?;
    match home {
        Some(t) if t == tenant => return Ok(Outcome::Ok(json!({}))),
        Some(_) => {
            return Ok(Outcome::Fail(
                BELONGS_ELSEWHERE,
                "the requester's login already belongs to another workspace".into(),
            ));
        }
        None => {}
    }
    let name: String = a.name.chars().take(120).collect();
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let inserted = sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by) \
         VALUES ($1, $2, 'tenant_admin', $3, $4) ON CONFLICT (sub) DO NOTHING",
    )
    .bind(sub)
    .bind(tenant)
    .bind(&name)
    .bind(format!("signup:{}", a.id))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        audit::record(
            &mut tx,
            TICK_ACTOR,
            "system",
            "user.upsert",
            "user",
            sub,
            true,
            json!({"tenant_role": "tenant_admin", "signup": a.id}),
        )
        .await?;
        tx.commit().await?;
        return Ok(Outcome::Ok(json!({})));
    }
    // Under this tenant's RLS: visible = a concurrent runner won; invisible = the row is in ANOTHER tenant.
    let here: Option<String> = sqlx::query_scalar("SELECT sub FROM users WHERE sub = $1")
        .bind(sub)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(if here.is_some() {
        Outcome::Ok(json!({}))
    } else {
        Outcome::Fail(
            "user_in_other_tenant",
            "the requester belongs to another workspace".into(),
        )
    })
}

/// The `base_release` step: the workspace's job on the current base release of its job key (migration 0021).
async fn adopt_base_release(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    tenant: Uuid,
) -> ApiResult<Outcome> {
    let (outcome, release): (String, Option<String>) = sqlx::query_as(
        "SELECT adopt_outcome, adopt_release FROM carebus_adopt_base_release($1, $2, $3, $4)",
    )
    .bind(tenant)
    .bind(&sg.cfg.job_key)
    .bind(sg.cfg.base_tenant)
    .bind(format!("signup:{}", a.id))
    .fetch_one(&st.db)
    .await?;
    Ok(match outcome.as_str() {
        "adopted" | "already_current" => {
            Outcome::Ok(json!({"release_id": release, "outcome": &outcome}))
        }
        "no_base_release" => Outcome::Skip(
            "no_base_release",
            format!(
                "job {} has no base release to start the workspace on; it starts empty",
                sg.cfg.job_key
            ),
        ),
        "no_job" => Outcome::Fail(
            "job_missing",
            format!("the workspace has no {} job", sg.cfg.job_key),
        ),
        other => Outcome::Fail("base_release_failed", format!("unexpected outcome {other}")),
    })
}

/// The tenant's name, read inside its own context (lesson #1).
async fn tenant_name_of(st: &AppState, tenant: Uuid) -> Result<String, UpstreamError> {
    let mut tx = begin_scoped(&st.db, tenant, None)
        .await
        .map_err(|e| UpstreamError::new(None, e.to_string()))?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    name.ok_or_else(|| UpstreamError::new(Some(404), "the care-bus tenant does not exist"))
}

/// What [`tenant_licence_set`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenceSet {
    /// Care-bus owns the tenant and ensured its licence set.
    Ensured,
    /// Another app owns the tenant (care-bus a grantee); its owner already ensured the care-bus licence set.
    HeldByOwner(String),
    /// Care-bus does not own the tenant and it holds no care-bus licence set: only its owner can ensure one
    /// (`tenant_owner_only`), so there is nothing for care-bus to retry. The owner when licences named it.
    NotOwner(Option<String>),
}

impl LicenceSet {
    /// The `not_owner` detail: who owns the tenant (no secrets, no personal data).
    pub fn not_owner_message(owner: Option<&str>) -> String {
        match owner {
            Some(o) => format!(
                "tenant owned by licences owner {o}; care-bus is a grantee and its care-bus bundle is not active \
                 (only the owner can ensure it)"
            ),
            None => {
                "tenant registered in licences to another owning app that granted care-bus nothing \
                     (tenant_owned_by_other_app)"
                    .into()
            }
        }
    }
}

/// The tenant registered in licences and its licence set (bundle) ensured, by care-bus when care-bus OWNS it. A
/// tenant another app owns is never ensured (licences answers a grantee 403 `tenant_owner_only`, by rule): it is
/// settled by whether its owner already ensured the care-bus bundle (GET /licenses/active for the bundle's service
/// subject, a read any internal-service caller may make on any tenant). Idempotent.
pub async fn tenant_licence_set(
    st: &AppState,
    lic: &LicencesAdmin,
    tenant: Uuid,
) -> Result<LicenceSet, UpstreamError> {
    let name = tenant_name_of(st, tenant).await?;
    match lic.register_tenant(tenant, &name).await? {
        TenantOwner::Caller => {
            lic.ensure_bundle(tenant).await?;
            Ok(LicenceSet::Ensured)
        }
        TenantOwner::Granted(owner) => Ok(match lic.bundle_licence(tenant).await? {
            Some(_) => LicenceSet::HeldByOwner(owner),
            None => LicenceSet::NotOwner(Some(owner)),
        }),
        TenantOwner::Other => Ok(LicenceSet::NotOwner(None)),
    }
}

/// [`tenant_licence_set`] for the flows that need the set in place (approve, licence on login): a tenant whose set
/// care-bus cannot have is a refusal (409 `not_owner`).
pub async fn tenant_licences(
    st: &AppState,
    lic: &LicencesAdmin,
    tenant: Uuid,
) -> Result<(), UpstreamError> {
    match tenant_licence_set(st, lic, tenant).await? {
        LicenceSet::Ensured | LicenceSet::HeldByOwner(_) => Ok(()),
        LicenceSet::NotOwner(owner) => Err(UpstreamError::new(
            Some(409),
            format!(
                "licences tenant: not_owner: {}",
                LicenceSet::not_owner_message(owner.as_deref())
            ),
        )),
    }
}

/// `sub`'s member licence on `tenant`, recorded in `member_licences` inside the tenant context. Idempotent.
pub async fn record_member_licence(
    st: &AppState,
    lic: &LicencesAdmin,
    tenant: Uuid,
    sub: &str,
) -> Result<String, UpstreamError> {
    let id = lic.member_licence(tenant, sub).await?;
    let mut tx = begin_scoped(&st.db, tenant, None)
        .await
        .map_err(|e| UpstreamError::new(None, e.to_string()))?;
    sqlx::query(
        "INSERT INTO member_licences (sub, tenant_id, license_id) VALUES ($1, $2, $3) \
         ON CONFLICT (sub) DO UPDATE SET license_id = EXCLUDED.license_id, tenant_id = EXCLUDED.tenant_id, \
                                        ensured_at = now()",
    )
    .bind(sub)
    .bind(tenant)
    .bind(&id)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(id)
}

/// Licence on login: the whole chain for a login that has none recorded (tenant registered, licence set, member
/// licence). Idempotent at every step.
pub async fn ensure_member_licence(
    st: &AppState,
    lic: &LicencesAdmin,
    tenant: Uuid,
    sub: &str,
) -> Result<String, UpstreamError> {
    tenant_licences(st, lic, tenant).await?;
    record_member_licence(st, lic, tenant, sub).await
}

/// The one email (C5): `locale` + `signup_app` first (non-fatal), then execute-actions-email.
async fn invite_email(
    sg: &Signup,
    kc: &crate::signup::keycloak_admin::KeycloakAdmin,
    a: &Approval,
    sub: &str,
) -> Outcome {
    let locale = a.locale.as_deref().unwrap_or(DEFAULT_LOCALE);
    if let Err(e) = kc
        .update_attributes(
            sub,
            &[
                ("locale", Some(locale)),
                ("signup_app", Some(sg.cfg.app_key.as_str())),
            ],
        )
        .await
    {
        // Without them Keycloak sends its generic email in the realm default language: still the right link.
        tracing::warn!(signup = %a.id, error = %e, "signup_email_attributes_failed");
    }
    let redirect = format!("{}/", sg.public_url());
    let to = ActionsEmail {
        actions: &INVITE_ACTIONS,
        client_id: &sg.cfg.frontend_client_id,
        redirect_uri: &redirect,
        lifespan_seconds: sg.cfg.invite_email_lifespan_seconds,
    };
    match kc.execute_actions_email(sub, &to).await {
        Ok(()) => Outcome::Ok(json!({"locale": locale})),
        // No answer: it may have been sent. Never re-sent automatically (a resend rotates the link).
        Err(e) if e.status.is_none() => Outcome::Fail(
            "invite_email_uncertain",
            format!(
                "{}; it may have been sent: check the inbox before Retry",
                e.message
            ),
        ),
        // A 5xx may come from a proxy after Keycloak sent it: uncertain too. Nothing here is retried automatically.
        Err(e) if e.status.is_some_and(|s| s >= 500) => Outcome::Fail(
            "invite_email_uncertain",
            format!(
                "{}; it may have been sent: check the inbox before Retry",
                e.message
            ),
        ),
        Err(e) => Outcome::Fail("invite_email_failed", e.message.chars().take(400).collect()),
    }
}

/// How the requester was told (journal, Telegram). No personal data.
pub fn email_note(steps: &[StepRow]) -> Option<String> {
    let r = steps.iter().find(|r| r.step == "invite_email")?;
    match r.status.as_str() {
        "ok" => Some("set-password email sent".into()),
        "failed" if r.code.as_deref() == Some("invite_email_uncertain") => {
            Some("email outcome UNKNOWN (no answer from Keycloak); check before Retry".into())
        }
        "failed" => Some("email FAILED - Retry it on the onboarding page".into()),
        _ => None,
    }
}

/// APPROVED signups whose FIRST outstanding step is pending, due and unclaimed.
async fn due(st: &AppState) -> ApiResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT signup_id FROM ( \
             SELECT DISTINCT ON (s.signup_id) s.signup_id, s.status, s.next_attempt_at, s.lease_until \
               FROM signup_provisioning_steps s \
               JOIN signup_approvals a ON a.id = s.signup_id AND a.status = 'approved' \
              WHERE s.status IN ('pending', 'failed') \
              ORDER BY s.signup_id, s.ord) AS head \
          WHERE status = 'pending' AND next_attempt_at <= now() \
            AND (lease_until IS NULL OR lease_until < now()) \
          LIMIT 20",
    )
    .fetch_all(&st.db)
    .await?)
}

/// Rejected signups whose repair is due.
async fn repairs_due(st: &AppState) -> ApiResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM signup_approvals WHERE status = 'rejected' AND repair_next_at <= now() \
          ORDER BY repair_next_at LIMIT 20",
    )
    .fetch_all(&st.db)
    .await?)
}

/// One pass of the retry tick: the signups it ran and the repairs it retried.
pub async fn tick(st: &AppState, sg: &Signup) -> ApiResult<(usize, usize)> {
    if !sg.enabled() {
        return Ok((0, 0));
    }
    let ids = due(st).await?;
    for id in &ids {
        if let Err(e) = run(st, sg, *id).await {
            tracing::warn!(signup = %id, error = %e, "signup_provisioning_tick_failed");
        }
    }
    let repairs = repairs_due(st).await?;
    for id in &repairs {
        // Take the row off the due list first so two replicas do not repair it at once.
        let taken: Option<Approval> = {
            let mut tx = st.db.begin().await?;
            let a = store::lock(&mut tx, *id).await?;
            let a = match a {
                Some(a) => {
                    let n = sqlx::query(
                        "UPDATE signup_approvals SET repair_next_at = now() + interval '10 minutes' \
                          WHERE id = $1 AND repair_next_at <= now()",
                    )
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                    (n == 1).then_some(a)
                }
                None => None,
            };
            tx.commit().await?;
            a
        };
        if let Some(a) = taken
            && let Err(e) = repair(st, sg, &a).await
        {
            tracing::warn!(signup = %id, error = %e, "signup_reject_repair_tick_failed");
        }
    }
    Ok((ids.len(), repairs.len()))
}

/// What a `retry` did.
pub enum Retried {
    NotFound,
    NotApproved(String),
    Steps {
        requeued: Vec<String>,
        steps: Vec<StepRow>,
    },
}

/// The super_admin Retry: every `failed` step of an APPROVED signup is set `pending` and the steps run inline.
pub async fn retry(st: &AppState, sg: &Signup, id: Uuid) -> ApiResult<Retried> {
    let mut tx = st.db.begin().await?;
    let Some(a) = store::lock(&mut tx, id).await? else {
        return Ok(Retried::NotFound);
    };
    if a.status != "approved" {
        return Ok(Retried::NotApproved(a.status));
    }
    let requeued: Vec<String> = sqlx::query_scalar(
        "UPDATE signup_provisioning_steps \
            SET status = 'pending', attempts = 0, next_attempt_at = now(), code = NULL, message = NULL, \
                lease_until = NULL, updated_at = now(), data = data - 'email_attempted' \
          WHERE signup_id = $1 AND status = 'failed' RETURNING step",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    if !requeued.is_empty() {
        tracing::info!(signup = %id, steps = ?requeued, "signup_retry");
        if let Err(e) = run(st, sg, id).await {
            tracing::warn!(signup = %id, error = %e, "signup_retry_run_failed");
        }
    }
    let mut tx = st.db.begin().await?;
    let steps = store::steps(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Retried::Steps { requeued, steps })
}

/// At most this many tenants per reconcile pass.
const RECONCILE_BATCH: i32 = 20;

/// Tenant licence-set reconcile (`CAREBUS_TENANT_BUNDLE_RECONCILE`): every ACTIVE tenant (the bootstrap tenant and
/// any provisioned before early access) is registered in licences and its licence set (bundle) ensured ONCE, so a
/// per-tenant strata token can be minted for it. Recorded in `tenant_bundle_reconcile` (migration 0022): a tenant is
/// claimed (`lease_until`) by one runner, `ok` is final, a transient failure (transport, 5xx, 408, 429) is retried
/// with the step backoff up to [`MAX_ATTEMPTS`], anything else is `failed` (an operator looks). A tenant care-bus
/// does not own in licences (migration 0025) is `ok` when its owner already ensured the care-bus set, else the
/// terminal `not_owner` (naming the owner): never an ensure care-bus is refused, never a retry. The licences calls
/// are idempotent, so a run lost mid-way is simply repeated. At most [`RECONCILE_BATCH`] tenants per pass, read
/// first by `carebus_bundle_reconcile_due` (only those not yet ok and due). Returns how many this
/// pass attempted.
pub async fn reconcile_bundles(st: &AppState, sg: &Signup) -> ApiResult<usize> {
    if !sg.cfg.bundle_reconcile {
        return Ok(0);
    }
    let Some(lic) = sg.licences.as_ref() else {
        return Ok(0);
    };
    // Only the tenants still to do (no row yet, or pending and due): a settled tenant costs no transaction.
    let due: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM carebus_bundle_reconcile_due($1)")
        .bind(RECONCILE_BATCH)
        .fetch_all(&st.db)
        .await?;
    let mut attempted = 0;
    for tenant in due {
        // The row is tenant data (RLS): seeded and claimed inside the tenant's own context.
        let mut tx = begin_scoped(&st.db, tenant, None).await?;
        sqlx::query(
            "INSERT INTO tenant_bundle_reconcile (tenant_id, status, next_attempt_at) \
             VALUES ($1, 'pending', now()) ON CONFLICT DO NOTHING",
        )
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
        let claimed: Option<i32> = sqlx::query_scalar(
            "UPDATE tenant_bundle_reconcile \
                SET lease_until = now() + make_interval(secs => $2), updated_at = now() \
              WHERE tenant_id = $1 AND status = 'pending' AND next_attempt_at <= now() \
                AND (lease_until IS NULL OR lease_until < now()) \
              RETURNING attempts",
        )
        .bind(tenant)
        .bind(LEASE_SECS)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(attempts) = claimed else { continue };
        attempted += 1;
        let attempts = attempts + 1;
        let (status, message, next): (&str, Option<String>, Option<f64>) =
            match tenant_licence_set(st, lic, tenant).await {
                Ok(LicenceSet::Ensured) => ("ok", None, None),
                Ok(LicenceSet::HeldByOwner(owner)) => (
                    "ok",
                    Some(format!(
                        "care-bus bundle active, ensured by licences owner {owner}"
                    )),
                    None,
                ),
                Ok(LicenceSet::NotOwner(owner)) => (
                    "not_owner",
                    Some(LicenceSet::not_owner_message(owner.as_deref())),
                    None,
                ),
                Err(e) => {
                    let msg: String = e.message.chars().take(400).collect();
                    let transient = e.status.is_none_or(|s| s >= 500 || s == 408 || s == 429);
                    if transient && attempts < MAX_ATTEMPTS {
                        ("pending", Some(msg), Some(backoff_secs(attempts)))
                    } else {
                        ("failed", Some(msg), None)
                    }
                }
            };
        let mut tx = begin_scoped(&st.db, tenant, None).await?;
        sqlx::query(
            "UPDATE tenant_bundle_reconcile \
                SET status = $2, message = $3, attempts = $4, lease_until = NULL, updated_at = now(), \
                    next_attempt_at = CASE WHEN $5::float8 IS NULL THEN NULL \
                                           ELSE now() + make_interval(secs => $5::float8) END \
              WHERE tenant_id = $1",
        )
        .bind(tenant)
        .bind(status)
        .bind(&message)
        .bind(attempts)
        .bind(next)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if status == "ok" {
            tracing::info!(%tenant, "tenant_bundle_reconciled");
        } else if status == "not_owner" {
            // Terminal: logged once, the row is never selected again.
            tracing::info!(%tenant, detail = message.as_deref().unwrap_or_default(), "tenant_bundle_reconcile_not_owner");
        } else {
            tracing::warn!(%tenant, status, attempts, "tenant_bundle_reconcile_failed");
        }
    }
    Ok(attempted)
}

/// The provisioning tick, every `SIGNUP_TICK_SECONDS`. Never stops on an error; idle while signup is off (the
/// tenant licence-set reconcile runs on its own flag).
pub fn spawn_tick(st: AppState) -> tokio::task::JoinHandle<()> {
    let every = Duration::from_secs(st.signup.cfg.tick_seconds);
    tokio::spawn(async move {
        let mut t = tokio::time::interval(every);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            let sg = st.signup.clone();
            match tick(&st, &sg).await {
                Ok((0, 0)) => {}
                Ok((runs, repairs)) => tracing::info!(runs, repairs, "signup_provisioning_tick"),
                Err(e) => tracing::warn!(error = %e, "signup_provisioning_tick_failed"),
            }
            if let Err(e) = reconcile_bundles(&st, &sg).await {
                tracing::warn!(error = %e, "tenant_bundle_reconcile_tick_failed");
            }
            match crate::signup::removal::tick(&st, &sg).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(removals = n, "signup_removal_tick"),
                Err(e) => tracing::warn!(error = %e, "signup_removal_tick_failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_six_hours() {
        assert_eq!(backoff_secs(0), 60.0);
        assert_eq!(backoff_secs(1), 60.0);
        assert_eq!(backoff_secs(2), 120.0);
        assert_eq!(backoff_secs(4), 480.0);
        assert_eq!(backoff_secs(20), 6.0 * 3600.0);
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
        assert!(retry(Some(503)));
        assert!(retry(Some(429)));
        assert!(retry(Some(408)));
        assert!(!retry(Some(400)));
        assert!(!retry(Some(403)));
    }

    #[test]
    fn split_name_follows_the_fr_rule() {
        assert_eq!(
            split_name("Gregorius Dekker"),
            (Some("Gregorius"), Some("Dekker".to_string()))
        );
        assert_eq!(
            split_name(" Anna \t van  der\nBerg "),
            (Some("Anna"), Some("van der Berg".to_string()))
        );
        assert_eq!(split_name("Cher"), (Some("Cher"), Some("Cher".to_string())));
        assert_eq!(split_name(" \t\n "), (None, None));
    }

    #[test]
    fn a_refused_gating_step_is_409_with_its_journal() {
        let e = not_landed(IDENTITY_EXISTS, "m".into(), &[]);
        assert_eq!(
            e.status_and_code(),
            (axum::http::StatusCode::CONFLICT, IDENTITY_EXISTS)
        );
        let e = not_landed("keycloak_unavailable", "m".into(), &[]);
        assert_eq!(e.status_and_code().0, axum::http::StatusCode::BAD_GATEWAY);
    }
}
