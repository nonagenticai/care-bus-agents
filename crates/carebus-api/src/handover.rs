//! Handover to a person: the Telegram bot that opens one forum topic per handed-over conversation, and the
//! mappings that reach that topic in both directions.
//!
//! `HANDOVER_*` (with the `TELEGRAM_SIGNUP_*` fallbacks the route has always read) name the bot, the support
//! supergroup and the operators the inbound route admits. `HANDOVER_API_URL` exists so tests can point the Bot API
//! at a stub; it is never set in a deployment.

use anyhow::{Context, Result};
use carebus::Bus;

pub const DEFAULT_API_URL: &str = "https://api.telegram.org";
/// The operator a handover admits when `HANDOVER_OPERATOR_IDS` is unset.
pub const DEFAULT_OPERATOR_ID: i64 = 7619636015;

/// `HANDOVER_*`: the Bot API door and its allow-list.
#[derive(Clone)]
pub struct HandoverConfig {
    /// `HANDOVER_API_URL`, default `https://api.telegram.org`, no trailing slash.
    pub api_url: String,
    /// `HANDOVER_BOT_TOKEN` / `TELEGRAM_SIGNUP_BOT_TOKEN`. Secret: never logged, never in an error (it is part of
    /// every Bot API URL).
    pub bot_token: String,
    /// `HANDOVER_CHAT_ID` / `TELEGRAM_SIGNUP_CHAT_ID`: the support supergroup (a forum, one topic per handover).
    pub chat_id: i64,
    /// `HANDOVER_OPERATOR_IDS`: Telegram user ids whose messages the reply route accepts. Empty = nobody.
    pub operator_ids: Vec<i64>,
}

/// The token is never printed.
impl std::fmt::Debug for HandoverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandoverConfig")
            .field("api_url", &self.api_url)
            .field("bot_token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .field("operator_ids", &self.operator_ids)
            .finish()
    }
}

impl HandoverConfig {
    /// `None` unless a bot token is configured (handover is off). The CHAT ID alone — care-bus sets
    /// `TELEGRAM_SIGNUP_CHAT_ID` for its signup notice, whose token secret is optional — never enables it. A
    /// token without any chat id is a misconfiguration and is an error naming the variable, never its value.
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let raw = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let Some(bot_token) =
            raw("HANDOVER_BOT_TOKEN").or_else(|| raw("TELEGRAM_SIGNUP_BOT_TOKEN"))
        else {
            return Ok(None);
        };
        let chat_id = raw("HANDOVER_CHAT_ID")
            .or_else(|| raw("TELEGRAM_SIGNUP_CHAT_ID"))
            .context(
                "handover needs HANDOVER_CHAT_ID (or TELEGRAM_SIGNUP_CHAT_ID) with its bot token",
            )?;
        let chat_id = chat_id
            .parse::<i64>()
            .context("HANDOVER_CHAT_ID must be a Telegram chat id")?;
        let operator_ids = match raw("HANDOVER_OPERATOR_IDS") {
            None => vec![DEFAULT_OPERATOR_ID],
            Some(raw) => raw
                .split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(|id| {
                    id.parse::<i64>().with_context(
                        || "HANDOVER_OPERATOR_IDS must be a comma list of numeric ids",
                    )
                })
                .collect::<Result<Vec<_>>>()?,
        };
        let api_url = raw("HANDOVER_API_URL").unwrap_or_else(|| DEFAULT_API_URL.into());
        Ok(Some(Self {
            api_url,
            bot_token,
            chat_id,
            operator_ids,
        }))
    }
}

/// One operator reply as the Bot API webhook delivers it: which topic it answers, who typed it, and what.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyInput {
    pub chat_id: i64,
    pub thread_id: i64,
    pub from_id: i64,
    pub text: String,
}

/// `handover:topic:{chat}:{thread}` -> `{tenant_id, job, conversation_id}`. Global on purpose: the reply route
/// finds the scope from the mapping itself, whoever delivers the message.
pub fn topic_key(chat: i64, thread: i64) -> String {
    format!("handover:topic:{chat}:{thread}")
}

/// In the conversation's scope: `handover:conv:{conversation}` -> `{chat_id, thread_id}`. The webchat channel
/// reads it to post a visitor's message into the topic while a person holds the conversation.
pub fn conv_topic_key(bus: &Bus, conversation_id: &str) -> String {
    bus.streams.key(&format!("handover:conv:{conversation_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn off_when_nothing_is_configured() {
        assert!(HandoverConfig::from_lookup(&|_| None).unwrap().is_none());
    }

    #[test]
    fn reads_the_operator_list_and_the_default_operator() {
        let c = HandoverConfig::from_lookup(&lookup(&[
            ("HANDOVER_BOT_TOKEN", "123:abc"),
            ("HANDOVER_CHAT_ID", "-1001234567890"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.chat_id, -1001234567890);
        assert_eq!(c.operator_ids, vec![DEFAULT_OPERATOR_ID]);
        assert_eq!(c.api_url, DEFAULT_API_URL);
        let named = HandoverConfig::from_lookup(&lookup(&[
            ("HANDOVER_BOT_TOKEN", "123:abc"),
            ("HANDOVER_CHAT_ID", "-1001234567890"),
            ("HANDOVER_OPERATOR_IDS", "7, 9"),
            ("HANDOVER_API_URL", "http://127.0.0.1:9/"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(named.operator_ids, vec![7, 9]);
        assert!(!format!("{named:?}").contains("123:abc"));
    }

    #[test]
    fn one_half_configured_fails_naming_the_variable() {
        // a token without a chat id is a misconfiguration
        let err = HandoverConfig::from_lookup(&lookup(&[("HANDOVER_BOT_TOKEN", "123:abc")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("HANDOVER_CHAT_ID"), "{err}");
        // a chat id alone is the signup notice's setting, not a handover
        assert!(
            HandoverConfig::from_lookup(&lookup(&[("TELEGRAM_SIGNUP_CHAT_ID", "-1")]))
                .unwrap()
                .is_none()
        );
        assert!(
            HandoverConfig::from_lookup(&lookup(&[
                ("HANDOVER_BOT_TOKEN", "1:x"),
                ("HANDOVER_CHAT_ID", "not-a-number"),
            ]))
            .is_err()
        );
    }

    #[test]
    fn the_signup_fallbacks_still_work() {
        let c = HandoverConfig::from_lookup(&lookup(&[
            ("TELEGRAM_SIGNUP_BOT_TOKEN", "1:x"),
            ("TELEGRAM_SIGNUP_CHAT_ID", "-5"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!((c.chat_id, c.bot_token.as_str()), (-5, "1:x"));
    }
}
