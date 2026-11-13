use crate::common::{TOKEN, call};
use axum::http::{Method, StatusCode};
use serde_json::json;

#[tokio::test]
async fn rma_is_idempotent_per_key() {
    let (app, db, _s) = crate::common::app(1).await;
    let body = json!({"reason": "too uncomfortable", "idempotency_key": "CASE-1:3:rma"});
    let (s1, a) = call(
        &app,
        Method::POST,
        "/api/orders/ORD-58213/rmas",
        Some(body.clone()),
        Some(TOKEN),
    )
    .await;
    let (s2, b) = call(
        &app,
        Method::POST,
        "/api/orders/ORD-58213/rmas",
        Some(body),
        Some(TOKEN),
    )
    .await;
    assert_eq!((s1, s2), (StatusCode::CREATED, StatusCode::OK));
    assert_eq!(a["rma_id"], "RMA-41001");
    assert_eq!(a["rma_id"], b["rma_id"]);
    assert_eq!(a["deadline"], "2026-11-04");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM rmas")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/orders/ORD-57102/rmas",
        Some(json!({"reason": "x", "idempotency_key": "CASE-1:3:rma"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"]["code"], "idempotency_conflict");
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/orders/ORD-58790/rmas",
        Some(json!({"reason": "x", "idempotency_key": "k2"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"]["code"], "not_delivered");
}

#[tokio::test]
async fn warranty_claim_is_idempotent() {
    let (app, _db, _s) = crate::common::app(1).await;
    let body = json!({"defect": "stopped working", "idempotency_key": "CASE-2:3:claim"});
    let (s1, a) = call(
        &app,
        Method::POST,
        "/api/orders/ORD-57102/warranty-claims",
        Some(body.clone()),
        Some(TOKEN),
    )
    .await;
    let (s2, b) = call(
        &app,
        Method::POST,
        "/api/orders/ORD-57102/warranty-claims",
        Some(body),
        Some(TOKEN),
    )
    .await;
    assert_eq!((s1, s2), (StatusCode::CREATED, StatusCode::OK));
    assert_eq!(a["claim_id"], "WC-7701");
    assert_eq!(a, b);
}

#[tokio::test]
async fn refunds_never_exceed_the_capture_even_when_racing() {
    let (app, db, _s) = crate::common::app(1).await;
    let mk = |k: &str, amt: f64| json!({"amount": amt, "idempotency_key": k});
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/payments/PAY-3399/refunds",
        Some(mk("r1", 229.0)),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(v["refund_id"], "REF-90201");
    assert_eq!(v["method"], "Visa •• 4417");
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/payments/PAY-3399/refunds",
        Some(mk("r2", 0.01)),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"]["code"], "refund_exceeds_capture");
    let (s, _) = call(
        &app,
        Method::POST,
        "/api/payments/PAY-3398/refunds",
        Some(mk("bad", -5.0)),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // Same key racing: one creates, the other gets the same refund back.
    let (x, y) = tokio::join!(
        call(
            &app,
            Method::POST,
            "/api/payments/PAY-3120/refunds",
            Some(mk("same", 10.0)),
            Some(TOKEN)
        ),
        call(
            &app,
            Method::POST,
            "/api/payments/PAY-3120/refunds",
            Some(mk("same", 10.0)),
            Some(TOKEN)
        ),
    );
    let mut st = [x.0.as_u16(), y.0.as_u16()];
    st.sort();
    assert_eq!(
        st,
        [200, 201],
        "same key racing: one creates, the other gets the same refund back"
    );
    assert_eq!(x.1["refund_id"], y.1["refund_id"]);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM refunds WHERE idempotency_key = 'same'")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn bad_input_is_a_json_422_not_axum_plain_text() {
    let (app, _db, _s) = crate::common::app(1).await;
    // missing idempotency_key → serde rejection
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/orders/ORD-58213/rmas",
        Some(json!({"reason": "too big"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "invalid_input");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("idempotency_key")
    );
    // required query parameter missing (GET /api/orders needs customer_id)
    let (s, v) = call(&app, Method::GET, "/api/orders", None, Some(TOKEN)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "invalid_input");
}

/// Different-key racers on one payment. A separate connection holds the payment row lock, both
/// refunds are spawned and parked on it (verified via pg_stat_activity), then the lock is released.
/// Without `FOR UPDATE` in create_refund neither request waits and both succeed (201,201).
#[tokio::test]
async fn racing_refunds_serialise_on_the_payment_lock() {
    let (app, db, _s) = crate::common::app(1).await;
    let mut holder = db.pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *holder).await.unwrap();
    sqlx::query("SELECT 1 FROM payments WHERE payment_id = 'PAY-3398' FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let spawn_refund = |k: &'static str| {
        let app = app.clone();
        tokio::spawn(async move {
            call(
                &app,
                Method::POST,
                "/api/payments/PAY-3398/refunds",
                Some(json!({"amount": 150.0, "idempotency_key": k})),
                Some(TOKEN),
            )
            .await
        })
    };
    let (ta, tb) = (spawn_refund("ra"), spawn_refund("rb"));
    // Wait (bounded) until both backends are blocked on the row lock, or both requests already finished.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if waiting >= 2
            || (ta.is_finished() && tb.is_finished())
            || std::time::Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    sqlx::query("COMMIT").execute(&mut *holder).await.unwrap();
    drop(holder);
    let (a, b) = (ta.await.unwrap(), tb.await.unwrap());
    let mut st = [a.0.as_u16(), b.0.as_u16()];
    st.sort();
    assert_eq!(st, [201, 409]);
    let sum: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(amount_cents), 0)::bigint FROM refunds WHERE payment_id = 'PAY-3398'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(sum <= 22900, "refunds {sum} exceed the 22900 capture");
}

#[tokio::test]
async fn declined_payment_is_not_refundable() {
    let (app, db, _s) = crate::common::app(1).await;
    sqlx::query("INSERT INTO payments (payment_id, order_id, method, amount_cents, status, captured_at) VALUES ('PAY-DECL', 'ORD-58213', 'iDEAL', 5000, 'declined', now())")
        .execute(&db.pool)
        .await
        .unwrap();
    let (s, v) = call(
        &app,
        Method::POST,
        "/api/payments/PAY-DECL/refunds",
        Some(json!({"amount": 10.0, "idempotency_key": "decl"})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"]["code"], "refund_exceeds_capture");
    assert!(v["error"]["message"].as_str().unwrap().contains("declined"));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM refunds WHERE payment_id = 'PAY-DECL'")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn giant_refund_is_rejected_and_never_overflows() {
    let (app, db, _s) = crate::common::app(1).await;
    let mk = |k: &str, a: f64| json!({"amount": a, "idempotency_key": k});
    let (s, _) = call(
        &app,
        Method::POST,
        "/api/payments/PAY-3120/refunds",
        Some(mk("partial", 10.0)),
        Some(TOKEN),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = call(
        &app,
        Method::POST,
        "/api/payments/PAY-3120/refunds",
        Some(mk("giant", 1e17)),
        Some(TOKEN),
    )
    .await;
    assert!(
        s == StatusCode::UNPROCESSABLE_ENTITY || s == StatusCode::CONFLICT,
        "giant refund must be 422/409, got {s}"
    );
    let sum: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(amount_cents), 0)::bigint FROM refunds WHERE payment_id = 'PAY-3120'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(sum <= 37900, "refunded sum {sum} exceeds the capture");
}
