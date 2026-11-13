//! Metering (plan 15): a chat turn's model usage always lands in `usage_ledger` in the job's scope (owner visibility);
//! `metering_outbox` is written only with METERING_EMIT_ENABLED, and the drainer sends each outbox row to the costs
//! stream once. (Emit stays off while license-authz meters the LLM URL; it is the billing meter.)

use axum::http::StatusCode;
use carebus::interaction::LlmUsage;
use carebus_api::chat_q::AnyQualifier;
use serde_json::json;
use uuid::Uuid;

fn usage() -> LlmUsage {
    LlmUsage {
        provider: "vllm".into(),
        model: "local-llm".into(),
        input_tokens: 812,
        output_tokens: 64,
        calls: 1,
        latency_ms: 900,
    }
}

async fn metered_env(test: &str, emit: bool) -> crate::common::TestEnv {
    crate::common::env_with(test, |c| {
        if emit {
            c.metering.emit_enabled = true;
            c.metering.redis_url = c.redis_url.clone();
            c.metering.costs_stream = format!("{}billing:costs", c.prefix);
        }
    })
    .await
}

/// One chat turn whose qualifier records `usage()`; returns the conversation id.
async fn metered_turn(e: &crate::common::TestEnv) -> String {
    let mut state = e.state.clone();
    state.qualifier = std::sync::Arc::new(AnyQualifier::Metered(usage()));
    let app = carebus_api::router(state);
    let t = crate::common::user();
    let (s, c) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let (s, b) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": "I want to return my headphones"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    id
}

#[derive(sqlx::FromRow)]
struct LedgerRow {
    tenant_id: Uuid,
    job_id: Uuid,
    input_tokens: i64,
    output_tokens: i64,
    model: String,
    conversation_id: Option<String>,
    event_key: Uuid,
}

async fn counts(e: &crate::common::TestEnv) -> (i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM usage_ledger), (SELECT count(*) FROM metering_outbox)",
    )
    .fetch_one(crate::common::pool(e))
    .await
    .unwrap()
}

async fn boot_job(e: &crate::common::TestEnv) -> Uuid {
    sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1::uuid AND key = $2")
        .bind(crate::common::BOOT_TENANT)
        .bind(crate::common::BOOT_JOB)
        .fetch_one(crate::common::pool(e))
        .await
        .unwrap()
}

#[tokio::test]
async fn emit_off_writes_the_ledger_but_no_outbox() {
    let e = metered_env("metering-off", false).await;
    let conv = metered_turn(&e).await;
    assert_eq!(counts(&e).await, (1, 0));
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job = boot_job(&e).await;
    let r: LedgerRow = sqlx::query_as(
        "SELECT tenant_id, job_id, input_tokens, output_tokens, model, conversation_id, event_key FROM usage_ledger",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!((r.tenant_id, r.job_id), (boot, job));
    assert_eq!(
        (r.input_tokens, r.output_tokens, r.model.as_str()),
        (812, 64, "local-llm")
    );
    assert_eq!(r.conversation_id.as_deref(), Some(conv.as_str()));
    assert!(carebus_api::metering::spawn_drainer(e.state.clone()).is_none());
}

#[tokio::test]
async fn a_metered_turn_writes_ledger_and_outbox_in_the_job_scope() {
    let e = metered_env("metering-ledger", true).await;
    let conv = metered_turn(&e).await;
    assert_eq!(counts(&e).await, (1, 1));
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job = boot_job(&e).await;
    let r: LedgerRow = sqlx::query_as(
        "SELECT tenant_id, job_id, input_tokens, output_tokens, model, conversation_id, event_key FROM usage_ledger",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    let key = r.event_key;
    assert_eq!((r.tenant_id, r.job_id), (boot, job));
    assert_eq!(
        (r.input_tokens, r.output_tokens, r.model.as_str()),
        (812, 64, "local-llm")
    );
    assert_eq!(r.conversation_id.as_deref(), Some(conv.as_str()));
    let (status, fields, o_tenant, o_job): (String, serde_json::Value, Uuid, Uuid) =
        sqlx::query_as(
            "SELECT status, fields, tenant_id, job_id FROM metering_outbox WHERE event_key = $1",
        )
        .bind(key)
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(status, "pending");
    assert_eq!((o_tenant, o_job), (boot, job));
    assert_eq!(fields["tenant_id"], crate::common::BOOT_TENANT);
    assert_eq!(fields["job"], crate::common::BOOT_JOB);
    assert_eq!(fields["event_key"], key.to_string());
    assert_eq!(fields["service_key"], "care-bus");
    assert_eq!(fields["correlation_id"], conv);
}

async fn xrange(
    conn: &mut redis::aio::ConnectionManager,
    stream: &str,
) -> Vec<redis::streams::StreamId> {
    let r: redis::streams::StreamRangeReply = redis::cmd("XRANGE")
        .arg(stream)
        .arg("-")
        .arg("+")
        .query_async(conn)
        .await
        .unwrap();
    r.ids
}

#[tokio::test]
async fn drainer_sends_each_event_once() {
    let e = metered_env("metering-drain", true).await;
    let conv = metered_turn(&e).await;
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job = boot_job(&e).await;
    let client = redis::Client::open(e.state.cfg.metering.redis_url.as_str()).unwrap();
    let mut conn = redis::aio::ConnectionManager::new(client).await.unwrap();
    let d = carebus_api::metering::drain_once(&e.state, boot, job, &mut conn)
        .await
        .unwrap();
    assert_eq!((d.sent, d.failed), (1, 0));
    let stream = e.state.cfg.metering.costs_stream.clone();
    let entries = xrange(&mut conn, &stream).await;
    assert_eq!(entries.len(), 1);
    let field = |k: &str| -> String {
        entries[0]
            .get::<String>(k)
            .unwrap_or_else(|| panic!("no field {k}"))
    };
    assert_eq!(field("tenant_id"), crate::common::BOOT_TENANT);
    assert_eq!(field("job"), crate::common::BOOT_JOB);
    assert_eq!(field("service_key"), "care-bus");
    assert_eq!(field("input_tokens"), "812");
    assert_eq!(field("correlation_id"), conv);
    let (status, sent): (String, bool) =
        sqlx::query_as("SELECT status, sent_at IS NOT NULL FROM metering_outbox")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!((status.as_str(), sent), ("sent", true));
    // a second tick sends nothing more
    let d = carebus_api::metering::drain_once(&e.state, boot, job, &mut conn)
        .await
        .unwrap();
    assert_eq!(d, carebus_api::metering::Drained::default());
    assert_eq!(xrange(&mut conn, &stream).await.len(), 1);
}
