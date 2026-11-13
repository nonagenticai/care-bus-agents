//! Conversation keys shared by carebus-api (writes customer turns) and the interaction bin (relays replies).

use anyhow::Result;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};

use crate::Bus;

pub fn chat_channel(bus: &Bus) -> String {
    bus.streams.key("chat")
}
pub fn conv_key(bus: &Bus, id: &str) -> String {
    bus.streams.key(&format!("chat:{id}"))
}
pub fn log_key(bus: &Bus, id: &str) -> String {
    bus.streams.key(&format!("chat:{id}:log"))
}
pub fn lock_key(bus: &Bus, id: &str) -> String {
    bus.streams.key(&format!("chat:{id}:lock"))
}
/// Customer texts accepted but not yet qualified. The lock holder drains it whole and qualifies the joined text.
pub fn inbox_key(bus: &Bus, id: &str) -> String {
    bus.streams.key(&format!("chat:{id}:inbox"))
}
pub fn cases_key(bus: &Bus, id: &str) -> String {
    bus.streams.key(&format!("chat:{id}:cases"))
}
/// Links a case to the conversation that opened it. One definition, used by the recorder too.
pub fn case_conv_key(bus: &Bus, case_id: &str) -> String {
    bus.streams.key(&format!("case_conv:{case_id}"))
}

pub const TTL_SECS: u64 = 7 * 24 * 3600;
/// Conversation lock lifetime: the qualifier's worst case plus slack for Redis and Postgres writes.
/// Covers the qualifier's whole budget plus the request gate's (one laya call, spec 2026-10-18 §3) and slack.
pub const LOCK_SECS: u64 =
    crate::interaction::QUALIFIER_BUDGET_SECS + crate::gates::GATE_BUDGET_SECS + 20;
const _: () =
    assert!(LOCK_SECS > crate::interaction::QUALIFIER_BUDGET_SECS + crate::gates::GATE_BUDGET_SECS);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChatEvent {
    pub conversation_id: String,
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_id: Option<String>,
    pub at_ms: u64,
}

impl ChatEvent {
    pub fn new(conversation_id: &str, role: &str, text: &str, case_id: Option<String>) -> Self {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            conversation_id: conversation_id.into(),
            role: role.into(),
            text: text.into(),
            case_id,
            at_ms,
        }
    }
}

pub async fn append_and_publish(bus: &Bus, ev: &ChatEvent) -> Result<()> {
    let raw = serde_json::to_string(ev)?;
    let mut c = bus.conn();
    let log = log_key(bus, &ev.conversation_id);
    let _: () = c.rpush(&log, &raw).await?;
    let _: () = c.expire(&log, TTL_SECS as i64).await?;
    let _: i64 = c.publish(chat_channel(bus), &raw).await?;
    Ok(())
}

/// Accept one customer message: log + publish it (the transcript shows it at once) and queue its text for the
/// next turn, in one MULTI so a logged message is always queued and a queued one always logged.
pub async fn accept_customer(bus: &Bus, ev: &ChatEvent) -> Result<()> {
    let raw = serde_json::to_string(ev)?;
    let log = log_key(bus, &ev.conversation_id);
    let inbox = inbox_key(bus, &ev.conversation_id);
    let _: () = redis::pipe()
        .atomic()
        .rpush(&log, &raw)
        .ignore()
        .expire(&log, TTL_SECS as i64)
        .ignore()
        .rpush(&inbox, &ev.text)
        .ignore()
        .expire(&inbox, TTL_SECS as i64)
        .ignore()
        .publish(chat_channel(bus), &raw)
        .ignore()
        .query_async(&mut bus.conn())
        .await?;
    Ok(())
}

/// Atomically take every queued text (oldest first) and empty the inbox.
pub async fn take_inbox(bus: &Bus, conversation_id: &str) -> Result<Vec<String>> {
    const SCRIPT: &str =
        "local v = redis.call('LRANGE', KEYS[1], 0, -1); redis.call('DEL', KEYS[1]); return v";
    let v: Vec<String> = redis::Script::new(SCRIPT)
        .key(inbox_key(bus, conversation_id))
        .invoke_async(&mut bus.conn())
        .await?;
    Ok(v)
}

/// What [`finish_turn`] did with the lock.
#[derive(Debug, PartialEq, Eq)]
pub enum Finish {
    /// Inbox empty: the lock is released.
    Released,
    /// Texts arrived meanwhile: the lock is kept (TTL refreshed) and the holder must drain again.
    Pending,
    /// The lock is no longer ours (expired, maybe re-taken); nothing was changed.
    Lost,
}

/// End of one drain: release the lock only if the inbox is empty, else keep it with a fresh TTL. One script, so
/// a text pushed by a sender whose acquire failed is either seen here (Pending) or lands after the release, when
/// that sender's own acquire succeeds. No text can be stranded behind a lock that is about to go away.
pub async fn finish_turn(bus: &Bus, conversation_id: &str, token: &str) -> Result<Finish> {
    const SCRIPT: &str = "if redis.call('GET', KEYS[1]) ~= ARGV[1] then return -1 end \
        if redis.call('LLEN', KEYS[2]) > 0 then redis.call('EXPIRE', KEYS[1], ARGV[2]); return 1 end \
        redis.call('DEL', KEYS[1]); return 0";
    let n: i64 = redis::Script::new(SCRIPT)
        .key(lock_key(bus, conversation_id))
        .key(inbox_key(bus, conversation_id))
        .arg(token)
        .arg(LOCK_SECS)
        .invoke_async(&mut bus.conn())
        .await?;
    Ok(match n {
        1 => Finish::Pending,
        0 => Finish::Released,
        _ => Finish::Lost,
    })
}

/// Conversations with queued texts and no lock holder (the holder died, or its lock expired), at most `limit`.
/// SCANs `chat:*:inbox` under the bus prefix; glob characters in the prefix are escaped.
pub async fn stranded_inboxes(bus: &Bus, limit: usize) -> Result<Vec<String>> {
    let head = bus.streams.key("chat:");
    let escaped: String = head
        .chars()
        .flat_map(|c| match c {
            '*' | '?' | '[' | ']' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    let pattern = format!("{escaped}*:inbox");
    let mut c = bus.conn();
    let mut cursor: u64 = 0;
    let mut out = Vec::new();
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(200)
            .query_async(&mut c)
            .await?;
        for k in keys {
            let Some(id) = k.strip_prefix(&head).and_then(|r| r.strip_suffix(":inbox")) else {
                continue;
            };
            let held: bool = c.exists(lock_key(bus, id)).await?;
            if !held && out.len() < limit && !out.iter().any(|o: &String| o == id) {
                out.push(id.to_string());
            }
        }
        cursor = next;
        if cursor == 0 || out.len() >= limit {
            return Ok(out);
        }
    }
}

/// Called by the interaction bin for every composed reply. Cases from the REPL have no conversation.
pub async fn relay_reply(bus: &Bus, case_id: &str, text: &str) -> Result<bool> {
    let conv: Option<String> = bus.conn().get(case_conv_key(bus, case_id)).await?;
    let Some(conv) = conv else { return Ok(false) };
    append_and_publish(
        bus,
        &ChatEvent::new(&conv, "agent", text, Some(case_id.into())),
    )
    .await?;
    Ok(true)
}

/// Idempotent: one MULTI removes any earlier copy of `case_id` before appending it, so a retry never
/// lists a case twice.
pub async fn link_case(bus: &Bus, conversation_id: &str, case_id: &str) -> Result<()> {
    let ck = cases_key(bus, conversation_id);
    let _: () = redis::pipe()
        .atomic()
        .set_ex(case_conv_key(bus, case_id), conversation_id, TTL_SECS)
        .ignore()
        .lrem(&ck, 0, case_id)
        .ignore()
        .rpush(&ck, case_id)
        .ignore()
        .expire(&ck, TTL_SECS as i64)
        .ignore()
        .query_async(&mut bus.conn())
        .await?;
    Ok(())
}

/// Take the per-conversation lock. `Some(token)` when acquired, `None` when another message holds it.
pub async fn acquire_lock(bus: &Bus, conversation_id: &str) -> Result<Option<String>> {
    let token = uuid::Uuid::new_v4().to_string();
    let got: Option<String> = redis::cmd("SET")
        .arg(lock_key(bus, conversation_id))
        .arg(&token)
        .arg("NX")
        .arg("EX")
        .arg(LOCK_SECS)
        .query_async(&mut bus.conn())
        .await?;
    Ok(got.map(|_| token))
}

/// Compare-and-delete: only the holder's token releases the lock, so a lock that expired and was taken
/// by a later message is never deleted by the slow first one.
pub async fn release_lock(bus: &Bus, conversation_id: &str, token: &str) -> Result<bool> {
    const SCRIPT: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then return redis.call('DEL', KEYS[1]) else return 0 end";
    let n: i64 = redis::Script::new(SCRIPT)
        .key(lock_key(bus, conversation_id))
        .arg(token)
        .invoke_async(&mut bus.conn())
        .await?;
    Ok(n == 1)
}

/// Run mode of the `interaction` bin. Pure, so it is unit-tested without touching the process env.
#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Daemon,
    Repl,
}

pub fn mode_from(args: &[String], daemon_env: Option<&str>) -> Mode {
    let env_on = daemon_env.map(str::trim).is_some_and(|v| {
        ["1", "true", "yes"]
            .iter()
            .any(|on| on.eq_ignore_ascii_case(v))
    });
    if env_on || args.iter().any(|a| a == "--daemon") {
        Mode::Daemon
    } else {
        Mode::Repl
    }
}

/// Deliver one composed reply to the conversation linked to `case_id`. The link is written right after
/// the case is published, so retry for up to 2 s before giving up.
/// Returns false for cases that have no conversation (REPL cases).
pub async fn deliver_reply(bus: &Bus, case_id: &str, text: &str) -> Result<bool> {
    for _ in 0..10 {
        if relay_reply(bus, case_id, text).await? {
            return Ok(true);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_mode_from_flag_or_env() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            mode_from(&a(&["interaction", "--daemon"]), None),
            Mode::Daemon
        );
        assert_eq!(mode_from(&a(&["interaction"]), Some("1")), Mode::Daemon);
        assert_eq!(mode_from(&a(&["interaction"]), Some("0")), Mode::Repl);
        assert_eq!(mode_from(&a(&["interaction"]), None), Mode::Repl);
    }

    #[test]
    fn daemon_env_is_case_insensitive() {
        let a = vec!["interaction".to_string()];
        for on in ["TRUE", "True", "Yes", "YES", " 1 ", "true"] {
            assert_eq!(mode_from(&a, Some(on)), Mode::Daemon, "{on}");
        }
        for off in ["0", "false", "No", "", "maybe"] {
            assert_eq!(mode_from(&a, Some(off)), Mode::Repl, "{off}");
        }
    }
}
