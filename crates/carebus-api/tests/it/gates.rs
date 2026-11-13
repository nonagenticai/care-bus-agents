//! Laya gates (spec 2026-10-18 §3): the engine against a stub laya, the request gate in a chat turn, the admin and
//! internal routes, RLS, and the contract fixtures of the gates page.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::{Value, json};

use crate::common::{admin, user};

const LAYA_TOKEN: &str = "laya-licence-9c2f";
const SERVICE: &str = "svc-test-token";

#[derive(Clone)]
enum Reply {
    Answers(Value),
    Status(u16),
    Slow(u64),
}

struct Stub {
    url: String,
    reply: Arc<Mutex<Reply>>,
    /// (Authorization header, request body) of every call.
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Stub {
    fn set(&self, r: Reply) {
        *self.reply.lock().unwrap() = r;
    }
    fn calls(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }
}

/// Answers for every seeded question; the stub returns those that were asked. By default only `emergency` (handoff)
/// and `topic` (annotate) fire.
fn answers() -> Value {
    json!({
        "emergency": {"type": "noul", "noul": 0.93},
        "credentials_shared": {"type": "choice", "choice": "no",
                               "probabilities": {"yes": 0.02, "no": 0.97, "unsure": 0.01}, "confidence": 0.97},
        "bereavement": {"type": "noul", "noul": 0.01},
        "frustration": {"type": "score", "score": 2,
                        "probabilities": {"0": 0.05, "1": 0.2, "2": 0.5, "3": 0.2, "4": 0.05}, "confidence": 0.5},
        "topic": {"type": "choice", "choice": "delivery",
                  "probabilities": {"returns": 0.05, "warranty": 0.05, "delivery": 0.8, "payment": 0.03, "faq": 0.02, "other": 0.05},
                  "confidence": 0.8},
        "customer_consent": {"type": "noul", "noul": 0.9},
        "unbacked_claim": {"type": "noul", "noul": 0.1},
        "pii_leak": {"type": "noul", "noul": 0.1}
    })
}

fn with(mut a: Value, key: &str, field: &str, v: Value) -> Value {
    a[key][field] = v;
    a
}

async fn laya_stub() -> Stub {
    let reply = Arc::new(Mutex::new(Reply::Answers(answers())));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (r, s) = (reply.clone(), seen.clone());
    let app = Router::new().route(
        "/laya/v1/systemone",
        post(move |h: HeaderMap, axum::Json(body): axum::Json<Value>| {
            let (r, s) = (r.clone(), s.clone());
            async move {
                let auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                s.lock().unwrap().push((auth, body.clone()));
                let reply = r.lock().unwrap().clone();
                let all = match reply {
                    Reply::Status(code) => {
                        return (
                            StatusCode::from_u16(code).unwrap(),
                            axum::Json(json!({"detail": "backend down"})),
                        )
                            .into_response();
                    }
                    Reply::Slow(ms) => {
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                        answers()
                    }
                    Reply::Answers(a) => a,
                };
                let asked: serde_json::Map<String, Value> = body["questions"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .filter_map(|k| all.get(k).map(|a| (k.clone(), a.clone())))
                    .collect();
                axum::Json(json!({"model": body["model"], "answers": asked, "usage": {"input_tokens": 10}}))
                    .into_response()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/laya", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Stub { url, reply, seen }
}

/// Binds the bootstrap tenant's `gates` use to a laya provider at `url` with a sealed token (owner pool: the stub
/// listens on loopback, which the API's base_url guard refuses).
async fn bind_laya(e: &crate::common::TestEnv, url: &str) {
    bind_laya_with(e, url, Some(LAYA_TOKEN)).await;
}

/// [`bind_laya`] with `token` sealed, or no stored token at all (`None`: the tenant's platform laya licence).
async fn bind_laya_with(e: &crate::common::TestEnv, url: &str, token: Option<&str>) {
    let id = uuid::Uuid::new_v4();
    let enc = token.map(|t| {
        carebus_api::routes::llm_providers::seal_token(&crate::common::source_key(), id, t)
    });
    sqlx::query(
        "INSERT INTO llm_providers (tenant_id, id, name, kind, base_url, secret_enc, updated_by)
         VALUES ($1::uuid, $2, 'Laya stub', 'laya', $3, $4, 'test')",
    )
    .bind(crate::common::BOOT_TENANT)
    .bind(id)
    .bind(url)
    .bind(enc)
    .execute(crate::common::pool(e))
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO llm_provider_uses (tenant_id, \"use\", provider_id, model, updated_by)
         VALUES ($1::uuid, 'gates', $2, 'jev-latest', 'test')",
    )
    .bind(crate::common::BOOT_TENANT)
    .bind(id)
    .execute(crate::common::pool(e))
    .await
    .unwrap();
}

/// A test env whose laya calls may reach the loopback stub (the test-only `source_fetch_allow_loopback`): every
/// laya call resolves, checks and pins its host (crate::netguard).
async fn env(test: &str) -> crate::common::TestEnv {
    crate::common::env_with(test, |c| c.source_fetch_allow_loopback = true).await
}

/// The playground evaluation, as a tenant_admin (it spends a laya call on the tenant's token).
async fn evaluate(e: &crate::common::TestEnv, body: Value) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/gates/evaluate"),
            Some(&admin()),
            body,
        ),
    )
    .await
}

async fn internal(
    e: &crate::common::TestEnv,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json("POST", "/api/internal/gates/evaluate", token, body),
    )
    .await
}

fn boot_bus(e: &crate::common::TestEnv) -> carebus::Bus {
    e.state.bus.scoped(&carebus::Scope {
        tenant: crate::common::BOOT_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    })
}

#[tokio::test]
async fn rules_fire_and_the_strongest_action_wins_in_one_call() {
    let e = env("gates-fire").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;

    // gte: emergency (handoff) and topic (annotate) fire; handoff wins and carries emergency's message
    let (s, r) = evaluate(
        &e,
        json!({"point": "request", "state": "My charger is smoking!"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["outcome"], "fired");
    assert_eq!(r["action"], "handoff");
    assert_eq!(r["fired_keys"], json!(["emergency", "topic"]));
    assert!(r["message"].as_str().unwrap().contains("112"), "{r}");
    assert_eq!(r["answers"]["emergency"]["fired"], true);
    assert_eq!(r["answers"]["frustration"]["fired"], false);
    assert_eq!(r["has_closed"], true);
    // One call per text, with the token and the bound model: emergency and bereavement (0027) read the customer's
    // words, frustration and topic the request text; credentials_shared (0018: the customer's words, precheck
    // digits/code words) is not asked about this message
    let calls = laya.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for c in &calls {
        assert_eq!(c.0, format!("Bearer {LAYA_TOKEN}"));
        assert_eq!(c.1["model"], "jev-latest");
    }
    let call_with = |key: &str| {
        calls
            .iter()
            .find(|c| c.1["questions"].as_object().unwrap().contains_key(key))
            .unwrap()
            .1
            .clone()
    };
    let own = call_with("emergency");
    assert_eq!(own["state"], "My charger is smoking!");
    let own = own["questions"].as_object().unwrap();
    assert_eq!(own.len(), 2);
    assert!(own.contains_key("bereavement"));
    let rest = call_with("frustration");
    assert_eq!(rest["state"], "Customer message: My charger is smoking!");
    let rest = rest["questions"].as_object().unwrap();
    assert_eq!(rest.len(), 2);
    assert!(rest.contains_key("topic"));
    assert_eq!(
        r["answers"]["credentials_shared"],
        json!({"type": "choice", "fired": false, "skipped": "precheck"})
    );
    assert_eq!(rest["frustration"]["type"], "score");
    assert_eq!(own["emergency"]["criteria"].as_object().unwrap().len(), 2);
    assert!(!r.to_string().contains(LAYA_TOKEN));

    // block beats annotate (credentials_shared: choice `yes` at a confidence over its 0.6 threshold)
    laya.set(Reply::Answers(with(
        with(
            with(answers(), "emergency", "noul", json!(0.1)),
            "credentials_shared",
            "choice",
            json!("yes"),
        ),
        "credentials_shared",
        "confidence",
        json!(0.8),
    )));
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "my password is hunter2"}),
    )
    .await;
    assert_eq!(r["action"], "block");
    assert_eq!(r["fired_keys"], json!(["credentials_shared", "topic"]));

    // annotate alone: fired, action annotate (does not stop anything)
    laya.set(Reply::Answers(with(
        answers(),
        "emergency",
        "noul",
        json!(0.1),
    )));
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "where is my order"}),
    )
    .await;
    assert_eq!(
        (r["outcome"].as_str(), r["action"].as_str()),
        (Some("fired"), Some("annotate"))
    );

    // lt: consent below 0.5 blocks the refund
    laya.set(Reply::Answers(with(
        answers(),
        "customer_consent",
        "noul",
        json!(0.2),
    )));
    let (_, r) = evaluate(
        &e,
        json!({"point": "tool", "state": "where is it", "tool": "order_issue_refund"}),
    )
    .await;
    assert_eq!(r["outcome"], "fired", "{r}");
    assert_eq!(r["action"], "block");
    // a tool no question covers: nothing to ask, no call
    let before = laya.calls().len();
    let (_, r) = evaluate(
        &e,
        json!({"point": "tool", "state": "x", "tool": "order_get_order"}),
    )
    .await;
    assert_eq!(r["outcome"], "pass");
    assert_eq!(laya.calls().len(), before);

    // in: topic fires only for the listed options
    let (s, q) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/gates/topic"),
            Some(&admin()),
            json!({"point": "request", "type": "choice", "instructions": "What is it about?",
                   "criteria": {"delivery": "an order on its way", "warranty": "a broken product"},
                   "fire_when": "in", "threshold": null, "fire_values": ["warranty"], "action": "handoff",
                   "message": "Warranty desk.", "fail": "open", "tools": null, "enabled": true, "ordinal": 4}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{q}");
    laya.set(Reply::Answers(with(
        answers(),
        "emergency",
        "noul",
        json!(0.1),
    )));
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "where is my order"}),
    )
    .await;
    assert_eq!(r["outcome"], "pass", "{r}");
    laya.set(Reply::Answers(with(
        with(answers(), "emergency", "noul", json!(0.1)),
        "topic",
        "choice",
        json!("warranty"),
    )));
    let (_, r) = evaluate(&e, json!({"point": "request", "state": "it broke"})).await;
    assert_eq!(
        (r["outcome"].as_str(), r["message"].as_str()),
        (Some("fired"), Some("Warranty desk."))
    );
}

/// `in` with a threshold (0017's calibrated `credentials_shared`: choice `yes`, minimum confidence 0.6): fires only
/// for a listed option whose confidence reaches the threshold.
#[tokio::test]
async fn an_in_rule_with_a_threshold_needs_the_option_and_the_confidence() {
    let e = env("gates-in-threshold").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/gates"), Some(&user())),
    )
    .await;
    let seeded = g["questions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["key"] == "credentials_shared")
        .unwrap()
        .clone();
    assert_eq!(
        (
            &seeded["type"],
            &seeded["fire_when"],
            &seeded["threshold"],
            &seeded["fire_values"]
        ),
        (&json!("choice"), &json!("in"), &json!(0.6), &json!(["yes"])),
        "{seeded}"
    );
    assert_eq!(seeded["input"], "message", "{seeded}");
    assert!(seeded["precheck"].as_str().is_some(), "{seeded}");
    let creds = |choice: &str, confidence: f64| {
        with(
            with(
                with(answers(), "emergency", "noul", json!(0.1)),
                "credentials_shared",
                "choice",
                json!(choice),
            ),
            "credentials_shared",
            "confidence",
            json!(confidence),
        )
    };
    let fired = |r: &Value| r["answers"]["credentials_shared"]["fired"].clone();

    laya.set(Reply::Answers(creds("yes", 0.74)));
    let (s, r) = evaluate(&e, json!({"point": "request", "state": "my PIN is 4821"})).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(fired(&r), true, "{r}");
    assert_eq!(r["action"], "block", "{r}");
    assert_eq!(r["fired_keys"], json!(["credentials_shared", "topic"]));

    laya.set(Reply::Answers(creds("yes", 0.48)));
    let (_, r) = evaluate(&e, json!({"point": "request", "state": "my order is 4821"})).await;
    assert_eq!(fired(&r), false, "{r}");
    assert_eq!(r["fired_keys"], json!(["topic"]));

    laya.set(Reply::Answers(creds("no", 0.97)));
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "where is my order"}),
    )
    .await;
    assert_eq!(fired(&r), false, "{r}");
    assert_eq!(r["fired_keys"], json!(["topic"]));

    // the API stores `in` with a threshold, and refuses one outside 0..1
    let body = |threshold: Value| {
        json!({"point": "request", "type": "choice", "instructions": "Is there a secret?",
               "criteria": {"yes": "a secret", "no": "no secret"}, "fire_when": "in", "threshold": threshold,
               "fire_values": ["yes"], "action": "block", "message": null, "fail": "closed", "tools": null,
               "enabled": true, "ordinal": 9})
    };
    let (s, q) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/gates/secret"),
            Some(&admin()),
            body(json!(0.6)),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{q}");
    assert_eq!(q["threshold"], 0.6);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/gates/secret"),
            Some(&admin()),
            body(json!(1.5)),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "invalid_rule");
}

/// 0018: a question reads its `input` (the customer's own words, or the request text), and is asked only when its
/// `precheck` matches that text; questions are asked in one laya call per distinct text.
#[tokio::test]
async fn a_precheck_keeps_a_question_from_laya_and_input_picks_its_text() {
    let e = env("gates-precheck").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;
    let quiet = || with(answers(), "emergency", "noul", json!(0.1));
    let creds_yes = || {
        with(
            with(quiet(), "credentials_shared", "choice", json!("yes")),
            "credentials_shared",
            "confidence",
            json!(0.87),
        )
    };
    let has = |call: &Value, key: &str| call["questions"].as_object().unwrap().contains_key(key);

    // no match ("where is my order?", the message verdicts 25–27 blocked): credentials_shared is never sent to
    // laya, even with laya ready to answer `yes` 0.87, and the turn passes it
    laya.set(Reply::Answers(creds_yes()));
    let (s, r) = evaluate(
        &e,
        json!({"point": "request", "state": "where is my order?"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let calls = laya.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|c| !has(&c.1, "credentials_shared")));
    assert_eq!(
        r["answers"]["credentials_shared"],
        json!({"type": "choice", "fired": false, "skipped": "precheck"})
    );
    assert_eq!(r["fired_keys"], json!(["topic"]), "{r}");
    assert_eq!(r["action"], "annotate");
    assert_eq!(r["texts"]["credentials_shared"], "where is my order?");
    assert_eq!(r["texts"]["emergency"], "where is my order?");
    assert_eq!(r["texts"]["bereavement"], "where is my order?");
    assert_eq!(
        r["texts"]["frustration"],
        "Customer message: where is my order?"
    );

    // a match: asked with exactly the customer's words, in the call of the other questions that read them
    // (emergency and bereavement, 0027); the request-text questions get a call of their own (mixed inputs: two)
    let before = laya.calls().len();
    let (_, r) = evaluate(&e, json!({"point": "request", "state": "my PIN is 4821"})).await;
    let calls = laya.calls()[before..].to_vec();
    assert_eq!(calls.len(), 2, "{calls:?}");
    let own = calls
        .iter()
        .find(|c| has(&c.1, "credentials_shared"))
        .unwrap();
    assert_eq!(own.1["state"], "my PIN is 4821");
    assert_eq!(own.1["questions"].as_object().unwrap().len(), 3);
    assert!(has(&own.1, "emergency") && has(&own.1, "bereavement"));
    let rest = calls
        .iter()
        .find(|c| !has(&c.1, "credentials_shared"))
        .unwrap();
    assert_eq!(rest.1["state"], "Customer message: my PIN is 4821");
    assert_eq!(rest.1["questions"].as_object().unwrap().len(), 2);
    assert_eq!(r["action"], "block", "{r}");
    assert_eq!(r["fired_keys"], json!(["credentials_shared", "topic"]));

    // the playground builds the request text a live turn builds from a draft; the message stays bare
    let before = laya.calls().len();
    let (s, r) = evaluate(
        &e,
        json!({"point": "request", "state": "ORD-1042",
               "request": {"type": "delivery", "first_message": "where is my order?",
                           "details": {"order_id": "ORD-1042"}}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        r["texts"]["frustration"],
        "Customer message: ORD-1042\nFirst message of this request: where is my order?\nRequest type: delivery\nDetails so far: {\"order_id\":\"ORD-1042\"}"
    );
    assert_eq!(r["texts"]["credentials_shared"], "ORD-1042");
    assert_eq!(r["texts"]["emergency"], "ORD-1042");
    assert_eq!(laya.calls().len(), before + 2);
    // `request` is the request point's only
    let (s, b) = evaluate(
        &e,
        json!({"point": "response", "state": "Refund issued.", "request": {"type": "delivery"}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "invalid_request");

    // every question skipped: no laya call at all, a pass
    sqlx::query(
        "UPDATE gate_questions SET enabled = false WHERE point = 'request' AND key <> 'credentials_shared'",
    )
    .execute(crate::common::pool(&e))
    .await
    .unwrap();
    let before = laya.calls().len();
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "where is my order?"}),
    )
    .await;
    assert_eq!(laya.calls().len(), before);
    assert_eq!(r["outcome"], "pass", "{r}");
    assert_eq!(r["answers"]["credentials_shared"]["skipped"], "precheck");
    // a laya failure while only open questions are asked passes, though the skipped one is fail-closed
    sqlx::query("UPDATE gate_questions SET enabled = true WHERE key = 'topic'")
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    laya.set(Reply::Status(503));
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "where is my order?"}),
    )
    .await;
    assert_eq!(r["outcome"], "failed_open", "{r}");
    assert_eq!(r["has_closed"], true);
}

/// A live turn: `input = 'message'` questions get exactly what the customer wrote, not the wrapped request.
#[tokio::test]
async fn a_live_turn_sends_the_bare_message_to_message_questions() {
    let e = env("gates-input-turn").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;
    laya.set(Reply::Answers(with(
        with(answers(), "emergency", "noul", json!(0.1)),
        "topic",
        "confidence",
        json!(0.2),
    )));
    let t = user();
    let (_, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let text = "Where is my airfryer? It is order 10421";
    let (s, m) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": text}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{m}");
    let calls = laya.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    let own = calls
        .iter()
        .find(|c| {
            c.1["questions"]
                .as_object()
                .unwrap()
                .contains_key("credentials_shared")
        })
        .unwrap();
    assert_eq!(own.1["state"], text);
    let rest = calls
        .iter()
        .find(|c| {
            !c.1["questions"]
                .as_object()
                .unwrap()
                .contains_key("credentials_shared")
        })
        .unwrap();
    assert!(
        rest.1["state"]
            .as_str()
            .unwrap()
            .starts_with(&format!("Customer message: {text}")),
        "{}",
        rest.1["state"]
    );
}

/// 0027 (verdict 31, live 2026-10-24): a refund question got the emergency "call 112" handoff because `emergency`
/// and `bereavement` were asked about the request wrapper (`Request type: faq`, `Details: {...}`) — 0.9768 / 0.9342
/// on it against 0.0044 / 0.0247 on the customer's words. On a live turn that qualifies a request they read exactly
/// what the customer wrote; the request-text questions still get the wrapper.
#[tokio::test]
async fn emergency_and_bereavement_read_the_customer_message_not_the_request_wrapper() {
    let e = env("gates-emotion-message").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;
    laya.set(Reply::Answers(with(
        with(answers(), "emergency", "noul", json!(0.1)),
        "topic",
        "confidence",
        json!(0.2),
    )));
    let t = user();
    let (_, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let text = "How long does a refund take?";
    let (s, m) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": text}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{m}");
    assert!(!m["text"].as_str().unwrap_or("").contains("112"), "{m}");
    let calls = laya.calls();
    let call_with = |key: &str| {
        let found: Vec<&Value> = calls
            .iter()
            .map(|c| &c.1)
            .filter(|b| b["questions"].as_object().unwrap().contains_key(key))
            .collect();
        assert_eq!(found.len(), 1, "{key} is asked once: {calls:?}");
        found[0].clone()
    };
    for key in ["emergency", "bereavement"] {
        let state = call_with(key)["state"].as_str().unwrap().to_string();
        assert_eq!(state, text, "{key} reads the customer's words");
        for wrapper in [
            "Customer message:",
            "First message of this request:",
            "Request type:",
            "Details",
        ] {
            assert!(!state.contains(wrapper), "{key} got {wrapper:?}: {state}");
        }
    }
    // the request-text questions still read the request this turn qualified
    let wrapped = call_with("frustration")["state"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        wrapped.starts_with(&format!("Customer message: {text}\n")),
        "{wrapped}"
    );
    assert!(wrapped.contains("\nRequest type: faq\n"), "{wrapped}");
    assert!(wrapped.contains("\nDetails"), "{wrapped}");
}

#[tokio::test]
async fn a_failed_call_fails_closed_to_a_handoff_or_open_to_a_pass() {
    let e = env("gates-fail").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;

    // The seeded response questions are disabled (0017: laya System-1 cannot compare a reply with tool outcomes);
    // enabled again here, they are the fail-closed handoff questions this test needs.
    sqlx::query("UPDATE gate_questions SET enabled = true WHERE point = 'response'")
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    // 503: the response point's questions are fail-closed => handoff with the first closed handoff's message
    laya.set(Reply::Status(503));
    let (_, r) = evaluate(&e, json!({"point": "response", "state": "Refund issued."})).await;
    assert_eq!(r["outcome"], "failed_closed", "{r}");
    assert_eq!(r["action"], "handoff");
    assert!(
        r["message"]
            .as_str()
            .unwrap()
            .contains("accurate information")
    );
    assert_eq!(r["error"], "laya answered HTTP 503");
    assert_eq!(r["answers"], Value::Null);

    // slower than 1500 ms: a timeout, also closed
    laya.set(Reply::Slow(2500));
    let started = std::time::Instant::now();
    let (_, r) = evaluate(&e, json!({"point": "response", "state": "Refund issued."})).await;
    assert!(started.elapsed() < std::time::Duration::from_millis(2400));
    assert_eq!(r["outcome"], "failed_closed", "{r}");
    assert!(r["error"].as_str().unwrap().contains("timed out"), "{r}");

    // only fail-open questions left at the request point: a failure passes
    sqlx::query(
        "UPDATE gate_questions SET enabled = false WHERE point = 'request' AND fail = 'closed'",
    )
    .execute(crate::common::pool(&e))
    .await
    .unwrap();
    laya.set(Reply::Status(503));
    let (_, r) = evaluate(&e, json!({"point": "request", "state": "hello"})).await;
    assert_eq!(r["outcome"], "failed_open", "{r}");
    assert_eq!(r["action"], Value::Null);
    assert_eq!(r["has_closed"], false);
}

#[tokio::test]
async fn unbound_gates_are_skipped_and_logged() {
    let e = env("gates-skip").await;
    let (s, r) = evaluate(&e, json!({"point": "request", "state": "hello"})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["outcome"], "skipped");
    assert_eq!(r["action"], Value::Null);
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/gates"), Some(&user())),
    )
    .await;
    assert_eq!(g["provider"], json!({"bound": false, "model": null}));
    assert_eq!(g["questions"].as_array().unwrap().len(), 8);
    let (_, v) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/gates/verdicts"), Some(&user())),
    )
    .await;
    assert_eq!(v["items"][0]["outcome"], "skipped");
    assert_eq!(v["items"][0]["point"], "request");
}

#[tokio::test]
async fn the_request_gate_stops_publishing_and_replies_with_the_message() {
    let e = env("gates-turn").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;
    let t = user();
    let bus = boot_bus(&e);
    let customer = bus.streams.customer("logistics");
    let xlen = |bus: carebus::Bus, key: String| async move {
        let n: i64 = redis::cmd("XLEN")
            .arg(key)
            .query_async(&mut bus.conn())
            .await
            .unwrap();
        n
    };

    // emergency fires: the turn answers with its message and nothing reaches the bus
    let (_, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let (s, m) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": "Where is my airfryer?"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{m}");
    assert_eq!(m["turn"], "ask");
    assert!(m["text"].as_str().unwrap().contains("112"), "{m}");
    assert_eq!(m["case_id"], Value::Null);
    assert_eq!(xlen(bus.clone(), customer.clone()).await, 0);
    let logged: (String, Option<String>) = sqlx::query_as(
        "SELECT outcome, conversation_id FROM gate_verdicts WHERE point = 'request' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(logged, ("fired".to_string(), Some(id.clone())));

    // only topic fires (annotate): published, with the answers on the qualified request
    laya.set(Reply::Answers(with(
        answers(),
        "emergency",
        "noul",
        json!(0.1),
    )));
    let (_, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let (_, m) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": "Where is my airfryer?"}),
        ),
    )
    .await;
    assert_eq!(m["turn"], "published", "{m}");
    let reply: redis::streams::StreamRangeReply = redis::cmd("XRANGE")
        .arg(&customer)
        .arg("-")
        .arg("+")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(reply.ids.len(), 1);
    let env: Value =
        serde_json::from_str(&reply.ids[0].get::<String>("envelope").unwrap()).unwrap();
    assert_eq!(env["payload"]["gates"]["fired_keys"], json!(["topic"]));
    assert_eq!(
        env["payload"]["gates"]["answers"]["topic"]["choice"],
        "delivery"
    );

    // nothing fires: published without a `gates` field
    laya.set(Reply::Answers(with(
        with(answers(), "emergency", "noul", json!(0.1)),
        "topic",
        "confidence",
        json!(0.2),
    )));
    let (_, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let (_, m) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": "Where is my airfryer?"}),
        ),
    )
    .await;
    assert_eq!(m["turn"], "published", "{m}");
    assert_eq!(xlen(bus.clone(), customer.clone()).await, 2);
}

#[tokio::test]
async fn the_request_gate_runs_on_a_turn_that_is_not_complete_yet() {
    let e = env("gates-incomplete").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;
    let t = user();
    let (_, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    // a return with neither order nor reason: the turn would ask for a field, but emergency fires first
    let (s, m) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": "I want to return it, it is smoking and I smell gas"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{m}");
    assert!(
        m["text"].as_str().unwrap().contains("112"),
        "handed off: {m}"
    );
    assert_eq!(m["case_id"], Value::Null);
    let calls = laya.calls();
    // two calls (0027): the customer's words for emergency/bereavement, the request so far for the others
    assert_eq!(calls.len(), 2, "the gate asked laya on the incomplete turn");
    assert!(
        calls
            .iter()
            .all(|c| c.1["state"].as_str().unwrap().contains("I smell gas")),
        "{calls:?}"
    );
    let logged: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT outcome, conversation_id, case_id FROM gate_verdicts WHERE point = 'request' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(logged, ("fired".to_string(), Some(id.clone()), None));
    // nothing fires: the same kind of turn asks for the missing field as before
    laya.set(Reply::Answers(with(
        with(answers(), "emergency", "noul", json!(0.1)),
        "topic",
        "confidence",
        json!(0.2),
    )));
    let (_, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let (_, m) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": "I want to return it"}),
        ),
    )
    .await;
    assert_eq!(m["turn"], "ask", "{m}");
    assert!(!m["text"].as_str().unwrap().contains("112"), "{m}");
    assert_eq!(laya.calls().len(), 4);
}

#[tokio::test]
async fn questions_are_validated_and_only_admins_write() {
    let e = env("gates-validate").await;
    let base = json!({"point": "request", "type": "noul", "instructions": "Is it urgent?",
                      "criteria": {"true": "urgent", "false": "not urgent"}, "fire_when": "gte", "threshold": 0.7,
                      "fire_values": null, "action": "handoff", "message": null, "fail": "closed", "tools": null,
                      "enabled": true, "ordinal": 9});
    let put = |key: &str, body: Value, token: String| {
        crate::common::json(
            "PUT",
            &crate::common::j(&format!("/gates/{key}")),
            Some(&token),
            body,
        )
    };
    let cases: Vec<(&str, Value, &str)> = vec![
        ("Bad-Key", base.clone(), "invalid_key"),
        (
            "q",
            with(base.clone(), "criteria", "maybe", json!("x")),
            "invalid_criteria",
        ),
        (
            "q",
            json!({"type": "choice", "criteria": {"only": "one"}, "fire_when": "gte", "threshold": 0.5}),
            "invalid_criteria",
        ),
        (
            "q",
            json!({"type": "score", "criteria": ["a"], "threshold": 0}),
            "invalid_criteria",
        ),
        (
            "q",
            json!({"type": "score", "criteria": ["a","b","c","d","e","f","g","h","i","j","k"], "threshold": 0}),
            "invalid_criteria",
        ),
        (
            "q",
            json!({"type": "score", "criteria": ["calm", "angry"], "threshold": 2}),
            "invalid_rule",
        ),
        ("q", json!({"threshold": 1.5}), "invalid_rule"),
        (
            "q",
            json!({"fire_when": "in", "threshold": null, "fire_values": ["true"]}),
            "invalid_rule",
        ),
        (
            "q",
            json!({"tools": ["order_issue_refund"]}),
            "invalid_tools",
        ),
        ("q", json!({"point": "elsewhere"}), "invalid_point"),
        ("q", json!({"action": "explode"}), "invalid_action"),
        ("q", json!({"input": "everything"}), "invalid_input"),
        (
            "q",
            json!({"point": "tool", "input": "message"}),
            "invalid_input",
        ),
        ("q", json!({"precheck": "(unclosed"}), "invalid_precheck"),
        (
            "q",
            json!({"precheck": "a".repeat(501)}),
            "invalid_precheck",
        ),
    ];
    for (key, patch, code) in cases {
        let mut body = base.clone();
        for (k, v) in patch.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (s, b) = crate::common::send(&e.app, put(key, body.clone(), admin())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{key} {body}: {b}");
        assert_eq!(b["error"]["code"], code, "{key} {body}: {b}");
    }
    // a tenant_user reads but cannot write, nor run the playground (a laya call on the tenant's token)
    let (s, _) = crate::common::send(&e.app, put("urgent", base.clone(), user())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/gates/evaluate"),
            Some(&user()),
            json!({"point": "request", "state": "hello"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, q) = crate::common::send(&e.app, put("urgent", base.clone(), admin())).await;
    assert_eq!(s, StatusCode::OK, "{q}");
    assert_eq!(q["key"], "urgent");
    assert_eq!(q["type"], "noul");
    assert_eq!(q["updated_by"], "u-admin");
    // without input/precheck (an older client): the request text, always asked
    assert_eq!(
        (&q["input"], &q["precheck"]),
        (&json!("request"), &Value::Null)
    );
    let mut with_precheck = base.clone();
    with_precheck["input"] = json!("message");
    with_precheck["precheck"] = json!(r" (?i)\burgent ");
    let (s, q) = crate::common::send(&e.app, put("urgent", with_precheck, admin())).await;
    assert_eq!(s, StatusCode::OK, "{q}");
    assert_eq!(
        (&q["input"], &q["precheck"]),
        (&json!("message"), &json!(r"(?i)\burgent"))
    );
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/gates"), Some(&user())),
    )
    .await;
    assert_eq!(g["questions"].as_array().unwrap().len(), 9);
    let del = |token: String| {
        crate::common::json(
            "DELETE",
            &crate::common::j("/gates/urgent"),
            Some(&token),
            json!({}),
        )
    };
    let (s, _) = crate::common::send(&e.app, del(user())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = crate::common::send(&e.app, del(admin())).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = crate::common::send(&e.app, del(admin())).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn gates_are_isolated_per_tenant() {
    let e = env("gates-rls").await;
    let other =
        crate::common::insert_tenant(crate::common::pool(&e), crate::common::OTHER_TENANT).await;
    let other_job =
        crate::common::insert_job(crate::common::pool(&e), other, crate::common::BOOT_JOB).await;
    let boot: uuid::Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let count = |tenant: uuid::Uuid, job: Option<uuid::Uuid>, table: &'static str| {
        let db = crate::common::app_pool(&e).clone();
        async move {
            let mut tx = carebus_api::tenancy::begin_scoped(&db, tenant, job)
                .await
                .unwrap();
            let sql = if table == "q" {
                "SELECT count(*) FROM gate_questions"
            } else {
                "SELECT count(*) FROM gate_verdicts"
            };
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&mut *tx)
                .await
                .unwrap()
        }
    };
    assert_eq!(count(boot, None, "q").await, 8);
    assert_eq!(count(other, None, "q").await, 0);
    assert_eq!(count(other, Some(other_job), "q").await, 0);
    // a verdict written in the bootstrap tenant is invisible to the other one
    evaluate(&e, json!({"point": "request", "state": "hello"})).await;
    assert_eq!(count(boot, None, "v").await, 1);
    assert_eq!(count(other, None, "v").await, 0);
    // and the other tenant cannot write into the bootstrap tenant's rows
    let mut tx =
        carebus_api::tenancy::begin_scoped(crate::common::app_pool(&e), other, Some(other_job))
            .await
            .unwrap();
    let hit = sqlx::query("UPDATE gate_questions SET enabled = false")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(hit, 0);
}

#[tokio::test]
async fn the_internal_route_takes_only_the_service_token() {
    let e = env("gates-internal").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;
    let body = json!({"tenant_id": crate::common::BOOT_TENANT, "job": crate::common::BOOT_JOB, "point": "tool",
                      "state": "refund me", "case_id": "CASE-0000aaaa",
                      "tool": "order_issue_refund"});
    let (s, _) = internal(&e, None, body.clone()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, b) = internal(&e, Some(&admin()), body.clone()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, r) = internal(&e, Some(SERVICE), body.clone()).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["outcome"], "pass");
    assert_eq!(r["has_closed"], true);
    let (case,): (Option<String>,) = sqlx::query_as(
        "SELECT case_id FROM gate_verdicts WHERE point = 'tool' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(case.as_deref(), Some("CASE-0000aaaa"));
    // a service token is still read-only everywhere else
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::json(
                "PUT",
                &crate::common::j("/gates/x"),
                Some(SERVICE),
                json!({}),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let mut unknown = body.clone();
    unknown["job"] = json!("no_such_job");
    let (s, _) = internal(&e, Some(SERVICE), unknown).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let mut extra = body;
    extra["model"] = json!("x");
    let (s, _) = internal(&e, Some(SERVICE), extra).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// GET /gates, a playground evaluation and the verdict log as the web reads them
/// (web/src/lib/api/fixtures/gates/*.json).
#[tokio::test]
async fn contract_fixtures_for_the_gates_page() {
    let e = env("gates-fixtures").await;
    let laya = laya_stub().await;
    bind_laya(&e, &laya.url).await;
    let (s, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/gates"), Some(&user())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert_eq!(g["questions"][0]["key"], "emergency");
    assert_eq!(g["provider"], json!({"bound": true, "model": "jev-latest"}));
    crate::common::fixture("gates/gates", &g);
    let (s, r) = evaluate(
        &e,
        json!({"point": "request", "state": "My charger is smoking!"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    crate::common::fixture("gates/evaluation", &r);
    let (s, v) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/gates/verdicts?limit=1"), Some(&user())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["items"][0]["fired_keys"], json!(["emergency", "topic"]));
    crate::common::fixture("gates/verdicts", &v);
}

/// The licences bundle minter (Keycloak SA token + `bundle-token`) and a platform laya that accepts only the tenant's
/// latest minted laya licence, on one port. `refuse_next` laya calls answer 401 whatever the token.
#[derive(Default)]
struct Platform {
    /// (tenant, app, bundle) of every mint
    mints: Vec<(String, String, Option<String>)>,
    /// tenant -> licences status to refuse its mint with
    refuse_mint: std::collections::HashMap<String, u16>,
    /// Authorization header of every laya call
    laya_auth: Vec<String>,
    refuse_next: usize,
}

type Plat = Arc<Mutex<Platform>>;

async fn platform() -> (String, Plat) {
    use axum::extract::{Path, Query, State};
    use std::collections::HashMap;
    let m: Plat = Arc::new(Mutex::new(Platform::default()));
    let router = Router::new()
        .route(
            "/token",
            post(|| async { axum::Json(json!({"access_token": "minter-sa", "expires_in": 300})) }),
        )
        .route(
            "/lic/internal/tenants/{tenant}/bundle-token",
            post(
                |State(m): State<Plat>,
                 Path(tenant): Path<String>,
                 Query(q): Query<HashMap<String, String>>| async move {
                    let app = q.get("app").cloned().unwrap_or_default();
                    let mut g = m.lock().unwrap();
                    if let Some(code) = g.refuse_mint.get(&tenant).copied() {
                        return (
                            StatusCode::from_u16(code).unwrap(),
                            axum::Json(json!({"detail": {"error": "app_not_in_bundle"}})),
                        )
                            .into_response();
                    }
                    g.mints.push((tenant.clone(), app.clone(), q.get("bundle").cloned()));
                    let n = g.mints.len();
                    let expires = chrono::Utc::now() + chrono::Duration::minutes(15);
                    (
                        StatusCode::CREATED,
                        axum::Json(json!({
                            "token": format!("tok:{tenant}:{app}:{n}"),
                            "expires_at": expires.to_rfc3339(),
                            "tenant_id": tenant, "bundle": "care-bus", "app": app,
                            "license_id": "lic-laya", "token_id": format!("t-{n}"),
                        })),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/laya/v1/systemone",
            post(
                |State(m): State<Plat>, h: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let mut g = m.lock().unwrap();
                    g.laya_auth.push(auth.clone());
                    let latest = g
                        .mints
                        .iter()
                        .rposition(|(_, app, _)| app == "laya")
                        .map(|i| {
                            let (t, a, _) = &g.mints[i];
                            format!("Bearer tok:{t}:{a}:{}", i + 1)
                        });
                    if g.refuse_next > 0 || latest.as_deref() != Some(auth.as_str()) {
                        g.refuse_next = g.refuse_next.saturating_sub(1);
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let all = answers();
                    let asked: serde_json::Map<String, Value> = body["questions"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .filter_map(|k| all.get(k).map(|a| (k.clone(), a.clone())))
                        .collect();
                    axum::Json(json!({"model": body["model"], "answers": asked})).into_response()
                },
            ),
        )
        .with_state(m.clone());
    (crate::common::fakes::serve(router).await, m)
}

/// A failed laya call, under whichever fail policy the asked questions have.
fn failed(r: &Value) -> bool {
    matches!(r["outcome"].as_str(), Some("failed_closed" | "failed_open"))
}

/// [`env`] with the licences bundle minter on, pointed at [`platform`].
async fn env_minting(test: &str, base: &str) -> crate::common::TestEnv {
    let base = base.to_string();
    crate::common::env_with(test, move |c| {
        c.source_fetch_allow_loopback = true;
        c.strata_bundle = Some(carebus_api::strata_tokens::BundleTokenConfig {
            url: format!("{base}/lic"),
            creds: carebus_api::signup::config::ClientCredentials {
                token_url: format!("{base}/token"),
                client_id: "care-bus-bundle-minter".into(),
                client_secret: "minter-secret".into(),
            },
            ttl_minutes: 15,
        });
    })
    .await
}

#[tokio::test]
async fn a_binding_without_a_token_asks_laya_with_the_tenants_platform_licence() {
    let (base, m) = platform().await;
    let e = env_minting("gates-laya-minted", &base).await;
    bind_laya_with(&e, &format!("{base}/laya"), None).await;
    for _ in 0..2 {
        let (s, r) = evaluate(
            &e,
            json!({"point": "request", "state": "My charger is smoking!"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{r}");
        assert_eq!(r["outcome"], "fired", "{r}");
        assert!(!r.to_string().contains("tok:"), "{r}");
    }
    let g = m.lock().unwrap();
    // minted once on the care-bus bundle for the tenant's own laya licence, then reused from the cache
    assert_eq!(
        g.mints,
        vec![(
            crate::common::BOOT_TENANT.to_string(),
            "laya".to_string(),
            Some("care-bus".to_string())
        )]
    );
    // two evaluations of two laya calls each (0027: the customer's words and the request text)
    assert_eq!(
        g.laya_auth,
        vec![format!("Bearer tok:{}:laya:1", crate::common::BOOT_TENANT); 4]
    );
}

#[tokio::test]
async fn a_refused_minted_licence_is_minted_again_and_asked_once_more() {
    let (base, m) = platform().await;
    let e = env_minting("gates-laya-401", &base).await;
    bind_laya_with(&e, &format!("{base}/laya"), None).await;
    m.lock().unwrap().refuse_next = 1;
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "My charger is smoking!"}),
    )
    .await;
    assert_eq!(r["outcome"], "fired", "{r}");
    {
        let g = m.lock().unwrap();
        assert_eq!(
            g.mints.len(),
            2,
            "the refused token is forgotten and re-minted"
        );
        // the evaluation's two calls (0027) both go out with the first licence, and both again with the second
        assert_eq!(
            g.laya_auth,
            vec![
                format!("Bearer tok:{}:laya:1", crate::common::BOOT_TENANT),
                format!("Bearer tok:{}:laya:1", crate::common::BOOT_TENANT),
                format!("Bearer tok:{}:laya:2", crate::common::BOOT_TENANT),
                format!("Bearer tok:{}:laya:2", crate::common::BOOT_TENANT),
            ]
        );
    }
    // once only: a second 401 is a failed call under the fail policy (both calls refused, twice)
    m.lock().unwrap().refuse_next = 4;
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "My charger is smoking!"}),
    )
    .await;
    assert!(failed(&r), "{r}");
    assert_eq!(r["error"], "laya answered HTTP 401");
    let g = m.lock().unwrap();
    assert_eq!(g.mints.len(), 3);
    assert_eq!(g.laya_auth.len(), 8);
}

#[tokio::test]
async fn a_stored_token_wins_over_the_platform_licence() {
    let (base, m) = platform().await;
    let e = env_minting("gates-laya-stored", &base).await;
    bind_laya_with(&e, &format!("{base}/laya"), Some(LAYA_TOKEN)).await;
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "My charger is smoking!"}),
    )
    .await;
    // the platform laya refuses a BYO token; a stored-token 401 is never retried with a minted one
    assert!(failed(&r), "{r}");
    assert_eq!(r["error"], "laya answered HTTP 401");
    let g = m.lock().unwrap();
    assert!(g.mints.is_empty(), "nothing minted for a BYO binding");
    assert_eq!(g.laya_auth, vec![format!("Bearer {LAYA_TOKEN}"); 2]);
}

#[tokio::test]
async fn a_tenant_without_a_laya_licence_fails_under_the_fail_policy() {
    let (base, m) = platform().await;
    let e = env_minting("gates-laya-unlicensed", &base).await;
    bind_laya_with(&e, &format!("{base}/laya"), None).await;
    m.lock()
        .unwrap()
        .refuse_mint
        .insert(crate::common::BOOT_TENANT.to_string(), 404);
    let (_, r) = evaluate(
        &e,
        json!({"point": "request", "state": "My charger is smoking!"}),
    )
    .await;
    assert!(failed(&r), "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .starts_with("laya_not_licensed_for_tenant"),
        "{r}"
    );
    assert!(
        m.lock().unwrap().laya_auth.is_empty(),
        "laya is never called"
    );
}
