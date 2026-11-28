//! The `faq.ask` job persona: carebus-api's effective job settings (actor, stance, preamble,
//! max_tokens) for one scope. The worker sends them to strata_rag `/api/ask` as `instructions`
//! and the answer budget, so the answer keeps the job's voice while strata_rag generates it.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::scope::Scope;

/// The model's own budget when the job does not set one.
pub const DEFAULT_MAX_TOKENS: u32 = 400;
pub const MIN_ANSWER_TOKENS: u32 = 32;
/// strata_rag's `MAX_ANSWER_TOKENS`: a configured budget above it is clamped.
pub const MAX_ANSWER_TOKENS: u32 = 8192;
/// One settings read's budget: a small JSON round trip, never the generation.
pub const JOB_PROMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// carebus-api's route (service token only).
pub const JOB_SETTINGS_PATH: &str = "/api/internal/job-settings";

/// The effective job settings (carebus-api `GET /api/jobs/{job}/settings`'s `effective` map).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct JobSettings {
    #[serde(default)]
    pub preamble: String,
    #[serde(default)]
    pub actor: String,
    #[serde(default)]
    pub stance: String,
    /// A string because that is what the settings surface stores; parsed and clamped here.
    #[serde(default)]
    pub max_tokens: Option<String>,
}

impl JobSettings {
    pub fn max_tokens(&self) -> u32 {
        self.max_tokens
            .as_deref()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .clamp(MIN_ANSWER_TOKENS, MAX_ANSWER_TOKENS)
    }

    /// The job's persona: the actor line, the stance and the preamble — no citation rules
    /// (strata_rag owns those).
    pub fn instructions(&self) -> String {
        let mut parts = Vec::new();
        if !self.actor.trim().is_empty() {
            parts.push(format!("You are {}.", self.actor.trim()));
        }
        if !self.stance.trim().is_empty() {
            parts.push(self.stance.trim().to_string());
        }
        if !self.preamble.trim().is_empty() {
            parts.push(self.preamble.trim().to_string());
        }
        parts.join("\n\n")
    }
}

/// What a `faq.ask` sends strata_rag besides the question: the job's persona and answer budget.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Instructions {
    pub text: String,
    pub max_tokens: u32,
}

#[derive(Deserialize)]
struct SettingsResponse {
    settings: JobSettings,
}

/// The worker's carebus-api client; no credential of its own, only the service token.
#[derive(Clone)]
pub struct JobPromptClient {
    http: reqwest::Client,
    api_base: String,
    api_token: String,
}

impl JobPromptClient {
    /// The process environment. `Ok(None)` when carebus-api is not configured: a local run with
    /// `CAREBUS_GATES=off` still serves MCP tools, only without a persona.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let base = get("CAREBUS_API_URL")
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty());
        let token = get("CAREBUS_API_TOKEN")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let (Some(api_base), Some(api_token)) = (base, token) else {
            tracing::warn!(
                "faq job prompt disabled: CAREBUS_API_URL and CAREBUS_API_TOKEN are required for the job's persona"
            );
            return Ok(None);
        };
        // The base URL is deployment configuration; the client follows no redirect and uses no
        // proxy, so it cannot be steered off it.
        let http = reqwest::Client::builder()
            .timeout(JOB_PROMPT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .context("building the job-settings HTTP client")?;
        Ok(Some(Self {
            http,
            api_base,
            api_token,
        }))
    }

    /// The job persona and answer budget for one `faq.ask`, over the service token.
    ///
    /// `Ok(None)` when carebus-api does not serve the route (an older API during a rollout, or an
    /// unknown job): the answer is then composed with strata_rag's default prompt. Any other
    /// failure is an error, and the step is worth retrying.
    pub async fn instructions(&self, scope: &Scope) -> Result<Option<Instructions>, String> {
        let resp = self
            .http
            .post(format!("{}{JOB_SETTINGS_PATH}", self.api_base))
            .bearer_auth(&self.api_token)
            .json(&json!({"tenant_id": scope.tenant, "job": scope.job}))
            .send()
            .await
            .map_err(|e| format!("job settings unreachable: {e}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!("job settings refused: {status}"));
        }
        let parsed: SettingsResponse = resp
            .json()
            .await
            .map_err(|e| format!("job settings unreadable: {e}"))?;
        Ok(Some(Instructions {
            text: parsed.settings.instructions(),
            max_tokens: parsed.settings.max_tokens(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> JobSettings {
        JobSettings {
            preamble: "We are the nonagentic.ai website host.".into(),
            actor: "the nonagentic.ai website assistant".into(),
            stance: "Be direct and helpful; hand work to George.".into(),
            max_tokens: Some("512".into()),
        }
    }

    #[test]
    fn the_instructions_are_the_persona_without_the_citation_rules() {
        assert_eq!(
            settings().instructions(),
            "You are the nonagentic.ai website assistant.\n\n\
             Be direct and helpful; hand work to George.\n\n\
             We are the nonagentic.ai website host."
        );
        assert_eq!(JobSettings::default().instructions(), "");
        assert_eq!(
            JobSettings {
                stance: "  ".into(),
                actor: String::new(),
                preamble: "Only the preamble.".into(),
                max_tokens: None,
            }
            .instructions(),
            "Only the preamble."
        );
    }

    #[test]
    fn max_tokens_is_parsed_and_clamped() {
        assert_eq!(settings().max_tokens(), 512);
        assert_eq!(
            JobSettings {
                max_tokens: Some("999999".into()),
                ..Default::default()
            }
            .max_tokens(),
            MAX_ANSWER_TOKENS
        );
        assert_eq!(
            JobSettings {
                max_tokens: Some("1".into()),
                ..Default::default()
            }
            .max_tokens(),
            MIN_ANSWER_TOKENS
        );
        assert_eq!(
            JobSettings {
                max_tokens: Some("nonsense".into()),
                ..Default::default()
            }
            .max_tokens(),
            DEFAULT_MAX_TOKENS
        );
    }

    #[test]
    fn the_client_is_absent_without_the_service_settings() {
        assert!(JobPromptClient::from_vars(|_| None).unwrap().is_none());
        let only_url = |k: &str| (k == "CAREBUS_API_URL").then(|| "http://api".to_string());
        assert!(JobPromptClient::from_vars(only_url).unwrap().is_none());
        let both = |k: &str| match k {
            "CAREBUS_API_URL" => Some("http://api/".to_string()),
            "CAREBUS_API_TOKEN" => Some("svc".to_string()),
            _ => None,
        };
        assert!(JobPromptClient::from_vars(both).unwrap().is_some());
    }
}
