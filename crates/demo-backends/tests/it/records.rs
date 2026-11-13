use crate::common::{TOKEN, call};
use axum::http::{Method, StatusCode};
use serde_json::json;

#[tokio::test]
async fn record_editor_is_safe() {
    let (app, db, _s) = crate::common::app(1).await;
    let (s, v) = call(
        &app,
        Method::GET,
        "/api/admin/records/orders?q=ORD-58&limit=10",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["primary_key"], "order_id");
    assert!(
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["order_id"] == "ORD-58790")
    );

    let (s, v) = call(
        &app,
        Method::PUT,
        "/api/admin/records/orders/ORD-58790",
        Some(json!({"status": "delivered", "delivered_on": "2026-10-14"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["delivered_on"], "2026-10-14");
    assert_eq!(
        v["item"], "Philips Airfryer XXL HD9285",
        "unpatched columns keep their value"
    );

    // Postgres error classes reach DemoError::from_db: asserted, not assumed.
    let (s, v) = call(
        &app,
        Method::PUT,
        "/api/admin/records/orders/ORD-58790",
        Some(json!({"delivered_on": "2026-13-45"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "bad date: {v}");
    assert_eq!(v["error"]["code"], "invalid_input");
    let (s, v) = call(
        &app,
        Method::PUT,
        "/api/admin/records/orders/ORD-58790",
        Some(json!({"status": "lost"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "CHECK constraint violation is a 422: {v}"
    );
    assert_eq!(v["error"]["code"], "invalid_input");
    let (s, v) = call(
        &app,
        Method::PUT,
        "/api/admin/records/orders/ORD-58790",
        Some(json!({"amount_cents": 1})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("amount_cents")
    );
    let (s, v) = call(
        &app,
        Method::PUT,
        "/api/admin/records/customers/C-10482",
        Some(json!({"name": null})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "NOT NULL violation is a 422: {v}"
    );
    let (s, _) = call(
        &app,
        Method::PUT,
        "/api/admin/records/orders/ORD-NOPE",
        Some(json!({"status": "delivered"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let still: String =
        sqlx::query_scalar("SELECT delivered_on::text FROM orders WHERE order_id='ORD-58790'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(still, "2026-10-14", "failed edits change nothing");

    let (s, v) = call(
        &app,
        Method::PUT,
        "/api/admin/records/refunds/REF-1",
        Some(json!({"amount_cents": 1})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(v["error"]["code"], "read_only_table");
    for t in ["pg_user", "order_lines", "shipment_events"] {
        let (s, _) = call(
            &app,
            Method::GET,
            &format!("/api/admin/records/{t}"),
            None,
            Some(TOKEN),
        )
        .await;
        assert_eq!(
            s,
            StatusCode::NOT_FOUND,
            "only the 8 allow-listed tables: {t}"
        );
    }
}
