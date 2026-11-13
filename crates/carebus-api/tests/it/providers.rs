//! `/api/providers/slots`: the tenant's model providers, merged from strata_rag (embed, rerank, chat) and strata_ingest
//! (ocr, vision, transcribe). Each service is faked here with the `GET/PUT /api/tenant/providers` contract.
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};

const RAG_TOKEN: &str = "rag-svc-token";
const INGEST_TOKEN: &str = "ingest-svc-token";
const UPDATED: &str = "2026-10-16T09:00:00.000000Z";

/// One provider service: the capabilities it offers, its tenant registrations (stored with whether a key is held)
/// and its dev defaults. Every PUT is recorded with its Authorization header.
#[derive(Clone)]
struct Fake {
    caps: &'static [&'static str],
    /// strata_rag's views carry `dims` (and, for chat, `thinking` / `temperature` when set: strata_rag #16 refuses
    /// both on any other capability); strata_ingest's have none of these fields and refuses them. strata_ingest also
    /// requires a key (422 `api_key_required` when none is sent and none is stored) and answers a malformed body
    /// with 400 `bad_request` (d0a439b).
    dims: bool,
    stored: Arc<Mutex<Map<String, Value>>>,
    defaults: Arc<Mutex<Map<String, Value>>>,
    puts: Arc<Mutex<Vec<(String, Value)>>>,
    /// When set, every PUT answers this status and body.
    reject: Arc<Mutex<Option<(u16, Value)>>>,
    /// When set, every GET and PUT first waits this long (widens a read-modify-write race).
    delay: Arc<Mutex<Option<std::time::Duration>>>,
}

const FIELDS: [&str; 9] = [
    "base_url",
    "dialect",
    "model",
    "model_id",
    "regions",
    "zdr",
    "timeout_ms",
    "concurrency",
    "api_key",
];

/// strata_rag only, chat only: `null` (or absent) = not sent to the provider.
const CHAT_FIELDS: [&str; 2] = ["thinking", "temperature"];

impl Fake {
    fn new(caps: &'static [&'static str], dims: bool) -> Self {
        Self {
            caps,
            dims,
            stored: Arc::default(),
            defaults: Arc::default(),
            puts: Arc::default(),
            reject: Arc::default(),
            delay: Arc::default(),
        }
    }

    /// A tenant registration as the service stores it (no key; only whether one is held).
    fn register(&self, cap: &str, base_url: &str, dialect: &str, model: &str, has_key: bool) {
        let mut v = json!({
            "base_url": base_url, "dialect": dialect, "model": model, "model_id": null,
            "regions": ["eu"], "zdr": true, "timeout_ms": 30000, "concurrency": 8,
            "has_api_key": has_key, "updated_at": UPDATED,
        });
        if self.dims {
            v["dims"] = if cap == "embed" {
                json!(1024)
            } else {
                Value::Null
            };
        }
        self.stored.lock().unwrap().insert(cap.into(), v);
    }

    fn default_for(&self, cap: &str, dialect: &str, model: &str) {
        let mut v = json!({"dialect": dialect, "model": model});
        if self.dims {
            v["dims"] = if cap == "embed" {
                json!(1024)
            } else {
                Value::Null
            };
        }
        self.defaults.lock().unwrap().insert(cap.into(), v);
    }

    fn view(&self) -> Value {
        let stored = self.stored.lock().unwrap();
        let defaults = self.defaults.lock().unwrap();
        let mut out = Map::new();
        for cap in self.caps {
            let mut v = if let Some(p) = stored.get(*cap) {
                p.clone()
            } else if let Some(d) = defaults.get(*cap) {
                d.clone()
            } else {
                json!({})
            };
            let source = if stored.contains_key(*cap) {
                "tenant"
            } else if defaults.contains_key(*cap) {
                "default"
            } else {
                "unconfigured"
            };
            v["source"] = json!(source);
            out.insert((*cap).into(), v);
        }
        Value::Object(out)
    }

    async fn wait(&self) {
        let d = *self.delay.lock().unwrap();
        if let Some(d) = d {
            tokio::time::sleep(d).await;
        }
    }

    fn puts(&self) -> Vec<(String, Value)> {
        self.puts.lock().unwrap().clone()
    }

    async fn start(self, token: &'static str) -> String {
        let app = axum::Router::new()
            .route("/api/tenant/providers", get(fake_get).put(fake_put))
            .with_state((self, token));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }
}

fn bad(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

fn authorized(h: &HeaderMap, token: &str) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {token}"))
        && h.get("x-tenant-id").and_then(|v| v.to_str().ok()) == Some(crate::common::BOOT_TENANT)
}

async fn fake_get(State((f, token)): State<(Fake, &'static str)>, h: HeaderMap) -> Response {
    if !authorized(&h, token) {
        return bad(StatusCode::UNAUTHORIZED, "unauthorized", "bad token");
    }
    let view = f.view();
    f.wait().await; // answer with what was read before the wait, as a slow upstream would
    Json(view).into_response()
}

/// The contract: `deny_unknown_fields`, the full set (a capability left out is removed), and an omitted
/// `api_key` keeps the stored key only when `base_url` is unchanged.
async fn fake_put(
    State((f, token)): State<(Fake, &'static str)>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authorized(&h, token) {
        return bad(StatusCode::UNAUTHORIZED, "unauthorized", "bad token");
    }
    f.wait().await;
    let auth = h["authorization"].to_str().unwrap().to_string();
    f.puts.lock().unwrap().push((auth, body.clone()));
    if let Some((status, b)) = f.reject.lock().unwrap().clone() {
        return (StatusCode::from_u16(status).unwrap(), Json(b)).into_response();
    }
    let malformed = if f.dims {
        "invalid_body"
    } else {
        "bad_request"
    };
    let Some(set) = body.as_object() else {
        return bad(StatusCode::BAD_REQUEST, malformed, "not an object");
    };
    let mut next = Map::new();
    for (cap, input) in set {
        if !f.caps.contains(&cap.as_str()) {
            return bad(StatusCode::BAD_REQUEST, malformed, "unknown field");
        }
        let Some(input) = input.as_object() else {
            continue; // null: removed
        };
        for k in input.keys() {
            let chat_only = CHAT_FIELDS.contains(&k.as_str());
            if !(FIELDS.contains(&k.as_str()) || (f.dims && (k == "dims" || chat_only))) {
                return bad(StatusCode::BAD_REQUEST, malformed, "unknown field");
            }
            if chat_only && cap != "chat" && !input[k].is_null() {
                return bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_provider",
                    "thinking and temperature are only valid for chat",
                );
            }
        }
        let prior = f.stored.lock().unwrap().get(cap).cloned();
        let has_key = match (input.get("api_key"), &prior) {
            (Some(Value::String(_)), _) => true,
            (_, Some(p)) if p["has_api_key"] == true => {
                if p["base_url"] != input["base_url"] {
                    return bad(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "api_key_required",
                        "a new base_url needs its key",
                    );
                }
                true
            }
            _ if !f.dims => {
                return bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "api_key_required",
                    "api_key is required",
                );
            }
            _ => false,
        };
        let mut v = json!({
            "base_url": input["base_url"], "dialect": input["dialect"], "model": input["model"],
            "model_id": input.get("model_id").cloned().unwrap_or(Value::Null),
            "regions": input["regions"], "zdr": input["zdr"],
            "timeout_ms": input.get("timeout_ms").cloned().unwrap_or(json!(30000)),
            "concurrency": input.get("concurrency").cloned().unwrap_or(json!(8)),
            "has_api_key": has_key, "updated_at": UPDATED,
        });
        if f.dims {
            v["dims"] = input.get("dims").cloned().unwrap_or(Value::Null);
            // strata_rag's view carries them only when set (`skip_serializing_if`)
            for k in CHAT_FIELDS {
                if let Some(x) = input.get(k).filter(|x| !x.is_null()) {
                    v[k] = x.clone();
                }
            }
        }
        next.insert(cap.clone(), v);
    }
    *f.stored.lock().unwrap() = next;
    Json(f.view()).into_response()
}

struct World {
    e: crate::common::TestEnv,
    rag: Fake,
    ingest: Fake,
}

/// strata_rag: embed registered (with a key), rerank and chat dev defaults. strata_ingest: ocr registered, the
/// rest unconfigured.
async fn world(test: &str) -> World {
    let rag = Fake::new(&["embed", "rerank", "chat"], true);
    rag.register(
        "embed",
        "https://embed.example.eu/v1",
        "openai",
        "acme-embed",
        true,
    );
    rag.default_for("rerank", "cohere", "BAAI/bge-reranker-v2-m3");
    rag.default_for("chat", "openai", "local-llm");
    let ingest = Fake::new(&["ocr", "vision", "transcribe"], false);
    ingest.register(
        "ocr",
        "https://ocr.example.eu/v1",
        "mistral",
        "mistral-ocr-latest",
        true,
    );
    let rag_url = rag.clone().start(RAG_TOKEN).await;
    let ingest_url = ingest.clone().start(INGEST_TOKEN).await;
    let e = crate::common::env_with(test, |c| {
        c.strata_rag_url = Some(rag_url);
        c.strata_rag_token = Some(RAG_TOKEN.into());
        c.strata_ingest_url = Some(ingest_url);
        c.strata_ingest_token = Some(INGEST_TOKEN.into());
    })
    .await;
    World { e, rag, ingest }
}

fn cap<'a>(b: &'a Value, name: &str) -> &'a Value {
    b["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["capability"] == name)
        .unwrap_or_else(|| panic!("no {name} in {b}"))
}

fn admin() -> String {
    crate::common::admin()
}

/// Built at runtime so no secret-looking literal is committed.
fn a_key() -> String {
    format!("k-{}", uuid::Uuid::new_v4().simple())
}

async fn put(w: &World, capability: &str, body: Value) -> (StatusCode, Value) {
    crate::common::send(
        &w.e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/slots/{capability}"),
            Some(&admin()),
            body,
        ),
    )
    .await
}

fn rerank_input(key: Option<&str>) -> Value {
    let mut v = json!({
        "base_url": "https://rerank.example.eu/v2", "dialect": "cohere", "model": "acme-rerank",
        "regions": ["eu"], "zdr": true,
    });
    if let Some(k) = key {
        v["api_key"] = json!(k);
    }
    v
}

fn chat_input(key: Option<&str>) -> Value {
    let mut v = json!({
        "base_url": "https://chat.example.eu/v1", "dialect": "openai", "model": "acme-chat",
        "regions": ["eu"], "zdr": true, "timeout_ms": 60000, "thinking": false, "temperature": 0.2,
    });
    if let Some(k) = key {
        v["api_key"] = json!(k);
    }
    v
}

#[tokio::test]
async fn the_list_merges_both_services_per_capability_with_owner_and_source() {
    let w = world("providers-list").await;
    let viewer = crate::common::user();
    let (s, b) = crate::common::send(
        &w.e.app,
        crate::common::get("/api/providers/slots", Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let names: Vec<&str> = b["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["capability"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["embed", "rerank", "chat", "ocr", "vision", "transcribe"]
    );
    assert_eq!(
        b["services"],
        json!([{"name": "strata_rag", "status": "ok"}, {"name": "strata_ingest", "status": "ok"}])
    );

    let embed = cap(&b, "embed");
    assert_eq!(
        (&embed["owner"], &embed["available"], &embed["source"]),
        (&json!("strata_rag"), &json!(true), &json!("tenant"))
    );
    assert_eq!(embed["provider"]["base_url"], "https://embed.example.eu/v1");
    assert_eq!(embed["provider"]["dims"], 1024);
    assert_eq!(embed["provider"]["has_api_key"], true);
    assert_eq!(embed["provider"]["updated_at"], UPDATED);
    assert_eq!(embed["default"], Value::Null);

    let rerank = cap(&b, "rerank");
    assert_eq!(rerank["source"], "default");
    assert_eq!(rerank["provider"], Value::Null);
    assert_eq!(
        rerank["default"],
        json!({"dialect": "cohere", "model": "BAAI/bge-reranker-v2-m3", "dims": null})
    );

    let chat = cap(&b, "chat");
    assert_eq!(
        (&chat["owner"], &chat["source"]),
        (&json!("strata_rag"), &json!("default"))
    );
    assert_eq!(
        chat["default"],
        json!({"dialect": "openai", "model": "local-llm", "dims": null})
    );
    assert_eq!(
        (
            &embed["provider"]["thinking"],
            &embed["provider"]["temperature"]
        ),
        (&Value::Null, &Value::Null),
        "chat-only settings are null elsewhere"
    );

    let ocr = cap(&b, "ocr");
    assert_eq!(
        (&ocr["owner"], &ocr["source"]),
        (&json!("strata_ingest"), &json!("tenant"))
    );
    assert_eq!(
        ocr["provider"]["dims"],
        Value::Null,
        "strata_ingest has no dims"
    );
    assert_eq!(cap(&b, "vision")["source"], "unconfigured");
    assert!(
        !b.to_string().contains("\"api_key\""),
        "never a key field: {b}"
    );
    crate::common::fixture("providers/providers", &b);
}

#[tokio::test]
async fn a_down_or_unconfigured_service_is_unavailable_never_unconfigured_providers() {
    let rag = Fake::new(&["embed", "rerank"], true);
    let rag_url = rag.clone().start(RAG_TOKEN).await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let e = crate::common::env_with("providers-down", |c| {
        c.strata_rag_url = Some(rag_url);
        c.strata_rag_token = Some(RAG_TOKEN.into());
        c.strata_ingest_url = Some(dead);
        c.strata_ingest_token = Some(INGEST_TOKEN.into());
    })
    .await;
    let viewer = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/providers/slots", Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["services"][1],
        json!({"name": "strata_ingest", "status": "unavailable"})
    );
    let ocr = cap(&b, "ocr");
    assert_eq!(
        (&ocr["available"], &ocr["source"]),
        (&json!(false), &Value::Null),
        "a down service says so instead of looking unconfigured"
    );
    assert_eq!(cap(&b, "embed")["available"], true);

    let e = crate::common::env_with("providers-unconf", |c| {
        c.strata_rag_url = None;
        c.strata_ingest_url = None;
    })
    .await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/providers/slots", Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["services"][0]["status"], "unconfigured");
    assert_eq!(cap(&b, "embed")["available"], false);
    assert_eq!(
        (&cap(&b, "chat")["owner"], &cap(&b, "chat")["available"]),
        (&json!("strata_rag"), &json!(false)),
        "chat is listed under strata_rag even while it cannot be asked"
    );

    // A write to a service that is not there is 503, not a silent no-op.
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            "/api/providers/slots/rerank",
            Some(&admin()),
            rerank_input(None),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!("upstream_unavailable")
        )
    );
}

/// The PUT replaces the whole set, so carebus-api sends every capability the owner already has, untouched and
/// without `api_key` (the stored key is kept), plus the edited one. The other service is not called.
#[tokio::test]
async fn a_save_sends_the_owners_full_set_and_keeps_untouched_keys() {
    let w = world("providers-put").await;
    let key = a_key();
    let (s, b) = put(&w, "rerank", rerank_input(Some(&key))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(cap(&b, "rerank")["source"], "tenant");
    assert_eq!(cap(&b, "rerank")["provider"]["has_api_key"], true);
    assert_eq!(
        cap(&b, "embed")["provider"]["has_api_key"],
        true,
        "embed key kept"
    );
    assert!(!b.to_string().contains(&key), "the key is never echoed");

    let puts = w.rag.puts();
    assert_eq!(puts.len(), 1);
    let (auth, sent) = &puts[0];
    assert_eq!(auth, &format!("Bearer {RAG_TOKEN}"));
    assert_eq!(
        sent["embed"],
        json!({
            "base_url": "https://embed.example.eu/v1", "dialect": "openai", "model": "acme-embed",
            "model_id": null, "dims": 1024, "regions": ["eu"], "zdr": true, "timeout_ms": 30000,
            "concurrency": 8,
        }),
        "untouched embed is resent as stored, with no api_key"
    );
    assert_eq!(sent["rerank"]["api_key"], json!(key));
    assert_eq!(sent["rerank"]["model"], "acme-rerank");
    assert!(w.ingest.puts().is_empty(), "strata_ingest is not touched");

    let audit = crate::common::audit_text(&w.e).await;
    assert!(audit.contains("\"provider\""), "{audit}");
    assert!(!audit.contains(&key), "the key never reaches the audit log");
}

#[tokio::test]
async fn an_ingest_capability_goes_to_strata_ingest_without_dims() {
    let w = world("providers-ingest").await;
    let (s, b) = put(
        &w,
        "vision",
        json!({"base_url": "https://vision.example.eu/v1", "dialect": "openai", "model": "acme-vl",
               "regions": ["eu"], "zdr": true, "api_key": a_key(), "timeout_ms": 20000}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(cap(&b, "vision")["source"], "tenant");
    let puts = w.ingest.puts();
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].0, format!("Bearer {INGEST_TOKEN}"));
    let sent = &puts[0].1;
    assert!(sent["ocr"].get("api_key").is_none());
    assert!(sent["ocr"].get("dims").is_none(), "{sent}");
    assert_eq!(sent["vision"]["timeout_ms"], 20000);
    assert!(w.rag.puts().is_empty());
}

/// strata_ingest requires a key: a first registration without one comes back as its 422, passed through.
#[tokio::test]
async fn an_ingest_registration_without_a_key_is_strata_ingests_api_key_required() {
    let w = world("providers-ingest-key").await;
    let (s, b) = put(
        &w,
        "transcribe",
        json!({"base_url": "https://asr.example.eu/v1", "dialect": "openai", "model": "acme-asr",
               "regions": ["eu"], "zdr": true, "api_key": "  "}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::UNPROCESSABLE_ENTITY, json!("api_key_required")),
        "{b}"
    );
    let puts = w.ingest.puts();
    let sent = &puts[0].1;
    assert!(
        sent["transcribe"].get("api_key").is_none(),
        "a blank key is dropped, not sent: {sent}"
    );
    assert!(b.get("capabilities").is_none(), "an error, not a view: {b}");
}

/// Two admins (on two replicas) save embed and rerank at once. Without serialising the read-modify-write both
/// read the same set and the second PUT reverts the first; with the per-owner lock both registrations survive.
#[tokio::test]
async fn concurrent_saves_to_two_capabilities_of_one_owner_both_survive() {
    let w = world("providers-race").await;
    *w.rag.delay.lock().unwrap() = Some(std::time::Duration::from_millis(300));
    let embed = json!({"base_url": "https://embed.example.eu/v1", "dialect": "openai", "model": "acme-embed-2",
                       "dims": 1024, "regions": ["eu"], "zdr": true});
    let ((s1, b1), (s2, b2)) = tokio::join!(
        put(&w, "embed", embed),
        put(&w, "rerank", rerank_input(Some(&a_key())))
    );
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{b1} {b2}");
    *w.rag.delay.lock().unwrap() = None;
    let (_, b) = crate::common::send(
        &w.e.app,
        crate::common::get("/api/providers/slots", Some(&admin())),
    )
    .await;
    assert_eq!(cap(&b, "embed")["provider"]["model"], "acme-embed-2", "{b}");
    assert_eq!(cap(&b, "rerank")["source"], "tenant", "{b}");
    assert_eq!(cap(&b, "rerank")["provider"]["model"], "acme-rerank", "{b}");
    assert_eq!(w.rag.puts().len(), 2);
}

#[tokio::test]
async fn delete_resends_the_set_without_that_capability() {
    let w = world("providers-delete").await;
    w.rag.register(
        "rerank",
        "https://rerank.example.eu/v2",
        "cohere",
        "acme-rerank",
        true,
    );
    let (s, b) = crate::common::send(
        &w.e.app,
        crate::common::json(
            "DELETE",
            "/api/providers/slots/embed",
            Some(&admin()),
            json!(null),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(cap(&b, "embed")["source"], "unconfigured");
    let puts = w.rag.puts();
    assert_eq!(puts.len(), 1);
    let sent = puts[0].1.as_object().unwrap();
    assert!(!sent.contains_key("embed") || sent["embed"].is_null());
    assert_eq!(sent["rerank"]["model"], "acme-rerank");
    assert!(sent["rerank"].get("api_key").is_none());

    // Removing what is not registered (a default) changes nothing upstream.
    let (s, _) = crate::common::send(
        &w.e.app,
        crate::common::json(
            "DELETE",
            "/api/providers/slots/vision",
            Some(&admin()),
            json!(null),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(w.ingest.puts().is_empty());
}

#[tokio::test]
async fn upstream_validation_codes_and_messages_pass_through_unchanged() {
    let w = world("providers-422").await;
    for (status, code, message) in [
        (422, "region_not_allowed", "regions must contain eu"),
        (422, "zdr_required", "zero data retention is required in eu"),
        (
            422,
            "probe_failed",
            "the provider answered a rerank probe with an auth error",
        ),
        (
            503,
            "provider_key_unconfigured",
            "STRATA_PROVIDER_KEY is not set",
        ),
    ] {
        *w.rag.reject.lock().unwrap() =
            Some((status, json!({"error": {"code": code, "message": message}})));
        let (s, b) = put(&w, "rerank", rerank_input(Some(&a_key()))).await;
        assert_eq!(s.as_u16(), status, "{b}");
        assert_eq!(
            b["error"],
            json!({"code": code, "message": message}),
            "{code}"
        );
        if code == "region_not_allowed" {
            // the shape the web maps to a field message
            crate::common::fixture("providers/provider_error", &b);
        }
    }
    // a 401 from the service is OUR credential problem: unavailable, not the user's error
    *w.rag.reject.lock().unwrap() = Some((401, json!({"error": {"code": "unauthorized"}})));
    let (s, b) = put(&w, "rerank", rerank_input(None)).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!("upstream_unavailable")
        )
    );
    let audit = crate::common::audit_text(&w.e).await;
    assert!(
        audit.contains("probe_failed"),
        "failed writes are audited with the code: {audit}"
    );
}

/// A provider that echoes the key: wherever it sits in the message (including straddling the 300-char cut), no
/// piece of it reaches the response.
#[tokio::test]
async fn an_upstream_message_echoing_the_key_never_returns_any_of_it() {
    let w = world("providers-echo").await;
    let key = a_key();
    for at in [0usize, 150, 285, 292, 299] {
        let message = format!("{}{key} was refused", "x".repeat(at));
        *w.rag.reject.lock().unwrap() = Some((
            422,
            json!({"error": {"code": "probe_failed", "message": message}}),
        ));
        let (s, b) = put(&w, "rerank", rerank_input(Some(&key))).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        let text = b["error"]["message"].as_str().unwrap();
        for piece in key.as_bytes().windows(8) {
            let piece = std::str::from_utf8(piece).unwrap();
            assert!(!text.contains(piece), "offset {at} leaked {piece}: {text}");
        }
    }
    // only part of the key: the upstream text is not echoed at all
    *w.rag.reject.lock().unwrap() = Some((
        422,
        json!({"error": {"code": "probe_failed", "message": format!("key {}… rejected", &key[..12])}}),
    ));
    let (_, b) = put(&w, "rerank", rerank_input(Some(&key))).await;
    assert_eq!(
        b["error"]["message"],
        "strata_rag rejected the provider (HTTP 422)."
    );
}

#[tokio::test]
async fn writes_are_care_admin_only_and_bodies_are_checked_before_any_upstream_call() {
    let w = world("providers-guard").await;
    let viewer = crate::common::user();
    for (method, body) in [("PUT", rerank_input(None)), ("DELETE", json!(null))] {
        let (s, _) = crate::common::send(
            &w.e.app,
            crate::common::json(method, "/api/providers/slots/rerank", Some(&viewer), body),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method}");
    }
    let cases = [
        (
            "chat2",
            rerank_input(None),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            "ocr",
            {
                let mut b = rerank_input(None);
                b["dims"] = json!(1024);
                b
            },
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
        (
            "rerank",
            {
                let mut b = rerank_input(None);
                b["source"] = json!("tenant");
                b
            },
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
        (
            "rerank",
            json!([1]),
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
        (
            "rerank",
            {
                let mut b = rerank_input(None);
                b["thinking"] = json!(true);
                b
            },
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
        (
            "chat",
            {
                let mut b = chat_input(None);
                b["dims"] = json!(1024);
                b
            },
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
        (
            "chat",
            {
                let mut b = chat_input(None);
                b["temperature"] = json!("warm");
                b
            },
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
        (
            "chat",
            {
                let mut b = chat_input(None);
                b["thinking"] = json!("yes");
                b
            },
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
    ];
    for (c, body, status, code) in cases {
        let (s, b) = put(&w, c, body).await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (status, json!(code)),
            "{c}: {b}"
        );
    }
    assert!(w.rag.puts().is_empty() && w.ingest.puts().is_empty());
}

/// chat is strata_rag's (#16): its own settings `thinking` / `temperature` go there and come back on the view, and
/// a later save of another strata_rag capability resends them, without the key.
#[tokio::test]
async fn a_chat_provider_round_trips_through_strata_rag_with_thinking_and_temperature() {
    let w = world("providers-chat").await;
    let key = a_key();
    let (s, b) = put(&w, "chat", chat_input(Some(&key))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let chat = cap(&b, "chat");
    assert_eq!(
        (&chat["owner"], &chat["source"]),
        (&json!("strata_rag"), &json!("tenant"))
    );
    assert_eq!(chat["provider"]["model"], "acme-chat");
    assert_eq!(chat["provider"]["thinking"], false);
    assert_eq!(chat["provider"]["temperature"], 0.2);
    assert_eq!(chat["provider"]["timeout_ms"], 60000);
    assert_eq!(chat["provider"]["has_api_key"], true);
    assert_eq!(chat["provider"]["dims"], Value::Null);
    assert!(!b.to_string().contains(&key), "the key is never echoed");
    assert!(w.ingest.puts().is_empty(), "strata_ingest is not touched");
    let puts = w.rag.puts();
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].0, format!("Bearer {RAG_TOKEN}"));
    let sent = &puts[0].1;
    assert_eq!(sent["chat"]["api_key"], json!(key));
    assert_eq!(
        (&sent["chat"]["thinking"], &sent["chat"]["temperature"]),
        (&json!(false), &json!(0.2))
    );
    assert!(sent["chat"].get("dims").is_none(), "{sent}");

    // The next save (rerank) resends chat as stored: its settings, no key.
    let (s, b) = put(&w, "rerank", rerank_input(Some(&a_key()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let puts = w.rag.puts();
    let sent = &puts[1].1;
    assert!(sent["chat"].get("api_key").is_none(), "{sent}");
    assert_eq!(
        (&sent["chat"]["thinking"], &sent["chat"]["temperature"]),
        (&json!(false), &json!(0.2)),
        "{sent}"
    );
    assert_eq!(cap(&b, "chat")["provider"]["has_api_key"], true, "key kept");

    // null = not sent: thinking and temperature leave the registration, and the view.
    let mut cleared = chat_input(None);
    cleared["thinking"] = Value::Null;
    cleared["temperature"] = Value::Null;
    let (s, b) = put(&w, "chat", cleared).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let puts = w.rag.puts();
    let sent = &puts[2].1;
    assert!(
        sent["chat"].get("thinking").is_none() && sent["chat"].get("temperature").is_none(),
        "{sent}"
    );
    assert_eq!(cap(&b, "chat")["provider"]["thinking"], Value::Null);
    assert_eq!(cap(&b, "chat")["provider"]["temperature"], Value::Null);
}

/// The full-set PUT: a chat save carries the untouched embed and rerank registrations (without their keys, and
/// without the chat-only fields strata_rag would refuse on them), so neither is dropped.
#[tokio::test]
async fn a_chat_save_keeps_the_embed_and_rerank_registrations() {
    let w = world("providers-chat-set").await;
    w.rag.register(
        "rerank",
        "https://rerank.example.eu/v2",
        "cohere",
        "acme-rerank",
        true,
    );
    let (s, b) = put(&w, "chat", chat_input(Some(&a_key()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let puts = w.rag.puts();
    let sent = &puts[0].1;
    assert_eq!(
        sent["embed"],
        json!({
            "base_url": "https://embed.example.eu/v1", "dialect": "openai", "model": "acme-embed",
            "model_id": null, "dims": 1024, "regions": ["eu"], "zdr": true, "timeout_ms": 30000,
            "concurrency": 8,
        }),
        "untouched embed is resent as stored, with no api_key"
    );
    assert_eq!(
        sent["rerank"],
        json!({
            "base_url": "https://rerank.example.eu/v2", "dialect": "cohere", "model": "acme-rerank",
            "model_id": null, "dims": null, "regions": ["eu"], "zdr": true, "timeout_ms": 30000,
            "concurrency": 8,
        }),
        "untouched rerank is resent as stored, with no api_key"
    );
    for c in ["embed", "rerank"] {
        assert_eq!(cap(&b, c)["source"], "tenant", "{c} survives: {b}");
        assert_eq!(cap(&b, c)["provider"]["has_api_key"], true, "{c} key kept");
    }
    assert_eq!(cap(&b, "chat")["source"], "tenant");
}

/// A strata_rag that does not offer chat yet (before #16) refuses it up front, without a PUT.
#[tokio::test]
async fn chat_on_a_strata_rag_without_it_is_capability_not_supported() {
    let rag = Fake::new(&["embed", "rerank"], true);
    let rag_url = rag.clone().start(RAG_TOKEN).await;
    let e = crate::common::env_with("providers-chat-old", |c| {
        c.strata_rag_url = Some(rag_url);
        c.strata_rag_token = Some(RAG_TOKEN.into());
        c.strata_ingest_url = None;
    })
    .await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            "/api/providers/slots/chat",
            Some(&admin()),
            chat_input(Some(&a_key())),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("capability_not_supported")),
        "{b}"
    );
    assert!(rag.puts().is_empty());
}
