use axum::http::StatusCode;
use carebus::Envelope;
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

/// Produce a dead letter the real way: a handler that always fails, until Bus::consume gives up.
/// A hand-built fixture would only prove the route agrees with itself, not with carebus' dead_letter.
async fn dead_letter_via_consume(bus: &carebus::Bus, origin: &str, case: &str) {
    let inner = Envelope::new(
        "payments.payment-errors.work.requested",
        "orchestrator",
        case,
        json!({"work_id":"W-1"}),
    )
    .unwrap();
    bus.publish(origin, &inner).await.unwrap();
    let opts = carebus::bus::ConsumeOptions {
        batch: 16,
        block: std::time::Duration::from_millis(100),
        reclaim_idle: std::time::Duration::from_millis(200),
        max_deliveries: 2,
    };
    let (b, o) = (bus.clone(), origin.to_string());
    let task = tokio::spawn(async move {
        b.consume(&o, "worker-order", "c1", opts, |_env| async {
            anyhow::bail!("always fails")
        })
        .await
    });
    let dead = bus.streams.dead();
    let mut ok = false;
    for _ in 0..100 {
        let n: u64 = redis::cmd("XLEN")
            .arg(&dead)
            .query_async(&mut bus.conn())
            .await
            .unwrap();
        if n == 1 {
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    task.abort();
    assert!(ok, "Bus::consume did not dead-letter within 10 s");
}

#[tokio::test]
async fn lists_streams_with_groups_and_entries() {
    let e = crate::common::env("streams").await;
    let bus = &boot_bus(&e);
    let key = bus.streams.key("bus:tool:delivery");
    bus.ensure_group(&key, "tool-executor").await.unwrap();
    let env = Envelope::new(
        "logistics.delivery-status.tool.call",
        "worker:delivery",
        "CASE-1",
        json!({"call_id":"T-1"}),
    )
    .unwrap();
    bus.publish(&key, &env).await.unwrap();
    let t = crate::common::user();

    let (s, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/streams"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let row = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "bus:tool:delivery")
        .expect("stream listed without prefix");
    assert_eq!(row["bus"], "tool");
    assert_eq!(row["length"], 1);
    assert_eq!(row["groups"][0]["name"], "tool-executor");
    assert_eq!(row["groups"][0]["lag"], 1);

    // colons URL-encoded resolve to the same key
    let (s, one) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/streams/bus%3Atool%3Adelivery/entries?count=10"),
            Some(&t),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(one["items"][0]["envelope"]["correlation_id"], "CASE-1");
    assert_eq!(one["items"][0]["stream"], "bus:tool:delivery");
    // and the unencoded form too
    let (s, info) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/streams/bus:tool:delivery"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(info["key"], "bus:tool:delivery");
}

#[tokio::test]
async fn keys_outside_the_bus_namespace_are_404() {
    let e = crate::common::env("streams404").await;
    let t = crate::common::user();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/streams/case%3ACASE-1"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/streams/bus%3Anope"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // a real stream outside bus:* is unreachable too
    let outside = boot_bus(&e).streams.key("case:CASE-1");
    let _: String = redis::cmd("XADD")
        .arg(&outside)
        .arg("*")
        .arg("a")
        .arg("b")
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/streams/case%3ACASE-1/entries"),
            Some(&t),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let admin = crate::common::admin();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/streams/case%3ACASE-1/groups/g/claim"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dead_letter_replay_republishes_to_origin_and_removes_it() {
    let e = crate::common::env("dead").await;
    let bus = boot_bus(&e);
    let origin = bus.streams.key("bus:worker:payments:order");
    dead_letter_via_consume(&bus, &origin, "CASE-2").await;
    let admin = crate::common::admin();

    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/dead-letters"), Some(&admin)),
    )
    .await;
    let id = list["items"][0]["entry_id"].as_str().unwrap().to_string();
    assert_eq!(list["items"][0]["stream"], "bus:dead");
    // the real shape: {stream (full prefixed key), entry_id, deliveries, reason, original}
    assert_eq!(
        list["items"][0]["envelope"]["payload"]["stream"],
        origin.as_str()
    );
    assert_eq!(
        list["items"][0]["envelope"]["payload"]["original"]["correlation_id"],
        "CASE-2"
    );
    let (s, body) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/dead-letters/{id}/replay")),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(
        body["stream"], "bus:worker:payments:order",
        "the answer names the origin stream like every other route does: unprefixed"
    );
    // published to the stored key as-is, never re-prefixed:
    // the original entry is still in the stream (XACK does not delete); the replay adds one
    let len: u64 = redis::cmd("XLEN")
        .arg(&origin)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(len, 2);
    let (_, after) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/dead-letters"), Some(&admin)),
    )
    .await;
    assert!(after["items"].as_array().unwrap().is_empty());
    // replay is audited, naming the unprefixed origin
    let details: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT details FROM audit_events WHERE action_type='replay'")
            .fetch_all(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(details.len(), 1);
    assert_eq!(details[0]["stream"], "bus:worker:payments:order");
}

#[tokio::test]
async fn dead_letters_list_is_503_when_redis_errors_never_empty() {
    // "unavailable is not zero": a Redis error must not look like "no dead letters".
    // Force a deterministic Redis error: make bus:dead a plain string, so XREVRANGE fails with WRONGTYPE.
    let e = crate::common::env("dead-err").await;
    let _: () = redis::cmd("SET")
        .arg(boot_bus(&e).streams.dead())
        .arg("not a stream")
        .query_async(&mut boot_bus(&e).conn())
        .await
        .unwrap();
    let admin = crate::common::admin();
    let (s, body) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/dead-letters"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "upstream_unavailable");
}

#[tokio::test]
async fn two_concurrent_replays_publish_once() {
    let e = crate::common::env("dead-race").await;
    let bus = boot_bus(&e);
    let origin = bus.streams.key("bus:worker:payments:order");
    dead_letter_via_consume(&bus, &origin, "CASE-3").await;
    let admin = crate::common::admin();
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/dead-letters"), Some(&admin)),
    )
    .await;
    let id = list["items"][0]["entry_id"].as_str().unwrap().to_string();
    let uri = crate::common::j(&format!("/dead-letters/{id}/replay"));

    // a viewer can read dead letters but never replay them
    let viewer = crate::common::user();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", &uri, Some(&viewer), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (a, b) = tokio::join!(
        crate::common::send(
            &e.app,
            crate::common::json("POST", &uri, Some(&admin), json!({}))
        ),
        crate::common::send(
            &e.app,
            crate::common::json("POST", &uri, Some(&admin), json!({}))
        ),
    );
    let mut codes = [a.0.as_u16(), b.0.as_u16()];
    codes.sort();
    assert_eq!(
        codes,
        [200, 404],
        "exactly one replay wins: {:?} {:?}",
        a.1,
        b.1
    );
    // 1 original + exactly 1 replay
    let len: u64 = redis::cmd("XLEN")
        .arg(&origin)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(len, 2);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action_type='replay'")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(n, 1, "only the winner is audited");
}

#[tokio::test]
async fn dead_letters_are_paged_newest_first_and_discard_is_audited() {
    let e = crate::common::env("dead-page").await;
    let bus = boot_bus(&e);
    let dead = bus.streams.dead();
    let mut ids = Vec::new();
    for i in 0..3 {
        let env = Envelope::new(
            "dead.letter",
            "g",
            &format!("CASE-{i}"),
            json!({"stream": bus.streams.key("bus:customer:returns"), "entry_id": "1-0", "deliveries": 5, "reason": "x", "original": null}),
        )
        .unwrap();
        ids.push(bus.publish(&dead, &env).await.unwrap());
    }
    let admin = crate::common::admin();
    let (s, p1) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/dead-letters?limit=2"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    assert_eq!(p1["items"][0]["entry_id"], ids[2].as_str());
    assert_eq!(p1["items"][1]["entry_id"], ids[1].as_str());
    let next = p1["next"]
        .as_str()
        .expect("full page has a cursor")
        .to_string();
    let (_, p2) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/dead-letters?limit=2&before={next}")),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(p2["items"].as_array().unwrap().len(), 1);
    assert_eq!(p2["items"][0]["entry_id"], ids[0].as_str());
    assert!(p2["next"].is_null());
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/dead-letters?before=nope"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // original null cannot be replayed (400) and is left in place
    let (s, body) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/dead-letters/{}/replay", ids[0])),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    // discard: 200 once, then 404; one audit row
    let uri = crate::common::j(&format!("/dead-letters/{}", ids[0]));
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("DELETE", &uri, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("DELETE", &uri, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action_type='discard'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn claim_unknown_group_404_and_claim_is_audited() {
    let e = crate::common::env("claim").await;
    let bus = boot_bus(&e);
    let key = bus.streams.key("bus:customer:returns");
    bus.ensure_group(&key, "g").await.unwrap();
    let env = Envelope::new(
        "returns.request.qualified",
        "interaction",
        "CASE-9",
        json!({}),
    )
    .unwrap();
    bus.publish(&key, &env).await.unwrap();
    // a consumer reads it and never acks
    let _: redis::Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg("g")
        .arg("dead-consumer")
        .arg("COUNT")
        .arg(1)
        .arg("STREAMS")
        .arg(&key)
        .arg(">")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let admin = crate::common::admin();
    let uri = &crate::common::j("/streams/bus%3Acustomer%3Areturns/groups/g/claim");
    // not idle for 30 s yet: nothing claimed, still audited
    let (s, body) = crate::common::send(
        &e.app,
        crate::common::json("POST", uri, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["claimed"], 0);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/streams/bus%3Acustomer%3Areturns/groups/nope/claim"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action_type='claim'")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/// Plan 13 Review Focus 3: a job's streams route lists and addresses only its own scope's keys.
#[tokio::test]
async fn streams_list_only_own_scope() {
    let e = crate::common::env("streams-scope").await;
    let acme = provision_other(&e).await;
    let (boot, other) = (boot_bus(&e), e.state.bus.scoped(&other_scope()));
    let env = Envelope::new(
        "faq.general.request.qualified",
        "interaction",
        "CASE-S",
        json!({}),
    )
    .unwrap();
    boot.publish(&boot.streams.customer("faq"), &env)
        .await
        .unwrap();
    boot.publish(&boot.streams.customer("billing"), &env)
        .await
        .unwrap();
    other
        .publish(&other.streams.customer("faq"), &env)
        .await
        .unwrap();
    let keys = |list: &serde_json::Value| -> Vec<String> {
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["key"].as_str().unwrap().to_string())
            .collect()
    };

    let (s, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/streams"), Some(&acme)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(
        keys(&list),
        ["bus:customer:faq"],
        "unprefixed, OTHER's only: {list}"
    );
    assert_eq!(list["items"][0]["length"], 1, "{list}");
    // BOOT's stream is not addressable from OTHER's job either
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/streams/bus%3Acustomer%3Abilling"),
            Some(&acme),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let admin = crate::common::admin();
    let (s, mine) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/streams"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{mine}");
    assert_eq!(
        keys(&mine),
        ["bus:customer:billing", "bus:customer:faq"],
        "{mine}"
    );
}
