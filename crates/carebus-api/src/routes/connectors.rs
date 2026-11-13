//! Backend connectors: a row holds a `secret_ref` name, never a secret value. `POST /{id}/test`
//! records reachability in `connector_health` (an observation, never a config row) so the list never probes live.
//!
//! Reads serve the effective connectors (base slots of the current release ⊕ tenant rows); writes upsert the
//! tenant row of the id only: a new id is an extension, a PUT of a base slot is its override (the tenant supplies
//! `base_url` and `secret_ref`), and the field mappings are rewritten as tenant rows.
use std::time::Instant;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::{
    audit,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    page::Paged,
    refine::{self, Layer, LayerQ, ObjectKind, RowKind},
    secrets,
    state::AppState,
    tenancy::{JobAdmin, JobMember},
};

/// `{id}` under the nested `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
pub struct IdPath {
    id: String,
}

#[derive(Serialize, Deserialize)]
pub struct Mapping {
    pub backend_field: String,
    pub attribute: String,
}
#[derive(Serialize, Deserialize)]
pub struct ConnectorInput {
    pub id: String,
    pub name: String,
    pub system: String,
    pub base_url: String,
    pub auth_type: String,
    pub secret_ref: Option<String>,
    pub timeout_ms: i32,
    pub rate_limit: Option<String>,
    #[serde(default)]
    pub mappings: Vec<Mapping>,
}

const AUTH_TYPES: [&str; 4] = [
    "oauth2_client_credentials",
    "api_key",
    "mtls_api_key",
    "none",
];

/// Connectors of `layer` with their mappings of the same layer and their last health observation.
fn select(layer: Layer) -> String {
    format!(
        "SELECT jsonb_build_object('id', c.id, 'name', c.name, 'system', c.system, 'base_url', c.base_url, 'auth_type', c.auth_type,
            'secret_ref', c.secret_ref, 'timeout_ms', c.timeout_ms, 'rate_limit', c.rate_limit,
            'health', COALESCE(h.health, 'unknown'), 'health_checked_at', h.checked_at,
            'origin', c.origin, 'overridden', c.overridden, 'hidden', c.hidden,
            'mappings', COALESCE((SELECT jsonb_agg(jsonb_build_object('backend_field', m.backend_field, 'attribute', m.attribute) ORDER BY m.backend_field)
                                  FROM {maps} m WHERE m.connector_id = c.id AND NOT m.hidden), '[]'::jsonb))
         FROM {src} c LEFT JOIN connector_health h ON h.tenant_id = c.tenant_id AND h.job_id = c.job_id AND h.connector_id = c.id",
        maps = refine::layer_source(ObjectKind::ConnectorFieldMapping, layer),
        src = refine::layer_source(ObjectKind::Connector, layer),
    )
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connectors", get(list).post(create))
        .route("/connectors/{id}", get(one).put(update))
        .route("/connectors/{id}/test", post(test))
}

async fn load(conn: &mut PgConnection, id: &str) -> ApiResult<Value> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "{} WHERE c.id = $1",
        select(Layer::Effective)
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("connector {id}")))
}

async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<LayerQ>,
) -> ApiResult<Json<Paged<Value>>> {
    let mut tx = ctx.begin(&s.db).await?;
    let items: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "{} ORDER BY c.id",
        select(q.layer.unwrap_or_default())
    )))
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(Paged { items, next: None }))
}

async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(p): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&s.db).await?;
    Ok(Json(load(&mut tx, &p.id).await?))
}

fn bad(code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest(code, msg.into())
}

async fn validate(conn: &mut PgConnection, raw: Value) -> ApiResult<ConnectorInput> {
    secrets::reject_secret_fields(&raw, &[])?;
    let i: ConnectorInput = secrets::parse_body(raw, "connector")?;
    secrets::check_secret_ref(i.secret_ref.as_deref())?;
    if i.id.is_empty()
        || !i
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_".contains(c))
    {
        return Err(bad(
            "invalid_id",
            "id must be lowercase letters, digits, - and _.",
        ));
    }
    if i.name.trim().is_empty() || i.system.trim().is_empty() {
        return Err(bad("invalid_body", "name and system are required."));
    }
    if !(100..=600_000).contains(&i.timeout_ms) {
        return Err(bad("invalid_timeout", "timeout_ms must be 100-600000."));
    }
    if !AUTH_TYPES.contains(&i.auth_type.as_str()) {
        return Err(bad(
            "invalid_auth_type",
            format!("auth_type must be one of {}", AUTH_TYPES.join(", ")),
        ));
    }
    // A credential in a URL would be stored and returned in clear.
    let url_ok = reqwest::Url::parse(&i.base_url).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.host().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    });
    if !url_ok {
        return Err(bad(
            "invalid_base_url",
            "base_url must be an http(s) URL without credentials, query or fragment.",
        ));
    }
    for (n, m) in i.mappings.iter().enumerate() {
        if m.backend_field.trim().is_empty()
            || i.mappings[..n]
                .iter()
                .any(|p| p.backend_field == m.backend_field)
        {
            return Err(bad(
                "invalid_mapping",
                "each mapping needs a distinct, non-empty backend_field.",
            ));
        }
        let (ent, attr) = m.attribute.split_once('.').ok_or_else(|| {
            bad(
                "invalid_mapping",
                format!("{} is not Entity.attribute", m.attribute),
            )
        })?;
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM eff_ontology_attributes WHERE entity=$1 AND name=$2)",
        )
        .bind(ent)
        .bind(attr)
        .fetch_one(&mut *conn)
        .await?;
        if !ok {
            return Err(bad(
                "unknown_attribute",
                format!("the ontology has no attribute {}", m.attribute),
            ));
        }
    }
    Ok(i)
}

/// Does `id` exist as a tenant row or a base slot of the current release (visible or not)?
async fn known(conn: &mut PgConnection, id: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM connectors c WHERE c.id = $1 AND (c.origin = 'tenant' OR c.release_id = {}))",
        refine::current_release("c")
    )))
    .bind(id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Upserts the tenant row of `i.id` (visible, `base_hash` = the current base slot's content_hash, NULL for an
/// extension) and makes `i.mappings` its effective field mappings with tenant rows only: a mapping equal to its base
/// row needs none, a changed or new one is a tenant row, a base mapping left out gets a tombstone. Inserts rely on
/// the scoped transaction's GUC defaults for `tenant_id` / `job_id` (migration 0007). Returns what the row is.
async fn save(conn: &mut PgConnection, i: &ConnectorInput) -> ApiResult<RowKind> {
    let key = ObjectKind::Connector.key_of(&[i.id.as_str()]);
    let base_hash = refine::base_hash_of(conn, ObjectKind::Connector, &key).await?;
    sqlx::query(
        "INSERT INTO connectors (id, name, system, base_url, auth_type, secret_ref, timeout_ms, rate_limit, base_hash)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (tenant_id, job_id, origin, id) DO UPDATE
           SET name=EXCLUDED.name, system=EXCLUDED.system, base_url=EXCLUDED.base_url, auth_type=EXCLUDED.auth_type,
               secret_ref=EXCLUDED.secret_ref, timeout_ms=EXCLUDED.timeout_ms, rate_limit=EXCLUDED.rate_limit,
               base_hash=EXCLUDED.base_hash, hidden=false, updated_at=now()",
    )
    .bind(&i.id)
    .bind(&i.name)
    .bind(&i.system)
    .bind(&i.base_url)
    .bind(&i.auth_type)
    .bind(&i.secret_ref)
    .bind(i.timeout_ms)
    .bind(&i.rate_limit)
    .bind(&base_hash)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM connector_field_mappings WHERE origin = 'tenant' AND connector_id = $1",
    )
    .bind(&i.id)
    .execute(&mut *conn)
    .await?;
    let base: Vec<(String, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT b.backend_field, b.attribute, b.content_hash FROM connector_field_mappings b
          WHERE b.origin = 'base' AND b.release_id = {} AND b.connector_id = $1",
        refine::current_release("b")
    )))
    .bind(&i.id)
    .fetch_all(&mut *conn)
    .await?;
    for m in &i.mappings {
        let b = base.iter().find(|(f, _, _)| *f == m.backend_field);
        if b.is_some_and(|(_, attr, _)| *attr == m.attribute) {
            continue;
        }
        sqlx::query("INSERT INTO connector_field_mappings (connector_id, backend_field, attribute, base_hash) VALUES ($1,$2,$3,$4)")
            .bind(&i.id)
            .bind(&m.backend_field)
            .bind(&m.attribute)
            .bind(b.map(|(_, _, h)| h.clone()))
            .execute(&mut *conn)
            .await?;
    }
    for (field, attr, hash) in &base {
        if i.mappings.iter().any(|m| m.backend_field == *field) {
            continue;
        }
        sqlx::query("INSERT INTO connector_field_mappings (connector_id, backend_field, attribute, hidden, base_hash) VALUES ($1,$2,$3,true,$4)")
            .bind(&i.id)
            .bind(field)
            .bind(attr)
            .bind(hash)
            .execute(&mut *conn)
            .await?;
    }
    Ok(if base_hash.is_some() {
        RowKind::Override
    } else {
        RowKind::Extension
    })
}

async fn create(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    let i = validate(&mut tx, raw).await?;
    if known(&mut tx, &i.id).await? {
        return Err(ApiError::Conflict(
            "connector_exists",
            format!("{} already exists", i.id),
        ));
    }
    let refinement = save(&mut tx, &i).await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "create",
        "connector",
        &i.id,
        true,
        json!({"refinement": refinement.as_str()}),
    )
    .await?;
    let row = load(&mut tx, &i.id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row)))
}

async fn update(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiJson(mut raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let Some(obj) = raw.as_object_mut() else {
        return Err(bad(
            "invalid_body",
            "The request body must be a JSON object.",
        ));
    };
    obj.insert("id".into(), Value::String(id.clone()));
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    if !known(&mut tx, &id).await? {
        return Err(ApiError::NotFound(format!("connector {id}"))); // 404 before body validation
    }
    let i = validate(&mut tx, raw).await?;
    let refinement = save(&mut tx, &i).await?;
    // written against the current base: its open conflicts are settled
    refine::resolve_conflicts(
        &mut tx,
        ObjectKind::Connector,
        &ObjectKind::Connector.key_of(&[id.as_str()]),
        "rebased",
        &ctx.t.caller.actor,
    )
    .await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "update",
        "connector",
        &id,
        true,
        json!({"refinement": refinement.as_str()}),
    )
    .await?;
    let row = load(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Json(row))
}

/// Reachability only: `GET {base_url minus /api}/health`. Sends no credential and reports neither
/// the URL nor any upstream body or error text. No transaction is held across the probe.
async fn test(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let row = {
        let mut tx = ctx.begin(&s.db).await?;
        load(&mut tx, &id).await?
    };
    let base = row["base_url"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('/');
    let url = format!("{}/health", base.strip_suffix("/api").unwrap_or(base));
    let timeout = row["timeout_ms"].as_u64().unwrap_or(3000).clamp(100, 5000);
    let started = Instant::now();
    let res = s
        .probe
        .get(&url)
        .timeout(std::time::Duration::from_millis(timeout))
        .send()
        .await;
    let ms = started.elapsed().as_millis() as u64;
    let ok = res.as_ref().is_ok_and(|r| r.status().is_success());
    let (status, detail) = match &res {
        Ok(r) => (
            Some(r.status().as_u16()),
            if ok {
                "reachable"
            } else {
                "answered with an error status"
            },
        ),
        Err(e) => (
            None,
            if e.is_timeout() {
                "timed out"
            } else {
                "unreachable"
            },
        ),
    };
    let mut tx = ctx.begin(&s.db).await?;
    sqlx::query(
        "INSERT INTO connector_health (connector_id, health, checked_at) VALUES ($1, $2, now())
         ON CONFLICT (tenant_id, job_id, connector_id) DO UPDATE SET health = EXCLUDED.health, checked_at = EXCLUDED.checked_at",
    )
    .bind(&id)
    .bind(if ok { "ok" } else { "down" })
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "test",
        "connector",
        &id,
        true,
        json!({"reachable": ok, "status": status}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"ok": ok, "status": status, "latency_ms": ms, "detail": detail}),
    ))
}
