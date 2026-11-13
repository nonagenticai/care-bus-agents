//! Provision-down (plan 10 Task 6): tenants, users and jobs are provisioned by a super_admin; a tenant_admin manages the users of their own tenant. Plus `GET /api/me` (who am I, any verified
//! caller, also unprovisioned) and `GET /api/jobs` (the tenant's jobs). Plan 14 adds two reads for the admin UI:
//! `GET /api/tenants` (super_admin's tenant picker) and `GET /api/tenants/{tenant_id}/users` (user management).
//!
//! The tenant is in the path for every provisioning route because the resource *is* the tenant and a super_admin
//! need not have a home tenant. Each handler writes in a transaction scoped to that tenant (`begin_scoped`),
//! so RLS confines it to exactly that tenant's rows (`tenant_self` on `tenants`). The scope registry (plan 13) is
//! updated after COMMIT, never inside the transaction; a failed registry write is a 500 the caller retries (every
//! route here is an idempotent PUT), and the API rebuilds the registry from the database on every start.
use std::collections::HashSet;

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, put};
use axum::{Json, Router};
use carebus::{Scope, scopes};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit;
use crate::auth::{Caller, CallerKind, TenantRole};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::state::AppState;
use crate::tenancy::{Member, Provisioner, TenantAdmin, active_tenant_from, begin_scoped};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me", get(me))
        .route("/api/jobs", get(jobs))
        .route("/api/tenants", get(list_tenants))
        .route("/api/tenants/{tenant_id}", put(put_tenant))
        .route("/api/tenants/{tenant_id}/users", get(list_users))
        .route(
            "/api/tenants/{tenant_id}/users/{sub}",
            put(put_user).delete(delete_user),
        )
        .route("/api/tenants/{tenant_id}/jobs/{key}", put(put_job))
}

// ---------------------------------------------------------------------------------------------------------------
// Guards and validation
// ---------------------------------------------------------------------------------------------------------------

/// Who may manage a tenant's users: a super_admin (any tenant), or a
/// `TenantAdmin`, only in their own tenant (`tenant_admin_of`, checked against the path by the handler).
pub struct UserManager {
    caller: Caller,
    tenant_admin_of: Option<Uuid>,
}

impl FromRequestParts<AppState> for UserManager {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let c = Caller::from_request_parts(parts, st).await?;
        if c.is_super_admin() {
            return Ok(UserManager {
                caller: c,
                tenant_admin_of: None,
            });
        }
        let TenantAdmin(t) = TenantAdmin::from_request_parts(parts, st).await?;
        Ok(UserManager {
            caller: t.caller,
            tenant_admin_of: Some(t.tenant),
        })
    }
}

impl UserManager {
    fn check_tenant(&self, tenant: Uuid) -> ApiResult<()> {
        match self.tenant_admin_of {
            Some(own) if own != tenant => Err(ApiError::Forbidden("tenant_admin of that tenant")),
            _ => Ok(()),
        }
    }
}

/// A bare (hyphenated), non-nil uuid; care-bus never mints tenant ids, so the path names an existing one.
fn tenant_id(raw: &str) -> ApiResult<Uuid> {
    match Uuid::parse_str(raw) {
        Ok(u) if !u.is_nil() && raw.len() == 36 => Ok(u),
        _ => Err(ApiError::BadRequest(
            "invalid_tenant_id",
            "The tenant id must be a bare, non-nil uuid.".into(),
        )),
    }
}

/// 1..=120 characters after trimming; returns the trimmed text.
fn bounded_text(field: &str, v: &str) -> ApiResult<String> {
    let t = v.trim();
    let n = t.chars().count();
    if (1..=120).contains(&n) {
        Ok(t.to_string())
    } else {
        Err(ApiError::BadRequest(
            "invalid_body",
            format!("{field} must be 1 to 120 characters"),
        ))
    }
}

/// `^[a-z][a-z0-9_]{0,62}$`, the `jobs.key` CHECK and the bus scope's job rule.
fn valid_job_key(k: &str) -> bool {
    let mut cs = k.chars();
    matches!(cs.next(), Some('a'..='z'))
        && k.len() <= 63
        && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The tenant's status read inside a transaction scoped to it; 404 when it does not exist.
async fn tenant_status(conn: &mut PgConnection, tenant: Uuid) -> ApiResult<String> {
    let s: Option<String> = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(conn)
        .await?;
    s.ok_or_else(|| ApiError::NotFound("tenant".into()))
}

/// How many tenant_admins the tenant has besides `sub`. Locks every admin row, so two admins removing each other
/// concurrently cannot both pass.
async fn admins_besides(conn: &mut PgConnection, sub: &str) -> ApiResult<usize> {
    let admins: Vec<String> = sqlx::query_scalar(
        "SELECT sub FROM users WHERE tenant_role = 'tenant_admin' ORDER BY sub FOR UPDATE",
    )
    .fetch_all(conn)
    .await?;
    Ok(admins.iter().filter(|s| s.as_str() != sub).count())
}

fn last_tenant_admin() -> ApiError {
    ApiError::Conflict(
        "last_tenant_admin",
        "This is the tenant's last tenant_admin; only a super_admin or the configurator may remove it.".into(),
    )
}

fn created_or_ok(created: bool) -> StatusCode {
    if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    }
}

// ---------------------------------------------------------------------------------------------------------------
// PUT /api/tenants/{tenant_id}
// ---------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TenantPath {
    tenant_id: String,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TenantStatus {
    Active,
    Suspended,
}

impl TenantStatus {
    fn as_str(self) -> &'static str {
        match self {
            TenantStatus::Active => "active",
            TenantStatus::Suspended => "suspended",
        }
    }
}

#[derive(Deserialize)]
pub struct TenantInput {
    name: String,
    status: TenantStatus,
}

async fn put_tenant(
    Provisioner(caller): Provisioner,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
    ApiJson(body): ApiJson<TenantInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let tenant = tenant_id(&p.tenant_id)?;
    let name = bounded_text("name", &body.name)?;
    let status = body.status.as_str();
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let prev: Option<String> =
        sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1 FOR UPDATE")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await?;
    sqlx::query(
        "INSERT INTO tenants (id, name, status, updated_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (id) DO UPDATE
           SET name = EXCLUDED.name, status = EXCLUDED.status, updated_at = now(), updated_by = EXCLUDED.updated_by",
    )
    .bind(tenant)
    .bind(&name)
    .bind(status)
    .bind(&caller.actor)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &caller.actor,
        caller.actor_type(),
        "tenant.upsert",
        "tenant",
        &tenant.to_string(),
        true,
        json!({"name": name, "status": status, "from_status": prev}),
    )
    .await?;
    let job_keys: Vec<String> = sqlx::query_scalar("SELECT key FROM jobs ORDER BY key")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    // Suspended tenants are not served. Always reconcile (idempotent), so a retry repairs a registry write that
    // failed after an earlier commit.
    for key in job_keys {
        let s = Scope { tenant, job: key };
        match body.status {
            TenantStatus::Active => scopes::add(&st.bus, &s).await?,
            TenantStatus::Suspended => scopes::remove(&st.bus, &s).await?,
        }
    }
    Ok((
        created_or_ok(prev.is_none()),
        Json(json!({"id": tenant, "name": name, "status": status})),
    ))
}

// ---------------------------------------------------------------------------------------------------------------
// PUT / DELETE /api/tenants/{tenant_id}/users/{sub}
// ---------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UserPath {
    tenant_id: String,
    sub: String,
}

#[derive(Deserialize)]
pub struct UserInput {
    tenant_role: TenantRole,
    #[serde(default)]
    display_name: Option<String>,
}

fn valid_sub(sub: &str) -> ApiResult<()> {
    if sub.trim().is_empty() || sub.len() > 255 {
        return Err(ApiError::BadRequest(
            "invalid_sub",
            "The user subject must be 1 to 255 characters.".into(),
        ));
    }
    Ok(())
}

async fn put_user(
    m: UserManager,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<UserPath>,
    ApiJson(body): ApiJson<UserInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let tenant = tenant_id(&p.tenant_id)?;
    m.check_tenant(tenant)?;
    valid_sub(&p.sub)?;
    let display_name = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| bounded_text("display_name", d))
        .transpose()?;
    let role = body.tenant_role;
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    tenant_status(&mut tx, tenant).await?;
    // Visible only when the user is in this tenant (RLS); a row in another tenant surfaces as a unique violation.
    let prev: Option<String> =
        sqlx::query_scalar("SELECT tenant_role FROM users WHERE sub = $1 FOR UPDATE")
            .bind(&p.sub)
            .fetch_optional(&mut *tx)
            .await?;
    // A tenant_admin must not demote the tenant's last tenant_admin either (that is a removal of the role).
    if m.tenant_admin_of.is_some()
        && prev.as_deref() == Some("tenant_admin")
        && role != TenantRole::TenantAdmin
        && admins_besides(&mut tx, &p.sub).await? == 0
    {
        return Err(last_tenant_admin());
    }
    match prev {
        None => {
            sqlx::query(
                "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&p.sub)
            .bind(tenant)
            .bind(role.as_str())
            .bind(&display_name)
            .bind(&m.caller.actor)
            .execute(&mut *tx)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(d) if d.constraint() == Some("users_pkey") => {
                    ApiError::Conflict(
                        "user_in_other_tenant",
                        "This user already belongs to another tenant.".into(),
                    )
                }
                _ => ApiError::from(e),
            })?;
        }
        Some(_) => {
            sqlx::query(
                "UPDATE users SET tenant_role = $2, display_name = $3, updated_at = now(), updated_by = $4
                 WHERE sub = $1",
            )
            .bind(&p.sub)
            .bind(role.as_str())
            .bind(&display_name)
            .bind(&m.caller.actor)
            .execute(&mut *tx)
            .await?;
        }
    }
    audit::record(
        &mut tx,
        &m.caller.actor,
        m.caller.actor_type(),
        "user.upsert",
        "user",
        &p.sub,
        true,
        json!({"tenant_role": role.as_str(), "from_role": prev}),
    )
    .await?;
    tx.commit().await?;
    Ok((
        created_or_ok(prev.is_none()),
        Json(json!({
            "sub": p.sub,
            "tenant_id": tenant,
            "tenant_role": role.as_str(),
            "display_name": display_name,
        })),
    ))
}

/// May `sub` be removed by `m`: its role (404 when it is no member, 409 `last_tenant_admin` for a tenant_admin
/// removing the last admin; a super_admin may remove it, it owns the tenant's membership). Locks the row.
async fn removable(m: &UserManager, conn: &mut PgConnection, sub: &str) -> ApiResult<String> {
    let role: Option<String> =
        sqlx::query_scalar("SELECT tenant_role FROM users WHERE sub = $1 FOR UPDATE")
            .bind(sub)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(role) = role else {
        return Err(ApiError::NotFound("user".into()));
    };
    if m.tenant_admin_of.is_some()
        && role == "tenant_admin"
        && admins_besides(conn, sub).await? == 0
    {
        return Err(last_tenant_admin());
    }
    Ok(role)
}

/// Removing a member also revokes its care-bus member licence in licences (the revoke the reject repair uses),
/// BEFORE the `users` row goes: the `member_licences` row is deleted with it (0019 cascade), and a licence nobody
/// records any more would outlive the membership. A failed revoke is 502 `upstream_rejected` and removes nothing,
/// so the caller retries; the revoke is idempotent (an already revoked licence is ok), so a retry after a lost
/// delete is safe. No HTTP call is made inside a transaction.
async fn delete_user(
    m: UserManager,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<UserPath>,
) -> ApiResult<StatusCode> {
    let tenant = tenant_id(&p.tenant_id)?;
    m.check_tenant(tenant)?;
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    removable(&m, &mut tx, &p.sub).await?;
    let licence: Option<String> =
        sqlx::query_scalar("SELECT license_id FROM member_licences WHERE sub = $1")
            .bind(&p.sub)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    let mut released: Vec<String> = Vec::new();
    if let Some(id) = &licence {
        let lic = st.signup.licences.as_ref().ok_or_else(|| {
            ApiError::Rejected(
                StatusCode::BAD_GATEWAY,
                "the member holds a licence and the licences admin is not configured".into(),
            )
        })?;
        released = lic
            .revoke(tenant, id)
            .await
            .map_err(|e| ApiError::Rejected(StatusCode::BAD_GATEWAY, e.message))?
            // `None`: licences holds no live licence under that id any more (unknown or already revoked).
            .unwrap_or_default();
        tracing::info!(%tenant, licence = %id, "member_licence_revoked");
    }
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let role = removable(&m, &mut tx, &p.sub).await?;
    sqlx::query("DELETE FROM users WHERE sub = $1")
        .bind(&p.sub)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &m.caller.actor,
        m.caller.actor_type(),
        "user.delete",
        "user",
        &p.sub,
        true,
        json!({"tenant_role": role, "revoked_licence": licence, "released_dependency_ids": released}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------------------------
// PUT /api/tenants/{tenant_id}/jobs/{key}
// ---------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct JobPath {
    tenant_id: String,
    key: String,
}

/// The only job settings plan 10 knows; any other key is 400 `invalid_body`.
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preamble: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stance: Option<String>,
}

/// `settings` seeds the base layer of a NEW job only. On an existing job the base is fixed (seeded by migrations)
/// and tenant edits go to `PUT /api/jobs/{job}/settings`, so a
/// re-PUT may repeat the current base settings or omit them, and anything else is 409 `job_settings_read_only`.
#[derive(Deserialize)]
pub struct JobInput {
    label: String,
    #[serde(default)]
    settings: Option<JobSettings>,
}

async fn put_job(
    Provisioner(caller): Provisioner,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<JobPath>,
    ApiJson(body): ApiJson<JobInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let tenant = tenant_id(&p.tenant_id)?;
    if !valid_job_key(&p.key) {
        return Err(ApiError::BadRequest(
            "invalid_job_key",
            "The job key must match ^[a-z][a-z0-9_]{0,62}$.".into(),
        ));
    }
    let label = bounded_text("label", &body.label)?;
    let settings = body
        .settings
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(anyhow::Error::from)?;
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let status = tenant_status(&mut tx, tenant).await?;
    let current: Option<Value> =
        sqlx::query_scalar("SELECT settings FROM jobs WHERE key = $1 FOR UPDATE")
            .bind(&p.key)
            .fetch_optional(&mut *tx)
            .await?;
    let existed = current.is_some();
    if let (Some(cur), Some(new)) = (&current, &settings)
        && cur != new
    {
        return Err(ApiError::Conflict(
            "job_settings_read_only",
            format!(
                "The settings of an existing job are its base; change them with \
                 PUT /api/jobs/{}/settings (tenant layer).",
                p.key
            ),
        ));
    }
    // ON CONFLICT never touches `settings`: it is the base layer, fixed once the job exists.
    let (label, settings): (String, Value) = sqlx::query_as(
        "INSERT INTO jobs (tenant_id, key, label, settings) VALUES ($1, $2, $3, $4)
         ON CONFLICT (tenant_id, key) DO UPDATE
           SET label = EXCLUDED.label, updated_at = now()
         RETURNING label, settings",
    )
    .bind(tenant)
    .bind(&p.key)
    .bind(&label)
    .bind(settings.unwrap_or_else(|| json!({})))
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &caller.actor,
        caller.actor_type(),
        "job.upsert",
        "job",
        &p.key,
        true,
        json!({"label": label, "settings": settings}),
    )
    .await?;
    tx.commit().await?;
    if status == "active" {
        scopes::add(
            &st.bus,
            &Scope {
                tenant,
                job: p.key.clone(),
            },
        )
        .await?;
    }
    Ok((
        created_or_ok(!existed),
        Json(json!({"key": p.key, "label": label, "settings": settings})),
    ))
}

// ---------------------------------------------------------------------------------------------------------------
// GET /api/tenants, GET /api/tenants/{tenant_id}/users (plan 14)
// ---------------------------------------------------------------------------------------------------------------

/// Every tenant (id, name, status) for the super_admin tenant picker. No active tenant is needed: the list is read
/// through the definer function `carebus_list_tenants` (migration 0013), the one read across tenants.
async fn list_tenants(caller: Caller, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    if !caller.is_super_admin() {
        return Err(ApiError::Forbidden("super_admin"));
    }
    let rows: Vec<(Uuid, String, String)> =
        sqlx::query_as("SELECT id, name, status FROM carebus_list_tenants()")
            .fetch_all(&st.db)
            .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, status)| json!({"id": id, "name": name, "status": status}))
        .collect();
    Ok(Json(json!({"items": items, "next": Value::Null})))
}

/// Who may read a tenant's users: a super_admin (any tenant), or a member of that tenant
/// (tenant_user included: read-only admin, plan 10 Q2 default).
pub struct UserReader {
    member_of: Option<Uuid>,
}

impl FromRequestParts<AppState> for UserReader {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let c = Caller::from_request_parts(parts, st).await?;
        if c.is_super_admin() {
            return Ok(UserReader { member_of: None });
        }
        let Member(t) = Member::from_request_parts(parts, st).await?;
        Ok(UserReader {
            member_of: Some(t.tenant),
        })
    }
}

/// One `users` row as listed: sub, tenant_role, display_name, updated_at, updated_by.
type UserRow = (String, String, Option<String>, DateTime<Utc>, String);

/// The users of `{tenant_id}`, ordered by sub; 404 when the tenant does not exist.
async fn list_users(
    r: UserReader,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    if r.member_of.is_some_and(|own| own != tenant) {
        return Err(ApiError::Forbidden("member of that tenant"));
    }
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    tenant_status(&mut tx, tenant).await?;
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT sub, tenant_role, display_name, updated_at, updated_by FROM users ORDER BY sub",
    )
    .fetch_all(&mut *tx)
    .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(sub, role, display_name, updated_at, updated_by)| {
            json!({"sub": sub, "tenant_role": role, "display_name": display_name,
                   "updated_at": updated_at, "updated_by": updated_by})
        })
        .collect();
    Ok(Json(json!({"items": items, "next": Value::Null})))
}

// ---------------------------------------------------------------------------------------------------------------
// GET /api/me, GET /api/jobs
// ---------------------------------------------------------------------------------------------------------------

/// (name, status) of a tenant, read under its own tenant GUC; `None` when it does not exist.
async fn tenant_row(st: &AppState, tenant: Uuid) -> ApiResult<Option<(String, String)>> {
    let mut tx = begin_scoped(&st.db, tenant, None).await?;
    let row = sqlx::query_as("SELECT name, status FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    Ok(row)
}

/// Any verified caller, provisioned or not (the one route an unprovisioned person may call). `active_tenant` is
/// the tenant a request with these headers would act in: a non-super person's home tenant; for a super_admin or a
/// service token the `X-Active-Tenant-Id` tenant when it exists, else (super_admin only) the
/// home tenant. Reading it never records a tenant switch.
async fn me(
    caller: Caller,
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let header = active_tenant_from(&headers)?;
    let home = caller.home().cloned();
    let tenant = match &home {
        None => Value::Null,
        Some(h) => match tenant_row(&st, h.tenant).await? {
            None => Value::Null,
            Some((name, status)) => json!({
                "id": h.tenant,
                "name": name,
                "role": h.role.as_str(),
                "status": status,
            }),
        },
    };
    let mut named = None;
    if let Some(t) = header
        && tenant_row(&st, t).await?.is_some()
    {
        named = Some(t);
    }
    let active_tenant = match &caller.kind {
        CallerKind::Person {
            super_admin: false, ..
        } => home.map(|h| h.tenant),
        CallerKind::Person {
            super_admin: true, ..
        } => named.or(home.map(|h| h.tenant)),
        CallerKind::Service => named,
    };
    Ok(Json(json!({
        "sub": caller.sub,
        "actor": caller.actor,
        "super_admin": caller.is_super_admin(),
        "tenant": tenant,
        "active_tenant": active_tenant,
    })))
}

/// The active tenant's jobs; `on_bus` = the job is in the scope registry (agents serve it).
async fn jobs(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let rows: Vec<(String, String, Value)> =
        sqlx::query_as("SELECT key, label, settings FROM jobs ORDER BY key")
            .fetch_all(&mut *tx)
            .await?;
    drop(tx);
    let on_bus: HashSet<String> = scopes::list(&st.bus)
        .await?
        .into_iter()
        .filter(|s| s.tenant == t.tenant)
        .map(|s| s.job)
        .collect();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(key, label, settings)| {
            json!({"on_bus": on_bus.contains(&key), "key": key, "label": label, "settings": settings})
        })
        .collect();
    Ok(Json(json!({"items": items, "next": Value::Null})))
}
