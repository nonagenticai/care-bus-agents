//! The one place that turns a provider row into a usable token.
//!
//! Two stored forms exist and both must open, because the rows written before migration 0031 are
//! sealed with the legacy `CAREBUS_SOURCE_KEY` envelope into `secret_enc` and there is deliberately
//! no backfill:
//!
//! * `v2:<kid>:…` under `PROVIDER_SECRET_KEYS` (crate::provider_keyring), bound to
//!   `<tenant>:llm_key:<provider>` — what every write uses once the keyring is configured, and what
//!   a re-seal moves a row onto;
//! * standard base64 of `nonce || ciphertext+tag` under `CAREBUS_SOURCE_KEY`, bound to the provider
//!   id alone (`secret_enc`).
//!
//! Reads go through [`open`], never through one form directly. A write that lands in a form some
//! reader cannot open is a credential the service can store and not use, and it would surface as a
//! failing turn rather than as a failing deploy.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use uuid::Uuid;

use crate::config::Config;
use crate::crypto::{self, SourceKey, Unreadable};
use crate::error::{ApiError, ApiResult};
use crate::provider_keyring::{self, Keyring};

/// The key material a provider secret can be sealed under, borrowed from [`Config`].
#[derive(Clone, Copy)]
pub struct Secrets<'a> {
    pub source_key: Option<&'a SourceKey>,
    pub keyring: Option<&'a Keyring>,
}

impl Config {
    pub fn secrets(&self) -> Secrets<'_> {
        Secrets {
            source_key: self.source_key.as_ref(),
            keyring: self.provider_keyring.as_ref(),
        }
    }
}

/// A secret to store: the value for `secret_sealed` and the fingerprint shown in its place.
#[derive(Clone, Debug)]
pub struct Stored {
    pub sealed: String,
    pub fingerprint: String,
}

/// Seal `token` for `provider` under the keyring's current kid. 503 `not_configured` when the
/// keyring is unset, so a write can never land in the legacy form the keyring ring does not manage.
pub fn seal(s: Secrets<'_>, tenant: Uuid, provider: Uuid, token: &str) -> ApiResult<Stored> {
    let kr = provider_keyring::require(s.keyring)?;
    let out = kr.seal(&provider_keyring::provider_scope(tenant, provider), token);
    Ok(Stored {
        sealed: out.sealed,
        fingerprint: out.fingerprint,
    })
}

/// The usable token for a row in either form; `None` when the row stores none.
///
/// `secret_sealed` wins when both are set (0031 never leaves both, but a row migrated by hand might).
pub fn open(
    s: Secrets<'_>,
    tenant: Uuid,
    provider: Uuid,
    sealed: Option<&str>,
    enc: Option<&str>,
) -> ApiResult<Option<String>> {
    if let Some(v) = sealed.filter(|v| !v.is_empty()) {
        let kr = provider_keyring::require(s.keyring)?;
        let token = kr
            .open(&provider_keyring::provider_scope(tenant, provider), v)
            .map_err(|_| {
                ApiError::Internal(anyhow::anyhow!(
                    "a provider's stored secret cannot be opened under the current keyring"
                ))
            })?;
        return Ok(Some(token));
    }
    match enc.filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(e) => {
            let key = crypto::require(s.source_key)?;
            Ok(Some(open_token(key, provider, e).map_err(|_| {
                ApiError::Internal(anyhow::anyhow!(
                    "a provider's stored token cannot be decrypted"
                ))
            })?))
        }
    }
}

fn aad(id: Uuid) -> Vec<u8> {
    format!("llm_provider:{id}").into_bytes()
}

/// `secret_enc`: standard base64 of nonce (12 bytes) followed by the AES-256-GCM ciphertext, bound to `id`.
pub fn seal_token(key: &SourceKey, id: Uuid, token: &str) -> String {
    let s = key.seal(&aad(id), token.as_bytes());
    let mut out = s.nonce;
    out.extend_from_slice(&s.ciphertext);
    STANDARD.encode(out)
}

/// The token sealed by [`seal_token`] for the same provider id.
pub fn open_token(key: &SourceKey, id: Uuid, secret_enc: &str) -> Result<String, Unreadable> {
    let raw = STANDARD.decode(secret_enc).map_err(|_| Unreadable)?;
    if raw.len() < 12 {
        return Err(Unreadable);
    }
    let (nonce, ciphertext) = raw.split_at(12);
    let plain = key.open(&aad(id), nonce, ciphertext)?;
    String::from_utf8(plain).map_err(|_| Unreadable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_keyring::{Keyring, Sealed as KeyringSealed};

    fn keyring() -> Keyring {
        let key = STANDARD.encode([7u8; 32]);
        let keys = format!(r#"{{"k1":"{key}"}}"#);
        Keyring::parse(Some(&keys), Some("k1"))
            .expect("parses")
            .expect("configured")
    }

    #[test]
    fn a_v2_secret_opens_for_its_own_tenant_and_provider_only() {
        let kr = keyring();
        let s = Secrets {
            source_key: None,
            keyring: Some(&kr),
        };
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let tenant = Uuid::new_v4();
        let stored = seal(s, tenant, a, "tok-123").expect("seals");
        assert!(stored.sealed.starts_with(provider_keyring::PREFIX));
        assert_eq!(
            open(s, tenant, a, Some(&stored.sealed), None)
                .expect("opens")
                .as_deref(),
            Some("tok-123")
        );
        // Another provider, another tenant: the scope is in the AAD, so neither opens.
        assert!(open(s, tenant, b, Some(&stored.sealed), None).is_err());
        assert!(open(s, Uuid::new_v4(), a, Some(&stored.sealed), None).is_err());
    }

    #[test]
    fn a_legacy_secret_still_opens_and_an_empty_one_is_none() {
        let src = SourceKey::from_base64(&STANDARD.encode([9u8; 32])).expect("key");
        let s = Secrets {
            source_key: Some(&src),
            keyring: None,
        };
        let (p, t) = (Uuid::new_v4(), Uuid::new_v4());
        let enc = seal_token(&src, p, "legacy-token");
        assert_eq!(
            open(s, t, p, None, Some(&enc)).expect("opens").as_deref(),
            Some("legacy-token")
        );
        assert_eq!(open(s, t, p, None, None).expect("none"), None);
        assert_eq!(open(s, t, p, Some(""), Some("")).expect("none"), None);
    }

    #[test]
    fn sealing_without_a_keyring_is_not_configured() {
        let s = Secrets {
            source_key: None,
            keyring: None,
        };
        assert!(matches!(
            seal(s, Uuid::new_v4(), Uuid::new_v4(), "t"),
            Err(ApiError::NotConfigured(_))
        ));
    }

    #[test]
    fn the_keyring_seal_type_carries_a_fingerprint() {
        // Guard against a keyring refactor that drops the fingerprint: the UI shows it instead of the secret.
        let kr = keyring();
        let out: KeyringSealed = kr.seal("scope", "plaintext");
        assert_eq!(out.fingerprint.len(), 8);
        assert!(!out.sealed.contains("plaintext"));
    }
}
