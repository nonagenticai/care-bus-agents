//! The operator side of a handover, over the web channel: the channel door accepts an operator's message for a
//! topic of its own tenant and job (and only an allowed operator), writes it as a `human` event the visitor's
//! events route replays, and `/auto` releases the conversation; a visitor's message while a person holds the chat
//! goes into the topic instead of running a turn. The Bot API is a local stub.
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use serde_json::{Value, json};

use carebus_api::handover::HandoverConfig;

const HEADER: &str = "x-carebus-webchat-secret";
const OPERATOR: i64 = 42;
const CHAT: i64 = -1001234567890;
const THREAD: i64 = 9;

/// A Bot API stub: records every call body and answers `ok` (`createForumTopic` mints [`THREAD`]).
async fn bot_stub() -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().fallback(post(move |axum::Json(body): axum::Json<Value>| {
        let s = s.clone();
        async move {
            s.lock().unwrap().push(body);
            axum::Json(
                json!({"ok": true, "result": {"message_thread_id": THREAD, "message_id": 1}}),
            )
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

async fn env_with_bot(test: &str) -> (crate::common::TestEnv, Arc<Mutex<Vec<Value>>>) {
    let (url, seen) = bot_stub().await;
    let e = crate::common::env_with(test, move |cfg| {
        cfg.handover = Some(HandoverConfig {
            api_url: url,
            bot_token: "123:abc".into(),
            chat_id: CHAT,
            operator_ids: vec![OPERATOR],
        });
    })
    .await;
    (e, seen)
}

fn request(method: &str, uri: &str, secret: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(secret) = secret {
        b = b.header(HEADER, secret);
    }
    match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            b.body(Body::from(v.to_string())).unwrap()
        }
        None => b.body(Body::empty()).unwrap(),
    }
}

async fn register(e: &crate::common::TestEnv) -> (String, String) {
    let (s, body) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/channels/webchat"),
            Some(&crate::common::admin()),
            json!({"label": "test"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    (
        body["key"].as_str().expect("key").to_string(),
        body["secret"].as_str().expect("secret").to_string(),
    )
}

/// A conversation with one visitor message in it; returns its id.
async fn conversation(e: &crate::common::TestEnv, key: &str, secret: &str) -> String {
    let uri = format!("/api/channel/webchat/{key}/messages");
    let (s, body) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            Some(secret),
            Some(json!({"session_key": "s1", "text": "Hello"})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    body["conversation_id"].as_str().expect("id").to_string()
}

async fn open_handover(e: &crate::common::TestEnv, id: &str) {
    let (s, body) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/internal/handover",
            Some("svc-test-token"),
            json!({"tenant_id": crate::common::BOOT_TENANT,
                   "job": crate::common::BOOT_JOB,
                   "conversation_id": id,
                   "reason": "I want to talk to George"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["thread_id"], json!(THREAD));
}

fn reply(key: &str, secret: &str, from: i64, text: &str) -> Request<Body> {
    request(
        "POST",
        &format!("/api/channel/webchat/{key}/handover/reply"),
        Some(secret),
        Some(json!({"chat_id": CHAT, "thread_id": THREAD, "from_id": from, "text": text})),
    )
}

async fn events(e: &crate::common::TestEnv, key: &str, secret: &str, id: &str) -> Vec<Value> {
    let uri = format!("/api/channel/webchat/{key}/conversations/{id}/events?after=0");
    let (s, page) = crate::common::send(&e.app, request("GET", &uri, Some(secret), None)).await;
    assert_eq!(s, StatusCode::OK, "{page}");
    page["events"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn an_operator_message_reaches_the_visitor_and_auto_releases_the_chat() {
    let (e, _seen) = env_with_bot("handover_reply").await;
    let (key, secret) = register(&e).await;
    let id = conversation(&e, &key, &secret).await;
    open_handover(&e, &id).await;

    // only an allowed operator gets through
    let (s, _) = crate::common::send(&e.app, reply(&key, &secret, 7, "not an operator")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = crate::common::send(&e.app, reply(&key, "wrong", OPERATOR, "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let (s, body) =
        crate::common::send(&e.app, reply(&key, &secret, OPERATOR, "Hello, George here")).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["released"], json!(false));
    let human = events(&e, &key, &secret, &id)
        .await
        .into_iter()
        .find(|ev| ev["role"] == "human" && ev["text"] == "Hello, George here")
        .expect("the operator's message is replayed to the visitor");
    assert!(human["seq"].as_u64().is_some(), "{human}");

    // /auto hands the conversation back to the assistant: the human lock is gone
    let (s, body) = crate::common::send(&e.app, reply(&key, &secret, OPERATOR, "/auto")).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["released"], json!(true));
    let uri = format!("/api/channel/webchat/{key}/conversations/{id}/state");
    let (s, state) = crate::common::send(&e.app, request("GET", &uri, Some(&secret), None)).await;
    assert_eq!(s, StatusCode::OK, "{state}");
    assert_eq!(state["locked"], json!(false));
}

/// The topic mapping is global across tenants and jobs, but it carries the deployment key prefix. Two
/// environments sharing one Redis (as this suite does) must not overwrite each other's mapping; gate run
/// 46168702000 lost one operator's message exactly that way.
#[tokio::test]
async fn two_environments_at_the_same_topic_keep_separate_mappings() {
    let (a, _) = env_with_bot("handover_isolation_a").await;
    let (b, _) = env_with_bot("handover_isolation_b").await;
    let (ka, sa) = register(&a).await;
    let ia = conversation(&a, &ka, &sa).await;
    open_handover(&a, &ia).await;
    let (kb, sb) = register(&b).await;
    let ib = conversation(&b, &kb, &sb).await;
    open_handover(&b, &ib).await;

    // B's handover opened last. A reply delivered on A's channel must still land in A's conversation.
    let (s, body) = crate::common::send(&a.app, reply(&ka, &sa, OPERATOR, "Hello from A")).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let a_events = events(&a, &ka, &sa, &ia).await;
    assert!(
        a_events
            .iter()
            .any(|ev| ev["role"] == "human" && ev["text"] == "Hello from A"),
        "A's operator message is replayed in A's conversation: {a_events:?}"
    );
    let b_events = events(&b, &kb, &sb, &ib).await;
    assert!(
        !b_events.iter().any(|ev| ev["text"] == "Hello from A"),
        "A's operator message never appears in B's conversation: {b_events:?}"
    );
}

#[tokio::test]
async fn a_visitor_message_while_handed_over_goes_into_the_topic() {
    let (e, seen) = env_with_bot("handover_forward").await;
    let (key, secret) = register(&e).await;
    let id = conversation(&e, &key, &secret).await;
    open_handover(&e, &id).await;
    let before = seen.lock().unwrap().len();

    let uri = format!("/api/channel/webchat/{key}/messages");
    let (s, body) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            Some(&secret),
            Some(json!({"session_key": "s1", "text": "Are you there?"})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["conversation_id"], json!(id));

    let calls = seen.lock().unwrap().clone();
    assert!(
        calls[before..]
            .iter()
            .any(|c| c["text"] == "Are you there?" && c["message_thread_id"] == json!(THREAD)),
        "the visitor's message must reach the operator's topic: {calls:?}"
    );
    let logged = events(&e, &key, &secret, &id)
        .await
        .into_iter()
        .any(|ev| ev["role"] == "customer" && ev["text"] == "Are you there?");
    assert!(
        logged,
        "the visitor's message stays in the conversation log"
    );
}
