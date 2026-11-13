//! Client-credentials tokens for the signup's service accounts and the error every upstream admin call returns
//! (agent `service_token.rs`). Never carries a secret or a URL (a lookup's URL carries the requester's address).
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::signup::config::ClientCredentials;

const EXPIRY_MARGIN: Duration = Duration::from_secs(30);
const DEFAULT_LIFETIME: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct UpstreamError {
    pub status: Option<u16>,
    pub message: String,
}

impl UpstreamError {
    pub fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn status(what: &str, status: StatusCode) -> Self {
        Self::new(
            Some(status.as_u16()),
            format!("{what} failed: HTTP {status}"),
        )
    }

    /// ⛔ NEVER `{e}`: reqwest's message carries the request URL (and so `users?email=…`).
    pub fn transport(what: &str, e: &reqwest::Error) -> Self {
        let kind = if e.is_timeout() {
            "timed out"
        } else if e.is_connect() {
            "could not connect"
        } else if e.is_decode() {
            "the response body could not be decoded"
        } else if e.is_body() {
            "the body could not be read"
        } else if e.is_redirect() {
            "too many redirects"
        } else if e.is_request() {
            "the request could not be sent"
        } else {
            "transport error"
        };
        Self::new(None, format!("{what} failed: {kind}"))
    }
}

/// `s` percent-encoded for a query value or a path segment: every byte except the RFC 3986 unreserved characters.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
}

/// One service account's access token, fetched with the client-credentials grant and cached.
pub struct ServiceToken {
    creds: ClientCredentials,
    http: reqwest::Client,
    cached: Mutex<Option<(String, Instant)>>,
}

impl ServiceToken {
    pub fn new(creds: &ClientCredentials, http: reqwest::Client) -> Self {
        Self {
            creds: creds.clone(),
            http,
            cached: Mutex::new(None),
        }
    }

    /// Drop the cached token: the next [`Self::get`] fetches a new one (the old one was refused).
    pub async fn forget(&self) {
        *self.cached.lock().await = None;
    }

    pub async fn get(&self) -> Result<String, UpstreamError> {
        let mut c = self.cached.lock().await;
        if let Some((token, refresh_at)) = c.as_ref()
            && Instant::now() < *refresh_at
        {
            return Ok(token.clone());
        }
        let what = "service-account token request";
        let resp = self
            .http
            .post(self.creds.token_url.as_str())
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.creds.client_id.as_str()),
                ("client_secret", self.creds.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        if !resp.status().is_success() {
            return Err(UpstreamError::status(what, resp.status()));
        }
        let t: TokenResponse = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport("service-account token response", &e))?;
        let token = t.access_token.filter(|s| !s.is_empty()).ok_or_else(|| {
            UpstreamError::new(None, "service-account token response has no access_token")
        })?;
        let lifetime = t
            .expires_in
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_LIFETIME);
        *c = Some((
            token.clone(),
            Instant::now() + lifetime.saturating_sub(EXPIRY_MARGIN),
        ));
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_transport_error_names_no_url_and_no_query() {
        let e = reqwest::Client::new()
            .get("http://127.0.0.1:1/admin/realms/care-bus/users?email=ann%40globex.com&exact=true")
            .send()
            .await
            .unwrap_err();
        assert!(e.to_string().contains("ann%40globex.com"), "{e}");
        let m = UpstreamError::transport("keycloak user lookup by email", &e).message;
        assert!(
            !m.contains("globex") && !m.contains("127.0.0.1") && !m.contains('?'),
            "{m}"
        );
    }

    #[test]
    fn encode_keeps_only_unreserved() {
        assert_eq!(encode("a+b@c.d e/f"), "a%2Bb%40c.d%20e%2Ff");
    }
}
