//! The job's knowledge store layout (W3 plan 11): base stores come with the release (key, label, parent, kind, areas,
//! description); the binding to a strata_rag store (`strata_store_id`) is tenant wiring, never part of a base.
//! `GET /knowledge-stores` serves the effective layout (`?layer=base|tenant` one side); `PUT /knowledge-stores/{key}`
//! overrides a base store's binding and label, or adds a store of the tenant's own (an extension).
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    audit,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    page::Paged,
    refine::{self, Layer, LayerQ, ObjectKind, RowKind},
    secrets,
    state::AppState,
    tenancy::{JobAdmin, JobMember},
};

#[derive(Deserialize)]
struct KeyPath {
    key: String,
}

/// `strata_store_id` binds (or, null, unbinds) the store. `label` may change on any store; `kind`, `parent`,
/// `areas` and `description` belong to the layout, so they are accepted for the tenant's own stores only (and
/// `label`, `kind` and `areas` are required to add one).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreInput {
    #[serde(default)]
    strata_store_id: Option<Uuid>,
    label: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    areas: Option<Vec<String>>,
    #[serde(default)]
    description: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/knowledge-stores", get(list))
        .route("/knowledge-stores/{key}", axum::routing::put(put_store))
}

fn select(layer: Layer) -> String {
    format!(
        "SELECT jsonb_build_object('key', s.key, 'label', s.label, 'parent', s.parent_key, 'kind', s.kind, 'areas', s.areas,
                'description', s.description, 'strata_store_id', s.strata_store_id, 'origin', s.origin,
                'overridden', s.overridden, 'hidden', s.hidden)
           FROM {} s",
        refine::layer_source(ObjectKind::KnowledgeStore, layer)
    )
}

async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<LayerQ>,
) -> ApiResult<Json<Paged<Value>>> {
    let mut tx = ctx.begin(&s.db).await?;
    let items: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "{} ORDER BY s.key",
        select(q.layer.unwrap_or_default())
    )))
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Paged { items, next: None }))
}

async fn load(conn: &mut PgConnection, key: &str) -> ApiResult<Value> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "{} WHERE s.key = $1",
        select(Layer::Effective)
    )))
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("knowledge store {key}")))
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.into())
}

/// `^[a-z][a-z0-9-]{0,62}$` (a store key, a parent key, a context-space area).
fn valid_slug(s: &str) -> bool {
    let mut c = s.chars();
    s.len() <= 63
        && c.next().is_some_and(|f| f.is_ascii_lowercase())
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '-')
}

fn check(i: &StoreInput, key: &str) -> ApiResult<()> {
    if let Some(l) = &i.label
        && (l.trim().is_empty() || l.chars().count() > 120)
    {
        return Err(bad("label must be 1 to 120 characters."));
    }
    if let Some(k) = &i.kind
        && !["group", "leaf"].contains(&k.as_str())
    {
        return Err(bad("kind must be group or leaf."));
    }
    if let Some(p) = &i.parent
        && (!valid_slug(p) || p == key)
    {
        return Err(bad("parent must be another store's key."));
    }
    if i.areas.iter().flatten().any(|a| !valid_slug(a)) {
        return Err(bad("each area must match ^[a-z][a-z0-9-]{0,62}$."));
    }
    Ok(())
}

async fn put_store(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(KeyPath { key }): ApiPath<KeyPath>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !valid_slug(&key) {
        return Err(ApiError::BadRequest(
            "invalid_key",
            "A knowledge store key must match ^[a-z][a-z0-9-]{0,62}$.".into(),
        ));
    }
    secrets::reject_secret_fields(&raw, &[])?;
    let i: StoreInput = secrets::parse_body(raw, "knowledge store")?;
    check(&i, &key)?;
    let nk = ObjectKind::KnowledgeStore.key_of(&[key.as_str()]);
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    if let Some(p) = &i.parent {
        let group: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM eff_knowledge_stores WHERE key = $1 AND kind = 'group')",
        )
        .bind(p)
        .fetch_one(&mut *tx)
        .await?;
        if !group {
            return Err(bad(format!("parent {p} is not a group store of this job.")));
        }
    }
    let base_hash = refine::base_hash_of(&mut tx, ObjectKind::KnowledgeStore, &nk).await?;
    let tenant = refine::tenant_row(&mut tx, ObjectKind::KnowledgeStore, &nk).await?;
    let (status, refinement) = if base_hash.is_none() && tenant.is_none() {
        let (Some(label), Some(kind), Some(areas)) = (&i.label, &i.kind, &i.areas) else {
            return Err(bad("a new store needs label, kind and areas."));
        };
        sqlx::query(
            "INSERT INTO knowledge_stores (key, label, parent_key, kind, areas, description, strata_store_id, updated_by)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(&key)
        .bind(label)
        .bind(&i.parent)
        .bind(kind)
        .bind(areas)
        .bind(&i.description)
        .bind(i.strata_store_id)
        .bind(&ctx.t.caller.actor)
        .execute(&mut *tx)
        .await?;
        (StatusCode::CREATED, RowKind::Extension)
    } else {
        if base_hash.is_some()
            && (i.kind.is_some()
                || i.parent.is_some()
                || i.areas.is_some()
                || i.description.is_some())
        {
            return Err(bad(
                "kind, parent, areas and description of a base store come with the release; only label and strata_store_id can be refined.",
            ));
        }
        refine::ensure_tenant_row(&mut tx, ObjectKind::KnowledgeStore, &nk).await?;
        sqlx::query(
            "UPDATE knowledge_stores
                SET strata_store_id = $2, label = COALESCE($3, label), kind = COALESCE($4, kind), areas = COALESCE($5, areas),
                    parent_key = CASE WHEN $6::text IS NULL THEN parent_key ELSE $6 END,
                    description = COALESCE($7, description), base_hash = $8, hidden = false,
                    updated_at = now(), updated_by = $9
              WHERE origin = 'tenant' AND key = $1",
        )
        .bind(&key)
        .bind(i.strata_store_id)
        .bind(&i.label)
        .bind(&i.kind)
        .bind(&i.areas)
        .bind(&i.parent)
        .bind(&i.description)
        .bind(&base_hash)
        .bind(&ctx.t.caller.actor)
        .execute(&mut *tx)
        .await?;
        refine::resolve_conflicts(
            &mut tx,
            ObjectKind::KnowledgeStore,
            &nk,
            "rebased",
            &ctx.t.caller.actor,
        )
        .await?;
        let r = if base_hash.is_some() {
            RowKind::Override
        } else {
            RowKind::Extension
        };
        (StatusCode::OK, r)
    };
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        if status == StatusCode::CREATED {
            "create"
        } else {
            "update"
        },
        "knowledge_store",
        &key,
        true,
        json!({"refinement": refinement.as_str(), "bound": i.strata_store_id.is_some()}),
    )
    .await?;
    let row = load(&mut tx, &key).await?;
    tx.commit().await?;
    Ok((status, Json(row)))
}
