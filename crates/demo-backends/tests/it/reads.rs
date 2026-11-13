use crate::common::{TOKEN, call};
use axum::http::{Method, StatusCode};

#[tokio::test]
async fn customer_search_and_start_data() {
    let (app, _db, _s) = crate::common::app(1).await;
    let (s, v) = call(
        &app,
        Method::GET,
        "/api/customers/C-10482",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["name"], "Anna de Vries");
    assert_eq!(v["tier"], "Gold");

    let (s, v) = call(
        &app,
        Method::GET,
        "/api/customers/C-99999",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "not_found");

    let (_, v) = call(
        &app,
        Method::GET,
        "/api/customers?q=devries",
        None,
        Some(TOKEN),
    )
    .await;
    assert!(
        v["customers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["customer_id"] == "C-10482")
    );

    let (s, v) = call(
        &app,
        Method::GET,
        "/api/customers/C-10482/start-data",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = v["orders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["order_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["ORD-58790", "ORD-58213"],
        "newest first; the June drill is older than 90 days"
    );
    assert_eq!(v["deliveries"].as_array().unwrap().len(), 2);
    assert_eq!(v["payments"].as_array().unwrap().len(), 3);
    assert_eq!(v["orders"][0]["amount"], 229.0);
    assert!(v["orders"][0]["delivered_on"].is_null());
}

#[tokio::test]
async fn orders_payments_shipments() {
    let (app, _db, _s) = crate::common::app(1).await;
    let (s, v) = call(
        &app,
        Method::GET,
        "/api/orders/ORD-58213",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["delivered_on"], "2026-10-05");
    assert_eq!(v["lines"][0]["sku"], "SO-WH1000XM6");
    assert_eq!(v["lines"][0]["unit_price"], 379.0);

    let (_, v) = call(
        &app,
        Method::GET,
        "/api/orders/ORD-58790/payments",
        None,
        Some(TOKEN),
    )
    .await;
    let p = v["payments"].as_array().unwrap();
    assert_eq!(p.len(), 2);
    assert_eq!(
        p[1]["at_epoch_s"].as_i64().unwrap() - p[0]["at_epoch_s"].as_i64().unwrap(),
        38
    );

    let (_, v) = call(
        &app,
        Method::GET,
        "/api/shipments/SHP-88123",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["carrier"], "PostNL");
    assert_eq!(v["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["last_event"],
        "Sorted, waiting for route at Sorting centre Nieuwegein, 2026-10-14 06:12"
    );
    assert_eq!(v["events"][1]["at"], "2026-10-14 06:12");

    let (_, v) = call(
        &app,
        Method::GET,
        "/api/shipments/SHP-87410",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["delivered_at"], "2026-10-05 13:42");
    let (_, v) = call(
        &app,
        Method::GET,
        "/api/shipments/SHP-88123",
        None,
        Some(TOKEN),
    )
    .await;
    assert!(v["delivered_at"].is_null(), "undelivered stays null");

    let (_, v) = call(
        &app,
        Method::GET,
        "/api/orders?customer_id=C-10482",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(v["orders"].as_array().unwrap().len(), 3);

    let (s, _) = call(
        &app,
        Method::GET,
        "/api/shipments/SHP-00000",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
