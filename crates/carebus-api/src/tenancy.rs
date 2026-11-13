//! Tenant and job for a request, and the guards built on them.
//!
//! A `Caller` (crate::auth) becomes a `TenantCtx`: a person's home tenant from `users`, or the tenant named by
//! `X-Active-Tenant-Id` for a super_admin or a service token. A `JobCtx` adds the job named by
//! the `{job}` path segment. Handlers reach tenant rows only through `begin`, which opens a transaction and sets the
//! tenant/job GUCs transaction-locally (`set_config(…, true)`, the bind-parameter form of `SET LOCAL`): RLS
//! (migration 0007) then confines every statement to that scope, and nothing survives COMMIT on the pooled
//! connection. Never hold a transaction across an upstream HTTP call: open one before and one after.
use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::request::Parts;
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::audit;
use crate::auth::{Caller, CallerKind, TenantRole};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub const ACTIVE_TENANT_HEADER: &str = "x-active-tenant-id";

/// The tenant a request acts in. `role` is the effective tenant role: `None` for service tokens, which are never
/// tenant members; `Some(TenantAdmin)` for a super_admin.
#[derive(Clone, Debug)]
pub struct TenantCtx {
    pub tenant: Uuid,
    pub role: Option<TenantRole>,
    pub caller: Caller,
}

/// A tenant plus one of its jobs (`/api/jobs/{job}/…`).
#[derive(Clone, Debug)]
pub struct JobCtx {
    pub t: TenantCtx,
    pub job_id: Uuid,
    pub job_key: String,
}

/// A transaction scoped to `tenant` and, when given, `job`, with NO subject: the system scope of non-request code
/// (provisioning a tenant by path, the recorder, metering, the `researcher` worker). Without a subject the per-user
/// legal-matter wall (migration 0026) does not apply, as an unset job GUC means every job. Request handlers never
/// call this: they go through `TenantCtx::begin` / `JobCtx::begin`, which always name the caller.
pub async fn begin_scoped(
    db: &PgPool,
    tenant: Uuid,
    job: Option<Uuid>,
) -> ApiResult<Transaction<'static, Postgres>> {
    begin_as(db, tenant, job, None).await
}

/// The caller of a request, as the `app.current_sub` / `app.current_sub_admin` GUCs (migration 0026).
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    pub sub: &'a str,
    /// tenant_admin, or a super_admin in the tenant.
    pub admin: bool,
}

/// `begin_scoped` plus the subject GUCs: every GUC is set transaction-locally (empty = unset).
pub async fn begin_as(
    db: &PgPool,
    tenant: Uuid,
    job: Option<Uuid>,
    subject: Option<Subject<'_>>,
) -> ApiResult<Transaction<'static, Postgres>> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "SELECT set_config('app.current_tenant', $1, true), set_config('app.current_job', $2, true),
                set_config('app.current_sub', $3, true), set_config('app.current_sub_admin', $4, true)",
    )
    .bind(tenant.to_string())
    .bind(job.map(|j| j.to_string()).unwrap_or_default())
    .bind(subject.map(|s| s.sub).unwrap_or_default())
    .bind(if subject.is_some_and(|s| s.admin) { "1" } else { "" })
    .execute(&mut *tx)
    .await?;
    Ok(tx)
}

impl TenantCtx {
    /// Tenant GUC set, job GUC empty: every job of the tenant is visible. The caller is the subject (0026).
    pub async fn begin(&self, db: &PgPool) -> ApiResult<Transaction<'static, Postgres>> {
        begin_as(db, self.tenant, None, Some(self.subject())).await
    }
    /// The caller as the RLS subject: its token subject, admin for tenant_admin / super_admin.
    pub fn subject(&self) -> Subject<'_> {
        Subject {
            sub: &self.caller.sub,
            admin: self.is_admin(),
        }
    }
    pub fn is_admin(&self) -> bool {
        self.role == Some(TenantRole::TenantAdmin)
    }
    pub fn is_member(&self) -> bool {
        self.role.is_some()
    }
}

impl JobCtx {
    /// Both GUCs set: only this job's rows are visible, and rows written without naming a scope land in it.
    pub async fn begin(&self, db: &PgPool) -> ApiResult<Transaction<'static, Postgres>> {
        begin_as(db, self.t.tenant, Some(self.job_id), Some(self.t.subject())).await
    }
    /// (tenant, job key): the bus scope of this job, for `st.bus.scoped(..)` (plan 13).
    pub fn scope(&self) -> carebus::Scope {
        carebus::Scope {
            tenant: self.t.tenant,
            job: self.job_key.clone(),
        }
    }
}

/// `X-Active-Tenant-Id`: absent, or a bare (hyphenated), non-nil uuid.
fn header_tenant(parts: &Parts) -> ApiResult<Option<Uuid>> {
    active_tenant_from(&parts.headers)
}

/// `X-Active-Tenant-Id` from a header map (`/api/me` reports it without resolving a tenant): absent, or a bare,
/// non-nil uuid; anything else is 400 `invalid_active_tenant`.
pub fn active_tenant_from(headers: &axum::http::HeaderMap) -> ApiResult<Option<Uuid>> {
    let Some(v) = headers.get(ACTIVE_TENANT_HEADER) else {
        return Ok(None);
    };
    let bad = || {
        ApiError::BadRequest(
            "invalid_active_tenant",
            "X-Active-Tenant-Id must be a bare, non-nil tenant uuid".into(),
        )
    };
    let raw = v.to_str().map_err(|_| bad())?;
    match Uuid::parse_str(raw) {
        Ok(u) if !u.is_nil() && raw.len() == 36 => Ok(Some(u)),
        _ => Err(bad()),
    }
}

fn active_tenant_required() -> ApiError {
    ApiError::BadRequest(
        "active_tenant_required",
        "Name the tenant to act in with the X-Active-Tenant-Id header.".into(),
    )
}

/// The tenant's status, read under its own tenant GUC (`tenants` is under RLS too); 404 when no such tenant.
async fn tenant_status(db: &PgPool, tenant: Uuid) -> ApiResult<String> {
    let mut tx = begin_scoped(db, tenant, None).await?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    status.ok_or_else(|| ApiError::NotFound("tenant".into()))
}

/// Records the super_admin's active tenant; a change is audited once, in the target tenant (Q3 default).
async fn switch_active_tenant(db: &PgPool, caller: &Caller, tenant: Uuid) -> ApiResult<()> {
    let mut tx = begin_scoped(db, tenant, None).await?;
    let prev: Option<Uuid> = sqlx::query_scalar("SELECT carebus_switch_active_tenant($1, $2)")
        .bind(&caller.sub)
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
    if prev != Some(tenant) {
        audit::record(
            &mut tx,
            &caller.actor,
            caller.actor_type(),
            "tenant_switch",
            "tenant",
            &tenant.to_string(),
            true,
            json!({ "from": prev }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn resolve(parts: &Parts, st: &AppState, caller: Caller) -> ApiResult<TenantCtx> {
    let header = header_tenant(parts)?;
    match caller.kind.clone() {
        CallerKind::Person {
            super_admin: true,
            home,
        } => {
            let tenant = match (header, home) {
                (Some(t), _) => {
                    tenant_status(&st.db, t).await?;
                    switch_active_tenant(&st.db, &caller, t).await?;
                    t
                }
                (None, Some(h)) => h.tenant,
                (None, None) => return Err(active_tenant_required()),
            };
            Ok(TenantCtx {
                tenant,
                role: Some(TenantRole::TenantAdmin),
                caller,
            })
        }
        CallerKind::Person {
            super_admin: false,
            home,
        } => {
            let h = home.ok_or(ApiError::NotProvisioned)?;
            if header.is_some_and(|t| t != h.tenant) {
                return Err(ApiError::Forbidden("tenant member of that tenant"));
            }
            if h.suspended {
                return Err(ApiError::TenantSuspended);
            }
            Ok(TenantCtx {
                tenant: h.tenant,
                role: Some(h.role),
                caller,
            })
        }
        CallerKind::Service => {
            let tenant = header.ok_or_else(active_tenant_required)?;
            let status = tenant_status(&st.db, tenant).await?;
            if status != "active" {
                return Err(ApiError::TenantSuspended);
            }
            Ok(TenantCtx {
                tenant,
                role: None,
                caller,
            })
        }
    }
}

/// Resolved once per request (cached in the request extensions), so the switch is audited once even when a
/// handler takes several guards.
impl FromRequestParts<AppState> for TenantCtx {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        if let Some(t) = parts.extensions.get::<TenantCtx>() {
            return Ok(t.clone());
        }
        let caller = Caller::from_request_parts(parts, st).await?;
        let t = resolve(parts, st, caller).await?;
        // Licence gate 2 (plan 15), once per request.
        st.licence.check(t.tenant).await?;
        parts.extensions.insert(t.clone());
        Ok(t)
    }
}

/// The `{job}` path segment, looked up inside the tenant (RLS); 404 when the tenant has no such job.
async fn job_ctx(parts: &mut Parts, st: &AppState, t: TenantCtx) -> ApiResult<JobCtx> {
    if let Some(j) = parts.extensions.get::<JobCtx>() {
        return Ok(j.clone());
    }
    let params = RawPathParams::from_request_parts(parts, st)
        .await
        .map_err(|_| ApiError::BadRequest("invalid_path", "The path is not valid.".into()))?;
    let key = params
        .iter()
        .find(|(k, _)| *k == "job")
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!("job guard on a route without {{job}}"))
        })?;
    let mut tx = t.begin(&st.db).await?;
    let job_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await?;
    drop(tx);
    let job_id = job_id.ok_or_else(|| ApiError::NotFound("job".into()))?;
    let j = JobCtx {
        t,
        job_id,
        job_key: key,
    };
    parts.extensions.insert(j.clone());
    Ok(j)
}

impl FromRequestParts<AppState> for JobCtx {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        job_ctx(parts, st, t).await
    }
}

fn need_member(t: &TenantCtx) -> ApiResult<()> {
    if t.is_member() {
        Ok(())
    } else {
        Err(ApiError::Forbidden("tenant member"))
    }
}

fn need_admin(t: &TenantCtx) -> ApiResult<()> {
    if t.is_admin() {
        Ok(())
    } else {
        Err(ApiError::Forbidden("tenant_admin"))
    }
}

/// tenant_user, tenant_admin, or a super_admin in the tenant (tenant_user reads what an admin reads: Q2 default,
/// owner may revise).
pub struct Member(pub TenantCtx);
/// = `Member`: a service token never reaches `Member` anyway. Kept for the conversation routes' intent (R24).
pub struct HumanMember(pub TenantCtx);
/// tenant_admin, or a super_admin in the tenant.
pub struct TenantAdmin(pub TenantCtx);
/// `Member` in the `{job}` of the path.
pub struct JobMember(pub JobCtx);
/// `TenantAdmin` in the `{job}` of the path.
pub struct JobAdmin(pub JobCtx);
/// `JobMember`, or a service token (which must name its tenant with the header): procedure and context-space reads only.
pub struct ProcedureReader(pub JobCtx);
/// super_admin only; no tenant is resolved (the tenant is the resource, named in the path).
pub struct Provisioner(pub Caller);
/// A super_admin acting in a tenant (demo-backends writes: one shared fake enterprise for every tenant).
pub struct SuperAdminIn(pub TenantCtx);

impl FromRequestParts<AppState> for Member {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        need_member(&t)?;
        Ok(Member(t))
    }
}

impl FromRequestParts<AppState> for HumanMember {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let Member(t) = Member::from_request_parts(parts, st).await?;
        if !matches!(t.caller.kind, CallerKind::Person { .. }) {
            return Err(ApiError::Forbidden("person in the tenant"));
        }
        Ok(HumanMember(t))
    }
}

impl FromRequestParts<AppState> for TenantAdmin {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        need_admin(&t)?;
        Ok(TenantAdmin(t))
    }
}

impl FromRequestParts<AppState> for JobMember {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        need_member(&t)?;
        Ok(JobMember(job_ctx(parts, st, t).await?))
    }
}

impl FromRequestParts<AppState> for JobAdmin {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        need_admin(&t)?;
        Ok(JobAdmin(job_ctx(parts, st, t).await?))
    }
}

impl FromRequestParts<AppState> for ProcedureReader {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        if !matches!(t.caller.kind, CallerKind::Service) {
            need_member(&t)?;
        }
        Ok(ProcedureReader(job_ctx(parts, st, t).await?))
    }
}

impl FromRequestParts<AppState> for Provisioner {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let c = Caller::from_request_parts(parts, st).await?;
        if c.is_super_admin() {
            Ok(Provisioner(c))
        } else {
            Err(ApiError::Forbidden("super_admin"))
        }
    }
}

impl FromRequestParts<AppState> for SuperAdminIn {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let c = Caller::from_request_parts(parts, st).await?;
        if !c.is_super_admin() {
            return Err(ApiError::Forbidden("super_admin"));
        }
        Ok(SuperAdminIn(
            TenantCtx::from_request_parts(parts, st).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(header: Option<&str>) -> Parts {
        let mut b = axum::http::Request::builder().uri("/");
        if let Some(h) = header {
            b = b.header(ACTIVE_TENANT_HEADER, h);
        }
        b.body(()).unwrap().into_parts().0
    }

    #[test]
    fn active_tenant_header_is_a_bare_non_nil_uuid() {
        assert_eq!(header_tenant(&parts(None)).unwrap(), None);
        let t = "0195f48b-d000-7000-8000-000000000099";
        assert_eq!(
            header_tenant(&parts(Some(t))).unwrap(),
            Some(t.parse().unwrap())
        );
        for bad in [
            "nope",
            "00000000-0000-0000-0000-000000000000",
            "0195f48bd00070008000000000000099",
            "{0195f48b-d000-7000-8000-000000000099}",
            "urn:uuid:0195f48b-d000-7000-8000-000000000099",
        ] {
            match header_tenant(&parts(Some(bad))) {
                Err(ApiError::BadRequest(code, _)) => assert_eq!(code, "invalid_active_tenant"),
                other => panic!("{bad}: {:?}", other.map(|_| ())),
            }
        }
    }
}
