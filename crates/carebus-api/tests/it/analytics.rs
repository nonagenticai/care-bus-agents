#[tokio::test]
async fn analytics_counts_outcomes_per_day_and_type() {
    let e = crate::common::env("analytics").await;
    let db = crate::common::pool(&e);
    // Fixed instants inside today (server's day), so the test never depends on the time of day.
    for (id, outcome, ms) in [
        ("A", "completed", 2000),
        ("B", "completed", 4000),
        ("C", "not_eligible", 3000),
        ("D", "failed", 9000),
    ] {
        sqlx::query("INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, outcome, started_at, resolved_at, duration_ms)
                     VALUES ($1,'C-1','return','order-support','retail:order-support:returns',$2, date_trunc('day', now()) + interval '1 minute', date_trunc('day', now()) + interval '2 minutes', $3)")
            .bind(id).bind(outcome).bind(ms as i64).execute(db).await.unwrap();
    }
    sqlx::query("INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, started_at) VALUES ('E','C-1','faq','faq','retail:faq:general', date_trunc('day', now()) + interval '1 minute')")
        .execute(db).await.unwrap();
    let t = crate::common::user();
    let (s, a) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/analytics?days=7"), Some(&t)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    let days = a["days"].as_array().unwrap();
    assert_eq!(days.len(), 7, "days without cases are present with zeros");
    let today = days.last().unwrap();
    assert_eq!(today["completed"], 2);
    assert_eq!(today["not_eligible"], 1);
    assert_eq!(today["failed"], 1);
    assert_eq!(today["open"], 1);
    assert_eq!(days[0]["completed"], 0);
    let ret = a["by_type"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["request_type"] == "return")
        .unwrap();
    assert_eq!(ret["cases"], 4);
    assert_eq!(ret["p50_ms"], 3500.0);
    assert_eq!(a["previous"]["cases"], 0);
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j("/analytics?days=28"), Some(&t))
        )
        .await
        .1["days"]
            .as_array()
            .unwrap()
            .len(),
        28
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/analytics?days=9"), Some(&t)),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (
            axum::http::StatusCode::BAD_REQUEST,
            serde_json::json!("invalid_days")
        )
    );
}
