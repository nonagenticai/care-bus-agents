use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

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

fn other_scope() -> carebus::Scope {
    carebus::Scope {
        tenant: crate::common::OTHER_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    }
}

/// OTHER tenant with a job of the same key, and its tenant_admin `u-acme` (owner pool: RLS does not apply).
/// Returns `u-acme`'s token.
async fn provision_other(e: &crate::common::TestEnv) -> String {
    let db = crate::common::pool(e);
    let t = crate::common::insert_tenant(db, crate::common::OTHER_TENANT).await;
    crate::common::insert_job(db, t, crate::common::BOOT_JOB).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-acme', $1, 'tenant_admin', 'Acme Admin', 'test')",
    )
    .bind(t)
    .execute(db)
    .await
    .unwrap();
    crate::common::token_for("u-acme", &[])
}

async fn read_until(body: &mut Body, secs: u64, done: impl Fn(&str) -> bool) -> String {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    while !done(&seen) && tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(frame))) =
            tokio::time::timeout(std::time::Duration::from_millis(500), body.frame()).await
            && let Ok(d) = frame.into_data()
        {
            seen.push_str(&String::from_utf8_lossy(&d));
        }
    }
    seen
}

#[tokio::test]
async fn streams_bus_and_chat_frames_filtered_by_conversation() {
    let e = crate::common::env("sse").await;
    let bus = boot_bus(&e);
    let _: () = redis::cmd("SET")
        .arg(carebus::chat::case_conv_key(&bus, "CASE-1"))
        .arg("conv-1")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let t = crate::common::user();
    let req = Request::get(crate::common::j("/events?conversation_id=conv-1"))
        .header("authorization", format!("Bearer {t}"))
        .body(Body::empty())
        .unwrap();
    let resp = e.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut body = resp.into_body();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await; // let the subscription land
    let other = carebus::Envelope::new(
        "faq.general.request.qualified",
        "interaction",
        "CASE-2",
        serde_json::json!({}),
    )
    .unwrap();
    bus.publish(&bus.streams.key("bus:customer:faq"), &other)
        .await
        .unwrap();
    let mine = carebus::Envelope::new(
        "faq.general.request.qualified",
        "interaction",
        "CASE-1",
        serde_json::json!({}),
    )
    .unwrap();
    bus.publish(&bus.streams.key("bus:customer:faq"), &mine)
        .await
        .unwrap();
    carebus::chat::append_and_publish(
        &bus,
        &carebus::chat::ChatEvent::new("conv-1", "agent", "hello", None),
    )
    .await
    .unwrap();

    let seen = read_until(&mut body, 5, |s| {
        s.contains("event: chat") && s.contains("CASE-1")
    })
    .await;
    assert!(
        seen.contains("event: bus") && seen.contains("CASE-1"),
        "{seen}"
    );
    // a bus frame's id is a Redis stream entry id `<ms>-<seq>` (not just "contains a dash": `id: chat-…` would pass that)
    let is_entry_id = |l: &str| {
        l.strip_prefix("id: ")
            .and_then(|v| v.split_once('-'))
            .is_some_and(|(ms, seq)| {
                !ms.is_empty()
                    && !seq.is_empty()
                    && ms.bytes().all(|b| b.is_ascii_digit())
                    && seq.bytes().all(|b| b.is_ascii_digit())
            })
    };
    assert!(
        seen.lines().any(is_entry_id),
        "bus frames carry the stream entry id: {seen}"
    );
    assert!(seen.contains("id: chat-"), "chat frames carry an id");
    assert!(
        !seen.contains("CASE-2"),
        "other conversations' cases are filtered out"
    );
    assert!(seen.contains("event: chat") && seen.contains("hello"));
    // The SSE `stream` field is the UNPREFIXED key (same BusEvent shape as REST).
    assert!(
        seen.contains(r#""stream":"bus:customer:faq""#),
        "unprefixed stream key: {seen}"
    );

    // Client disconnect: dropping the body must drop the Pub/Sub subscription (Review Focus 2).
    let channel = bus.streams.console();
    let numsub = || {
        let (bus, channel) = (bus.clone(), channel.clone());
        async move {
            let v: (String, i64) = redis::cmd("PUBSUB")
                .arg("NUMSUB")
                .arg(&channel)
                .query_async(&mut bus.conn())
                .await
                .unwrap();
            v.1
        }
    };
    assert_eq!(
        numsub().await,
        1,
        "this connection is subscribed while the body is alive"
    );
    drop(body);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    while numsub().await != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "subscription still alive 1 s after the client went away: leaked task"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_case_linked_after_its_first_event_still_reaches_the_conversation() {
    // Interaction::handle publishes request.qualified (mirrored to `console`) BEFORE link_case runs.
    // The connection must not remember "no owner" for that case.
    let e = crate::common::env("sse-late").await;
    let bus = boot_bus(&e);
    let t = crate::common::user();
    let req = Request::get(crate::common::j("/events?conversation_id=conv-7"))
        .header("authorization", format!("Bearer {t}"))
        .body(Body::empty())
        .unwrap();
    let mut body = e.app.clone().oneshot(req).await.unwrap().into_body();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await; // let the subscription land
    let publish = |case: &'static str, topic: &'static str| {
        let bus = bus.clone();
        async move {
            let env =
                carebus::Envelope::new(topic, "orchestrator", case, serde_json::json!({})).unwrap();
            bus.publish(&bus.streams.key("bus:customer:faq"), &env)
                .await
                .unwrap();
        }
    };
    // A case already linked to this conversation: its event is the in-order marker.
    let _: () = redis::cmd("SET")
        .arg(carebus::chat::case_conv_key(&bus, "CASE-M"))
        .arg("conv-7")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    publish("CASE-7", "faq.general.request.qualified").await; // not linked yet: filtered
    publish("CASE-M", "faq.general.marker").await;
    // One subscription handles messages in order, so once the marker arrives CASE-7's owner lookup is done
    // (and found nothing) before the link below is written. No sleep, no race.
    let seen = read_until(&mut body, 5, |s| s.contains("faq.general.marker")).await;
    assert!(
        seen.contains("faq.general.marker"),
        "marker never arrived: {seen}"
    );
    assert!(
        !seen.contains("request.qualified"),
        "an unlinked case's event is filtered: {seen}"
    );
    let _: () = redis::cmd("SET")
        .arg(carebus::chat::case_conv_key(&bus, "CASE-7"))
        .arg("conv-7")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    publish("CASE-7", "faq.general.request.resolved").await;
    let seen = read_until(&mut body, 5, |s| s.contains("request.resolved")).await;
    assert!(
        seen.contains("request.resolved"),
        "events after the link must arrive (a cached miss would hide them): {seen}"
    );
}

#[tokio::test]
async fn events_need_a_token() {
    let e = crate::common::env("sse-auth").await;
    let (st, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/events"), None),
    )
    .await;
    assert_eq!(st, 401);
}

#[tokio::test]
async fn ack_frames_reach_the_conversation_and_the_case_trace_recomputes_ack_state() {
    let e = crate::common::env("sse-ack").await;
    let bus = boot_bus(&e);
    for (case, conv) in [("CASE-K", "conv-k"), ("CASE-Z", "conv-z")] {
        let _: () = redis::cmd("SET")
            .arg(carebus::chat::case_conv_key(&bus, case))
            .arg(conv)
            .query_async(&mut bus.conn())
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, started_at)
         VALUES ('CASE-K','C-1','delivery_status','logistics','retail:logistics:delivery-status', now())",
    )
    .execute(crate::common::pool(&e))
    .await
    .unwrap();
    let t = crate::common::user();
    let req = Request::get(crate::common::j("/events?conversation_id=conv-k"))
        .header("authorization", format!("Bearer {t}"))
        .body(Body::empty())
        .unwrap();
    let mut body = e.app.clone().oneshot(req).await.unwrap().into_body();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await; // let the subscription land

    let stream = bus.streams.tool("order");
    let call = |case: &str, fail: bool| {
        carebus::Envelope::new(
            "logistics.delivery-status.tool.call",
            "worker:order",
            case,
            serde_json::json!({ "fail": fail }),
        )
        .unwrap()
    };
    let ok_id = bus.publish(&stream, &call("CASE-K", false)).await.unwrap();
    let bad_id = bus.publish(&stream, &call("CASE-K", true)).await.unwrap();
    bus.publish(&stream, &call("CASE-Z", false)).await.unwrap();
    // The executor is tool.call's recipient; it fails the second CASE-K call, which stays pending.
    let (b, s) = (bus.clone(), stream.clone());
    let consumer = tokio::spawn(async move {
        let opts = carebus::bus::ConsumeOptions {
            block: std::time::Duration::from_millis(100),
            reclaim_idle: std::time::Duration::from_secs(60),
            ..Default::default()
        };
        b.consume(&s, "tool-executor", "c1", opts, |env| async move {
            if env.payload["fail"] == true {
                anyhow::bail!("boom")
            }
            Ok(())
        })
        .await
    });

    let seen = read_until(&mut body, 5, |s| s.contains("event: ack")).await;
    let frames: Vec<(String, serde_json::Value)> = seen
        .split("\n\n")
        .filter_map(|block| {
            let ev = block.lines().find_map(|l| l.strip_prefix("event: "))?;
            let data = block.lines().find_map(|l| l.strip_prefix("data: "))?;
            Some((ev.to_string(), serde_json::from_str(data).ok()?))
        })
        .collect();
    let bus_frames: Vec<_> = frames.iter().filter(|(e, _)| e == "bus").collect();
    assert_eq!(bus_frames.len(), 2, "only CASE-K's two entries: {seen}");
    for (_, v) in &bus_frames {
        assert_eq!(v["consumer_group"], "tool-executor", "{v}");
        assert_eq!(v["acked"], false, "{v}");
    }
    let acks: Vec<_> = frames.iter().filter(|(e, _)| e == "ack").collect();
    assert_eq!(acks.len(), 1, "{seen}");
    let ack = &acks[0].1;
    assert_eq!(ack["entry_id"], ok_id.as_str());
    assert_eq!(
        ack["stream"], "bus:tool:order",
        "unprefixed like the bus frame"
    );
    assert_eq!(ack["consumer_group"], "tool-executor");
    assert_eq!(ack["correlation_id"], "CASE-K");
    assert!(
        !seen.contains("CASE-Z"),
        "other conversations' acks are filtered: {seen}"
    );

    // Wait until the failed call is in the executor's PEL, then rebuild like a reconnecting console.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let p: redis::streams::StreamPendingReply = redis::cmd("XPENDING")
            .arg(&stream)
            .arg("tool-executor")
            .query_async(&mut bus.conn())
            .await
            .unwrap();
        if p.count() == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the failed call never became pending"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (st, case) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases/CASE-K"), Some(&t)),
    )
    .await;
    consumer.abort();
    assert_eq!(st, 200, "{case}");
    let trace = case["trace"].as_array().unwrap();
    let by_id = |id: &str| {
        trace
            .iter()
            .find(|e| e["entry_id"] == id)
            .unwrap_or_else(|| panic!("{id} not in {case}"))
            .clone()
    };
    let (ok, bad) = (by_id(&ok_id), by_id(&bad_id));
    assert_eq!(ok["consumer_group"], "tool-executor");
    assert_eq!(ok["acked"], true, "acked by its recipient: {ok}");
    assert_eq!(bad["consumer_group"], "tool-executor");
    assert_eq!(bad["acked"], false, "delivered but still pending: {bad}");
}

/// Plan 13 Review Focus 3: the console SSE of a job carries only its own scope's bus and chat frames.
#[tokio::test]
async fn console_is_scoped() {
    let e = crate::common::env("sse-scope").await;
    let acme = provision_other(&e).await;
    let req = Request::get(crate::common::j("/events"))
        .header("authorization", format!("Bearer {acme}"))
        .body(Body::empty())
        .unwrap();
    let resp = e.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let mut body = resp.into_body();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await; // let the subscription land

    let (boot, other) = (boot_bus(&e), e.state.bus.scoped(&other_scope()));
    let env = |case: &str| {
        carebus::Envelope::new(
            "faq.general.request.qualified",
            "interaction",
            case,
            serde_json::json!({}),
        )
        .unwrap()
    };
    boot.publish(&boot.streams.customer("faq"), &env("CASE-BOOT"))
        .await
        .unwrap();
    carebus::chat::append_and_publish(
        &boot,
        &carebus::chat::ChatEvent::new("conv-boot", "agent", "boot only", None),
    )
    .await
    .unwrap();
    let seen = read_until(&mut body, 1, |s| {
        s.contains("CASE-BOOT") || s.contains("boot only")
    })
    .await;
    assert!(
        !seen.contains("CASE-BOOT") && !seen.contains("boot only"),
        "another scope's frames reached the connection: {seen}"
    );

    other
        .publish(&other.streams.customer("faq"), &env("CASE-OTHER"))
        .await
        .unwrap();
    let seen = read_until(&mut body, 5, |s| s.contains("CASE-OTHER")).await;
    assert!(
        seen.contains("event: bus") && seen.contains("CASE-OTHER"),
        "{seen}"
    );
    assert!(
        seen.contains(r#""stream":"bus:customer:faq""#),
        "the scoped prefix is stripped: {seen}"
    );
    assert!(
        seen.contains(&format!(r#""tenant_id":"{}""#, crate::common::OTHER_TENANT)),
        "the envelope carries its scope: {seen}"
    );
}
