use axum::body::Body;
use axum::http::{Request, StatusCode};

#[tokio::test]
async fn health_reports_redis_and_db_without_auth() {
    let e = crate::common::env("health").await;
    let (status, body) = crate::common::send(&e.app, crate::common::get("/health", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["redis"], "ok");
    assert_eq!(body["db"], "ok");
}

#[tokio::test]
async fn unknown_route_is_a_404_envelope() {
    let e = crate::common::env("fallback").await;
    let (status, body) = crate::common::send(&e.app, crate::common::get("/api/nope", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn cors_preflight_from_allowed_origin_is_answered() {
    let e = crate::common::env_with("cors", |c| {
        c.cors_origins = vec!["https://console.test".into()]
    })
    .await;
    let req = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/health")
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization")
            .body(Body::empty())
            .unwrap()
    };
    let resp = tower::ServiceExt::oneshot(e.app.clone(), req("https://console.test"))
        .await
        .unwrap();
    assert_eq!(
        resp.headers()["access-control-allow-origin"],
        "https://console.test"
    );
    let resp = tower::ServiceExt::oneshot(e.app.clone(), req("https://evil.test"))
        .await
        .unwrap();
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn no_cors_layer_when_origins_unset() {
    let e = crate::common::env("nocors").await;
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("origin", "https://console.test")
        .body(Body::empty())
        .unwrap();
    let resp = tower::ServiceExt::oneshot(e.app.clone(), req)
        .await
        .unwrap();
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn wrong_method_is_a_405_envelope() {
    let e = crate::common::env("method").await;
    let (status, body) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/conversations"), None),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    assert_eq!(body["error"]["code"], "method_not_allowed");
    assert!(body["error"]["message"].is_string());
}
