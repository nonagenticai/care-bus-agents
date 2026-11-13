//! Secret-shaped value detection. Configuration stores secret *names* (`secret_ref`), never values.
use serde_json::Value;

use crate::error::{ApiError, ApiResult};

/// Keys whose non-empty string value is always a secret, whatever it looks like (compared ignoring case).
const SECRET_KEYS: [&str; 9] = [
    "password",
    "passwd",
    "secret",
    "client_secret",
    "api_key",
    "apikey",
    "access_token",
    "token",
    "private_key",
];

/// The forms a secret can be echoed in: as is, base64 (standard and URL-safe, padded or not) and
/// percent-encoded (upper- and lowercase hex), longest first, without duplicates. Empty for an empty secret.
pub fn secret_forms(secret: &str) -> Vec<String> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    if secret.is_empty() {
        return Vec::new();
    }
    let pct = |upper: bool| -> String {
        secret
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    (b as char).to_string()
                } else if upper {
                    format!("%{b:02X}")
                } else {
                    format!("%{b:02x}")
                }
            })
            .collect()
    };
    let mut forms = vec![
        secret.to_string(),
        STANDARD.encode(secret),
        STANDARD_NO_PAD.encode(secret),
        URL_SAFE.encode(secret),
        URL_SAFE_NO_PAD.encode(secret),
        pct(true),
        pct(false),
    ];
    forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    forms.dedup();
    forms
}

/// `text` with every [`secret_forms`] form of `secret` replaced by `[redacted]`.
pub fn scrub_secret(text: &str, secret: &str) -> String {
    secret_forms(secret)
        .iter()
        .fold(text.to_string(), |t, f| t.replace(f.as_str(), "[redacted]"))
}

pub fn is_secret_key(k: &str) -> bool {
    SECRET_KEYS.iter().any(|s| s.eq_ignore_ascii_case(k))
}

pub fn looks_like_secret(s: &str) -> bool {
    let t = s.trim();
    if t.starts_with("sk-") || t.starts_with("eyJ") || t.contains("-----BEGIN") {
        return true;
    }
    // Long, high-entropy tokens, including base64 (`/`, `+`). URLs, dotted names and prose are not.
    if t.len() < 40
        || t.contains("://")
        || t.chars().any(|c| c.is_whitespace() || c == ':' || c == '.')
    {
        return false;
    }
    let classes = [
        t.chars().any(|c| c.is_ascii_lowercase()),
        t.chars().any(|c| c.is_ascii_uppercase()),
        t.chars().any(|c| c.is_ascii_digit()),
    ];
    classes.iter().filter(|b| **b).count() >= 3
}

/// Walks every string in `v` (object keys included) and rejects a secret-shaped one, or any non-empty
/// string under a secret key name (`password`, `token`, ...), unless its key is in `allowed`.
/// The error names the field only, never the value.
pub fn reject_secret_fields(v: &Value, allowed: &[&str]) -> ApiResult<()> {
    fn name(key: &str) -> String {
        if key.len() > 64 || looks_like_secret(key) {
            "(unnamed field)".into()
        } else {
            key.into()
        }
    }
    fn walk(path: &str, v: &Value, allowed: &[&str]) -> ApiResult<()> {
        match v {
            Value::String(s)
                if (looks_like_secret(s) || (!s.trim().is_empty() && is_secret_key(path)))
                    && !allowed.contains(&path) =>
            {
                Err(ApiError::SecretRejected(name(path)))
            }
            Value::Object(m) => m.iter().try_for_each(|(k, v)| {
                if looks_like_secret(k) {
                    return Err(ApiError::SecretRejected(name(k)));
                }
                walk(k, v, allowed)
            }),
            Value::Array(a) => a.iter().try_for_each(|v| walk(path, v, allowed)),
            _ => Ok(()),
        }
    }
    walk("", v, allowed)
}

/// A `secret_ref` is a Secret name, `^[a-z0-9._-]+(/[a-z0-9._-]+)*$`.
pub fn valid_secret_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 253
        && r.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
        })
}

/// 400 `invalid_secret_ref` unless `r` is `None` or a valid secret name.
pub fn check_secret_ref(r: Option<&str>) -> ApiResult<()> {
    match r {
        Some(r) if !valid_secret_ref(r) => Err(ApiError::BadRequest(
            "invalid_secret_ref",
            "secret_ref must be a secret name such as carebus-llm/api-key (lowercase letters, digits, . _ - and /).".into(),
        )),
        _ => Ok(()),
    }
}

/// Typed deserialize with a fixed 400 message: serde's own text echoes the offending value.
pub fn parse_body<T: serde::de::DeserializeOwned>(raw: Value, schema: &str) -> ApiResult<T> {
    serde_json::from_value(raw).map_err(|_| {
        ApiError::BadRequest(
            "invalid_body",
            format!("The request body does not match the {schema} schema."),
        )
    })
}

#[cfg(test)]
mod forms_tests {
    use super::secret_forms;

    #[test]
    fn a_secret_is_scrubbed_raw_base64_and_percent_encoded() {
        let sec = "s3cr/t+VAL=ue?&";
        let forms = secret_forms(sec);
        for want in [
            sec,
            "czNjci90K1ZBTD11ZT8m",
            "czNjci90K1ZBTD11ZT8m",
            "s3cr%2Ft%2BVAL%3Due%3F%26",
            "s3cr%2ft%2bVAL%3due%3f%26",
        ] {
            assert!(forms.iter().any(|f| f == want), "missing {want}: {forms:?}");
        }
        let text = format!("got {sec}, b64 czNjci90K1ZBTD11ZT8m, url s3cr%2Ft%2BVAL%3Due%3F%26");
        let clean = super::scrub_secret(&text, sec);
        assert_eq!(clean, "got [redacted], b64 [redacted], url [redacted]");
        assert!(secret_forms("").is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejection_names_the_field_not_the_value() {
        let e = reject_secret_fields(&json!({"options": {"api_key": "sk-live-abcdefgh"}}), &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("api_key") && !e.contains("abcdefgh"), "{e}");
        let e = reject_secret_fields(&json!({"sk-live-abcdefgh": 1}), &[])
            .unwrap_err()
            .to_string();
        assert!(!e.contains("abcdefgh"), "{e}");
        assert!(reject_secret_fields(&json!({"a": ["x", {"b": "local-llm"}]}), &[]).is_ok());
        assert!(reject_secret_fields(&json!({"a": "sk-x"}), &["a"]).is_ok());
    }

    #[test]
    fn base64_with_slash_or_plus_is_a_secret_but_a_url_is_not() {
        // AWS-style 40-char secret access key: base64 alphabet with `/` and `+`.
        let aws = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(aws.len(), 40);
        assert!(looks_like_secret(aws));
        assert!(looks_like_secret(
            "Zx81Qm0aB+c9dE7fG2hJ4kL6mN8pQ1rS3tU5vW7x"
        ));
        let e = reject_secret_fields(&json!({"options": {"aws": aws}}), &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("aws") && !e.contains("wJalr"), "{e}");
        // URLs (with `://`) stay allowed, however long and mixed-case.
        let url = "https://Vllm.Internal:8001/v1/Models/Qwen3ABCdef123456789/xyz+q";
        assert!(!looks_like_secret(url));
        assert!(reject_secret_fields(&json!({"base_url": url}), &[]).is_ok());
    }

    #[test]
    fn secret_key_names_reject_any_non_empty_string() {
        for k in [
            "password",
            "Passwd",
            "SECRET",
            "client_secret",
            "api_key",
            "ApiKey",
            "access_token",
            "token",
            "private_key",
        ] {
            let e = reject_secret_fields(&json!({"options": {k: "hunter2"}}), &[])
                .unwrap_err()
                .to_string();
            assert!(e.contains(k) && !e.contains("hunter2"), "{k}: {e}");
            // empty string and non-strings under the key are not values
            assert!(reject_secret_fields(&json!({k: ""}), &[]).is_ok(), "{k}");
            assert!(reject_secret_fields(&json!({k: 3}), &[]).is_ok(), "{k}");
        }
        assert!(reject_secret_fields(&json!({"password": "hunter2"}), &[]).is_err());
        // near-misses are fine
        for (k, v) in [
            ("max_tokens", "512"),
            ("token_budget", "1000"),
            ("secret_ref", "carebus-llm/api-key"),
            ("password_hint", "ask the admin"),
        ] {
            assert!(reject_secret_fields(&json!({k: v}), &[]).is_ok(), "{k}");
        }
        // an allowed path is still allowed
        assert!(reject_secret_fields(&json!({"token": "x"}), &["token"]).is_ok());
    }

    #[test]
    fn secret_ref_shape() {
        for ok in ["carebus-llm/vllm-license-token", "a", "a.b/c_d-e"] {
            assert!(valid_secret_ref(ok), "{ok}");
        }
        for bad in [
            "",
            &"a".repeat(254),
            "/a",
            "a/",
            "a//b",
            "A/b",
            "a b",
            "a/b?x",
            "https://x",
        ] {
            assert!(!valid_secret_ref(bad), "{bad}");
        }
    }
}
