//! Demo-data proxy guards (plan 10 Task 7): demo-backends is one shared fake enterprise for every tenant, so a
//! job member may read it but only a super_admin acting in a tenant may write it.

use axum::http::StatusCode;
use axum::{
    Json, Router,
    routing::{get, post, put},
};
use serde_json::{Value, json};

/// A demo-backends stand-in answering every route the proxy forwards to.
async fn fake_demo() -> String {
    let app = Router::new()
        .route(
            "/api/admin/reset",
            post(|| async { Json(json!({"reset": true})) }),
        )
        .route(
            "/api/admin/faults",
            get(|| async { Json(json!({"faults": []})) })
                .put(|| async { Json(json!({"ok": true})) }),
        )
        .route(
            "/api/admin/scenarios/{name}",
            post(|| async { Json(json!({"applied": true})) }),
        )
        .route(
            "/api/admin/records/{table}/{id}",
            put(|| async { Json(json!({"updated": true})) }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

/// Every demo write, as (method, job-relative path, body).
fn writes() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("POST", "/demo/reset", json!({"seed": 42})),
        (
            "PUT",
            "/demo/faults",
            json!({"tool": "order", "mode": "timeout", "count": 1}),
        ),
        ("POST", "/demo/scenarios/late-parcel", json!({})),
        (
            "PUT",
            "/demo/records/orders/ORD-58790",
            json!({"status": "delivered"}),
        ),
    ]
}

#[tokio::test]
async fn demo_writes_need_super_admin() {
    let up = fake_demo().await;
    let e = crate::common::env_with("demo-guard", |c| {
        c.demo_backends_url = Some(up.clone());
        c.demo_backends_token = Some("demo-token".into());
    })
    .await;

    // a tenant admin or user must not reset the demo data of every tenant
    for t in [crate::common::admin(), crate::common::user()] {
        for (m, path, body) in writes() {
            let (s, b) = crate::common::send(
                &e.app,
                crate::common::json(m, &crate::common::j(path), Some(&t), body),
            )
            .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{m} {path}: {b}");
        }
    }
    // nothing was forwarded, nothing audited
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE resource_type = 'demo'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(n, 0);

    // a super_admin must name the tenant it acts in
    let sa = crate::common::super_admin();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/demo/reset"),
            Some(&sa),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "active_tenant_required", "{b}");

    // acting in the bootstrap tenant it passes the guard, and the write is audited in that job's scope
    for (m, path, body) in writes() {
        let req = crate::common::with_tenant(
            crate::common::json(m, &crate::common::j(path), Some(&sa), body),
            crate::common::BOOT_TENANT,
        );
        let (s, b) = crate::common::send(&e.app, req).await;
        assert_eq!(s, StatusCode::OK, "{m} {path}: {b}");
    }
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT a.action_type, a.actor_id, a.tenant_id::text, j.key
           FROM audit_events a JOIN jobs j ON j.id = a.job_id
          WHERE a.resource_type = 'demo' ORDER BY a.id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    let actions: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        actions,
        ["reset", "set_faults", "apply_scenario", "update_record"]
    );
    for (_, actor, tenant, job) in &rows {
        assert_eq!(
            (actor.as_str(), tenant.as_str(), job.as_str()),
            (
                "u-super",
                crate::common::BOOT_TENANT,
                crate::common::BOOT_JOB
            )
        );
    }

    // reads stay open to every job member
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/demo/faults"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
