use crate::common::{TOKEN, call};
use axum::http::{Method, StatusCode};

#[tokio::test]
async fn rules_over_http_use_demo_today() {
    let (app, _db, _s) = crate::common::app(1).await;
    let (_, v) = call(
        &app,
        Method::GET,
        "/api/rules/return-window?order_id=ORD-58213",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["eligible"], true);
    let (_, v) = call(
        &app,
        Method::GET,
        "/api/rules/return-window?order_id=ORD-57102",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["eligible"], false);
    let (_, v) = call(
        &app,
        Method::GET,
        "/api/rules/warranty?order_id=ORD-57102",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["warranty_until"], "2028-06-17");
    let (_, v) = call(
        &app,
        Method::GET,
        "/api/rules/payment-issue?order_id=ORD-58790&error_type=duplicate_charge",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["refund_payment_id"], "PAY-3399");
    let (s, _) = call(
        &app,
        Method::GET,
        "/api/rules/payment-issue?order_id=ORD-58790&error_type=bogus",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn pay_04_ignores_refunded_payments() {
    let (app, _db, _s) = crate::common::app(1).await;
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/payments/PAY-3399/refunds",
        Some(serde_json::json!({"amount": 229.0, "idempotency_key": "r14"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = call(
        &app,
        Method::GET,
        "/api/rules/payment-issue?order_id=ORD-58790&error_type=duplicate_charge",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["eligible"], false,
        "a fully refunded duplicate is no longer a duplicate charge: {v}"
    );
}
