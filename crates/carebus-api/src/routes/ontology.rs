//! Context spaces (streams derived from the area and the spaces' published procedures) and the ontology (entities,
//! attributes, relations).
//!
//! Reads serve the effective configuration (base of the current release ⊕ tenant refinements, the `eff_*` views of
//! migration 0010); `?layer=base|tenant` serves one side. Writes create or change `origin = 'tenant'` rows only,
//! through `crate::refine`: a new entity or relation is an extension, editing a base entity overrides it, hiding or
//! deleting a base row writes a tombstone. `PUT /context-spaces/{key}` overrides a base space's label, required
//! fields and entities (like `PUT /knowledge-stores/{key}` does a store's label).
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::collections::HashMap;

use crate::{
    audit,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    page::Paged,
    refine::{self, Layer, LayerQ, ObjectKind, RowKind},
    secrets,
    state::AppState,
    tenancy::{JobAdmin, JobMember, ProcedureReader},
};

/// `{name}` under the nested `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
struct NamePath {
    name: String,
}

#[derive(Deserialize)]
struct RelationPath {
    subject: String,
    predicate: String,
    object: String,
}

#[derive(Deserialize)]
struct KeyPath {
    key: String,
}

#[derive(Serialize, sqlx::FromRow)]
struct SpaceRow {
    key: String,
    area: String,
    label: String,
    request_type: String,
    required: Vec<String>,
    entities: Vec<String>,
    procedure: String,
    origin: String,
    overridden: bool,
    hidden: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub data_type: String,
    pub pii: bool,
    #[serde(default)]
    pub synonyms: Vec<String>,
}
/// POST/PUT body. On PUT, an absent `attributes` leaves the entity's attributes as they are; a present one is the
/// complete list (base attributes left out are tombstoned).
#[derive(Serialize, Deserialize)]
pub struct EntityInput {
    pub name: String,
    pub scope: String,
    pub industry: Option<String>,
    pub department: Option<String>,
    pub label: String,
    #[serde(default)]
    pub synonyms: Vec<String>,
    #[serde(default)]
    pub attributes: Option<Vec<Attribute>>,
}

#[derive(Deserialize)]
struct EntitiesQ {
    layer: Option<Layer>,
    /// Adds hidden entities (tombstoned base entities, hidden extensions) with `"hidden": true`: the admin page's
    /// unhide list. Without it a hidden entity is absent (what agents and the console consume).
    #[serde(default)]
    include_hidden: bool,
}

/// Workers per area, derived from configuration: the step workers of each space's effective PUBLISHED (not
/// shadowed) procedure, unioned over the area's spaces. `spaces` are (key, area) in key order, `plans` are
/// (context_space, plan). Order: first appearance by space key, then phase, then step. A space with no published
/// procedure, or a plan without phases, adds no workers.
fn workers_by_area(
    spaces: &[(String, String)],
    plans: &[(String, Value)],
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (key, area) in spaces {
        let workers = out.entry(area.clone()).or_default();
        for (_, plan) in plans.iter().filter(|(space, _)| space == key) {
            let phases = plan.get("phases").and_then(Value::as_array);
            for phase in phases.into_iter().flatten() {
                let steps = phase.get("steps").and_then(Value::as_array);
                for w in steps
                    .into_iter()
                    .flatten()
                    .filter_map(|st| st.get("worker").and_then(Value::as_str))
                {
                    if !workers.iter().any(|x| x == w) {
                        workers.push(w.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Entities of `layer`; in the effective layer `include_hidden` adds the hidden tenant rows.
fn entity_source(layer: Layer, include_hidden: bool) -> String {
    let src = refine::layer_source(ObjectKind::OntologyEntity, layer);
    if layer != Layer::Effective || !include_hidden {
        return src;
    }
    format!(
        "(SELECT * FROM {src}
          UNION ALL
          SELECT t.*, EXISTS (SELECT 1 FROM ontology_entities b
                               WHERE b.tenant_id = t.tenant_id AND b.job_id = t.job_id AND b.name = t.name
                                 AND b.origin = 'base' AND b.release_id = {cur}) AS overridden
            FROM ontology_entities t WHERE t.origin = 'tenant' AND t.hidden)",
        cur = refine::current_release("b"),
    )
}

/// Attributes served with an entity of `layer`. Effective: per (entity, name) the tenant row over the current base
/// row, tombstones dropped (the entity itself may be hidden: its unhide card still lists them).
fn attribute_source(layer: Layer) -> String {
    match layer {
        Layer::Effective => format!(
            "(SELECT w.* FROM (SELECT DISTINCT ON (x.tenant_id, x.job_id, x.entity, x.name) x.*
                                 FROM ontology_attributes x
                                WHERE x.origin = 'tenant' OR x.release_id = {cur}
                                ORDER BY x.tenant_id, x.job_id, x.entity, x.name, (x.origin = 'tenant') DESC) w
               WHERE NOT w.hidden)",
            cur = refine::current_release("x"),
        ),
        Layer::Base => format!(
            "(SELECT x.* FROM ontology_attributes x WHERE x.origin = 'base' AND x.release_id = {cur})",
            cur = refine::current_release("x"),
        ),
        Layer::Tenant => {
            "(SELECT x.* FROM ontology_attributes x WHERE x.origin = 'tenant' AND NOT x.hidden)".into()
        }
    }
}

fn entity_select(layer: Layer, include_hidden: bool) -> String {
    format!(
        "SELECT jsonb_build_object('name', e.name, 'scope', e.scope::text, 'industry', e.industry, 'department', e.department, 'label', e.label,
            'synonyms', e.synonyms, 'hidden', e.hidden, 'origin', e.origin, 'overridden', e.overridden,
            'attributes', COALESCE((SELECT jsonb_agg(jsonb_build_object('name', a.name, 'data_type', a.data_type, 'pii', a.pii, 'synonyms', a.synonyms) ORDER BY a.name)
                                    FROM {attrs} a WHERE a.entity = e.name), '[]'::jsonb))
         FROM {src} e",
        attrs = attribute_source(layer),
        src = entity_source(layer, include_hidden),
    )
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/context-spaces", get(spaces))
        .route("/context-spaces/{key}", axum::routing::put(put_space))
        .route("/ontology/entities", get(entities).post(create))
        .route("/ontology/entities/{name}", get(one).put(update))
        .route("/ontology/relations", get(relations).post(create_relation))
        .route(
            "/ontology/relations/{subject}/{predicate}/{object}",
            delete(delete_relation),
        )
}

/// Cardinalities the ontology uses (subject side : object side).
const CARDINALITIES: [&str; 4] = ["1:1", "1:n", "n:1", "n:m"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelationInput {
    subject: String,
    predicate: String,
    object: String,
    cardinality: String,
}

/// lowerCamel verb phrase, like the seeded `places`, `paidBy`, `shippedVia`.
fn valid_predicate(p: &str) -> bool {
    let mut chars = p.chars();
    p.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Members read it, and so does the agents' service token (naming its tenant): agents route by these spaces.
async fn spaces(
    ProcedureReader(ctx): ProcedureReader,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<LayerQ>,
) -> ApiResult<Json<Paged<Value>>> {
    let layer = q.layer.unwrap_or_default();
    let mut tx = ctx.begin(&s.db).await?;
    let rows: Vec<SpaceRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{} ORDER BY key",
        space_select(layer)
    )))
    .fetch_all(&mut *tx)
    .await?;
    let items = space_items(&mut tx, rows).await?;
    Ok(Json(Paged { items, next: None }))
}

fn space_select(layer: Layer) -> String {
    format!(
        "SELECT key, area, label, request_type, required, entities, procedure, origin, overridden, hidden FROM {}",
        refine::layer_source(ObjectKind::ContextSpace, layer)
    )
}

/// The spaces as the list route serves them. Streams are a runtime fact: always from the effective spaces and their
/// effective PUBLISHED procedures, whatever layer the rows come from.
async fn space_items(tx: &mut PgConnection, rows: Vec<SpaceRow>) -> ApiResult<Vec<Value>> {
    let eff: Vec<(String, String)> =
        sqlx::query_as("SELECT key, area FROM eff_context_spaces ORDER BY key")
            .fetch_all(&mut *tx)
            .await?;
    let plans: Vec<(String, Value)> = sqlx::query_as(
        "SELECT context_space, plan FROM eff_procedures
          WHERE version_status = 'PUBLISHED' AND NOT shadowed ORDER BY context_space, origin",
    )
    .fetch_all(&mut *tx)
    .await?;
    let by_area = workers_by_area(&eff, &plans);
    Ok(rows
        .into_iter()
        .map(|r| {
            let space = r.key.rsplit(':').next().unwrap_or_default().to_string();
            let workers: &[String] = by_area.get(&r.area).map(Vec::as_slice).unwrap_or(&[]);
            json!({"key": r.key, "area": r.area, "label": r.label, "request_type": r.request_type, "required": r.required, "entities": r.entities,
                   "procedure": r.procedure, "origin": r.origin, "overridden": r.overridden, "hidden": r.hidden,
                   "topic_prefix": format!("{}.{space}", r.area),
                   "streams": {"customer": format!("bus:customer:{}", r.area), "worker_results": format!("bus:worker:{}:results", r.area),
                               "worker": workers.iter().map(|w| format!("bus:worker:{}:{w}", r.area)).collect::<Vec<_>>()}})
        })
        .collect())
}

/// PUT body of a context space: the fields a tenant may refine. `label` is what admins and the console read,
/// `required` the fields the qualifier collects before it dispatches, `entities` the ontology entities the space is
/// about. `key` is the space's identity; `area` (its stream names and topic prefix), `request_type` (what the
/// qualifier classifies a message as, and what in-flight work carries) and `procedure` route the space, so they come
/// with the release only (a tenant runs another procedure by publishing a version of its own for the space).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpaceInput {
    label: Option<String>,
    required: Option<Vec<String>>,
    entities: Option<Vec<String>>,
    #[serde(default)]
    key: Option<Value>,
    #[serde(default)]
    area: Option<Value>,
    #[serde(default)]
    request_type: Option<Value>,
    #[serde(default)]
    procedure: Option<Value>,
}

/// Fields a tenant may refine on a context space.
const SPACE_REFINABLE: [&str; 3] = ["label", "required", "entities"];
const SPACE_LABEL_MAX: usize = 120;
const SPACE_LIST_MAX: usize = 64;

fn bad_space(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.into())
}

/// `^[a-z0-9-]+(:[a-z0-9-]+){1,3}$` (the bundle's context-space key).
fn space_key_ok(k: &str) -> bool {
    let parts: Vec<&str> = k.split(':').collect();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// `^[a-z][a-z0-9_]{0,63}$` (a required field, as the bundle validates it).
fn slot_ok(s: &str) -> bool {
    let mut c = s.chars();
    s.len() <= 64
        && c.next().is_some_and(|f| f.is_ascii_lowercase())
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_')
}

fn has_duplicate(v: &[String]) -> Option<&str> {
    v.iter()
        .enumerate()
        .find(|(i, x)| v[..*i].contains(*x))
        .map(|(_, x)| x.as_str())
}

fn check_space(i: &SpaceInput) -> ApiResult<()> {
    if i.key.is_some() || i.area.is_some() || i.request_type.is_some() || i.procedure.is_some() {
        return Err(bad_space(format!(
            "key, area, request_type and procedure of a context space come with the release; only {} can be refined.",
            SPACE_REFINABLE.join(", ")
        )));
    }
    if i.label.is_none() && i.required.is_none() && i.entities.is_none() {
        return Err(bad_space(format!(
            "give at least one of {}.",
            SPACE_REFINABLE.join(", ")
        )));
    }
    if let Some(l) = &i.label
        && (l.trim().is_empty() || l.chars().count() > SPACE_LABEL_MAX)
    {
        return Err(bad_space(format!(
            "label must be 1 to {SPACE_LABEL_MAX} characters."
        )));
    }
    if let Some(r) = &i.required {
        if r.len() > SPACE_LIST_MAX || r.iter().any(|f| !slot_ok(f)) {
            return Err(bad_space(format!(
                "required lists up to {SPACE_LIST_MAX} fields, each matching ^[a-z][a-z0-9_]{{0,63}}$."
            )));
        }
        if let Some(d) = has_duplicate(r) {
            return Err(bad_space(format!("required lists {d} twice.")));
        }
    }
    if let Some(e) = &i.entities {
        if e.len() > SPACE_LIST_MAX {
            return Err(bad_space(format!(
                "entities lists up to {SPACE_LIST_MAX} entities."
            )));
        }
        if let Some(d) = has_duplicate(e) {
            return Err(bad_space(format!("entities lists {d} twice.")));
        }
    }
    Ok(())
}

/// Overrides a context space: copy-on-write of the base row (or the tenant's own row), then the given fields. The
/// tenant row records the base it was written against (`base_hash`), so a release that changes the base raises a
/// `base_changed` conflict; revert drops the row, rebase accepts the new base. Editing a hidden space un-hides it.
/// 404 when the job has no such space.
async fn put_space(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(KeyPath { key }): ApiPath<KeyPath>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    if !space_key_ok(&key) {
        return Err(ApiError::BadRequest(
            "invalid_key",
            "A context space key must match ^[a-z0-9-]+(:[a-z0-9-]+){1,3}$.".into(),
        ));
    }
    secrets::reject_secret_fields(&raw, &[])?;
    let i: SpaceInput = secrets::parse_body(raw, "context space")?;
    check_space(&i)?;
    let nk = ObjectKind::ContextSpace.key_of(&[key.as_str()]);
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    let base_hash = refine::base_hash_of(&mut tx, ObjectKind::ContextSpace, &nk).await?;
    if base_hash.is_none()
        && refine::tenant_row(&mut tx, ObjectKind::ContextSpace, &nk)
            .await?
            .is_none()
    {
        return Err(ApiError::NotFound(format!("context space {key}")));
    }
    if let Some(entities) = &i.entities {
        let unknown: Option<String> = sqlx::query_scalar(
            "SELECT e FROM unnest($1::text[]) WITH ORDINALITY AS u(e, n)
              WHERE NOT EXISTS (SELECT 1 FROM eff_ontology_entities o WHERE o.name = u.e)
              ORDER BY u.n LIMIT 1",
        )
        .bind(entities)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(e) = unknown {
            return Err(ApiError::BadRequest(
                "unknown_entity",
                format!("entity {e} is not an existing entity of this job."),
            ));
        }
    }
    refine::ensure_tenant_row(&mut tx, ObjectKind::ContextSpace, &nk).await?;
    sqlx::query(
        "UPDATE context_spaces
            SET label = COALESCE($2, label), required = COALESCE($3, required), entities = COALESCE($4, entities),
                base_hash = $5, hidden = false
          WHERE origin = 'tenant' AND key = $1",
    )
    .bind(&key)
    .bind(i.label.as_deref().map(str::trim))
    .bind(&i.required)
    .bind(&i.entities)
    .bind(&base_hash)
    .execute(&mut *tx)
    .await?;
    refine::resolve_conflicts(
        &mut tx,
        ObjectKind::ContextSpace,
        &nk,
        "rebased",
        &ctx.t.caller.actor,
    )
    .await?;
    let refinement = if base_hash.is_some() {
        RowKind::Override
    } else {
        RowKind::Extension
    };
    let fields: Vec<&str> = [
        ("label", i.label.is_some()),
        ("required", i.required.is_some()),
        ("entities", i.entities.is_some()),
    ]
    .into_iter()
    .filter_map(|(f, set)| set.then_some(f))
    .collect();
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "update",
        "context_space",
        &key,
        true,
        json!({"refinement": refinement.as_str(), "fields": fields}),
    )
    .await?;
    let rows: Vec<SpaceRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{} WHERE key = $1",
        space_select(Layer::Effective)
    )))
    .bind(&key)
    .fetch_all(&mut *tx)
    .await?;
    let row = space_items(&mut tx, rows)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::NotFound(format!("context space {key}")))?;
    tx.commit().await?;
    Ok(Json(row))
}

async fn entities(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<EntitiesQ>,
) -> ApiResult<Json<Paged<Value>>> {
    let layer = q.layer.unwrap_or_default();
    let mut tx = ctx.begin(&s.db).await?;
    let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "{} ORDER BY e.scope, e.name",
        entity_select(layer, q.include_hidden)
    )))
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Paged {
        items: rows,
        next: None,
    }))
}

/// One entity as an admin sees it: effective, or hidden (a tombstone or a hidden extension, `"hidden": true`).
async fn fetch(conn: &mut PgConnection, name: &str) -> ApiResult<Value> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "{} WHERE e.name = $1",
        entity_select(Layer::Effective, true)
    )))
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("entity {name}")))
}

async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(NamePath { name }): ApiPath<NamePath>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&s.db).await?;
    Ok(Json(fetch(&mut tx, &name).await?))
}

/// The base attributes of `entity` in the current release: (name, content_hash, attribute).
async fn base_attributes(
    tx: &mut PgConnection,
    entity: &str,
) -> ApiResult<Vec<(String, String, Attribute)>> {
    let rows: Vec<(String, String, String, bool, Vec<String>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT b.name, b.content_hash, b.data_type, b.pii, b.synonyms FROM ontology_attributes b
              WHERE b.origin = 'base' AND b.release_id = {} AND b.entity = $1",
            refine::current_release("b")
        )))
        .bind(entity)
        .fetch_all(&mut *tx)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(name, hash, data_type, pii, synonyms)| {
            (
                name.clone(),
                hash,
                Attribute {
                    name,
                    data_type,
                    pii,
                    synonyms,
                },
            )
        })
        .collect())
}

/// Makes `attrs` the effective attribute list of `entity` with tenant rows only: the tenant's attribute rows are
/// rewritten; an attribute equal to its base row needs none, one that differs is an override, a new one an
/// extension, and a base attribute left out gets a tombstone.
async fn write_attrs(tx: &mut PgConnection, entity: &str, attrs: &[Attribute]) -> ApiResult<()> {
    for (i, a) in attrs.iter().enumerate() {
        if attrs[..i].iter().any(|p| p.name == a.name) {
            return Err(ApiError::Conflict(
                "duplicate_attribute",
                format!("attribute {} is listed twice", a.name),
            ));
        }
    }
    sqlx::query("DELETE FROM ontology_attributes WHERE origin = 'tenant' AND entity = $1")
        .bind(entity)
        .execute(&mut *tx)
        .await?;
    let base = base_attributes(tx, entity).await?;
    for a in attrs {
        let b = base.iter().find(|(n, _, _)| *n == a.name);
        if b.is_some_and(|(_, _, ba)| ba == a) {
            continue;
        }
        sqlx::query("INSERT INTO ontology_attributes (entity, name, data_type, pii, synonyms, base_hash) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(entity)
            .bind(&a.name)
            .bind(&a.data_type)
            .bind(a.pii)
            .bind(&a.synonyms)
            .bind(b.map(|(_, h, _)| h.clone()))
            .execute(&mut *tx)
            .await?;
    }
    for (name, hash, ba) in &base {
        if attrs.iter().any(|a| a.name == *name) {
            continue;
        }
        sqlx::query("INSERT INTO ontology_attributes (entity, name, data_type, pii, synonyms, hidden, base_hash) VALUES ($1,$2,$3,$4,$5,true,$6)")
            .bind(entity)
            .bind(name)
            .bind(&ba.data_type)
            .bind(ba.pii)
            .bind(&ba.synonyms)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

fn scope_error(e: sqlx::Error) -> ApiError {
    match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("22P02") => ApiError::BadRequest(
            "invalid_scope",
            "scope must be generic, industry or industry_dept".into(),
        ),
        _ => e.into(),
    }
}

fn present(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.trim().is_empty())
}

fn check(i: &EntityInput) -> ApiResult<()> {
    if i.name.trim().is_empty()
        || i.label.trim().is_empty()
        || i.name.contains('/')
        || i.attributes
            .iter()
            .flatten()
            .any(|a| a.name.trim().is_empty() || a.data_type.trim().is_empty())
    {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "name, label and every attribute's name and data_type are required, and name cannot contain /.".into(),
        ));
    }
    match i.scope.as_str() {
        "industry" if !present(&i.industry) => Err(ApiError::BadRequest(
            "invalid_scope",
            "scope industry requires an industry.".into(),
        )),
        "industry_dept" if !present(&i.industry) || !present(&i.department) => {
            Err(ApiError::BadRequest(
                "invalid_scope",
                "scope industry_dept requires an industry and a department.".into(),
            ))
        }
        _ => Ok(()),
    }
}

/// Does `name` exist as a tenant row or a base row of the current release (visible or not)?
async fn entity_known(tx: &mut PgConnection, name: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM ontology_entities e WHERE e.name = $1 AND (e.origin = 'tenant' OR e.release_id = {}))",
        refine::current_release("e")
    )))
    .bind(name)
    .fetch_one(&mut *tx)
    .await?)
}

async fn create(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    secrets::reject_secret_fields(&raw, &[])?;
    let i: EntityInput = secrets::parse_body(raw, "entity")?;
    check(&i)?;
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    if entity_known(&mut tx, &i.name).await? {
        return Err(ApiError::Conflict(
            "entity_exists",
            format!("{} already exists", i.name),
        ));
    }
    let r = sqlx::query("INSERT INTO ontology_entities (name, scope, industry, department, label, synonyms) VALUES ($1,$2::ontology_scope,$3,$4,$5,$6)")
        .bind(&i.name).bind(&i.scope).bind(&i.industry).bind(&i.department).bind(&i.label).bind(&i.synonyms).execute(&mut *tx).await;
    match r {
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23505") => {
            return Err(ApiError::Conflict(
                "entity_exists",
                format!("{} already exists", i.name),
            ));
        }
        Err(e) => return Err(scope_error(e)),
        Ok(_) => {}
    }
    write_attrs(
        &mut tx,
        &i.name,
        i.attributes.as_deref().unwrap_or_default(),
    )
    .await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "create",
        "ontology_entity",
        &i.name,
        true,
        json!({"refinement": RowKind::Extension.as_str()}),
    )
    .await?;
    let row = fetch(&mut tx, &i.name).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row)))
}

async fn update(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(NamePath { name }): ApiPath<NamePath>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let Some(obj) = v.as_object() else {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "The request body must be a JSON object.".into(),
        ));
    };
    secrets::reject_secret_fields(&v, &[])?;
    let key = ObjectKind::OntologyEntity.key_of(&[name.as_str()]);
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    if !entity_known(&mut tx, &name).await? {
        return Err(ApiError::NotFound(format!("entity {name}")));
    }
    let (action, refinement) = if obj.len() == 1 && obj.contains_key("hidden") {
        let Some(h) = obj["hidden"].as_bool() else {
            return Err(ApiError::BadRequest(
                "invalid_body",
                "hidden must be true or false.".into(),
            ));
        };
        if h {
            refine::hide(&mut tx, ObjectKind::OntologyEntity, &key).await?;
            ("hide", RowKind::Tombstone)
        } else {
            refine::unhide(&mut tx, ObjectKind::OntologyEntity, &key).await?;
            ("unhide", RowKind::Tombstone)
        }
    } else {
        let mut v = v;
        v["name"] = Value::String(name.clone());
        let i: EntityInput = secrets::parse_body(v, "entity")?;
        check(&i)?;
        let kind = refine::ensure_tenant_row(&mut tx, ObjectKind::OntologyEntity, &key).await?;
        // written against the current base: the override's base_hash follows it
        let base_hash = refine::base_hash_of(&mut tx, ObjectKind::OntologyEntity, &key).await?;
        sqlx::query("UPDATE ontology_entities SET scope=$2::ontology_scope, industry=$3, department=$4, label=$5, synonyms=$6, base_hash=$7, updated_at=now()
                     WHERE name=$1 AND origin='tenant'")
            .bind(&name).bind(&i.scope).bind(&i.industry).bind(&i.department).bind(&i.label).bind(&i.synonyms).bind(&base_hash)
            .execute(&mut *tx).await
            .map_err(scope_error)?;
        if let Some(attrs) = &i.attributes {
            write_attrs(&mut tx, &name, attrs).await?;
        }
        let orphan: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT m.attribute FROM eff_connector_field_mappings m
             WHERE split_part(m.attribute, '.', 1) = $1
               AND NOT EXISTS (SELECT 1 FROM {} a
                               WHERE a.entity = $1 AND a.name = substr(m.attribute, length($1) + 2))
             LIMIT 1",
            attribute_source(Layer::Effective)
        )))
        .bind(&name)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(attr) = orphan {
            // dropping `tx` rolls the whole update back
            return Err(ApiError::Conflict(
                "attribute_in_use",
                format!("a connector still maps {attr}; remove that mapping first."),
            ));
        }
        refine::resolve_conflicts(
            &mut tx,
            ObjectKind::OntologyEntity,
            &key,
            "rebased",
            &ctx.t.caller.actor,
        )
        .await?;
        let refinement = match kind {
            RowKind::Tombstone => RowKind::Tombstone,
            _ if base_hash.is_some() => RowKind::Override,
            _ => RowKind::Extension,
        };
        ("update", refinement)
    };
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        action,
        "ontology_entity",
        &name,
        true,
        json!({"refinement": refinement.as_str()}),
    )
    .await?;
    let row = fetch(&mut tx, &name).await?;
    tx.commit().await?;
    Ok(Json(row))
}

async fn relations(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<LayerQ>,
) -> ApiResult<Json<Paged<Value>>> {
    let layer = q.layer.unwrap_or_default();
    let mut tx = ctx.begin(&s.db).await?;
    let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT jsonb_build_object('subject', r.subject, 'predicate', r.predicate, 'object', r.object, 'cardinality', r.cardinality,
                'origin', r.origin, 'overridden', r.overridden, 'hidden', r.hidden)
           FROM {} r ORDER BY r.subject, r.predicate, r.object",
        refine::layer_source(ObjectKind::OntologyRelation, layer)
    )))
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Paged {
        items: rows,
        next: None,
    }))
}

async fn relation_effective(
    tx: &mut PgConnection,
    subject: &str,
    predicate: &str,
    object: &str,
) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM eff_ontology_relations WHERE subject=$1 AND predicate=$2 AND object=$3)",
    )
    .bind(subject)
    .bind(predicate)
    .bind(object)
    .fetch_one(&mut *tx)
    .await?)
}

async fn create_relation(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    secrets::reject_secret_fields(&raw, &[])?;
    let i: RelationInput = secrets::parse_body(raw, "relation")?;
    if !valid_predicate(&i.predicate) {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "predicate must be a lowerCamel verb such as places or paidBy (letters and digits, up to 64).".into(),
        ));
    }
    if !CARDINALITIES.contains(&i.cardinality.as_str()) {
        return Err(ApiError::BadRequest(
            "invalid_body",
            format!("cardinality must be one of {}.", CARDINALITIES.join(", ")),
        ));
    }
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    for (role, name) in [("subject", &i.subject), ("object", &i.object)] {
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM eff_ontology_entities WHERE name=$1)")
                .bind(name)
                .fetch_one(&mut *tx)
                .await?;
        if !known {
            return Err(ApiError::BadRequest(
                "unknown_entity",
                format!("{role} {name} is not an existing entity."),
            ));
        }
    }
    if relation_effective(&mut tx, &i.subject, &i.predicate, &i.object).await? {
        return Err(ApiError::Conflict(
            "relation_exists",
            format!("{} {} {} already exists.", i.subject, i.predicate, i.object),
        ));
    }
    let key = ObjectKind::OntologyRelation.key_of(&[
        i.subject.as_str(),
        i.predicate.as_str(),
        i.object.as_str(),
    ]);
    let base: Option<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT b.content_hash, b.cardinality FROM ontology_relations b
          WHERE b.origin = 'base' AND b.release_id = {} AND b.subject=$1 AND b.predicate=$2 AND b.object=$3",
        refine::current_release("b")
    )))
    .bind(&i.subject)
    .bind(&i.predicate)
    .bind(&i.object)
    .fetch_optional(&mut *tx)
    .await?;
    // Not effective with both ends effective: either nothing, or a tombstone of this key.
    let tenant = refine::tenant_row(&mut tx, ObjectKind::OntologyRelation, &key).await?;
    let refinement = match (tenant, &base) {
        // re-creating a tombstoned base relation as it was: the tombstone goes, the base row serves again
        (Some(_), Some((_, card))) if *card == i.cardinality => {
            refine::revert(
                &mut tx,
                ObjectKind::OntologyRelation,
                &key,
                &ctx.t.caller.actor,
            )
            .await?;
            RowKind::Tombstone
        }
        (Some(_), b) => {
            sqlx::query(
                "UPDATE ontology_relations SET hidden=false, cardinality=$4, base_hash=$5
                  WHERE origin='tenant' AND subject=$1 AND predicate=$2 AND object=$3",
            )
            .bind(&i.subject)
            .bind(&i.predicate)
            .bind(&i.object)
            .bind(&i.cardinality)
            .bind(b.as_ref().map(|(h, _)| h.clone()))
            .execute(&mut *tx)
            .await?;
            if b.is_some() {
                RowKind::Override
            } else {
                RowKind::Extension
            }
        }
        (None, b) => {
            sqlx::query(
                "INSERT INTO ontology_relations (subject, predicate, object, cardinality, base_hash) VALUES ($1,$2,$3,$4,$5)",
            )
            .bind(&i.subject)
            .bind(&i.predicate)
            .bind(&i.object)
            .bind(&i.cardinality)
            .bind(b.as_ref().map(|(h, _)| h.clone()))
            .execute(&mut *tx)
            .await
            .map_err(|e| match e {
                sqlx::Error::Database(d) if d.is_unique_violation() => ApiError::Conflict(
                    "relation_exists",
                    format!("{} {} {} already exists.", i.subject, i.predicate, i.object),
                ),
                e => e.into(),
            })?;
            if b.is_some() {
                RowKind::Override
            } else {
                RowKind::Extension
            }
        }
    };
    let id = format!("{}/{}/{}", i.subject, i.predicate, i.object);
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "create",
        "ontology_relation",
        &id,
        true,
        json!({"refinement": refinement.as_str()}),
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"subject": i.subject, "predicate": i.predicate, "object": i.object, "cardinality": i.cardinality}),
        ),
    ))
}

/// A base relation gets a tombstone; an extension is deleted. 404 when the relation is not effective.
async fn delete_relation(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(RelationPath {
        subject,
        predicate,
        object,
    }): ApiPath<RelationPath>,
) -> ApiResult<StatusCode> {
    let id = format!("{subject}/{predicate}/{object}");
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    if !relation_effective(&mut tx, &subject, &predicate, &object).await? {
        return Err(ApiError::NotFound(format!("relation {id}")));
    }
    let key = ObjectKind::OntologyRelation.key_of(&[
        subject.as_str(),
        predicate.as_str(),
        object.as_str(),
    ]);
    let refinement = if refine::base_hash_of(&mut tx, ObjectKind::OntologyRelation, &key)
        .await?
        .is_some()
    {
        refine::hide(&mut tx, ObjectKind::OntologyRelation, &key).await?;
        RowKind::Tombstone
    } else {
        sqlx::query(
            "DELETE FROM ontology_relations WHERE origin='tenant' AND subject=$1 AND predicate=$2 AND object=$3",
        )
        .bind(&subject)
        .bind(&predicate)
        .bind(&object)
        .execute(&mut *tx)
        .await?;
        RowKind::Extension
    };
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "delete",
        "ontology_relation",
        &id,
        true,
        json!({"refinement": refinement.as_str()}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(workers: &[&[&str]]) -> Value {
        json!({"phases": workers.iter().map(|ph| json!({"steps": ph.iter().map(|w| json!({"worker": w, "action": "a", "as": w, "args": {}})).collect::<Vec<_>>(), "gate": null})).collect::<Vec<_>>()})
    }

    fn s(key: &str, area: &str) -> (String, String) {
        (key.into(), area.into())
    }

    #[test]
    fn workers_follow_the_published_procedure() {
        let spaces = [
            s("retail:faq:general", "faq"),
            s("retail:logistics:delivery-status", "logistics"),
            s("retail:order-support:returns", "order-support"),
            s("retail:order-support:warranty", "order-support"),
            s("retail:payments:payment-errors", "payments"),
            s("retail:returns-desk:x", "returns-desk"),
            s("retail:broken:y", "broken"),
        ];
        let full: &[&[&str]] = &[
            &["crm", "order"],
            &["business-rules", "procedures"],
            &["order"],
        ];
        let plans = [
            ("retail:faq:general".to_string(), plan(&[&["faq"]])),
            (
                "retail:logistics:delivery-status".to_string(),
                plan(&[&["order"], &["delivery"]]),
            ),
            ("retail:order-support:returns".to_string(), plan(full)),
            ("retail:order-support:warranty".to_string(), plan(full)),
            ("retail:payments:payment-errors".to_string(), plan(full)),
            ("retail:broken:y".to_string(), json!({"steps": []})),
        ];
        let got = workers_by_area(&spaces, &plans);
        let four = ["crm", "order", "business-rules", "procedures"];
        assert_eq!(got["order-support"], four);
        assert_eq!(got["payments"], four);
        assert_eq!(got["logistics"], ["order", "delivery"]);
        assert_eq!(got["faq"], ["faq"]);
        assert!(got["returns-desk"].is_empty(), "no published procedure");
        assert!(
            got["broken"].is_empty(),
            "a plan without phases adds nothing"
        );
    }

    fn input(v: Value) -> SpaceInput {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn space_keys_and_fields_validate() {
        assert!(space_key_ok("retail:faq:general"));
        assert!(space_key_ok("a:b"));
        for bad in [
            "faq",
            "retail::general",
            "Retail:faq",
            "a:b:c:d:e",
            "retail:faq_x",
        ] {
            assert!(!space_key_ok(bad), "{bad}");
        }
        assert!(slot_ok("order_id"));
        assert!(!slot_ok("Order") && !slot_ok("1x") && !slot_ok("") && !slot_ok("a-b"));
        assert!(check_space(&input(json!({"label": "FAQ"}))).is_ok());
        assert!(check_space(&input(json!({"required": [], "entities": ["Policy"]}))).is_ok());
        for bad in [
            json!({}),
            json!({"label": " "}),
            json!({"label": "x".repeat(121)}),
            json!({"required": ["order_id", "order_id"]}),
            json!({"required": ["Order"]}),
            json!({"entities": ["Policy", "Policy"]}),
            json!({"label": "x", "request_type": "faq"}),
            json!({"label": "x", "area": "faq"}),
            json!({"label": "x", "key": "retail:faq:other"}),
            json!({"label": "x", "procedure": "faq_answer"}),
        ] {
            assert_eq!(
                check_space(&input(bad.clone())).unwrap_err().code(),
                "invalid_body",
                "{bad}"
            );
        }
        assert!(serde_json::from_value::<SpaceInput>(json!({"colour": "red"})).is_err());
    }
}
