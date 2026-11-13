//! RAG eval: the tenant's golden questions and stored eval runs, proxied to strata_rag's `/api/eval/...`.
//!
//! - `GET    /api/knowledge/eval/questions`            member; `{items, next: null}` (a tenant holds at most 200)
//! - `POST   /api/knowledge/eval/questions`            tenant_admin; 201 the stored question
//! - `PATCH  /api/knowledge/eval/questions/{id}`       tenant_admin; a field left out is kept, `null` clears it
//! - `DELETE /api/knowledge/eval/questions/{id}`       tenant_admin; 204 (stored runs keep their snapshot)
//! - `POST   /api/knowledge/eval/questions/import`     tenant_admin; CSV (`text/csv`) or `{questions: [...]}`; all
//!   or nothing; 201 `{added, total}`
//! - `POST   /api/knowledge/eval/runs`                 tenant_admin; starts scoring the tenant's stored questions (all,
//!   or `question_ids`) in `modes` under `root_id` (whole tree when absent); 202 the run summary, `status: running`,
//!   `Location` its detail. strata_rag scores it in the background (a run can take minutes, past any proxy cutoff)
//! - `GET    /api/knowledge/eval/runs?limit=&before=`  member; newest first, without `results` (running runs too);
//!   a run whose root (or, without one, the whole tree) reaches a store walled for a matter the caller is not a
//!   member of is left out, and 404 by id (crate::walls)
//! - `GET    /api/knowledge/eval/runs/{id}`            member; one run: `status` (running / completed / failed),
//!   `error`, `progress {done, total}`, and once completed its per-question `results`. Clients poll it.
//!
//! Writes go out with `X-Actor-Id` (the caller, see [`strata::actor`]) and are audited. strata_rag's eval codes
//! reach the client unchanged: `eval_running` (one eval per tenant at a time), `question_exists`,
//! `eval_not_ready`, `conflict`, `too_many_questions`, `invalid_input`. Only stored tenant questions are run from
//! here: the built-in file eval and inline questions are strata_rag's own tooling.
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::Paged;
use crate::routes::knowledge::audit_write;
use crate::state::AppState;
use crate::strata::{self, NAME};
use crate::tenancy::{Member, TenantAdmin, TenantCtx};
use crate::upstream::{Payload, Reply};
use crate::walls;

/// strata_rag's own page cap for `GET /api/eval/runs`.
const RUNS_MAX_LIMIT: i64 = 100;
const RUNS_DEFAULT_LIMIT: i64 = 20;
/// The search modes strata_rag scores.
pub const MODES: [&str; 4] = ["embed", "laya", "hybrid", "flat"];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/knowledge/eval/questions",
            get(list_questions).post(create_question),
        )
        .route(
            "/api/knowledge/eval/questions/import",
            post(import_questions),
        )
        .route(
            "/api/knowledge/eval/questions/{id}",
            patch(update_question).delete(delete_question),
        )
        .route("/api/knowledge/eval/runs", get(list_runs).post(run_eval))
        .route("/api/knowledge/eval/runs/{id}", get(get_run))
}

/// One strata_rag eval call for `ctx`: success is the reply body (2xx), a 4xx is [`strata::eval_refusal`].
async fn eval_call(
    s: &AppState,
    ctx: &TenantCtx,
    method: Method,
    path: &str,
    payload: Payload,
    what: &str,
) -> ApiResult<Value> {
    let actor = strata::actor(&ctx.caller);
    let Reply { status, body } =
        strata::call_as(s, ctx.tenant, actor, method, path, payload, None).await?;
    if status.is_success() {
        return Ok(body);
    }
    Err(strata::eval_refusal(status, &body, what))
}

/// A 2xx object, else strata_rag answered something we do not understand (503, never a blank success).
fn object(v: Value) -> ApiResult<Value> {
    match v {
        Value::Object(_) => Ok(v),
        _ => Err(ApiError::Upstream(NAME)),
    }
}

/// strata_rag's `{items, next}` page as ours; a missing `items` array or a `next` that is neither a string nor null
/// is 503, never an empty list.
fn page(v: Value) -> ApiResult<Paged<Value>> {
    let Value::Object(mut o) = v else {
        return Err(ApiError::Upstream(NAME));
    };
    let items = match o.remove("items") {
        Some(Value::Array(a)) => a,
        _ => return Err(ApiError::Upstream(NAME)),
    };
    let next = match o.remove("next") {
        None | Some(Value::Null) => None,
        Some(Value::String(c)) => Some(c),
        Some(_) => return Err(ApiError::Upstream(NAME)),
    };
    Ok(Paged { items, next })
}

/// The body must be a JSON object; strata_rag validates the fields (422 `invalid_input`).
fn body_object(b: Value) -> ApiResult<Value> {
    match b {
        Value::Object(_) => Ok(b),
        _ => Err(ApiError::BadRequest(
            "invalid_body",
            "The request body must be a JSON object.".into(),
        )),
    }
}

async fn list_questions(
    Member(ctx): Member,
    State(s): State<AppState>,
) -> ApiResult<Json<Paged<Value>>> {
    let v = eval_call(
        &s,
        &ctx,
        Method::GET,
        "/api/eval/questions",
        Payload::Empty,
        "eval questions",
    )
    .await?;
    Ok(Json(page(v)?))
}

async fn create_question(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiJson(b): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    strata::provisioned_for(&s, ctx.tenant).await?;
    let b = body_object(b)?;
    let res = eval_call(
        &s,
        &ctx,
        Method::POST,
        "/api/eval/questions",
        Payload::Json(b),
        "eval question",
    )
    .await
    .and_then(object);
    let resource = match &res {
        Ok(v) => v["id"].as_str().unwrap_or("").to_string(),
        Err(_) => String::new(),
    };
    audit_write(
        &s,
        &ctx,
        "create",
        "knowledge_eval_question",
        &resource,
        &res,
        json!({}),
    )
    .await?;
    res.map(|v| (StatusCode::CREATED, Json(v)))
}

async fn update_question(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(b): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    strata::provisioned_for(&s, ctx.tenant).await?;
    let b = body_object(b)?;
    let fields: Vec<String> = b
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let res = eval_call(
        &s,
        &ctx,
        Method::PATCH,
        &format!("/api/eval/questions/{id}"),
        Payload::Json(b),
        "eval question",
    )
    .await
    .and_then(object);
    audit_write(
        &s,
        &ctx,
        "update",
        "knowledge_eval_question",
        &id.to_string(),
        &res,
        json!({"fields": fields}),
    )
    .await?;
    res.map(Json)
}

async fn delete_question(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    strata::provisioned_for(&s, ctx.tenant).await?;
    let res = eval_call(
        &s,
        &ctx,
        Method::DELETE,
        &format!("/api/eval/questions/{id}"),
        Payload::Empty,
        "eval question",
    )
    .await
    .map(|_| StatusCode::NO_CONTENT);
    audit_write(
        &s,
        &ctx,
        "delete",
        "knowledge_eval_question",
        &id.to_string(),
        &res,
        json!({}),
    )
    .await?;
    res
}

/// The media type of a request, lowercased, without parameters (`text/csv; charset=utf-8` is `text/csv`).
fn media_type(h: &HeaderMap) -> String {
    h.get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// CSV (`text/csv`, `text/plain`, or no content type) or JSON `{questions: [...]}`, forwarded as sent. strata_rag
/// imports all or nothing and lists every bad line in its 422 `invalid_input`.
async fn import_questions(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    strata::provisioned_for(&s, ctx.tenant).await?;
    let content_type = match media_type(&headers).as_str() {
        "application/json" => "application/json",
        "" | "text/csv" | "text/plain" => "text/csv",
        other => {
            return Err(ApiError::Unsupported(format!(
                "An import is CSV (text/csv) or JSON (application/json), not {other}."
            )));
        }
    };
    if body.is_empty() {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "The import is empty.".into(),
        ));
    }
    let res = eval_call(
        &s,
        &ctx,
        Method::POST,
        "/api/eval/questions/import",
        Payload::Raw {
            content_type: content_type.into(),
            body: body.to_vec(),
        },
        "eval questions",
    )
    .await
    .and_then(|v| match (v["added"].as_u64(), v["total"].as_u64()) {
        (Some(_), Some(_)) => Ok(v),
        _ => Err(ApiError::Upstream(NAME)),
    });
    let details = match &res {
        Ok(v) => json!({"format": content_type, "added": v["added"], "total": v["total"]}),
        Err(_) => json!({"format": content_type}),
    };
    audit_write(
        &s,
        &ctx,
        "import",
        "knowledge_eval_question",
        "",
        &res,
        details,
    )
    .await?;
    res.map(|v| (StatusCode::CREATED, Json(v)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    /// The store whose subtree is evaluated; absent = the tenant's whole tree.
    #[serde(default, alias = "store_id")]
    root_id: Option<Uuid>,
    #[serde(default)]
    modes: Option<Vec<String>>,
    #[serde(default)]
    beam: Option<u32>,
    #[serde(default)]
    min_score: Option<f32>,
    /// Only these stored questions (in this order); absent = all of them.
    #[serde(default)]
    question_ids: Option<Vec<Uuid>>,
}

/// strata_rag's tenant eval body: always `source: "tenant"`, only the options given.
fn run_body(i: &RunInput) -> ApiResult<Value> {
    let mut b = Map::new();
    b.insert("source".into(), json!("tenant"));
    if let Some(r) = i.root_id {
        b.insert("root_id".into(), json!(r));
    }
    if let Some(m) = &i.modes {
        if m.is_empty() {
            return Err(ApiError::BadRequest(
                "invalid_modes",
                "modes must name at least one of embed, laya, hybrid, flat.".into(),
            ));
        }
        if let Some(bad) = m.iter().find(|x| !MODES.contains(&x.as_str())) {
            return Err(ApiError::BadRequest(
                "invalid_modes",
                format!("{bad} is not a mode; use embed, laya, hybrid or flat."),
            ));
        }
        b.insert("modes".into(), json!(m));
    }
    if let Some(beam) = i.beam {
        if beam == 0 {
            return Err(ApiError::BadRequest(
                "invalid_beam",
                "beam must be at least 1.".into(),
            ));
        }
        b.insert("beam".into(), json!(beam));
    }
    if let Some(ms) = i.min_score {
        if !ms.is_finite() {
            return Err(ApiError::BadRequest(
                "invalid_min_score",
                "min_score must be a finite number.".into(),
            ));
        }
        b.insert("min_score".into(), json!(ms));
    }
    if let Some(ids) = &i.question_ids {
        if ids.is_empty() {
            return Err(ApiError::BadRequest(
                "invalid_question_ids",
                "question_ids must name at least one question; leave it out to run all.".into(),
            ));
        }
        b.insert("question_ids".into(), json!(ids));
    }
    Ok(Value::Object(b))
}

/// strata_rag's 202 for a started tenant run: a summary with an id and a `status`. Anything else (the built-in
/// report, a synchronous older strata_rag) is not a run we can poll: 503.
fn started(status: StatusCode, v: Value) -> ApiResult<Value> {
    if status == StatusCode::ACCEPTED
        && v["id"].as_str().is_some()
        && v["status"].as_str().is_some()
    {
        Ok(v)
    } else {
        Err(ApiError::Upstream(NAME))
    }
}

async fn run_eval(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiJson(i): ApiJson<RunInput>,
) -> ApiResult<(StatusCode, HeaderMap, Json<Value>)> {
    strata::provisioned_for(&s, ctx.tenant).await?;
    let body = run_body(&i)?;
    // An eval searches the subtree of `root_id` (the whole tree when absent): never one holding a store walled for a
    // matter the admin is not a member of (crate::walls). Naming such a store is 404, reaching one 409 `store_walled`.
    let (hidden, tree) = walls::hidden(&s, &ctx).await?;
    if !hidden.is_empty() {
        let roots = match i.root_id {
            Some(r) if hidden.contains(&r.to_string()) => {
                return Err(ApiError::NotFound(format!("store {r}")));
            }
            Some(r) => vec![r.to_string()],
            None => tree.roots(),
        };
        if !tree.closure(&roots).is_disjoint(&hidden) {
            return Err(ApiError::Conflict(
                "store_walled",
                "That tree holds stores of legal matters you are not a member of; evaluate a store outside them."
                    .into(),
            ));
        }
    }
    // strata_rag answers once the run is stored and its questions checked; the scoring runs on without us, so
    // the default timeout holds.
    let res = strata::call_as(
        &s,
        ctx.tenant,
        strata::actor(&ctx.caller),
        Method::POST,
        "/api/eval",
        Payload::Json(body.clone()),
        None,
    )
    .await
    .and_then(|Reply { status, body }| {
        if status.is_success() {
            started(status, body)
        } else {
            Err(strata::eval_refusal(
                status,
                &body,
                "store or eval question",
            ))
        }
    });
    let resource = match &res {
        Ok(v) => v["id"].as_str().unwrap_or("").to_string(),
        Err(_) => String::new(),
    };
    let mut details = json!({
        "root_id": body.get("root_id"),
        "modes": body.get("modes"),
        "question_ids": i.question_ids.as_ref().map(Vec::len),
    });
    if let Ok(v) = &res {
        details["status"] = v["status"].clone();
    }
    audit_write(
        &s,
        &ctx,
        "run",
        "knowledge_eval_run",
        &resource,
        &res,
        details,
    )
    .await?;
    let v = res?;
    let mut h = HeaderMap::new();
    if let Ok(loc) = format!("/api/knowledge/eval/runs/{resource}").parse() {
        h.insert(header::LOCATION, loc);
    }
    Ok((StatusCode::ACCEPTED, h, Json(v)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunsQuery {
    limit: Option<i64>,
    /// strata_rag's opaque `next` cursor.
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

async fn list_runs(
    Member(ctx): Member,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<RunsQuery>,
) -> ApiResult<Json<Paged<Value>>> {
    let limit = q
        .limit
        .unwrap_or(RUNS_DEFAULT_LIMIT)
        .clamp(1, RUNS_MAX_LIMIT);
    let mut path = format!("/api/eval/runs?limit={limit}");
    if let Some(c) = q.before.as_deref().filter(|c| !c.is_empty()) {
        path.push_str("&cursor=");
        path.push_str(&encode_component(c));
    }
    let v = eval_call(&s, &ctx, Method::GET, &path, Payload::Empty, "eval runs").await?;
    let mut p = page(v)?;
    let (hidden, tree) = walls::hidden(&s, &ctx).await?;
    p.items
        .retain(|r| !walls::reaches(&tree, &hidden, r["root_id"].as_str()));
    Ok(Json(p))
}

async fn get_run(
    Member(ctx): Member,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let v = eval_call(
        &s,
        &ctx,
        Method::GET,
        &format!("/api/eval/runs/{id}"),
        Payload::Empty,
        &format!("eval run {id}"),
    )
    .await?;
    let v = object(v)?;
    // A run that searched a store walled for a matter the caller is not a member of (its root, or the whole tree)
    // does not exist for them: its results come from that store (crate::walls).
    let (hidden, tree) = walls::hidden(&s, &ctx).await?;
    if walls::reaches(&tree, &hidden, v["root_id"].as_str()) {
        return Err(ApiError::NotFound(format!("eval run {id}")));
    }
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(v: Value) -> RunInput {
        serde_json::from_value(v).expect("valid run input")
    }

    #[test]
    fn a_run_is_always_a_tenant_eval_with_only_the_options_given() {
        let b = run_body(&input(json!({}))).unwrap();
        assert_eq!(b, json!({"source": "tenant"}));
        let root = "7f1c9a2e-0000-4000-8000-0000000000a0";
        let b = run_body(&input(
            json!({"store_id": root, "modes": ["embed", "flat"], "beam": 3, "min_score": 0.25}),
        ))
        .unwrap();
        assert_eq!(
            b,
            json!({"source": "tenant", "root_id": root, "modes": ["embed", "flat"], "beam": 3, "min_score": 0.25})
        );
    }

    #[test]
    fn bad_run_options_are_refused_before_strata_rag() {
        for (v, code) in [
            (json!({"modes": []}), "invalid_modes"),
            (json!({"modes": ["embed", "bm25"]}), "invalid_modes"),
            (json!({"beam": 0}), "invalid_beam"),
            (json!({"question_ids": []}), "invalid_question_ids"),
        ] {
            let e = run_body(&input(v.clone())).expect_err("refused");
            assert_eq!(e.code(), code, "{v}");
        }
        // the built-in eval and inline questions are not reachable from here
        for v in [json!({"source": "builtin"}), json!({"questions": []})] {
            assert!(serde_json::from_value::<RunInput>(v).is_err());
        }
    }

    #[test]
    fn only_a_202_run_with_a_status_is_started() {
        let run = json!({"id": "r1", "status": "running"});
        assert!(started(StatusCode::ACCEPTED, run.clone()).is_ok());
        // a synchronous answer (the built-in report, an older strata_rag) cannot be polled
        assert!(started(StatusCode::OK, run).is_err());
        assert!(started(StatusCode::ACCEPTED, json!({"id": "r1"})).is_err());
        assert!(
            started(
                StatusCode::ACCEPTED,
                json!({"reports": [], "verdict": "PASS"})
            )
            .is_err()
        );
    }

    #[test]
    fn a_page_without_items_is_unavailable_not_empty() {
        assert!(page(json!({"next": null})).is_err());
        assert!(page(json!({"items": [], "next": 7})).is_err());
        let p = page(json!({"items": [{"id": "a"}], "next": "c1"})).unwrap();
        assert_eq!((p.items.len(), p.next.as_deref()), (1, Some("c1")));
    }

    #[test]
    fn media_types_ignore_parameters_and_case() {
        let mut h = HeaderMap::new();
        assert_eq!(media_type(&h), "");
        h.insert(
            header::CONTENT_TYPE,
            "Text/CSV; charset=utf-8".parse().unwrap(),
        );
        assert_eq!(media_type(&h), "text/csv");
    }
}
