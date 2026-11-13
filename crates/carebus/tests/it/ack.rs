//! The console's ack state: the recipient group's XACK is mirrored live, and `is_acked` recomputes it from Redis
//! for a reader that missed the frame.
use std::time::Duration;

use carebus::bus::Envelope;
use futures_util::StreamExt;
use serde_json::{Value, json};

#[tokio::test]
async fn the_recipients_ack_is_mirrored_and_recomputable_and_a_failed_handler_stays_pending() {
    let bus = crate::common::bus("ack").await;
    let stream = bus.streams.tool("order");
    let mut pubsub = bus.client().get_async_pubsub().await.unwrap();
    pubsub.subscribe(bus.streams.console()).await.unwrap();
    let mut frames = pubsub.into_on_message();

    let ok = Envelope::new(
        "x.y.tool.call",
        "worker:order",
        "CASE-A",
        json!({"fail": false}),
    )
    .unwrap();
    let ok_id = bus.publish(&stream, &ok).await.unwrap();
    let bad = Envelope::new(
        "x.y.tool.call",
        "worker:order",
        "CASE-B",
        json!({"fail": true}),
    )
    .unwrap();
    let bad_id = bus.publish(&stream, &bad).await.unwrap();
    // nobody has read the stream yet
    assert!(
        !bus.is_acked(&stream, "tool-executor", &ok_id)
            .await
            .unwrap()
    );
    assert!(
        !bus.is_acked(&bus.streams.tool("nope"), "tool-executor", &ok_id)
            .await
            .unwrap()
    );

    // The executor (recipient of tool.call) fails CASE-B; the worker group reads and acks everything as well.
    let handler = |env: Envelope| async move {
        if env.payload["fail"] == true {
            anyhow::bail!("boom")
        }
        Ok(())
    };
    let opts = carebus::bus::ConsumeOptions {
        reclaim_idle: Duration::from_secs(60), // no redelivery inside this test
        ..crate::common::fast()
    };
    let (b1, b2, s1, s2, o1, o2) = (
        bus.clone(),
        bus.clone(),
        stream.clone(),
        stream.clone(),
        opts.clone(),
        opts,
    );
    let t1 = tokio::spawn(async move { b1.consume(&s1, "tool-executor", "c1", o1, handler).await });
    let t2 = tokio::spawn(async move {
        b2.consume(&s2, "worker-order", "c2", o2, |_| async { Ok(()) })
            .await
    });

    let mut publishes = Vec::new();
    let mut acks = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(200), frames.next()).await
        else {
            continue;
        };
        let v: Value = serde_json::from_str(&m.get_payload::<String>().unwrap()).unwrap();
        if v["kind"] == "ack" {
            acks.push(v);
        } else {
            publishes.push(v);
        }
    }
    t1.abort();
    t2.abort();

    // the publish mirror names the recipient group and starts unacked
    assert_eq!(publishes.len(), 2, "{publishes:?}");
    for p in &publishes {
        assert_eq!(p["consumer_group"], "tool-executor", "{p}");
        assert_eq!(p["acked"], false, "{p}");
    }
    // exactly one ack frame: the executor's for CASE-A. The worker group's acks are not the recipient's,
    // and CASE-B's handler failed, so it was never acked.
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["entry_id"], ok_id.as_str());
    assert_eq!(acks[0]["consumer_group"], "tool-executor");
    assert_eq!(acks[0]["correlation_id"], "CASE-A");
    assert_eq!(acks[0]["stream"], stream.as_str());

    // the rebuild path recomputes the same answer from Redis
    assert!(
        bus.is_acked(&stream, "tool-executor", &ok_id)
            .await
            .unwrap()
    );
    assert!(
        !bus.is_acked(&stream, "tool-executor", &bad_id)
            .await
            .unwrap(),
        "delivered but in the PEL is pending"
    );
    assert!(
        bus.is_acked(&stream, "worker-order", &bad_id)
            .await
            .unwrap()
    );
    assert!(
        !bus.is_acked(&stream, "no-such-group", &ok_id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_dead_letter_frame_has_no_consumer_group() {
    let bus = crate::common::bus("ack-dead").await;
    let mut pubsub = bus.client().get_async_pubsub().await.unwrap();
    pubsub.subscribe(bus.streams.console()).await.unwrap();
    let mut frames = pubsub.into_on_message();
    let env = Envelope::new("dead.letter", "x", "CASE-D", json!({})).unwrap();
    bus.publish(&bus.streams.dead(), &env).await.unwrap();
    let m = tokio::time::timeout(Duration::from_secs(2), frames.next())
        .await
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(&m.get_payload::<String>().unwrap()).unwrap();
    assert_eq!(v["consumer_group"], Value::Null);
    assert_eq!(v["acked"], Value::Null);
}

#[tokio::test]
async fn scoped_keys_are_prefixed() {
    let root = crate::common::root("scopedkeys").await;
    let s = crate::common::scope_a();
    let p = crate::common::prefix(&root);
    assert_eq!(
        root.scoped(&s).streams.customer("faq"),
        format!("{p}s:{}:{}:bus:customer:faq", s.tenant, s.job)
    );
    assert_eq!(
        root.scoped(&s).streams.key("case:C1"),
        format!("{p}s:{}:{}:case:C1", s.tenant, s.job)
    );
    assert_eq!(root.scoped(&s).scope(), Some(&s));
    // the registry key stays at the environment root, also from a scoped bus
    assert_eq!(root.scoped(&s).root_key("scopes"), format!("{p}scopes"));
    assert_eq!(root.root_key("scopes"), format!("{p}scopes"));
}

#[tokio::test]
async fn unscoped_bus_refuses_publish() {
    let root = crate::common::root("unscoped").await;
    let e = Envelope::new("x.request.qualified", "t", "c", json!({})).unwrap();
    let err = root.publish("bus:customer:faq", &e).await.unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
    let err = root
        .ensure_group("bus:customer:faq", "g")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
    let err = root
        .consume(
            "bus:customer:faq",
            "g",
            "c1",
            crate::common::fast(),
            |_| async { Ok(()) },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
    let err = root
        .consume_all(vec![], "g", "c1", crate::common::fast(), |_| async {
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
}

#[tokio::test]
async fn publish_stamps_scope() {
    let a = crate::common::bus("stamp").await;
    let s = crate::common::scope_a();
    let e = Envelope::new("x.request.qualified", "t", "CASE-S", json!({})).unwrap();
    assert_eq!(e.tenant_id, None);
    a.publish(&a.streams.customer("faq"), &e).await.unwrap();
    let got = crate::common::entries(&a, &a.streams.customer("faq")).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].tenant_id, Some(s.tenant));
    assert_eq!(got[0].job.as_deref(), Some(s.job.as_str()));
}

#[tokio::test]
async fn mismatched_scope_is_dead_lettered() {
    let root = crate::common::root("mismatch").await;
    let a_bus = root.scoped(&crate::common::scope_a());
    let b = crate::common::scope_b();
    let stream = a_bus.streams.customer("faq");
    let mut foreign =
        Envelope::new("faq.general.request.qualified", "t", "CASE-X", json!({})).unwrap();
    foreign.tenant_id = Some(b.tenant);
    foreign.job = Some(b.job.clone());
    let _: String = redis::cmd("XADD")
        .arg(&stream)
        .arg("*")
        .arg("envelope")
        .arg(serde_json::to_string(&foreign).unwrap())
        .query_async(&mut a_bus.conn())
        .await
        .unwrap();

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (bus, st, c) = (a_bus.clone(), stream.clone(), calls.clone());
    let task = tokio::spawn(async move {
        bus.consume(
            &st,
            "orchestrator",
            "c1",
            crate::common::fast(),
            move |_env| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await
    });
    let dead = crate::common::wait_entries(&a_bus, &a_bus.streams.dead(), 1, |_| true).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    task.abort();

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(dead.len(), 1);
    let reason = dead[0].payload["reason"].as_str().unwrap();
    assert!(reason.contains("scope_mismatch"), "{reason}");
    assert_eq!(
        dead[0].payload["original"]["tenant_id"],
        b.tenant.to_string()
    );
    // the entry is acked, not left pending for redelivery
    let pending: redis::streams::StreamPendingReply = redis::cmd("XPENDING")
        .arg(&stream)
        .arg("orchestrator")
        .query_async(&mut a_bus.conn())
        .await
        .unwrap();
    assert_eq!(pending.count(), 0);
}
