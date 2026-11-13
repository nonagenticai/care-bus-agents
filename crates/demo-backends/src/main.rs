use std::sync::Arc;

use demo_backends::{AppState, Config, api, db, faults::FaultState, seed};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cfg = Config::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;
    let empty: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM customers)")
        .fetch_one(&pool)
        .await?;
    if empty {
        let r = seed::reset(&pool, cfg.seed, cfg.today).await?;
        tracing::info!(
            customers = r.customers,
            orders = r.orders,
            "seeded empty database"
        );
    }
    let bind = cfg.bind;
    api::serve(
        AppState {
            pool,
            cfg: Arc::new(cfg),
            faults: FaultState::default(),
        },
        bind,
    )
    .await?;
    Ok(())
}
