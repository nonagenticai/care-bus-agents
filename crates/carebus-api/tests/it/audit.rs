use axum::http::StatusCode;
use carebus_api::tenancy::begin_scoped;
use serde_json::json;
use uuid::Uuid;

async fn boot_job(e: &crate::common::TestEnv) -> Uuid {
    sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1::uuid AND key = $2")
        .bind(crate::common::BOOT_TENANT)
        .bind(crate::common::BOOT_JOB)
        .fetch_one(crate::common::pool(e))
        .await
        .unwrap()
}

/// Three procedure rows in the bootstrap job, one tenant-level tool failure, and one procedure row in another
/// tenant that no bootstrap reader may see. Written as the API writes them: scoped transactions on the app pool.
async fn seed(e: &crate::common::TestEnv) {
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job = boot_job(e).await;
    let mut tx = begin_scoped(crate::common::app_pool(e), boot, Some(job))
        .await
        .unwrap();
    for i in 0..3 {
        carebus_api::audit::record(
            &mut tx,
            "a.bakker",
            "human",
            "update",
            "procedure",
            &format!("return@1.{i}.0"),
            true,
            json!({}),
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let mut tx = begin_scoped(crate::common::app_pool(e), boot, None)
        .await
        .unwrap();
    carebus_api::audit::record(
        &mut tx,
        "svc",
        "system",
        "execute",
        "tool",
        "delivery_track_shipment",
        false,
        json!({"latency_ms": 3001}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let other =
        crate::common::insert_tenant(crate::common::pool(e), crate::common::OTHER_TENANT).await;
    let mut tx = begin_scoped(crate::common::app_pool(e), other, None)
        .await
        .unwrap();
    carebus_api::audit::record(
        &mut tx,
        "acme",
        "human",
        "update",
        "procedure",
        "other@1.0.0",
        true,
        json!({}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn audit_filters_and_pages_newest_first() {
    let e = crate::common::env("audit").await;
    seed(&e).await;
    let t = crate::common::user();

    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/audit?status=failure", Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["items"].as_array().unwrap().len(), 1);
    assert_eq!(b["items"][0]["resource_id"], "delivery_track_shipment");
    assert_eq!(b["items"][0]["details"]["latency_ms"], 3001);

    let (_, p1) = crate::common::send(
        &e.app,
        crate::common::get("/api/audit?resource_type=procedure&limit=2", Some(&t)),
    )
    .await;
    assert_eq!(p1["items"][0]["resource_id"], "return@1.2.0");
    let next = p1["next"].as_str().expect("full page has a cursor");
    let (_, p2) = crate::common::send(
        &e.app,
        crate::common::get(
            &format!("/api/audit?resource_type=procedure&limit=2&before={next}"),
            Some(&t),
        ),
    )
    .await;
    assert_eq!(
        p2["items"].as_array().unwrap().len(),
        1,
        "the other tenant's procedure row is not listed"
    );
    assert!(p2["next"].is_null());

    let (_, q) =
        crate::common::send(&e.app, crate::common::get("/api/audit?q=track", Some(&t))).await;
    assert_eq!(q["items"].as_array().unwrap().len(), 1);

    let (_, a) = crate::common::send(
        &e.app,
        crate::common::get("/api/audit?actor_type=system", Some(&t)),
    )
    .await;
    assert_eq!(a["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn audit_job_filter_sets_the_job_scope() {
    let e = crate::common::env("audit_job").await;
    seed(&e).await;
    let t = crate::common::user();
    let uri = format!("/api/audit?job={}", crate::common::BOOT_JOB);
    let (s, b) = crate::common::send(&e.app, crate::common::get(&uri, Some(&t))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let ids: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["resource_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["return@1.2.0", "return@1.1.0", "return@1.0.0"],
        "only the job's rows: no tenant-level row, no other tenant's"
    );
    let (s, b) = crate::common::send(&e.app, crate::common::get("/api/audit", Some(&t))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        b["items"].as_array().unwrap().len(),
        4,
        "without ?job: every job's and the tenant-level rows"
    );
    let (s, b) =
        crate::common::send(&e.app, crate::common::get("/api/audit?job=nope", Some(&t))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn audit_rejects_bad_cursor_and_anonymous() {
    let e = crate::common::env("audit_bad").await;
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/audit?before=zzz", Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_cursor");
    let (s, _) = crate::common::send(&e.app, crate::common::get("/api/audit", None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
