//! The carebus-api half of the provider keyring: the 503 mapping the HTTP layer needs.
//!
//! The keyring itself (`Keyring`, `Sealed`, `PREFIX`, `KEYS_VAR`, `CURRENT_VAR`, `valid_kid`,
//! `fingerprint`, `parse`, `seal`, `open`, `provider_scope`) lives in `carebus::provider_keyring`, so
//! the library can open a provider secret. Everything is re-exported so the old paths keep working.
pub use carebus::provider_keyring::*;

use crate::error::{ApiError, ApiResult};

/// The keyring, or 503 `not_configured`: without it no provider secret is stored or used.
pub fn require(kr: Option<&Keyring>) -> ApiResult<&Keyring> {
    kr.ok_or(ApiError::NotConfigured(
        "PROVIDER_SECRET_KEYS is not set, so provider secrets cannot be stored or used.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn require_is_503_not_configured_without_a_keyring() {
        use axum::response::IntoResponse;
        use http_body_util::BodyExt;
        let e = require(None).unwrap_err();
        let resp = e.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "not_configured");
    }
}
