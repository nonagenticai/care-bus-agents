//! `GET /api/jobs/{job}/usage`: the owner's view of `usage_ledger`, summed and grouped per UTC day, model and provider,
//! for this job only (RLS); another tenant's and another job's rows are invisible.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

/// One ledger row through the owner pool (RLS does not apply to it).
async fn ledger(
    e: &crate::common::TestEnv,
    tenant: Uuid,
    job: Uuid,
    at: &str,
    model: &str,
    tokens: (i64, i64),
) {
    sqlx::query(
        "INSERT INTO usage_ledger (tenant_id, job_id, event_key, occurred_at, source, provider, model,
                                   input_tokens, output_tokens, calls, latency_ms)
         VALUES ($1, $2, gen_random_uuid(), $3::timestamptz, 'chat_qualifier', 'vllm', $4, $5, $6, 1, 100)",
    )
    .bind(tenant)
    .bind(job)
    .bind(at)
    .bind(model)
    .bind(tokens.0)
    .bind(tokens.1)
    .execute(crate::common::pool(e))
    .await
    .unwrap();
}

async fn boot_job(e: &crate::common::TestEnv) -> Uuid {
    sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1::uuid AND key = $2")
        .bind(crate::common::BOOT_TENANT)
        .bind(crate::common::BOOT_JOB)
        .fetch_one(crate::common::pool(e))
        .await
        .unwrap()
}

/// OTHER tenant with a job of the same key and its member `u-acme`; returns (job id, `u-acme`'s token).
async fn provision_other(e: &crate::common::TestEnv) -> (Uuid, String) {
    let db = crate::common::pool(e);
    let t = crate::common::insert_tenant(db, crate::common::OTHER_TENANT).await;
    let job = crate::common::insert_job(db, t, crate::common::BOOT_JOB).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-acme', $1, 'tenant_admin', 'Acme Admin', 'test')",
    )
    .bind(t)
    .execute(db)
    .await
    .unwrap();
    (job, crate::common::token_for("u-acme", &[]))
}

const WINDOW: &str = "/usage?from=2026-09-01T00:00:00Z&to=2026-09-03T00:00:00Z";

#[tokio::test]
async fn usage_sums_per_day_for_this_job_only() {
    let e = crate::common::env("usage-sums").await;
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job = boot_job(&e).await;
    let other_job = crate::common::insert_job(crate::common::pool(&e), boot, "other_job").await;
    let (acme_job, acme) = provision_other(&e).await;
    let other: Uuid = crate::common::OTHER_TENANT.parse().unwrap();

    ledger(
        &e,
        boot,
        job,
        "2026-09-01T08:00:00Z",
        "local-llm",
        (100, 10),
    )
    .await;
    ledger(
        &e,
        boot,
        job,
        "2026-09-01T23:30:00Z",
        "local-llm",
        (200, 20),
    )
    .await;
    ledger(
        &e,
        boot,
        job,
        "2026-09-02T09:00:00Z",
        "local-llm",
        (300, 30),
    )
    .await;
    ledger(&e, boot, job, "2026-09-02T10:00:00Z", "big-llm", (400, 40)).await;
    // outside the window, in another job, in another tenant: all invisible here
    ledger(&e, boot, job, "2026-08-31T23:59:59Z", "local-llm", (9, 9)).await;
    ledger(&e, boot, job, "2026-09-03T00:00:00Z", "local-llm", (9, 9)).await;
    ledger(
        &e,
        boot,
        other_job,
        "2026-09-01T12:00:00Z",
        "local-llm",
        (5000, 500),
    )
    .await;
    ledger(
        &e,
        other,
        acme_job,
        "2026-09-01T12:00:00Z",
        "local-llm",
        (7000, 700),
    )
    .await;

    let t = crate::common::user();
    let (s, u) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j(WINDOW), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!(u["from"], "2026-09-01T00:00:00.000Z");
    assert_eq!(u["to"], "2026-09-03T00:00:00.000Z");
    assert_eq!(
        u["total"],
        json!({"calls": 4, "input_tokens": 1000, "output_tokens": 100})
    );
    assert_eq!(
        u["by_day"],
        json!([
            {"day": "2026-09-01", "model": "local-llm", "provider": "vllm", "calls": 2, "input_tokens": 300, "output_tokens": 30},
            {"day": "2026-09-02", "model": "big-llm", "provider": "vllm", "calls": 1, "input_tokens": 400, "output_tokens": 40},
            {"day": "2026-09-02", "model": "local-llm", "provider": "vllm", "calls": 1, "input_tokens": 300, "output_tokens": 30},
        ])
    );

    // the other tenant's member, on the same job key, sees only its own row
    let (s, u) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j(WINDOW), Some(&acme)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!(
        u["total"],
        json!({"calls": 1, "input_tokens": 7000, "output_tokens": 700})
    );
    assert_eq!(u["by_day"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn usage_defaults_to_the_last_30_days() {
    let e = crate::common::env("usage-default").await;
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job = boot_job(&e).await;
    let recent = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    let old = (chrono::Utc::now() - chrono::Duration::days(40)).to_rfc3339();
    ledger(&e, boot, job, &recent, "local-llm", (100, 10)).await;
    ledger(&e, boot, job, &old, "local-llm", (900, 90)).await;
    let t = crate::common::user();
    let (s, u) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/usage"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!(
        u["total"],
        json!({"calls": 1, "input_tokens": 100, "output_tokens": 10})
    );
    let from = chrono::DateTime::parse_from_rfc3339(u["from"].as_str().unwrap()).unwrap();
    let to = chrono::DateTime::parse_from_rfc3339(u["to"].as_str().unwrap()).unwrap();
    assert_eq!((to - from).num_days(), 30);
}

#[tokio::test]
async fn usage_rejects_a_bad_window() {
    let e = crate::common::env("usage-bad").await;
    let t = crate::common::user();
    for (q, code) in [
        ("/usage?from=yesterday", "invalid_from"),
        ("/usage?to=2026-13-01T00:00:00Z", "invalid_to"),
        (
            "/usage?from=2026-09-03T00:00:00Z&to=2026-09-01T00:00:00Z",
            "invalid_window",
        ),
    ] {
        let (s, b) =
            crate::common::send(&e.app, crate::common::get(&crate::common::j(q), Some(&t))).await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::BAD_REQUEST, json!(code)),
            "{q}"
        );
    }
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/usage"), None),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
