//! `POST /api/internal/answer-context`: the website answer stage's settings and provider.
//! Service-token only; the response carries a provider token, so it must be `no-store`.

use axum::http::StatusCode;
use serde_json::{Value, json};

fn req(token: &str, body: Value) -> axum::http::Request<axum::body::Body> {
    crate::common::json("POST", "/api/internal/answer-context", Some(token), body)
}

fn input(job: &str) -> Value {
    json!({"tenant_id": crate::common::BOOT_TENANT, "job": job})
}

#[tokio::test]
async fn a_service_token_reads_the_effective_settings_and_the_reply_is_no_store() {
    use tower::ServiceExt;

    let e = crate::common::env("answer-context").await;
    let (status, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/settings"),
            Some(&crate::common::admin()),
            json!({"preamble": "Be kind.", "max_tokens": "777"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let resp = e
        .app
        .clone()
        .oneshot(req("svc-test-token", input(crate::common::BOOT_JOB)))
        .await
        .expect("router is infallible");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("cache-control").map(|v| v.as_bytes()),
        Some(b"no-store".as_slice())
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("the body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["settings"]["preamble"], "Be kind.");
    assert_eq!(body["settings"]["max_tokens"], "777");
    assert_eq!(body["settings"]["actor"], "");
    // The test config seeds no platform row and no `answer` binding.
    assert!(body["provider"].is_null(), "{body}");
}

#[tokio::test]
async fn only_a_service_token_and_a_known_job_are_accepted() {
    let e = crate::common::env("answer-context-auth").await;

    let (status, error) = crate::common::send(
        &e.app,
        req(&crate::common::user(), input(crate::common::BOOT_JOB)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");

    let (status, error) = crate::common::send(&e.app, req("svc-test-token", input("nope"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
    assert_eq!(error["error"]["code"], "not_found");
}
