//! Legal matters of one job (plan 18 compliance, migration 0026, `/api/jobs/{job}/legal-matters`). A matter is the
//! ethical wall around legal research: its runs, their steps, claims and citations are visible to its members only.
//!
//! Who may do what (RLS enforces the same, so a forgotten check here still cannot leak a row):
//! - any tenant member creates a matter and becomes its `lead`;
//! - members see the matter, its members and stores; a tenant_admin sees EVERY matter of the job (list, members,
//!   stores) and manages membership, but does not read the matter's runs unless also a member;
//! - lead or tenant_admin: rename / close / reopen / delete (delete only while no run is filed under it and no store
//!   is attached) and add, change or remove members (the last lead cannot be removed or demoted; an admin who is not
//!   a lead cannot add or promote themselves);
//! - lead only: attach and detach stores (the job's knowledge stores). A store belongs to at most one matter of the
//!   tenant, and a store above or below another matter's store cannot be attached (409 `store_in_other_matter`):
//!   otherwise anyone could open a matter of their own and search another matter's documents through it.
//!
//! A matter the caller may not see is 404. Every write is audited without the matter's name (client confidential).
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, put},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    audit,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    page::Paged,
    routes::research::{canonical_store, check_stores},
    state::AppState,
    tenancy::{JobCtx, JobMember},
    walls,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/legal-matters", get(list).post(create))
        .route("/legal-matters/{id}", get(one).patch(update).delete(remove))
        .route(
            "/legal-matters/{id}/members/{sub}",
            put(set_member).delete(remove_member),
        )
        .route(
            "/legal-matters/{id}/stores/{store}",
            put(attach_store).delete(detach_store),
        )
}

const NAME_MAX: usize = 200;
const REFERENCE_MAX: usize = 100;
const ROLES: [&str; 2] = ["lead", "member"];
const LIST_MAX: i64 = 500;

/// `{id}` under the nested `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
struct IdPath {
    id: Uuid,
}

#[derive(Deserialize)]
struct MemberPath {
    id: Uuid,
    sub: String,
}

#[derive(Deserialize)]
struct StorePath {
    id: Uuid,
    store: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatterInput {
    name: String,
    #[serde(default)]
    reference: Option<String>,
}

/// Every key optional; `reference: ""` clears it; `closed` closes (true) or reopens (false).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatterPatch {
    name: Option<String>,
    reference: Option<String>,
    closed: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberInput {
    role: String,
}

#[derive(Serialize, sqlx::FromRow)]
struct MatterView {
    id: Uuid,
    name: String,
    reference: Option<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
    /// The caller's role in the matter; null for a tenant_admin who is not a member.
    my_role: Option<String>,
    member_count: i64,
    store_count: i64,
}

const MATTER_SELECT: &str = "SELECT m.id, m.name, m.reference, m.created_by, m.created_at, m.closed_at,
        (SELECT mm.role FROM legal_matter_members mm WHERE mm.matter_id = m.id AND mm.user_sub = $1) AS my_role,
        (SELECT count(*) FROM legal_matter_members mm WHERE mm.matter_id = m.id) AS member_count,
        (SELECT count(*) FROM legal_matter_stores st WHERE st.matter_id = m.id) AS store_count
   FROM legal_matters m";

#[derive(Serialize, sqlx::FromRow)]
struct MemberView {
    user_sub: String,
    display_name: Option<String>,
    role: String,
    added_by: String,
    added_at: DateTime<Utc>,
}

#[derive(Serialize, sqlx::FromRow)]
struct StoreView {
    store_id: String,
    added_by: String,
    added_at: DateTime<Utc>,
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.into())
}

fn not_found(id: Uuid) -> ApiError {
    ApiError::NotFound(format!("legal matter {id}"))
}

/// The name trimmed, 1..=200 characters.
fn clean_name(n: &str) -> ApiResult<String> {
    let n = n.trim();
    let c = n.chars().count();
    if c == 0 || c > NAME_MAX {
        return Err(bad(format!("name must be 1 to {NAME_MAX} characters.")));
    }
    Ok(n.to_string())
}

/// The reference trimmed; blank = none; at most 100 characters.
fn clean_reference(r: Option<&str>) -> ApiResult<Option<String>> {
    let Some(r) = r.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    if r.chars().count() > REFERENCE_MAX {
        return Err(bad(format!(
            "reference must be at most {REFERENCE_MAX} characters."
        )));
    }
    Ok(Some(r.to_string()))
}

/// A unique violation on the reference index is 409 `reference_taken`; anything else as usual.
fn reference_conflict(e: sqlx::Error) -> ApiError {
    match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => ApiError::Conflict(
            "reference_taken",
            "Another matter of this job has that reference.".into(),
        ),
        _ => e.into(),
    }
}

/// The matter as the caller sees it (RLS: member, or tenant_admin); 404 otherwise.
async fn load(conn: &mut PgConnection, sub: &str, id: Uuid) -> ApiResult<MatterView> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{MATTER_SELECT} WHERE m.id = $2"
    )))
    .bind(sub)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| not_found(id))
}

/// Lead or tenant_admin: may change the matter and its members.
fn need_manager(ctx: &JobCtx, m: &MatterView) -> ApiResult<()> {
    if ctx.t.is_admin() || m.my_role.as_deref() == Some("lead") {
        Ok(())
    } else {
        Err(ApiError::Forbidden("matter lead or tenant_admin"))
    }
}

/// Lead only: may attach and detach stores.
fn need_lead(m: &MatterView) -> ApiResult<()> {
    if m.my_role.as_deref() == Some("lead") {
        Ok(())
    } else {
        Err(ApiError::Forbidden("matter lead"))
    }
}

async fn record(
    conn: &mut PgConnection,
    ctx: &JobCtx,
    action: &str,
    id: Uuid,
    details: Value,
) -> ApiResult<()> {
    audit::record(
        conn,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        action,
        "legal_matter",
        &id.to_string(),
        true,
        details,
    )
    .await
}

#[derive(Deserialize)]
struct ListQuery {
    /// Only the matters the caller is a member of (a tenant_admin otherwise sees every matter of the job).
    mine: Option<bool>,
    /// Only open matters.
    open: Option<bool>,
}

/// The matters the caller may see, newest first (at most 500; `next` is always null).
async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Paged<MatterView>>> {
    let mut tx = ctx.begin(&s.db).await?;
    let items: Vec<MatterView> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{MATTER_SELECT}
          WHERE (NOT $2 OR EXISTS (SELECT 1 FROM legal_matter_members mm
                                    WHERE mm.matter_id = m.id AND mm.user_sub = $1))
            AND (NOT $3 OR m.closed_at IS NULL)
          ORDER BY m.created_at DESC, m.id DESC LIMIT $4"
    )))
    .bind(&ctx.t.caller.sub)
    .bind(q.mine.unwrap_or(false))
    .bind(q.open.unwrap_or(false))
    .bind(LIST_MAX)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Paged { items, next: None }))
}

/// Any tenant member with a `users` row (a super_admin without one is 422 `not_a_tenant_user`: a lead must be a
/// user of the tenant). The creator becomes the lead.
async fn create(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiJson(i): ApiJson<MatterInput>,
) -> ApiResult<(StatusCode, Json<MatterView>)> {
    let name = clean_name(&i.name)?;
    let reference = clean_reference(i.reference.as_deref())?;
    let id = Uuid::new_v4();
    let mut tx = ctx.begin(&s.db).await?;
    let is_user: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE sub = $1)")
        .bind(&ctx.t.caller.sub)
        .fetch_one(&mut *tx)
        .await?;
    if !is_user {
        return Err(ApiError::Unprocessable(
            "not_a_tenant_user",
            "Only a user of this tenant can lead a matter.".into(),
        ));
    }
    sqlx::query("SELECT carebus_legal_matter_create($1, $2, $3)")
        .bind(id)
        .bind(&name)
        .bind(&reference)
        .execute(&mut *tx)
        .await
        .map_err(reference_conflict)?;
    let m = load(&mut tx, &ctx.t.caller.sub, id).await?;
    record(&mut tx, &ctx, "create", id, json!({})).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(m)))
}

/// The matter with its members (and their display names) and stores.
async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&s.db).await?;
    let m = load(&mut tx, &ctx.t.caller.sub, id).await?;
    let members: Vec<MemberView> = sqlx::query_as(
        "SELECT mm.user_sub, u.display_name, mm.role, mm.added_by, mm.added_at
           FROM legal_matter_members mm LEFT JOIN users u ON u.sub = mm.user_sub
          WHERE mm.matter_id = $1
          ORDER BY mm.role = 'lead' DESC, mm.added_at, mm.user_sub",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let stores: Vec<StoreView> = sqlx::query_as(
        "SELECT store_id, added_by, added_at FROM legal_matter_stores WHERE matter_id = $1
          ORDER BY added_at, store_id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(
        json!({"matter": m, "members": members, "stores": stores}),
    ))
}

/// Lead or tenant_admin: rename, change the reference, close or reopen.
async fn update(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiJson(p): ApiJson<MatterPatch>,
) -> ApiResult<Json<MatterView>> {
    let name = p.name.as_deref().map(clean_name).transpose()?;
    let reference = p
        .reference
        .as_deref()
        .map(|r| clean_reference(Some(r)))
        .transpose()?;
    let mut tx = ctx.begin(&s.db).await?;
    let before = load(&mut tx, &ctx.t.caller.sub, id).await?;
    need_manager(&ctx, &before)?;
    sqlx::query(
        "UPDATE legal_matters
            SET name = coalesce($2, name),
                reference = CASE WHEN $3 THEN $4 ELSE reference END,
                closed_at = CASE WHEN $5::boolean IS NULL THEN closed_at
                                 WHEN $5 THEN coalesce(closed_at, now())
                                 ELSE NULL END
          WHERE id = $1",
    )
    .bind(id)
    .bind(&name)
    .bind(reference.is_some())
    .bind(reference.flatten())
    .bind(p.closed)
    .execute(&mut *tx)
    .await
    .map_err(reference_conflict)?;
    let m = load(&mut tx, &ctx.t.caller.sub, id).await?;
    record(
        &mut tx,
        &ctx,
        "update",
        id,
        json!({"renamed": name.is_some(), "closed": p.closed}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(m))
}

fn has_runs() -> ApiError {
    ApiError::Conflict(
        "matter_has_runs",
        "Research runs are filed under this matter; close it instead.".into(),
    )
}

fn has_stores() -> ApiError {
    ApiError::Conflict(
        "matter_has_stores",
        "Stores are attached to this matter; detach them first (deleting it would unwall them)."
            .into(),
    )
}

/// Lead or tenant_admin; only while no run is filed under the matter (else 409 `matter_has_runs`: close it) and no
/// store is attached (else 409 `matter_has_stores`: a delete must never silently unwall a store).
async fn remove(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<StatusCode> {
    let mut tx = ctx.begin(&s.db).await?;
    let m = load(&mut tx, &ctx.t.caller.sub, id).await?;
    need_manager(&ctx, &m)?;
    // The runs a member sees; for anyone else the RESTRICT below still answers.
    let runs: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM research_runs WHERE matter_id = $1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if runs {
        return Err(has_runs());
    }
    if m.store_count > 0 {
        return Err(has_stores());
    }
    sqlx::query("DELETE FROM legal_matters WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            // ON DELETE RESTRICT raises 23001 restrict_violation, not 23503: match the constraint itself.
            let constraint = match &e {
                sqlx::Error::Database(d) => d.constraint().map(str::to_string),
                _ => None,
            };
            match constraint.as_deref() {
                Some("research_runs_matter_fkey") => has_runs(),
                Some("legal_matter_stores_matter_fkey") => has_stores(),
                _ => e.into(),
            }
        })?;
    record(&mut tx, &ctx, "delete", id, json!({})).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The number of leads of a matter other than `sub`.
async fn other_leads(conn: &mut PgConnection, id: Uuid, sub: &str) -> ApiResult<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM legal_matter_members WHERE matter_id = $1 AND role = 'lead' AND user_sub <> $2",
    )
    .bind(id)
    .bind(sub)
    .fetch_one(&mut *conn)
    .await?;
    Ok(n)
}

/// The role `sub` holds in the matter, if any.
async fn role_of(conn: &mut PgConnection, id: Uuid, sub: &str) -> ApiResult<Option<String>> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM legal_matter_members WHERE matter_id = $1 AND user_sub = $2",
    )
    .bind(id)
    .bind(sub)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(role)
}

fn last_lead() -> ApiError {
    ApiError::Conflict(
        "last_lead",
        "A matter needs at least one lead; make someone else lead first.".into(),
    )
}

/// Lead or tenant_admin: adds `sub` (a user of this tenant, else 422 `unknown_user`) or changes their role.
async fn set_member(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(MemberPath { id, sub }): ApiPath<MemberPath>,
    ApiJson(i): ApiJson<MemberInput>,
) -> ApiResult<Json<Value>> {
    if !ROLES.contains(&i.role.as_str()) {
        return Err(bad("role must be lead or member."));
    }
    let mut tx = ctx.begin(&s.db).await?;
    let m = load(&mut tx, &ctx.t.caller.sub, id).await?;
    need_manager(&ctx, &m)?;
    // An admin who is not a lead may not grant themselves access to a client file (RLS `member_insert` agrees).
    if sub == ctx.t.caller.sub && m.my_role.as_deref() != Some("lead") {
        return Err(ApiError::Forbidden("matter lead (to add yourself)"));
    }
    // `users` is under tenant RLS: a subject of another tenant is not found.
    let is_user: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE sub = $1)")
        .bind(&sub)
        .fetch_one(&mut *tx)
        .await?;
    if !is_user {
        return Err(ApiError::Unprocessable(
            "unknown_user",
            format!("{sub} is not a user of this tenant."),
        ));
    }
    let from = role_of(&mut tx, id, &sub).await?;
    if from.as_deref() == Some("lead")
        && i.role != "lead"
        && other_leads(&mut tx, id, &sub).await? == 0
    {
        return Err(last_lead());
    }
    sqlx::query(
        "INSERT INTO legal_matter_members (matter_id, user_sub, role, added_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (matter_id, user_sub) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(id)
    .bind(&sub)
    .bind(&i.role)
    .bind(&ctx.t.caller.sub)
    .execute(&mut *tx)
    .await?;
    record(
        &mut tx,
        &ctx,
        "member_set",
        id,
        json!({"user_sub": sub, "from": from, "to": i.role}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"user_sub": sub, "role": i.role})))
}

/// Lead or tenant_admin: removes `sub` (404 when not a member; 409 `last_lead` for the only lead).
async fn remove_member(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(MemberPath { id, sub }): ApiPath<MemberPath>,
) -> ApiResult<StatusCode> {
    let mut tx = ctx.begin(&s.db).await?;
    let m = load(&mut tx, &ctx.t.caller.sub, id).await?;
    need_manager(&ctx, &m)?;
    let from = role_of(&mut tx, id, &sub)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("member {sub}")))?;
    if from == "lead" && other_leads(&mut tx, id, &sub).await? == 0 {
        return Err(last_lead());
    }
    sqlx::query("DELETE FROM legal_matter_members WHERE matter_id = $1 AND user_sub = $2")
        .bind(id)
        .bind(&sub)
        .execute(&mut *tx)
        .await?;
    record(
        &mut tx,
        &ctx,
        "member_remove",
        id,
        json!({"user_sub": sub, "from": from}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn in_other_matter(store: &str) -> ApiError {
    ApiError::Conflict(
        "store_in_other_matter",
        format!("store {store} (or a store above or below it) belongs to another matter."),
    )
}

/// Lead only: attaches a knowledge store of this job (422 `unknown_store` otherwise) to an open matter
/// (409 `matter_closed`), walling it. A store already attached to another matter of the tenant, or above or below
/// one in the strata tree, is 409 `store_in_other_matter` (the table's `(tenant_id, store_id)` key agrees for the
/// store itself). Idempotent.
async fn attach_store(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(StorePath { id, store }): ApiPath<StorePath>,
) -> ApiResult<Json<Value>> {
    let store = canonical_store(&store)
        .ok_or_else(|| ApiError::BadRequest("invalid_path", format!("{store} is not a uuid.")))?;
    let mut tx = ctx.begin(&s.db).await?;
    let m = load(&mut tx, &ctx.t.caller.sub, id).await?;
    need_lead(&m)?;
    if m.closed_at.is_some() {
        return Err(ApiError::Conflict(
            "matter_closed",
            "This matter is closed.".into(),
        ));
    }
    check_stores(&mut tx, std::slice::from_ref(&store)).await?;
    let others: Vec<String> = walls::walls(&mut tx)
        .await?
        .into_iter()
        .filter(|w| w.matter_id != id)
        .map(|w| w.store_id)
        .collect();
    if others.contains(&store) {
        return Err(in_other_matter(&store));
    }
    if !others.is_empty() {
        let tree = walls::Tree::load(&s, ctx.t.tenant).await?;
        let mut related = tree.subtree(&store);
        related.extend(tree.ancestors(&store));
        if others.iter().any(|o| related.contains(o)) {
            return Err(in_other_matter(&store));
        }
    }
    let added = sqlx::query(
        "INSERT INTO legal_matter_stores (matter_id, store_id, added_by) VALUES ($1, $2, $3)
         ON CONFLICT (matter_id, store_id) DO NOTHING",
    )
    .bind(id)
    .bind(&store)
    .bind(&ctx.t.caller.sub)
    .execute(&mut *tx)
    .await
    .map_err(|e| match &e {
        // a concurrent attach to another matter: the tenant-wide key
        sqlx::Error::Database(d)
            if d.constraint() == Some("legal_matter_stores_tenant_store_key") =>
        {
            in_other_matter(&store)
        }
        _ => e.into(),
    })?
    .rows_affected();
    if added > 0 {
        record(
            &mut tx,
            &ctx,
            "store_attach",
            id,
            json!({"store_id": store}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"store_id": store})))
}

/// Lead only: detaches a store (404 when not attached). Runs already filed keep their store list.
async fn detach_store(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(StorePath { id, store }): ApiPath<StorePath>,
) -> ApiResult<StatusCode> {
    let store = canonical_store(&store)
        .ok_or_else(|| ApiError::BadRequest("invalid_path", format!("{store} is not a uuid.")))?;
    let mut tx = ctx.begin(&s.db).await?;
    let m = load(&mut tx, &ctx.t.caller.sub, id).await?;
    need_lead(&m)?;
    let gone =
        sqlx::query("DELETE FROM legal_matter_stores WHERE matter_id = $1 AND store_id = $2")
            .bind(id)
            .bind(&store)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if gone == 0 {
        return Err(ApiError::NotFound(format!("store {store} of this matter")));
    }
    record(
        &mut tx,
        &ctx,
        "store_detach",
        id,
        json!({"store_id": store}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_references_are_trimmed_and_bounded() {
        assert_eq!(
            clean_name("  Kowalski v. Nowak ").unwrap(),
            "Kowalski v. Nowak"
        );
        assert!(clean_name("   ").is_err());
        assert!(clean_name(&"x".repeat(201)).is_err());
        assert_eq!(clean_reference(None).unwrap(), None);
        assert_eq!(clean_reference(Some("  ")).unwrap(), None);
        assert_eq!(
            clean_reference(Some(" I C 12/26 ")).unwrap().as_deref(),
            Some("I C 12/26")
        );
        assert!(clean_reference(Some(&"x".repeat(101))).is_err());
    }
}
