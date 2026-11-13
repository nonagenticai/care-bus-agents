//! Laya gates (spec 2026-10-18 §3): the engine every gate point runs through. carebus-api owns the questions, the
//! laya provider (the tenant's `gates` binding) and its token; the worker and the interaction daemon reach it over
//! `POST /api/internal/gates/evaluate` (crate::routes::gates), the chat turn in-process ([`TurnGate`]).
//!
//! One evaluation: load the enabled questions of (tenant, job, point) (tool point: only those covering the tool),
//! give each its text (`input`: the customer's own words this turn, or the request text / state), drop those whose
//! `precheck` pattern does not match their text (answer `{"fired": false, "skipped": "precheck"}`, laya not asked),
//! ask the rest in ONE laya call per distinct text (1500 ms, concurrently; usually one or two), apply each question's
//! firing rule, pick the strongest action of the fired ones (`handoff` > `block` > `annotate`), and write one
//! `gate_verdicts` row. A failed call (timeout, non-2xx, bad body, unreadable token) is `failed_closed` (action
//! `handoff`) when any of the questions asked is fail-closed, else `failed_open` (passes). No `gates` binding:
//! `skipped`.
//!
//! The laya bearer: the binding's stored token when it has one (the tenant's own laya); else, with the licences
//! bundle minter on (crate::strata_tokens), the tenant's own platform laya licence (`app=laya&bundle=care-bus`,
//! never a shared token), minted per tenant, cached, and on a laya 401 dropped, re-minted and the call retried once.
//! A refused mint is a failed call (its `laya_*` code in `error`). Minter off and no stored token: no bearer.
pub mod laya;

use std::time::{Duration, Instant};

use carebus::domain::QualifiedRequest;
use carebus::gates::{
    DEFAULT_HANDOFF, GATE_BUDGET_SECS, LAYA_TIMEOUT_MS, RequestDecision, RequestGate,
};
use carebus::interaction::Draft;
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::crypto;
use crate::error::{ApiError, ApiResult};
use crate::netguard;
use crate::routes::llm_providers::open_token;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// Laya's answer budget for one gate point.
pub const LAYA_TIMEOUT: Duration = Duration::from_millis(LAYA_TIMEOUT_MS);
/// The whole in-process evaluation (questions, DNS, laya, verdict row): [`TurnGate::check`] gives up after this.
pub const GATE_BUDGET: Duration = Duration::from_secs(GATE_BUDGET_SECS);
/// Of [`GATE_BUDGET`], what reading the questions and asking laya may use; the rest is for the verdict row.
const DECIDE_BUDGET: Duration = Duration::from_millis(GATE_BUDGET_SECS * 1000 - 500);
/// The model asked when the `gates` binding names none.
pub const DEFAULT_LAYA_MODEL: &str = "jev-latest";
/// What the customer reads when a blocking question has no message of its own.
pub const DEFAULT_BLOCK: &str =
    "I can't continue with this request here. A colleague will follow up with you by email.";
/// At most this many characters of state go to laya.
pub const MAX_STATE_CHARS: usize = 20_000;

pub const POINTS: [&str; 3] = ["request", "tool", "response"];

/// One question as stored and as every read returns it.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Question {
    pub key: String,
    pub point: String,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    pub qtype: String,
    pub instructions: String,
    pub criteria: Value,
    pub fire_when: String,
    pub threshold: Option<f64>,
    pub fire_values: Option<Value>,
    pub action: String,
    pub message: Option<String>,
    pub fail: String,
    pub tools: Option<Value>,
    pub enabled: bool,
    pub ordinal: i32,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
    /// What laya reads: `request` (the request text at the request point, the state elsewhere) or `message` (the
    /// customer's own words this turn; request point only).
    pub input: String,
    /// A regex that must match the question's text before laya is asked; `None` = always asked.
    pub precheck: Option<String>,
}

/// The columns of [`Question`], as a literal for `concat!` (sqlx takes `&'static str` queries).
#[macro_export]
macro_rules! gate_question_columns {
    () => {
        "key, point, \"type\", instructions, criteria, fire_when, threshold, fire_values, action, message, fail, \
         tools, enabled, ordinal, updated_at, updated_by, input, precheck"
    };
}

impl Question {
    /// Whether this tool-point question covers `tool`: its `tools` list, or the side-effect tools when it has none.
    fn covers(&self, tool: &str) -> bool {
        match self.tools.as_ref().and_then(Value::as_array) {
            Some(list) if !list.is_empty() => list.iter().any(|t| t.as_str() == Some(tool)),
            _ => carebus::mcp::SIDE_EFFECT_TOOLS.contains(&tool),
        }
    }

    /// The text this question is asked about: the customer's own words for `input = 'message'` (when the caller
    /// has them), else the state; at most [`MAX_STATE_CHARS`] characters.
    fn text(&self, i: &EvalInput<'_>) -> String {
        let raw = match (self.input.as_str(), i.message) {
            ("message", Some(m)) => m,
            _ => i.state,
        };
        raw.chars().take(MAX_STATE_CHARS).collect()
    }

    /// Whether laya is asked about `text`: no precheck, or the precheck matches. A pattern that does not compile
    /// (the API refuses one on save) asks: a broken filter never silences a question.
    fn prechecked(&self, text: &str) -> bool {
        match self.precheck.as_deref() {
            None => true,
            Some(p) => match regex::Regex::new(p) {
                Ok(r) => r.is_match(text),
                Err(_) => true,
            },
        }
    }

    /// Laya's question object.
    fn laya(&self) -> Value {
        json!({"type": self.qtype, "instructions": self.instructions, "criteria": self.criteria})
    }

    /// The firing rule against this question's answer; `None` = the answer lacks the value the rule reads.
    /// `in` fires when the choice is one of `fire_values` and, when the question has a threshold, laya's confidence
    /// in that choice is at least the threshold.
    pub fn fires(&self, a: &Value) -> Option<bool> {
        if self.fire_when == "in" {
            let choice = a.get("choice")?.as_str()?;
            let values = self.fire_values.as_ref().and_then(Value::as_array)?;
            if !values.iter().any(|v| v.as_str() == Some(choice)) {
                return Some(false);
            }
            return match self.threshold {
                None => Some(true),
                Some(t) => Some(a.get("confidence")?.as_f64()? >= t),
            };
        }
        let field = match self.qtype.as_str() {
            "noul" => "noul",
            "score" => "score",
            _ => "confidence",
        };
        let v = a.get(field)?.as_f64()?;
        let t = self.threshold?;
        match self.fire_when.as_str() {
            "gte" => Some(v >= t),
            "lt" => Some(v < t),
            _ => None,
        }
    }
}

fn rank(action: &str) -> u8 {
    match action {
        "handoff" => 3,
        "block" => 2,
        "annotate" => 1,
        _ => 0,
    }
}

/// What a point evaluated to (the internal route's and the playground's response).
#[derive(Clone, Debug, Serialize)]
pub struct Evaluation {
    pub outcome: String,
    pub action: Option<String>,
    pub message: Option<String>,
    pub fired_keys: Vec<String>,
    /// Each asked question's answer as laya returned it, plus `fired`; a question its precheck kept from laya is
    /// `{"type", "fired": false, "skipped": "precheck"}`. `null` when laya was not asked or failed.
    pub answers: Option<Value>,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
    /// The point has an enabled fail-closed question: a caller that cannot reach carebus-api fails closed.
    pub has_closed: bool,
    /// Per question, the text it was (or, skipped by its precheck, would have been) asked about. The playground's
    /// view of what a live turn sends; not logged, and the internal route leaves it out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub texts: Option<Value>,
}

impl Evaluation {
    fn plain(outcome: &str, has_closed: bool) -> Self {
        Self {
            outcome: outcome.into(),
            action: None,
            message: None,
            fired_keys: vec![],
            answers: None,
            latency_ms: None,
            error: None,
            has_closed,
            texts: None,
        }
    }

    /// Handoff/block fired, or failed closed: the guarded step must not happen.
    pub fn stops(&self) -> bool {
        match self.outcome.as_str() {
            "failed_closed" => true,
            "fired" => matches!(self.action.as_deref(), Some("handoff" | "block")),
            _ => false,
        }
    }

    /// The customer-facing text of a stopping verdict.
    pub fn customer_text(&self) -> String {
        match self.message.as_deref().map(str::trim) {
            Some(m) if !m.is_empty() => m.to_string(),
            _ if self.action.as_deref() == Some("block") => DEFAULT_BLOCK.into(),
            _ => DEFAULT_HANDOFF.into(),
        }
    }
}

/// One evaluation request.
pub struct EvalInput<'a> {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub point: &'a str,
    /// The request text (request point) or the state (tool and response points).
    pub state: &'a str,
    /// Request point: the customer's own words this turn, what `input = 'message'` questions read. `None`: they read
    /// `state`.
    pub message: Option<&'a str>,
    /// Tool point: only questions covering this tool are asked. `None` (the playground without a tool): all.
    pub tool: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub case_id: Option<&'a str>,
}

/// The `gates` binding: (provider id, base_url, sealed token, model).
type Binding = (Uuid, String, Option<String>, Option<String>);

/// The licences app and bundle the tenant's platform laya licence is minted on (licences 080: the care-bus bundle
/// includes laya).
pub const LAYA_APP: &str = "laya";
pub const LAYA_BUNDLE: &str = "care-bus";

/// The bearer of one evaluation's laya calls.
enum Bearer {
    /// The binding's stored token (tenant BYO), or none at all (no stored token, minter off).
    Fixed(Option<String>),
    /// The tenant's minted platform laya licence: refreshed and retried once on a 401.
    Minted(String),
}

impl Bearer {
    fn token(&self) -> Option<&str> {
        match self {
            Bearer::Fixed(t) => t.as_deref(),
            Bearer::Minted(t) => Some(t),
        }
    }
}

/// The tenant's platform laya licence, as a failed call's error text when it cannot be minted.
async fn mint_laya(st: &AppState, tenant: Uuid) -> Result<String, String> {
    st.strata_tokens
        .minted(LAYA_APP, Some(LAYA_BUNDLE), tenant)
        .await
        .map(|(t, _)| t)
        .map_err(|e| format!("{}: {e}", e.code()))
}

/// Evaluate one gate point and log its verdict. Errors are database errors (or reading the questions timed out);
/// laya failures, and running out of [`DECIDE_BUDGET`] while asking, are outcomes under the fail policy.
pub async fn evaluate(st: &AppState, i: &EvalInput<'_>) -> ApiResult<Evaluation> {
    let started = tokio::time::Instant::now();
    let decide_by = started + DECIDE_BUDGET;
    let (questions, binding) = match tokio::time::timeout_at(decide_by, load(st, i)).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "reading the gate questions timed out"
            )));
        }
    };
    let questions: Vec<Question> = questions
        .into_iter()
        .filter(|q| i.point != "tool" || i.tool.is_none_or(|t| q.covers(t)))
        .collect();
    // Nothing to ask: passes, and nothing is logged.
    if questions.is_empty() {
        return Ok(Evaluation::plain("pass", false));
    }
    let has_closed = questions.iter().any(|q| q.fail == "closed");
    let plan = Plan::new(&questions, i);
    let mut ev = match binding {
        None => Evaluation::plain("skipped", has_closed),
        // Every question kept from laya by its precheck: nothing to ask, nothing fires.
        Some(_) if plan.calls.is_empty() => decide(&plan, Map::new(), None, has_closed),
        Some(b) => {
            match tokio::time::timeout_at(decide_by, ask(st, i.tenant, &plan, b, has_closed)).await
            {
                Ok(ev) => ev,
                // Out of budget is a transport failure: closed questions hand off, open ones pass.
                Err(_) => failed(
                    &plan.asked,
                    has_closed,
                    "the gate evaluation timed out".into(),
                    Some(started.elapsed().as_millis() as u64),
                ),
            }
        }
    };
    ev.texts = Some(plan.texts());
    // The verdict row gets what is left of the whole budget (less a margin, so [`TurnGate::check`]'s own deadline
    // never drops a decided verdict); a slow write is dropped, never the verdict.
    if tokio::time::timeout_at(
        started + GATE_BUDGET - Duration::from_millis(100),
        log_verdict(st, i, &ev),
    )
    .await
    .is_err()
    {
        tracing::warn!(point = i.point, "writing the gate verdict timed out");
    }
    Ok(ev)
}

/// The enabled questions of the point and the `gates` binding.
async fn load(st: &AppState, i: &EvalInput<'_>) -> ApiResult<(Vec<Question>, Option<Binding>)> {
    Ok({
        let mut tx = begin_scoped(&st.db, i.tenant, Some(i.job_id)).await?;
        let questions: Vec<Question> = sqlx::query_as(concat!(
            "SELECT ",
            crate::gate_question_columns!(),
            " FROM gate_questions WHERE point = $1 AND enabled ORDER BY ordinal, key"
        ))
        .bind(i.point)
        .fetch_all(&mut *tx)
        .await?;
        let binding: Option<Binding> = sqlx::query_as(
            "SELECT p.id, p.base_url, p.secret_enc, u.model
               FROM llm_provider_uses u
               JOIN llm_providers p ON p.tenant_id = u.tenant_id AND p.id = u.provider_id
              WHERE u.\"use\" = 'gates' AND p.kind = 'laya'",
        )
        .fetch_optional(&mut *tx)
        .await?;
        (questions, binding)
    })
}

/// The questions of one evaluation by the text each reads: one laya call per distinct text, and those whose precheck
/// did not match (not asked).
struct Plan<'q> {
    calls: Vec<(String, Vec<&'q Question>)>,
    skipped: Vec<(String, &'q Question)>,
    /// The asked questions in question order (ordinal, key): the order the firing rules run in, so `fired_keys` and
    /// the leading question do not depend on how the calls were grouped.
    asked: Vec<&'q Question>,
}

impl<'q> Plan<'q> {
    fn new(questions: &'q [Question], i: &EvalInput<'_>) -> Self {
        let mut calls: Vec<(String, Vec<&'q Question>)> = vec![];
        let mut skipped = vec![];
        let mut asked = vec![];
        for q in questions {
            let text = q.text(i);
            if !q.prechecked(&text) {
                skipped.push((text, q));
                continue;
            }
            asked.push(q);
            match calls.iter_mut().find(|c| c.0 == text) {
                Some(c) => c.1.push(q),
                None => calls.push((text, vec![q])),
            }
        }
        Self {
            calls,
            skipped,
            asked,
        }
    }

    /// `{key: text}` of every question, asked or skipped.
    fn texts(&self) -> Value {
        let mut out = Map::new();
        for (text, qs) in &self.calls {
            for q in qs {
                out.insert(q.key.clone(), Value::String(text.clone()));
            }
        }
        for (text, q) in &self.skipped {
            out.insert(q.key.clone(), Value::String(text.clone()));
        }
        Value::Object(out)
    }
}

/// A call that produced no answers: `failed_closed` (action `handoff`, the first closed handoff question's message)
/// when any question asked is fail-closed, else `failed_open`. `has_closed` is the point's (any enabled question).
fn failed(
    asked: &[&Question],
    has_closed: bool,
    error: String,
    latency_ms: Option<u64>,
) -> Evaluation {
    let closed = asked.iter().any(|q| q.fail == "closed");
    let mut ev = Evaluation::plain(
        if closed {
            "failed_closed"
        } else {
            "failed_open"
        },
        has_closed,
    );
    if closed {
        ev.action = Some("handoff".into());
        ev.message = asked
            .iter()
            .filter(|q| q.fail == "closed" && q.action == "handoff")
            .find_map(|q| q.message.clone());
    }
    ev.error = Some(error);
    ev.latency_ms = latency_ms;
    ev
}

/// The laya calls (one per distinct text, concurrently) and the firing rules.
async fn ask(
    st: &AppState,
    tenant: Uuid,
    plan: &Plan<'_>,
    (provider, base_url, secret_enc, model): Binding,
    has_closed: bool,
) -> Evaluation {
    let asked = &plan.asked;
    let fail =
        |error: String, latency_ms: Option<u64>| failed(asked, has_closed, error, latency_ms);
    let mut bearer = match secret_enc.filter(|e| !e.is_empty()) {
        Some(enc) => {
            let opened = crypto::require(st.cfg.source_key.as_ref())
                .ok()
                .and_then(|k| open_token(k, provider, &enc).ok());
            match opened {
                Some(t) => Bearer::Fixed(Some(t)),
                None => return fail("the gates provider's token cannot be read".into(), None),
            }
        }
        None if st.strata_tokens.minting() => match mint_laya(st, tenant).await {
            Ok(t) => Bearer::Minted(t),
            Err(e) => return fail(e, None),
        },
        None => Bearer::Fixed(None),
    };
    let model = model.unwrap_or_else(|| DEFAULT_LAYA_MODEL.to_string());
    let started = Instant::now();
    // The stored base_url passed the literal guard on save; its host is resolved now, every answer checked and the
    // connection pinned (crate::netguard). A refused or unresolvable host is a failed call.
    let http = match netguard::check_and_pin(
        &laya::systemone_url(&base_url),
        st.cfg.source_fetch_allow_loopback,
    )
    .await
    {
        Ok((_, builder)) => match builder.build() {
            Ok(c) => c,
            Err(_) => return fail("the HTTP client could not be built".into(), None),
        },
        Err(r) => {
            return fail(
                format!("laya is not callable: {}", r.0),
                Some(started.elapsed().as_millis() as u64),
            );
        }
    };
    let mut results = call_all(&http, &base_url, bearer.token(), &model, plan).await;
    // A refused minted licence (expired early, revoked): drop it, mint a new one and ask once more.
    let refused = results
        .iter()
        .any(|r| matches!(r, Err(laya::LayaError::Status(401))));
    if refused && matches!(bearer, Bearer::Minted(_)) {
        st.strata_tokens.forget(tenant, LAYA_APP);
        bearer = match mint_laya(st, tenant).await {
            Ok(t) => Bearer::Minted(t),
            Err(e) => return fail(e, Some(started.elapsed().as_millis() as u64)),
        };
        results = call_all(&http, &base_url, bearer.token(), &model, plan).await;
    }
    let latency_ms = started.elapsed().as_millis() as u64;
    let mut answers = Map::new();
    for result in results {
        match result {
            Ok(a) => answers.extend(a),
            Err(e) => return fail(e.to_string(), Some(latency_ms)),
        }
    }
    decide(plan, answers, Some(latency_ms), has_closed)
}

/// One laya call per distinct text of the plan, concurrently.
async fn call_all(
    http: &reqwest::Client,
    base_url: &str,
    token: Option<&str>,
    model: &str,
    plan: &Plan<'_>,
) -> Vec<Result<Map<String, Value>, laya::LayaError>> {
    let calls = plan.calls.iter().map(|(text, qs)| {
        let mut questions = Map::new();
        for q in qs {
            questions.insert(q.key.clone(), q.laya());
        }
        laya::systemone(http, base_url, token, model, text, questions, LAYA_TIMEOUT)
    });
    futures_util::future::join_all(calls).await
}

/// The firing rules over laya's answers (every asked question must have a readable one, else the call failed) and
/// the precheck-skipped questions, which never fire.
fn decide(
    plan: &Plan<'_>,
    answers: Map<String, Value>,
    latency_ms: Option<u64>,
    has_closed: bool,
) -> Evaluation {
    let asked = &plan.asked;
    let mut out = Map::new();
    let mut fired: Vec<&Question> = vec![];
    for &q in asked {
        let Some(a) = answers.get(&q.key) else {
            return failed(
                asked,
                has_closed,
                format!("laya returned no answer for {}", q.key),
                latency_ms,
            );
        };
        let Some(f) = q.fires(a) else {
            return failed(
                asked,
                has_closed,
                format!("laya's answer for {} cannot be read", q.key),
                latency_ms,
            );
        };
        let mut a = a.clone();
        if let Value::Object(m) = &mut a {
            m.insert("fired".into(), Value::Bool(f));
        }
        out.insert(q.key.clone(), a);
        if f {
            fired.push(q);
        }
    }
    for (_, q) in &plan.skipped {
        out.insert(
            q.key.clone(),
            json!({"type": q.qtype, "fired": false, "skipped": "precheck"}),
        );
    }
    let strongest = fired.iter().map(|q| rank(&q.action)).max().unwrap_or(0);
    let lead = fired.iter().find(|q| rank(&q.action) == strongest);
    Evaluation {
        outcome: if fired.is_empty() { "pass" } else { "fired" }.into(),
        action: lead.map(|q| q.action.clone()),
        message: lead.and_then(|q| q.message.clone()),
        fired_keys: fired.iter().map(|q| q.key.clone()).collect(),
        answers: Some(Value::Object(out)),
        latency_ms,
        error: None,
        has_closed,
        texts: None,
    }
}

/// One `gate_verdicts` row. Best effort: a failed write is logged and never changes the verdict.
async fn log_verdict(st: &AppState, i: &EvalInput<'_>, ev: &Evaluation) {
    let r: ApiResult<()> = async {
        let mut tx = begin_scoped(&st.db, i.tenant, Some(i.job_id)).await?;
        sqlx::query(
            "INSERT INTO gate_verdicts (point, conversation_id, case_id, outcome, fired_keys, action, answers,
                                        latency_ms, error)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(i.point)
        .bind(i.conversation_id)
        .bind(i.case_id)
        .bind(&ev.outcome)
        .bind(json!(ev.fired_keys))
        .bind(&ev.action)
        .bind(&ev.answers)
        .bind(ev.latency_ms.map(|l| l.min(i32::MAX as u64) as i32))
        .bind(&ev.error)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = r {
        tracing::warn!(point = i.point, error = %e, "writing the gate verdict failed");
    }
}

/// The request point of a chat turn (web or Telegram): plugged into `Interaction::with_request_gate`.
pub struct TurnGate {
    pub st: AppState,
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub conversation_id: String,
}

/// The request point's state: this turn's message, and the request it completes (`req`) or the request so far
/// (`draft`, its slots still incomplete). A turn with neither is the message alone.
pub fn request_state(text: &str, draft: Option<&Draft>, req: Option<&QualifiedRequest>) -> String {
    if let Some(req) = req {
        let slots = serde_json::to_string(&req.slots).unwrap_or_default();
        return request_text(
            text,
            Some(RequestParts {
                first_message: &req.utterance,
                request_type: req.request_type.as_ref(),
                details: &slots,
                complete: true,
            }),
        );
    }
    match draft {
        Some(d) => {
            let slots = serde_json::to_string(&d.slots).unwrap_or_default();
            request_text(
                text,
                Some(RequestParts {
                    first_message: &d.utterance,
                    request_type: d.request_type.as_ref(),
                    details: &slots,
                    complete: false,
                }),
            )
        }
        None => request_text(text, None),
    }
}

/// What a request turn knows besides its message: the request it completes or the draft so far.
pub struct RequestParts<'a> {
    pub first_message: &'a str,
    pub request_type: &'a str,
    /// The slots as JSON.
    pub details: &'a str,
    /// The request is complete (`Details:`), else a draft (`Details so far:`).
    pub complete: bool,
}

/// The request text as a live turn sends it (also the playground's, from the parts an admin types).
pub fn request_text(text: &str, request: Option<RequestParts<'_>>) -> String {
    match request {
        None => format!("Customer message: {text}"),
        Some(r) => format!(
            "Customer message: {text}\nFirst message of this request: {}\nRequest type: {}\n{}: {}",
            r.first_message,
            r.request_type,
            if r.complete {
                "Details"
            } else {
                "Details so far"
            },
            r.details
        ),
    }
}

impl RequestGate for TurnGate {
    fn check<'a>(
        &'a self,
        text: &'a str,
        draft: Option<&'a Draft>,
        req: Option<&'a QualifiedRequest>,
    ) -> BoxFuture<'a, RequestDecision> {
        Box::pin(async move {
            let state = request_state(text, draft, req);
            let input = EvalInput {
                tenant: self.tenant,
                job_id: self.job_id,
                point: "request",
                state: &state,
                // `input = 'message'` questions read exactly what the customer wrote this turn.
                message: Some(text),
                tool: None,
                conversation_id: Some(&self.conversation_id),
                case_id: req.map(|r| r.case_id.as_str()),
            };
            // `evaluate` keeps its own budget; this is the backstop for the whole in-process evaluation.
            match tokio::time::timeout(GATE_BUDGET, evaluate(&self.st, &input)).await {
                Ok(Ok(ev)) if ev.stops() => RequestDecision::Stop(ev.customer_text()),
                Ok(Ok(ev)) if ev.outcome == "fired" => RequestDecision::Annotate(
                    json!({"fired_keys": ev.fired_keys, "answers": ev.answers}),
                ),
                Ok(Ok(_)) => RequestDecision::Pass,
                // The questions could not be read: the request gate is a safety gate, so it fails closed.
                Ok(Err(e)) => {
                    tracing::warn!(conversation = %self.conversation_id, error = ?e, "request gate failed; handing off");
                    RequestDecision::Stop(DEFAULT_HANDOFF.into())
                }
                // Out of budget with the questions unknown: a transport failure, failed closed.
                Err(_) => {
                    tracing::warn!(conversation = %self.conversation_id, "request gate timed out; handing off");
                    RequestDecision::Stop(DEFAULT_HANDOFF.into())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(qtype: &str, fire_when: &str, threshold: Option<f64>, values: Option<Value>) -> Question {
        Question {
            key: "k".into(),
            point: "request".into(),
            qtype: qtype.into(),
            instructions: "i".into(),
            criteria: json!({}),
            fire_when: fire_when.into(),
            threshold,
            fire_values: values,
            action: "handoff".into(),
            message: None,
            fail: "open".into(),
            tools: None,
            enabled: true,
            ordinal: 0,
            updated_at: Utc::now(),
            updated_by: "t".into(),
            input: "request".into(),
            precheck: None,
        }
    }

    #[test]
    fn firing_reads_the_value_of_each_type() {
        let noul = q("noul", "gte", Some(0.7), None);
        assert_eq!(
            noul.fires(&json!({"type": "noul", "noul": 0.73})),
            Some(true)
        );
        assert_eq!(
            noul.fires(&json!({"type": "noul", "noul": 0.2})),
            Some(false)
        );
        assert_eq!(noul.fires(&json!({"type": "noul"})), None);
        let score = q("score", "gte", Some(3.0), None);
        assert_eq!(
            score.fires(&json!({"score": 3, "confidence": 0.4})),
            Some(true)
        );
        assert_eq!(score.fires(&json!({"score": 2})), Some(false));
        let lt = q("noul", "lt", Some(0.5), None);
        assert_eq!(lt.fires(&json!({"noul": 0.1})), Some(true));
        let conf = q("choice", "gte", Some(0.5), None);
        assert_eq!(
            conf.fires(&json!({"choice": "billing", "confidence": 0.53})),
            Some(true)
        );
        let inq = q("choice", "in", None, Some(json!(["warranty", "returns"])));
        assert_eq!(inq.fires(&json!({"choice": "warranty"})), Some(true));
        assert_eq!(inq.fires(&json!({"choice": "faq"})), Some(false));
        let inq_t = q("choice", "in", Some(0.6), Some(json!(["yes"])));
        assert_eq!(
            inq_t.fires(&json!({"choice": "yes", "confidence": 0.74})),
            Some(true)
        );
        assert_eq!(
            inq_t.fires(&json!({"choice": "yes", "confidence": 0.48})),
            Some(false)
        );
        assert_eq!(
            inq_t.fires(&json!({"choice": "no", "confidence": 0.97})),
            Some(false)
        );
        assert_eq!(inq_t.fires(&json!({"choice": "yes"})), None);
    }

    #[test]
    fn tool_questions_cover_their_list_or_the_side_effect_tools() {
        let mut t = q("noul", "lt", Some(0.5), None);
        assert!(t.covers("order_issue_refund"));
        assert!(!t.covers("order_get_order"));
        t.tools = Some(json!(["order_get_order"]));
        assert!(t.covers("order_get_order"));
        assert!(!t.covers("order_issue_refund"));
    }

    fn input<'a>(state: &'a str, message: Option<&'a str>) -> EvalInput<'a> {
        EvalInput {
            tenant: Uuid::nil(),
            job_id: Uuid::nil(),
            point: "request",
            state,
            message,
            tool: None,
            conversation_id: None,
            case_id: None,
        }
    }

    #[test]
    fn a_question_reads_its_input_and_asks_only_when_its_precheck_matches() {
        let wrapped = "Customer message: where is my order?\nDetails: {\"order\": \"ORD-1042\"}";
        let mut m = q("choice", "in", Some(0.6), Some(json!(["yes"])));
        m.input = "message".into();
        assert_eq!(
            m.text(&input(wrapped, Some("where is my order?"))),
            "where is my order?"
        );
        // without the customer's words (tool/response callers) it reads the state
        assert_eq!(m.text(&input(wrapped, None)), wrapped);
        let r = q("noul", "gte", Some(0.5), None);
        assert_eq!(r.text(&input(wrapped, Some("where is my order?"))), wrapped);
        assert!(m.prechecked("anything"));
        m.precheck = Some(r"(?i)\d(?:[ -]?\d){3,}|\b(?:password|pin)".into());
        assert!(!m.prechecked("where is my order?"));
        assert!(m.prechecked("my PIN is 4821"));
        assert!(m.prechecked("card 4111 1111"));
        // a broken pattern never silences a question
        m.precheck = Some("(".into());
        assert!(m.prechecked("where is my order?"));
    }

    #[test]
    fn the_plan_groups_questions_by_text_and_sets_skipped_ones_aside() {
        let mut a = q("noul", "gte", Some(0.5), None);
        a.key = "a".into();
        let mut b = q("noul", "gte", Some(0.5), None);
        b.key = "b".into();
        b.input = "message".into();
        let mut c = q("noul", "gte", Some(0.5), None);
        c.key = "c".into();
        let mut d = q("noul", "gte", Some(0.5), None);
        d.key = "d".into();
        d.input = "message".into();
        d.precheck = Some("secret".into());
        let qs = [a, b, c, d];
        let i = input("Customer message: hi", Some("hi"));
        let plan = Plan::new(&qs, &i);
        let calls: Vec<(&str, Vec<&str>)> = plan
            .calls
            .iter()
            .map(|(t, qs)| (t.as_str(), qs.iter().map(|q| q.key.as_str()).collect()))
            .collect();
        assert_eq!(
            calls,
            vec![("Customer message: hi", vec!["a", "c"]), ("hi", vec!["b"])]
        );
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.texts()["d"], "hi");
        let ev = decide(
            &plan,
            serde_json::from_value(
                json!({"a": {"noul": 0.9}, "b": {"noul": 0.1}, "c": {"noul": 0.1}}),
            )
            .unwrap(),
            Some(5),
            false,
        );
        assert_eq!(ev.fired_keys, vec!["a".to_string()]);
        let answers = ev.answers.unwrap();
        assert_eq!(
            answers["d"],
            json!({"type": "noul", "fired": false, "skipped": "precheck"})
        );
        // all skipped: a pass without a call
        let only = [qs[3].clone()];
        let plan = Plan::new(&only, &i);
        assert!(plan.calls.is_empty());
        let ev = decide(&plan, Map::new(), None, false);
        assert_eq!(ev.outcome, "pass");
        assert!(ev.answers.unwrap()["d"]["skipped"] == "precheck");
    }

    #[test]
    fn the_request_text_is_what_a_live_turn_sends() {
        assert_eq!(request_text("hi", None), "Customer message: hi");
        assert_eq!(
            request_text(
                "ORD-1",
                Some(RequestParts {
                    first_message: "where is my order?",
                    request_type: "delivery",
                    details: "{}",
                    complete: false,
                })
            ),
            "Customer message: ORD-1\nFirst message of this request: where is my order?\nRequest type: delivery\nDetails so far: {}"
        );
    }

    #[test]
    fn handoff_outranks_block_outranks_annotate() {
        assert!(rank("handoff") > rank("block") && rank("block") > rank("annotate"));
    }
}
