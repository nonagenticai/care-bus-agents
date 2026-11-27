//! Web channel: a job-registered, shared-secret door for an external front end (chat-bridge).
//!
//! `PUT /api/jobs/{job}/channels/webchat` (JobAdmin) mints a key and a secret and stores only the
//! secret's SHA-256. The public routes under `/api/channel/webchat/{key}` authenticate with
//! `x-carebus-webchat-secret`, resolve the job from that registration (no JWT), and reuse the
//! conversation primitives the Telegram channel uses. The message route answers 202 at once — the
//! qualifier can take minutes — and the answer arrives on the events route, which replays the
//! conversation log from a cursor and long-polls for new entries.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use carebus::chat::{self, ChatEvent};
use carebus::{Bus, Scope};
use redis::AsyncCommands;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{
    audit,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    state::AppState,
    tenancy::JobAdmin,
};

/// The bearer secret's header. One header for every public web-channel route.
pub const SECRET_HEADER: &str = "x-carebus-webchat-secret";
/// Cap on a long-poll wait; a caller reconnects with the cursor it already has.
const WAIT_MAX_MS: u64 = 25_000;
const LABEL_MAX: usize = 120;
const IP_MAX: usize = 64;
const USER_AGENT_MAX: usize = 512;
const INTEL_MAX: usize = 4_000;

/// Job-scoped: the channel registration and rotation.
pub fn job_routes() -> Router<AppState> {
    Router::new().route(
        "/channels/webchat",
        axum::routing::put(register).delete(unregister),
    )
}

/// Public: the external front end's door. Absolute paths, merged outside `/api/jobs/{job}`.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/api/channel/webchat/{channel}/messages", post(messages))
        .route(
            "/api/channel/webchat/{channel}/conversations/{id}/events",
            get(events),
        )
        .route(
            "/api/channel/webchat/{channel}/conversations/{id}/state",
            get(state),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterInput {
    #[serde(default)]
    label: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ChannelRow {
    tenant_id: Uuid,
    job_key: String,
    secret_hash: String,
    enabled: bool,
}

#[derive(Deserialize)]
struct ChannelPath {
    channel: String,
}

#[derive(Deserialize)]
struct ConvPath {
    channel: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageInput {
    session_key: String,
    #[serde(default)]
    conversation_id: Option<String>,
    text: String,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    user_agent: Option<String>,
    #[serde(default)]
    intel: Option<String>,
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: Option<usize>,
    #[serde(default)]
    wait_ms: Option<u64>,
}

fn sha256_hex(value: &str) -> String {
    sha256_hex_bytes(value.as_bytes())
}

fn sha256_hex_bytes(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

/// 32 random bytes as 64 hex characters: two v4 UUIDs carry 244 random bits.
fn new_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Mint (or rotate) the job's channel. The secret is returned once and never stored in clear.
async fn register(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(input): ApiJson<RegisterInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let label = input.label.unwrap_or_default();
    let label = label.trim();
    if label.chars().count() > LABEL_MAX {
        return Err(ApiError::BadRequest(
            "invalid_body",
            format!("label must be at most {LABEL_MAX} characters."),
        ));
    }
    let key = format!("wc_{}", Uuid::new_v4().simple());
    let secret = new_secret();
    let secret_hash = sha256_hex(&secret);
    let mut tx = ctx.begin(&s.db).await?;
    let key: String = sqlx::query_scalar(
        "INSERT INTO web_channels (job_id, key, secret_hash, label)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (tenant_id, job_id) DO UPDATE
            SET secret_hash = EXCLUDED.secret_hash,
                label = CASE WHEN EXCLUDED.label = '' THEN web_channels.label ELSE EXCLUDED.label END,
                enabled = true,
                updated_at = now()
         RETURNING key",
    )
    .bind(ctx.job_id)
    .bind(&key)
    .bind(&secret_hash)
    .bind(label)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "update",
        "web_channel",
        &key,
        true,
        json!({"enabled": true}),
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "key": key,
            "secret": secret,
            "note": "the secret is shown once; store it now.",
        })),
    ))
}

async fn unregister(JobAdmin(ctx): JobAdmin, State(s): State<AppState>) -> ApiResult<StatusCode> {
    let mut tx = ctx.begin(&s.db).await?;
    let key: Option<String> = sqlx::query_scalar("SELECT key FROM web_channels WHERE job_id = $1")
        .bind(ctx.job_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(key) = key else {
        return Err(ApiError::NotFound("web channel".into()));
    };
    sqlx::query("DELETE FROM web_channels WHERE job_id = $1")
        .bind(ctx.job_id)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "delete",
        "web_channel",
        &key,
        true,
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The scope and scoped bus behind a channel key + secret. A missing, disabled or mismatched
/// channel is the same 401, so a caller cannot tell "wrong key" from "wrong secret".
async fn authenticate(s: &AppState, channel: &str, headers: &HeaderMap) -> ApiResult<(Scope, Bus)> {
    let row: Option<ChannelRow> = sqlx::query_as(
        "SELECT tenant_id, job_key, secret_hash, enabled FROM web_channel_lookup($1)",
    )
    .bind(channel)
    .fetch_optional(&s.db)
    .await?;
    let Some(row) = row.filter(|r| r.enabled) else {
        return Err(ApiError::Unauthorized);
    };
    let Some(presented) = headers.get(SECRET_HEADER).map(|v| v.as_bytes()) else {
        return Err(ApiError::Unauthorized);
    };
    // The stored value is SHA-256 hex, so the presented secret is hashed first; both sides are
    // exactly 64 characters, and the compare stays constant-time.
    let presented = sha256_hex_bytes(presented);
    if presented.len() != row.secret_hash.len()
        || !bool::from(presented.as_bytes().ct_eq(row.secret_hash.as_bytes()))
    {
        return Err(ApiError::Unauthorized);
    }
    let scope = Scope::parse(&format!("{}/{}", row.tenant_id.hyphenated(), row.job_key))
        .map_err(ApiError::Internal)?;
    let bus = s.bus.scoped(&scope);
    Ok((scope, bus))
}

async fn exists(bus: &Bus, id: &str) -> ApiResult<()> {
    let alive: bool = bus.conn().exists(chat::conv_key(bus, id)).await?;
    if alive {
        Ok(())
    } else {
        Err(ApiError::NotFound(format!("conversation {id}")))
    }
}

/// One conversation per `session_key`, race-safe across replicas (Telegram's `SET NX` shape). The
/// customer id is a pseudonym of the session, never the external front end's own identity.
async fn conversation_for(bus: &Bus, session_key: &str) -> ApiResult<String> {
    let digest = sha256_hex(session_key);
    let customer = format!("web:{}", &digest[..32]);
    let map = bus.streams.key(&format!("webchat:session:{digest}"));
    let mut c = bus.conn();
    let mapped: Option<String> = c.get(&map).await?;
    if let Some(id) = mapped {
        let alive: bool = c.exists(chat::conv_key(bus, &id)).await?;
        if alive {
            let _: () = c.expire(&map, chat::TTL_SECS as i64).await?;
            return Ok(id);
        }
    }
    let id = super::conversations::new_conversation(bus, &customer).await?;
    let won: Option<String> = redis::cmd("SET")
        .arg(&map)
        .arg(&id)
        .arg("NX")
        .arg("EX")
        .arg(chat::TTL_SECS)
        .query_async(&mut c)
        .await?;
    if won.is_some() {
        return Ok(id);
    }
    let current: Option<String> = c.get(&map).await?;
    current
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("the webchat session mapping vanished")))
}

/// Visitor facts the answer and the handover header can use. They live beside the conversation, not
/// in its transcript.
async fn remember(bus: &Bus, id: &str, session_key: &str, m: &MessageInput) -> ApiResult<()> {
    let conv = chat::conv_key(bus, id);
    let mut fields: Vec<(&str, String)> = vec![("web_session", sha256_hex(session_key))];
    for (name, value, max) in [
        ("web_ip", m.ip.as_deref(), IP_MAX),
        ("web_user_agent", m.user_agent.as_deref(), USER_AGENT_MAX),
        ("web_intel", m.intel.as_deref(), INTEL_MAX),
    ] {
        let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            continue;
        };
        if value.len() > max {
            return Err(ApiError::BadRequest(
                "invalid_body",
                format!("{name} must be at most {max} bytes."),
            ));
        }
        fields.push((name, value.to_string()));
    }
    let _: () = redis::pipe()
        .hset_multiple(&conv, &fields)
        .ignore()
        .expire(&conv, chat::TTL_SECS as i64)
        .ignore()
        .query_async(&mut bus.conn())
        .await?;
    Ok(())
}

async fn messages(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiPath(p): ApiPath<ChannelPath>,
    ApiJson(m): ApiJson<MessageInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (scope, bus) = authenticate(&s, &p.channel, &headers).await?;
    let text = m.text.trim();
    if text.is_empty() {
        return Err(ApiError::BadRequest(
            "empty_message",
            "Type a message first.".into(),
        ));
    }
    let session_key = m.session_key.trim();
    if session_key.is_empty() || session_key.len() > 200 {
        return Err(ApiError::BadRequest(
            "invalid_session_key",
            "session_key must be 1 to 200 characters.".into(),
        ));
    }
    let id = match m.conversation_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            exists(&bus, id).await?;
            id.to_string()
        }
        _ => conversation_for(&bus, session_key).await?,
    };
    remember(&bus, &id, session_key, &m).await?;
    let conv = super::conversations::Conv::for_scope(&s, &scope)
        .await
        .map_err(ApiError::Internal)?;
    let (turn_id, turn_text) = (id.clone(), text.to_string());
    // The qualifier (and then the pipeline) runs in its own task: the front end gets its 202 at
    // once and reads the answer on the events route, exactly like Telegram's webhook.
    tokio::spawn(async move {
        if let Err(e) = super::conversations::submit(conv, turn_id.clone(), turn_text).await {
            tracing::warn!(conversation = %turn_id, error = ?e, "webchat turn failed");
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({"conversation_id": id}))))
}

fn cursor_expired(after: usize) -> ApiError {
    ApiError::Detailed(
        StatusCode::GONE,
        "cursor_expired",
        "the conversation log no longer holds that cursor.".into(),
        json!({"cursor": after}),
    )
}

async fn events(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiPath(p): ApiPath<ConvPath>,
    ApiQuery(q): ApiQuery<EventsQuery>,
) -> ApiResult<Json<Value>> {
    let (_scope, bus) = authenticate(&s, &p.channel, &headers).await?;
    exists(&bus, &p.id).await?;
    let log = chat::log_key(&bus, &p.id);
    let after = q.after.unwrap_or(0);
    let wait = q.wait_ms.unwrap_or(0).min(WAIT_MAX_MS);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(wait);
    let mut c = bus.conn();
    let raw: Vec<String> = loop {
        let len: usize = c.llen(&log).await?;
        if after > len {
            return Err(cursor_expired(after));
        }
        if len > after {
            break c.lrange(&log, after as isize, -1).await?;
        }
        if wait == 0 || tokio::time::Instant::now() >= deadline {
            return Ok(Json(json!({"events": [], "cursor": after})));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let mut events = Vec::with_capacity(raw.len());
    for (i, raw) in raw.iter().enumerate() {
        match serde_json::from_str::<ChatEvent>(raw) {
            Ok(ev) => events.push(json!({
                "seq": after + i,
                "role": ev.role,
                "text": ev.text,
                "case_id": ev.case_id,
                "at_ms": ev.at_ms,
            })),
            Err(e) => tracing::warn!(error = %e, "skipping an unreadable chat event"),
        }
    }
    let cursor = after + raw.len();
    Ok(Json(json!({"events": events, "cursor": cursor})))
}

async fn state(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiPath(p): ApiPath<ConvPath>,
) -> ApiResult<Json<Value>> {
    let (_scope, bus) = authenticate(&s, &p.channel, &headers).await?;
    exists(&bus, &p.id).await?;
    let conv = chat::conv_key(&bus, &p.id);
    let mut c = bus.conn();
    let customer: Option<String> = c.hget(&conv, "customer_id").await?;
    let draft: Option<String> = c.hget(&conv, "draft").await?;
    let locked: bool = c.exists(chat::lock_key(&bus, &p.id)).await?;
    let cases: Vec<String> = c.lrange(chat::cases_key(&bus, &p.id), 0, -1).await?;
    Ok(Json(json!({
        "conversation_id": p.id,
        "customer_id": customer,
        "has_draft": draft.as_deref().is_some_and(|d| d != "null"),
        "locked": locked,
        "cases": cases,
    })))
}
