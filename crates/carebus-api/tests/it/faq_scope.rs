//! The customer chat's store scope is the JOB's own bound leaf stores — a website job binds several, each its own
//! strata root, and strata's default routing would be ambiguous — minus the stores excluded from chat and every
//! legal-matter wall (the wall cases live in legal_matters.rs).
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::fakes::{LEAF, OTHER_LEAF};

const SVC: &str = "svc-test-token";

async fn scope(e: &crate::common::TestEnv, body: Value) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json("POST", "/api/internal/faq/scope", Some(SVC), body),
    )
    .await
}

async fn store(e: &crate::common::TestEnv, key: &str, sid: &str) {
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j(&format!("/knowledge-stores/{key}")),
            Some(&crate::common::admin()),
            json!({"label": key, "kind": "leaf", "areas": ["general"], "strata_store_id": sid}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn a_job_with_bound_leaves_searches_exactly_them_minus_the_excluded() {
    let e = crate::common::env("faq_scope_job").await;
    store(&e, "site-core", LEAF).await;
    store(&e, "site-legal", OTHER_LEAF).await;

    // no job named: the tenant-level answer, unchanged (no wall -> strata_rag's default routing)
    let (s, b) = scope(&e, json!({"tenant_id": crate::common::BOOT_TENANT})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"restricted": false}));

    // the job's bound leaves are the scope, canonical and sorted
    let (s, b) = scope(
        &e,
        json!({"tenant_id": crate::common::BOOT_TENANT, "job": crate::common::BOOT_JOB}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"restricted": true, "leaf_ids": [LEAF, OTHER_LEAF]})
    );

    // a store excluded from chat stays bound (the refine route always writes the binding, so it rides along)
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/knowledge-stores/site-legal"),
            Some(&crate::common::admin()),
            json!({"chat_excluded": true, "strata_store_id": OTHER_LEAF}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["chat_excluded"], json!(true));
    assert_eq!(b["strata_store_id"], json!(OTHER_LEAF));

    let (s, b) = scope(
        &e,
        json!({"tenant_id": crate::common::BOOT_TENANT, "job": crate::common::BOOT_JOB}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"restricted": true, "leaf_ids": [LEAF]}));

    // a job with no bound leaf store (an unknown key included) falls back to the tenant-level answer
    for job in ["legal_pages", "nope"] {
        let (s, b) = scope(
            &e,
            json!({"tenant_id": crate::common::BOOT_TENANT, "job": job}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b, json!({"restricted": false}), "{job}");
    }

    // every store excluded: the scope is empty, which the worker reads as "search nothing"
    store(&e, "only", "7f1c9a2e-0000-4000-8000-0000000000a9").await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/knowledge-stores/site-core"),
            Some(&crate::common::admin()),
            json!({"chat_excluded": true, "strata_store_id": LEAF}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = scope(
        &e,
        json!({"tenant_id": crate::common::BOOT_TENANT, "job": crate::common::BOOT_JOB}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"restricted": true, "leaf_ids": ["7f1c9a2e-0000-4000-8000-0000000000a9"]})
    );
}

#[tokio::test]
async fn a_persons_token_is_refused() {
    let e = crate::common::env("faq_scope_job_forbidden").await;
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/internal/faq/scope",
            Some(&crate::common::admin()),
            json!({"tenant_id": crate::common::BOOT_TENANT, "job": crate::common::BOOT_JOB}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
