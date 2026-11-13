use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health(State(s): State<AppState>) -> Json<Value> {
    let redis_ok = redis::cmd("PING")
        .query_async::<String>(&mut s.bus.conn())
        .await
        .is_ok();
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&s.db)
        .await
        .is_ok();
    Json(json!({"redis": if redis_ok {"ok"} else {"down"}, "db": if db_ok {"ok"} else {"down"}}))
}
