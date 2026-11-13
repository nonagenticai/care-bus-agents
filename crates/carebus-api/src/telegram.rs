//! Telegram channel: the owner talks to the care-bus agents from a Telegram bot.
//!
//! The setup follows chat-bridge (`src/telegram.ts`, `src/index.ts`, `src/routes/telegram.ts`): plain Bot API calls
//! over HTTP, no library; `setWebhook` on boot with a `secret_token` and `allowed_updates=["message"]`; inbound updates
//! authenticated by the `X-Telegram-Bot-Api-Secret-Token` header; an allow-list of Telegram user ids. The flow is
//! care-bus's own: one Telegram [`Place`] (a chat, or one topic of a forum supergroup) is one care-bus conversation
//! (scoped Redis keys `telegram:chat:{place}` and the reverse `telegram:conv:{conversation_id}` -> `{place}`), and
//! every inbound text runs the same turn as `POST /api/jobs/{job}/conversations/{id}/messages`.
//!
//! Outbound: [`spawn_forwarder`] subscribes to the scope's chat channel and sends every `role=agent` event of a
//! Telegram-mapped conversation with `sendMessage`: the turn's own reply, a failed-turn notice, and the case results
//! the interaction bin relays later. The webhook path never sends a turn reply itself, so nothing is sent twice. A
//! `SET NX` claim per event (sha256 of the published payload) makes the send happen once even when several replicas
//! each run a forwarder.
//!
//! Off unless `TELEGRAM_ENABLED=true`; then the route is live and the forwarder runs. Disabled, the webhook route is
//! a 404 like any unknown route.

use std::time::Duration;

use anyhow::{Context, Result};
use carebus::chat::{self, ChatEvent};
use carebus::{Bus, Scope};
use futures_util::StreamExt;
use redis::AsyncCommands;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::routes::conversations;
use crate::state::AppState;

pub const DEFAULT_API_URL: &str = "https://api.telegram.org";

/// Telegram's `sendMessage` limit, in characters.
const MAX_TEXT_CHARS: usize = 4096;
/// Retries after the first attempt, for transport errors, 5xx and 429.
const RETRIES: u32 = 4;
/// Per-attempt timeout (overrides the shared client's 8 s).
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
/// Ceiling on Telegram's own `retry_after`: a flood-wait of minutes would park the forwarder for every chat.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// How long a forwarded event's claim is kept (dedupe across replicas and re-deliveries).
const SENT_CLAIM_SECS: u64 = 24 * 3600;
/// Delay before the forwarder resubscribes after its subscription failed or closed.
const RESUBSCRIBE: Duration = Duration::from_secs(5);

pub const GREETING: &str = "Hello! I am the care-bus assistant. Send me a message and I will help. Send /new to start a fresh conversation.";
pub const NEW_CONVERSATION: &str = "Started a new conversation. What can I help you with?";
pub const TEXT_ONLY: &str = "Sorry, I can only read text messages.";

/// `TELEGRAM_*`. Present in [`crate::config::Config`] only when `TELEGRAM_ENABLED=true`.
#[derive(Clone)]
pub struct TelegramConfig {
    /// `TELEGRAM_API_URL`, default `https://api.telegram.org`, no trailing slash.
    pub api_url: String,
    /// `TELEGRAM_BOT_TOKEN`. Secret: never logged, never in an error (it is part of every Bot API URL).
    pub bot_token: String,
    /// `TELEGRAM_WEBHOOK_SECRET`: Telegram echoes it in `X-Telegram-Bot-Api-Secret-Token`. Secret.
    pub webhook_secret: String,
    /// `TELEGRAM_WEBHOOK_URL`: the public https URL of `POST /api/telegram/webhook`, registered on boot.
    pub webhook_url: String,
    /// `TELEGRAM_ALLOWED_USER_IDS`: Telegram user ids allowed to talk to the bot. Empty = nobody.
    pub allowed_user_ids: Vec<i64>,
    /// `TELEGRAM_TENANT_ID` + `TELEGRAM_JOB`: the scope this bot serves.
    pub scope: Scope,
    /// `TELEGRAM_CUSTOMER_ID`: the demo customer the chat acts as (default: the console's demo customer).
    pub customer_id: String,
}

/// The token and the webhook secret are never printed.
impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("api_url", &self.api_url)
            .field("bot_token", &"<redacted>")
            .field("webhook_secret", &"<redacted>")
            .field("webhook_url", &self.webhook_url)
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("scope", &self.scope)
            .field("customer_id", &self.customer_id)
            .finish()
    }
}

impl TelegramConfig {
    /// `None` unless `TELEGRAM_ENABLED=true`. Enabled but incomplete or malformed is an error naming the variable,
    /// never its value, so startup fails.
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        if get("TELEGRAM_ENABLED").as_deref().map(str::trim) != Some("true") {
            return Ok(None);
        }
        let req = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .with_context(|| format!("TELEGRAM_ENABLED=true needs {k}"))
        };
        let bot_token = req("TELEGRAM_BOT_TOKEN")?;
        let webhook_secret = req("TELEGRAM_WEBHOOK_SECRET")?;
        // Telegram accepts 1-256 characters of A-Z, a-z, 0-9, `_` and `-`.
        if webhook_secret.len() > 256
            || !webhook_secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            anyhow::bail!(
                "TELEGRAM_WEBHOOK_SECRET must be 1-256 characters of A-Z, a-z, 0-9, _ and -"
            );
        }
        let webhook_url = req("TELEGRAM_WEBHOOK_URL")?;
        if !webhook_url.starts_with("https://") {
            anyhow::bail!(
                "TELEGRAM_WEBHOOK_URL must be an https URL (Telegram refuses anything else)"
            );
        }
        // Required to be SET, but may be empty: then nobody is allowed (fail closed).
        let raw_ids = get("TELEGRAM_ALLOWED_USER_IDS").context(
            "TELEGRAM_ENABLED=true needs TELEGRAM_ALLOWED_USER_IDS (empty refuses everyone)",
        )?;
        let allowed_user_ids = crate::config::split_list(&raw_ids)
            .iter()
            .map(|id| id.parse::<i64>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| {
                anyhow::anyhow!(
                    "TELEGRAM_ALLOWED_USER_IDS must be a comma list of numeric Telegram user ids"
                )
            })?;
        let tenant = req("TELEGRAM_TENANT_ID")?;
        let job = req("TELEGRAM_JOB")?;
        let scope = Scope::parse(&format!("{tenant}/{job}"))
            .context("TELEGRAM_TENANT_ID / TELEGRAM_JOB do not name a valid scope")?;
        let api_url = get("TELEGRAM_API_URL")
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_API_URL.into());
        let customer_id = get("TELEGRAM_CUSTOMER_ID")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| conversations::DEMO_CUSTOMER.into());
        Ok(Some(Self {
            api_url,
            bot_token,
            webhook_secret,
            webhook_url,
            allowed_user_ids,
            scope,
            customer_id,
        }))
    }

    /// Constant-time compare of the `X-Telegram-Bot-Api-Secret-Token` header with the configured secret.
    pub fn secret_matches(&self, presented: Option<&[u8]>) -> bool {
        use subtle::ConstantTimeEq;
        match presented {
            Some(p) => bool::from(p.ct_eq(self.webhook_secret.as_bytes())),
            None => false,
        }
    }
}

/// The parts of a Telegram `Update` this channel reads (the webhook asks for `message` updates only).
#[derive(Debug, Default, Deserialize)]
pub struct Update {
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub chat: Option<Chat>,
    #[serde(default)]
    pub from: Option<User>,
    /// The forum topic (or, outside forums, the replied-to thread) the message belongs to.
    #[serde(default)]
    pub message_thread_id: Option<i64>,
    /// True only for a message sent to a forum topic.
    #[serde(default)]
    pub is_topic_message: bool,
}

impl Message {
    /// The forum topic: `message_thread_id` counts only when `is_topic_message` is set. A plain reply in a non-forum
    /// group carries a `message_thread_id` too, and a forum's General topic has none; both are the chat itself.
    pub fn thread(&self) -> Option<i64> {
        self.message_thread_id.filter(|_| self.is_topic_message)
    }
}

/// Where a Telegram conversation lives: a chat, plus the forum topic in a forum supergroup. Written `{chat_id}` or
/// `{chat_id}:{thread}`, the form both Redis keys use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub chat_id: i64,
    pub thread: Option<i64>,
}

impl Place {
    /// Read back the reverse key's value; a bare chat id (the pre-topic format) is a place without a thread.
    pub fn parse(s: &str) -> Option<Self> {
        let (chat_id, thread): (i64, Option<i64>) = match s.split_once(':') {
            Some((chat, thread)) => (chat.parse().ok()?, Some(thread.parse().ok()?)),
            None => (s.parse().ok()?, None),
        };
        Some(Self { chat_id, thread })
    }
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.thread {
            Some(thread) => write!(f, "{}:{thread}", self.chat_id),
            None => write!(f, "{}", self.chat_id),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Default, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Debug, Default, Deserialize)]
struct TgResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<TgParameters>,
}

#[derive(Debug, Default, Deserialize)]
struct TgParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((400 * (u64::from(attempt) + 1)).min(3000))
}

/// Bot API client: the shared HTTP client plus the bot's config.
pub struct Telegram<'a> {
    http: &'a reqwest::Client,
    cfg: &'a TelegramConfig,
}

impl<'a> Telegram<'a> {
    pub fn new(http: &'a reqwest::Client, cfg: &'a TelegramConfig) -> Self {
        Self { http, cfg }
    }

    /// Call a Bot API method and return its `result`. Transport errors, 5xx and 429 are retried with a short
    /// backoff (429 waits Telegram's `retry_after`, capped). Errors never carry the URL: it contains the token.
    pub async fn call(&self, method: &str, body: &Value) -> Result<Value> {
        let url = format!("{}/bot{}/{method}", self.cfg.api_url, self.cfg.bot_token);
        let mut attempt = 0;
        loop {
            let sent = self
                .http
                .post(&url)
                .timeout(ATTEMPT_TIMEOUT)
                .json(body)
                .send()
                .await;
            let wait = match sent {
                Err(e) => {
                    let e = e.without_url();
                    if attempt >= RETRIES {
                        anyhow::bail!("telegram {method}: {e}");
                    }
                    tracing::warn!(method, attempt, error = %e, "telegram call failed; retrying");
                    backoff(attempt)
                }
                Ok(r) => {
                    let status = r.status();
                    let data: TgResponse = r.json().await.unwrap_or_default();
                    if status.is_success() && data.ok {
                        return Ok(data.result.unwrap_or(Value::Null));
                    }
                    let description = data.description.unwrap_or_default();
                    let transient = status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                    if !transient || attempt >= RETRIES {
                        anyhow::bail!("telegram {method} {status}: {description}");
                    }
                    let asked = data
                        .parameters
                        .and_then(|p| p.retry_after)
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| backoff(attempt));
                    tracing::warn!(method, %status, attempt, description = %description, "telegram call refused; retrying");
                    asked.min(MAX_RETRY_AFTER)
                }
            };
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// `sendMessage` to the place (with `message_thread_id` for a forum topic); the text is cut to Telegram's 4096
    /// characters.
    pub async fn send_message(&self, place: Place, text: &str) -> Result<()> {
        let text: String = text.chars().take(MAX_TEXT_CHARS).collect();
        let mut body = json!({"chat_id": place.chat_id, "text": text});
        if let Some(thread) = place.thread {
            body["message_thread_id"] = json!(thread);
        }
        self.call("sendMessage", &body).await.map(|_| ())
    }

    /// Point Telegram at this deployment: idempotent, called on every boot.
    pub async fn set_webhook(&self) -> Result<()> {
        self.call(
            "setWebhook",
            &json!({
                "url": self.cfg.webhook_url,
                "secret_token": self.cfg.webhook_secret,
                "allowed_updates": ["message"],
            }),
        )
        .await
        .map(|_| ())
    }
}

/// `telegram:chat:{chat_id}` or `telegram:chat:{chat_id}:{thread}` -> conversation id, in the bot's scope.
pub fn chat_key(bus: &Bus, place: Place) -> String {
    bus.streams.key(&format!("telegram:chat:{place}"))
}

/// `telegram:conv:{conversation_id}` -> the place (`{chat_id}` or `{chat_id}:{thread}`), in the bot's scope (the
/// forwarder's lookup).
pub fn conv_key(bus: &Bus, conversation_id: &str) -> String {
    bus.streams.key(&format!("telegram:conv:{conversation_id}"))
}

/// One forwarded agent event: claimed once per payload, so replicas and repeats never send it twice.
fn sent_key(bus: &Bus, raw: &str) -> String {
    let digest = hex::encode(Sha256::digest(raw.as_bytes()));
    bus.streams.key(&format!("telegram:sent:{digest}"))
}

/// Register the webhook on boot. A failure is logged, never fatal: the bot only stays deaf until the next boot.
pub fn spawn_register_webhook(st: AppState) -> Option<tokio::task::JoinHandle<()>> {
    st.cfg.telegram.as_ref()?;
    Some(tokio::spawn(async move {
        let Some(cfg) = st.cfg.telegram.as_ref() else {
            return;
        };
        match Telegram::new(&st.http, cfg).set_webhook().await {
            Ok(()) => tracing::info!(url = %cfg.webhook_url, "telegram webhook registered"),
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "telegram setWebhook failed; inbound messages will not arrive")
            }
        }
    }))
}

/// Handle one authenticated update. Runs in its own task: Telegram already has its 200.
pub async fn handle_update(st: AppState, update: Update) {
    let Some(cfg) = st.cfg.telegram.as_ref() else {
        return;
    };
    let Some(m) = update.message else { return };
    let thread = m.thread();
    let (Some(from), Some(chat)) = (m.from, m.chat) else {
        return;
    };
    if from.is_bot {
        return;
    }
    if !cfg.allowed_user_ids.contains(&from.id) {
        tracing::warn!(
            user = from.id,
            chat = chat.id,
            thread = ?thread,
            "telegram message from a user not on the allow-list; ignored"
        );
        return;
    }
    let place = Place {
        chat_id: chat.id,
        thread,
    };
    let tg = Telegram::new(&st.http, cfg);
    let Some(text) = m
        .text
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    else {
        reply(&tg, place, TEXT_ONLY).await;
        return;
    };
    let bus = st.bus.scoped(&cfg.scope);
    match command(&text) {
        Some("/start") => reply(&tg, place, GREETING).await,
        Some("/new") => match map_new_conversation(&bus, place, &cfg.customer_id).await {
            Ok(_) => reply(&tg, place, NEW_CONVERSATION).await,
            Err(e) => {
                tracing::warn!(place = %place, error = %format!("{e:#}"), "telegram /new failed")
            }
        },
        _ => {
            if let Err(e) = turn(&st, cfg, &bus, place, text).await {
                tracing::warn!(place = %place, error = %format!("{e:#}"), "telegram turn failed");
            }
        }
    }
}

/// A direct (non-turn) reply; a failure is logged.
async fn reply(tg: &Telegram<'_>, place: Place, text: &str) {
    if let Err(e) = tg.send_message(place, text).await {
        tracing::warn!(place = %place, error = %format!("{e:#}"), "telegram reply failed");
    }
}

/// `/new`, `/start` or `/new@my_bot`; `None` for anything that is not a command.
fn command(text: &str) -> Option<&str> {
    let first = text.split_whitespace().next()?;
    if !first.starts_with('/') {
        return None;
    }
    first.split('@').next()
}

/// The place's conversation, creating (and mapping) one when the place has none or its conversation expired.
async fn conversation_for(bus: &Bus, place: Place, customer: &str) -> Result<String> {
    let mut c = bus.conn();
    let mapped: Option<String> = c.get(chat_key(bus, place)).await?;
    if let Some(id) = mapped {
        let alive: bool = c.exists(chat::conv_key(bus, &id)).await?;
        if alive {
            refresh(bus, place, &id).await?;
            return Ok(id);
        }
    }
    let id = conversations::new_conversation(bus, customer)
        .await
        .map_err(|e| anyhow::anyhow!("creating a conversation: {e}"))?;
    // Two updates of one place may race here: the first mapping wins, the loser's conversation simply expires.
    let won: Option<String> = redis::cmd("SET")
        .arg(chat_key(bus, place))
        .arg(&id)
        .arg("NX")
        .arg("EX")
        .arg(chat::TTL_SECS)
        .query_async(&mut c)
        .await?;
    if won.is_some() {
        refresh(bus, place, &id).await?;
        return Ok(id);
    }
    let current: Option<String> = c.get(chat_key(bus, place)).await?;
    let current = current.context("the chat's conversation mapping vanished")?;
    refresh(bus, place, &current).await?;
    Ok(current)
}

/// `/new`: a fresh conversation replaces the place's mapping (other topics of the chat keep theirs). The old
/// conversation keeps its log until it expires; its reverse key is dropped, so its late case results no longer reach
/// the place.
async fn map_new_conversation(bus: &Bus, place: Place, customer: &str) -> Result<String> {
    let mut c = bus.conn();
    let old: Option<String> = c.get(chat_key(bus, place)).await?;
    let id = conversations::new_conversation(bus, customer)
        .await
        .map_err(|e| anyhow::anyhow!("creating a conversation: {e}"))?;
    let _: () = c.set_ex(chat_key(bus, place), &id, chat::TTL_SECS).await?;
    let _: () = c
        .set_ex(conv_key(bus, &id), place.to_string(), chat::TTL_SECS)
        .await?;
    if let Some(old) = old.filter(|o| *o != id) {
        let _: () = c.del(conv_key(bus, &old)).await?;
    }
    Ok(id)
}

/// Both directions of the mapping, with the conversation's TTL renewed.
async fn refresh(bus: &Bus, place: Place, id: &str) -> Result<()> {
    let _: () = redis::pipe()
        .set_ex(chat_key(bus, place), id, chat::TTL_SECS)
        .ignore()
        .set_ex(conv_key(bus, id), place.to_string(), chat::TTL_SECS)
        .ignore()
        .query_async(&mut bus.conn())
        .await?;
    Ok(())
}

/// The same turn as `POST /api/jobs/{job}/conversations/{id}/messages`. Its reply (or the failed-turn notice) is
/// published on the chat channel, and the forwarder sends it.
async fn turn(
    st: &AppState,
    cfg: &TelegramConfig,
    bus: &Bus,
    place: Place,
    text: String,
) -> Result<()> {
    let id = conversation_for(bus, place, &cfg.customer_id).await?;
    let conv = conversations::Conv::for_scope(st, &cfg.scope).await?;
    match conversations::submit(conv, id.clone(), text).await {
        Ok((status, body)) => {
            tracing::debug!(conversation = %id, %status, turn = %body.0["turn"], "telegram turn done")
        }
        // The drain already logged and published the failed-turn notice; the forwarder relays it.
        Err(e) => tracing::warn!(conversation = %id, error = ?e, "telegram turn failed"),
    }
    Ok(())
}

/// The forwarder for the bot's scope: `None` when Telegram is off. Resubscribes after 5 s when the subscription
/// fails or closes; events published while it is down are not replayed (Pub/Sub), but stay in the conversation log.
pub fn spawn_forwarder(st: AppState) -> Option<tokio::task::JoinHandle<()>> {
    let scope = st.cfg.telegram.as_ref()?.scope.clone();
    Some(tokio::spawn(async move {
        let bus = st.bus.scoped(&scope);
        loop {
            if let Err(e) = forward(&st, &bus).await {
                tracing::warn!(scope = %scope, error = %format!("{e:#}"), "telegram forwarder stopped; resubscribing in 5 s");
            }
            tokio::time::sleep(RESUBSCRIBE).await;
        }
    }))
}

async fn forward(st: &AppState, bus: &Bus) -> Result<()> {
    let channel = chat::chat_channel(bus);
    let mut ps = bus.client().get_async_pubsub().await?;
    ps.subscribe(&channel).await?;
    tracing::info!(channel = %channel, "telegram forwarder subscribed");
    let mut messages = ps.into_on_message();
    while let Some(m) = messages.next().await {
        let raw: String = match m.get_payload() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "unreadable chat event; skipped");
                continue;
            }
        };
        // One at a time: keeps a chat's messages in order (the bot has one rate budget anyway).
        if let Err(e) = forward_one(st, bus, &raw).await {
            tracing::warn!(error = %format!("{e:#}"), "forwarding a chat event to telegram failed");
        }
    }
    anyhow::bail!("the chat subscription closed")
}

/// Send one published chat event to Telegram when it is an agent message of a Telegram-mapped conversation.
pub async fn forward_one(st: &AppState, bus: &Bus, raw: &str) -> Result<()> {
    let Some(cfg) = st.cfg.telegram.as_ref() else {
        return Ok(());
    };
    let ev: ChatEvent = serde_json::from_str(raw).context("parsing a chat event")?;
    if ev.role != "agent" {
        return Ok(());
    }
    let mut c = bus.conn();
    let raw_place: Option<String> = c.get(conv_key(bus, &ev.conversation_id)).await?;
    let Some(raw_place) = raw_place else {
        return Ok(()); // a console conversation
    };
    let place = Place::parse(&raw_place)
        .with_context(|| format!("unreadable telegram place {raw_place:?}"))?;
    let claimed: Option<String> = redis::cmd("SET")
        .arg(sent_key(bus, raw))
        .arg(1)
        .arg("NX")
        .arg("EX")
        .arg(SENT_CLAIM_SECS)
        .query_async(&mut c)
        .await?;
    if claimed.is_none() {
        return Ok(()); // another forwarder already sent it
    }
    Telegram::new(&st.http, cfg)
        .send_message(place, &ev.text)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(k: &str) -> Option<String> {
        match k {
            "TELEGRAM_ENABLED" => Some("true"),
            "TELEGRAM_BOT_TOKEN" => Some("123:abc"),
            "TELEGRAM_WEBHOOK_SECRET" => Some("s3cret_value-1"),
            "TELEGRAM_WEBHOOK_URL" => Some("https://bot.example.test/api/telegram/webhook"),
            "TELEGRAM_ALLOWED_USER_IDS" => Some("42, 7"),
            "TELEGRAM_TENANT_ID" => Some("0195f48b-d000-7000-8000-000000000099"),
            "TELEGRAM_JOB" => Some("customer_care_agent"),
            _ => None,
        }
        .map(String::from)
    }

    fn without(skip: &'static str) -> impl Fn(&str) -> Option<String> {
        move |k: &str| if k == skip { None } else { base(k) }
    }

    #[test]
    fn off_unless_enabled() {
        assert!(
            TelegramConfig::from_lookup(&|_: &str| None::<String>)
                .unwrap()
                .is_none()
        );
        let off = |k: &str| {
            if k == "TELEGRAM_ENABLED" {
                Some("false".to_string())
            } else {
                base(k)
            }
        };
        assert!(TelegramConfig::from_lookup(&off).unwrap().is_none());
    }

    #[test]
    fn enabled_reads_everything_with_defaults() {
        let c = TelegramConfig::from_lookup(&base).unwrap().unwrap();
        assert_eq!(c.api_url, DEFAULT_API_URL);
        assert_eq!(c.allowed_user_ids, vec![42, 7]);
        assert_eq!(c.scope.job, "customer_care_agent");
        assert_eq!(c.customer_id, conversations::DEMO_CUSTOMER);
        let printed = format!("{c:?}");
        assert!(
            !printed.contains("123:abc") && !printed.contains("s3cret"),
            "{printed}"
        );
        assert!(c.secret_matches(Some(b"s3cret_value-1")));
        assert!(!c.secret_matches(Some(b"s3cret_value-2")));
        assert!(!c.secret_matches(Some(b"s3cret")));
        assert!(!c.secret_matches(None));
    }

    #[test]
    fn enabled_but_incomplete_fails_naming_the_variable() {
        for k in [
            "TELEGRAM_BOT_TOKEN",
            "TELEGRAM_WEBHOOK_SECRET",
            "TELEGRAM_WEBHOOK_URL",
            "TELEGRAM_ALLOWED_USER_IDS",
            "TELEGRAM_TENANT_ID",
            "TELEGRAM_JOB",
        ] {
            let err = TelegramConfig::from_lookup(&without(k))
                .unwrap_err()
                .to_string();
            assert!(err.contains(k), "{k}: {err}");
        }
        let empty_ids = |k: &str| {
            if k == "TELEGRAM_ALLOWED_USER_IDS" {
                Some(String::new())
            } else {
                base(k)
            }
        };
        let c = TelegramConfig::from_lookup(&empty_ids).unwrap().unwrap();
        assert!(c.allowed_user_ids.is_empty(), "set but empty = nobody");
        let bad_ids = |k: &str| {
            if k == "TELEGRAM_ALLOWED_USER_IDS" {
                Some("42,george".to_string())
            } else {
                base(k)
            }
        };
        assert!(TelegramConfig::from_lookup(&bad_ids).is_err());
        let http = |k: &str| {
            if k == "TELEGRAM_WEBHOOK_URL" {
                Some("http://bot.example.test/hook".to_string())
            } else {
                base(k)
            }
        };
        assert!(TelegramConfig::from_lookup(&http).is_err());
    }

    #[test]
    fn commands_strip_the_bot_suffix() {
        assert_eq!(command("/new"), Some("/new"));
        assert_eq!(command("/start@care_bus_bot hi"), Some("/start"));
        assert_eq!(command("hello /new"), None);
    }

    #[test]
    fn a_place_round_trips_and_reads_a_bare_chat_id() {
        let chat = Place {
            chat_id: -1001234567890,
            thread: None,
        };
        let topic = Place {
            chat_id: -1001234567890,
            thread: Some(7),
        };
        assert_eq!(chat.to_string(), "-1001234567890");
        assert_eq!(topic.to_string(), "-1001234567890:7");
        assert_eq!(Place::parse(&chat.to_string()), Some(chat));
        assert_eq!(Place::parse(&topic.to_string()), Some(topic));
        assert_eq!(
            Place::parse("4242"),
            Some(Place {
                chat_id: 4242,
                thread: None
            })
        );
        for bad in ["", "abc", "1:", ":7", "1:x", "1:2:3"] {
            assert_eq!(Place::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_a_topic_message_has_a_thread() {
        let m = |thread: Option<i64>, topic: bool| Message {
            message_thread_id: thread,
            is_topic_message: topic,
            ..Message::default()
        };
        assert_eq!(m(Some(7), true).thread(), Some(7));
        assert_eq!(m(Some(7), false).thread(), None, "a plain reply in a group");
        assert_eq!(m(None, false).thread(), None);
        let parsed: Message = serde_json::from_value(json!({
            "chat": {"id": -1001234567890_i64, "type": "supergroup", "is_forum": true},
            "message_thread_id": 9,
            "is_topic_message": true,
        }))
        .unwrap();
        assert_eq!(parsed.thread(), Some(9));
    }
}
