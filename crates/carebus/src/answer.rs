//! The website job's answer stage: the job's voice and the retrieved passages become one cited
//! answer.
//!
//! Generation is slow (the owner accepts minutes), so it runs in the `faq` worker, not in the
//! interaction daemon: the daemon stays a fast composer, or one answer would block every reply on
//! its consumer. The job settings and the model provider come from carebus-api in one request
//! (`POST /api/internal/answer-context`); the worker holds no model credential of its own.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::scope::Scope;

/// The model's own budget when the job does not set one.
pub const DEFAULT_MAX_TOKENS: u32 = 400;
pub const MIN_ANSWER_TOKENS: u32 = 32;
pub const MAX_ANSWER_TOKENS: u32 = 2048;
/// One answer's whole budget: the owner accepts long waits (2026-11-03), and the faq tool's own
/// timeout in the deployment is the outer bound.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

const RULES: &str = "Answer using only the numbered sources provided below. Cite the source of \
every statement with its number in square brackets, like [1] or [2]. If the sources do not \
contain the answer, say that you do not know. Do not use outside knowledge. Answer in complete \
sentences that state the answer itself; a citation marker on its own is not an answer.";

/// The effective job settings (carebus-api `GET /api/jobs/{job}/settings`'s `effective` map).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
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

    /// The job's persona: the actor line, the stance and the preamble — no citation rules. This is
    /// what a `faq.ask` sends strata_rag as `instructions`, so the answer keeps the job's voice
    /// while strata_rag generates it.
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

    /// The system prompt: who answers, how, and the fixed citation contract.
    pub fn system_prompt(&self) -> String {
        let persona = self.instructions();
        if persona.is_empty() {
            RULES.to_string()
        } else {
            format!("{persona}\n\n{RULES}")
        }
    }
}

/// What a `faq.ask` sends strata_rag besides the question: the job's persona and the answer budget.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Instructions {
    pub text: String,
    pub max_tokens: u32,
}

/// The model to call: the tenant's `answer` binding, else the deployment's platform default.
#[derive(Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Provider {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub token: String,
}

/// Never prints the provider token.
impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("token", &(!self.token.is_empty()).then_some("<redacted>"))
            .finish()
    }
}

/// One turn's answer context, from carebus-api.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct AnswerContext {
    pub settings: JobSettings,
    pub provider: Option<Provider>,
}

/// The answerer's carebus-api client; the model credential arrives per call, never from env.
#[derive(Clone)]
pub struct Answerer {
    http: reqwest::Client,
    api_base: String,
    api_token: String,
}

impl Answerer {
    /// The process environment. `Ok(None)` when carebus-api is not configured: a local run with
    /// `CAREBUS_GATES=off` still serves MCP tools; only the answer action is unavailable.
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
                "faq answerer disabled: CAREBUS_API_URL and CAREBUS_API_TOKEN are required for the answer stage"
            );
            return Ok(None);
        };
        // A provider base_url is admin-set and checked by carebus-api; the worker's own client
        // follows no redirect and uses no proxy, so it cannot be steered off that URL.
        let http = reqwest::Client::builder()
            .timeout(ANSWER_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .context("building the answer HTTP client")?;
        Ok(Some(Self {
            http,
            api_base,
            api_token,
        }))
    }

    /// The job's settings and provider for one turn, over the service token.
    pub async fn context(&self, scope: &Scope) -> Result<AnswerContext, String> {
        let resp = self
            .http
            .post(format!("{}/api/internal/answer-context", self.api_base))
            .bearer_auth(&self.api_token)
            .json(&json!({"tenant_id": scope.tenant, "job": scope.job}))
            .send()
            .await
            .map_err(|e| format!("answer context unreachable: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("answer context refused: {status}"));
        }
        resp.json()
            .await
            .map_err(|e| format!("answer context unreadable: {e}"))
    }

    /// The job persona and answer budget a `faq.ask` sends strata_rag, over the service token.
    ///
    /// `Ok(None)` when carebus-api does not serve the settings route (an older API during a
    /// rollout, or an unknown job): the answer is then composed with strata_rag's default prompt.
    /// Any other failure is an error, and the step is worth retrying.
    pub async fn instructions(&self, scope: &Scope) -> Result<Option<Instructions>, String> {
        let resp = self
            .http
            .post(format!("{}/api/internal/answer-context", self.api_base))
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
        let ctx: AnswerContext = resp
            .json()
            .await
            .map_err(|e| format!("job settings unreadable: {e}"))?;
        Ok(Some(Instructions {
            text: ctx.settings.instructions(),
            max_tokens: ctx.settings.max_tokens(),
        }))
    }

    /// One grounded answer. The result is the `faq` step's fact: `answer`, the cited chunk ids,
    /// the numbered sources and timings. An uncited answer keeps `citations: []`, which the
    /// composer's guard turns into the fallback.
    pub async fn answer(
        &self,
        ctx: &AnswerContext,
        question: &str,
        sources: &[Value],
    ) -> Result<Value, String> {
        let provider = ctx
            .provider
            .as_ref()
            .filter(|p| !p.base_url.trim().is_empty() && !p.model.trim().is_empty())
            .ok_or("no answer provider is configured for this tenant")?;
        if sources.is_empty() {
            return Ok(json!({
                "answer": "", "citations": [], "sources": [], "timings_ms": {"total": 0}
            }));
        }
        let started = Instant::now();
        let body = json!({
            "model": provider.model,
            "messages": [
                {"role": "system", "content": ctx.settings.system_prompt()},
                {"role": "user", "content": source_prompt(question, sources)},
            ],
            "max_tokens": ctx.settings.max_tokens(),
            "temperature": 0.2,
        });
        let resp = self
            .http
            .post(format!(
                "{}/chat/completions",
                provider.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&provider.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("answer model unreachable: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("answer model refused: {status}"));
        }
        let payload: Value = resp
            .json()
            .await
            .map_err(|e| format!("answer model reply unreadable: {e}"))?;
        let content = payload["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if content.is_empty() {
            return Err("answer model returned no text".into());
        }
        let citations = citations(&content, sources);
        let elapsed = started.elapsed().as_millis() as u64;
        Ok(json!({
            "answer": content,
            "citations": citations,
            "sources": sources,
            "timings_ms": {"generate": elapsed, "total": elapsed},
        }))
    }
}

/// The user message: one numbered source per block, then the question.
pub fn source_prompt(question: &str, sources: &[Value]) -> String {
    let mut out = String::from("Sources:\n\n");
    for (i, source) in sources.iter().enumerate() {
        let label = source
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .or_else(|| source.get("source").and_then(Value::as_str))
            .unwrap_or("source");
        let text = source
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        out.push_str(&format!("[{}] ({label})\n{text}\n\n", i + 1));
    }
    out.push_str("Question: ");
    out.push_str(question);
    out
}

/// The source numbers the answer cites, mapped to their chunk ids; empty when it cites none (the
/// composer then treats the answer as ungrounded).
pub fn citations(answer: &str, sources: &[Value]) -> Vec<String> {
    let mut cited = BTreeSet::new();
    let bytes = answer.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'['
            && let Some(close) = answer[i + 1..].find(']')
        {
            if let Ok(n) = answer[i + 1..i + 1 + close].parse::<usize>()
                && (1..=sources.len()).contains(&n)
            {
                cited.insert(n);
            }
            i += close + 2;
            continue;
        }
        i += 1;
    }
    cited
        .into_iter()
        .filter_map(|n| {
            sources
                .get(n - 1)
                .and_then(|s| s.get("chunk_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
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
    fn the_system_prompt_carries_actor_stance_and_preamble() {
        let p = settings().system_prompt();
        assert!(
            p.starts_with("You are the nonagentic.ai website assistant."),
            "{p}"
        );
        assert!(
            p.contains("Be direct and helpful; hand work to George."),
            "{p}"
        );
        assert!(p.contains("We are the nonagentic.ai website host."), "{p}");
        assert!(p.contains("Cite the source of every statement"), "{p}");
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
    fn the_source_prompt_numbers_each_passage() {
        let sources = vec![
            json!({"chunk_id": "c1", "text": "first", "title": "A"}),
            json!({"chunk_id": "c2", "text": "second", "source": "b.md"}),
        ];
        let p = source_prompt("Why?", &sources);
        assert!(p.contains("[1] (A)\nfirst"), "{p}");
        assert!(p.contains("[2] (b.md)\nsecond"), "{p}");
        assert!(p.ends_with("Question: Why?"), "{p}");
    }

    #[test]
    fn citations_map_numbers_to_chunk_ids_and_ignore_out_of_range() {
        let sources = vec![json!({"chunk_id": "c1"}), json!({"chunk_id": "c2"})];
        assert_eq!(
            citations("A [2], then [1] and [2] again. [9]", &sources),
            vec!["c1".to_string(), "c2".to_string()]
        );
        assert!(citations("no citation", &sources).is_empty());
    }

    #[test]
    fn the_provider_debug_never_prints_the_token() {
        let p = Provider {
            base_url: "http://llm.example.test/v1".into(),
            model: "m".into(),
            token: "secret-token".into(),
        };
        let debug = format!("{p:?}");
        assert!(!debug.contains("secret-token"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[tokio::test]
    async fn the_answer_posts_the_prompt_and_maps_citations() {
        use axum::{Json, Router, extract::State, routing::post};
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Seen(Arc<Mutex<Vec<Value>>>);
        async fn chat(State(seen): State<Seen>, Json(body): Json<Value>) -> Json<Value> {
            seen.0.lock().unwrap().push(body);
            Json(json!({"choices": [{"message": {"content": "Within 5 business days [1]."}}]}))
        }

        let seen = Seen(Arc::new(Mutex::new(Vec::new())));
        let app = Router::new()
            .route("/v1/chat/completions", post(chat))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let answerer = Answerer::from_vars(|k| match k {
            "CAREBUS_API_URL" => Some("http://127.0.0.1:9".into()),
            "CAREBUS_API_TOKEN" => Some("svc-token".into()),
            _ => None,
        })
        .unwrap()
        .expect("an answerer");
        let ctx = AnswerContext {
            settings: settings(),
            provider: Some(Provider {
                base_url: format!("http://{addr}/v1"),
                model: "test-model".into(),
                token: "provider-token".into(),
            }),
        };
        let sources =
            vec![json!({"chunk_id": "c1", "text": "five business days", "title": "Returns"})];
        let out = answerer
            .answer(&ctx, "How long is a refund?", &sources)
            .await
            .expect("an answer");

        assert_eq!(out["answer"], "Within 5 business days [1].");
        assert_eq!(out["citations"], json!(["c1"]));
        let body = seen.0.lock().unwrap()[0].clone();
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["max_tokens"], 512);
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(
            system.contains("You are the nonagentic.ai website assistant."),
            "{system}"
        );
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("[1] (Returns)"), "{user}");
        assert!(user.ends_with("Question: How long is a refund?"), "{user}");
    }

    #[tokio::test]
    async fn the_context_fetches_settings_and_provider_over_the_service_token() {
        use axum::{Json, Router, extract::State, routing::post};
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Seen(Arc<Mutex<Vec<String>>>);
        async fn context(
            State(seen): State<Seen>,
            headers: axum::http::HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            seen.0.lock().unwrap().push(format!("{auth}|{body}"));
            Json(json!({
                "settings": {"preamble": "p", "actor": "a", "stance": "s", "max_tokens": "123"},
                "provider": {"base_url": "http://llm/v1", "model": "m", "token": "t"}
            }))
        }

        let seen = Seen(Arc::new(Mutex::new(Vec::new())));
        let app = Router::new()
            .route("/api/internal/answer-context", post(context))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let base = format!("http://{addr}");
        let answerer = Answerer::from_vars(|k| match k {
            "CAREBUS_API_URL" => Some(base.clone()),
            "CAREBUS_API_TOKEN" => Some("svc-token".into()),
            _ => None,
        })
        .unwrap()
        .expect("an answerer");
        let scope = Scope {
            tenant: uuid::Uuid::new_v4(),
            job: "website_chat".into(),
        };
        let ctx = answerer.context(&scope).await.expect("a context");
        assert_eq!(ctx.settings.max_tokens(), 123);
        assert_eq!(ctx.provider.as_ref().unwrap().model, "m");

        let recorded = seen.0.lock().unwrap()[0].clone();
        assert!(recorded.starts_with("Bearer svc-token|"), "{recorded}");
        assert!(recorded.contains("\"job\":\"website_chat\""), "{recorded}");
    }
}
