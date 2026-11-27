//! `POST /api/jobs/{job}/context-spaces`: a job defines its own space (origin `tenant`), which the
//! qualifier then sees in its catalog. The base release stays untouched; PUT still refines only an
//! existing space.

use axum::http::StatusCode;
use serde_json::{Value, json};

const NEW: &str = "website:ebook";

fn body() -> Value {
    json!({
        "key": NEW,
        "area": "faq",
        "label": "Ebook",
        "request_type": "ebook",
        "required": ["email"],
        "entities": [],
        "procedure": "ebook_request"
    })
}

async fn post(e: &crate::common::TestEnv, token: &str, body: Value) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/context-spaces"),
            Some(token),
            body,
        ),
    )
    .await
}

async fn spaces(e: &crate::common::TestEnv, token: &str) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/context-spaces"), Some(token)),
    )
    .await
}

#[tokio::test]
async fn creates_a_tenant_space_the_effective_catalog_serves() {
    let e = crate::common::env("context_space_create").await;
    let (status, created) = post(&e, &crate::common::admin(), body()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["key"], NEW);
    assert_eq!(created["origin"], "tenant");
    assert_eq!(created["request_type"], "ebook");

    let (status, list) = spaces(&e, &crate::common::admin()).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let item = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["key"] == NEW)
        .expect("the new space is in the effective catalog");
    assert_eq!(item["request_type"], "ebook");
    assert_eq!(item["required"][0], "email");
    assert_eq!(item["origin"], "tenant");
}

#[tokio::test]
async fn only_a_job_admin_may_create_a_space() {
    let e = crate::common::env("context_space_create_role").await;
    let (status, _) = post(&e, &crate::common::user(), body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_base_key_or_a_taken_request_type_is_refused() {
    let e = crate::common::env("context_space_create_conflict").await;
    let admin = crate::common::admin();

    let mut base = body();
    base["key"] = json!("retail:faq:general");
    base["request_type"] = json!("faq");
    let (status, error) = post(&e, &admin, base).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "space_exists");

    let mut taken = body();
    taken["key"] = json!("website:other");
    taken["request_type"] = json!("faq");
    let (status, error) = post(&e, &admin, taken).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "request_type_taken");
}

#[tokio::test]
async fn unknown_entities_and_a_bad_area_are_refused() {
    let e = crate::common::env("context_space_create_invalid").await;
    let admin = crate::common::admin();

    let mut unknown = body();
    unknown["entities"] = json!(["NotAnEntity"]);
    let (status, error) = post(&e, &admin, unknown).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(error["error"]["code"], "unknown_entity");

    let mut area = body();
    area["area"] = json!("website");
    let (status, error) = post(&e, &admin, area).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(error["error"]["code"], "invalid_body");
}

#[tokio::test]
async fn put_of_an_absent_space_still_refuses_to_create() {
    let e = crate::common::env("context_space_create_put").await;
    let (status, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j(&format!("/context-spaces/{NEW}")),
            Some(&crate::common::admin()),
            json!({"label": "Ebook"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
