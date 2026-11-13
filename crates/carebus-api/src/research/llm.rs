//! The research model: an OpenAI-compatible `POST {base_url}/chat/completions` with `tools` (function calling),
//! behind the [`Llm`] trait so tests drive the runner with a scripted fake.
//!
//! Which endpoint a run uses: the tenant's `research` binding (`research_write` for a forced final turn, falling
//! back to `research`) → an `openai` provider in `llm_providers` (token opened with `CAREBUS_SOURCE_KEY`, host
//! checked and pinned by crate::netguard, as for `qualify`). Unbound → the platform default ([`DefaultLlm`]): the
//! licence-gated vLLM gateway, bearer `VLLM_LICENSE_TOKEN`, as carebus' `RigQualifier` sends it.
use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::crypto::SourceKey;
use crate::routes::llm_providers::open_token;
use crate::tenancy::begin_scoped;

pub const USE_RESEARCH: &str = "research";
pub const USE_RESEARCH_WRITE: &str = "research_write";

/// The platform default model endpoint (no tenant binding).
#[derive(Clone)]
pub struct DefaultLlm {
    pub base_url: String,
    pub model: String,
    pub token: Option<String>,
}

impl std::fmt::Debug for DefaultLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultLlm")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("token", &self.token.as_ref().map(|_| "[set]"))
            .finish()
    }
}

impl DefaultLlm {
    /// `RESEARCH_LLM_BASE_URL` → `CAREBUS_LLM_BASE_URL` → the vLLM gateway; `RESEARCH_LLM_TOKEN` →
    /// `VLLM_LICENSE_TOKEN`; `RESEARCH_LLM_MODEL` → `CAREBUS_MODEL` → `local-llm`. Empty values count as unset.
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let opt = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        Self {
            base_url: opt("RESEARCH_LLM_BASE_URL")
                .or_else(|| opt("CAREBUS_LLM_BASE_URL"))
                .unwrap_or_else(|| carebus::interaction::DEFAULT_LLM_BASE_URL.to_string()),
            model: opt("RESEARCH_LLM_MODEL")
                .or_else(|| opt("CAREBUS_MODEL"))
                .unwrap_or_else(|| carebus::interaction::DEFAULT_MODEL.to_string()),
            token: opt("RESEARCH_LLM_TOKEN").or_else(|| opt("VLLM_LICENSE_TOKEN")),
        }
    }
}

/// Where one model call goes.
#[derive(Clone)]
pub struct LlmEndpoint {
    pub base_url: String,
    pub model: String,
    pub token: Option<String>,
    pub client: reqwest::Client,
    /// The tenant provider id, or `None` for the platform default.
    pub provider: Option<Uuid>,
    /// The end tenant sent as `X-Effective-Tenant-Id` (`carebus::interaction::EFFECTIVE_TENANT_HEADER`), so the
    /// platform model bills it: set only when this endpoint is the platform model ([`is_platform_url`]). With it go
    /// `X-App-Id` and `X-Project-Id` (`carebus::identity::router_headers`).
    pub effective_tenant: Option<Uuid>,
    /// Sent as `X-Actor-Id` next to `effective_tenant` (never without it): the person who started the run, else
    /// care-bus' service account (the runner sets it, `carebus::identity::pick_actor`).
    pub actor: Option<Uuid>,
}

pub struct ChatRequest {
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    /// `"required"` or `{"type":"function","function":{"name":…}}`; `None` = the server's default (`auto`).
    pub tool_choice: Option<Value>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

pub struct ChatReply {
    /// The assistant message as returned (`content`, `tool_calls`); the runner normalises it.
    pub message: Value,
    pub finish_reason: Option<String>,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Worth retrying: network, timeout, 429, 5xx.
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Permanent(String),
}

pub trait Llm: Send + Sync + 'static {
    fn chat(
        &self,
        ep: &LlmEndpoint,
        req: &ChatRequest,
    ) -> impl Future<Output = Result<ChatReply, LlmError>> + Send;
}

/// The plain OpenAI chat-completions dialect.
pub struct OpenAiLlm;

impl Llm for OpenAiLlm {
    async fn chat(&self, ep: &LlmEndpoint, req: &ChatRequest) -> Result<ChatReply, LlmError> {
        let resp = chat_request(ep, req).send().await.map_err(|e| {
            LlmError::Transient(if e.is_timeout() {
                "the model did not answer in time".into()
            } else {
                "the model endpoint is unreachable".into()
            })
        })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|_| LlmError::Transient("the model reply could not be read".into()))?;
        if !status.is_success() {
            let excerpt: String = text.chars().take(300).collect();
            let msg = format!(
                "the model endpoint answered HTTP {}: {excerpt}",
                status.as_u16()
            );
            return Err(
                if status.is_server_error() || status.as_u16() == 429 || status.as_u16() == 408 {
                    LlmError::Transient(msg)
                } else {
                    LlmError::Permanent(msg)
                },
            );
        }
        parse_reply(&text)
    }
}

/// `POST {base_url}/chat/completions` for `req`: bearer `ep.token`, and, when `ep.effective_tenant` is set (the
/// platform model only), `X-Effective-Tenant-Id`, `X-App-Id`, `X-Project-Id` and `X-Actor-Id` (`ep.actor`).
pub fn chat_request(ep: &LlmEndpoint, req: &ChatRequest) -> reqwest::RequestBuilder {
    let url = format!("{}/chat/completions", ep.base_url.trim_end_matches('/'));
    let mut body = json!({"model": ep.model, "messages": req.messages});
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.clone());
    }
    if let Some(tc) = &req.tool_choice {
        body["tool_choice"] = tc.clone();
    }
    let mut r = ep.client.post(&url).json(&body);
    if let Some(t) = ep.token.as_deref().filter(|t| !t.is_empty()) {
        r = r.bearer_auth(t);
    }
    if let Some(tenant) = ep.effective_tenant {
        for (name, value) in carebus::identity::router_headers(tenant, ep.actor) {
            r = r.header(name, value);
        }
    }
    r
}

/// `choices[0].message`, its `finish_reason` and `usage` from a chat-completions body.
pub fn parse_reply(text: &str) -> Result<ChatReply, LlmError> {
    let v: Value = serde_json::from_str(text)
        .map_err(|_| LlmError::Transient("the model reply is not JSON".into()))?;
    let choice = &v["choices"][0];
    let message = choice["message"].clone();
    if !message.is_object() {
        return Err(LlmError::Transient("the model reply has no message".into()));
    }
    let u = &v["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let mut usage = Usage {
        prompt_tokens: n("prompt_tokens"),
        completion_tokens: n("completion_tokens"),
        total_tokens: n("total_tokens"),
    };
    if usage.total_tokens == 0 {
        usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
    }
    Ok(ChatReply {
        message,
        finish_reason: choice["finish_reason"].as_str().map(String::from),
        usage,
    })
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim()
        .trim_end_matches('/')
        .eq_ignore_ascii_case(b.trim().trim_end_matches('/'))
}

/// The platform's own endpoints as configured for this deployment: a tenant provider may name the
/// platform's gateways explicitly, and such a URL still serves the platform model. Both come from the
/// environment so no deployment hostname is baked into the source.
///
/// * `INTERNAL_VLLM_URL` — the licence-gated in-cluster vLLM gateway.
/// * `INTERNAL_INFERENCE_ROUTER_URL` — inference-router's OpenAI-compatible base
///   (`{base}/chat/completions`), which bills `X-Effective-Tenant-Id` to that tenant.
fn configured_platform_urls() -> Vec<String> {
    ["INTERNAL_VLLM_URL", "INTERNAL_INFERENCE_ROUTER_URL"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

fn is_platform_url_within(base_url: &str, default: &DefaultLlm, configured: &[String]) -> bool {
    std::iter::once(carebus::interaction::DEFAULT_LLM_BASE_URL)
        .chain(std::iter::once(default.base_url.as_str()))
        .chain(configured.iter().map(String::as_str))
        .any(|u| same_url(base_url, u))
}

/// Whether `base_url` serves the platform's own (self-hosted) model: the public gateway
/// (`carebus::interaction::DEFAULT_LLM_BASE_URL`), an endpoint from [`configured_platform_urls`], or the
/// deployment's configured default. Exact URLs (case and a trailing `/` aside), never a prefix.
pub fn is_platform_url(base_url: &str, default: &DefaultLlm) -> bool {
    is_platform_url_within(base_url, default, &configured_platform_urls())
}

/// Whether `ep` is the platform's own model: the platform default itself, or a tenant provider whose base URL is
/// a platform URL ([`is_platform_url`]). Anything else is an external model, which a research run may use only
/// with the tenant's `external_llm_allowed` (migration 0023).
pub fn is_platform_model(ep: &LlmEndpoint, default: &DefaultLlm) -> bool {
    is_platform_model_within(ep, default, &configured_platform_urls())
}

fn is_platform_model_within(ep: &LlmEndpoint, default: &DefaultLlm, configured: &[String]) -> bool {
    ep.provider.is_none() || is_platform_url_within(&ep.base_url, default, configured)
}

/// The endpoint for `use_name` in `tenant`. `research_write` falls back to `research`, then to the platform default.
/// A bound provider that cannot be used (token undecryptable, host refused) is an error, never a silent fallback.
pub async fn resolve_endpoint(
    db: &PgPool,
    tenant: Uuid,
    use_name: &str,
    key: Option<&SourceKey>,
    allow_loopback: bool,
    default: &DefaultLlm,
    timeout: Duration,
) -> Result<LlmEndpoint> {
    type Bound = (Uuid, String, Option<String>, Option<String>, String);
    let rows: Vec<Bound> = {
        let mut tx = begin_scoped(db, tenant, None).await?;
        sqlx::query_as(
            "SELECT p.id, p.base_url, p.secret_enc, u.model, u.\"use\"
               FROM llm_provider_uses u
               JOIN llm_providers p ON p.tenant_id = u.tenant_id AND p.id = u.provider_id
              WHERE u.\"use\" IN ('research', 'research_write') AND p.kind = 'openai'",
        )
        .fetch_all(&mut *tx)
        .await?
    };
    let pick = |u: &str| rows.iter().find(|r| r.4 == u);
    let bound = if use_name == USE_RESEARCH_WRITE {
        pick(USE_RESEARCH_WRITE).or_else(|| pick(USE_RESEARCH))
    } else {
        pick(USE_RESEARCH)
    };
    let Some((id, base_url, secret_enc, model, _)) = bound else {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("building the model client")?;
        return Ok(LlmEndpoint {
            base_url: default.base_url.clone(),
            model: default.model.clone(),
            token: default.token.clone(),
            client,
            provider: None,
            effective_tenant: Some(tenant),
            actor: None,
        });
    };
    let token = match secret_enc {
        None => None,
        Some(enc) => {
            let key = key.context(
                "CAREBUS_SOURCE_KEY is not set, so the research provider's token cannot be used",
            )?;
            Some(open_token(key, *id, enc).map_err(|_| {
                anyhow::anyhow!("the research provider's token cannot be decrypted")
            })?)
        }
    };
    let (_, builder) = crate::netguard::check_and_pin(base_url, allow_loopback)
        .await
        .map_err(|r| {
            anyhow::anyhow!("the research provider's base_url is not callable: {}", r.0)
        })?;
    let client = builder
        .timeout(timeout)
        .build()
        .context("building the model client")?;
    Ok(LlmEndpoint {
        base_url: base_url.clone(),
        model: model
            .clone()
            .unwrap_or_else(|| carebus::interaction::DEFAULT_MODEL.to_string()),
        token,
        client,
        provider: Some(*id),
        // A tenant provider at a platform URL is still the platform model; any other is the tenant's own and
        // never gets the header.
        effective_tenant: is_platform_url(base_url, default).then_some(tenant),
        actor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_go_through_the_gateway() {
        let d = DefaultLlm::from_lookup(&|_| None);
        assert_eq!(d.base_url, "https://api.nonagentic.ai/vllm/v1");
        assert_eq!(d.model, "local-llm");
        assert!(d.token.is_none());
        let d = DefaultLlm::from_lookup(&|k| match k {
            "CAREBUS_LLM_BASE_URL" => Some("https://gw.example/v1".into()),
            "VLLM_LICENSE_TOKEN" => Some("lic".into()),
            "RESEARCH_LLM_MODEL" => Some("".into()),
            "CAREBUS_MODEL" => Some("m2".into()),
            _ => None,
        });
        assert_eq!(
            (d.base_url.as_str(), d.model.as_str(), d.token.as_deref()),
            ("https://gw.example/v1", "m2", Some("lic"))
        );
        let d = DefaultLlm::from_lookup(&|k| match k {
            "RESEARCH_LLM_BASE_URL" => Some("https://r.example/v1".into()),
            "CAREBUS_LLM_BASE_URL" => Some("https://gw.example/v1".into()),
            "RESEARCH_LLM_TOKEN" => Some("rt".into()),
            "VLLM_LICENSE_TOKEN" => Some("lic".into()),
            _ => None,
        });
        assert_eq!(d.base_url, "https://r.example/v1");
        assert_eq!(d.token.as_deref(), Some("rt"));
        assert!(!format!("{d:?}").contains("rt\""), "Debug hides the token");
    }

    fn ep(base_url: &str, provider: Option<Uuid>) -> LlmEndpoint {
        LlmEndpoint {
            base_url: base_url.into(),
            model: "m".into(),
            token: None,
            client: reqwest::Client::new(),
            provider,
            effective_tenant: None,
            actor: None,
        }
    }

    fn default_at(base_url: &str) -> DefaultLlm {
        DefaultLlm {
            base_url: base_url.into(),
            model: "local-llm".into(),
            token: None,
        }
    }

    const VLLM: &str = "http://vllm.example:8001/v1";
    const ROUTER: &str = "http://router.example:8150/api";

    fn platform(base_url: &str, default: &str) -> bool {
        is_platform_url_within(
            base_url,
            &default_at(default),
            &[VLLM.to_string(), ROUTER.to_string()],
        )
    }

    fn platform_model(base_url: &str, default: &str) -> bool {
        is_platform_model_within(
            &ep(base_url, Some(Uuid::new_v4())),
            &default_at(default),
            &[VLLM.to_string(), ROUTER.to_string()],
        )
    }

    #[test]
    fn platform_urls_cover_the_gateway_the_configured_endpoints_and_the_default() {
        // (base_url, default) -> platform?
        let table = [
            ("https://api.nonagentic.ai/vllm/v1", VLLM, true),
            ("http://vllm.example:8001/v1/", VLLM, true),
            (ROUTER, VLLM, true),
            ("https://api.openai.com/v1", VLLM, false),
            ("http://vllm.example:8001/v1", ROUTER, true),
            ("HTTP://ROUTER.EXAMPLE:8150/api/", ROUTER, true),
            ("https://api.openai.com/v1", ROUTER, false),
            // a deployment default outside the list is still platform; prefixes and other ports are not
            ("https://gw.example/v1", "https://gw.example/v1", true),
            ("http://vllm.example:8001/v1/x", VLLM, false),
            ("http://vllm.example:8002/v1", ROUTER, false),
            ("http://router.example:8150", ROUTER, false),
        ];
        for (url, default, want) in table {
            assert_eq!(platform(url, default), want, "{url} with default {default}");
            assert_eq!(
                platform_model(url, default),
                want,
                "bound provider {url} with default {default}"
            );
        }
    }

    fn chat_headers(ep: &LlmEndpoint) -> reqwest::header::HeaderMap {
        let req = ChatRequest {
            messages: vec![json!({"role": "user", "content": "q"})],
            tools: vec![],
            tool_choice: None,
        };
        chat_request(ep, &req).build().unwrap().headers().clone()
    }

    #[test]
    fn the_effective_tenant_header_goes_only_where_it_is_set() {
        let tenant = Uuid::new_v4();
        let mut e = ep(ROUTER, None);
        e.token = Some("lic".into());
        assert!(chat_headers(&e).get("x-effective-tenant-id").is_none());
        e.effective_tenant = Some(tenant);
        let h = chat_headers(&e);
        assert_eq!(
            h.get("X-Effective-Tenant-Id").and_then(|v| v.to_str().ok()),
            Some(tenant.to_string().as_str())
        );
        assert_eq!(
            h.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer lic")
        );
    }

    fn hv<'a>(h: &'a reqwest::header::HeaderMap, k: &str) -> Option<&'a str> {
        h.get(k).and_then(|v| v.to_str().ok())
    }

    /// Guards the router identity on the research model call: with an effective tenant (the platform model) the
    /// request carries `X-App-Id`, `X-Project-Id` (the tenant's tenant-level project) and `X-Actor-Id`; without one
    /// (a tenant's own provider) it carries none of them, whatever `actor` holds.
    #[test]
    fn the_platform_model_gets_care_bus_identity_and_a_tenant_provider_none() {
        let tenant = Uuid::parse_str("0195f48b-d000-7000-8000-000000000100").unwrap();
        let actor = Uuid::parse_str("7f1c9a2e-0000-4000-8000-00000000ac70").unwrap();
        let mut e = ep(ROUTER, None);
        e.effective_tenant = Some(tenant);
        e.actor = Some(actor);
        let h = chat_headers(&e);
        assert_eq!(hv(&h, "X-App-Id"), Some("care-bus"));
        assert_eq!(
            hv(&h, "X-Project-Id"),
            Some("00100000-0000-0008-0007-000db84f5910")
        );
        assert_eq!(hv(&h, "X-Actor-Id"), Some(actor.to_string().as_str()));
        assert_eq!(
            hv(&h, "X-Effective-Tenant-Id"),
            Some(tenant.to_string().as_str())
        );
        for k in [
            "x-app-id",
            "x-project-id",
            "x-actor-id",
            "x-effective-tenant-id",
        ] {
            assert_eq!(h.get_all(k).iter().count(), 1, "{k} sent once");
        }

        // no actor: the identity without X-Actor-Id
        e.actor = None;
        let h = chat_headers(&e);
        assert_eq!(hv(&h, "X-App-Id"), Some("care-bus"));
        assert!(h.get("x-actor-id").is_none());

        // a tenant's own provider (resolve_endpoint leaves effective_tenant unset): nothing of care-bus' identity
        let mut byo = ep("https://api.openai.com/v1", Some(Uuid::new_v4()));
        byo.actor = Some(actor);
        let h = chat_headers(&byo);
        for k in [
            "x-app-id",
            "x-project-id",
            "x-actor-id",
            "x-effective-tenant-id",
        ] {
            assert!(h.get(k).is_none(), "{k} must not reach a tenant provider");
        }
    }

    #[test]
    fn only_the_platform_model_counts_as_local() {
        let d = DefaultLlm {
            base_url: "http://vllm.example:8001/v1".into(),
            model: "local-llm".into(),
            token: None,
        };
        let tenant = Some(Uuid::new_v4());
        // unbound: the platform default, whatever its URL
        assert!(is_platform_model(
            &ep("https://anything.example/v1", None),
            &d
        ));
        // a tenant provider pointing at the platform gateway or the configured default
        assert!(is_platform_model(
            &ep("https://api.nonagentic.ai/vllm/v1/", tenant),
            &d
        ));
        assert!(is_platform_model(
            &ep("HTTP://VLLM.EXAMPLE:8001/v1", tenant),
            &d
        ));
        // anything else is external
        assert!(!is_platform_model(
            &ep("https://api.openai.com/v1", tenant),
            &d
        ));
        assert!(!is_platform_model(
            &ep("https://api.nonagentic.ai/vllm/v1/x", tenant),
            &d
        ));
    }

    #[test]
    fn reply_parsing() {
        let r = parse_reply(
            r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"x","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        )
        .unwrap();
        assert_eq!(r.usage.total_tokens, 15);
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert!(parse_reply("{}").is_err());
        assert!(parse_reply("nope").is_err());
    }
}
