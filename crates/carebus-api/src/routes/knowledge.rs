//! strata_rag proxy. Stores are normalised to `Paged {items, next: null}`; documents map `limit`/`before` onto strata_rag's `limit`/`cursor`.
//!
//! Ethical walls (migration 0026, crate::walls): a store attached to a legal matter, and everything below it, is
//! invisible to whoever is not a member of that matter (a tenant_admin included): absent from the store list, 404 for
//! its documents and for a query naming it, and a query routed through it searches only the visible leaves.
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{
    Json, Router,
    extract::State,
    routing::{delete, get, post},
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit;
use crate::auth::{Caller, CallerKind};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::{Page, Paged};
use crate::secrets;
use crate::state::AppState;
use crate::strata::{self, Gone};
use crate::tenancy::{Member, TenantAdmin, TenantCtx};
use crate::upstream::{Reply, segment};
use crate::walls;

/// Absolute path; merged by `routes::all`. Service token only (the bus `faq` worker), as the gate route.
pub fn internal_routes() -> Router<AppState> {
    Router::new().route("/api/internal/faq/scope", post(faq_scope))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FaqScopeInput {
    tenant_id: Uuid,
}

/// What the customer chat of `tenant_id` may search ([`walls::chat_leaves`]): `{"restricted": false}` when the
/// tenant has no walled store, else `{"restricted": true, "leaf_ids": [...]}` (empty: search nothing). The worker
/// passes `leaf_ids` to `faq_ask`, and the executor accepts the answer only when the MCP server confirms the filter.
async fn faq_scope(
    caller: Caller,
    State(s): State<AppState>,
    ApiJson(i): ApiJson<FaqScopeInput>,
) -> ApiResult<Json<Value>> {
    if !matches!(caller.kind, CallerKind::Service) {
        return Err(ApiError::Forbidden("service token"));
    }
    if i.tenant_id.is_nil() {
        return Err(ApiError::BadRequest(
            "invalid_tenant",
            "tenant_id must be a non-nil uuid.".into(),
        ));
    }
    Ok(Json(match walls::chat_leaves(&s, i.tenant_id).await? {
        None => json!({"restricted": false}),
        Some(leaves) => json!({"restricted": true, "leaf_ids": leaves}),
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/knowledge/stores", get(stores).post(create_store))
        .route("/api/knowledge/stores/{id}", delete(delete_store))
        .route("/api/knowledge/stores/{id}/documents", get(documents))
        .route("/api/knowledge/documents/{id}", delete(delete_document))
        .route("/api/knowledge/query", post(query))
}

enum Outcome {
    Ok(Value),
    /// Another upstream 4xx: its status goes to the client, inside our error envelope.
    Rejected(StatusCode),
}

async fn proxy(
    s: &AppState,
    tenant: Uuid,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> ApiResult<Outcome> {
    let Reply { status, body } = strata::call(s, tenant, method, path, body).await?;
    // A route strata_rag does not have yet (404/405) is "unavailable", never an empty result.
    if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
        return Err(ApiError::Upstream(strata::NAME));
    }
    if status.is_client_error() {
        return Ok(Outcome::Rejected(status));
    }
    Ok(Outcome::Ok(body))
}

fn rejected(status: StatusCode) -> Response {
    (
        status,
        Json(json!({"error": {
            "code": "upstream_rejected",
            "message": format!("{} rejected the request (HTTP {}).", strata::NAME, status.as_u16()),
        }})),
    )
        .into_response()
}

/// The list inside an upstream reply: a bare array, or an object holding it under `key` or `items`.
fn items_of(v: Value, key: &str) -> ApiResult<Vec<Value>> {
    match v {
        Value::Array(a) => Ok(a),
        Value::Object(mut o) => match o.remove(key).or_else(|| o.remove("items")) {
            Some(Value::Array(a)) => Ok(a),
            _ => Err(ApiError::Upstream(strata::NAME)),
        },
        _ => Err(ApiError::Upstream(strata::NAME)),
    }
}

async fn list(s: &AppState, tenant: Uuid, path: &str, key: &str) -> ApiResult<Response> {
    match proxy(s, tenant, Method::GET, path, None).await? {
        Outcome::Ok(v) => Ok(Json(Paged {
            items: items_of(v, key)?,
            next: None,
        })
        .into_response()),
        Outcome::Rejected(st) => Ok(rejected(st)),
    }
}

async fn stores(Member(ctx): Member, State(s): State<AppState>) -> ApiResult<Response> {
    let (hidden, _) = walls::hidden(&s, &ctx).await?;
    if hidden.is_empty() {
        return list(&s, ctx.tenant, "/api/stores", "stores").await;
    }
    match proxy(&s, ctx.tenant, Method::GET, "/api/stores", None).await? {
        Outcome::Ok(v) => {
            let mut items = items_of(v, "stores")?;
            items.retain(|st| {
                st["id"]
                    .as_str()
                    .is_none_or(|id| !hidden.contains(&walls::canonical(id)))
            });
            for st in &mut items {
                if let Some(c) = st.get_mut("children").and_then(Value::as_array_mut) {
                    c.retain(|id| {
                        id.as_str()
                            .is_none_or(|id| !hidden.contains(&walls::canonical(id)))
                    });
                }
            }
            Ok(Json(Paged { items, next: None }).into_response())
        }
        Outcome::Rejected(st) => Ok(rejected(st)),
    }
}

#[derive(Debug, Deserialize)]
struct DocsQuery {
    limit: Option<i64>,
    before: Option<String>,
}

/// Percent-encode everything but RFC 3986 unreserved characters (the cursor is opaque to us).
fn encode_component(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Our `limit`/`before` onto strata_rag's `limit`/`cursor`; its `{items, next}` back as our `Paged`.
/// A missing `items` array or a `next` that is neither string nor null is 503, never an empty list.
async fn documents(
    Member(ctx): Member,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<DocsQuery>,
) -> ApiResult<Response> {
    let id = segment(&id, true)?;
    let (hidden, _) = walls::hidden(&s, &ctx).await?;
    if hidden.contains(&walls::canonical(id)) {
        return Err(ApiError::NotFound(format!("store {id}")));
    }
    let page = Page::new(q.limit, q.before);
    let mut path = format!("/api/stores/{id}/documents?limit={}", page.limit());
    if let Some(c) = &page.before {
        path.push_str("&cursor=");
        path.push_str(&encode_component(c));
    }
    match proxy(&s, ctx.tenant, Method::GET, &path, None).await? {
        Outcome::Ok(Value::Object(mut o)) => {
            let items = match o.remove("items") {
                Some(Value::Array(a)) => a,
                _ => return Err(ApiError::Upstream(strata::NAME)),
            };
            let next = match o.remove("next") {
                None | Some(Value::Null) => None,
                Some(Value::String(c)) => Some(c),
                Some(_) => return Err(ApiError::Upstream(strata::NAME)),
            };
            Ok(Json(Paged { items, next }).into_response())
        }
        Outcome::Ok(_) => Err(ApiError::Upstream(strata::NAME)),
        Outcome::Rejected(st) => Ok(rejected(st)),
    }
}

/// Read-only search preview; body passed through, confined to the stores the caller may see (crate::walls).
async fn query(
    Member(ctx): Member,
    State(s): State<AppState>,
    ApiJson(mut b): ApiJson<Value>,
) -> ApiResult<Response> {
    let (hidden, tree) = walls::hidden(&s, &ctx).await?;
    walls::confine_query(&tree, &hidden, &mut b)?;
    match proxy(&s, ctx.tenant, Method::POST, "/api/query", Some(b)).await? {
        Outcome::Ok(v) => Ok(Json(v).into_response()),
        Outcome::Rejected(st) => Ok(rejected(st)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreInput {
    parent_id: Option<Uuid>,
    kind: String,
    name: String,
    #[serde(default)]
    description: String,
}

/// Audit row for a write, in the request's tenant (knowledge is tenant-level: job GUC empty): `details` hold ids
/// we chose, plus on failure our own error code and HTTP status, never `e.to_string()` or upstream text.
pub(crate) async fn audit_write<T>(
    s: &AppState,
    ctx: &TenantCtx,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    res: &ApiResult<T>,
    mut details: Value,
) -> ApiResult<()> {
    if let Err(e) = res {
        let (status, code) = e.status_and_code();
        details["error_code"] = json!(code);
        details["http_status"] = json!(status.as_u16());
    }
    let mut tx = ctx.begin(&s.db).await?;
    audit::record(
        &mut tx,
        &ctx.caller.actor,
        ctx.caller.actor_type(),
        action,
        resource_type,
        resource_id,
        res.is_ok(),
        details,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn create_store(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    strata::provisioned_for(&s, ctx.tenant).await?;
    secrets::reject_secret_fields(&raw, &[])?;
    let i: StoreInput = secrets::parse_body(raw, "store")?;
    if !matches!(i.kind.as_str(), "router" | "leaf") {
        return Err(ApiError::BadRequest(
            "invalid_kind",
            "kind must be router or leaf.".into(),
        ));
    }
    let name = i.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::BadRequest(
            "invalid_name",
            "name must be 1-100 characters.".into(),
        ));
    }
    if i.description.chars().count() > 2000 {
        return Err(ApiError::BadRequest(
            "invalid_description",
            "description must be at most 2000 characters.".into(),
        ));
    }
    let body = json!({"parent_id": i.parent_id, "kind": i.kind, "name": name, "description": i.description});
    let res = strata::create_store(&s, ctx.tenant, body).await;
    let resource = match &res {
        Ok(v) => v["id"].as_str().unwrap_or(name).to_string(),
        Err(_) => name.to_string(),
    };
    audit_write(
        &s,
        &ctx,
        "create",
        "knowledge_store",
        &resource,
        &res,
        json!({"kind": i.kind, "parent_id": i.parent_id}),
    )
    .await?;
    res.map(|v| (StatusCode::CREATED, Json(v)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteStoreQuery {
    recursive: Option<bool>,
}

/// Refused while any source still writes into the store (or, recursively, into a store below it): deleting it
/// would leave those sources pointing at nothing.
async fn delete_store(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<DeleteStoreQuery>,
) -> ApiResult<StatusCode> {
    strata::provisioned_for(&s, ctx.tenant).await?;
    let recursive = q.recursive.unwrap_or(false);
    let affected = if recursive {
        strata::descendants(&s, ctx.tenant, id).await?
    } else {
        vec![id]
    };
    // A store walled for a matter the admin is not a member of (or one below it) does not exist for them.
    let (hidden, _) = walls::hidden(&s, &ctx).await?;
    if affected.iter().any(|a| hidden.contains(&a.to_string())) {
        return Err(ApiError::NotFound(format!("store {id}")));
    }
    // Every job's sources of the tenant (job GUC empty); the read transaction ends before strata_rag is called.
    let mut tx = ctx.begin(&s.db).await?;
    let users: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sources WHERE target_store_id = ANY($1) ORDER BY name",
    )
    .bind(&affected)
    .fetch_all(&mut *tx)
    .await?;
    drop(tx);
    if !users.is_empty() {
        return Err(ApiError::Conflict(
            "store_in_use",
            format!(
                "Sources still write to this store: {}. Point them elsewhere or delete them first.",
                users.join(", ")
            ),
        ));
    }
    let res = strata::delete_store(&s, ctx.tenant, id, recursive).await;
    audit_write(
        &s,
        &ctx,
        "delete",
        "knowledge_store",
        &id.to_string(),
        &res,
        json!({"recursive": recursive}),
    )
    .await?;
    res.map(|_| StatusCode::NO_CONTENT)
}

async fn delete_document(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    strata::provisioned_for(&s, ctx.tenant).await?;
    // A document in a store walled for a matter the admin is not a member of does not exist for them. strata_rag has
    // no document lookup, so the walled leaves are listed (only when there is such a wall); a failed listing refuses.
    let (hidden, tree) = walls::hidden(&s, &ctx).await?;
    if walled_document(&s, &ctx, &tree, &hidden, &id.to_string()).await? {
        return Err(ApiError::NotFound(format!("document {id}")));
    }
    let res = match strata::delete_document(&s, ctx.tenant, &id.to_string()).await {
        Ok(Gone::Deleted) => Ok(StatusCode::NO_CONTENT),
        Ok(Gone::Missing) => Err(ApiError::NotFound(format!("document {id}"))),
        Err(e) => Err(e),
    };
    audit_write(
        &s,
        &ctx,
        "delete",
        "knowledge_document",
        &id.to_string(),
        &res,
        json!({}),
    )
    .await?;
    res
}

/// strata_rag's own page cap for a store's documents.
const DOCS_PAGE: i64 = 1000;

/// Whether document `id` lies in a leaf of `hidden` (every page of every hidden leaf is read; `tree` says which
/// hidden stores are leaves, and a store it does not know is read as well).
async fn walled_document(
    s: &AppState,
    ctx: &TenantCtx,
    tree: &walls::Tree,
    hidden: &std::collections::HashSet<String>,
    id: &str,
) -> ApiResult<bool> {
    let want = walls::canonical(id);
    let mut stores: Vec<&String> = hidden.iter().filter(|h| tree.leaf_or_unknown(h)).collect();
    stores.sort();
    for st in stores {
        let mut cursor: Option<String> = None;
        loop {
            let mut path = format!("/api/stores/{st}/documents?limit={DOCS_PAGE}");
            if let Some(c) = &cursor {
                path.push_str("&cursor=");
                path.push_str(&encode_component(c));
            }
            let Reply { status, body } =
                strata::call(s, ctx.tenant, Method::GET, &path, None).await?;
            if status == StatusCode::NOT_FOUND {
                break; // the store is gone: nothing of it to protect
            }
            if !status.is_success() {
                return Err(strata::refusal(status, &body, "documents"));
            }
            let items = body["items"]
                .as_array()
                .ok_or(ApiError::Upstream(strata::NAME))?;
            if items.iter().any(|d| {
                d["id"]
                    .as_str()
                    .is_some_and(|d| walls::canonical(d) == want)
            }) {
                return Ok(true);
            }
            match &body["next"] {
                Value::String(c) if !c.is_empty() => cursor = Some(c.clone()),
                _ => break,
            }
        }
    }
    Ok(false)
}
