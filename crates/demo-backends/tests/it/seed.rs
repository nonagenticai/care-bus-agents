#[tokio::test]
async fn migration_creates_all_tables() {
    let db = crate::common::TestDb::start().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_schema='public' AND table_name = ANY($1)",
    )
    .bind(&["customers","orders","order_lines","shipments","shipment_events","payments","refunds","rmas","warranty_claims","communications"][..])
    .fetch_one(&db.pool).await.unwrap();
    assert_eq!(n, 10);
    let next: String = sqlx::query_scalar("SELECT 'RMA-' || nextval('rma_seq')")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(next, "RMA-41001");
}

use demo_backends::seed;

#[tokio::test]
async fn reset_loads_the_design_customer_exactly() {
    let db = crate::common::TestDb::start().await;
    let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
    seed::reset(&db.pool, 1, today).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM customers WHERE customer_id='C-10482'")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(name, "Anna de Vries");
    let orders: Vec<(String, String, i64, Option<chrono::NaiveDate>)> = sqlx::query_as(
        "SELECT order_id, status, amount_cents, delivered_on FROM orders WHERE customer_id='C-10482' ORDER BY order_id")
        .fetch_all(&db.pool).await.unwrap();
    assert_eq!(
        orders,
        vec![
            (
                "ORD-57102".into(),
                "delivered".into(),
                14900,
                chrono::NaiveDate::from_ymd_opt(2026, 6, 18)
            ),
            (
                "ORD-58213".into(),
                "delivered".into(),
                37900,
                chrono::NaiveDate::from_ymd_opt(2026, 10, 5)
            ),
            ("ORD-58790".into(), "in_transit".into(), 22900, None),
        ]
    );
    let gap: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM (SELECT captured_at FROM payments WHERE payment_id='PAY-3399') - (SELECT captured_at FROM payments WHERE payment_id='PAY-3398'))::float8")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(gap, 38.0);
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shipment_events WHERE shipment_id='SHP-88123'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(events, 2);
}

#[tokio::test]
async fn reset_twice_is_clean_and_restarts_id_sequences() {
    let db = crate::common::TestDb::start().await;
    let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
    seed::reset(&db.pool, 1, today).await.unwrap();
    sqlx::query("INSERT INTO rmas (rma_id, order_id, reason, deadline, idempotency_key) VALUES ('RMA-' || nextval('rma_seq'), 'ORD-58213', 'x', '2026-11-04', 'k1')")
        .execute(&db.pool).await.unwrap();
    let r = seed::reset(&db.pool, 1, today).await.unwrap();
    let rmas: i64 = sqlx::query_scalar("SELECT count(*) FROM rmas")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rmas, 0);
    let next: String = sqlx::query_scalar("SELECT 'RMA-' || nextval('rma_seq')")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(next, "RMA-41001");
    assert!(r.customers >= 1);
}
async fn fingerprint(pool: &sqlx::PgPool) -> String {
    sqlx::query_scalar("SELECT md5(string_agg(order_id || ':' || customer_id || ':' || amount_cents || ':' || status, ',' ORDER BY order_id)) FROM orders")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn generator_is_deterministic_per_seed_and_plausible() {
    let db = crate::common::TestDb::start().await;
    let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
    let r = seed::reset(&db.pool, 7, today).await.unwrap();
    assert_eq!(r.customers, 501, "500 generated + the fixed customer");
    assert!(r.orders > 600 && r.orders < 2000, "orders: {}", r.orders);
    let a = fingerprint(&db.pool).await;
    seed::reset(&db.pool, 7, today).await.unwrap();
    assert_eq!(a, fingerprint(&db.pool).await, "same seed, same data");
    seed::reset(&db.pool, 8, today).await.unwrap();
    assert_ne!(
        a,
        fingerprint(&db.pool).await,
        "different seed, different data"
    );
    let future: i64 =
        sqlx::query_scalar("SELECT count(*) FROM orders WHERE placed > $1 OR delivered_on > $1")
            .bind(today)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(future, 0, "nothing happens after DEMO_TODAY");
    let orphan: i64 = sqlx::query_scalar("SELECT count(*) FROM orders o WHERE o.shipment_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM shipments s WHERE s.shipment_id = o.shipment_id)").fetch_one(&db.pool).await.unwrap();
    assert_eq!(orphan, 0);
}
