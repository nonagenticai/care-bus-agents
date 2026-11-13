use axum::http::StatusCode;
use serde_json::json;

fn boot_scope() -> carebus::Scope {
    carebus::Scope {
        tenant: crate::common::BOOT_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    }
}

/// The bootstrap job's bus: the routes under `crate::common::j(..)` read exactly these keys.
fn boot_bus(e: &crate::common::TestEnv) -> carebus::Bus {
    e.state.bus.scoped(&boot_scope())
}

#[tokio::test]
async fn return_is_held_until_reason_then_published_with_case_link() {
    let e = crate::common::env("conv").await;
    let t = crate::common::user();
    let (s, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let url = crate::common::j(&format!("/conversations/{id}/messages"));
    let (_, a) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &url,
            Some(&t),
            json!({"text":"I want to return my headphones"}),
        ),
    )
    .await;
    assert_eq!(a["turn"], "ask");
    wait_idle(&boot_bus(&e), &id).await;
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &url,
            Some(&t),
            json!({"text":"they're too uncomfortable"}),
        ),
    )
    .await;
    assert_eq!(b["turn"], "published");
    wait_idle(&boot_bus(&e), &id).await;
    let case = b["case_id"].as_str().unwrap();
    let mut conn = boot_bus(&e).conn();
    let linked: String = redis::cmd("GET")
        .arg(carebus::chat::case_conv_key(&boot_bus(&e), case))
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(linked, id);
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    assert_eq!(
        g["messages"].as_array().unwrap().len(),
        4,
        "two customer + two agent messages"
    );
    assert_eq!(g["case_ids"][0], case);
    // the lock is released after each message
    let held: bool = redis::cmd("EXISTS")
        .arg(carebus::chat::lock_key(&boot_bus(&e), &id))
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(!held);
}

#[tokio::test]
async fn busy_message_is_queued_202_logged_at_once_and_empty_is_400() {
    let e = crate::common::env("conv-busy").await;
    let t = crate::common::user();
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
    let id = c["conversation_id"].as_str().unwrap();
    let _: () = redis::cmd("SET")
        .arg(carebus::chat::lock_key(&boot_bus(&e), id))
        .arg("x")
        .arg("PX")
        .arg(5000)
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text":"hi"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["turn"], "queued");
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text":"   "}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j("/conversations/nope"), Some(&t))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // the queued message is in the transcript at once and waits in the inbox; the other holder's lock is untouched
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    let msgs = g["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{g}");
    assert_eq!(
        (msgs[0]["role"].clone(), msgs[0]["text"].clone()),
        (json!("customer"), json!("hi"))
    );
    let inbox: Vec<String> = redis::cmd("LRANGE")
        .arg(carebus::chat::inbox_key(&boot_bus(&e), id))
        .arg(0)
        .arg(-1)
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    assert_eq!(inbox, vec!["hi".to_string()]);
    let v: String = redis::cmd("GET")
        .arg(carebus::chat::lock_key(&boot_bus(&e), id))
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    assert_eq!(v, "x");
}

/// Polls until the conversation lock is gone and its inbox is empty: every queued text was answered.
async fn wait_idle(bus: &carebus::Bus, id: &str) {
    for _ in 0..100 {
        let (held, queued): (bool, i64) = redis::pipe()
            .exists(carebus::chat::lock_key(bus, id))
            .llen(carebus::chat::inbox_key(bus, id))
            .query_async(&mut bus.conn())
            .await
            .unwrap();
        if !held && queued == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("conversation {id} never went idle");
}

#[tokio::test]
async fn messages_sent_during_a_slow_turn_are_coalesced_into_one_qualifier_call() {
    let e = crate::common::env("conv-coalesce").await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(carebus_api::chat_q::AnyQualifier::Record(
        std::time::Duration::from_millis(500),
        seen.clone(),
    ));
    let app = carebus_api::router(state);
    let t = crate::common::user();
    let (_, c) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let url = crate::common::j(&format!("/conversations/{id}/messages"));
    let req = crate::common::json("POST", &url, Some(&t), json!({"text":"first"}));
    let app2 = app.clone();
    let holder = tokio::spawn(async move { crate::common::send(&app2, req).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    for text in ["a", "b"] {
        let (s, b) = crate::common::send(
            &app,
            crate::common::json("POST", &url, Some(&t), json!({ "text": text })),
        )
        .await;
        assert_eq!(
            (s, b["turn"].clone()),
            (StatusCode::ACCEPTED, json!("queued")),
            "{b}"
        );
    }
    // both queued texts are in the transcript before the slow turn has answered
    let (_, g) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    let roles: Vec<&str> = g["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["customer", "customer", "customer"], "{g}");
    let (s, first) = holder.await.unwrap();
    assert_eq!(s, StatusCode::OK, "{first}");
    assert_ne!(first["turn"], "queued");
    wait_idle(&boot_bus(&e), &id).await;
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["first".to_string(), "a\nb".to_string()],
        "one qualifier call for the holder's text, ONE for everything queued behind it"
    );
    let (_, g) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    let roles: Vec<&str> = g["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["customer", "customer", "customer", "agent", "agent"],
        "{g}"
    );
}

#[tokio::test]
async fn a_text_queued_while_the_last_batch_is_qualified_is_still_answered() {
    let e = crate::common::env("conv-late").await;
    let t = crate::common::user();
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
    // During the holder's (only) qualification, another sender queues a text and finds the lock held: it
    // does exactly what message() does on 202 — log + queue, no drain of its own.
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (bus, cid, n) = (boot_bus(&e), id.clone(), calls.clone());
    let hook: carebus_api::chat_q::Hook = std::sync::Arc::new(move || {
        let (bus, cid, n) = (bus.clone(), cid.clone(), n.clone());
        Box::pin(async move {
            if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                carebus::chat::accept_customer(
                    &bus,
                    &carebus::chat::ChatEvent::new(&cid, "customer", "late", None),
                )
                .await
                .unwrap();
                assert!(
                    carebus::chat::acquire_lock(&bus, &cid)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
        })
    });
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(carebus_api::chat_q::AnyQualifier::Then(hook));
    let app = carebus_api::router(state);
    let (s, b) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text":"hello"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    wait_idle(&boot_bus(&e), &id).await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the late text got its own turn"
    );
    let (_, g) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    let roles: Vec<&str> = g["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["customer", "customer", "agent", "agent"], "{g}");
}

#[tokio::test]
async fn finish_turn_keeps_the_lock_while_texts_wait_and_releases_it_when_empty() {
    let e = crate::common::env("conv-finish").await;
    let bus = &boot_bus(&e);
    let token = carebus::chat::acquire_lock(bus, "cf")
        .await
        .unwrap()
        .unwrap();
    // a text lands after the holder's last drain but before it lets go
    carebus::chat::accept_customer(
        bus,
        &carebus::chat::ChatEvent::new("cf", "customer", "x", None),
    )
    .await
    .unwrap();
    assert_eq!(
        carebus::chat::finish_turn(bus, "cf", &token).await.unwrap(),
        carebus::chat::Finish::Pending
    );
    assert!(
        carebus::chat::acquire_lock(bus, "cf")
            .await
            .unwrap()
            .is_none(),
        "still held"
    );
    assert_eq!(
        carebus::chat::take_inbox(bus, "cf").await.unwrap(),
        vec!["x".to_string()]
    );
    assert!(
        carebus::chat::take_inbox(bus, "cf")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        carebus::chat::finish_turn(bus, "cf", "stale")
            .await
            .unwrap(),
        carebus::chat::Finish::Lost,
        "a stale token changes nothing"
    );
    assert_eq!(
        carebus::chat::finish_turn(bus, "cf", &token).await.unwrap(),
        carebus::chat::Finish::Released
    );
    assert!(
        carebus::chat::acquire_lock(bus, "cf")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn simultaneous_sends_publish_the_case_at_most_once() {
    let e = crate::common::env("conv-race").await;
    let t = crate::common::user();
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
    let url = crate::common::j(&format!("/conversations/{id}/messages"));
    let (_, a) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &url,
            Some(&t),
            json!({"text":"I want to return my headphones"}),
        ),
    )
    .await;
    assert_eq!(a["turn"], "ask");
    let body = json!({"text":"they're too uncomfortable"});
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let app = e.app.clone();
        let req = crate::common::json("POST", &url, Some(&t), body.clone());
        set.spawn(async move { crate::common::send(&app, req).await });
    }
    let mut res = Vec::new();
    while let Some(r) = set.join_next().await {
        res.push(r.unwrap());
    }
    let ok = res.iter().filter(|(s, _)| *s == StatusCode::OK).count();
    let queued = res
        .iter()
        .filter(|(s, b)| *s == StatusCode::ACCEPTED && b["turn"] == "queued")
        .count();
    assert_eq!(ok + queued, 6, "only 200 or 202 queued: {res:?}");
    wait_idle(&boot_bus(&e), &id).await;
    assert!(ok >= 1);
    let published = res.iter().filter(|(_, b)| b["turn"] == "published").count();
    assert_eq!(published, 1, "exactly one send publishes the case: {res:?}");
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    assert_eq!(g["case_ids"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn lock_release_only_deletes_the_holders_own_lock() {
    let e = crate::common::env("conv-lock").await;
    let bus = &boot_bus(&e);
    let first = carebus::chat::acquire_lock(bus, "c1")
        .await
        .unwrap()
        .unwrap();
    assert!(
        carebus::chat::acquire_lock(bus, "c1")
            .await
            .unwrap()
            .is_none()
    );
    // a stale holder (wrong token) must not delete the live lock
    assert!(
        !carebus::chat::release_lock(bus, "c1", "stale")
            .await
            .unwrap()
    );
    assert!(
        carebus::chat::acquire_lock(bus, "c1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        carebus::chat::release_lock(bus, "c1", &first)
            .await
            .unwrap()
    );
    assert!(
        carebus::chat::acquire_lock(bus, "c1")
            .await
            .unwrap()
            .is_some()
    );
    let ttl: i64 = redis::cmd("TTL")
        .arg(carebus::chat::lock_key(bus, "c1"))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let max = carebus::chat::LOCK_SECS as i64;
    assert!((1..=max).contains(&ttl), "EX {max}, got {ttl}");
}

#[tokio::test]
async fn case_history_carries_the_conversation_id_whichever_side_writes_first() {
    let e = crate::common::env("conv-hist").await;
    let recorder = carebus_api::recorder::spawn(e.state.clone(), "conv-hist-test".into());
    let t = crate::common::user();
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
    let url = crate::common::j(&format!("/conversations/{id}/messages"));
    crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &url,
            Some(&t),
            json!({"text":"I want to return my headphones"}),
        ),
    )
    .await;
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &url,
            Some(&t),
            json!({"text":"they're too uncomfortable"}),
        ),
    )
    .await;
    let case = b["case_id"].as_str().unwrap().to_string();
    let mut got: Option<Option<String>> = None;
    for _ in 0..50 {
        got = sqlx::query_scalar("SELECT conversation_id FROM case_history WHERE case_id = $1")
            .bind(&case)
            .fetch_optional(crate::common::pool(&e))
            .await
            .unwrap();
        if matches!(&got, Some(Some(_))) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    recorder.abort();
    assert_eq!(got, Some(Some(id)));
}

#[tokio::test]
async fn relay_reply_routes_to_the_linked_conversation_only() {
    let e = crate::common::env("relay").await;
    let bus = &boot_bus(&e);
    let _: () = redis::cmd("SET")
        .arg(carebus::chat::case_conv_key(bus, "CASE-X"))
        .arg("conv-1")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert!(
        carebus::chat::relay_reply(bus, "CASE-X", "Your return is approved")
            .await
            .unwrap()
    );
    assert!(
        !carebus::chat::relay_reply(bus, "CASE-REPL", "hi")
            .await
            .unwrap()
    );
    let log: Vec<String> = redis::cmd("LRANGE")
        .arg(carebus::chat::log_key(bus, "conv-1"))
        .arg(0)
        .arg(-1)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].contains("Your return is approved"));
}

#[tokio::test]
async fn daemon_reply_path_reaches_the_conversation_without_stdin() {
    let e = crate::common::env("daemon").await;
    let bus = boot_bus(&e);
    let _: () = redis::cmd("SET")
        .arg(carebus::chat::case_conv_key(&bus, "CASE-D"))
        .arg("conv-d")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert!(
        carebus::chat::deliver_reply(&bus, "CASE-D", "Your parcel arrives tomorrow")
            .await
            .unwrap()
    );
    let log: Vec<String> = redis::cmd("LRANGE")
        .arg(carebus::chat::log_key(&bus, "conv-d"))
        .arg(0)
        .arg(-1)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert!(log[0].contains("Your parcel arrives tomorrow"));
    let t0 = std::time::Instant::now();
    assert!(
        !carebus::chat::deliver_reply(&bus, "CASE-REPL", "x")
            .await
            .unwrap()
    );
    assert!(t0.elapsed() >= std::time::Duration::from_millis(1800));
}

#[tokio::test]
async fn dropping_the_request_mid_handle_releases_the_lock_and_publishes_exactly_once() {
    let e = crate::common::env_with("conv-cancel", |_| {}).await;
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(carebus_api::chat_q::AnyQualifier::Slow(
        std::time::Duration::from_millis(400),
    ));
    let app = carebus_api::router(state.clone());
    let t = crate::common::user();
    let (_, c) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let url = crate::common::j(&format!("/conversations/{id}/messages"));
    crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &url,
            Some(&t),
            json!({"text":"I want to return my headphones"}),
        ),
    )
    .await;
    // second message completes the return; drop its request while the qualifier is still running
    let req = crate::common::json(
        "POST",
        &url,
        Some(&t),
        json!({"text":"they're too uncomfortable"}),
    );
    let app2 = app.clone();
    let h = tokio::spawn(async move { crate::common::send(&app2, req).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    h.abort();
    let lock = carebus::chat::lock_key(&boot_bus(&e), &id);
    let mut released = false;
    for _ in 0..60 {
        let held: bool = redis::cmd("EXISTS")
            .arg(&lock)
            .query_async(&mut boot_bus(&e).conn())
            .await
            .unwrap();
        if !held {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(released, "lock must be released after a dropped request");
    let (_, g) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    assert_eq!(
        g["case_ids"].as_array().unwrap().len(),
        1,
        "the work ran to completion"
    );
    // draft was cleared, so another message cannot publish a second case
    let (_, again) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &url,
            Some(&t),
            json!({"text":"they're too uncomfortable"}),
        ),
    )
    .await;
    assert_ne!(again["turn"], "published");
    let (_, g) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    assert_eq!(g["case_ids"].as_array().unwrap().len(), 1);
}

/// Opens a conversation and sends the first return message, so the stored draft awaits `reason`.
async fn conversation_awaiting_reason(app: &axum::Router, t: &str) -> String {
    let (_, c) = crate::common::send(
        app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let (_, a) = crate::common::send(
        app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(t),
            json!({"text":"I want to return my headphones"}),
        ),
    )
    .await;
    assert_eq!(a["turn"], "ask");
    id
}

#[tokio::test]
async fn a_failing_qualifier_is_a_503_naming_the_llm_and_leaves_the_draft() {
    let e = crate::common::env("conv-llm-down").await;
    let t = crate::common::user();
    let id = conversation_awaiting_reason(&e.app, &t).await;
    wait_idle(&boot_bus(&e), &id).await;
    let conv = carebus::chat::conv_key(&boot_bus(&e), &id);
    let draft_before: String = redis::cmd("HGET")
        .arg(&conv)
        .arg("draft")
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(carebus_api::chat_q::AnyQualifier::Failing);
    let app = carebus_api::router(state);
    let (s, b) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text":"they're too uncomfortable"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "upstream_unavailable");
    assert_eq!(b["error"]["message"], "llm is unavailable");
    wait_idle(&boot_bus(&e), &id).await;
    let held: bool = redis::cmd("EXISTS")
        .arg(carebus::chat::lock_key(&boot_bus(&e), &id))
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    assert!(!held, "lock released");
    let draft_after: String = redis::cmd("HGET")
        .arg(&conv)
        .arg("draft")
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    assert_eq!(draft_after, draft_before, "draft unchanged");
    // and the conversation still works once the model is back
    let (s, ok) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text":"they're too uncomfortable"}),
        ),
    )
    .await;
    assert_eq!(
        (s, ok["turn"].clone()),
        (StatusCode::OK, json!("published"))
    );
}

#[tokio::test]
async fn a_published_turn_answers_published_and_links_even_when_saving_the_draft_fails() {
    let e = crate::common::env("conv-draft-fail").await;
    let t = crate::common::user();
    let id = conversation_awaiting_reason(&e.app, &t).await;
    wait_idle(&boot_bus(&e), &id).await;
    // While the turn is being qualified, turn the conversation hash into a plain string: every
    // later HSET on it fails with WRONGTYPE (a non-transient Redis error, so all retries fail).
    let bus = boot_bus(&e);
    let conv = carebus::chat::conv_key(&bus, &id);
    let hook: carebus_api::chat_q::Hook = std::sync::Arc::new(move || {
        let bus = bus.clone();
        let conv = conv.clone();
        Box::pin(async move {
            let _: () = redis::cmd("SET")
                .arg(&conv)
                .arg("not a hash")
                .query_async(&mut bus.conn())
                .await
                .unwrap();
        })
    });
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(carebus_api::chat_q::AnyQualifier::Then(hook));
    let app = carebus_api::router(state);
    let (s, b) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text":"they're too uncomfortable"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["turn"], "published");
    let case = b["case_id"].as_str().unwrap().to_string();
    let linked: Vec<String> = redis::cmd("LRANGE")
        .arg(carebus::chat::cases_key(&boot_bus(&e), &id))
        .arg(0)
        .arg(-1)
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    assert_eq!(linked, vec![case.clone()], "the case is still linked");
    let conv_of_case: String = redis::cmd("GET")
        .arg(carebus::chat::case_conv_key(&boot_bus(&e), &case))
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    assert_eq!(conv_of_case, id);
}

/// Guards the mutation "remove the `with_effective_tenant(cv.tenant, …)` wrap around the turn in
/// `routes::conversations::handle`": the qualifier of a chat turn must run with the conversation's tenant as the
/// effective tenant (the platform RigQualifier sends it as `X-Effective-Tenant-Id`, see
/// crates/carebus/tests/effective_tenant.rs); without the wrap it sees none and this fails.
#[tokio::test]
async fn a_chat_turn_qualifies_with_the_conversations_tenant_as_effective_tenant() {
    let e = crate::common::env("conv-effective-tenant").await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Option<uuid::Uuid>>::new()));
    let s = seen.clone();
    // Called inside the qualifier's extract, i.e. exactly where the platform model's request is made.
    let hook: carebus_api::chat_q::Hook = std::sync::Arc::new(move || {
        s.lock()
            .unwrap()
            .push(carebus::interaction::effective_tenant());
        Box::pin(async {})
    });
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(carebus_api::chat_q::AnyQualifier::Then(hook));
    let app = carebus_api::router(state);
    let t = crate::common::user();
    let (_, c) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let (s, b) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text":"I want to return my headphones"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let tenant: uuid::Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    assert_ne!(tenant, uuid::Uuid::nil());
    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "the turn was qualified");
    assert!(seen.iter().all(|t| *t == Some(tenant)), "{seen:?}");
    // outside a turn there is no effective tenant
    assert_eq!(carebus::interaction::effective_tenant(), None);
}

#[tokio::test]
async fn link_case_is_idempotent() {
    let e = crate::common::env("conv-link-twice").await;
    let bus = &boot_bus(&e);
    carebus::chat::link_case(bus, "conv-l", "CASE-1")
        .await
        .unwrap();
    carebus::chat::link_case(bus, "conv-l", "CASE-1")
        .await
        .unwrap();
    carebus::chat::link_case(bus, "conv-l", "CASE-2")
        .await
        .unwrap();
    let cases: Vec<String> = redis::cmd("LRANGE")
        .arg(carebus::chat::cases_key(bus, "conv-l"))
        .arg(0)
        .arg(-1)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(cases, vec!["CASE-1".to_string(), "CASE-2".to_string()]);
}

fn roles_and_texts(g: &serde_json::Value) -> Vec<(String, String)> {
    g["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                m["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_queued_turn_that_fails_tells_the_conversation_and_releases_the_lock() {
    let e = crate::common::env("conv-queued-fail").await;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(carebus_api::chat_q::AnyQualifier::FailAfter(
        1,
        std::time::Duration::from_millis(400),
        calls.clone(),
    ));
    let app = carebus_api::router(state);
    let t = crate::common::user();
    let (_, c) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let url = crate::common::j(&format!("/conversations/{id}/messages"));
    let req = crate::common::json("POST", &url, Some(&t), json!({"text":"first"}));
    let app2 = app.clone();
    let holder = tokio::spawn(async move { crate::common::send(&app2, req).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let (s, b) = crate::common::send(
        &app,
        crate::common::json("POST", &url, Some(&t), json!({"text":"a"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(holder.await.unwrap().0, StatusCode::OK);
    wait_idle(&boot_bus(&e), &id).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let (_, g) = crate::common::send(
        &app,
        crate::common::get(&crate::common::j(&format!("/conversations/{id}")), Some(&t)),
    )
    .await;
    let log = roles_and_texts(&g);
    assert_eq!(log.len(), 4, "{log:?}");
    assert_eq!(
        log.last().unwrap(),
        &(
            "agent".to_string(),
            carebus_api::routes::conversations::TURN_FAILED.to_string()
        ),
        "the failed queued turn is visible in the conversation"
    );
}

#[tokio::test]
async fn the_sweeper_drains_an_inbox_whose_holder_died() {
    let e = crate::common::env("conv-sweep").await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(carebus_api::chat_q::AnyQualifier::Record(
        std::time::Duration::ZERO,
        seen.clone(),
    ));
    let app = carebus_api::router(state.clone());
    let t = crate::common::user();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (_, c) = crate::common::send(
            &app,
            crate::common::json(
                "POST",
                &crate::common::j("/conversations"),
                Some(&t),
                json!({}),
            ),
        )
        .await;
        ids.push(c["conversation_id"].as_str().unwrap().to_string());
    }
    let (stranded, busy) = (&ids[0], &ids[1]);
    let bus = boot_bus(&e);
    // both were accepted (logged + queued) and their senders died before draining; `busy` is still locked
    for (id, text) in [(stranded, "hello?"), (busy, "held")] {
        carebus::chat::accept_customer(
            &bus,
            &carebus::chat::ChatEvent::new(id, "customer", text, None),
        )
        .await
        .unwrap();
    }
    let other = carebus::chat::acquire_lock(&bus, busy)
        .await
        .unwrap()
        .unwrap();
    let n = carebus_api::routes::conversations::sweep_inboxes(&state, &boot_scope(), 50)
        .await
        .unwrap();
    assert_eq!(n, 1, "only the unlocked inbox is drained");
    assert_eq!(*seen.lock().unwrap(), vec!["hello?".to_string()]);
    wait_idle(&bus, stranded).await;
    let (_, g) = crate::common::send(
        &app,
        crate::common::get(
            &crate::common::j(&format!("/conversations/{stranded}")),
            Some(&t),
        ),
    )
    .await;
    let roles: Vec<String> = roles_and_texts(&g).into_iter().map(|(r, _)| r).collect();
    assert_eq!(roles, ["customer", "agent"]);
    // the locked one is left to its holder
    assert_eq!(
        carebus::chat::finish_turn(&bus, busy, &other)
            .await
            .unwrap(),
        carebus::chat::Finish::Pending
    );
}

/// A demo-backends stand-in: `/api/admin/records/customers/{id}` answers `status` with `body`, and records
/// which id was asked for and whether the bearer came along.
async fn fake_demo_customer(
    status: u16,
    body: serde_json::Value,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<(String, bool)>>>,
) {
    use axum::{extract::Path, http::HeaderMap, routing::get};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = axum::Router::new().route(
        "/api/admin/records/customers/{id}",
        get(move |Path(id): Path<String>, h: HeaderMap| {
            let (log, body) = (log.clone(), body.clone());
            async move {
                let bearer = h
                    .get("authorization")
                    .is_some_and(|v| v == "Bearer demo-secret");
                log.lock().unwrap().push((id, bearer));
                (StatusCode::from_u16(status).unwrap(), axum::Json(body))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

#[tokio::test]
async fn start_returns_the_customers_name_and_tier_from_the_demo_customers_table() {
    let (url, seen) = fake_demo_customer(
        200,
        json!({"customer_id": "C-10482", "name": "Anna de Vries", "tier": "Gold", "city": "Utrecht"}),
    )
    .await;
    let e = crate::common::env_with("conv-start-profile", |c| {
        c.demo_backends_url = Some(url);
        c.demo_backends_token = Some("demo-secret".into());
    })
    .await;
    let t = crate::common::user();
    let (s, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    assert_eq!(c["customer_id"], "C-10482");
    assert_eq!(c["customer_name"], "Anna de Vries");
    assert_eq!(c["customer_tier"], "Gold");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![("C-10482".to_string(), true)],
        "one lookup of the conversation's own customer, with the demo-backends bearer"
    );
}

#[tokio::test]
async fn start_still_succeeds_with_null_profile_when_the_lookup_fails() {
    // unconfigured, an upstream 500, a 404, and a record without the fields
    let (err, _) = fake_demo_customer(500, json!({"error": "boom"})).await;
    let (missing, _) = fake_demo_customer(404, json!({"error": "no such customer"})).await;
    let (blank, _) = fake_demo_customer(200, json!({"customer_id": "C-10482", "name": ""})).await;
    for (label, url) in [
        ("none", None),
        ("500", Some(err)),
        ("404", Some(missing)),
        ("blank", Some(blank)),
        ("down", Some("http://127.0.0.1:9".to_string())),
    ] {
        let e = crate::common::env_with(&format!("conv-start-{label}"), |c| {
            c.demo_backends_url = url
        })
        .await;
        let t = crate::common::user();
        let (s, c) = crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                &crate::common::j("/conversations"),
                Some(&t),
                json!({}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{label}: {c}");
        assert!(c["conversation_id"].is_string(), "{label}: {c}");
        assert_eq!(c["customer_id"], "C-10482", "{label}");
        assert!(c["customer_name"].is_null(), "{label}: {c}");
        assert!(c["customer_tier"].is_null(), "{label}: {c}");
    }
}
