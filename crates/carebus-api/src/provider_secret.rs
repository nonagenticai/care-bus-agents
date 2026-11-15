//! The carebus-api half of provider secrets: the `Config` accessor and the mapping into `ApiError`.
//!
//! `Secrets`, `Stored`, `SecretError` and the seal/open functions live in `carebus::provider_secret`,
//! so the library can open a provider secret without HTTP. They are re-exported here so callers keep
//! using `crate::provider_secret::…`, and `From<SecretError> for ApiError` keeps their `?` working.

use crate::config::Config;
use crate::error::ApiError;
pub use carebus::provider_secret::{
    open, open_token, seal, seal_token, SecretError, Secrets, Stored,
};

impl Config {
    pub fn secrets(&self) -> Secrets<'_> {
        Secrets {
            source_key: self.source_key.as_ref(),
            keyring: self.provider_keyring.as_ref(),
        }
    }
}

impl From<SecretError> for ApiError {
    fn from(e: SecretError) -> Self {
        match e {
            SecretError::NotConfigured(m) => ApiError::NotConfigured(m),
            SecretError::Unreadable => ApiError::Internal(anyhow::anyhow!(
                "a provider's stored secret cannot be decrypted"
            )),
        }
    }
}
