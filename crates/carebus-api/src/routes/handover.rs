//! Internal handover between the website chat and Telegram.
//!
//! `POST /api/internal/handover` (service token) opens a forum topic in the support supergroup
//! for one conversation, records the `(chat_id, thread_id) -> conversation` mapping, posts the
//! opening note and takes the long human-mode lock so the agent stops answering.
//! `POST /api/internal/handover/reply` accepts an operator's message for a topic: it writes the
//! text to the conversation log as a `human` event (the web channel replays it to the visitor)
//! and supports `/auto` and `/close`. The web channel's own door
//! (`POST /api/channel/webchat/{key}/handover/reply`, authenticated by the channel secret) is the
//! same reply, with the caller's scope checked against the topic's.
//!
//! The bot is the existing @nonagenticaibot (one bot, one webhook: chat-bridge owns the webhook
//! and relays an unknown topic's message here), so nothing here registers a second bot.

use axum::{Json, Router, extract::State, routing::post};
use redis::AsyncCommands;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use carebus::Scope;
use carebus::chat::{self, ChatEvent};

use crate::{
    auth::{Caller, CallerKind},
    error::{ApiError, ApiJson, ApiResult},
    handover::{HandoverConfig, ReplyInput, conv_topic_key, topic_key},
    state::AppState,
};

/// The human-mode lock outlives a per-turn lock: an operator may take an hour to answer.
const HUMAN_LOCK_SECS: u64 = 24 * 3600;
const OPENING_MAX: usize = 3_500;

pub fn internal_routes() -> Router<AppState> {
    Router::new()
        .route("/api/internal/handover", post(open))
        .route("/api/internal/handover/reply", post(reply))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenInput {
    tenant_id: Uuid,
    job: String,
    conversation_id: String,
    /// The visitor's own words, for the topic's opening note.
    reason: String,
}

fn service_only(caller: &Caller) -> ApiResult<()> {
    if matches!(caller.kind, CallerKind::Service) {
        Ok(())
    } else {
        Err(ApiError::Forbidden("service token"))
    }
}

/// The handover door's configuration; a deployment without it refuses with a named 503.
fn cfg(s: &AppState) -> ApiResult<&HandoverConfig> {
    s.cfg
        .handover
        .as_ref()
        .ok_or(ApiError::NotConfigured("handover_not_configured"))
}

/// One Bot API call. The token is never logged; a Telegram refusal is a 500 with its description.
async fn telegram(
    s: &AppState,
    cfg: &HandoverConfig,
    method: &str,
    body: Value,
) -> ApiResult<Value> {
    let url = format!("{}/bot{}/{method}", cfg.api_url, cfg.bot_token);
    let resp = s
        .http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("telegram {method}: {e}")))?;
    let status = resp.status();
    let payload: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() || payload.get("ok").and_then(Value::as_bool) != Some(true) {
        let why = payload
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("no description");
        return Err(ApiError::Internal(anyhow::anyhow!(
            "telegram {method} refused: {why}"
        )));
    }
    Ok(payload.get("result").cloned().unwrap_or(Value::Null))
}

async fn open(
    caller: Caller,
    State(s): State<AppState>,
    ApiJson(i): ApiJson<OpenInput>,
) -> ApiResult<Json<Value>> {
    service_only(&caller)?;
    let cfg = cfg(&s)?;
    let job = i.job.trim();
    let conv = i.conversation_id.trim();
    if i.tenant_id.is_nil()
        || job.is_empty()
        || job.len() > 63
        || conv.is_empty()
        || conv.len() > 64
    {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "tenant_id, job and conversation_id are required.".into(),
        ));
    }
    let scope = Scope::parse(&format!("{}/{}", i.tenant_id.hyphenated(), job))
        .map_err(ApiError::Internal)?;
    let bus = s.bus.scoped(&scope);
    let alive: bool = bus
        .conn()
        .exists(chat::conv_key(&bus, conv))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    if !alive {
        return Err(ApiError::NotFound(format!("conversation {conv}")));
    }
    let chat_id = cfg.chat_id;
    let name = format!("chat {}", &conv[..conv.len().min(12)]);
    let topic = telegram(
        &s,
        cfg,
        "createForumTopic",
        json!({"chat_id": chat_id, "name": name}),
    )
    .await?;
    let thread = topic
        .get("message_thread_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("createForumTopic: no thread id")))?;
    let mapping = json!({"tenant_id": i.tenant_id, "job": job, "conversation_id": conv});
    let reverse = json!({"chat_id": chat_id, "thread_id": thread});
    let mut c = bus.conn();
    let _: () = c
        .set_ex(
            topic_key(chat_id, thread),
            mapping.to_string(),
            chat::TTL_SECS,
        )
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    // The reverse mapping: a visitor's next message goes into its topic while a person holds the chat.
    let _: () = c
        .set_ex(
            conv_topic_key(&bus, conv),
            reverse.to_string(),
            chat::TTL_SECS,
        )
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    // Visitor facts live beside the conversation, never in the transcript.
    let conv_key = chat::conv_key(&bus, conv);
    let intel: Option<String> = c.hget(&conv_key, "web_intel").await.unwrap_or_default();
    let ip: Option<String> = c.hget(&conv_key, "web_ip").await.unwrap_or_default();
    let mut note = format!("Website chat {conv}\n{}", i.reason.trim());
    if let Some(ip) = ip.filter(|v| !v.is_empty()) {
        note.push_str(&format!("\nIP: {ip}"));
    }
    if let Some(intel) = intel.filter(|v| !v.is_empty()) {
        note.push_str(&format!("\nIntel: {intel}"));
    }
    let note: String = note.chars().take(OPENING_MAX).collect();
    telegram(
        &s,
        cfg,
        "sendMessage",
        json!({"chat_id": chat_id, "message_thread_id": thread, "text": note}),
    )
    .await?;
    // Human mode: the per-turn lock holds a foreign token with a long TTL, so the agent leaves
    // the conversation alone until /auto or /close removes the key.
    let _: () = redis::cmd("SET")
        .arg(chat::lock_key(&bus, conv))
        .arg(Uuid::new_v4().to_string())
        .arg("EX")
        .arg(HUMAN_LOCK_SECS)
        .query_async(&mut c)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    Ok(Json(
        json!({"status": "handed_over", "chat_id": chat_id, "thread_id": thread}),
    ))
}

/// The inbound half: one operator message for a topic. `expected` is the caller's scope when the caller is a
/// channel (the web door); a topic of another tenant or job is then a 404, exactly as an unknown topic is.
pub async fn apply_reply(
    s: &AppState,
    expected: Option<&Scope>,
    i: &ReplyInput,
) -> ApiResult<Json<Value>> {
    let cfg = cfg(s)?;
    if !cfg.operator_ids.contains(&i.from_id) {
        return Err(ApiError::Forbidden("the operator"));
    }
    let text = i.text.trim();
    if text.is_empty() || text.len() > 4_000 {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "text must be 1 to 4000 bytes.".into(),
        ));
    }
    // The mapping is global (chat_id, thread_id): find the bus from the key itself.
    let mut admin = s.bus.conn();
    let raw: Option<String> = admin
        .get(topic_key(i.chat_id, i.thread_id))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    let Some(raw) = raw else {
        return Err(ApiError::NotFound("handover topic".into()));
    };
    let m: Value = serde_json::from_str(&raw)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("handover mapping: {e}")))?;
    let tenant = m
        .get("tenant_id")
        .and_then(Value::as_str)
        .and_then(|v| Uuid::parse_str(v).ok())
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("handover mapping: tenant")))?;
    let job = m
        .get("job")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("handover mapping: job")))?;
    let conv = m
        .get("conversation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("handover mapping: conversation")))?;
    if let Some(expected) = expected
        && (expected.tenant != tenant || expected.job != job)
    {
        return Err(ApiError::NotFound("handover topic".into()));
    }
    let scope =
        Scope::parse(&format!("{}/{}", tenant.hyphenated(), job)).map_err(ApiError::Internal)?;
    let bus = s.bus.scoped(&scope);
    let command = text.split_whitespace().next().unwrap_or_default();
    let (say, release) = match command {
        "/auto" => (
            "I'm handing this back to the assistant — you can keep talking here.".to_string(),
            true,
        ),
        "/close" => (
            "George has closed this conversation. Thank you for your message.".to_string(),
            true,
        ),
        _ => (text.to_string(), false),
    };
    chat::append_and_publish(&bus, &ChatEvent::new(conv, "human", &say, None))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    let _: i64 = admin
        .expire(topic_key(i.chat_id, i.thread_id), chat::TTL_SECS as i64)
        .await
        .unwrap_or(0);
    let _: i64 = admin
        .expire(conv_topic_key(&bus, conv), chat::TTL_SECS as i64)
        .await
        .unwrap_or(0);
    if release {
        let _: i64 = admin
            .del(chat::lock_key(&bus, conv))
            .await
            .unwrap_or_default();
        let _: i64 = admin
            .del(conv_topic_key(&bus, conv))
            .await
            .unwrap_or_default();
    } else {
        let _: i64 = admin
            .expire(chat::lock_key(&bus, conv), HUMAN_LOCK_SECS as i64)
            .await
            .unwrap_or(0);
    }
    Ok(Json(
        json!({"ok": true, "conversation_id": conv, "released": release}),
    ))
}

async fn reply(
    caller: Caller,
    State(s): State<AppState>,
    ApiJson(i): ApiJson<ReplyInput>,
) -> ApiResult<Json<Value>> {
    service_only(&caller)?;
    apply_reply(&s, None, &i).await
}

/// Post a visitor's message into the conversation's topic while a person holds it. A missing mapping or a Telegram
/// refusal is logged, never returned: the visitor's message is already in the conversation log.
pub async fn forward_to_topic(s: &AppState, bus: &carebus::Bus, conversation_id: &str, text: &str) {
    let Some(cfg) = s.cfg.handover.as_ref() else {
        return;
    };
    let mut c = bus.conn();
    let raw: Option<String> = match c.get(conv_topic_key(bus, conversation_id)).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(conversation = %conversation_id, error = %e, "handover topic lookup failed");
            return;
        }
    };
    let Some(raw) = raw else {
        return;
    };
    let place: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(conversation = %conversation_id, error = %e, "handover topic mapping unreadable");
            return;
        }
    };
    let (Some(chat_id), Some(thread_id)) = (
        place.get("chat_id").and_then(Value::as_i64),
        place.get("thread_id").and_then(Value::as_i64),
    ) else {
        tracing::warn!(conversation = %conversation_id, "handover topic mapping incomplete");
        return;
    };
    let text: String = text.chars().take(OPENING_MAX).collect();
    if let Err(e) = telegram(
        s,
        cfg,
        "sendMessage",
        json!({"chat_id": chat_id, "message_thread_id": thread_id, "text": text}),
    )
    .await
    {
        tracing::warn!(conversation = %conversation_id, error = %e, "forwarding a visitor message to its topic failed");
        return;
    }
    let _: i64 = c
        .expire(conv_topic_key(bus, conversation_id), chat::TTL_SECS as i64)
        .await
        .unwrap_or(0);
}
