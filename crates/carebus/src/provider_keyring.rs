//! The `v2:` keyring for provider secrets — the same shape as `agent`'s `crypto::keyring` (`ProviderSecret`
//! purpose), so a credential can be rotated without downtime and its presence can be shown without ever
//! returning it.
//!
//! Configuration: `PROVIDER_SECRET_KEYS` is a JSON object `{kid: base64(32 bytes)}`; `PROVIDER_SECRET_KEY_CURRENT`
//! names the kid new seals use. A kid matches `[A-Za-z0-9_-]{1,32}`. Old kids stay in the set, so values sealed
//! under them still open after `current` moves — rotation is: add a kid, move `current`, rewrap.
//!
//! Stored form: `v2:<kid>:` + base64url (no padding) of `nonce(12) || ciphertext+tag`. The associated data is the
//! caller's scope (`<tenant>:<kind>:<row id>`) followed by the kid: a blob copied onto another row, another tenant
//! or another kid does not open.
//!
//! Both variables unset means provider secrets are unavailable (503 `not_configured`) rather than mis-keyed.
//! [`Keyring`]'s `Debug` prints the kids, never a key.

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, Generate, KeyInit, Nonce, Payload};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::crypto::Unreadable;

/// The version prefix of a keyring value.
pub const PREFIX: &str = "v2:";
/// The variable holding the `{kid: base64}` object.
pub const KEYS_VAR: &str = "PROVIDER_SECRET_KEYS";
/// The variable naming the kid new seals use.
pub const CURRENT_VAR: &str = "PROVIDER_SECRET_KEY_CURRENT";
/// Bytes of one key.
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KID_MAX: usize = 32;

/// One purpose's keys. `Debug` shows the kids, never the key bytes.
#[derive(Clone)]
pub struct Keyring {
    current: String,
    keys: BTreeMap<String, Aes256Gcm>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("current", &self.current)
            .field("kids", &self.keys.keys().collect::<Vec<_>>())
            .field("keys", &"[redacted]")
            .finish()
    }
}

/// A sealed secret and its fingerprint. `Debug` shows the fingerprint only.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    /// The `v2:<kid>:…` value to store.
    pub sealed: String,
    pub fingerprint: String,
}

impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealed")
            .field("sealed", &"[redacted]")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// `[A-Za-z0-9_-]{1,32}`.
pub fn valid_kid(kid: &str) -> bool {
    !kid.is_empty()
        && kid.len() <= KID_MAX
        && kid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The AAD of one row's secret: the caller's scope followed by the kid that sealed it.
fn aad(scope: &str, kid: &str) -> Vec<u8> {
    format!("{scope}{kid}").into_bytes()
}

/// First 8 hex of SHA-256(`scope` || plaintext): stable for one tenant and kind, different across tenants.
/// Shown in the UI so an operator can tell one credential from another without the credential.
pub fn fingerprint(scope: &str, plaintext: &str) -> String {
    let mut h = Sha256::new();
    h.update(scope.as_bytes());
    h.update(plaintext.as_bytes());
    let mut hex = hex::encode(h.finalize());
    hex.truncate(8);
    hex
}

impl Keyring {
    /// The keyring from the raw values of both variables (`None` = unset; blank counts as unset). Both unset →
    /// `Ok(None)`. One without the other, a value that is not a JSON object of strings, an empty object, a bad kid,
    /// a key that is not base64 of exactly 32 bytes, or a current kid missing from the set is an error naming the
    /// variable — never echoing a key.
    pub fn parse(keys_json: Option<&str>, current: Option<&str>) -> Result<Option<Self>, String> {
        let keys_json = keys_json.map(str::trim).filter(|s| !s.is_empty());
        let current = current.map(str::trim).filter(|s| !s.is_empty());
        let (raw, current) = match (keys_json, current) {
            (None, None) => return Ok(None),
            (Some(_), None) => return Err(format!("{KEYS_VAR} is set but {CURRENT_VAR} is not")),
            (None, Some(_)) => return Err(format!("{CURRENT_VAR} is set but {KEYS_VAR} is not")),
            (Some(raw), Some(current)) => (raw, current),
        };
        let encoded: BTreeMap<String, String> = serde_json::from_str(raw)
            .map_err(|_| format!("{KEYS_VAR} must be a JSON object of kid to base64 key"))?;
        if encoded.is_empty() {
            return Err(format!("{KEYS_VAR} holds no key"));
        }
        let mut keys = BTreeMap::new();
        for (kid, b64) in encoded {
            if !valid_kid(&kid) {
                return Err(format!(
                    "{KEYS_VAR}: every kid must match [A-Za-z0-9_-]{{1,{KID_MAX}}}"
                ));
            }
            let bytes = STANDARD
                .decode(b64.trim())
                .map_err(|_| format!("{KEYS_VAR}: kid {kid} is not valid base64"))?;
            if bytes.len() != KEY_LEN {
                return Err(format!(
                    "{KEYS_VAR}: kid {kid} must decode to exactly {KEY_LEN} bytes, got {}",
                    bytes.len()
                ));
            }
            let cipher = Aes256Gcm::new_from_slice(&bytes)
                .map_err(|_| format!("{KEYS_VAR}: kid {kid} is not a usable AES-256 key"))?;
            keys.insert(kid, cipher);
        }
        if !keys.contains_key(current) {
            return Err(format!(
                "{CURRENT_VAR} names {current}, which is not in {KEYS_VAR}"
            ));
        }
        Ok(Some(Self {
            current: current.to_string(),
            keys,
        }))
    }

    /// Seal `plaintext` for `scope` under the current kid.
    pub fn seal(&self, scope: &str, plaintext: &str) -> Sealed {
        let cipher = self
            .keys
            .get(&self.current)
            .expect("parse() guarantees the current kid is in the set");
        let kid = self.current.as_str();
        let nonce = Nonce::<Aes256Gcm>::generate();
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &aad(scope, kid),
                },
            )
            .expect("AES-GCM encryption of an in-memory buffer cannot fail");
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ciphertext);
        Sealed {
            sealed: format!("{PREFIX}{kid}:{}", URL_SAFE_NO_PAD.encode(blob)),
            fingerprint: fingerprint(scope, plaintext),
        }
    }

    /// Open a `v2:` value sealed for `scope`. Wrong kid, wrong scope, wrong key or a damaged blob is
    /// [`Unreadable`], which carries no detail on purpose.
    pub fn open(&self, scope: &str, sealed: &str) -> Result<String, Unreadable> {
        let rest = sealed.strip_prefix(PREFIX).ok_or(Unreadable)?;
        let (kid, body) = rest.split_once(':').ok_or(Unreadable)?;
        let cipher = self.keys.get(kid).ok_or(Unreadable)?;
        let blob = URL_SAFE_NO_PAD.decode(body).map_err(|_| Unreadable)?;
        if blob.len() < NONCE_LEN + TAG_LEN {
            return Err(Unreadable);
        }
        let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = <&Nonce<Aes256Gcm>>::try_from(nonce).map_err(|_| Unreadable)?;
        let plaintext = cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: &aad(scope, kid),
                },
            )
            .map_err(|_| Unreadable)?;
        String::from_utf8(plaintext).map_err(|_| Unreadable)
    }
}

/// The AAD scope of one provider row: `<tenant>:llm_key:<row id>`.
pub fn provider_scope(tenant: uuid::Uuid, provider: uuid::Uuid) -> String {
    format!("{tenant}:llm_key:{provider}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test keys are built at runtime from a byte range, so no key literal is ever committed.
    fn b64(bytes: impl IntoIterator<Item = u8>) -> String {
        STANDARD.encode(bytes.into_iter().collect::<Vec<u8>>())
    }

    fn keys_json() -> String {
        format!(r#"{{"k1":"{}","k2":"{}"}}"#, b64(0u8..32), b64(32u8..64))
    }

    fn keyring() -> Keyring {
        Keyring::parse(Some(&keys_json()), Some("k1"))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn round_trip_under_the_current_kid() {
        let k = keyring();
        let s = k.seal("t:llm_key:p", "hunter2");
        assert!(s.sealed.starts_with("v2:k1:"), "{}", s.sealed);
        assert_eq!(s.fingerprint.len(), 8);
        assert_eq!(k.open("t:llm_key:p", &s.sealed).unwrap(), "hunter2");
        assert_eq!(format!("{s:?}").contains("hunter2"), false);
    }

    #[test]
    fn a_value_does_not_open_under_another_scope_or_kid() {
        let k = keyring();
        let s = k.seal("t:llm_key:p", "hunter2");
        assert_eq!(k.open("t:llm_key:q", &s.sealed), Err(Unreadable));
        // Relabelling the blob with another kid that holds the same bytes does not open it: the kid is in the AAD.
        let relabelled = s.sealed.replacen("v2:k1:", "v2:k2:", 1);
        assert_eq!(k.open("t:llm_key:p", &relabelled), Err(Unreadable));
        // A tampered blob does not open.
        let body = s.sealed.rsplit_once(':').unwrap().1;
        let mut blob = URL_SAFE_NO_PAD.decode(body).unwrap();
        blob[NONCE_LEN] ^= 1;
        let tampered = format!("v2:k1:{}", URL_SAFE_NO_PAD.encode(&blob));
        assert_eq!(k.open("t:llm_key:p", &tampered), Err(Unreadable));
    }

    #[test]
    fn rotation_keeps_old_values_openable() {
        let old = keyring();
        let s = old.seal("t:llm_key:p", "hunter2");
        // Add k2, move current to it: values sealed under k1 still open.
        let rotated = Keyring::parse(Some(&keys_json()), Some("k2"))
            .unwrap()
            .unwrap();
        assert_eq!(rotated.open("t:llm_key:p", &s.sealed).unwrap(), "hunter2");
        assert!(
            rotated
                .seal("t:llm_key:p", "x")
                .sealed
                .starts_with("v2:k2:")
        );
    }

    #[test]
    fn fingerprint_is_scoped_and_stable() {
        assert_eq!(fingerprint("a", "x"), fingerprint("a", "x"));
        assert_ne!(fingerprint("a", "x"), fingerprint("b", "x"));
        assert_ne!(fingerprint("a", "x"), fingerprint("a", "y"));
    }

    #[test]
    fn bad_configuration_is_named_without_echoing_a_key() {
        assert!(Keyring::parse(None, None).unwrap().is_none());
        assert!(
            Keyring::parse(Some("{}"), None)
                .unwrap_err()
                .contains(KEYS_VAR)
        );
        assert!(
            Keyring::parse(Some("not json"), Some("k1"))
                .unwrap_err()
                .contains(KEYS_VAR)
        );
        assert!(
            Keyring::parse(Some(&format!(r#"{{"k1":"{}"}}"#, b64(0u8..16))), Some("k1"))
                .unwrap_err()
                .contains("exactly 32 bytes")
        );
        assert!(
            Keyring::parse(Some(&keys_json()), Some("nope"))
                .unwrap_err()
                .contains(CURRENT_VAR)
        );
        assert!(
            Keyring::parse(
                Some(&format!(r#"{{"bad kid!":"{}"}}"#, b64(0u8..32))),
                Some("bad kid!")
            )
            .unwrap_err()
            .contains("kid must match")
        );
        let k = keyring();
        assert!(!format!("{k:?}").contains(&b64(0u8..32)));
        assert!(format!("{k:?}").contains("k1"));
    }
}
