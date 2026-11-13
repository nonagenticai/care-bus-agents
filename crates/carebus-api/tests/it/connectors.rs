use axum::http::StatusCode;
use serde_json::{Value, json};

fn oms() -> Value {
    json!({"id":"oms","name":"Order management","system":"OMS","base_url":"http://127.0.0.1:9/api","auth_type":"api_key",
           "secret_ref":"carebus-connectors/oms-api-key","timeout_ms":2500,"rate_limit":"80 req/s",
           "mappings":[{"backend_field":"ord_no","attribute":"Order.order_id"}]})
}

#[tokio::test]
async fn connector_create_with_mappings_rejects_secrets() {
    let e = crate::common::env("conn").await;
    let admin = crate::common::admin();
    let (s, c) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/connectors"),
            Some(&admin),
            oms(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(c["mappings"][0]["attribute"], "Order.order_id");
    let bad = json!({"id":"psp","name":"Payments","system":"PSP","base_url":"https://psp","auth_type":"api_key",
                     "secret_ref":"Zx81Qm0aPv7LhK2cYt5NwR9eB3uJd6Fg0SxT4oHq","timeout_ms":2000,"rate_limit":null,"mappings":[]});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/connectors"), Some(&admin), bad),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "secret_value_rejected");
    assert!(!b.to_string().contains("Zx81Qm0a"));
    let unknown = json!({"id":"crm","name":"CRM","system":"CRM","base_url":"https://crm","auth_type":"none","secret_ref":null,"timeout_ms":2000,"rate_limit":null,
                         "mappings":[{"backend_field":"x","attribute":"Order.nonexistent"}]});
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                &crate::common::j("/connectors"),
                Some(&admin),
                unknown
            )
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/connectors"), Some(&admin)),
    )
    .await;
    assert_eq!(
        list["items"].as_array().unwrap().len(),
        1,
        "rejected writes stored nothing"
    );
    assert_eq!(list["items"][0]["health"], "unknown");
    let (s, t) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/connectors/oms/test"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(t["ok"], false);
    assert_eq!(t["detail"], "unreachable");
    assert!(!t.to_string().contains("127.0.0.1"), "no URL echoed: {t}");
    let (_, after) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/connectors"), Some(&admin)),
    )
    .await;
    assert_eq!(
        after["items"][0]["health"], "down",
        "list shows the last test result"
    );
    assert!(after["items"][0]["health_checked_at"].is_string());
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE resource_type='connector' AND resource_id='oms' AND action_type='test'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(n, 1, "one audit row per test");
    let st: String = sqlx::query_scalar(
        "SELECT status FROM audit_events WHERE resource_type='connector' AND action_type='test'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        st, "success",
        "the admin action succeeded even though the target is down"
    );
}

#[tokio::test]
async fn put_validates_shape_secret_ref_and_non_objects() {
    let e = crate::common::env("conn-put").await;
    let admin = crate::common::admin();
    let viewer = crate::common::user();
    crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/connectors"),
            Some(&admin),
            oms(),
        ),
    )
    .await;
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/connectors/oms"),
            Some(&viewer),
            oms(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    for body in [json!([1]), json!("x"), json!(null), json!(3)] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(
                "PUT",
                &crate::common::j("/connectors/oms"),
                Some(&admin),
                body,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b["error"]["code"], "invalid_body");
    }
    // typed failure: fixed message, value not echoed
    let mut b = oms();
    b["timeout_ms"] = json!("ECHOMARKER");
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json("PUT", &crate::common::j("/connectors/oms"), Some(&admin), b),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!r.to_string().contains("ECHOMARKER"), "{r}");
    for bad in ["Has Space", "UPPER/x", "a//b", "x?y"] {
        let mut b = oms();
        b["secret_ref"] = json!(bad);
        let (s, r) = crate::common::send(
            &e.app,
            crate::common::json("PUT", &crate::common::j("/connectors/oms"), Some(&admin), b),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(r["error"]["code"], "invalid_secret_ref", "{bad}");
    }
    let mut b = oms();
    b["name"] = json!("Renamed");
    b["mappings"] = json!([]);
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json("PUT", &crate::common::j("/connectors/oms"), Some(&admin), b),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["name"], "Renamed");
    assert_eq!(r["mappings"].as_array().unwrap().len(), 0);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/connectors/ghost"),
            Some(&admin),
            oms(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/connectors"),
            Some(&admin),
            oms(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/connectors/ghost"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
