//! The harness drops the scratch databases it creates on a shared `TEST_DATABASE_URL` server.

use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::PgPool;

#[tokio::test]
async fn a_scratch_database_is_dropped() {
    let Ok(base) = std::env::var("TEST_DATABASE_URL") else {
        return; // container mode: the per-test container is removed with the process
    };
    if base.is_empty() {
        return;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("cb02_t_{}_{nanos}_0", std::process::id());
    let admin = PgPool::connect(&base)
        .await
        .expect("connect TEST_DATABASE_URL");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create scratch database");
    let dropped = crate::common::drop_scratch_databases(&base, &[name.clone()]).await;
    assert_eq!(dropped, 1, "the named database is dropped");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&name)
            .fetch_one(&admin)
            .await
            .expect("query pg_database");
    assert!(!exists, "dropped database no longer exists");
    admin.close().await;
}
