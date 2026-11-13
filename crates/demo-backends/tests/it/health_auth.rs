use axum::http::{Method, StatusCode};

#[tokio::test]
async fn health_is_open_and_api_needs_the_token() {
    let (app, _db, _s) = crate::common::app(1).await;
    let (s, v) = crate::common::call(&app, Method::GET, "/health", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["db"], "ok");
    let (s, v) = crate::common::call(&app, Method::GET, "/api/customers/C-10482", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"]["code"], "unauthorized");
    let (s, _) = crate::common::call(
        &app,
        Method::GET,
        "/api/customers/C-10482",
        None,
        Some("wrong"),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = crate::common::call(
        &app,
        Method::GET,
        "/api/customers/C-10482",
        None,
        Some(crate::common::TOKEN),
    )
    .await;
    assert_ne!(
        s,
        StatusCode::UNAUTHORIZED,
        "right token passes the gate (404 until Task 6 is fine)"
    );
}

#[tokio::test]
async fn unknown_paths_and_wrong_methods_are_json_and_authenticated() {
    let (app, _db, _s) = crate::common::app(1).await;
    let (s, v) = crate::common::call(&app, Method::GET, "/api/nope", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"]["code"], "unauthorized");
    let (s, v) = crate::common::call(
        &app,
        Method::GET,
        "/api/nope",
        None,
        Some(crate::common::TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "not_found");
    let (s, v) = crate::common::call(
        &app,
        Method::DELETE,
        "/api/customers/C-10482",
        None,
        Some(crate::common::TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(v["error"]["code"], "method_not_allowed");
    let (s, v) =
        crate::common::call(&app, Method::DELETE, "/api/customers/C-10482", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"]["code"], "unauthorized");
}
