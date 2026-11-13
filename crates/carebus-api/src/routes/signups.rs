//! Early-access signup routes (care-bus 17; agent `routes/signups.rs`). Every route answers 503
//! `signup_not_configured` while `CAREBUS_SIGNUP_ENABLED` is off or a part the approve needs is missing.
//!
//! - `POST /api/signup-approvals/intake` — the intake handler's client-credentials token (azp
//!   `care-bus-signup-intake`, realm role of the same name, issuer `SIGNUP_INTAKE_ISSUER` = secure-apps). Strict body
//!   `{email, name, company?, message?, source: web|chat, locale?}`. 202 `{"status":"queued"}` | `{"status":
//!   "already_pending"}`; 503 `signup_queue_full` past `SIGNUP_QUEUE_MAX_PENDING`.
//! - `GET /api/signup-approvals?status=&limit=&before=` — super_admin: the queue with each row's suggested workspace
//!   name and journal. `GET /api/signup-approvals/{id}` — one row.
//! - `POST /api/signup-approvals/{id}/approve` `{tenantName?}` — super_admin: the journaled approve. 404
//!   `signup_not_found`, 409 `signup_not_pending` | `approval_in_progress` | `approval_claim_lost` | `identity_exists`
//!   | `user_belongs_to_another_workspace`, 502 `<step code>` with `error.steps` (nothing decided).
//! - `POST /api/signup-approvals/invite` `{email, name, company?, tenantName?, locale?}` — super_admin: the
//!   pre-approved invite (`source = 'invite'`), then the same approve.
//! - `POST /api/signup-approvals/{id}/reject` `{reason?}` — super_admin; `reject_repair` says what was undone.
//! - `POST /api/signup-approvals/{id}/retry` — super_admin: every failed step of an approved signup runs again.
//! - `POST /api/signup-approvals/{id}/remove` `{confirm}` — super_admin: remove an APPROVED signup end to end
//!   (crate::signup::removal): `confirm` must be the workspace slug (`tenant_slug` of `GET …/{id}`). 200 with the
//!   signup and its `removal` (`state` pending | blocked | failed | done, journal); 409 `signup_not_approved` |
//!   `removal_exists` | `signup_no_workspace` | `tenant_mismatch` | `provisioning_in_progress`, 422
//!   `removal_confirm_mismatch`.
//! - `POST /api/signup-approvals/{id}/removal/retry` — super_admin: a pending / blocked / failed removal runs again
//!   (its failed and blocked steps reset). 409 `signup_not_removed` | `removal_done` | `removal_in_progress`.
//! - `POST /api/signup-approvals/{id}/telegram-decision` `{action, telegramUserId}` — chat-bridge's decider token
//!   (azp `chat-bridge-signup-decider-care-bus`, role `care-bus-signup-decide`, issuer `SIGNUP_DECIDER_ISSUER`), C3:
//!   200 `{status: approved|rejected|already_decided, tenantId?, outcome?, summary}`, 502 `{status:"failed", code,
//!   summary}` when it could not be carried out (the row stays pending).
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::ServiceClaims;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::signup::provisioning::{self, Decision, TenantInput};
use crate::signup::removal;
use crate::signup::store::{self, Approval, NewSignup, StepRow};
use crate::signup::telegram::{self, Decided};
use crate::signup::{
    SIGNUP_LOCALES, Signup, default_tenant_name, email_address, multi_line_field, normalise_locale,
    require_decider, require_intake, single_line_field, tenant_name,
};
use crate::state::AppState;
use crate::tenancy::Provisioner;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/signup-approvals", get(list))
        .route("/api/signup-approvals/intake", post(intake))
        .route("/api/signup-approvals/invite", post(invite))
        .route("/api/signup-approvals/{id}", get(get_one))
        .route("/api/signup-approvals/{id}/approve", post(approve))
        .route("/api/signup-approvals/{id}/reject", post(reject))
        .route("/api/signup-approvals/{id}/retry", post(retry))
        .route("/api/signup-approvals/{id}/remove", post(remove))
        .route(
            "/api/signup-approvals/{id}/removal/retry",
            post(removal_retry),
        )
        .route(
            "/api/signup-approvals/{id}/telegram-decision",
            post(telegram_decision),
        )
}

const MAX_TEXT: usize = 160;
const MAX_MESSAGE: usize = 2000;
const MAX_REASON: usize = 500;
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;
/// Where a decision taken on the screen says it came from.
const VIA_UI: &str = "care-bus onboarding page";

/// The signup service, or 503 `signup_not_configured` (read per call).
fn signup(st: &AppState) -> ApiResult<Arc<Signup>> {
    if st.signup.enabled() {
        Ok(st.signup.clone())
    } else {
        Err(ApiError::Unavailable(
            "signup_not_configured",
            "early-access signup is not enabled for care-bus".into(),
        ))
    }
}

fn invalid(msg: &str) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.into())
}

/// What the intake handler sends (guide: `{email,name,company?,message?,source,locale?}`). Strict, camelCase.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Intake {
    email: String,
    name: String,
    company: Option<String>,
    message: Option<String>,
    source: String,
    locale: Option<String>,
}

async fn intake(
    State(st): State<AppState>,
    caller: ServiceClaims,
    ApiJson(b): ApiJson<Intake>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let sg = signup(&st)?;
    let client = require_intake(&sg.cfg, &caller)?;
    let email = email_address("email", &b.email)?;
    let name = single_line_field("name", &b.name, MAX_TEXT)?;
    let company = b
        .company
        .as_deref()
        .map(|c| single_line_field("company", c, MAX_TEXT))
        .transpose()?;
    let message = match b.message.as_deref() {
        Some(m) => multi_line_field("message", m, MAX_MESSAGE)?,
        None => None,
    };
    let source = match b.source.as_str() {
        "web" | "chat" => b.source.clone(),
        _ => return Err(invalid("source must be web or chat")),
    };
    let locale = normalise_locale(b.locale.as_deref());

    let mut tx = st.db.begin().await?;
    store::lock_queue(&mut tx).await?;
    // A re-submit of an address that already waits is `already_pending`, also when the queue is full.
    if store::pending_of(&mut tx, &email).await?.is_some() {
        tx.commit().await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({"status": "already_pending"})),
        ));
    }
    let pending = store::pending_count(&mut tx).await?;
    if pending >= sg.cfg.max_pending {
        tracing::warn!(pending, cap = sg.cfg.max_pending, intake_client = %client, "signup_queue_full");
        return Err(ApiError::Unavailable(
            "signup_queue_full",
            "sign-up requests are temporarily closed; try again later".into(),
        ));
    }
    let new = NewSignup {
        email: &email,
        name: &name,
        company: company.as_deref(),
        message: message.as_deref(),
        source: &source,
        locale,
        intake_client: &client,
    };
    let Some(id) = store::insert(&mut tx, &new).await? else {
        tx.commit().await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({"status": "already_pending"})),
        ));
    };
    tx.commit().await?;
    // Ids only in the log: the requester's address and name never reach it.
    tracing::info!(signup = %id, %source, intake_client = %client, "signup_intake_queued");
    let notice = telegram::NewSignup {
        id,
        name: &name,
        email: &email,
        company: company.as_deref(),
        message: message.as_deref(),
        source: &source,
        locale,
        pending: pending + 1,
    };
    telegram::send(
        &sg.cfg,
        &sg.http,
        telegram::new_signup_payload(&sg.cfg, &notice),
        "new_signup",
    );
    Ok((StatusCode::ACCEPTED, Json(json!({"status": "queued"}))))
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    limit: Option<String>,
    before: Option<String>,
}

fn invalid_query(msg: &str) -> ApiError {
    ApiError::BadRequest("invalid_query", msg.into())
}

fn cursor(raw: Option<&str>) -> ApiResult<Option<(i64, Uuid)>> {
    let Some(s) = raw.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let bad = || invalid_query("before must be a cursor returned as next by this list");
    let (us, id) = s.split_once('.').ok_or_else(bad)?;
    Ok(Some((
        us.parse().map_err(|_| bad())?,
        Uuid::parse_str(id).map_err(|_| bad())?,
    )))
}

fn step_json(r: &StepRow) -> Value {
    json!({
        "step": r.step,
        "status": r.status,
        "code": r.code,
        "message": r.message,
        "attempts": r.attempts,
        "at": r.last_attempt_at,
        "next_attempt_at": r.next_attempt_at,
    })
}

fn item(a: &Approval, steps: &[&StepRow]) -> Value {
    let mut v = json!(a);
    v["suggested_tenant_name"] = json!(default_tenant_name(
        a.company.as_deref(),
        &a.email,
        Some(a.name.as_str())
    ));
    v["steps"] = Value::Array(steps.iter().map(|s| step_json(s)).collect());
    v
}

async fn list(
    State(st): State<AppState>,
    Provisioner(_): Provisioner,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let status = q.status.as_deref().filter(|s| !s.is_empty() && *s != "all");
    if let Some(s) = status
        && !matches!(
            s,
            "pending" | "approved" | "rejected" | "removed" | "decided"
        )
    {
        return Err(invalid_query(
            "status must be pending, approved, rejected, removed, decided or all",
        ));
    }
    let limit = match q.limit.as_deref() {
        None | Some("") => DEFAULT_LIMIT,
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| invalid_query("limit must be an integer from 1 to 200"))?,
    };
    let before = cursor(q.before.as_deref())?;
    let mut tx = st.db.begin().await?;
    let mut rows = store::list(&mut tx, status, before, limit + 1).await?;
    let next = if rows.len() as i64 > limit {
        rows.truncate(usize::try_from(limit).unwrap_or(0));
        rows.last().map(|a| format!("{}.{}", a.ts_us, a.id))
    } else {
        None
    };
    let ids: Vec<Uuid> = rows.iter().map(|a| a.id).collect();
    let journal = store::steps_many(&mut tx, &ids).await?;
    let mut by_id: HashMap<Uuid, Vec<&StepRow>> = HashMap::new();
    for (id, s) in &journal {
        by_id.entry(*id).or_default().push(s);
    }
    let items: Vec<Value> = rows
        .iter()
        .map(|a| item(a, by_id.get(&a.id).map(Vec::as_slice).unwrap_or_default()))
        .collect();
    let (received, pending, approved, rejected, removed) = store::counts(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "items": items,
        "next": next,
        "queue_cap": sg.cfg.max_pending,
        "pending": pending,
        "counts": {"received": received, "approved": approved, "rejected": rejected, "removed": removed},
        "as_of": chrono::Utc::now(),
    })))
}

fn signup_not_found(id: &str) -> ApiError {
    ApiError::Detailed(
        StatusCode::NOT_FOUND,
        "signup_not_found",
        format!("no signup {id:?}"),
        json!({}),
    )
}

fn signup_not_pending(a: &Approval) -> ApiError {
    ApiError::Conflict(
        "signup_not_pending",
        format!("this signup was {} already", a.status),
    )
}

#[derive(Deserialize)]
struct IdPath {
    id: String,
}

fn signup_id(raw: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(raw).map_err(|_| signup_not_found(raw))
}

async fn get_one(
    State(st): State<AppState>,
    Provisioner(_): Provisioner,
    ApiPath(p): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    signup(&st)?;
    let id = signup_id(&p.id)?;
    Ok(Json(detail(&st, id, &p.id).await?))
}

/// One signup with its journal, the workspace slug a removal is confirmed with, and its removal.
async fn detail(st: &AppState, id: Uuid, raw: &str) -> ApiResult<Value> {
    let mut tx = st.db.begin().await?;
    let a = store::get(&mut tx, id)
        .await?
        .ok_or_else(|| signup_not_found(raw))?;
    let steps = store::steps(&mut tx, id).await?;
    tx.commit().await?;
    let refs: Vec<&StepRow> = steps.iter().collect();
    let mut v = item(&a, &refs);
    v["tenant_slug"] = json!(removal::slug_of(st, &a).await?);
    v["removal"] = removal::view(st, &a).await?;
    Ok(v)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveBody {
    confirm: String,
}

/// Remove an APPROVED signup (crate::signup::removal), confirmed with the workspace slug.
async fn remove(
    State(st): State<AppState>,
    Provisioner(caller): Provisioner,
    ApiPath(p): ApiPath<IdPath>,
    ApiJson(b): ApiJson<RemoveBody>,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let id = signup_id(&p.id)?;
    let actor = removal::Actor {
        id: &caller.actor,
        kind: caller.actor_type(),
    };
    removal::start(&st, &sg, id, &b.confirm, actor).await?;
    Ok(Json(detail(&st, id, &p.id).await?))
}

/// Retry a pending / blocked / failed removal.
async fn removal_retry(
    State(st): State<AppState>,
    Provisioner(caller): Provisioner,
    ApiPath(p): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let id = signup_id(&p.id)?;
    let actor = removal::Actor {
        id: &caller.actor,
        kind: caller.actor_type(),
    };
    removal::retry(&st, &sg, id, actor).await?;
    Ok(Json(detail(&st, id, &p.id).await?))
}

/// An optional JSON body: none, `null` and `{}` all mean the defaults.
fn optional_body<T: Default + serde::de::DeserializeOwned>(body: &Bytes) -> ApiResult<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    Ok(serde_json::from_slice::<Option<T>>(body)
        .map_err(|e| invalid(&format!("The request body is not valid: {e}")))?
        .unwrap_or_default())
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ApproveBody {
    #[serde(alias = "tenant_name")]
    tenant_name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RejectBody {
    reason: Option<String>,
}

async fn after_approval(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    tenant: Uuid,
    name: &str,
    via: &str,
) -> ApiResult<Vec<StepRow>> {
    let mut tx = st.db.begin().await?;
    let steps = store::steps(&mut tx, a.id).await?;
    tx.commit().await?;
    let not_ok: Vec<(String, String, Option<String>)> = steps
        .iter()
        .filter(|r| r.status != "ok" && r.status != "skipped")
        .map(|r| (r.step.clone(), r.status.clone(), r.code.clone()))
        .collect();
    let note = provisioning::email_note(&steps);
    let d = Decided {
        id: a.id,
        approved: true,
        name: &a.name,
        email: &a.email,
        via,
        tenant: Some((tenant, name)),
        not_ok: &not_ok,
        email_note: note.as_deref(),
        repair: None,
    };
    telegram::send(
        &sg.cfg,
        &sg.http,
        telegram::decision_payload(&sg.cfg, &d),
        "decision",
    );
    Ok(steps)
}

fn repair_line(r: &Value) -> Option<String> {
    let o = r.as_object().filter(|o| !o.is_empty())?;
    Some(
        o.iter()
            .map(|(k, v)| format!("{k} {}", v.as_str().unwrap_or("?")))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn announce_rejection(sg: &Signup, a: &Approval, via: &str, repaired: &Value) {
    let line = repair_line(repaired);
    let d = Decided {
        id: a.id,
        approved: false,
        name: &a.name,
        email: &a.email,
        via,
        tenant: None,
        not_ok: &[],
        email_note: None,
        repair: line.as_deref(),
    };
    telegram::send(
        &sg.cfg,
        &sg.http,
        telegram::decision_payload(&sg.cfg, &d),
        "decision",
    );
}

async fn chosen_name(
    st: &AppState,
    id: Uuid,
    given: Option<String>,
) -> ApiResult<(Approval, String)> {
    let mut tx = st.db.begin().await?;
    let a = store::get(&mut tx, id)
        .await?
        .ok_or_else(|| signup_not_found(&id.to_string()))?;
    tx.commit().await?;
    let name = tenant_name(&match given {
        Some(n) => n,
        None => default_tenant_name(a.company.as_deref(), &a.email, Some(a.name.as_str())),
    })?;
    Ok((a, name))
}

fn approval_in_progress() -> ApiError {
    ApiError::Conflict(
        "approval_in_progress",
        "this signup is being approved right now; reload in a moment".into(),
    )
}

async fn approved(st: &AppState, sg: &Signup, d: Decision, raw_id: &str) -> ApiResult<Value> {
    match d {
        Decision::NotFound => Err(signup_not_found(raw_id)),
        Decision::AlreadyDecided(a) | Decision::Rejected(a, _) => Err(signup_not_pending(&a)),
        Decision::InProgress(_) => Err(approval_in_progress()),
        Decision::Approved {
            approval,
            tenant,
            tenant_name,
        } => {
            let steps = after_approval(st, sg, &approval, tenant, &tenant_name, VIA_UI).await?;
            let refs: Vec<&StepRow> = steps.iter().collect();
            Ok(json!({
                "signup": item(&approval, &refs),
                "tenant": {"id": tenant, "name": tenant_name, "status": "active"},
                "steps": steps.iter().map(step_json).collect::<Vec<_>>(),
            }))
        }
    }
}

async fn approve(
    State(st): State<AppState>,
    Provisioner(caller): Provisioner,
    ApiPath(p): ApiPath<IdPath>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let id = signup_id(&p.id)?;
    let b: ApproveBody = optional_body(&body)?;
    let (a, name) = chosen_name(&st, id, b.tenant_name).await?;
    if a.status != "pending" {
        return Err(signup_not_pending(&a));
    }
    let input = TenantInput { name: &name };
    let d = provisioning::approve(&st, &sg, id, &input, &caller.sub, "ui").await?;
    Ok(Json(approved(&st, &sg, d, &p.id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct InviteBody {
    email: String,
    name: String,
    company: Option<String>,
    #[serde(alias = "tenant_name")]
    tenant_name: Option<String>,
    locale: Option<String>,
}

/// The pre-approved invite (CP `inviteEarlyAccess`): record the signup (`source = 'invite'`; a pending row for the
/// address is reused and the chosen language wins), then the SAME approve. No new-signup notice, no queue ceiling.
async fn invite(
    State(st): State<AppState>,
    Provisioner(caller): Provisioner,
    ApiJson(b): ApiJson<InviteBody>,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let email = email_address("email", &b.email)?;
    let name = single_line_field("name", &b.name, MAX_TEXT)?;
    let company = b
        .company
        .as_deref()
        .map(|c| single_line_field("company", c, MAX_TEXT))
        .transpose()?;
    let locale = match b.locale.as_deref() {
        None => None,
        Some(l) => Some(
            SIGNUP_LOCALES
                .iter()
                .copied()
                .find(|x| *x == l)
                .ok_or_else(|| {
                    ApiError::Unprocessable(
                        "invalid_locale",
                        format!("locale must be one of {}", SIGNUP_LOCALES.join(", ")),
                    )
                })?,
        ),
    };
    let requested = b.tenant_name.as_deref().map(tenant_name).transpose()?;
    let mut tx = st.db.begin().await?;
    let inserted = store::insert_invite(
        &mut tx,
        &email,
        &name,
        company.as_deref(),
        locale,
        &caller.sub,
    )
    .await?;
    let (id, reused) = match inserted {
        Some(id) => (id, false),
        None => {
            let id = store::pending_of(&mut tx, &email).await?.ok_or_else(|| {
                ApiError::Conflict(
                    "signup_not_pending",
                    "the pending signup for this address was just decided; send the invite again"
                        .into(),
                )
            })?;
            if let Some(l) = locale {
                store::set_locale(&mut tx, id, l).await?;
            }
            (id, true)
        }
    };
    tx.commit().await?;
    tracing::info!(signup = %id, reused, "signup_invite_recorded");
    let (a, wname) = chosen_name(&st, id, requested).await?;
    if a.status != "pending" {
        return Err(signup_not_pending(&a));
    }
    let input = TenantInput { name: &wname };
    let d = provisioning::approve(&st, &sg, id, &input, &caller.sub, "ui").await?;
    let mut out = approved(&st, &sg, d, &id.to_string()).await?;
    out["reused_pending"] = json!(reused);
    Ok(Json(out))
}

async fn reject(
    State(st): State<AppState>,
    Provisioner(caller): Provisioner,
    ApiPath(p): ApiPath<IdPath>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let id = signup_id(&p.id)?;
    let b: RejectBody = optional_body(&body)?;
    let reason = match b.reason.as_deref() {
        Some(r) => multi_line_field("reason", r, MAX_REASON)?,
        None => None,
    };
    match provisioning::reject(&st, &sg, id, &caller.sub, "ui", reason.as_deref()).await? {
        Decision::NotFound => Err(signup_not_found(&p.id)),
        Decision::AlreadyDecided(a) | Decision::Approved { approval: a, .. } => {
            Err(signup_not_pending(&a))
        }
        Decision::InProgress(_) => Err(approval_in_progress()),
        Decision::Rejected(a, repaired) => {
            announce_rejection(&sg, &a, VIA_UI, &repaired);
            let mut v = item(&a, &[]);
            v["reject_repair"] = repaired;
            Ok(Json(v))
        }
    }
}

/// C3 body: strict.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TelegramDecision {
    action: String,
    telegram_user_id: i64,
}

/// At most 200 characters on one line (shown in Telegram).
fn summary(s: String) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 200 {
        flat
    } else {
        let mut t: String = flat.chars().take(199).collect();
        t.push('…');
        t
    }
}

fn c3(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn already_decided(a: &Approval) -> Response {
    let tenant = a.tenant_id.filter(|_| a.status == "approved");
    let mut v = json!({
        "status": "already_decided",
        "outcome": a.status,
        "summary": summary(format!(
            "Already {}: {}{}.",
            a.status,
            a.email,
            tenant.map(|t| format!(" (tenant {t})")).unwrap_or_default()
        )),
    });
    if let Some(t) = tenant {
        v["tenantId"] = json!(t);
    }
    c3(StatusCode::OK, v)
}

fn in_progress(a: &Approval) -> Response {
    c3(
        StatusCode::OK,
        json!({
            "status": "already_decided",
            "outcome": "in_progress",
            "summary": summary(format!("A decision for {} is already being carried out.", a.email)),
        }),
    )
}

fn mail_note(steps: &[StepRow]) -> String {
    let status = |s: &str| {
        steps
            .iter()
            .find(|r| r.step == s)
            .map(|r| r.status.as_str())
            .unwrap_or("pending")
    };
    match status("invite_email") {
        "ok" | "failed" => {
            provisioning::email_note(steps).unwrap_or_else(|| "email step done".into())
        }
        _ if status("member_licence") != "ok" || status("licences_tenant") != "ok" => {
            "email follows once the licence is ready".into()
        }
        _ => "email pending (retried automatically)".into(),
    }
}

/// C3 502: `{status: "failed", code, summary}`; nothing was decided.
fn failed(id: Uuid, action: &str, e: &ApiError) -> Response {
    let code = e.code();
    tracing::error!(signup = %id, action, code, error = %e, "signup_telegram_decision_failed");
    let message = match e {
        ApiError::Internal(_) => "internal error".to_string(),
        other => other.to_string(),
    };
    c3(
        StatusCode::BAD_GATEWAY,
        json!({
            "status": "failed",
            "code": code,
            "summary": summary(format!("Could not {action}: {code} - {message}")),
        }),
    )
}

/// The Telegram decision door (C3). Approve uses the defaults: a new workspace with the suggested name.
async fn telegram_decision(
    State(st): State<AppState>,
    caller: ServiceClaims,
    ApiPath(p): ApiPath<IdPath>,
    ApiJson(b): ApiJson<TelegramDecision>,
) -> ApiResult<Response> {
    let sg = signup(&st)?;
    require_decider(&sg.cfg, &caller)?;
    let id = signup_id(&p.id)?;
    let approve = match b.action.as_str() {
        "approve" => true,
        "reject" => false,
        _ => return Err(invalid("action must be approve or reject")),
    };
    if b.telegram_user_id <= 0 {
        return Err(invalid("telegramUserId must be a Telegram user id"));
    }
    let read = async {
        let mut tx = st.db.begin().await?;
        let a = store::get(&mut tx, id).await?;
        tx.commit().await?;
        Ok::<_, ApiError>(a)
    }
    .await;
    let a = match read {
        Ok(Some(a)) => a,
        Ok(None) => return Err(signup_not_found(&p.id)),
        Err(e) => return Ok(failed(id, b.action.as_str(), &e)),
    };
    if a.status != "pending" {
        return Ok(already_decided(&a));
    }
    let by = format!("telegram:{}", b.telegram_user_id);
    let via = format!("Telegram ({})", b.telegram_user_id);
    let out = if approve {
        telegram_approve(&st, &sg, &a, &by, &via).await
    } else {
        telegram_reject(&st, &sg, &a, &by, &via).await
    };
    Ok(out.unwrap_or_else(|e| failed(id, b.action.as_str(), &e)))
}

async fn telegram_approve(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    by: &str,
    via: &str,
) -> ApiResult<Response> {
    let name = tenant_name(&default_tenant_name(
        a.company.as_deref(),
        &a.email,
        Some(a.name.as_str()),
    ))?;
    let input = TenantInput { name: &name };
    Ok(
        match provisioning::approve(st, sg, a.id, &input, by, "telegram").await? {
            Decision::NotFound => return Err(signup_not_found(&a.id.to_string())),
            Decision::AlreadyDecided(now) | Decision::Rejected(now, _) => already_decided(&now),
            Decision::InProgress(now) => in_progress(&now),
            Decision::Approved {
                approval,
                tenant,
                tenant_name,
            } => {
                let steps = after_approval(st, sg, &approval, tenant, &tenant_name, via).await?;
                let mail = mail_note(&steps);
                c3(
                    StatusCode::OK,
                    json!({
                        "status": "approved",
                        "tenantId": tenant,
                        "summary": summary(format!(
                            "Approved: {} -> workspace \"{tenant_name}\"; {mail}.",
                            approval.email
                        )),
                    }),
                )
            }
        },
    )
}

async fn telegram_reject(
    st: &AppState,
    sg: &Signup,
    a: &Approval,
    by: &str,
    via: &str,
) -> ApiResult<Response> {
    Ok(
        match provisioning::reject(st, sg, a.id, by, "telegram", None).await? {
            Decision::NotFound => return Err(signup_not_found(&a.id.to_string())),
            Decision::AlreadyDecided(now) | Decision::Approved { approval: now, .. } => {
                already_decided(&now)
            }
            Decision::InProgress(now) => in_progress(&now),
            Decision::Rejected(now, repaired) => {
                announce_rejection(sg, &now, via, &repaired);
                let note = repair_line(&repaired)
                    .map(|l| format!(" Cleaned up: {l}."))
                    .unwrap_or_default();
                c3(
                    StatusCode::OK,
                    json!({
                        "status": "rejected",
                        "summary": summary(format!("Rejected: {}.{note}", now.email)),
                    }),
                )
            }
        },
    )
}

async fn retry(
    State(st): State<AppState>,
    Provisioner(_): Provisioner,
    ApiPath(p): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let sg = signup(&st)?;
    let id = signup_id(&p.id)?;
    match provisioning::retry(&st, &sg, id).await? {
        provisioning::Retried::NotFound => Err(signup_not_found(&p.id)),
        provisioning::Retried::NotApproved(status) => Err(ApiError::Conflict(
            "signup_not_approved",
            format!("only an approved signup is provisioned; this one is {status}"),
        )),
        provisioning::Retried::Steps { requeued, steps } => Ok(Json(json!({
            "requeued": requeued,
            "steps": steps.iter().map(step_json).collect::<Vec<_>>(),
        }))),
    }
}
