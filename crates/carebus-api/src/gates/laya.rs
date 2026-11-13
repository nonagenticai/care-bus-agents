//! Laya (Jev System-1): `POST {base_url}/v1/systemone`, bearer licence token, body
//! `{model, state, questions:{key:{type,instructions,criteria}}}` → `{model, answers:{key:…}, usage}`.
//! 422 = validation (`{detail:[{loc,msg,type}]}`), 503 = backend down. Errors never carry the URL, the token or an
//! upstream body.
use std::time::Duration;

use serde_json::{Map, Value, json};

/// Why a call produced no answers. The text is safe to log and to store in `gate_verdicts.error`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayaError {
    #[error("laya timed out after {0} ms")]
    Timeout(u64),
    #[error("laya answered HTTP {0}")]
    Status(u16),
    #[error("laya is unreachable")]
    Transport,
    #[error("laya returned an unreadable body")]
    BadBody,
}

/// A body larger than this is not read.
const MAX_BODY: usize = 1 << 20;

pub fn systemone_url(base_url: &str) -> String {
    format!("{}/v1/systemone", base_url.trim_end_matches('/'))
}

/// One call with every question of a gate point. Returns the `answers` object.
pub async fn systemone(
    http: &reqwest::Client,
    base_url: &str,
    token: Option<&str>,
    model: &str,
    state: &str,
    questions: Map<String, Value>,
    timeout: Duration,
) -> Result<Map<String, Value>, LayaError> {
    let ms = timeout.as_millis() as u64;
    let body = json!({"model": model, "state": state, "questions": questions});
    let mut req = http
        .post(systemone_url(base_url))
        .timeout(timeout)
        .json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let call = async {
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                LayaError::Timeout(ms)
            } else {
                LayaError::Transport
            }
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LayaError::Status(status.as_u16()));
        }
        let bytes = resp.bytes().await.map_err(|e| {
            if e.is_timeout() {
                LayaError::Timeout(ms)
            } else {
                LayaError::Transport
            }
        })?;
        if bytes.len() > MAX_BODY {
            return Err(LayaError::BadBody);
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| LayaError::BadBody)?;
        match v.get("answers") {
            Some(Value::Object(a)) => Ok(a.clone()),
            _ => Err(LayaError::BadBody),
        }
    };
    // The client timeout covers the request; this one also bounds a slow body.
    match tokio::time::timeout(timeout, call).await {
        Ok(r) => r,
        Err(_) => Err(LayaError::Timeout(ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_is_under_v1() {
        assert_eq!(
            systemone_url("https://api.nonagentic.ai/laya/"),
            "https://api.nonagentic.ai/laya/v1/systemone"
        );
    }
}
