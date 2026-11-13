//! `POST /api/telegram/webhook`: Telegram delivers the bot's updates here. Public (no JWT): the only credential is the
//! `X-Telegram-Bot-Api-Secret-Token` header, compared in constant time with `TELEGRAM_WEBHOOK_SECRET` (chat-bridge's
//! check, `src/index.ts`). Telegram gets its 200 at once and the turn runs in a spawned task, so a slow model never
//! makes Telegram redeliver. With Telegram off the route is a 404, like any unknown route.
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
    telegram::{self, Update},
};

pub const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/telegram/webhook", post(webhook))
}

async fn webhook(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Some(cfg) = s.cfg.telegram.as_ref() else {
        return Err(ApiError::NotFound("route".into()));
    };
    let presented = headers.get(SECRET_HEADER).map(|v| v.as_bytes());
    if !cfg.secret_matches(presented) {
        return Err(ApiError::Unauthorized);
    }
    // A body we cannot read is acknowledged too: a retry would carry the same body.
    match serde_json::from_slice::<Update>(&body) {
        Ok(update) => {
            tokio::spawn(telegram::handle_update(s, update));
        }
        Err(e) => {
            tracing::warn!(error = %e, "unreadable telegram update; acknowledged and dropped")
        }
    }
    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}
