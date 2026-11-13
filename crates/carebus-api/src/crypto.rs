//! AES-256-GCM for source credentials. The key comes from `CAREBUS_SOURCE_KEY` (32 bytes, standard
//! base64). Every seal draws a fresh random 96-bit nonce, and the source id is bound in as associated
//! data, so a ciphertext copied onto another source's row does not decrypt.
use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, Generate, KeyInit, Nonce, Payload};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::error::{ApiError, ApiResult};

/// The decoded key. `Debug` never prints it.
#[derive(Clone)]
pub struct SourceKey(Aes256Gcm);

impl std::fmt::Debug for SourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceKey(..)")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("CAREBUS_SOURCE_KEY is not valid base64")]
    NotBase64,
    #[error("CAREBUS_SOURCE_KEY must decode to exactly 32 bytes, got {0}")]
    WrongLength(usize),
}

/// Decryption failed: wrong key, wrong source, or a damaged row. Carries no detail on purpose.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("the stored credentials cannot be decrypted")]
pub struct Unreadable;

/// What goes into `sources.credentials_enc` / `sources.credentials_nonce`.
pub struct Sealed {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl SourceKey {
    pub fn from_base64(raw: &str) -> Result<Self, KeyError> {
        let bytes = STANDARD
            .decode(raw.trim())
            .map_err(|_| KeyError::NotBase64)?;
        if bytes.len() != 32 {
            return Err(KeyError::WrongLength(bytes.len()));
        }
        let cipher =
            Aes256Gcm::new_from_slice(&bytes).map_err(|_| KeyError::WrongLength(bytes.len()))?;
        Ok(Self(cipher))
    }

    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Sealed {
        let nonce = Nonce::<Aes256Gcm>::generate();
        let ciphertext = self
            .0
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("AES-GCM encryption of an in-memory buffer cannot fail");
        Sealed {
            nonce: nonce.to_vec(),
            ciphertext,
        }
    }

    pub fn open(&self, aad: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, Unreadable> {
        let nonce = <&Nonce<Aes256Gcm>>::try_from(nonce).map_err(|_| Unreadable)?;
        self.0
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| Unreadable)
    }
}

/// The key, or 503 `not_configured`: without `CAREBUS_SOURCE_KEY` nothing secret is stored or used.
pub fn require(key: Option<&SourceKey>) -> ApiResult<&SourceKey> {
    key.ok_or(ApiError::NotConfigured(
        "CAREBUS_SOURCE_KEY is not set, so source credentials cannot be stored or used.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test keys are built at runtime from a byte range, so no key literal is ever committed.
    fn b64(bytes: impl IntoIterator<Item = u8>) -> String {
        use base64::Engine;
        STANDARD.encode(bytes.into_iter().collect::<Vec<u8>>())
    }

    fn key() -> String {
        b64(0u8..32)
    }

    #[test]
    fn round_trip_with_a_fresh_nonce_each_time() {
        let k = SourceKey::from_base64(&key()).unwrap();
        let a = k.seal(b"source-1", br#"{"secret":"hunter2"}"#);
        let b = k.seal(b"source-1", br#"{"secret":"hunter2"}"#);
        assert_eq!(a.nonce.len(), 12);
        assert_ne!(a.nonce, b.nonce, "nonce is random per seal");
        assert_ne!(a.ciphertext, b.ciphertext);
        assert!(!String::from_utf8_lossy(&a.ciphertext).contains("hunter2"));
        assert_eq!(
            k.open(b"source-1", &a.nonce, &a.ciphertext).unwrap(),
            br#"{"secret":"hunter2"}"#
        );
    }

    #[test]
    fn wrong_source_wrong_key_or_tampering_is_unreadable() {
        let k = SourceKey::from_base64(&key()).unwrap();
        let s = k.seal(b"source-1", b"x");
        assert_eq!(
            k.open(b"source-2", &s.nonce, &s.ciphertext),
            Err(Unreadable)
        );
        let other = SourceKey::from_base64(&b64(32u8..64)).unwrap();
        assert_eq!(
            other.open(b"source-1", &s.nonce, &s.ciphertext),
            Err(Unreadable)
        );
        let mut bad = s.ciphertext.clone();
        bad[0] ^= 1;
        assert_eq!(k.open(b"source-1", &s.nonce, &bad), Err(Unreadable));
        assert_eq!(
            k.open(b"source-1", &s.nonce[..11], &s.ciphertext),
            Err(Unreadable)
        );
    }

    #[test]
    fn key_must_be_32_bytes_of_base64_and_debug_hides_it() {
        assert_eq!(
            SourceKey::from_base64("not base64!").unwrap_err(),
            KeyError::NotBase64
        );
        assert_eq!(
            SourceKey::from_base64(&b64(0u8..16)).unwrap_err(),
            KeyError::WrongLength(16)
        );
        let k = SourceKey::from_base64(&key()).unwrap();
        assert_eq!(format!("{k:?}"), "SourceKey(..)");
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
