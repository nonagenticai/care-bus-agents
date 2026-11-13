//! `Authorization: Bearer <token>` on every /api route, where the token is any of `Config::tokens`.
//! Constant-time compare against every candidate (no early exit).
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use crate::{AppState, DemoError, Result};

pub async fn require_bearer(
    State(s): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response> {
    let given = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let ok = s.cfg.tokens.iter().fold(false, |acc, t| {
        let hit: bool = given.as_bytes().ct_eq(t.as_bytes()).into();
        acc | hit
    });
    if !ok || given.is_empty() {
        return Err(DemoError::Unauthorized);
    }
    Ok(next.run(req).await)
}
