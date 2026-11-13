use anyhow::Result;
use carebus_api::{config::Config, state};

#[tokio::main]
async fn main() -> Result<()> {
    carebus::init_tracing();
    let cfg = Config::from_env()?;
    if cfg.source_key.is_none() {
        tracing::warn!(
            "CAREBUS_SOURCE_KEY is not set: source credentials can be neither saved nor used"
        );
    }
    let bind = cfg.bind.clone();
    let st = state::build(cfg).await?;
    let _recorder = carebus_api::recorder::spawn(st.clone(), carebus::consumer_name("carebus-api"));
    let _inbox_sweeper = carebus_api::routes::conversations::spawn_inbox_sweeper(st.clone());
    let _metering_drainer = carebus_api::metering::spawn_drainer(st.clone());
    // Telegram channel (off unless TELEGRAM_ENABLED=true): register the webhook (non-fatal) and forward agent replies.
    let _telegram_webhook = carebus_api::telegram::spawn_register_webhook(st.clone());
    let _telegram_forwarder = carebus_api::telegram::spawn_forwarder(st.clone());
    // Early-access signup (care-bus 17): the provisioning retry tick (idle while CAREBUS_SIGNUP_ENABLED is off).
    st.signup.log_status();
    let _signup_tick = carebus_api::signup::provisioning::spawn_tick(st.clone());
    let app = carebus_api::router(st);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "carebus-api listening");
    axum::serve(listener, app).await?;
    Ok(())
}
