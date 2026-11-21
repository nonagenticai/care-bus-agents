//! The harness itself: proves `drop_scratch_databases` actually removes a database, so a shared `TEST_DATABASE_URL`
//! server does not accumulate one per test process.

use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::{AssertSqlSafe, PgPool};

#[tokio::test]
async fn a_scratch_database_is_dropped() {
    let base = crate::common::server_base().await;
    let name = format!(
        "cb05_t_{}_{}_0",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let admin = PgPool::connect(base)
        .await
        .expect("connect to the test postgres server");
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create test database");
    let dropped = crate::common::drop_scratch_databases(base, &[name.clone()]).await;
    assert_eq!(dropped, 1, "drop_scratch_databases should report one drop");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&name)
            .fetch_one(&admin)
            .await
            .expect("read pg_database");
    assert!(!exists, "{name} still exists after the drop");
    admin.close().await;
}
