use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::error::Result;

pub async fn connect(url: &str) -> Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(2))
        .connect(url)
        .await?)
}

/// Applies migrations without sqlx's session advisory lock: DATABASE_URL goes through pgbouncer in
/// transaction-pool mode, where a session-level lock leaks across clients. demo-backends runs a
/// single replica and each migration is transactional, so the lock is not needed.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    let mut m = sqlx::migrate!("./migrations");
    m.set_locking(false);
    m.run(pool)
        .await
        .map_err(|e| crate::error::DemoError::Config(format!("migrate: {e}")))?;
    Ok(())
}
