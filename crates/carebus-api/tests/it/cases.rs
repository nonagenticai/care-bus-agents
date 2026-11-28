use axum::http::StatusCode;
use carebus::Envelope;
use carebus::domain::{
    Outcome, QualifiedRequest, REQUEST_QUALIFIED, REQUEST_RESOLVED, RequestType, Resolution, Slots,
    TOOL_RESPONSE, ToolResponse,
};
use redis::AsyncCommands;
use serde_json::{Map, json};

fn boot_scope() -> carebus::Scope {
    carebus::Scope {
        tenant: crate::common::BOOT_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    }
}

/// The bootstrap job's bus: the recorder and the routes under `crate::common::j(..)` use exactly these keys.
fn boot_bus(e: &crate::common::TestEnv) -> carebus::Bus {
    e.state.bus.scoped(&boot_scope())
}

/// The bootstrap scope as the recorder writes it.
async fn boot_ids(e: &crate::common::TestEnv) -> carebus_api::recorder::ScopeIds {
    carebus_api::recorder::ScopeIds::resolve(&e.state, boot_bus(e), &boot_scope())
        .await
        .unwrap()
}

fn qualified(case: &str) -> QualifiedRequest {
    QualifiedRequest {
        case_id: case.into(),
        customer_id: "C-10482".into(),
        request_type: RequestType::r#return(),
        area: "order-support".into(),
        context_space: "retail:order-support:returns".into(),
        slots: Slots::of(&[("order_id", "ORD-58213"), ("reason", "too uncomfortable")]),
        utterance: "return my headphones".into(),
        gates: None,
    }
}

fn qualified_env(case: &str) -> Envelope {
    let q = qualified(case);
    Envelope::new(&q.topic(REQUEST_QUALIFIED), "interaction", case, q).unwrap()
}

fn resolved_env(case: &str, outcome: Outcome) -> Envelope {
    let q = qualified(case);
    let r = Resolution {
        case_id: case.into(),
        request_type: q.request_type.clone(),
        outcome,
        reason: None,
        facts: Map::new(),
    };
    Envelope::new(&q.topic(REQUEST_RESOLVED), "orchestrator", case, r).unwrap()
}

fn tool_response_env(call_id: &str, ok: bool) -> Envelope {
    let r = ToolResponse {
        call_id: call_id.into(),
        mcp_tool: "delivery_track_shipment".into(),
        ok,
        result: None,
        error: (!ok).then(|| "timeout after 3000 ms".to_string()),
        retryable: !ok,
        latency_ms: 3001,
    };
    Envelope::new(
        &format!("logistics.delivery-status.{TOOL_RESPONSE}"),
        "mcp:delivery",
        "CASE-B",
        r,
    )
    .unwrap()
}

#[tokio::test]
async fn recorder_writes_case_history_and_is_idempotent() {
    let e = crate::common::env("recorder").await;
    let ids = boot_ids(&e).await;
    let q = qualified_env("CASE-A");
    carebus_api::recorder::handle_customer(&e.state, &ids, &q)
        .await
        .unwrap();
    carebus_api::recorder::handle_customer(&e.state, &ids, &q)
        .await
        .unwrap(); // redelivery
    let r = resolved_env("CASE-A", Outcome::Completed);
    carebus_api::recorder::handle_customer(&e.state, &ids, &r)
        .await
        .unwrap();
    carebus_api::recorder::handle_customer(&e.state, &ids, &r)
        .await
        .unwrap();
    let rows: Vec<(String, String, String, String, i32)> = sqlx::query_as(
        "SELECT area, context_space, outcome, request_type, work_items FROM case_history WHERE case_id='CASE-A'",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(
            "order-support".into(),
            "retail:order-support:returns".into(),
            "completed".into(),
            "return".into(),
            0
        )]
    );
}

#[tokio::test]
async fn resolved_before_qualified_stays_pending_then_succeeds() {
    let e = crate::common::env("recorder-order").await;
    let ids = boot_ids(&e).await;
    let r = resolved_env("CASE-O", Outcome::Failed);
    assert!(
        carebus_api::recorder::handle_customer(&e.state, &ids, &r)
            .await
            .is_err()
    );
    carebus_api::recorder::handle_customer(&e.state, &ids, &qualified_env("CASE-O"))
        .await
        .unwrap();
    carebus_api::recorder::handle_customer(&e.state, &ids, &r)
        .await
        .unwrap();
    let (outcome, ms): (String, Option<i64>) =
        sqlx::query_as("SELECT outcome, duration_ms FROM case_history WHERE case_id='CASE-O'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(outcome, "failed");
    assert!(ms.is_some());
}

#[tokio::test]
async fn conversation_id_is_filled_from_the_case_link_on_either_event() {
    let e = crate::common::env("recorder-conv").await;
    let ids = boot_ids(&e).await;
    let boot = boot_bus(&e);
    let mut c = boot.conn();
    // link exists before the qualified event
    let _: () = c
        .set(carebus::chat::case_conv_key(&boot, "CASE-V1"), "conv-1")
        .await
        .unwrap();
    carebus_api::recorder::handle_customer(&e.state, &ids, &qualified_env("CASE-V1"))
        .await
        .unwrap();
    // link appears only after the case was recorded; the resolved event picks it up
    carebus_api::recorder::handle_customer(&e.state, &ids, &qualified_env("CASE-V2"))
        .await
        .unwrap();
    let _: () = c
        .set(carebus::chat::case_conv_key(&boot, "CASE-V2"), "conv-2")
        .await
        .unwrap();
    carebus_api::recorder::handle_customer(
        &e.state,
        &ids,
        &resolved_env("CASE-V2", Outcome::Completed),
    )
    .await
    .unwrap();
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT case_id, conversation_id FROM case_history ORDER BY case_id")
            .fetch_all(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("CASE-V1".into(), Some("conv-1".into())),
            ("CASE-V2".into(), Some("conv-2".into()))
        ]
    );
}

#[tokio::test]
async fn tool_responses_become_execute_audit_rows() {
    let e = crate::common::env("recorder-tool").await;
    let ids = boot_ids(&e).await;
    let resp = tool_response_env("T-9", false);
    carebus_api::recorder::handle_tool(&e.state, &ids, &resp)
        .await
        .unwrap();
    carebus_api::recorder::handle_tool(&e.state, &ids, &resp)
        .await
        .unwrap();
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT resource_id, status FROM audit_events WHERE action_type='execute'")
            .fetch_all(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![("delivery_track_shipment".into(), "failure".into())],
        "same call_id recorded once"
    );
}

#[tokio::test]
async fn two_replicas_handling_the_same_tool_response_write_one_row() {
    let e = crate::common::env("recorder-tool-race").await;
    let ids = boot_ids(&e).await;
    let resp = tool_response_env("T-RACE", true);
    let (a, b) = tokio::join!(
        carebus_api::recorder::handle_tool(&e.state, &ids, &resp),
        carebus_api::recorder::handle_tool(&e.state, &ids, &resp)
    );
    a.unwrap();
    b.unwrap();
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM audit_events WHERE request_id='T-RACE'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn spawned_recorder_consumes_streams_without_blocking() {
    let e = crate::common::env("recorder-spawn").await;
    carebus::scopes::add(&e.state.bus, &boot_scope())
        .await
        .unwrap();
    let handle = carebus_api::recorder::spawn(e.state.clone(), "test-consumer".into());
    let bus = &boot_bus(&e);
    bus.publish(
        &bus.streams.customer("order-support"),
        &qualified_env("CASE-S"),
    )
    .await
    .unwrap();
    bus.publish(
        &bus.streams.tool("delivery"),
        &tool_response_env("T-S", true),
    )
    .await
    .unwrap();
    let mut seen = (0, 0);
    for _ in 0..50 {
        seen = (
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM case_history WHERE case_id='CASE-S'",
            )
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap(),
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE request_id='T-S'",
            )
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap(),
        );
        if seen == (1, 1) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    handle.abort();
    assert_eq!(seen, (1, 1));
}

#[tokio::test]
async fn cases_list_filters_and_detail_has_trace() {
    let e = crate::common::env("cases").await;
    let ids = boot_ids(&e).await;
    let bus = &boot_bus(&e);
    let stream = bus.streams.customer("order-support");
    let q = qualified_env("CASE-C");
    bus.publish(&stream, &q).await.unwrap();
    // an entry whose envelope cannot be parsed must not break the trace
    let _: String = bus
        .conn()
        .xadd(&stream, "*", &[("data", "not json")])
        .await
        .unwrap();
    carebus_api::recorder::handle_customer(&e.state, &ids, &q)
        .await
        .unwrap();
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases?area=order-support"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["items"][0]["case_id"], "CASE-C");
    assert!(
        b["items"][0]["outcome"].is_null(),
        "open case has no outcome yet"
    );
    let (_, empty) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases?outcome=failed"), Some(&t)),
    )
    .await;
    assert!(empty["items"].as_array().unwrap().is_empty());
    let (s, d) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases/CASE-C"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d["case"]["case_id"], "CASE-C");
    assert_eq!(d["trace"].as_array().unwrap().len(), 1);
    assert_eq!(d["trace"][0]["stream"], "bus:customer:order-support");
    assert!(d["trace"][0]["entry_id"].is_string());
    assert_eq!(d["trace"][0]["envelope"]["correlation_id"], "CASE-C");
    assert!(d["hash"].is_object());
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j("/cases/CASE-NOPE"), Some(&t))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j("/cases"), None)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn case_trace_recomputes_recipient_ack_on_the_prefixed_stream() {
    let e = crate::common::env("cases-trace-ack").await;
    let ids = boot_ids(&e).await;
    let bus = &boot_bus(&e);
    let stream = bus.streams.customer("order-support");
    let group = carebus::orchestrator::GROUP;
    bus.ensure_group(&stream, group).await.unwrap();
    let q = qualified_env("CASE-ACK");
    let id = bus.publish(&stream, &q).await.unwrap();
    carebus_api::recorder::handle_customer(&e.state, &ids, &q)
        .await
        .unwrap();
    let token = crate::common::user();
    let (_, before) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases/CASE-ACK"), Some(&token)),
    )
    .await;
    assert_eq!(before["trace"][0]["consumer_group"], group);
    assert_eq!(before["trace"][0]["acked"], false);

    let _: redis::streams::StreamReadReply = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(group)
        .arg("test-consumer")
        .arg("STREAMS")
        .arg(&stream)
        .arg(">")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let _: i64 = bus.conn().xack(&stream, group, &[&id]).await.unwrap();
    let (status, after) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases/CASE-ACK"), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["trace"][0]["stream"], "bus:customer:order-support");
    assert_eq!(after["trace"][0]["acked"], true);
}

#[tokio::test]
async fn case_detail_hash_is_the_raw_redis_field_map() {
    let e = crate::common::env("cases-hash").await;
    let ids = boot_ids(&e).await;
    carebus_api::recorder::handle_customer(&e.state, &ids, &qualified_env("CASE-H"))
        .await
        .unwrap();
    let boot = boot_bus(&e);
    let key = boot.streams.key("case:CASE-H");
    let _: () = boot
        .conn()
        .hset_multiple(&key, &[("phase", "2"), ("pending:1", "0")])
        .await
        .unwrap();
    let t = crate::common::user();
    let (_, d) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases/CASE-H"), Some(&t)),
    )
    .await;
    assert_eq!(d["hash"], json!({"phase": "2", "pending:1": "0"}));
}

#[tokio::test]
async fn cases_are_paged_newest_first_and_bad_cursor_is_400() {
    let e = crate::common::env("cases-page").await;
    for (i, c) in ["CASE-P1", "CASE-P2", "CASE-P3"].iter().enumerate() {
        sqlx::query(
            "INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, started_at)
             VALUES ($1,'C-1','return','order-support','retail:order-support:returns', now() - make_interval(mins => $2))",
        )
        .bind(c)
        .bind(-(i as i32))
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    }
    let t = crate::common::user();
    let (_, p1) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases?limit=2"), Some(&t)),
    )
    .await;
    let ids = |v: &serde_json::Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["case_id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&p1), ["CASE-P3", "CASE-P2"]);
    let next = p1["next"].as_str().unwrap();
    let (_, p2) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/cases?limit=2&before={next}")),
            Some(&t),
        ),
    )
    .await;
    assert_eq!(ids(&p2), ["CASE-P1"]);
    assert!(p2["next"].is_null());
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases?before=garbage"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_cursor");
}

#[tokio::test]
async fn tied_started_at_rows_page_without_skips_and_cursor_is_validated() {
    let e = crate::common::env("cases-ties").await;
    for c in ["CASE-T1", "CASE-T2", "CASE-T3"] {
        sqlx::query(
            "INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, started_at)
             VALUES ($1,'C-1','return','order-support','retail:order-support:returns','2026-10-15T10:00:00Z')",
        )
        .bind(c)
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    }
    let t = crate::common::user();
    let mut seen = Vec::new();
    let mut uri = crate::common::j("/cases?limit=1").to_string();
    for _ in 0..5 {
        let (_, p) = crate::common::send(&e.app, crate::common::get(&uri, Some(&t))).await;
        for i in p["items"].as_array().unwrap() {
            seen.push(i["case_id"].as_str().unwrap().to_string());
        }
        match p["next"].as_str() {
            Some(n) => uri = crate::common::j(&format!("/cases?limit=1&before={n}")),
            None => break,
        }
    }
    assert_eq!(seen, ["CASE-T3", "CASE-T2", "CASE-T1"]);
    for bad in ["2026-10-15T10:00:00.000000Z", "nope|CASE-T1"] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j(&format!("/cases?before={bad}")), Some(&t)),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(b["error"]["code"], "invalid_cursor");
    }
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases?outcome=bogus"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_query");
}

#[tokio::test]
async fn resolved_counts_attempts_as_work_items_and_retries() {
    let e = crate::common::env("recorder-work").await;
    let ids = boot_ids(&e).await;
    carebus_api::recorder::handle_customer(&e.state, &ids, &qualified_env("CASE-W"))
        .await
        .unwrap();
    let boot = boot_bus(&e);
    let key = boot.streams.key("case:CASE-W");
    let w1 = json!({"work_id":"CASE-W:0:order:1","attempt":1,"status":"failed"}).to_string();
    let w2 = json!({"work_id":"CASE-W:0:order:2","attempt":2,"status":"succeeded"}).to_string();
    let _: () = boot
        .conn()
        .hset_multiple(
            &key,
            &[("work:CASE-W:0:order:1", w1), ("work:CASE-W:0:order:2", w2)],
        )
        .await
        .unwrap();
    carebus_api::recorder::handle_customer(
        &e.state,
        &ids,
        &resolved_env("CASE-W", Outcome::Completed),
    )
    .await
    .unwrap();
    let row: (i32, i32) =
        sqlx::query_as("SELECT work_items, retries FROM case_history WHERE case_id='CASE-W'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(row, (2, 1));
}

/// The recorder persists the `name@version` the orchestrator stamped on `request.resolved`; an envelope without
/// one (older entries) leaves the columns NULL. The list and the detail expose it as `procedure`.
#[tokio::test]
async fn recorder_persists_the_procedure_version_and_the_api_exposes_it() {
    let e = crate::common::env("recorder-procedure").await;
    let ids = boot_ids(&e).await;
    for case in ["CASE-PV", "CASE-PN"] {
        carebus_api::recorder::handle_customer(&e.state, &ids, &qualified_env(case))
            .await
            .unwrap();
    }
    let stamped =
        resolved_env("CASE-PV", Outcome::Completed).with_procedure(Some("return@1.4.0-beta"));
    carebus_api::recorder::handle_customer(&e.state, &ids, &stamped)
        .await
        .unwrap();
    carebus_api::recorder::handle_customer(
        &e.state,
        &ids,
        &resolved_env("CASE-PN", Outcome::Failed),
    )
    .await
    .unwrap();
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT case_id, procedure_name, procedure_version FROM case_history ORDER BY case_id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("CASE-PN".into(), None, None),
            (
                "CASE-PV".into(),
                Some("return".into()),
                Some("1.4.0-beta".into())
            ),
        ]
    );
    let t = crate::common::user();
    let (_, l) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases"), Some(&t)),
    )
    .await;
    let proc_of = |id: &str| {
        l["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["case_id"] == id)
            .unwrap()["procedure"]
            .clone()
    };
    assert_eq!(proc_of("CASE-PV"), json!("return@1.4.0-beta"));
    assert_eq!(proc_of("CASE-PN"), serde_json::Value::Null);
    assert!(
        l["items"][0].get("procedure_name").is_none(),
        "only the joined name@version is exposed"
    );
    let (_, d) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/cases/CASE-PV"), Some(&t)),
    )
    .await;
    assert_eq!(d["case"]["procedure"], "return@1.4.0-beta");
}

#[tokio::test]
async fn open_filter_finds_cases_beyond_the_first_unfiltered_page() {
    let e = crate::common::env("cases-open").await;
    for i in 0..55 {
        sqlx::query("INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, outcome, started_at) VALUES ($1,'C-1','return','order-support','retail:order-support:returns',$2, now() - make_interval(mins => $3))")
            .bind(format!("CASE-OPEN-{i:03}"))
            .bind(if i >= 52 { None } else { Some("completed") })
            .bind(i)
            .execute(crate::common::pool(&e)).await.unwrap();
    }
    let token = crate::common::user();
    let (status, page) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/cases?outcome=open&area=order-support&limit=2"),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"][0]["case_id"], "CASE-OPEN-052");
    assert_eq!(page["items"][1]["case_id"], "CASE-OPEN-053");
    let next = page["next"].as_str().unwrap();
    let (_, page) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!(
                "/cases?outcome=open&area=order-support&limit=2&before={next}"
            )),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["case_id"], "CASE-OPEN-054");
    assert!(page["next"].is_null());
}

// ---------------------------------------------------------------------------------------------------------------
// Server-side search and sort
// ---------------------------------------------------------------------------------------------------------------

/// Inserts one case row directly; `search_text` is a generated column, so it fills itself.
async fn insert_case(
    e: &crate::common::TestEnv,
    case: &str,
    customer: &str,
    request_type: &str,
    outcome: Option<&str>,
    duration_ms: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO case_history
           (case_id, customer_id, request_type, area, context_space, outcome, duration_ms, started_at)
         VALUES ($1, $2, $3, 'order-support', 'retail:order-support:returns', $4, $5, now())",
    )
    .bind(case)
    .bind(customer)
    .bind(request_type)
    .bind(outcome)
    .bind(duration_ms)
    .execute(crate::common::pool(e))
    .await
    .unwrap();
}

/// The list as the console reads it, under the provisioned tenant user.
async fn get_cases(e: &crate::common::TestEnv, query: &str) -> (StatusCode, serde_json::Value) {
    let t = crate::common::user();
    crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j(&format!("/cases{query}")), Some(&t)),
    )
    .await
}

fn case_ids(v: &serde_json::Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["case_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn cases_search_matches_customer_and_request_type_tokens() {
    let e = crate::common::env("cases-search").await;
    insert_case(&e, "CASE-S1", "C-10482", "return", None, None).await;
    insert_case(&e, "CASE-S2", "C-99999", "billing", None, None).await;

    let (s, b) = get_cases(&e, "?q=C-10482").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(case_ids(&b), ["CASE-S1"], "customer id is searched");

    let (_, b) = get_cases(&e, "?q=billing").await;
    assert_eq!(case_ids(&b), ["CASE-S2"], "request type is searched");

    let (_, b) = get_cases(&e, "?q=C-99999%20billing").await;
    assert_eq!(case_ids(&b), ["CASE-S2"], "two tokens match the same row");

    let (_, b) = get_cases(&e, "?q=C-10482%20billing").await;
    assert!(
        b["items"].as_array().unwrap().is_empty(),
        "tokens are ANDed across fields"
    );
}

#[tokio::test]
async fn cases_search_never_treats_percent_or_underscore_as_wildcards() {
    let e = crate::common::env("cases-search-wildcard").await;
    for c in ["CASE-AB", "CASE-AXB", "CASE-AYB"] {
        insert_case(&e, c, "C-1", "return", None, None).await;
    }
    // `%25` is a literal `%`: the token becomes `case-ab`, which must not match `CASE-AXB`/`CASE-AYB`.
    let (s, b) = get_cases(&e, "?q=CASE-A%25B").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(case_ids(&b), ["CASE-AB"]);
    // `_` is LIKE's single-character wildcard and is stripped as well.
    let (_, b) = get_cases(&e, "?q=CASE-A_B").await;
    assert_eq!(case_ids(&b), ["CASE-AB"]);
    // A query of only wildcard characters has no tokens and therefore filters nothing.
    let (_, b) = get_cases(&e, "?q=%25&sort=case&dir=asc").await;
    assert_eq!(case_ids(&b), ["CASE-AB", "CASE-AXB", "CASE-AYB"]);
}

#[tokio::test]
async fn cases_request_type_filter_is_exact() {
    let e = crate::common::env("cases-request-type").await;
    insert_case(&e, "CASE-RT1", "C-1", "return", None, None).await;
    insert_case(&e, "CASE-RT2", "C-1", "returning", None, None).await;
    insert_case(&e, "CASE-RT3", "C-2", "billing", None, None).await;

    let (s, b) = get_cases(&e, "?request_type=return").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(case_ids(&b), ["CASE-RT1"], "`returning` is not `return`");

    let (_, b) = get_cases(&e, "?request_type=return&q=C-1").await;
    assert_eq!(
        case_ids(&b),
        ["CASE-RT1"],
        "the exact filter narrows the search"
    );

    let (_, b) = get_cases(&e, "?request_type=return&q=CASE-RT2").await;
    assert!(b["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn cases_sort_uses_the_header_keys_and_puts_nulls_last() {
    let e = crate::common::env("cases-sort").await;
    insert_case(&e, "CASE-M1", "C-1", "return", Some("completed"), Some(300)).await;
    insert_case(&e, "CASE-M2", "C-1", "return", None, None).await;
    insert_case(&e, "CASE-M3", "C-1", "return", Some("failed"), Some(100)).await;

    let (s, b) = get_cases(&e, "?sort=case").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(case_ids(&b), ["CASE-M1", "CASE-M2", "CASE-M3"]);
    let (_, b) = get_cases(&e, "?sort=case&dir=desc").await;
    assert_eq!(case_ids(&b), ["CASE-M3", "CASE-M2", "CASE-M1"]);

    // time is duration_ms: 100, 300, then the null duration last in both directions
    let (_, b) = get_cases(&e, "?sort=time&dir=asc").await;
    assert_eq!(case_ids(&b), ["CASE-M3", "CASE-M1", "CASE-M2"]);
    let (_, b) = get_cases(&e, "?sort=time&dir=desc").await;
    assert_eq!(case_ids(&b), ["CASE-M1", "CASE-M3", "CASE-M2"]);

    // outcome is compared lowercased; a missing outcome stays last in both directions
    let (_, b) = get_cases(&e, "?sort=outcome&dir=asc").await;
    assert_eq!(case_ids(&b), ["CASE-M1", "CASE-M3", "CASE-M2"]);
    let (_, b) = get_cases(&e, "?sort=outcome&dir=desc").await;
    assert_eq!(case_ids(&b), ["CASE-M3", "CASE-M1", "CASE-M2"]);
}

#[tokio::test]
async fn sorted_pages_continue_with_an_offset_cursor_and_keep_the_order() {
    let e = crate::common::env("cases-sort-page").await;
    for c in ["CASE-G1", "CASE-G2", "CASE-G3"] {
        insert_case(&e, c, "C-1", "return", None, None).await;
    }
    let (s, p1) = get_cases(&e, "?sort=case&dir=asc&limit=2").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(case_ids(&p1), ["CASE-G1", "CASE-G2"]);
    let next = p1["next"].as_str().unwrap();
    assert_eq!(next, "o:2");
    let (_, p2) = get_cases(&e, &format!("?sort=case&dir=asc&limit=2&before={next}")).await;
    assert_eq!(case_ids(&p2), ["CASE-G3"]);
    assert!(p2["next"].is_null(), "a short page ends the list");

    // the offset cursor belongs to the sorted path only
    let (s, b) = get_cases(&e, "?before=o:2").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_cursor");
}

#[tokio::test]
async fn unknown_sort_or_direction_is_400() {
    let e = crate::common::env("cases-sort-bad").await;
    let (s, b) = get_cases(&e, "?sort=nope").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_sort");

    let (s, b) = get_cases(&e, "?sort=case&dir=sideways").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_dir");

    let (s, b) = get_cases(&e, "?sort=case&before=nope").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_cursor");
}
