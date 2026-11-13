//! Telegram channel: webhook auth, allow-list, the turn, `/new`, and the forwarder, against a stub Bot API.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use carebus_api::telegram::{self, Place, TelegramConfig};
use serde_json::{Value, json};

const SECRET: &str = "tg-test-secret_1";
const OWNER: i64 = 4242;
const CHAT: i64 = 4242;
/// A forum supergroup (made-up id): every topic is its own conversation.
const FORUM: i64 = -1001234567890;

/// Every Bot API call the stub received: (method, body).
type Calls = Arc<Mutex<Vec<(String, Value)>>>;
/// A `sendMessage` call: (chat_id, message_thread_id when present, text).
type TopicSend = (i64, Option<i64>, String);

async fn stub_api(
    State(calls): State<Calls>,
    Path((_bot, method)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    calls.lock().unwrap().push((method, body));
    Json(json!({"ok": true, "result": {"message_id": 1}}))
}

/// A stub Telegram Bot API on an ephemeral port: `POST /bot{token}/{method}`.
async fn stub_telegram() -> (String, Calls) {
    let calls: Calls = Arc::default();
    let app = Router::new()
        .route("/{bot}/{method}", post(stub_api))
        .with_state(calls.clone());
    (crate::common::fakes::serve(app).await, calls)
}

fn boot_scope() -> carebus::Scope {
    carebus::Scope {
        tenant: crate::common::BOOT_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    }
}

async fn tg_env(test: &str, api_url: &str) -> crate::common::TestEnv {
    let api_url = api_url.to_string();
    crate::common::env_with(test, move |c| {
        c.telegram = Some(TelegramConfig {
            api_url,
            bot_token: "123:test".into(),
            webhook_secret: SECRET.into(),
            webhook_url: "https://bot.example.test/api/telegram/webhook".into(),
            allowed_user_ids: vec![OWNER],
            scope: boot_scope(),
            customer_id: carebus_api::routes::conversations::DEMO_CUSTOMER.into(),
        })
    })
    .await
}

fn update(from: i64, is_bot: bool, text: Option<&str>) -> Value {
    let mut m = json!({
        "message_id": 1,
        "chat": {"id": CHAT, "type": "private"},
        "from": {"id": from, "is_bot": is_bot, "first_name": "G"},
    });
    if let Some(t) = text {
        m["text"] = json!(t);
    }
    json!({"update_id": 1, "message": m})
}

/// A text from the owner in the forum: `thread` sets `message_thread_id`, `topic` sets `is_topic_message`.
fn forum_update(thread: Option<i64>, topic: bool, text: &str) -> Value {
    let mut m = json!({
        "message_id": 1,
        "chat": {"id": FORUM, "type": "supergroup", "is_forum": true},
        "from": {"id": OWNER, "is_bot": false, "first_name": "G"},
        "text": text,
    });
    if let Some(t) = thread {
        m["message_thread_id"] = json!(t);
    }
    if topic {
        m["is_topic_message"] = json!(true);
    }
    json!({"update_id": 1, "message": m})
}

fn topic(thread: i64) -> Place {
    Place {
        chat_id: FORUM,
        thread: Some(thread),
    }
}

fn webhook(secret: Option<&str>, body: &Value) -> Request<Body> {
    let mut b = Request::post("/api/telegram/webhook").header("content-type", "application/json");
    if let Some(s) = secret {
        b = b.header("x-telegram-bot-api-secret-token", s);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// The `sendMessage` calls so far: (chat_id, text).
fn sends(calls: &Calls) -> Vec<(i64, String)> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "sendMessage")
        .map(|(_, b)| {
            (
                b["chat_id"].as_i64().unwrap(),
                b["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The `sendMessage` calls so far, with their `message_thread_id` (`None` when the body has none).
fn topic_sends(calls: &Calls) -> Vec<TopicSend> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "sendMessage")
        .map(|(_, b)| {
            (
                b["chat_id"].as_i64().unwrap(),
                b.get("message_thread_id").map(|t| t.as_i64().unwrap()),
                b["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn wait_topic_sends(calls: &Calls, n: usize) -> Vec<TopicSend> {
    for _ in 0..200 {
        let s = topic_sends(calls);
        if s.len() >= n {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "expected {n} sendMessage calls, got {:?}",
        topic_sends(calls)
    );
}

async fn wait_sends(calls: &Calls, n: usize) -> Vec<(i64, String)> {
    for _ in 0..200 {
        let s = sends(calls);
        if s.len() >= n {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("expected {n} sendMessage calls, got {:?}", sends(calls));
}

/// Start `n` forwarders (replicas) and wait until each is subscribed to the scope's chat channel.
async fn forwarders(e: &crate::common::TestEnv, n: usize) -> Vec<tokio::task::JoinHandle<()>> {
    let handles: Vec<_> = (0..n)
        .map(|_| telegram::spawn_forwarder(e.state.clone()).expect("telegram is on"))
        .collect();
    let bus = e.state.bus.scoped(&boot_scope());
    let channel = carebus::chat::chat_channel(&bus);
    for _ in 0..200 {
        let (_, subs): (String, usize) = redis::cmd("PUBSUB")
            .arg("NUMSUB")
            .arg(&channel)
            .query_async(&mut bus.conn())
            .await
            .unwrap();
        if subs >= n {
            return handles;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the forwarders never subscribed");
}

async fn mapped_at(e: &crate::common::TestEnv, place: Place) -> Option<String> {
    let bus = e.state.bus.scoped(&boot_scope());
    redis::cmd("GET")
        .arg(telegram::chat_key(&bus, place))
        .query_async(&mut bus.conn())
        .await
        .unwrap()
}

async fn mapped_conversation(e: &crate::common::TestEnv) -> Option<String> {
    mapped_at(
        e,
        Place {
            chat_id: CHAT,
            thread: None,
        },
    )
    .await
}

async fn wait_mapped_at(e: &crate::common::TestEnv, place: Place) -> String {
    for _ in 0..200 {
        if let Some(id) = mapped_at(e, place).await {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{place} never got a conversation");
}

async fn wait_mapped(e: &crate::common::TestEnv) -> String {
    wait_mapped_at(
        e,
        Place {
            chat_id: CHAT,
            thread: None,
        },
    )
    .await
}

/// The reverse key's value (the place the forwarder sends to).
async fn reverse(e: &crate::common::TestEnv, id: &str) -> Option<String> {
    let bus = e.state.bus.scoped(&boot_scope());
    redis::cmd("GET")
        .arg(telegram::conv_key(&bus, id))
        .query_async(&mut bus.conn())
        .await
        .unwrap()
}

async fn log(e: &crate::common::TestEnv, id: &str) -> Vec<carebus::chat::ChatEvent> {
    let bus = e.state.bus.scoped(&boot_scope());
    let raw: Vec<String> = redis::cmd("LRANGE")
        .arg(carebus::chat::log_key(&bus, id))
        .arg(0)
        .arg(-1)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    raw.iter()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect()
}

async fn wait_agent_reply(e: &crate::common::TestEnv, id: &str) -> String {
    for _ in 0..200 {
        if let Some(ev) = log(e, id).await.into_iter().find(|m| m.role == "agent") {
            return ev.text;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("conversation {id} never got an agent reply");
}

#[tokio::test]
async fn the_webhook_is_404_when_telegram_is_off() {
    let e = crate::common::env("tg-off").await;
    assert!(e.state.cfg.telegram.is_none(), "off by default");
    assert!(telegram::spawn_forwarder(e.state.clone()).is_none());
    let body = update(OWNER, false, Some("hi"));
    let (s, b) = crate::common::send(&e.app, webhook(Some(SECRET), &body)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn a_missing_or_wrong_secret_is_401_and_runs_nothing() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-secret", &api).await;
    let body = update(OWNER, false, Some("I want to return my headphones"));
    for secret in [
        None,
        Some("wrong"),
        Some("tg-test-secret_"),
        Some("tg-test-secret_12"),
    ] {
        let (s, b) = crate::common::send(&e.app, webhook(secret, &body)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{secret:?}: {b}");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(mapped_conversation(&e).await.is_none());
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn strangers_and_bots_get_no_turn_and_non_text_gets_a_text_only_reply() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-allow", &api).await;
    for body in [
        update(999, false, Some("I want to return my headphones")),
        update(OWNER, true, Some("I want to return my headphones")),
    ] {
        let (s, _) = crate::common::send(&e.app, webhook(Some(SECRET), &body)).await;
        assert_eq!(s, StatusCode::OK, "Telegram is always acknowledged");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(mapped_conversation(&e).await.is_none(), "no conversation");
    assert!(calls.lock().unwrap().is_empty(), "no reply at all");
    // a sticker from the owner: no turn, a short text-only reply
    let (s, _) =
        crate::common::send(&e.app, webhook(Some(SECRET), &update(OWNER, false, None))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        wait_sends(&calls, 1).await,
        vec![(CHAT, telegram::TEXT_ONLY.to_string())]
    );
    assert!(mapped_conversation(&e).await.is_none());
}

#[tokio::test]
async fn a_text_runs_the_turn_and_its_reply_is_sent_once() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-turn", &api).await;
    let _f = forwarders(&e, 1).await;
    let (s, b) = crate::common::send(
        &e.app,
        webhook(
            Some(SECRET),
            &update(OWNER, false, Some("I want to return my headphones")),
        ),
    )
    .await;
    assert_eq!((s, b), (StatusCode::OK, json!({"ok": true})));
    let id = wait_mapped(&e).await;
    let reply = wait_agent_reply(&e, &id).await;
    let sent = wait_sends(&calls, 1).await;
    assert_eq!(sent, vec![(CHAT, reply)]);
    let msgs = log(&e, &id).await;
    assert_eq!(msgs[0].role, "customer");
    assert_eq!(msgs[0].text, "I want to return my headphones");
    // the reverse key the forwarder reads
    let bus = e.state.bus.scoped(&boot_scope());
    let chat: i64 = redis::cmd("GET")
        .arg(telegram::conv_key(&bus, &id))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(chat, CHAT);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        sends(&calls).len(),
        1,
        "the turn reply is sent exactly once"
    );
}

#[tokio::test]
async fn start_greets_and_new_starts_a_fresh_conversation() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-new", &api).await;
    let _f = forwarders(&e, 1).await;
    let (s, _) = crate::common::send(
        &e.app,
        webhook(Some(SECRET), &update(OWNER, false, Some("/start"))),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        wait_sends(&calls, 1).await,
        vec![(CHAT, telegram::GREETING.to_string())]
    );
    crate::common::send(
        &e.app,
        webhook(
            Some(SECRET),
            &update(OWNER, false, Some("where is my order")),
        ),
    )
    .await;
    let first = wait_mapped(&e).await;
    wait_agent_reply(&e, &first).await;
    wait_sends(&calls, 2).await;
    crate::common::send(
        &e.app,
        webhook(Some(SECRET), &update(OWNER, false, Some("/new"))),
    )
    .await;
    let sent = wait_sends(&calls, 3).await;
    assert_eq!(sent[2], (CHAT, telegram::NEW_CONVERSATION.to_string()));
    let second = mapped_conversation(&e).await.unwrap();
    assert_ne!(first, second, "a new conversation");
    let bus = e.state.bus.scoped(&boot_scope());
    let (old, new): (Option<i64>, Option<i64>) = redis::pipe()
        .get(telegram::conv_key(&bus, &first))
        .get(telegram::conv_key(&bus, &second))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!((old, new), (None, Some(CHAT)));
    // the next text goes to the new conversation
    crate::common::send(
        &e.app,
        webhook(
            Some(SECRET),
            &update(OWNER, false, Some("I want to return my headphones")),
        ),
    )
    .await;
    wait_agent_reply(&e, &second).await;
    assert_eq!(
        log(&e, &second).await[0].text,
        "I want to return my headphones"
    );
}

#[tokio::test]
async fn a_relayed_case_result_is_forwarded_once_even_with_two_replicas() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-relay", &api).await;
    let _f = forwarders(&e, 2).await;
    crate::common::send(
        &e.app,
        webhook(
            Some(SECRET),
            &update(OWNER, false, Some("I want to return my headphones")),
        ),
    )
    .await;
    let id = wait_mapped(&e).await;
    wait_agent_reply(&e, &id).await;
    wait_sends(&calls, 1).await;
    // the interaction bin relays a case result later
    let bus = e.state.bus.scoped(&boot_scope());
    carebus::chat::link_case(&bus, &id, "CASE-TG-1")
        .await
        .unwrap();
    assert!(
        carebus::chat::relay_reply(&bus, "CASE-TG-1", "Your return is approved.")
            .await
            .unwrap()
    );
    // an agent message of a console conversation is not Telegram's
    carebus::chat::append_and_publish(
        &bus,
        &carebus::chat::ChatEvent::new("conv-console", "agent", "not for telegram", None),
    )
    .await
    .unwrap();
    let sent = wait_sends(&calls, 2).await;
    assert_eq!(sent[1], (CHAT, "Your return is approved.".to_string()));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let sent = sends(&calls);
    assert_eq!(
        sent.len(),
        2,
        "each agent message once, across both replicas: {sent:?}"
    );
    assert!(sent.iter().all(|(_, t)| t != "not for telegram"));
}

#[tokio::test]
async fn a_topic_message_is_answered_in_its_topic() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-topic", &api).await;
    let _f = forwarders(&e, 1).await;
    let (s, _) = crate::common::send(
        &e.app,
        webhook(
            Some(SECRET),
            &forum_update(Some(7), true, "I want to return my headphones"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let id = wait_mapped_at(&e, topic(7)).await;
    let reply = wait_agent_reply(&e, &id).await;
    assert_eq!(
        wait_topic_sends(&calls, 1).await,
        vec![(FORUM, Some(7), reply)]
    );
    assert_eq!(reverse(&e, &id).await.as_deref(), Some("-1001234567890:7"));
    let general = Place {
        chat_id: FORUM,
        thread: None,
    };
    assert!(
        mapped_at(&e, general).await.is_none(),
        "the chat itself is not mapped"
    );
}

#[tokio::test]
async fn two_topics_of_one_chat_are_two_conversations() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-topics", &api).await;
    let _f = forwarders(&e, 1).await;
    for thread in [7, 9] {
        crate::common::send(
            &e.app,
            webhook(
                Some(SECRET),
                &forum_update(Some(thread), true, "where is my order"),
            ),
        )
        .await;
    }
    let a = wait_mapped_at(&e, topic(7)).await;
    let b = wait_mapped_at(&e, topic(9)).await;
    assert_ne!(a, b, "one conversation per topic");
    let (reply_a, reply_b) = (
        wait_agent_reply(&e, &a).await,
        wait_agent_reply(&e, &b).await,
    );
    let sent = wait_topic_sends(&calls, 2).await;
    assert!(sent.contains(&(FORUM, Some(7), reply_a)), "{sent:?}");
    assert!(sent.contains(&(FORUM, Some(9), reply_b)), "{sent:?}");
    assert_eq!(reverse(&e, &a).await.as_deref(), Some("-1001234567890:7"));
    assert_eq!(reverse(&e, &b).await.as_deref(), Some("-1001234567890:9"));
}

#[tokio::test]
async fn a_message_outside_a_topic_is_answered_without_a_thread() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-general", &api).await;
    let _f = forwarders(&e, 1).await;
    // a reply carries message_thread_id without is_topic_message: still the chat itself
    crate::common::send(
        &e.app,
        webhook(
            Some(SECRET),
            &forum_update(Some(5), false, "I want to return my headphones"),
        ),
    )
    .await;
    let general = Place {
        chat_id: FORUM,
        thread: None,
    };
    let id = wait_mapped_at(&e, general).await;
    let reply = wait_agent_reply(&e, &id).await;
    assert_eq!(
        wait_topic_sends(&calls, 1).await,
        vec![(FORUM, None, reply)]
    );
    assert_eq!(reverse(&e, &id).await.as_deref(), Some("-1001234567890"));
    assert!(mapped_at(&e, topic(5)).await.is_none());
    // the General topic (no thread fields at all) is the same conversation
    crate::common::send(
        &e.app,
        webhook(Some(SECRET), &forum_update(None, false, "/start")),
    )
    .await;
    let sent = wait_topic_sends(&calls, 2).await;
    assert_eq!(sent[1], (FORUM, None, telegram::GREETING.to_string()));
    assert_eq!(mapped_at(&e, general).await, Some(id));
}

#[tokio::test]
async fn new_in_one_topic_leaves_the_other_topic_alone() {
    let (api, calls) = stub_telegram().await;
    let e = tg_env("tg-topic-new", &api).await;
    let _f = forwarders(&e, 1).await;
    for thread in [7, 9] {
        crate::common::send(
            &e.app,
            webhook(
                Some(SECRET),
                &forum_update(Some(thread), true, "where is my order"),
            ),
        )
        .await;
    }
    let a = wait_mapped_at(&e, topic(7)).await;
    let b = wait_mapped_at(&e, topic(9)).await;
    wait_agent_reply(&e, &a).await;
    wait_agent_reply(&e, &b).await;
    wait_topic_sends(&calls, 2).await;
    crate::common::send(
        &e.app,
        webhook(Some(SECRET), &forum_update(Some(7), true, "/new")),
    )
    .await;
    let sent = wait_topic_sends(&calls, 3).await;
    assert_eq!(
        sent[2],
        (FORUM, Some(7), telegram::NEW_CONVERSATION.to_string())
    );
    let fresh = mapped_at(&e, topic(7)).await.unwrap();
    assert_ne!(fresh, a, "topic 7 has a new conversation");
    assert_eq!(reverse(&e, &a).await, None, "its old reverse key is gone");
    assert_eq!(
        reverse(&e, &fresh).await.as_deref(),
        Some("-1001234567890:7")
    );
    assert_eq!(
        mapped_at(&e, topic(9)).await,
        Some(b.clone()),
        "topic 9 keeps its conversation"
    );
    assert_eq!(reverse(&e, &b).await.as_deref(), Some("-1001234567890:9"));
}
