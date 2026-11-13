//! Laya gates as the bus agents see them (spec 2026-10-18 §3). carebus-api owns every gate: the questions, the laya
//! provider and its token, the firing rules and the verdict log. The worker (point `tool`) and the interaction daemon
//! (point `response`) ask it through `POST {CAREBUS_API_URL}/api/internal/gates/evaluate` with the service token
//! (`CAREBUS_API_TOKEN`); carebus-api's own chat turn (point `request`) plugs in a [`RequestGate`] instead.
//!
//! Gates are on unless switched off explicitly: `CAREBUS_GATES=off` (local runs without an API) disables them with a
//! WARN; otherwise `CAREBUS_API_URL` and `CAREBUS_API_TOKEN` are required and a missing one stops the agent at
//! startup. Tests construct [`GateClient::new`] (or pass `None`) directly.
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::QualifiedRequest;
use crate::interaction::Draft;
use crate::scope::Scope;

/// carebus-api's timeout for the one laya call of a gate point.
pub const LAYA_TIMEOUT_MS: u64 = 1500;
/// The whole gate budget a caller allows: the laya call plus the database reads/writes around it and transport.
/// Covered by the conversation lock (`chat::LOCK_SECS`).
pub const GATE_BUDGET_SECS: u64 = 3;
/// The error of a tool step a gate stopped (non-retryable, so the case resolves to a human handoff).
pub const GATE_BLOCKED: &str = "gate_blocked";
/// What the customer reads when a stopping question has no message of its own.
pub const DEFAULT_HANDOFF: &str =
    "I'm handing this over to a colleague, who will get back to you shortly.";

/// carebus-api's answer for one gate point (`POST /api/internal/gates/evaluate`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// `pass`, `fired`, `failed_open`, `failed_closed` or `skipped`.
    pub outcome: String,
    /// The strongest action of the fired questions (`handoff` > `block` > `annotate`); `handoff` when failed closed.
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub fired_keys: Vec<String>,
    #[serde(default)]
    pub answers: Option<Value>,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// Whether the point has an enabled fail-closed question (a caller's transport error then means closed).
    #[serde(default)]
    pub has_closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Verdict {
    /// The caller could not reach carebus-api: treated as closed (tool and response gates are safety gates).
    pub fn failed_closed(error: impl Into<String>) -> Self {
        Self {
            outcome: "failed_closed".into(),
            action: Some("handoff".into()),
            error: Some(error.into()),
            has_closed: true,
            ..Self::default()
        }
    }

    /// Fired with `handoff`/`block`, or failed closed: the guarded step must not happen.
    pub fn stops(&self) -> bool {
        match self.outcome.as_str() {
            "failed_closed" => true,
            "fired" => matches!(self.action.as_deref(), Some("handoff" | "block")),
            _ => false,
        }
    }

    /// The text the customer reads instead: the question's message, else [`DEFAULT_HANDOFF`].
    pub fn text(&self) -> String {
        self.message
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(DEFAULT_HANDOFF)
            .to_string()
    }

    /// Why a tool step was stopped, for the step's error (`gate_blocked: …`).
    pub fn reason(&self) -> String {
        if self.outcome == "failed_closed" {
            format!("{GATE_BLOCKED}: gates unavailable")
        } else {
            format!("{GATE_BLOCKED}: {}", self.fired_keys.join(", "))
        }
    }
}

/// What the customer chat may search (carebus-api `POST /api/internal/faq/scope`, crate::walls there): unrestricted
/// when the tenant has no walled store, else exactly `leaf_ids` (empty: nothing).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FaqScope {
    pub restricted: bool,
    #[serde(default)]
    pub leaf_ids: Vec<String>,
}

/// The internal evaluate body.
#[derive(Serialize)]
struct EvalBody<'a> {
    tenant_id: uuid::Uuid,
    job: &'a str,
    point: &'a str,
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    case_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'a str>,
}

/// carebus-api's internal gate route.
#[derive(Clone)]
pub struct GateClient {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

/// Never prints the service token.
impl std::fmt::Debug for GateClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateClient")
            .field("base_url", &self.base_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl GateClient {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(GATE_BUDGET_SECS))
                .build()
                .expect("reqwest client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        }
    }

    /// [`Self::from_vars`] over the process environment.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// `CAREBUS_GATES=off`: `None`, gates disabled (logged as a WARN). Otherwise `CAREBUS_API_URL` and
    /// `CAREBUS_API_TOKEN` are both required (unset or empty is an error naming the variable): gates are never off
    /// because a variable is missing.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        if get("CAREBUS_GATES").is_some_and(|v| v.trim().eq_ignore_ascii_case("off")) {
            tracing::warn!("CAREBUS_GATES=off: Laya gates are disabled for this agent");
            return Ok(None);
        }
        const HINT: &str = "gates need carebus-api (set CAREBUS_GATES=off to run without gates)";
        let url = crate::required_var("CAREBUS_API_URL", get("CAREBUS_API_URL")).context(HINT)?;
        let token =
            crate::required_var("CAREBUS_API_TOKEN", get("CAREBUS_API_TOKEN")).context(HINT)?;
        Ok(Some(Self::new(url, Some(token))))
    }

    /// Ask carebus-api what the customer chat of `scope`'s tenant may search. Any transport or status error is an
    /// `Err`: the caller fails closed.
    pub async fn faq_scope(&self, scope: &Scope) -> Result<FaqScope> {
        let mut req = self
            .http
            .post(format!("{}/api/internal/faq/scope", self.base_url))
            .json(&serde_json::json!({"tenant_id": scope.tenant}));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let v: FaqScope = req
            .send()
            .await
            .context("carebus-api unreachable")?
            .error_for_status()
            .context("carebus-api refused the faq scope")?
            .json()
            .await
            .context("carebus-api returned an unreadable faq scope")?;
        Ok(v)
    }

    /// Ask carebus-api to evaluate `point` for `scope`. Any transport or status error is an `Err`: the caller
    /// decides the fail policy.
    pub async fn evaluate(
        &self,
        scope: &Scope,
        point: &str,
        state: &str,
        case_id: Option<&str>,
        tool: Option<&str>,
    ) -> Result<Verdict> {
        let body = EvalBody {
            tenant_id: scope.tenant,
            job: &scope.job,
            point,
            state,
            case_id,
            tool,
        };
        let mut req = self
            .http
            .post(format!("{}/api/internal/gates/evaluate", self.base_url))
            .json(&body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let v: Verdict = req
            .send()
            .await
            .context("carebus-api unreachable")?
            .error_for_status()
            .context("carebus-api refused the gate evaluation")?
            .json()
            .await
            .context("carebus-api returned an unreadable verdict")?;
        Ok(v)
    }
}

/// What the request gate decided for a qualified request.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestDecision {
    /// Publish as is.
    Pass,
    /// Publish with these answers in `QualifiedRequest::gates`.
    Annotate(Value),
    /// Do not publish; the customer reads this text.
    Stop(String),
}

/// The request point, run by the interaction agent on EVERY inbound turn, after qualifying and before the turn is
/// answered (a question for a missing field, "unclear", or publishing `request.qualified`). carebus-api implements
/// it in-process (it owns the gates). `text` is the customer's message of this turn; `draft` the request so far
/// (after this turn's extraction), when there is one; `req` the request this turn completes, when it does.
/// `Annotate` only takes effect on a turn that publishes; `Stop` ends the request on any turn.
pub trait RequestGate: Send + Sync {
    fn check<'a>(
        &'a self,
        text: &'a str,
        draft: Option<&'a Draft>,
        req: Option<&'a QualifiedRequest>,
    ) -> BoxFuture<'a, RequestDecision>;
}

/// At most this many characters of tool outcomes go into a response gate's state.
const MAX_FACTS_CHARS: usize = 4000;

/// The response point's state: the reply and a summary of the tool outcomes behind it.
pub fn response_state(reply: &str, res: &crate::domain::Resolution) -> String {
    let facts = serde_json::to_string(&res.facts).unwrap_or_default();
    let facts: String = facts.chars().take(MAX_FACTS_CHARS).collect();
    format!(
        "Reply to the customer: {reply}\nCase outcome: {:?}\nTool outcomes: {facts}",
        res.outcome
    )
}

/// What the tool point's state is when the customer's request cannot be read.
pub const NO_REQUEST_TEXT: &str = "(the customer's request is not available)";

/// The tool point's state: the customer's own words only (the request's first message). The tool goes to
/// carebus-api separately (`tool`); its name and arguments are NOT in the state, because laya read words such as
/// "refund" in them as the customer asking for one (calibration 2026-10-18). An unreadable request is a fixed
/// placeholder, which no consent question reads as consent.
pub fn tool_state(utterance: &str) -> String {
    match utterance.trim() {
        "" => NO_REQUEST_TEXT.to_string(),
        u => u.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_stopping_action_or_a_closed_failure_stops() {
        let v = |outcome: &str, action: Option<&str>| Verdict {
            outcome: outcome.into(),
            action: action.map(String::from),
            ..Verdict::default()
        };
        assert!(v("fired", Some("handoff")).stops());
        assert!(v("fired", Some("block")).stops());
        assert!(!v("fired", Some("annotate")).stops());
        assert!(v("failed_closed", Some("handoff")).stops());
        for o in ["pass", "failed_open", "skipped"] {
            assert!(!v(o, None).stops(), "{o}");
        }
        assert!(Verdict::failed_closed("x").stops());
        assert_eq!(v("fired", Some("handoff")).text(), DEFAULT_HANDOFF);
        let mut m = v("fired", Some("handoff"));
        m.message = Some(" Call us. ".into());
        assert_eq!(m.text(), "Call us.");
    }

    #[test]
    fn gates_are_off_only_when_switched_off() {
        fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
            move |k| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        }
        assert!(
            GateClient::from_vars(vars(&[("CAREBUS_GATES", "off")]))
                .unwrap()
                .is_none()
        );
        let e = GateClient::from_vars(vars(&[])).unwrap_err();
        assert!(format!("{e:#}").contains("CAREBUS_API_URL"), "{e:#}");
        let e = GateClient::from_vars(vars(&[("CAREBUS_API_URL", "http://api")])).unwrap_err();
        assert!(format!("{e:#}").contains("CAREBUS_API_TOKEN"), "{e:#}");
        let e = GateClient::from_vars(vars(&[
            ("CAREBUS_API_URL", "http://api"),
            ("CAREBUS_API_TOKEN", ""),
        ]))
        .unwrap_err();
        assert!(format!("{e:#}").contains("CAREBUS_API_TOKEN"), "{e:#}");
        let on = GateClient::from_vars(vars(&[
            ("CAREBUS_GATES", "on"),
            ("CAREBUS_API_URL", "http://api/"),
            ("CAREBUS_API_TOKEN", "t"),
        ]))
        .unwrap()
        .expect("gates on");
        assert_eq!(on.base_url, "http://api");
        assert_eq!(on.token.as_deref(), Some("t"));
    }

    #[test]
    fn a_verdict_reads_with_only_the_outcome() {
        let v: Verdict = serde_json::from_str(r#"{"outcome":"skipped"}"#).unwrap();
        assert_eq!(v.outcome, "skipped");
        assert!(!v.has_closed && !v.stops());
    }

    #[test]
    fn the_tool_state_is_the_customers_words_only() {
        assert_eq!(tool_state("  I want a refund "), "I want a refund");
        assert_eq!(tool_state(" "), NO_REQUEST_TEXT);
    }
}
