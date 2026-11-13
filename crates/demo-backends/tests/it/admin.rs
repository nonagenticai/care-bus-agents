use crate::common::{TOKEN, call};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn reset_and_scenarios() {
    let (app, _db, _s) = crate::common::app(1).await;
    call(
        &app,
        Method::POST,
        "/api/payments/PAY-3399/refunds",
        Some(json!({"amount": 229.0, "idempotency_key": "k"})),
        Some(TOKEN),
    )
    .await;
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/admin/reset",
        Some(json!({"seed": 1})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["customers"], 501);
    let (s, _) = call(
        &app,
        Method::POST,
        "/api/payments/PAY-3399/refunds",
        Some(json!({"amount": 229.0, "idempotency_key": "k"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CREATED,
        "refund state is gone after reset; the fixed payment survives"
    );
    // Runtime-created state is wiped by reset; fixed ids survive.
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/orders/ORD-58213/rmas",
        Some(json!({"reason": "too big", "idempotency_key": "reset-probe"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(v["rma_id"], "RMA-41001");
    let (s, _) = call(
        &app,
        Method::GET,
        "/api/admin/records/rmas/RMA-41001",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "the RMA exists before reset");
    call(&app, Method::POST, "/api/admin/reset", None, Some(TOKEN)).await;
    let (s, v) = call(
        &app,
        Method::GET,
        "/api/admin/records/rmas/RMA-41001",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "reset removed the RMA");
    assert_eq!(v["error"]["code"], "not_found");
    let (s, _) = call(
        &app,
        Method::GET,
        "/api/orders/ORD-58213",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "fixed ids survive reset");

    // Reset clears injected faults.
    let (s, _) = call(
        &app,
        Method::PUT,
        "/api/admin/faults",
        Some(json!({"tool": "delivery_track_shipment", "mode": "error", "count": 5})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    call(&app, Method::POST, "/api/admin/reset", None, Some(TOKEN)).await;
    let (_, v) = call(&app, Method::GET, "/api/admin/faults", None, Some(TOKEN)).await;
    assert_eq!(v, json!({"faults": {}}), "reset clears faults");

    let (_, v) = call(&app, Method::GET, "/api/admin/scenarios", None, Some(TOKEN)).await;
    assert_eq!(v["scenarios"].as_array().unwrap().len(), 4);

    let (s, _) = call(
        &app,
        Method::POST,
        "/api/admin/scenarios/late-parcel",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = call(
        &app,
        Method::GET,
        "/api/shipments/SHP-88123",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["status"], "delayed");

    call(
        &app,
        Method::POST,
        "/api/admin/scenarios/old-order-return",
        None,
        Some(TOKEN),
    )
    .await;
    let (_, v) = call(
        &app,
        Method::GET,
        "/api/rules/return-window?order_id=ORD-58213",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["eligible"], false, "headphones now delivered 45 days ago");
    let (_, v) = call(
        &app,
        Method::GET,
        "/api/shipments/SHP-87410",
        None,
        Some(TOKEN),
    )
    .await;
    assert!(
        v["delivered_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-08-31 "),
        "shipment agrees with the order: {v}"
    );

    call(
        &app,
        Method::POST,
        "/api/admin/scenarios/carrier-timeout",
        None,
        Some(TOKEN),
    )
    .await;
    let (_, v) = call(&app, Method::GET, "/api/admin/faults", None, Some(TOKEN)).await;
    assert_eq!(v["faults"]["delivery_track_shipment"]["mode"], "timeout");

    let (s, _) = call(
        &app,
        Method::POST,
        "/api/admin/scenarios/nope",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reset_body_handling() {
    let (app, _db, _s) = crate::common::app(1).await;
    let (s, _) = call(&app, Method::POST, "/api/admin/reset", None, Some(TOKEN)).await;
    assert_eq!(s, StatusCode::OK, "no body resets with the configured seed");

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/reset")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"seed":"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "invalid_input");
}
