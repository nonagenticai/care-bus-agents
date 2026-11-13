//! Telegram notices for early-access signups (contract C1; agent `signup/telegram.rs`), sent with the
//! @nonagenticaibot token (`TELEGRAM_SIGNUP_BOT_TOKEN`) into care-bus's forum topic of the owner's signup chat.
//!
//! ⛔ SEND ONLY: this module calls `sendMessage` and nothing else. chat-bridge owns that bot's updates and relays an
//! Approve/Reject tap to `POST /api/signup-approvals/{id}/telegram-decision` (C2/C3). A test greps the signup code
//! for the update methods. care-bus's OWN bot (@care_bus_agent_bot, crate::telegram, `TELEGRAM_*`) is a different
//! bot that does register a webhook; the two share no token, no config and no code.
//!
//! ⛔ FIRE-AND-FORGET: a notice is sent from a detached task with a bounded retry; a Telegram outage never fails an
//! intake or a decision. The bot token is part of the URL, so no transport error is ever formatted into a log.
//!
//! Requester-typed values are forced onto one line and rendered inside `code` entities (offsets in UTF-16 units),
//! so Telegram cannot auto-link them and a newline cannot forge a line; there is no `parse_mode`.
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::signup::config::{SignupConfig, SignupTelegram};
use crate::signup::single_line;

const MAX_ATTEMPTS: usize = 3;
const BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
pub const MESSAGE_EXCERPT_MAX: usize = 300;
const FIELD_MAX: usize = 200;

/// C1 callback data: `ea1:<key>:a|r:<signupId>` (at most 64 bytes for a uuid and a 16-character key).
pub fn callback_data(key: &str, id: Uuid, approve: bool) -> String {
    format!("ea1:{key}:{}:{id}", if approve { "a" } else { "r" })
}

/// The super_admin onboarding page for one signup.
pub fn deep_link(public_url: &str, id: Uuid) -> String {
    format!("{public_url}/admin/onboarding?signup={id}")
}

pub fn excerpt(v: &str, max: usize) -> String {
    let flat = single_line(v);
    if flat.chars().count() > max {
        let mut s: String = flat.chars().take(max).collect();
        s.push('…');
        s
    } else {
        flat
    }
}

#[derive(Default)]
struct Message {
    text: String,
    utf16: usize,
    entities: Vec<Value>,
}

impl Message {
    fn plain(&mut self, s: &str) -> &mut Self {
        self.text.push_str(s);
        self.utf16 += s.encode_utf16().count();
        self
    }

    fn field(&mut self, label: &str, value: Option<&str>, max: usize) -> &mut Self {
        let v = excerpt(value.unwrap_or_default(), max);
        self.plain(&format!("{label}: "));
        if v.is_empty() {
            return self.plain("—\n");
        }
        let len = v.encode_utf16().count();
        self.entities
            .push(json!({"type": "code", "offset": self.utf16, "length": len}));
        self.plain(&v);
        self.plain("\n")
    }

    fn payload(&self, t: &SignupTelegram) -> Value {
        json!({
            "chat_id": t.chat_id,
            "message_thread_id": t.thread_id,
            "text": self.text.trim_end_matches('\n'),
            "entities": self.entities,
            "link_preview_options": {"is_disabled": true},
        })
    }
}

pub struct NewSignup<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub email: &'a str,
    pub company: Option<&'a str>,
    pub message: Option<&'a str>,
    pub source: &'a str,
    pub locale: Option<&'a str>,
    pub pending: i64,
}

pub struct Decided<'a> {
    pub id: Uuid,
    pub approved: bool,
    pub name: &'a str,
    pub email: &'a str,
    pub via: &'a str,
    pub tenant: Option<(Uuid, &'a str)>,
    pub not_ok: &'a [(String, String, Option<String>)],
    pub email_note: Option<&'a str>,
    /// A reject's repair line (codes only).
    pub repair: Option<&'a str>,
}

pub const NEW_SIGNUP_HEADLINE: &str = "New care-bus early-access signup";

pub fn new_signup_payload(cfg: &SignupConfig, n: &NewSignup<'_>) -> Option<Value> {
    let t = cfg.telegram.as_ref()?;
    let mut m = Message::default();
    m.plain(&format!("{NEW_SIGNUP_HEADLINE}\n\n"))
        .field("Name", Some(n.name), FIELD_MAX)
        .field("Email", Some(n.email), FIELD_MAX)
        .field("Company", n.company, FIELD_MAX)
        .plain(&format!("Source: {}\n", n.source))
        .plain(&format!("Language: {}\n", n.locale.unwrap_or("— (en)")))
        .plain("Trusted via: website/chat intake (service account)\n");
    if let Some(msg) = n.message.filter(|m| !single_line(m).is_empty()) {
        m.field("Message", Some(msg), MESSAGE_EXCERPT_MAX);
    }
    m.plain(&format!("Pending signups: {}\n", n.pending))
        .plain(&format!("Signup id: {}\n\n", n.id))
        .plain(
            "Approve creates the workspace now (suggested name) and emails the set-password link once it is \
             ready. Open in care-bus to choose a name first.",
        );
    let mut p = m.payload(t);
    let mut rows = vec![json!([
        {"text": "Approve", "callback_data": callback_data(&cfg.telegram_key, n.id, true)},
        {"text": "Reject", "callback_data": callback_data(&cfg.telegram_key, n.id, false)},
    ])];
    if let Some(url) = cfg.public_url.as_deref() {
        rows.push(json!([{"text": "Open in care-bus", "url": deep_link(url, n.id)}]));
    }
    p["reply_markup"] = json!({"inline_keyboard": rows});
    Some(p)
}

pub fn decision_payload(cfg: &SignupConfig, d: &Decided<'_>) -> Option<Value> {
    let t = cfg.telegram.as_ref()?;
    let mut m = Message::default();
    m.plain(&format!(
        "care-bus signup {}\n\n",
        if d.approved { "approved" } else { "rejected" }
    ))
    .field("Name", Some(d.name), FIELD_MAX)
    .field("Email", Some(d.email), FIELD_MAX)
    .plain(&format!("Decided via: {}\n", d.via));
    if d.approved {
        if let Some((id, name)) = d.tenant {
            m.field("Workspace", Some(name), FIELD_MAX)
                .plain(&format!("Tenant id: {id}\n"));
        }
        if d.not_ok.is_empty() {
            m.plain(&format!(
                "Provisioning: all steps ok; {}.\n",
                d.email_note.unwrap_or("email step done")
            ));
        } else {
            m.plain("Provisioning steps NOT ok:\n");
            for (step, status, code) in d.not_ok {
                let code = code
                    .as_deref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default();
                let next = if status == "failed" {
                    "use Retry on the onboarding page"
                } else {
                    "retried automatically"
                };
                m.plain(&format!("  - {step}: {status}{code} - {next}\n"));
            }
        }
    } else if let Some(r) = d.repair {
        m.plain(&format!("Cleanup: {r}\n"));
    }
    m.plain(&format!("Signup id: {}", d.id));
    Some(m.payload(t))
}

/// Send `payload` from a detached task. Never returns an error; no-op while Telegram is off.
pub fn send(
    cfg: &SignupConfig,
    http: &reqwest::Client,
    payload: Option<Value>,
    kind: &'static str,
) {
    let (Some(t), Some(payload)) = (cfg.telegram.as_ref(), payload) else {
        return;
    };
    let url = format!("{}/bot{}/sendMessage", t.api_base, t.bot_token);
    let http = http.clone();
    tokio::spawn(async move {
        deliver(&http, &url, &payload, kind).await;
    });
}

async fn deliver(http: &reqwest::Client, url: &str, payload: &Value, kind: &'static str) -> bool {
    for attempt in 0..MAX_ATTEMPTS {
        let mut wait = None;
        match http
            .post(url)
            .json(payload)
            .timeout(SEND_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                tracing::info!(kind, outcome = "ok", "signup_telegram_send");
                return true;
            }
            Ok(r) if r.status() == StatusCode::TOO_MANY_REQUESTS => {
                let body: Option<Value> = r.json().await.ok();
                wait = body
                    .as_ref()
                    .and_then(|b| b["parameters"]["retry_after"].as_u64())
                    .map(|s| Duration::from_secs(s).min(MAX_RETRY_AFTER));
            }
            Ok(r) if r.status().is_client_error() => {
                tracing::warn!(
                    kind,
                    status = r.status().as_u16(),
                    outcome = "failed",
                    "signup_telegram_send refused; not retried (check chat/thread id and bot membership)"
                );
                return false;
            }
            Ok(r) => {
                tracing::warn!(
                    kind,
                    status = r.status().as_u16(),
                    attempt,
                    "signup_telegram_send failed"
                );
            }
            // The error text carries the URL, which carries the bot token: never format it.
            Err(e) => {
                tracing::warn!(
                    kind,
                    timeout = e.is_timeout(),
                    attempt,
                    "signup_telegram_send failed"
                );
            }
        }
        let Some(backoff) = BACKOFF.get(attempt) else {
            break;
        };
        tokio::time::sleep(wait.unwrap_or(*backoff)).await;
    }
    tracing::warn!(kind, outcome = "failed", "signup_telegram_send gave up");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_data_is_c1_and_fits_telegrams_64_bytes() {
        let id = Uuid::parse_str("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b").unwrap();
        let a = callback_data("carebus", id, true);
        assert_eq!(a, "ea1:carebus:a:6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b");
        assert_eq!(
            callback_data("carebus", id, false),
            "ea1:carebus:r:6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b"
        );
        assert!(a.len() <= 64);
        assert!(callback_data("abcdefghijklmnop", id, true).len() <= 64);
    }

    #[test]
    fn entities_wrap_requester_text_with_utf16_offsets() {
        let mut m = Message::default();
        m.plain("Ü😀\n")
            .field("Name", Some("Ann\nEvil.example/x"), 200);
        let e = &m.entities[0];
        assert_eq!(e["offset"], 10);
        assert_eq!(e["length"], "Ann Evil.example/x".encode_utf16().count());
        assert!(m.text.contains("Name: Ann Evil.example/x\n"), "{}", m.text);
        assert!(excerpt(&"x".repeat(301), 300).ends_with('…'));
    }

    /// Send-only (C1): the early-access signup code never claims @nonagenticaibot's updates. Scans the signup module,
    /// its routes and its integration tests (crate::telegram, care-bus's own bot, legitimately registers a webhook and
    /// is out of scope). The method names are assembled at runtime so this test does not match itself.
    #[test]
    fn the_signup_code_never_calls_the_update_methods() {
        let forbidden = [
            ["set", "webhook"].concat(),
            ["delete", "webhook"].concat(),
            ["get", "updates"].concat(),
            ["get", "webhookinfo"].concat(),
        ];
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        let mut stack = vec![root.join("src/signup")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    files.push(p);
                }
            }
        }
        files.push(root.join("src/routes/signups.rs"));
        files.push(root.join("tests/it/signups.rs"));
        for p in &files {
            let text = std::fs::read_to_string(p).unwrap().to_lowercase();
            for f in &forbidden {
                assert!(!text.contains(f.as_str()), "{} names {f}", p.display());
            }
        }
        for must in [
            "src/signup/telegram.rs",
            "src/signup/provisioning.rs",
            "src/routes/signups.rs",
        ] {
            assert!(files.contains(&root.join(must)), "the scan missed {must}");
        }
        // The signup bot token never shares a name with care-bus's own bot.
        let cfg = std::fs::read_to_string(root.join("src/signup/config.rs")).unwrap();
        assert!(cfg.contains("\"TELEGRAM_SIGNUP_BOT_TOKEN\""));
        assert!(!cfg.contains("trimmed(\"TELEGRAM_BOT_TOKEN\")"));
        let probe = format!("POST /bot<t>/{}", ["set", "Webhook"].concat()).to_lowercase();
        assert!(forbidden.iter().any(|f| probe.contains(f.as_str())));
    }
}
