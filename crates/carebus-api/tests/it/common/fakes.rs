//! In-process fakes with the real upstream shapes: strata_rag (origin/main), strata_ingest
//! (nonagenticai/strata_ingest PR #3, feat/api-datatable-source-kinds) and a customer API for test-connection. One server answers strata_rag and
//! strata_ingest, each behind its own bearer. The eval routes (`/api/eval/...`) follow strata_rag PR #21
//! (feat/eval-tenant-questions): tenant questions, CSV/JSON import, stored runs scored asynchronously (202
//! `running`, then `completed` / `failed` on the next poll), as strata_rag #21 @e202c88: no `contaminated` /
//! `foreign_stores` on tenant runs, `progress` counts question x mode, pre-run refusals come from the POST.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{FromRequest, Multipart, Path, Query, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use carebus_api::config::Config;
use serde_json::{Value, json};

use super::TestEnv;

pub const RAG_TOKEN: &str = "rag-test-token";
pub const INGEST_TOKEN: &str = "ingest-test-token";
pub const ROUTER: &str = "7f1c9a2e-0000-4000-8000-0000000000a0";
pub const LEAF: &str = "7f1c9a2e-0000-4000-8000-0000000000a1";
pub const OTHER_LEAF: &str = "7f1c9a2e-0000-4000-8000-0000000000a2";
pub const NEW_STORE: &str = "7f1c9a2e-0000-4000-8000-0000000000a9";
/// The customer API's key; also used as a source's credential in tests.
pub const API_KEY: &str = "k-123-SAMPLEKEY";

#[derive(Default)]
pub struct Fake {
    pub rag_down: bool,
    pub ingest_down: bool,
    /// An api/datatable source fetch/parse failure `(code, message)`: as strata_ingest PR #3, the POST is still a
    /// 202 and the job is created `failed` with that error and no steps (a retry of it is 409).
    pub source_failure: Option<(String, String)>,
    /// strata_ingest's 400 for a malformed structured-source request.
    pub source_bad_request: bool,
    /// Models a strata_ingest that knows no structured kind: every `source` is a 422 `unsupported_source_kind`.
    pub unsupported_kinds: bool,
    /// Puts the received credential into the source error / 400 message, to prove carebus-api scrubs it.
    pub echo_secret_in_errors: bool,
    /// With `echo_secret_in_errors`: echo it base64- and percent-encoded too (as a source that reflects an
    /// `Authorization` header or a query string would).
    pub echo_secret_encoded: bool,
    /// Holds every job lookup this long, so concurrent readers all see the same non-final state first.
    pub job_delay_ms: u64,
    pub job_requests: Vec<Value>,
    pub jobs: BTreeMap<String, Value>,
    pub next_job: u32,
    pub list_queries: Vec<String>,
    /// Every `POST /api/ingest/jobs/{id}/retry` the fake received.
    pub retried: Vec<String>,
    pub deleted_docs: Vec<String>,
    pub failing_docs: BTreeSet<String>,
    pub missing_docs: BTreeSet<String>,
    pub created_stores: Vec<Value>,
    pub deleted_stores: Vec<(String, bool)>,
    /// Every job `DELETE /api/ingest/jobs/{id}` removed.
    pub deleted_jobs: Vec<String>,
    /// storage_service refuses a blob delete: `DELETE` of a job is strata_ingest's 502 and leaves it `deleting`;
    /// the blobs deleted before the refusal are gone ([`Fake::blobs_lost`]), so the job can never run again.
    pub blob_delete_fails: bool,
    /// Jobs a failed `DELETE` already removed some blobs of (strata_ingest PR #11: a 502 is a PARTIAL delete).
    pub blobs_lost: BTreeSet<String>,
    /// `DELETE` of a job answers 202 `{job_id, status: "deleting"}` and leaves the job `deleting` for the background
    /// sweeper ([`sweep_deletes`]) to finish.
    pub delete_async: bool,
    /// A `deleting` job's status before its first `DELETE` (what the finished `DELETE` reports).
    pub deleting_was: BTreeMap<String, Value>,
    /// Jobs with a step running right now ([`claim`]); a running job without one is between steps.
    pub steps_in_flight: BTreeSet<String>,
    /// strata_rag eval: the tenant's questions (oldest first) and stored runs (oldest first, with `results`).
    pub eval_questions: Vec<Value>,
    pub eval_runs: Vec<Value>,
    pub next_eval: u32,
    /// How many questions a tenant may hold (strata_rag: 200).
    pub eval_cap: Option<usize>,
    /// The finished form of each `running` run, keyed by id: the next `GET /api/eval/runs/{id}` (the client's poll)
    /// stores and answers it, as strata_rag's background task would have by then.
    pub eval_pending: BTreeMap<String, Value>,
    /// The next run polled fails with this error instead of completing.
    pub eval_fail: Option<String>,
    /// `POST /api/eval` is strata_rag's generic 409 `conflict` (e.g. a serialization failure), not `eval_running`.
    pub eval_conflict: bool,
    /// Every eval request the fake received, in order.
    pub eval_seen: Vec<EvalSeen>,
    /// Every `POST /api/query` body strata_rag received, in order.
    pub queries: Vec<Value>,
    /// The document ids `GET /api/stores/{id}/documents` lists for a store (default: `doc_id(1)` in every store).
    pub store_docs: BTreeMap<String, Vec<String>>,
}

/// One eval request as strata_rag received it.
#[derive(Clone, Debug)]
pub struct EvalSeen {
    pub method: String,
    pub path_and_query: String,
    /// `X-Actor-Id`, when sent.
    pub actor: Option<String>,
    pub content_type: Option<String>,
    pub body: String,
}
pub type Shared = Arc<Mutex<Fake>>;

pub fn job_id(n: u32) -> String {
    format!("7f1c9a2e-0000-4000-8000-{:012x}", 0xb00 + n)
}
pub fn doc_id(n: u32) -> String {
    format!("7f1c9a2e-0000-4000-8000-{:012x}", 0xd00 + n)
}

fn bearer(h: &HeaderMap, want: &str) -> bool {
    h.get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {want}"))
}
/// strata_rag / strata_ingest: our bearer AND the tenant we act for (`X-Tenant-Id`, bare uuid).
fn strata_auth(h: &HeaderMap, want: &str) -> bool {
    bearer(h, want)
        && h.get("x-tenant-id").and_then(|v| v.to_str().ok()) == Some(super::BOOT_TENANT)
}
fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}
fn unauthorized() -> Response {
    err(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "missing or wrong bearer",
    )
}
fn down() -> Response {
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        "upstream_unavailable",
        "down for this test",
    )
}

fn store(id: &str, parent: Option<&str>, kind: &str, name: &str, children: &[&str]) -> Value {
    json!({"id": id, "parent_id": parent, "kind": kind, "name": name, "description": format!("{name} knowledge"),
           "token_count": 1200, "chunk_count": 12, "children": children})
}
pub fn stores() -> Vec<Value> {
    vec![
        store(ROUTER, None, "router", "care", &[LEAF, OTHER_LEAF]),
        store(LEAF, Some(ROUTER), "leaf", "help-centre", &[]),
        store(OTHER_LEAF, Some(ROUTER), "leaf", "customers", &[]),
    ]
}

// ---- strata_rag ---------------------------------------------------------------------------------------------

async fn list_stores(State(f): State<Shared>, h: HeaderMap) -> Response {
    if !strata_auth(&h, RAG_TOKEN) {
        return unauthorized();
    }
    if f.lock().unwrap().rag_down {
        return down();
    }
    Json(json!({"stores": stores()})).into_response()
}

/// `POST /api/query`: records the body; one hit per searched store (`root_id` / `root_ids` / `leaf_ids`).
async fn query(State(f): State<Shared>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if !strata_auth(&h, RAG_TOKEN) {
        return unauthorized();
    }
    let mut g = f.lock().unwrap();
    if g.rag_down {
        return down();
    }
    g.queries.push(b.clone());
    let mut named: Vec<Value> = vec![];
    for k in ["root_ids", "leaf_ids"] {
        named.extend(b[k].as_array().cloned().unwrap_or_default());
    }
    if !b["root_id"].is_null() {
        named.push(b["root_id"].clone());
    }
    let results: Vec<Value> = named
        .iter()
        .map(|s| json!({"chunk_id": format!("c-{}", s.as_str().unwrap_or("")), "document_id": "d", "store_path": [s],
                        "ord": 0, "text": "t", "page_start": null, "page_end": null, "source": "s", "title": "t"}))
        .collect();
    Json(json!({"results": results, "routing": {}, "timings_ms": {}})).into_response()
}

/// `GET /api/stores/{id}/documents`: one document in any store of the tree.
async fn store_documents(
    State(f): State<Shared>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !strata_auth(&h, RAG_TOKEN) {
        return unauthorized();
    }
    let g = f.lock().unwrap();
    if g.rag_down {
        return down();
    }
    if !stores().iter().any(|s| s["id"] == id.as_str()) {
        return err(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("store {id} not found"),
        );
    }
    let ids = g
        .store_docs
        .get(&id)
        .cloned()
        .unwrap_or_else(|| vec![doc_id(1)]);
    let items: Vec<Value> = ids
        .iter()
        .map(|d| {
            json!({"id": d, "store_id": id, "source": "brief.pdf", "title": "Pozew",
                   "content_hash": "h", "token_count": 1, "chunk_count": 1,
                   "created_at": "2026-10-15T10:00:00Z"})
        })
        .collect();
    Json(json!({"items": items, "next": null})).into_response()
}

async fn get_store(State(f): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !strata_auth(&h, RAG_TOKEN) {
        return unauthorized();
    }
    if f.lock().unwrap().rag_down {
        return down();
    }
    match stores().into_iter().find(|s| s["id"] == id.as_str()) {
        Some(s) => Json(s).into_response(),
        None => err(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("store {id} not found"),
        ),
    }
}

async fn create_store(State(f): State<Shared>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if !strata_auth(&h, RAG_TOKEN) {
        return unauthorized();
    }
    let mut g = f.lock().unwrap();
    if g.rag_down {
        return down();
    }
    if b["name"] == "taken" {
        return err(
            StatusCode::CONFLICT,
            "duplicate_name",
            "a sibling store is already named taken",
        );
    }
    g.created_stores.push(b.clone());
    (
        StatusCode::CREATED,
        Json(json!({"id": NEW_STORE, "parent_id": b["parent_id"], "kind": b["kind"], "name": b["name"],
                    "description": b["description"], "token_count": 0, "chunk_count": 0, "children": []})),
    )
        .into_response()
}

async fn delete_store(
    State(f): State<Shared>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    if !strata_auth(&h, RAG_TOKEN) {
        return unauthorized();
    }
    let mut g = f.lock().unwrap();
    if g.rag_down {
        return down();
    }
    let recursive = q.get("recursive").is_some_and(|v| v == "true");
    if !stores().iter().any(|s| s["id"] == id.as_str()) {
        return err(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("store {id} not found"),
        );
    }
    if id == ROUTER && !recursive {
        return err(
            StatusCode::CONFLICT,
            "has_children",
            "the store has children; pass recursive=true",
        );
    }
    g.deleted_stores.push((id, recursive));
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_document(
    State(f): State<Shared>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !strata_auth(&h, RAG_TOKEN) {
        return unauthorized();
    }
    let mut g = f.lock().unwrap();
    if g.rag_down {
        return down();
    }
    if g.failing_docs.contains(&id) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "disk full");
    }
    if g.missing_docs.contains(&id) || g.deleted_docs.contains(&id) {
        return err(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("document {id} not found"),
        );
    }
    g.deleted_docs.push(id);
    StatusCode::NO_CONTENT.into_response()
}

// ---- strata_ingest ------------------------------------------------------------------------------------------

fn new_job(f: &mut Fake, store_id: &str, source: Value, options: Value) -> String {
    new_job_with(f, store_id, source, options, &Value::Null)
}

/// `meta` is `{"source_id"}` when the request carried one (strata_ingest `initial_meta`).
fn new_job_with(
    f: &mut Fake,
    store_id: &str,
    source: Value,
    options: Value,
    source_id: &Value,
) -> String {
    f.next_job += 1;
    let id = job_id(f.next_job);
    let meta = match source_id.as_str() {
        Some(s) => json!({"source_id": s}),
        None => json!({}),
    };
    f.jobs.insert(
        id.clone(),
        json!({"id": id, "tenant": "7f1c9a2e-0000-4000-8000-0000000000f0", "store_id": store_id, "source": source,
               "options": options, "meta": meta, "status": "queued", "error": null, "document_ids": [],
               "created_at": "2026-10-16T09:00:00Z", "updated_at": "2026-10-16T09:00:00Z",
               "steps": [{"kind": "fetch", "key": "fetch", "status": "pending", "attempt": 1, "output": null,
                          "provider": "webscraper", "model": null, "usage": null, "cost_micros": null,
                          "error": null, "started_at": null, "finished_at": null}]}),
    );
    id
}

async fn create_jobs(State(f): State<Shared>, req: Request) -> Response {
    if !strata_auth(req.headers(), INGEST_TOKEN) {
        return unauthorized();
    }
    if f.lock().unwrap().ingest_down {
        return down();
    }
    let is_multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/form-data"));
    if is_multipart {
        let mut mp = match Multipart::from_request(req, &()).await {
            Ok(m) => m,
            Err(e) => return err(StatusCode::BAD_REQUEST, "bad_request", &e.body_text()),
        };
        let (mut store, mut options, mut files) = (String::new(), Value::Null, Vec::new());
        let mut source_id = Value::Null;
        while let Ok(Some(field)) = mp.next_field().await {
            let name = field.name().unwrap_or_default().to_string();
            let file_name = field.file_name().map(str::to_string);
            let content_type = field.content_type().map(str::to_string);
            let bytes = field.bytes().await.unwrap_or_default();
            match name.as_str() {
                "store_id" => store = String::from_utf8_lossy(&bytes).into_owned(),
                "options" => options = serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                "source_id" if files.is_empty() => {
                    source_id = json!(String::from_utf8_lossy(&bytes));
                }
                "file" => files.push(json!({"name": file_name, "content_type": content_type,
                                            "size": bytes.len(), "text": String::from_utf8_lossy(&bytes)})),
                other => {
                    return err(StatusCode::BAD_REQUEST, "bad_request", &format!("unexpected multipart field `{other}`"));
                }
            }
        }
        let mut g = f.lock().unwrap();
        g.job_requests
            .push(json!({"multipart": {"store_id": store, "source_id": source_id, "options": options, "files": files}}));
        let jobs: Vec<Value> = files
            .iter()
            .map(|file| {
                let source = json!({"kind": "upload", "filename": file["name"]});
                let id = new_job_with(&mut g, &store, source, options.clone(), &source_id);
                json!({"job_id": id, "status": "queued"})
            })
            .collect();
        return (StatusCode::ACCEPTED, Json(json!({"jobs": jobs}))).into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 1 << 20).await else {
        return err(StatusCode::BAD_REQUEST, "bad_request", "unreadable body");
    };
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let mut g = f.lock().unwrap();
    g.job_requests.push(body.clone());
    let store = body["store_id"].as_str().unwrap_or_default().to_string();
    if body.get("source").is_some() {
        return source_job(&mut g, &store, &body);
    }
    if store == ROUTER {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_target",
            "the target store is a router",
        );
    }
    let urls: Vec<String> = match (&body["url"], &body["urls"]) {
        (Value::String(u), _) => vec![u.clone()],
        (_, Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    if urls.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "exactly one of `url` or `urls` is required",
        );
    }
    let options = if body["options"].is_null() {
        json!({"ocr": "auto", "captions": true, "language": null})
    } else {
        body["options"].clone()
    };
    let source_id = body["source_id"].clone();
    let jobs: Vec<Value> = urls
        .iter()
        .map(|u| {
            let source = json!({"kind": "url", "url": u});
            let id = new_job_with(&mut g, &store, source, options.clone(), &source_id);
            json!({"job_id": id, "status": "queued"})
        })
        .collect();
    (StatusCode::ACCEPTED, Json(json!({"jobs": jobs}))).into_response()
}

/// A structured source (strata_ingest `api::sources::source_job`): unknown kind 422, malformed 400, and a
/// fetch/parse failure a 202 whose job is `failed`. The fake ECHOES the request's `source` (credentials included)
/// into the job, so only carebus-api's redaction keeps them out of its responses.
fn source_job(g: &mut Fake, store: &str, body: &Value) -> Response {
    // A crawl or sitemap is ONE parent job, answered as `{job_id}`; tests start its page jobs with `add_pages`
    // and end its discovery with `settle_parent`.
    if !g.unsupported_kinds && matches!(body["source"]["kind"].as_str(), Some("crawl" | "sitemap"))
    {
        let id = new_job_with(
            g,
            store,
            body["source"].clone(),
            body["options"].clone(),
            &body["source_id"],
        );
        return (
            StatusCode::ACCEPTED,
            Json(json!({"job_id": id, "status": "queued"})),
        )
            .into_response();
    }
    let secret = body["source"]["credentials"]["secret"].as_str();
    let with_secret = |msg: &str| match secret {
        Some(sec) if g.echo_secret_in_errors && g.echo_secret_encoded => {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(sec);
            let pct: String = sec
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            format!("{msg} (got Basic {b64}, url ?key={pct})")
        }
        Some(sec) if g.echo_secret_in_errors => format!("{msg} (got {sec})"),
        _ => msg.to_string(),
    };
    let kind = body["source"]["kind"].as_str().unwrap_or_default();
    if g.unsupported_kinds || !matches!(kind, "api" | "datatable") {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_source_kind",
            &format!("source kind `{kind}` is not supported; supported: api, datatable"),
        );
    }
    if g.source_bad_request {
        let msg =
            with_secret("invalid body: `source.config.auth.type` must be none, bearer or header");
        return err(StatusCode::BAD_REQUEST, "bad_request", &msg);
    }
    let failure = g
        .source_failure
        .clone()
        .map(|(code, msg)| (code, with_secret(&msg)));
    let id = new_job_with(
        g,
        store,
        body["source"].clone(),
        body["options"].clone(),
        &body["source_id"],
    );
    let status = match failure {
        Some((code, message)) => {
            let j = g.jobs.get_mut(&id).expect("just created");
            j["status"] = json!("failed");
            j["error"] =
                json!({"class": "invalid", "code": code, "message": message, "provider": "source"});
            j["steps"] = json!([]);
            "failed"
        }
        None => "queued",
    };
    (
        StatusCode::ACCEPTED,
        Json(json!({"jobs": [{"job_id": id, "status": status}]})),
    )
        .into_response()
}

async fn list_jobs(
    State(f): State<Shared>,
    h: HeaderMap,
    RawQuery(raw): RawQuery,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    if !strata_auth(&h, INGEST_TOKEN) {
        return unauthorized();
    }
    let mut g = f.lock().unwrap();
    if g.ingest_down {
        return down();
    }
    g.list_queries.push(raw.unwrap_or_default());
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50);
    let all: Vec<Value> = g
        .jobs
        .values()
        .rev()
        .filter(|j| {
            q.get("store_id")
                .is_none_or(|s| j["store_id"] == s.as_str())
        })
        .filter(|j| q.get("status").is_none_or(|s| j["status"] == s.as_str()))
        .cloned()
        .collect();
    let start = match q.get("before") {
        None => 0,
        Some(b) if g.jobs.contains_key(b) => all
            .iter()
            .position(|j| j["id"] == b.as_str())
            .map_or(all.len(), |p| p + 1),
        Some(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "bad request: `before` is not one of your jobs",
            );
        }
    };
    let jobs: Vec<Value> = all.iter().skip(start).take(limit).cloned().collect();
    let next = (all.len() > start + limit)
        .then(|| jobs.last().map(|j| j["id"].clone()))
        .flatten();
    Json(json!({"jobs": jobs, "next": next})).into_response()
}

async fn get_job(State(f): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !strata_auth(&h, INGEST_TOKEN) {
        return unauthorized();
    }
    let (delay, reply) = {
        let g = f.lock().unwrap();
        if g.ingest_down {
            return down();
        }
        let reply = match g.jobs.get(&id) {
            Some(j) => Json(j.clone()).into_response(),
            None => err(StatusCode::NOT_FOUND, "not_found", "job not found"),
        };
        (g.job_delay_ms, reply)
    };
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    reply
}

async fn retry_job(State(f): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !strata_auth(&h, INGEST_TOKEN) {
        return unauthorized();
    }
    let mut g = f.lock().unwrap();
    if g.ingest_down {
        return down();
    }
    g.retried.push(id.clone());
    let Some(j) = g.jobs.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, "not_found", "job not found");
    };
    if j["status"] == "failed" && j["steps"].as_array().is_some_and(Vec::is_empty) {
        // a structured source's failed job: its credentials were never kept, so nothing can run again
        return err(
            StatusCode::CONFLICT,
            "conflict",
            "conflict: job has no failed step to retry",
        );
    }
    if j["status"] != "failed" {
        return err(
            StatusCode::CONFLICT,
            "conflict",
            "only a failed job can be retried",
        );
    }
    j["status"] = json!("queued");
    j["error"] = Value::Null;
    Json(j.clone()).into_response()
}

/// strata_ingest's `cancelled_or_cancelling`: `cancelling` while a step is in flight, else `cancelled` at once.
fn cancel_one(g: &mut Fake, id: &str) {
    let in_flight = g.steps_in_flight.contains(id);
    if let Some(j) = g.jobs.get_mut(id) {
        j["status"] = json!(if in_flight { "cancelling" } else { "cancelled" });
    }
}

/// Cancels `parent`'s queued / running page jobs; how many.
fn cancel_children(g: &mut Fake, parent: &str) -> usize {
    let open: Vec<String> = g.jobs[parent]["child_job_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect::<Vec<String>>()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|c| {
            g.jobs
                .get(c)
                .is_some_and(|j| matches!(j["status"].as_str(), Some("queued" | "running")))
        })
        .collect();
    for c in &open {
        cancel_one(g, c);
    }
    open.len()
}

/// `POST /api/ingest/jobs/{id}/cancel` (strata_ingest PR #11, `db::jobs::cancel`): a queued job, or a running one
/// between steps, is `cancelled` at once; a running one with a step in flight is `cancelling` ([`settle_cancel`] ends
/// it). A crawl/sitemap parent's open page jobs are cancelled with it. cancelling / cancelled → 200 unchanged;
/// `deleting` → 409. An ended job → 409, except a parent that has ended (its Discover step is done) while page jobs
/// still run: those are cancelled and the parent is answered as it is.
async fn cancel_job(State(f): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !strata_auth(&h, INGEST_TOKEN) {
        return unauthorized();
    }
    let mut g = f.lock().unwrap();
    if g.ingest_down {
        return down();
    }
    let Some(status) = g
        .jobs
        .get(&id)
        .map(|j| j["status"].as_str().unwrap_or("").to_string())
    else {
        return err(StatusCode::NOT_FOUND, "not_found", "job not found");
    };
    match status.as_str() {
        "queued" | "running" => {
            cancel_one(&mut g, &id);
            cancel_children(&mut g, &id);
        }
        "cancelling" | "cancelled" => {}
        "deleting" => {
            return err(
                StatusCode::CONFLICT,
                "conflict",
                "job is being deleted; DELETE it again to finish",
            );
        }
        _ => {
            if cancel_children(&mut g, &id) == 0 {
                return err(
                    StatusCode::CONFLICT,
                    "conflict",
                    "the job has already ended",
                );
            }
        }
    }
    Json(g.jobs[&id].clone()).into_response()
}

/// The step a `cancelling` job held has ended: the job is `cancelled`.
pub fn settle_cancel(f: &Shared, job: &str) {
    let mut g = f.lock().unwrap();
    g.steps_in_flight.remove(job);
    let j = g.jobs.get_mut(job).expect("known job");
    assert_eq!(j["status"], "cancelling");
    j["status"] = json!("cancelled");
}

/// Marks a job `running` with a step in flight (the worker claimed its next step).
pub fn claim(f: &Shared, job: &str) {
    let mut g = f.lock().unwrap();
    g.jobs.get_mut(job).expect("known job")["status"] = json!("running");
    g.steps_in_flight.insert(job.to_string());
}

/// Marks a job `running` between steps: its last step ended, the next is not claimed yet.
pub fn between_steps(f: &Shared, job: &str) {
    let mut g = f.lock().unwrap();
    g.jobs.get_mut(job).expect("known job")["status"] = json!("running");
    g.steps_in_flight.remove(job);
}

/// `DELETE /api/ingest/jobs/{id}` (strata_ingest PR #11 @db716d5, `db::jobs::delete`): a crawl/sitemap page job is
/// 409 `child_of_parent_job` with `parent_job_id` whatever its status (it goes only with its parent); otherwise only
/// an ended job (and its children, which must all have ended); its documents stay in strata_rag. 409 otherwise. 502
/// when a blob cannot be deleted: the job (and its children) stay `deleting` with some blobs already gone, and a
/// `DELETE` again finishes it. With `delete_async`, 202 `{job_id, status: "deleting"}`: the sweeper finishes it.
async fn delete_job(State(f): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !strata_auth(&h, INGEST_TOKEN) {
        return unauthorized();
    }
    let mut g = f.lock().unwrap();
    if g.ingest_down {
        return down();
    }
    let Some(j) = g.jobs.get(&id).cloned() else {
        return err(StatusCode::NOT_FOUND, "not_found", "job not found");
    };
    // checked first, as strata_ingest does (before the status): deleting a page job alone would drop it from its
    // parent's `child_job_ids`
    if let Some(parent) = j["source"]["parent_job_id"].as_str() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": {"code": "child_of_parent_job",
                "message": format!("job {id} is a page job of {parent}; delete the parent job, which removes it"),
                "parent_job_id": parent}})),
        )
            .into_response();
    }
    let ended = |v: &Value| {
        matches!(
            v["status"].as_str(),
            Some("completed" | "failed" | "rejected" | "cancelled")
        )
    };
    let children: Vec<String> = j["child_job_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let resuming = j["status"] == "deleting";
    if !resuming
        && (!ended(&j)
            || children
                .iter()
                .any(|c| g.jobs.get(c).is_some_and(|c| !ended(c))))
    {
        return err(
            StatusCode::CONFLICT,
            "conflict",
            "the job has not ended: cancel it first, then wait for cancelled",
        );
    }
    // phase 1: the job and its children are `deleting` (never claimed, cancel/retry 409)
    if !resuming {
        g.deleting_was.insert(id.clone(), j["status"].clone());
        for k in children.iter().chain(std::iter::once(&id)) {
            if let Some(x) = g.jobs.get_mut(k) {
                x["status"] = json!("deleting");
            }
        }
    }
    if g.blob_delete_fails {
        g.blobs_lost.insert(id.clone());
        return err(
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            "storage_service could not delete a blob",
        );
    }
    if g.delete_async {
        return (
            StatusCode::ACCEPTED,
            Json(json!({"job_id": id, "status": "deleting"})),
        )
            .into_response();
    }
    let mut gone = 1;
    for c in &children {
        if g.jobs.remove(c).is_some() {
            gone += 1;
        }
    }
    g.jobs.remove(&id);
    g.deleted_jobs.push(id.clone());
    let was = g.deleting_was.remove(&id).unwrap_or(Value::Null);
    Json(
        json!({"job_id": id, "status": was, "deleted_jobs": gone, "deleted_objects": 3,
                "document_ids": j["document_ids"], "documents": "retained"}),
    )
    .into_response()
}

/// strata_ingest's sweeper: every `deleting` job (and its page jobs) is gone, as a background delete ends.
pub fn sweep_deletes(f: &Shared) {
    let mut g = f.lock().unwrap();
    let ids: Vec<String> = g
        .jobs
        .iter()
        .filter(|(_, j)| j["status"] == "deleting")
        .map(|(k, _)| k.clone())
        .collect();
    for id in ids {
        g.jobs.remove(&id);
        g.deleted_jobs.push(id);
    }
}

/// Completes a job with these documents (Deliver ran), with a priced vision step, as strata_ingest's worker would.
pub fn complete(f: &Shared, job: &str, docs: &[String]) {
    let mut g = f.lock().unwrap();
    let j = g.jobs.get_mut(job).expect("known job");
    j["status"] = json!("completed");
    j["document_ids"] = json!(docs);
    j["steps"] = json!([
        {"kind": "fetch", "key": "fetch", "status": "completed", "attempt": 1, "output": null,
         "provider": "webscraper", "model": null, "usage": {"bytes": 18000}, "cost_micros": 0, "error": null,
         "started_at": "2026-10-16T09:00:01Z", "finished_at": "2026-10-16T09:00:02Z"},
        {"kind": "vision", "key": "figure-1", "status": "completed", "attempt": 1, "output": null,
         "provider": "vllm", "model": "local-llm", "usage": {"input_tokens": 812, "output_tokens": 64},
         "cost_micros": 1840, "error": null,
         "started_at": "2026-10-16T09:00:02Z", "finished_at": "2026-10-16T09:00:05Z"}]);
}

/// Starts `n` page jobs under a crawl/sitemap parent job, as strata_ingest's crawler does; returns their ids.
pub fn add_pages(f: &Shared, parent: &str, n: usize) -> Vec<String> {
    let mut g = f.lock().unwrap();
    let store = g.jobs[parent]["store_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let source_id = g.jobs[parent]["meta"]["source_id"].clone();
    let options = g.jobs[parent]["options"].clone();
    let mut pages = Vec::new();
    for i in 0..n {
        // strata_ingest's page-job source names its parent (`pipeline::discover`)
        let source = json!({"kind": "url", "url": format!("https://help.example.nl/page-{i}"),
                            "parent_job_id": parent});
        pages.push(new_job_with(
            &mut g,
            &store,
            source,
            options.clone(),
            &source_id,
        ));
    }
    let p = g.jobs.get_mut(parent).expect("known job");
    let mut all = p["child_job_ids"].as_array().cloned().unwrap_or_default();
    all.extend(pages.iter().map(|c| json!(c)));
    p["child_job_ids"] = json!(all);
    pages
}

/// The parent is done discovering: `completed`, its `children` summed from its page jobs' current statuses.
pub fn settle_parent(f: &Shared, parent: &str) {
    let mut g = f.lock().unwrap();
    let pages: Vec<String> = g.jobs[parent]["child_job_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut by_status = serde_json::Map::new();
    for c in &pages {
        let st = g.jobs[c.as_str()]["status"]
            .as_str()
            .unwrap_or("queued")
            .to_string();
        let n = by_status.get(&st).and_then(Value::as_u64).unwrap_or(0) + 1;
        by_status.insert(st, json!(n));
    }
    let p = g.jobs.get_mut(parent).expect("known job");
    p["status"] = json!("completed");
    p["children"] = json!({"total": pages.len(), "by_status": by_status});
    p["child_job_ids"] = json!(pages);
}

pub fn fail(f: &Shared, job: &str, message: &str) {
    let mut g = f.lock().unwrap();
    let j = g.jobs.get_mut(job).expect("known job");
    j["status"] = json!("failed");
    j["error"] = json!({"code": "fetch_failed", "message": message});
}

/// A job created directly in the fake (not through carebus-api), e.g. one carrying an echoed source.
pub fn add_job(f: &Shared, store: &str, source: Value) -> String {
    let mut g = f.lock().unwrap();
    new_job(
        &mut g,
        store,
        source,
        json!({"ocr": "auto", "captions": true, "language": null}),
    )
}

// ---- strata_rag eval (PR #21) ----------------------------------------------------------------------------------

pub fn eval_id(n: u32) -> String {
    format!("7f1c9a2e-0000-4000-8000-{:012x}", 0xe00 + n)
}

const EVAL_TS: &str = "2026-10-19T09:00:00.000000Z";

fn eval_seen(g: &mut Fake, req: &Request<axum::body::Body>, body: &str) {
    let h = req.headers();
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    g.eval_seen.push(EvalSeen {
        method: req.method().to_string(),
        path_and_query: req
            .uri()
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default(),
        actor: get("x-actor-id"),
        content_type: get("content-type"),
        body: body.to_string(),
    });
}

fn text_field(v: &Value, k: &str) -> Value {
    match v[k].as_str().map(str::trim) {
        Some(t) if !t.is_empty() => json!(t),
        _ => Value::Null,
    }
}
fn ids_field(v: &Value, k: &str) -> Value {
    v[k].as_array()
        .cloned()
        .map(Value::Array)
        .unwrap_or(json!([]))
}

/// strata_rag's `QuestionInput::problems` for the fields the fake models.
fn question_problems(q: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if q["query"].as_str().is_none_or(|t| t.trim().is_empty()) {
        out.push("query is empty".to_string());
    }
    let has = |k: &str| !text_field(q, k).is_null();
    let has_ids = |k: &str| q[k].as_array().is_some_and(|a| !a.is_empty());
    if !has("expected_document")
        && !has("expected_text")
        && !has_ids("expected_document_ids")
        && !has_ids("expected_chunk_ids")
    {
        out.push("no expectation: give expected_document, expected_document_ids, expected_chunk_ids or expected_text".into());
    }
    out
}

fn stored_question(id: &str, q: &Value, actor: Option<&str>) -> Value {
    json!({
        "id": id, "query": q["query"].as_str().unwrap_or_default().trim(),
        "expected_document": text_field(q, "expected_document"),
        "expected_leaf": text_field(q, "expected_leaf"),
        "expected_document_ids": ids_field(q, "expected_document_ids"),
        "expected_chunk_ids": ids_field(q, "expected_chunk_ids"),
        "expected_text": text_field(q, "expected_text"),
        "created_by": actor, "created_at": EVAL_TS, "updated_at": EVAL_TS,
    })
}

fn duplicate(g: &Fake, query: &str, except: Option<&str>) -> bool {
    g.eval_questions.iter().any(|q| {
        q["query"].as_str().unwrap_or_default().to_lowercase() == query.trim().to_lowercase()
            && except.is_none_or(|id| q["id"] != id)
    })
}

/// Splits the request and checks auth; `Err` is the response to send.
#[allow(clippy::result_large_err)] // a test fake: the Err is the response to send
async fn eval_in(f: &Shared, req: Request) -> Result<(Value, String), Response> {
    if !strata_auth(req.headers(), RAG_TOKEN) {
        return Err(unauthorized());
    }
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let req = Request::from_parts(parts, axum::body::Body::empty());
    let mut g = f.lock().unwrap();
    eval_seen(&mut g, &req, &text);
    if g.rag_down {
        return Err(down());
    }
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((json, text))
}

fn cap(g: &Fake) -> usize {
    g.eval_cap.unwrap_or(200)
}

async fn eval_list_questions(State(f): State<Shared>, req: Request) -> Response {
    if let Err(r) = eval_in(&f, req).await {
        return r;
    }
    let g = f.lock().unwrap();
    Json(json!({"items": g.eval_questions, "next": null})).into_response()
}

async fn eval_create_question(State(f): State<Shared>, req: Request) -> Response {
    let actor = req
        .headers()
        .get("x-actor-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (q, _) = match eval_in(&f, req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let p = question_problems(&q);
    if !p.is_empty() {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            &p.join("; "),
        );
    }
    let mut g = f.lock().unwrap();
    if duplicate(&g, q["query"].as_str().unwrap_or_default(), None) {
        return err(
            StatusCode::CONFLICT,
            "question_exists",
            "a question with this query already exists",
        );
    }
    if g.eval_questions.len() >= cap(&g) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "too_many_questions",
            "a tenant holds at most 200 eval questions",
        );
    }
    g.next_eval += 1;
    let row = stored_question(&eval_id(g.next_eval), &q, actor.as_deref());
    g.eval_questions.push(row.clone());
    (StatusCode::CREATED, Json(row)).into_response()
}

async fn eval_update_question(
    State(f): State<Shared>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let (patch, _) = match eval_in(&f, req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    if patch.as_object().is_none_or(|o| o.is_empty()) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "nothing to update: give at least one field",
        );
    }
    let mut g = f.lock().unwrap();
    let Some(i) = g.eval_questions.iter().position(|q| q["id"] == id.as_str()) else {
        return err(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("eval question {id}"),
        );
    };
    let mut next = g.eval_questions[i].clone();
    for (k, v) in patch.as_object().cloned().unwrap_or_default() {
        next[k] = v;
    }
    let p = question_problems(&next);
    if !p.is_empty() {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            &p.join("; "),
        );
    }
    if duplicate(
        &g,
        next["query"].as_str().unwrap_or_default(),
        Some(id.as_str()),
    ) {
        return err(
            StatusCode::CONFLICT,
            "question_exists",
            "a question with this query already exists",
        );
    }
    let created_by = next["created_by"].as_str().map(str::to_string);
    let mut row = stored_question(&id, &next, created_by.as_deref());
    row["updated_at"] = json!("2026-10-19T10:00:00.000000Z");
    g.eval_questions[i] = row.clone();
    Json(row).into_response()
}

async fn eval_delete_question(
    State(f): State<Shared>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    if let Err(r) = eval_in(&f, req).await {
        return r;
    }
    let mut g = f.lock().unwrap();
    let before = g.eval_questions.len();
    g.eval_questions.retain(|q| q["id"] != id.as_str());
    if g.eval_questions.len() == before {
        return err(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("eval question {id}"),
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

/// A plain CSV (no quoting) with a `query` header and expectation columns; ids `;`-separated.
fn csv_questions(text: &str) -> Result<Vec<Value>, Vec<String>> {
    let mut lines = text.lines();
    let header: Vec<String> = lines
        .next()
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_lowercase())
        .collect();
    if !header.iter().any(|h| h == "query") {
        return Err(vec!["line 1: the header needs a `query` column".into()]);
    }
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (n, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let cells: Vec<&str> = line.split(',').collect();
        let mut q = serde_json::Map::new();
        for (i, h) in header.iter().enumerate() {
            let c = cells.get(i).copied().unwrap_or_default().trim();
            let v = if h.ends_with("_ids") {
                json!(c.split(';').filter(|s| !s.is_empty()).collect::<Vec<_>>())
            } else {
                json!(c)
            };
            q.insert(h.clone(), v);
        }
        let q = Value::Object(q);
        let p = question_problems(&q);
        if p.is_empty() {
            out.push(q);
        } else {
            errors.push(format!("line {}: {}", n + 2, p.join("; ")));
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

async fn eval_import(State(f): State<Shared>, req: Request) -> Response {
    let ct = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let actor = req
        .headers()
        .get("x-actor-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (json, text) = match eval_in(&f, req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let parsed = if ct.starts_with("application/json") {
        let qs = json["questions"].as_array().cloned().unwrap_or_default();
        let bad: Vec<String> = qs
            .iter()
            .enumerate()
            .filter_map(|(i, q)| {
                let p = question_problems(q);
                (!p.is_empty()).then(|| format!("question {}: {}", i + 1, p.join("; ")))
            })
            .collect();
        if bad.is_empty() { Ok(qs) } else { Err(bad) }
    } else {
        csv_questions(&text)
    };
    let qs = match parsed {
        Ok(q) if !q.is_empty() => q,
        Ok(_) => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                "nothing imported: no question rows",
            );
        }
        Err(p) => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                &format!("nothing imported: {}", p.join("; ")),
            );
        }
    };
    let mut g = f.lock().unwrap();
    if let Some(q) = qs
        .iter()
        .find(|q| duplicate(&g, q["query"].as_str().unwrap_or_default(), None))
    {
        return err(
            StatusCode::CONFLICT,
            "question_exists",
            &format!(
                "nothing imported: `{}` is already stored",
                q["query"].as_str().unwrap_or_default()
            ),
        );
    }
    if g.eval_questions.len() + qs.len() > cap(&g) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "too_many_questions",
            "a tenant holds at most 200 eval questions",
        );
    }
    for q in &qs {
        g.next_eval += 1;
        let row = stored_question(&eval_id(g.next_eval), q, actor.as_deref());
        g.eval_questions.push(row);
    }
    (
        StatusCode::CREATED,
        Json(json!({"added": qs.len(), "total": g.eval_questions.len()})),
    )
        .into_response()
}

/// Deterministic scores: question `i` hits at rank `i + 1` in `embed`/`hybrid`/`laya` and misses in `flat`.
async fn eval_run(State(f): State<Shared>, req: Request) -> Response {
    let actor = req
        .headers()
        .get("x-actor-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (b, _) = match eval_in(&f, req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b["source"] != "tenant" && b["question_ids"].is_null() && b["questions"].is_null() {
        // the built-in file eval: its report has no run id; refused (generic 409 `conflict`) while a tenant run runs
        if f.lock()
            .unwrap()
            .eval_runs
            .iter()
            .any(|r| r["status"] == "running")
        {
            return err(
                StatusCode::CONFLICT,
                "conflict",
                "an eval is already running for this tenant",
            );
        }
        return Json(
            json!({"reports": [], "verdict": "PASS", "contaminated": false, "foreign_stores": []}),
        )
        .into_response();
    }
    let mut g = f.lock().unwrap();
    if g.eval_conflict {
        return err(
            StatusCode::CONFLICT,
            "conflict",
            "the request conflicted with a concurrent change; retry",
        );
    }
    if g.eval_runs.iter().any(|r| r["status"] == "running") {
        return err(
            StatusCode::CONFLICT,
            "eval_running",
            "an eval is already running for this tenant; retry when it finishes",
        );
    }
    if let Some(root) = b["root_id"].as_str()
        && !stores().iter().any(|s| s["id"] == root)
    {
        return err(StatusCode::NOT_FOUND, "not_found", &format!("store {root}"));
    }
    let picked: Vec<Value> = match b["question_ids"].as_array() {
        Some(ids) => {
            let mut out = Vec::new();
            for id in ids {
                match g.eval_questions.iter().find(|q| q["id"] == *id) {
                    Some(q) => out.push(q.clone()),
                    None => {
                        return err(
                            StatusCode::NOT_FOUND,
                            "not_found",
                            &format!("eval question {id}"),
                        );
                    }
                }
            }
            out
        }
        None => g.eval_questions.clone(),
    };
    if picked.is_empty() {
        return err(
            StatusCode::CONFLICT,
            "eval_not_ready",
            "this tenant has no eval questions; add some with POST /api/eval/questions",
        );
    }
    let leaves = ["help-centre", "customers"];
    if let Some(q) = picked.iter().find(|q| {
        q["expected_leaf"]
            .as_str()
            .is_some_and(|l| !leaves.contains(&l))
    }) {
        return err(
            StatusCode::CONFLICT,
            "eval_not_ready",
            &format!(
                "question `{}`: expected_leaf is not a leaf store in the evaluated tree",
                q["query"].as_str().unwrap_or_default()
            ),
        );
    }
    let modes: Vec<String> = b["modes"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec!["embed".into(), "hybrid".into(), "flat".into()]);
    let n = picked.len();
    let mut results = Vec::new();
    let mut reports = Vec::new();
    for mode in &modes {
        let mut ranks = Vec::new();
        for (i, q) in picked.iter().enumerate() {
            let rank = (mode != "flat").then_some(i + 1);
            let leaf_hit = if mode == "flat" || q["expected_leaf"].is_null() {
                Value::Null
            } else {
                json!(true)
            };
            results.push(json!({"index": i, "question_id": q["id"], "mode": mode, "rank": rank, "leaf_hit": leaf_hit}));
            ranks.push(rank);
        }
        let at =
            |k: usize| ranks.iter().filter(|r| r.is_some_and(|r| r <= k)).count() as f64 / n as f64;
        let leaf = picked.iter().any(|q| !q["expected_leaf"].is_null()) && mode != "flat";
        let leaf_accuracy = if leaf { json!(1.0) } else { Value::Null };
        let laya_consulted = if mode == "laya" || mode == "hybrid" {
            n
        } else {
            0
        };
        let (r1, r5, r10) = (at(1), at(5), at(10));
        reports.push(
            json!({"mode": mode, "n": n, "recall_at_1": r1, "recall_at_5": r5, "recall_at_10": r10,
            "leaf_accuracy": leaf_accuracy, "laya_consulted": laya_consulted, "laya_fallbacks": 0}),
        );
    }
    let questions: Vec<Value> = picked
        .iter()
        .map(|q| json!({"id": q["id"], "query": q["query"], "expected_document": q["expected_document"],
            "expected_leaf": q["expected_leaf"], "expected_document_ids": q["expected_document_ids"],
            "expected_chunk_ids": q["expected_chunk_ids"], "expected_text": q["expected_text"]}))
        .collect();
    g.next_eval += 1;
    let verdict = if reports
        .iter()
        .all(|r| r["recall_at_10"].as_f64() >= Some(0.8))
    {
        "PASS"
    } else {
        "FAIL"
    };
    let id = eval_id(g.next_eval);
    let done = json!({
        "id": id, "source": "tenant", "root_id": b["root_id"], "modes": modes,
        "beam": b["beam"].as_u64().unwrap_or(3), "min_score": b["min_score"].as_f64().unwrap_or(0.35),
        "status": "completed", "error": null, "progress": {"done": n * modes.len(), "total": n * modes.len()},
        "verdict": verdict, "started_by": actor, "started_at": EVAL_TS, "finished_at": "2026-10-19T09:00:04.250000Z",
        "took_ms": 4250, "questions": questions, "reports": reports, "results": results,
    });
    // stored `running` at once (snapshot taken, nothing scored yet); the scores land by the next poll
    let mut running = done.clone();
    for k in ["verdict", "finished_at", "took_ms", "reports"] {
        running[k] = Value::Null;
    }
    running["status"] = json!("running");
    running["progress"] = json!({"done": 0, "total": n * modes.len()});
    running["results"] = json!([]);
    g.eval_runs.push(running.clone());
    g.eval_pending.insert(id.clone(), done);
    let mut h = HeaderMap::new();
    h.insert(
        header::LOCATION,
        format!("/api/eval/runs/{id}").parse().unwrap(),
    );
    (StatusCode::ACCEPTED, h, Json(summary(&running))).into_response()
}

/// strata_rag PR #21 @dc61a3a: a listed (or just started) run is a summary, without `questions` and `results`.
fn summary(r: &Value) -> Value {
    let mut r = r.clone();
    let o = r.as_object_mut().unwrap();
    o.remove("results");
    let n = o
        .remove("questions")
        .and_then(|q| q.as_array().map(Vec::len));
    o.insert("question_count".into(), json!(n.unwrap_or(0)));
    r
}

/// The run's background task has ended by the time it is polled: `completed` with its scores, or `failed`.
fn settle_eval(g: &mut Fake, id: &str) {
    let Some(mut done) = g.eval_pending.remove(id) else {
        return;
    };
    if let Some(e) = g.eval_fail.take() {
        for k in ["verdict", "reports"] {
            done[k] = Value::Null;
        }
        done["status"] = json!("failed");
        done["error"] = json!(e);
        done["results"] = json!([]);
        done["progress"]["done"] = json!(0);
    }
    if let Some(r) = g.eval_runs.iter_mut().find(|r| r["id"] == id) {
        *r = done;
    }
}

#[derive(serde::Deserialize)]
struct RunsQ {
    limit: Option<usize>,
    cursor: Option<String>,
}

async fn eval_list_runs(State(f): State<Shared>, Query(q): Query<RunsQ>, req: Request) -> Response {
    if let Err(r) = eval_in(&f, req).await {
        return r;
    }
    let g = f.lock().unwrap();
    let newest: Vec<Value> = g.eval_runs.iter().rev().map(summary).collect();
    let start = q.cursor.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
    let limit = q.limit.unwrap_or(20);
    let items: Vec<Value> = newest.iter().skip(start).take(limit).cloned().collect();
    let next = (start + limit < newest.len()).then(|| (start + limit).to_string());
    Json(json!({"items": items, "next": next})).into_response()
}

async fn eval_get_run(State(f): State<Shared>, Path(id): Path<String>, req: Request) -> Response {
    if let Err(r) = eval_in(&f, req).await {
        return r;
    }
    let mut g = f.lock().unwrap();
    settle_eval(&mut g, &id);
    match g.eval_runs.iter().find(|r| r["id"] == id.as_str()) {
        Some(r) => Json(r.clone()).into_response(),
        None => err(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("eval run {id}"),
        ),
    }
}

pub async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

/// strata_rag + strata_ingest on one port (each behind its own bearer).
pub async fn start() -> (String, Shared) {
    let f: Shared = Arc::new(Mutex::new(Fake::default()));
    let app = Router::new()
        .route("/api/stores", get(list_stores).post(create_store))
        .route("/api/stores/{id}", get(get_store).delete(delete_store))
        .route("/api/stores/{id}/documents", get(store_documents))
        .route("/api/query", post(query))
        .route("/api/documents/{id}", delete(delete_document))
        .route("/api/ingest/jobs", get(list_jobs).post(create_jobs))
        .route("/api/ingest/jobs/{id}", get(get_job).delete(delete_job))
        .route("/api/ingest/jobs/{id}/cancel", post(cancel_job))
        .route("/api/ingest/jobs/{id}/retry", post(retry_job))
        .route(
            "/api/eval/questions",
            get(eval_list_questions).post(eval_create_question),
        )
        .route("/api/eval/questions/import", post(eval_import))
        .route(
            "/api/eval/questions/{id}",
            axum::routing::patch(eval_update_question).delete(eval_delete_question),
        )
        .route("/api/eval", post(eval_run))
        .route("/api/eval/runs", get(eval_list_runs))
        .route("/api/eval/runs/{id}", get(eval_get_run))
        .with_state(f.clone());
    (serve(app).await, f)
}

pub async fn sources_env(test: &str) -> (TestEnv, Shared) {
    sources_env_with(test, |_| {}).await
}

/// A test env whose strata_rag and strata_ingest are the fake; `tweak` runs last.
pub async fn sources_env_with(test: &str, tweak: impl FnOnce(&mut Config)) -> (TestEnv, Shared) {
    let (url, fake) = start().await;
    let e = super::env_with(test, |c| {
        c.strata_rag_url = Some(url.clone());
        c.strata_rag_token = Some(RAG_TOKEN.into());
        c.strata_ingest_url = Some(url.clone());
        c.strata_ingest_token = Some(INGEST_TOKEN.into());
        tweak(c);
    })
    .await;
    (e, fake)
}

// ---- a customer's API (test-connection target) -----------------------------------------------------------------

pub struct SampleApi {
    pub url: String,
    /// Every request that reached the server, whatever its path.
    pub hits: Arc<AtomicUsize>,
}

async fn count(State(hits): State<Arc<AtomicUsize>>, req: Request, next: Next) -> Response {
    hits.fetch_add(1, Ordering::SeqCst);
    next.run(req).await
}

pub async fn sample_api() -> SampleApi {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/customers",
            get(|h: HeaderMap| async move {
                if h.get("x-api-key").and_then(|v| v.to_str().ok()) != Some(API_KEY) {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"error": "no key"})))
                        .into_response();
                }
                Json(json!([
                    {"customer_id": "C-10482", "name": "Anna de Vries", "email": "anna@example.nl"},
                    {"customer_id": "C-10483", "name": "Bram Jansen", "email": "bram@example.nl"}]))
                .into_response()
            }),
        )
        .route(
            "/me",
            get(|h: HeaderMap| async move {
                if !bearer(&h, API_KEY) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Json(json!({"customer_id": "C-10482", "tier": "gold"})).into_response()
            }),
        )
        .route("/big", get(|| async { "a".repeat(300 * 1024) }))
        .route("/moved", get(|| async { Redirect::to("/customers") }))
        .route(
            "/echo",
            get(|h: HeaderMap| async move {
                let seen: serde_json::Map<String, Value> = h
                    .iter()
                    .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or_default())))
                    .collect();
                Json(json!({"seen": seen}))
            }),
        )
        .route(
            "/echo-text",
            get(|h: HeaderMap| async move {
                // the credential lands across the 2000-character excerpt cut
                format!("{}{:?}", "p".repeat(1950), h)
            }),
        )
        .route("/text", get(|| async { "plain text body" }))
        .layer(middleware::from_fn_with_state(hits.clone(), count));
    SampleApi {
        url: serve(app).await,
        hits,
    }
}
