//! The carebus-api half of the source-key envelope: the 503 mapping the HTTP layer needs.
//!
//! The cipher itself (`SourceKey`, `KeyError`, `Unreadable`, `Sealed`) lives in `carebus::crypto`, so a
//! library without HTTP can open a source credential. Everything is re-exported so the old paths keep
//! working.
pub use carebus::crypto::*;

use crate::error::{ApiError, ApiResult};

/// The key, or 503 `not_configured`: without `CAREBUS_SOURCE_KEY` nothing secret is stored or used.
pub fn require(key: Option<&SourceKey>) -> ApiResult<&SourceKey> {
    key.ok_or(ApiError::NotConfigured(
        "CAREBUS_SOURCE_KEY is not set, so source credentials cannot be stored or used.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    /// Test keys are built at runtime from a byte range, so no key literal is ever committed.
    fn b64(bytes: impl IntoIterator<Item = u8>) -> String {
        STANDARD.encode(bytes.into_iter().collect::<Vec<u8>>())
    }

    fn key() -> String {
        b64(0u8..32)
    }

    #[tokio::test]
    async fn require_is_503_not_configured_without_a_key() {
        use axum::response::IntoResponse;
        use http_body_util::BodyExt;
        let e = require(None).unwrap_err();
        let resp = e.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "not_configured");
        let k = SourceKey::from_base64(&key()).unwrap();
        assert!(require(Some(&k)).is_ok());
    }
}
