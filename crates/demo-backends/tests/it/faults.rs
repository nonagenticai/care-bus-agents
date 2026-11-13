use crate::common::{TOKEN, call};
use axum::http::{Method, StatusCode};
use serde_json::json;
use std::time::Instant;

#[tokio::test]
async fn fault_count_affects_exactly_n_calls() {
    let (app, _db, _s) = crate::common::app(1).await;
    let (s, _) = call(
        &app,
        Method::PUT,
        "/api/admin/faults",
        Some(json!({"tool": "delivery_track_shipment", "mode": "error", "count": 1})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // GET /api/admin/faults (read by carebus-api's GET /api/demo/faults for the demo-data UI)
    let (s, v) = call(&app, Method::GET, "/api/admin/faults", None, Some(TOKEN)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v,
        json!({"faults": {"delivery_track_shipment": {"mode": "error", "remaining": 1}}})
    );
    let (s, v) = call(
        &app,
        Method::GET,
        "/api/shipments/SHP-88123",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["error"]["code"], "fault_injected");
    let (s, _) = call(
        &app,
        Method::GET,
        "/api/shipments/SHP-88123",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "count 1 means only the next call fails");

    call(
        &app,
        Method::PUT,
        "/api/admin/faults",
        Some(json!({"tool": "order_get_order", "mode": "timeout", "count": 1})),
        Some(TOKEN),
    )
    .await;
    let t = Instant::now();
    let (s, _) = call(
        &app,
        Method::GET,
        "/api/orders/ORD-58790",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "a timeout fault is slow, not failed");
    assert!(t.elapsed().as_millis() >= 60, "test config sleeps 60 ms");

    let (s, v) = call(
        &app,
        Method::PUT,
        "/api/admin/faults",
        Some(json!({"tool": "nope", "mode": "error", "count": 1})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("delivery_track_shipment"),
        "error lists valid names"
    );
    let (_, v) = call(&app, Method::GET, "/api/admin/faults", None, Some(TOKEN)).await;
    assert_eq!(v["faults"], json!({}), "used-up faults disappear");
}
