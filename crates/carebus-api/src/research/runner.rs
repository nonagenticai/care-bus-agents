//! The research loop. One `process` call drives one claimed run until it is done, failed or cancelled, the lease is
//! lost, or the process shuts down.
//!
//! Each iteration renews the lease (and reads `cancel_requested`), rebuilds the conversation from the stored steps
//! ([`replay`]) and takes exactly one action, written as one `research_steps` row before the next: a model turn, or
//! one tool call of the last model turn. A crashed runner's run is therefore re-claimed and continued from its last
//! step without repeating any. The loop only ends through `submit_report`; when a budget runs out (or the model
//! stops making progress) the model gets up to [`WRITE_TURNS`] forced write turns (on the `research_write`
//! provider) that offer `submit_report` alone, so it writes from the evidence it already has; a call to any other
//! tool in a write turn is refused, never executed. If no write turn produces an accepted report the run fails
//! (`<reason>: insufficient_evidence ...`); an accepted report is still verified as usual.
//!
//! A tool call identical to one that already succeeded in this run (same tool, same normalised arguments,
//! [`call_key`]) is not executed again: the model is told which step holds the result. A call to a tool that is not
//! offered is refused with the list of tools that are.
//!
//! While a model call or a tool call is in flight, a heartbeat renews the lease every `lease_s / 3` (a model call may
//! take up to `RESEARCH_LLM_TIMEOUT_S` per attempt, retries included), so a healthy run is never re-claimed; a lost
//! lease or a cancel request seen by the heartbeat abandons the call. Every model call is metered into
//! `usage_ledger` (source `research`, migration 0023) right after it returns. A model that is not the platform's
//! own fails the run unless the tenant has `external_llm_allowed` (research runs only).
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::watch;
use uuid::Uuid;

use super::llm::{
    ChatReply, ChatRequest, DefaultLlm, Llm, LlmEndpoint, LlmError, USE_RESEARCH,
    USE_RESEARCH_WRITE, Usage, is_platform_model, resolve_endpoint,
};
use super::prompts::{NUDGE_SYNTHESIS, NUDGE_TOOLS, WRITE_NOW};
use super::store::{self, NewStep, ReportIn, RunScope, StepRow};
use super::tools::{
    Resolver, SUBMIT_REPORT, StepStatus, ToolCtx, ToolOutcome, ToolSet, submit_report_spec,
};
use super::verify::{self, CitationIn, ClaimReport, Verdict};
use super::{Budget, Profile, SourceKind, cap_tool_text};
use crate::crypto::SourceKey;
use crate::metering::{self, Attribution, UsageSource};
use crate::walls;

pub const DEFAULT_LEASE_S: i32 = 600;
/// `RESEARCHER_MAX_ATTEMPTS`.
pub const DEFAULT_MAX_ATTEMPTS: i32 = 3;
/// Forced `submit_report` turns a run gets once its research budget is used up, before it fails. More than one, so
/// a report rejected for its form (the rejection says what to fix) can be resubmitted. They run on top of the
/// [`Budget`]: a run that used `max_steps` model turns can take up to `max_steps + WRITE_TURNS`, and the write turns'
/// tokens and wall time are spent past `max_tokens` / `max_wall_s` (a model call each, no tools).
pub const WRITE_TURNS: u32 = 2;
/// Consecutive calls of one tool after which the model is reminded to write up what it has.
pub const SAME_TOOL_NUDGE: u32 = 8;
/// Consecutive model turns whose every tool call was refused (repeats, unknown tools) after which the research
/// phase ends and the model must write (`no_progress`).
pub const STALL_TURNS: u32 = 3;

pub struct RunnerConfig {
    /// `lease_owner`: unique per worker (host:pid:worker).
    pub owner: String,
    pub lease_s: i32,
    pub default_llm: DefaultLlm,
    /// Per model call; keep it well under `lease_s`.
    pub llm_timeout: Duration,
    pub source_key: Option<SourceKey>,
    /// Test-only (loopback provider URLs); never settable from the environment.
    pub allow_loopback: bool,
    /// Model call attempts on a transient failure, and the first backoff (doubling).
    pub llm_attempts: u32,
    pub llm_backoff: Duration,
    /// Test hook: return as if crashed (lease kept, nothing released) after writing this many steps.
    pub crash_after_steps: Option<usize>,
    /// A run claimed more often than this (crash loop) fails with `too_many_attempts`.
    pub max_attempts: i32,
    /// How often the lease is renewed while a model or tool call is in flight; `None` = `lease_s / 3`.
    pub heartbeat: Option<Duration>,
    /// `METERING_EMIT_ENABLED`: also write the billing outbox row (the API's drainer sends it). The ledger row is
    /// written regardless.
    pub metering_emit: bool,
    /// `SERVICE_ACTOR_ID`: care-bus' service-account sub, the `X-Actor-Id` of a platform-model call when the run's
    /// creator is not a uuid subject. `None` = no `X-Actor-Id` then.
    pub service_actor: Option<Uuid>,
}

impl RunnerConfig {
    pub fn new(owner: String, default_llm: DefaultLlm) -> Self {
        Self {
            owner,
            lease_s: DEFAULT_LEASE_S,
            default_llm,
            llm_timeout: Duration::from_secs(300),
            source_key: None,
            allow_loopback: false,
            llm_attempts: 3,
            llm_backoff: Duration::from_secs(2),
            crash_after_steps: None,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            heartbeat: None,
            metering_emit: false,
            service_actor: None,
        }
    }

    /// The heartbeat period: `heartbeat`, else a third of the lease (at least 1 s).
    pub fn heartbeat_every(&self) -> Duration {
        self.heartbeat.unwrap_or_else(|| {
            Duration::from_secs(u64::try_from(self.lease_s / 3).unwrap_or(0).max(1))
        })
    }
}

/// Why an in-flight call was abandoned by the heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupt {
    /// Another runner holds the run now.
    LeaseLost,
    /// `cancel_requested` was set.
    Cancelled,
}

/// The `usage_ledger` provider label of a research model call: the platform's (`vllm`, as the chat qualifier
/// records it) or the tenant provider's id.
pub fn usage_provider(ep: &LlmEndpoint) -> String {
    match ep.provider {
        None => carebus::interaction::LLM_PROVIDER.to_string(),
        Some(id) => format!("provider:{id}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Failed(String),
    Cancelled,
    /// Shutdown requested: the lease was released for another runner.
    Shutdown,
    /// Another runner holds the run now; nothing more was written.
    LeaseLost,
    /// The test hook stopped the run (lease kept).
    Crashed,
    /// A tool call started an MCP task: the run is `waiting` (no lease) and the task's end requeues it.
    Waiting,
    /// Nothing to do (already terminal, or gone).
    Skipped,
}

pub struct Runner<L, T, R> {
    pub db: PgPool,
    pub cfg: RunnerConfig,
    pub llm: L,
    pub tools: Arc<T>,
    pub resolver: Arc<R>,
}

/// One tool call of a model turn.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Parsed arguments, or the raw text when it is not JSON.
    pub arguments: Result<Value, String>,
}

impl ToolCall {
    fn arguments_json(&self) -> Value {
        match &self.arguments {
            Ok(v) => v.clone(),
            Err(raw) => Value::String(raw.clone()),
        }
    }
}

/// The conversation and counters rebuilt from a run's steps.
#[derive(Debug)]
pub struct Replay {
    pub messages: Vec<Value>,
    /// Tool calls of the last model turn that have no step yet, in order.
    pub pending: Vec<ToolCall>,
    pub next_ordinal: i32,
    pub model_turns: u32,
    /// Tool calls executed (ok or error), `submit_report` and denied calls excluded.
    pub tool_calls: u32,
    /// The last model turn was a forced `submit_report` turn, and why.
    pub last_forced: Option<String>,
    /// Model turns at the end without any tool call.
    pub no_tool_streak: u32,
    /// Forced write turns so far.
    pub forced_turns: u32,
    /// [`call_key`] of every tool call that succeeded, with the step that holds its result (the first one).
    pub done_calls: HashMap<String, i32>,
    /// Consecutive tool steps (any status) of the same tool at the end; `submit_report` resets it.
    pub same_tool_streak: u32,
    /// Consecutive complete model turns at the end that called tools but had none run: every call refused, or its
    /// arguments not valid JSON. A turn without tool calls leaves it as it is: [`Replay::no_tool_streak`] judges those.
    pub stalled_turns: u32,
}

/// `value` as JSON with object keys sorted at every level, so the same arguments in any key order compare equal. Only
/// `url` values are normalised ([`normalise_url`]); every other string is compared as the model wrote it.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let mut m = serde_json::Map::new();
            for k in keys {
                let v = &o[k];
                let v = match (k.as_str(), v) {
                    ("url", Value::String(u)) => Value::String(normalise_url(u)),
                    _ => canonical(v),
                };
                m.insert(k.clone(), v);
            }
            Value::Object(m)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// A URL as compared for repeats: trimmed, without a trailing `/` and without its `#fragment`, except a `#/...`
/// fragment, which is an SPA route (`#/a` and `#/b` are different pages).
fn normalise_url(u: &str) -> String {
    let u = u.trim();
    let (base, fragment) = match u.split_once('#') {
        Some((a, f)) if f.starts_with('/') => (a, Some(f)),
        Some((a, _)) => (a, None),
        None => (u, None),
    };
    let base = base.trim_end_matches('/');
    match fragment {
        Some(f) => format!("{base}#{f}"),
        None => base.to_string(),
    }
}

/// What makes two tool calls "the same call": the tool name and its arguments, keys sorted and `url` values normalised.
pub fn call_key(name: &str, args: &Value) -> String {
    format!("{name}\u{0}{}", canonical(args))
}

/// The refusal of a call to a tool that is not offered: names the tools that are, so the model can recover.
pub fn unknown_tool_message(name: &str, offered: &[&str]) -> String {
    let mut names: Vec<String> = offered.iter().map(|n| format!("`{n}`")).collect();
    names.push(format!("`{SUBMIT_REPORT}`"));
    format!(
        "`{name}` is not an available tool. The only tools in this run are: {}. Use one of them, or call \
         `{SUBMIT_REPORT}` with your cited claims.",
        names.join(", ")
    )
}

/// The refusal of a call that already succeeded at step `ordinal`.
pub fn repeat_message(name: &str, ordinal: i32) -> String {
    format!(
        "`{name}` was already called with these arguments; its result is above under [step {ordinal}] and was \
         not fetched again. Use that result (cite step {ordinal}), make a different call, or call \
         `{SUBMIT_REPORT}` with your cited claims."
    )
}

/// The system prompt's note on the tools actually offered for this turn (`specs` as sent to the model).
pub fn tools_note(specs: &[Value]) -> String {
    let names: Vec<String> = specs
        .iter()
        .map(|t| format!("`{}`", spec_name(t)))
        .collect();
    format!(
        "Tools available in this run: {}. No other tool exists (a call to any other name is refused); a tool \
         mentioned above that is not in this list is not offered for this run.",
        names.join(", ")
    )
}

fn calls_of(message: &Value) -> Vec<ToolCall> {
    message["tool_calls"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| {
                    let raw = &c["function"]["arguments"];
                    let arguments = match raw {
                        Value::String(s) if s.trim().is_empty() => Ok(json!({})),
                        Value::String(s) => serde_json::from_str(s).map_err(|_| s.clone()),
                        Value::Null => Ok(json!({})),
                        other => Ok(other.clone()),
                    };
                    ToolCall {
                        id: c["id"].as_str().unwrap_or("").to_string(),
                        name: c["function"]["name"].as_str().unwrap_or("").to_string(),
                        arguments,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Tool results are shown to the model headed by their step number, which citations name.
fn tool_message(ordinal: i32, req: &Value, resp: Option<&Value>, status: &str) -> Value {
    let text = resp.and_then(|r| r["text"].as_str()).unwrap_or("");
    let truncated = resp.and_then(|r| r["truncated"].as_bool()).unwrap_or(false);
    let mut content = match status {
        "ok" => format!("[step {ordinal}]\n{text}"),
        "denied" => format!("[step {ordinal}] DENIED: {text}"),
        _ => format!("[step {ordinal}] ERROR: {text}"),
    };
    if truncated {
        content.push_str("\n[result truncated]");
    }
    json!({"role": "tool", "tool_call_id": req["tool_call_id"], "content": content})
}

/// The tool step's call actually ran: not refused, and its arguments parsed (a call with invalid JSON arguments is
/// stored with the raw text as `arguments` and status `error`, and never reached the tool).
fn ran(s: &StepRow) -> bool {
    s.status != "denied" && !s.request["arguments"].is_string()
}

pub fn replay(profile: Profile, question: &str, steps: &[StepRow]) -> Replay {
    let mut messages = vec![
        json!({"role": "system", "content": profile.system_prompt()}),
        json!({"role": "user", "content": question}),
    ];
    let mut current: Vec<ToolCall> = vec![];
    let mut answered = 0usize;
    let mut model_turns = 0u32;
    let mut tool_calls = 0u32;
    let mut last_forced = None;
    let mut no_tool_streak = 0u32;
    let mut forced_turns = 0u32;
    let mut done_calls: HashMap<String, i32> = HashMap::new();
    let mut same_tool_streak = 0u32;
    let mut last_tool: Option<String> = None;
    let mut stalled_turns = 0u32;
    let mut turn_executed = false;
    for s in steps {
        match s.kind.as_str() {
            "model" => {
                // the previous turn is complete here (its calls are answered before the next model turn)
                if !current.is_empty() {
                    if turn_executed {
                        stalled_turns = 0;
                    } else {
                        stalled_turns += 1;
                    }
                }
                turn_executed = false;
                model_turns += 1;
                messages.push(s.request.clone());
                current = calls_of(&s.request);
                answered = 0;
                last_forced = s
                    .response
                    .as_ref()
                    .filter(|r| r["forced"] == Value::Bool(true))
                    .map(|r| {
                        r["forced_reason"]
                            .as_str()
                            .unwrap_or("no_report")
                            .to_string()
                    });
                if last_forced.is_some() {
                    forced_turns += 1;
                }
                if current.is_empty() {
                    no_tool_streak += 1;
                    messages.push(json!({"role": "user", "content": NUDGE_TOOLS}));
                } else {
                    no_tool_streak = 0;
                }
            }
            _ => {
                answered += 1;
                let name = s.request["name"].as_str().unwrap_or("");
                if name != SUBMIT_REPORT && s.status != "denied" {
                    tool_calls += 1;
                }
                if ran(s) {
                    turn_executed = true;
                }
                if name == SUBMIT_REPORT {
                    same_tool_streak = 0;
                    last_tool = None;
                } else if last_tool.as_deref() == Some(name) {
                    same_tool_streak += 1;
                } else {
                    same_tool_streak = 1;
                    last_tool = Some(name.to_string());
                }
                if s.status == "ok" && name != SUBMIT_REPORT {
                    done_calls
                        .entry(call_key(name, &s.request["arguments"]))
                        .or_insert(s.ordinal);
                }
                messages.push(tool_message(
                    s.ordinal,
                    &s.request,
                    s.response.as_ref(),
                    &s.status,
                ));
            }
        }
    }
    if !current.is_empty() && answered >= current.len() {
        if turn_executed {
            stalled_turns = 0;
        } else {
            stalled_turns += 1;
        }
    }
    let pending = current.into_iter().skip(answered).collect();
    Replay {
        messages,
        pending,
        next_ordinal: steps.last().map_or(0, |s| s.ordinal + 1),
        model_turns,
        tool_calls,
        last_forced,
        no_tool_streak,
        forced_turns,
        done_calls,
        same_tool_streak,
        stalled_turns,
    }
}

/// The assistant message as it is stored and sent back: `content` and `tool_calls` only (no reasoning fields),
/// every call with an id and string arguments.
pub fn assistant_message(raw: &Value, ordinal: i32) -> Value {
    let content = match &raw["content"] {
        Value::String(s) => Value::String(s.clone()),
        _ => Value::Null,
    };
    let calls: Vec<Value> = raw["tool_calls"]
        .as_array()
        .map(|a| {
            a.iter()
                .enumerate()
                .map(|(i, c)| {
                    let id = c["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| format!("call_{ordinal}_{i}"));
                    let args = match &c["function"]["arguments"] {
                        Value::String(s) => s.clone(),
                        Value::Null => "{}".to_string(),
                        other => other.to_string(),
                    };
                    json!({"id": id, "type": "function",
                           "function": {"name": c["function"]["name"].as_str().unwrap_or(""), "arguments": args}})
                })
                .collect()
        })
        .unwrap_or_default();
    let mut m = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        m["tool_calls"] = Value::Array(calls);
    }
    m
}

const MAX_CLAIMS: usize = 200;
const MAX_CITATIONS: usize = 20;

/// `submit_report` arguments, checked: at least one claim, every claim with 1..=20 citations of a known
/// kind and within the column limits. The message says what to fix.
pub fn parse_report(args: &Value) -> Result<ReportIn, String> {
    let claims = args["claims"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("`claims` must be a non-empty array")?;
    if claims.len() > MAX_CLAIMS {
        return Err(format!("at most {MAX_CLAIMS} claims"));
    }
    let mut out = vec![];
    for (i, c) in claims.iter().enumerate() {
        let n = i + 1;
        let text = c["text"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty() && t.chars().count() <= 4000)
            .ok_or_else(|| format!("claim {n}: `text` must be 1..4000 characters"))?;
        let cits = c["citations"]
            .as_array()
            .filter(|a| !a.is_empty() && a.len() <= MAX_CITATIONS)
            .ok_or_else(|| format!("claim {n}: needs 1..{MAX_CITATIONS} citations"))?;
        let mut parsed = vec![];
        for (j, x) in cits.iter().enumerate() {
            let m = j + 1;
            let kind = x["source_kind"]
                .as_str()
                .and_then(SourceKind::parse)
                .ok_or_else(|| format!(
                    "claim {n} citation {m}: `source_kind` must be one of strata, web, eli, saos, eurlex, krs, literature, stats, other"
                ))?;
            let locator = x["locator"]
                .as_str()
                .map(str::trim)
                .filter(|l| !l.is_empty() && l.chars().count() <= 2000)
                .ok_or_else(|| {
                    format!("claim {n} citation {m}: `locator` must be 1..2000 characters")
                })?;
            let quote = x["quote"]
                .as_str()
                .filter(|q| !q.trim().is_empty() && q.chars().count() <= 4000)
                .ok_or_else(|| {
                    format!("claim {n} citation {m}: `quote` must be 1..4000 characters")
                })?;
            let step_ordinal = match &x["step_ordinal"] {
                Value::Null => None,
                v => Some(
                    v.as_i64()
                        .and_then(|o| i32::try_from(o).ok())
                        .filter(|o| *o >= 0)
                        .ok_or_else(|| {
                            format!("claim {n} citation {m}: `step_ordinal` must be a step number")
                        })?,
                ),
            };
            parsed.push(CitationIn {
                source_kind: kind,
                locator: locator.to_string(),
                title: x["title"]
                    .as_str()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from),
                quote: quote.to_string(),
                step_ordinal,
            });
        }
        out.push((text.to_string(), parsed));
    }
    Ok(ReportIn { claims: out })
}

/// What the loop does after the last model turn's calls are all answered.
#[derive(Debug, PartialEq, Eq)]
pub enum Turn {
    /// A research turn (all tools offered).
    Research,
    /// A forced write turn (`submit_report` only), and why.
    Write(String),
    /// The run fails with this error.
    Fail(String),
}

/// The next turn: a write turn while the last turn was one and [`WRITE_TURNS`] are not used up; once they are, the
/// run fails as `insufficient_evidence` under the reason it was forced for. A research turn otherwise, unless the
/// budget is `exhausted`, the model answered twice without tools (`no_report`), or [`STALL_TURNS`] turns had every
/// call refused (`no_progress`).
pub fn next_turn(rp: &Replay, exhausted: bool) -> Turn {
    if let Some(reason) = &rp.last_forced {
        if rp.forced_turns >= WRITE_TURNS {
            return Turn::Fail(format!(
                "{reason}: insufficient_evidence: no report with cited claims was accepted in {} write turn(s) \
                 after {} tool call(s)",
                rp.forced_turns, rp.tool_calls
            ));
        }
        return Turn::Write(reason.clone());
    }
    if exhausted {
        Turn::Write("budget_exhausted".into())
    } else if rp.no_tool_streak >= 2 {
        Turn::Write("no_report".into())
    } else if rp.stalled_turns >= STALL_TURNS {
        Turn::Write("no_progress".into())
    } else {
        Turn::Research
    }
}

/// The messages of one model turn: the replayed conversation with the system prompt completed by [`tools_note`] for
/// the tools actually sent, then [`WRITE_NOW`] on a write turn, or [`NUDGE_SYNTHESIS`] after [`SAME_TOOL_NUDGE`]
/// consecutive calls of one tool. Derived from the steps only, so a resumed run sends the same.
pub fn turn_messages(
    mut messages: Vec<Value>,
    tools: &[Value],
    write: bool,
    same_tool_streak: u32,
) -> Vec<Value> {
    if let Some(sys) = messages.first_mut() {
        let base = sys["content"].as_str().unwrap_or("").to_string();
        sys["content"] = Value::String(format!("{base}\n\n{}", tools_note(tools)));
    }
    if write {
        messages.push(json!({"role": "user", "content": WRITE_NOW}));
    } else if same_tool_streak >= SAME_TOOL_NUDGE {
        messages.push(json!({"role": "user", "content": NUDGE_SYNTHESIS}));
    }
    messages
}

fn spec_name(spec: &Value) -> &str {
    spec["function"]["name"].as_str().unwrap_or("")
}

fn ms(since: Instant) -> i32 {
    i32::try_from(since.elapsed().as_millis()).unwrap_or(i32::MAX)
}

/// Owner label for a worker: `$HOSTNAME:pid:w{index}`.
pub fn owner_label(worker: usize) -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "researcher".into());
    format!("{host}:{}:w{worker}", std::process::id())
}

enum Step {
    Written,
    Waiting,
    LeaseLost,
    Cancelled,
}

impl<L: Llm, T: ToolSet, R: Resolver> Runner<L, T, R> {
    pub async fn claim_once(&self) -> Result<Option<RunScope>> {
        store::claim(&self.db, &self.cfg.owner, self.cfg.lease_s).await
    }

    /// Claim and process runs until shutdown; sleeps `poll` when there is nothing to claim. A run whose processing
    /// errors keeps its lease and is retried by any runner once the lease expires.
    pub async fn work(self: Arc<Self>, mut shutdown: watch::Receiver<bool>, poll: Duration) {
        loop {
            let stopping = *shutdown.borrow();
            if stopping {
                return;
            }
            match self.claim_once().await {
                Ok(Some(s)) => match self.process(s, &shutdown).await {
                    Ok(o) => {
                        tracing::info!(run = %s.run, tenant = %s.tenant, outcome = ?o, "research run")
                    }
                    Err(e) => {
                        tracing::error!(run = %s.run, tenant = %s.tenant, error = %format!("{e:#}"),
                            "research run errored; retried after its lease expires")
                    }
                },
                Ok(None) => {
                    tokio::select! {
                        _ = tokio::time::sleep(poll) => {}
                        _ = shutdown.changed() => {}
                    }
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "research claim failed");
                    tokio::select! {
                        _ = tokio::time::sleep(poll) => {}
                        _ = shutdown.changed() => {}
                    }
                }
            }
        }
    }

    async fn fail(&self, s: RunScope, error: &str) -> Result<Outcome> {
        let msg: String = error.chars().take(1000).collect();
        Ok(
            if store::finish(
                &self.db,
                s,
                &self.cfg.owner,
                "failed",
                Some(msg.as_str()),
                None,
            )
            .await?
            {
                Outcome::Failed(msg)
            } else {
                Outcome::LeaseLost
            },
        )
    }

    /// Drives `fut` while renewing the lease every [`RunnerConfig::heartbeat_every`]. A lost lease or a cancel request
    /// drops `fut` (the call is abandoned) and says why. A failed renewal (database hiccup) is logged and retried on
    /// the next tick: the lease is still valid until it expires, and every write re-checks it.
    pub async fn with_heartbeat<F: Future>(
        &self,
        s: RunScope,
        fut: F,
    ) -> Result<F::Output, Interrupt> {
        let every = self.cfg.heartbeat_every();
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                out = &mut fut => return Ok(out),
                _ = tick.tick() => {
                    match store::renew(&self.db, s, &self.cfg.owner, self.cfg.lease_s).await {
                        Ok(None) => return Err(Interrupt::LeaseLost),
                        Ok(Some(hb)) if hb.cancel_requested => return Err(Interrupt::Cancelled),
                        Ok(Some(_)) => {}
                        Err(e) => tracing::warn!(run = %s.run, error = %format!("{e:#}"),
                            "renewing the research lease failed; retrying on the next heartbeat"),
                    }
                }
            }
        }
    }

    /// One model call into `usage_ledger` (source `research`). A failure is logged, never returned: metering must
    /// not fail a run whose model call already happened.
    async fn meter(
        &self,
        s: RunScope,
        job_key: &str,
        ep: &LlmEndpoint,
        usage: &Usage,
        latency: Duration,
    ) {
        let a = Attribution {
            tenant: s.tenant,
            job_id: s.job,
            job_key,
            source: UsageSource::Research { run_id: s.run },
        };
        let u = carebus::interaction::LlmUsage {
            provider: usage_provider(ep),
            model: ep.model.clone(),
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            calls: 1,
            latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
        };
        if let Err(e) =
            metering::record_with(&self.db, self.cfg.metering_emit, None, &a, &[u]).await
        {
            tracing::error!(run = %s.run, tenant = %s.tenant, error = ?e,
                "recording research model usage failed; this call is not metered");
        }
    }

    /// The first of the run's stores that reaches a wall it may not search ([`store::forbidden_walls`]): one of those
    /// stores, or one above it in the strata tree (a search routed from it reaches it, crate::walls). The tree is read
    /// only when such a wall exists; a tree that cannot be read is an error (the run is retried, never continued
    /// unchecked).
    async fn walled_store(&self, s: RunScope, run: &store::RunRow) -> Result<Option<String>> {
        if run.store_ids.is_empty() {
            return Ok(None);
        }
        let forbidden: HashSet<String> =
            store::forbidden_walls(&self.db, s.tenant, run.matter_id, &run.created_by)
                .await?
                .into_iter()
                .collect();
        if forbidden.is_empty() {
            return Ok(None);
        }
        if let Some(st) = run
            .store_ids
            .iter()
            .find(|st| forbidden.contains(&walls::canonical(st)))
        {
            return Ok(Some(st.clone()));
        }
        let Some(body) = self
            .tools
            .store_tree(s.tenant)
            .await
            .map_err(|e| anyhow::anyhow!("store tree for the wall check: {e}"))?
        else {
            return Ok(None);
        };
        let tree = walls::Tree::from_stores(&body)
            .map_err(|_| anyhow::anyhow!("store tree for the wall check: unreadable"))?;
        Ok(run
            .store_ids
            .iter()
            .find(|st| !tree.subtree(&walls::canonical(st)).is_disjoint(&forbidden))
            .cloned())
    }

    async fn cancelled(&self, s: RunScope) -> Result<Outcome> {
        Ok(
            if store::finish(&self.db, s, &self.cfg.owner, "cancelled", None, None).await? {
                Outcome::Cancelled
            } else {
                Outcome::LeaseLost
            },
        )
    }

    pub async fn process(&self, s: RunScope, shutdown: &watch::Receiver<bool>) -> Result<Outcome> {
        let Some(run) = store::load_run(&self.db, s).await? else {
            return Ok(Outcome::Skipped);
        };
        if run.lease_owner.as_deref() != Some(self.cfg.owner.as_str()) {
            return Ok(Outcome::LeaseLost);
        }
        if !matches!(run.status.as_str(), "running" | "verifying") {
            return Ok(Outcome::Skipped);
        }
        if run.attempts > self.cfg.max_attempts {
            return self.fail(s, "too_many_attempts").await;
        }
        let Some(profile) = Profile::parse(&run.profile) else {
            return self.fail(s, "unknown_profile").await;
        };
        // The walls are re-checked at every claim, not only when the run was filed: a store walled since for a
        // matter this run may not search stops it before it searches again.
        if let Some(st) = self.walled_store(s, &run).await? {
            return self
                .fail(
                    s,
                    &format!(
                        "store_walled: store {st} is now walled for a legal matter this run may not search"
                    ),
                )
                .await;
        }
        let budget = Budget::from_json(&run.budget);
        // The person who started the run (`created_by`, a Keycloak sub), when it is a uuid.
        let creator = carebus::identity::human_actor(&run.created_by);
        let ctx = ToolCtx {
            tenant: s.tenant,
            profile,
            store_ids: &run.store_ids,
            actor: creator,
            run: Some(s),
            created_by: &run.created_by,
        };
        let mut endpoints: HashMap<&'static str, LlmEndpoint> = HashMap::new();
        let mut written = 0usize;
        loop {
            let stopping = *shutdown.borrow();
            if stopping {
                store::release(&self.db, s, &self.cfg.owner).await?;
                return Ok(Outcome::Shutdown);
            }
            if self.cfg.crash_after_steps == Some(written) {
                return Ok(Outcome::Crashed);
            }
            let Some(hb) = store::renew(&self.db, s, &self.cfg.owner, self.cfg.lease_s).await?
            else {
                return Ok(Outcome::LeaseLost);
            };
            match hb.status.as_str() {
                "verifying" => return self.verify(s, profile).await,
                "running" => {}
                _ => return Ok(Outcome::Skipped),
            }
            if hb.cancel_requested {
                return self.cancelled(s).await;
            }
            let steps = store::load_steps(&self.db, s).await?;
            let rp = replay(profile, &run.question, &steps);
            if let Some(call) = rp.pending.first() {
                match self.tool_step(s, &ctx, &rp, call, &budget).await? {
                    Step::Written => {}
                    Step::Waiting => return Ok(Outcome::Waiting),
                    Step::LeaseLost => return Ok(Outcome::LeaseLost),
                    Step::Cancelled => return self.cancelled(s).await,
                }
                written += 1;
                continue;
            }
            let exhausted = rp.model_turns >= budget.max_steps
                || rp.tool_calls >= budget.max_tool_calls
                || hb.elapsed_s >= budget.max_wall_s as f64
                || u64::try_from(hb.tokens_used).unwrap_or(0) >= budget.max_tokens;
            let forced = match next_turn(&rp, exhausted) {
                Turn::Fail(msg) => return self.fail(s, &msg).await,
                Turn::Research => None,
                Turn::Write(reason) => Some(reason),
            };
            let use_name = if forced.is_some() {
                USE_RESEARCH_WRITE
            } else {
                USE_RESEARCH
            };
            let mut ep = match endpoints.get(use_name) {
                Some(ep) => ep.clone(),
                None => match resolve_endpoint(
                    &self.db,
                    s.tenant,
                    use_name,
                    self.cfg.source_key.as_ref(),
                    self.cfg.allow_loopback,
                    &self.cfg.default_llm,
                    self.cfg.llm_timeout,
                )
                .await
                {
                    Ok(ep) => {
                        endpoints.insert(use_name, ep.clone());
                        ep
                    }
                    Err(e) => return self.fail(s, &format!("llm_provider_unusable: {e:#}")).await,
                },
            };
            // X-Actor-Id of a platform-model call (chat_request sends it only with effective_tenant).
            ep.actor = carebus::identity::pick_actor(creator, self.cfg.service_actor);
            // Default false: only the platform's own model, never a silent fallback to it. Re-read before EVERY
            // call to an external model (the endpoint is cached for the run), so turning the flag off stops a
            // running run at its next model call.
            if !is_platform_model(&ep, &self.cfg.default_llm)
                && !store::external_llm_allowed(&self.db, s.tenant).await?
            {
                return self
                    .fail(
                        s,
                        &format!(
                            "external_llm_not_allowed: the `{use_name}` provider ({}) is not the platform model \
                             and this tenant does not allow external LLMs (external_llm_allowed is off)",
                            ep.base_url
                        ),
                    )
                    .await;
            }
            // A write turn offers `submit_report` alone: a model that ignores `tool_choice` cannot wander off.
            let tools = if forced.is_some() {
                vec![submit_report_spec()]
            } else {
                let mut t = self.tools.specs(&ctx).await;
                t.push(submit_report_spec());
                t
            };
            let tool_choice = match forced {
                Some(_) => Some(json!({"type": "function", "function": {"name": SUBMIT_REPORT}})),
                None if rp.no_tool_streak == 1 => Some(json!("required")),
                None => None,
            };
            let messages =
                turn_messages(rp.messages, &tools, forced.is_some(), rp.same_tool_streak);
            let req = ChatRequest {
                messages,
                tools,
                tool_choice,
            };
            let started = Instant::now();
            let reply = match self.with_heartbeat(s, self.chat(&ep, &req, shutdown)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return self.fail(s, &format!("llm_error: {e}")).await,
                Err(Interrupt::LeaseLost) => return Ok(Outcome::LeaseLost),
                Err(Interrupt::Cancelled) => return self.cancelled(s).await,
            };
            // Metered before the step is written: the tokens are spent even if the lease is lost now.
            self.meter(s, &run.job_key, &ep, &reply.usage, started.elapsed())
                .await;
            let step = NewStep {
                ordinal: rp.next_ordinal,
                kind: "model",
                request: assistant_message(&reply.message, rp.next_ordinal),
                response: json!({
                    "usage": reply.usage,
                    "finish_reason": reply.finish_reason,
                    "model": ep.model,
                    "provider": ep.provider,
                    "use": use_name,
                    "forced": forced.is_some(),
                    "forced_reason": forced,
                }),
                status: "ok",
                duration_ms: ms(started),
                turns: 1,
                tokens: i64::try_from(reply.usage.total_tokens).unwrap_or(i64::MAX),
            };
            if !store::append_step(&self.db, s, &self.cfg.owner, self.cfg.lease_s, &step).await? {
                return Ok(Outcome::LeaseLost);
            }
            written += 1;
        }
    }

    /// One model call, retried on transient failures with doubling backoff (not while shutting down).
    async fn chat(
        &self,
        ep: &LlmEndpoint,
        req: &ChatRequest,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<ChatReply, LlmError> {
        let mut delay = self.cfg.llm_backoff;
        let attempts = self.cfg.llm_attempts.max(1);
        for attempt in 1..=attempts {
            let result = self.llm.chat(ep, req).await;
            let stopping = *shutdown.borrow();
            match result {
                Ok(r) => return Ok(r),
                Err(LlmError::Transient(m)) if attempt < attempts && !stopping => {
                    tracing::warn!(attempt, error = %m, "research model call failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => return Err(e),
            }
        }
        Err(LlmError::Transient("no attempt made".into()))
    }

    async fn tool_step(
        &self,
        s: RunScope,
        ctx: &ToolCtx<'_>,
        rp: &Replay,
        call: &ToolCall,
        budget: &Budget,
    ) -> Result<Step> {
        let started = Instant::now();
        let request =
            json!({"tool_call_id": call.id, "name": call.name, "arguments": call.arguments_json()});
        let step = |o: ToolOutcome, started: Instant| {
            let (text, truncated) = cap_tool_text(&call.name, &o.text);
            NewStep {
                ordinal: rp.next_ordinal,
                kind: "tool",
                request: request.clone(),
                response: json!({"ok": o.status == StepStatus::Ok, "text": text, "truncated": truncated}),
                status: o.status.as_str(),
                duration_ms: ms(started),
                turns: 0,
                tokens: 0,
            }
        };
        let lease = |held: bool| if held { Step::Written } else { Step::LeaseLost };
        let args = match &call.arguments {
            Ok(a) => a,
            Err(_) => {
                let st = step(
                    ToolOutcome::error("the arguments are not valid JSON"),
                    started,
                );
                return Ok(lease(
                    store::append_step(&self.db, s, &self.cfg.owner, self.cfg.lease_s, &st).await?,
                ));
            }
        };
        if call.name == SUBMIT_REPORT {
            return Ok(match parse_report(args) {
                Ok(report) => {
                    let st = step(
                        ToolOutcome::ok("report accepted; citations are being verified"),
                        started,
                    );
                    lease(
                        store::accept_report(
                            &self.db,
                            s,
                            &self.cfg.owner,
                            self.cfg.lease_s,
                            &st,
                            &report,
                        )
                        .await?,
                    )
                }
                Err(msg) => {
                    let st = step(
                        ToolOutcome::error(format!("submit_report rejected: {msg}")),
                        started,
                    );
                    lease(
                        store::append_step(&self.db, s, &self.cfg.owner, self.cfg.lease_s, &st)
                            .await?,
                    )
                }
            });
        }
        let offered = self.tools.specs(ctx).await;
        let names: Vec<&str> = offered.iter().map(spec_name).collect();
        let outcome = if rp.last_forced.is_some() {
            ToolOutcome::denied(format!(
                "`{}` was not run: research is over, only `{SUBMIT_REPORT}` may be called now. Submit your cited \
                 claims from the steps above.",
                call.name
            ))
        } else if !names.contains(&call.name.as_str()) {
            ToolOutcome::denied(unknown_tool_message(&call.name, &names))
        } else if let Some(at) = rp.done_calls.get(&call_key(&call.name, args)) {
            ToolOutcome::denied(repeat_message(&call.name, *at))
        } else if rp.tool_calls >= budget.max_tool_calls {
            ToolOutcome::denied("the tool-call budget is exhausted; call submit_report now")
        } else {
            match self
                .with_heartbeat(s, self.tools.call(ctx, &call.name, args))
                .await
            {
                Ok(o) => o,
                Err(Interrupt::LeaseLost) => return Ok(Step::LeaseLost),
                Err(Interrupt::Cancelled) => return Ok(Step::Cancelled),
            }
        };
        // A call that started an MCP task parks the run: the step is stored as waiting (replay counts it as executed,
        // so a resumed run never starts a second task) and `mcp_tasks::finish` writes the result into it.
        if let Some(task_id) = outcome.task_id.clone() {
            let mut st = step(outcome, started);
            st.response["task_id"] = json!(task_id);
            st.response["waiting"] = json!(true);
            return Ok(
                match store::park_on_task(
                    &self.db,
                    s,
                    &self.cfg.owner,
                    self.cfg.lease_s,
                    &st,
                    &task_id,
                    &call.name,
                )
                .await?
                {
                    store::Parked::Waiting => Step::Waiting,
                    store::Parked::Settled => Step::Written,
                    store::Parked::LeaseLost => Step::LeaseLost,
                },
            );
        }
        let st = step(outcome, started);
        Ok(lease(
            store::append_step(&self.db, s, &self.cfg.owner, self.cfg.lease_s, &st).await?,
        ))
    }

    async fn verify(&self, s: RunScope, profile: Profile) -> Result<Outcome> {
        let input = store::load_verify_input(&self.db, s).await?;
        let mut verdicts: Vec<(Uuid, Verdict)> = vec![];
        for c in &input.citations {
            let Some(hb) = store::renew(&self.db, s, &self.cfg.owner, self.cfg.lease_s).await?
            else {
                return Ok(Outcome::LeaseLost);
            };
            // Cancelled while verifying: stop here; the final write below checks once more.
            if hb.cancel_requested {
                return self.cancelled(s).await;
            }
            let v =
                verify::verify_citation(s.tenant, &c.citation, &input.steps, &*self.resolver).await;
            verdicts.push((c.id, v));
        }
        let by_id: HashMap<Uuid, (&CitationIn, &Verdict)> = input
            .citations
            .iter()
            .zip(verdicts.iter())
            .map(|(c, (_, v))| (c.id, (&c.citation, v)))
            .collect();
        let missing = store::missing_citation();
        let placeholder = CitationIn {
            source_kind: SourceKind::Other,
            locator: "?".into(),
            title: None,
            quote: String::new(),
            step_ordinal: None,
        };
        let reports: Vec<ClaimReport> = input
            .claims
            .iter()
            .map(|c| ClaimReport {
                text: &c.text,
                citations: c
                    .citation_ids
                    .iter()
                    .map(|id| by_id.get(id).copied().unwrap_or((&placeholder, &missing)))
                    .collect(),
            })
            .collect();
        let claim_status: Vec<(Uuid, bool)> = input
            .claims
            .iter()
            .zip(reports.iter())
            .map(|(c, r)| (c.id, r.supported()))
            .collect();
        let report = verify::build_report(profile == Profile::Legal, &reports);
        Ok(
            match store::save_verification(
                &self.db,
                s,
                &self.cfg.owner,
                &verdicts,
                &claim_status,
                &report,
            )
            .await?
            {
                store::Saved::Done => Outcome::Done,
                store::Saved::Cancelled => Outcome::Cancelled,
                store::Saved::LeaseLost => Outcome::LeaseLost,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(ordinal: i32, calls: &[(&str, &str, &str)], forced: bool) -> StepRow {
        let tool_calls: Vec<Value> = calls
            .iter()
            .map(|(id, n, a)| json!({"id": id, "function": {"name": n, "arguments": a}}))
            .collect();
        let msg = assistant_message(&json!({"content": null, "tool_calls": tool_calls}), ordinal);
        let reason: Option<&str> = forced.then_some("budget_exhausted");
        StepRow {
            ordinal,
            kind: "model".into(),
            request: msg,
            response: Some(json!({"forced": forced, "forced_reason": reason})),
            status: "ok".into(),
        }
    }

    fn tool(ordinal: i32, id: &str, name: &str, status: &str) -> StepRow {
        StepRow {
            ordinal,
            kind: "tool".into(),
            request: json!({"tool_call_id": id, "name": name, "arguments": {}}),
            response: Some(json!({"ok": status == "ok", "text": "result", "truncated": false})),
            status: status.into(),
        }
    }

    fn tool_with(ordinal: i32, id: &str, name: &str, args: Value, status: &str) -> StepRow {
        StepRow {
            request: json!({"tool_call_id": id, "name": name, "arguments": args}),
            ..tool(ordinal, id, name, status)
        }
    }

    /// C6: a step that waits on an MCP task is an executed call (never run again on resume); once the task ends its
    /// result is the tool message the model reads, and an error result leaves the call free to be retried.
    #[test]
    fn a_waiting_or_finished_task_step_is_never_executed_again() {
        let calls = [("c1", "fr_project_start", "{}")];
        let mut waiting = tool(1, "c1", "fr_project_start", "ok");
        waiting.response = Some(
            json!({"ok": true, "text": "waiting for MCP task t-1", "task_id": "t-1", "waiting": true}),
        );
        let steps = vec![model(0, &calls, false), waiting.clone()];
        let rp = replay(Profile::Research, "q", &steps);
        assert!(
            rp.pending.is_empty(),
            "the call has its step: not executed again"
        );
        assert_eq!(rp.tool_calls, 1);

        let mut done = waiting.clone();
        done.response = Some(
            json!({"ok": true, "text": "the task result", "truncated": false, "task_id": "t-1"}),
        );
        let rp = replay(Profile::Research, "q", &[model(0, &calls, false), done]);
        assert!(rp.pending.is_empty());
        let last = rp.messages.last().unwrap();
        assert_eq!(last["role"], "tool");
        assert_eq!(last["content"], "[step 1]\nthe task result");

        let mut failed = waiting;
        failed.status = "error".into();
        failed.response = Some(
            json!({"ok": false, "text": "task cancelled", "truncated": false, "task_id": "t-1"}),
        );
        let rp = replay(Profile::Research, "q", &[model(0, &calls, false), failed]);
        assert!(rp.pending.is_empty());
        assert_eq!(
            rp.messages.last().unwrap()["content"],
            "[step 1] ERROR: task cancelled"
        );
        assert!(
            rp.done_calls.is_empty(),
            "a failed task may be started again"
        );
    }

    #[test]
    fn the_same_call_has_the_same_key_whatever_its_spelling() {
        let a = call_key(
            "web_scrape",
            &json!({"url": "https://x.de/faq", "max_chars": 10}),
        );
        let b = call_key(
            "web_scrape",
            &json!({"max_chars": 10, "url": " https://x.de/faq/#top "}),
        );
        assert_eq!(
            a, b,
            "key order, whitespace, trailing slash and fragment do not matter"
        );
        assert_ne!(
            a,
            call_key(
                "web_scrape",
                &json!({"url": "https://x.de/agb", "max_chars": 10})
            )
        );
        assert_ne!(
            a,
            call_key(
                "web_scrape",
                &json!({"url": "https://x.de/faq", "max_chars": 20})
            )
        );
        assert_ne!(
            a,
            call_key(
                "other",
                &json!({"url": "https://x.de/faq", "max_chars": 10})
            )
        );
    }

    #[test]
    fn replay_remembers_succeeded_calls_and_streaks() {
        let u = |p: &str| json!({"url": format!("https://x.de/{p}")});
        let steps = [
            model(
                0,
                &[("a", "web_scrape", "{\"url\":\"https://x.de/faq\"}")],
                false,
            ),
            tool_with(1, "a", "web_scrape", u("faq"), "ok"),
            model(
                2,
                &[
                    ("b", "web_scrape", "{\"url\":\"https://x.de/agb\"}"),
                    ("c", "web_scrape", "{\"url\":\"https://x.de/x\"}"),
                ],
                false,
            ),
            tool_with(3, "b", "web_scrape", u("agb"), "error"),
            tool_with(4, "c", "web_scrape", u("x"), "ok"),
        ];
        let r = replay(Profile::Research, "Q", &steps);
        assert_eq!(
            r.done_calls.get(&call_key("web_scrape", &u("faq"))),
            Some(&1)
        );
        assert_eq!(r.done_calls.get(&call_key("web_scrape", &u("x"))), Some(&4));
        assert!(
            !r.done_calls
                .contains_key(&call_key("web_scrape", &u("agb"))),
            "a failed call may be retried"
        );
        assert_eq!(r.same_tool_streak, 3);
        assert_eq!(r.stalled_turns, 0);
        assert_eq!(r.forced_turns, 0);
    }

    #[test]
    fn spa_routes_and_non_url_strings_keep_their_spelling_in_the_key() {
        let k = |a: Value| call_key("web_scrape", &a);
        assert_ne!(
            k(json!({"url": "https://x.de/#/a"})),
            k(json!({"url": "https://x.de/#/b"})),
            "a `#/` fragment is an SPA route"
        );
        assert_eq!(
            k(json!({"url": " https://x.de/#/a "})),
            k(json!({"url": "https://x.de/#/a"}))
        );
        assert_eq!(
            k(json!({"url": "https://x.de/faq#top"})),
            k(json!({"url": "https://x.de/faq"}))
        );
        assert_ne!(
            call_key("strata_search", &json!({"query": "refund "})),
            call_key("strata_search", &json!({"query": "refund"})),
            "only `url` is normalised"
        );
    }

    #[test]
    fn a_call_with_invalid_json_arguments_does_not_end_a_stall() {
        let bad = |ordinal: i32, id: &str| {
            tool_with(ordinal, id, "web_scrape", json!("{not json"), "error")
        };
        let steps = [
            model(0, &[("a", "web_search", "{}")], false),
            tool(1, "a", "web_search", "denied"),
            model(2, &[("b", "web_scrape", "{not json")], false),
            bad(3, "b"),
        ];
        let r = replay(Profile::Research, "Q", &steps);
        assert_eq!(r.stalled_turns, 2, "the call never reached the tool");
        assert_eq!(r.tool_calls, 1);
        // a call that ran and failed is progress
        let mut more = steps.to_vec();
        more.push(model(4, &[("c", "web_scrape", "{}")], false));
        more.push(tool(5, "c", "web_scrape", "error"));
        assert_eq!(replay(Profile::Research, "Q", &more).stalled_turns, 0);
    }

    #[test]
    fn a_turn_without_tool_calls_leaves_the_stall_count_alone() {
        let steps = [
            model(0, &[("a", "web_search", "{}")], false),
            tool(1, "a", "web_search", "denied"),
            model(2, &[], false),
        ];
        let r = replay(Profile::Research, "Q", &steps);
        assert_eq!(r.stalled_turns, 1);
        assert_eq!(r.no_tool_streak, 1);
        let mut more = steps.to_vec();
        more.push(model(3, &[("b", "web_scrape", "{}")], false));
        more.push(tool(4, "b", "web_scrape", "denied"));
        let r = replay(Profile::Research, "Q", &more);
        assert_eq!(
            r.stalled_turns, 2,
            "neither reset nor counted by the turn in between"
        );
        assert_eq!(r.no_tool_streak, 0);
    }

    #[test]
    fn turns_whose_every_call_was_refused_count_as_stalled() {
        let steps = [
            model(0, &[("a", "web_search", "{}")], false),
            tool(1, "a", "web_search", "denied"),
            model(2, &[("b", "web_scrape", "{}")], false),
            tool(3, "b", "web_scrape", "denied"),
        ];
        let r = replay(Profile::Research, "Q", &steps);
        assert_eq!(r.stalled_turns, 2);
        assert_eq!(next_turn(&r, false), Turn::Research);
        let mut more = steps.to_vec();
        more.push(model(4, &[("c", "web_scrape", "{}")], false));
        // an unanswered turn is not judged yet
        assert_eq!(replay(Profile::Research, "Q", &more).stalled_turns, 2);
        more.push(tool(5, "c", "web_scrape", "denied"));
        let r = replay(Profile::Research, "Q", &more);
        assert_eq!(r.stalled_turns, 3);
        assert_eq!(next_turn(&r, false), Turn::Write("no_progress".into()));
        // one executed call ends the stall
        more.push(model(6, &[("d", "web_scrape", "{}")], false));
        more.push(tool(7, "d", "web_scrape", "ok"));
        assert_eq!(replay(Profile::Research, "Q", &more).stalled_turns, 0);
    }

    #[test]
    fn an_unknown_tool_is_refused_with_the_tools_that_exist() {
        let m = unknown_tool_message("web_search", &["web_scrape", "research_literature_search"]);
        assert!(
            m.starts_with("`web_search` is not an available tool"),
            "{m}"
        );
        for t in [
            "`web_scrape`",
            "`research_literature_search`",
            "`submit_report`",
        ] {
            assert!(m.contains(t), "{m}");
        }
        let r = repeat_message("web_scrape", 7);
        assert!(r.contains("[step 7]") && r.contains("submit_report"), "{r}");
    }

    #[test]
    fn an_exhausted_budget_gets_write_turns_then_fails_as_insufficient_evidence() {
        let research = [
            model(0, &[("a", "web_scrape", "{}")], false),
            tool(1, "a", "web_scrape", "ok"),
        ];
        let r = replay(Profile::Research, "Q", &research);
        assert_eq!(next_turn(&r, false), Turn::Research);
        assert_eq!(next_turn(&r, true), Turn::Write("budget_exhausted".into()));
        // first write turn: the model called a research tool anyway (refused), so it gets another
        let mut steps = research.to_vec();
        steps.push(model(2, &[("b", "web_scrape", "{}")], true));
        steps.push(tool(3, "b", "web_scrape", "denied"));
        let r = replay(Profile::Research, "Q", &steps);
        assert_eq!(r.forced_turns, 1);
        assert_eq!(next_turn(&r, true), Turn::Write("budget_exhausted".into()));
        // second write turn: a rejected report; no write turns left
        steps.push(model(4, &[("c", SUBMIT_REPORT, "{}")], true));
        steps.push(tool(5, "c", SUBMIT_REPORT, "error"));
        let r = replay(Profile::Research, "Q", &steps);
        assert_eq!(r.forced_turns, WRITE_TURNS);
        match next_turn(&r, true) {
            Turn::Fail(m) => assert!(
                m.starts_with("budget_exhausted: insufficient_evidence:")
                    && m.contains("1 tool call"),
                "{m}"
            ),
            t => panic!("{t:?}"),
        }
    }

    #[test]
    fn the_system_prompt_names_the_tools_actually_sent() {
        let base = replay(Profile::Research, "Q", &[]).messages;
        let tools = [
            super::super::tools::function_spec("web_scrape", "d", json!({"type": "object"})),
            submit_report_spec(),
        ];
        let m = turn_messages(base.clone(), &tools, false, 0);
        assert_eq!(m.len(), 2);
        let sys = m[0]["content"].as_str().unwrap();
        assert!(sys.starts_with(Profile::Research.system_prompt()));
        assert!(
            sys.ends_with(&tools_note(&tools)) && sys.contains("`web_scrape`, `submit_report`"),
            "{sys}"
        );
        let w = turn_messages(base.clone(), &[submit_report_spec()], true, 0);
        assert_eq!(w.last().unwrap()["content"], WRITE_NOW);
        assert!(!w[0]["content"].as_str().unwrap().contains("`web_scrape`,"));
        let n = turn_messages(base.clone(), &tools, false, SAME_TOOL_NUDGE);
        assert_eq!(n.last().unwrap()["content"], NUDGE_SYNTHESIS);
        assert_eq!(
            turn_messages(base, &tools, false, SAME_TOOL_NUDGE - 1).len(),
            2
        );
    }

    #[test]
    fn the_heartbeat_is_a_third_of_the_lease_unless_set() {
        let d = DefaultLlm {
            base_url: "http://m.invalid/v1".into(),
            model: "m".into(),
            token: None,
        };
        let mut c = RunnerConfig::new("w".into(), d);
        assert_eq!(c.heartbeat_every(), Duration::from_secs(200));
        // the default lease outlives a full model call (300 s) only because of the heartbeat
        assert!(c.heartbeat_every() < c.llm_timeout);
        c.lease_s = 2;
        assert_eq!(c.heartbeat_every(), Duration::from_secs(1));
        c.heartbeat = Some(Duration::from_millis(50));
        assert_eq!(c.heartbeat_every(), Duration::from_millis(50));
        assert!(!c.metering_emit, "emit is off unless METERING_EMIT_ENABLED");
    }

    #[test]
    fn usage_names_the_platform_or_the_tenant_provider() {
        let id = Uuid::new_v4();
        let ep = |provider| LlmEndpoint {
            base_url: "http://m.invalid/v1".into(),
            model: "m".into(),
            token: None,
            client: reqwest::Client::new(),
            provider,
            effective_tenant: None,
            actor: None,
        };
        assert_eq!(usage_provider(&ep(None)), "vllm");
        assert_eq!(usage_provider(&ep(Some(id))), format!("provider:{id}"));
    }

    #[test]
    fn replay_rebuilds_messages_and_pending_calls() {
        let steps = [
            model(
                0,
                &[
                    ("a", "web_scrape", "{\"url\":\"u\"}"),
                    ("b", "web_scrape", "{"),
                ],
                false,
            ),
            tool(1, "a", "web_scrape", "ok"),
        ];
        let r = replay(Profile::Research, "Q?", &steps);
        assert_eq!(r.next_ordinal, 2);
        assert_eq!(r.model_turns, 1);
        assert_eq!(r.tool_calls, 1);
        assert_eq!(r.pending.len(), 1);
        assert_eq!(r.pending[0].id, "b");
        assert!(r.pending[0].arguments.is_err(), "bad JSON is kept raw");
        assert_eq!(r.messages.len(), 4); // system, user, assistant, tool
        assert_eq!(r.messages[3]["content"], "[step 1]\nresult");
        assert!(r.last_forced.is_none());
    }

    #[test]
    fn replay_counts_no_tool_turns_and_forced_turns() {
        let steps = [model(0, &[], false), model(1, &[], false)];
        let r = replay(Profile::Legal, "Q", &steps);
        assert_eq!(r.no_tool_streak, 2);
        // each tool-less turn is followed by the nudge
        assert_eq!(r.messages.len(), 6);
        let r = replay(Profile::Legal, "Q", &[model(0, &[], true)]);
        assert_eq!(r.last_forced.as_deref(), Some("budget_exhausted"));
        let r = replay(
            Profile::Legal,
            "Q",
            &[
                model(0, &[("a", "x", "{}")], false),
                tool(1, "a", "x", "denied"),
            ],
        );
        assert_eq!(r.tool_calls, 0, "denied calls are not counted");
    }

    #[test]
    fn assistant_messages_are_normalised() {
        let m = assistant_message(
            &json!({"content": "hi", "reasoning_content": "secret", "tool_calls": [
                {"function": {"name": "f", "arguments": {"a": 1}}}]}),
            7,
        );
        assert_eq!(m["role"], "assistant");
        assert!(m.get("reasoning_content").is_none());
        assert_eq!(m["tool_calls"][0]["id"], "call_7_0");
        assert_eq!(m["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        let plain = assistant_message(&json!({"content": "x"}), 0);
        assert!(plain.get("tool_calls").is_none());
    }

    #[test]
    fn report_validation() {
        let ok = json!({"summary_markdown": "S", "claims": [{"text": "T", "citations": [
            {"source_kind": "eli", "locator": "DU/1964/59 art. 1", "quote": "q", "step_ordinal": 1}]}]});
        let r = parse_report(&ok).unwrap();
        assert_eq!(r.claims[0].1[0].step_ordinal, Some(1));
        // the summary is optional: the report is built from the claims
        let mut no_summary = ok.clone();
        no_summary
            .as_object_mut()
            .unwrap()
            .remove("summary_markdown");
        assert_eq!(parse_report(&no_summary).unwrap().claims.len(), 1);
        for (bad, needle) in [
            (json!({}), "claims"),
            (json!({"summary_markdown": "S", "claims": []}), "claims"),
            (
                json!({"summary_markdown": "S", "claims": [{"text": "T", "citations": []}]}),
                "citations",
            ),
            (
                json!({"summary_markdown": "S", "claims": [{"text": "T", "citations": [
                {"source_kind": "lex", "locator": "x", "quote": "q"}]}]}),
                "source_kind",
            ),
            (
                json!({"summary_markdown": "S", "claims": [{"text": "T", "citations": [
                {"source_kind": "web", "locator": "x", "quote": " "}]}]}),
                "quote",
            ),
        ] {
            let e = parse_report(&bad).unwrap_err();
            assert!(e.contains(needle), "{e}");
        }
    }
}
