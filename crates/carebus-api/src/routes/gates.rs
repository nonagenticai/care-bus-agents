//! Laya gates (spec 2026-10-18 §3). Job-scoped admin routes under `/api/jobs/{job}/gates` (read = any tenant role;
//! write and the playground evaluate = tenant_admin: an evaluation calls laya with the tenant's token), and the internal `POST /api/internal/gates/evaluate` the worker and the
//! interaction daemon call with the service token.
//!
//! Service tokens are read-only because every write guard (`TenantAdmin`, `JobAdmin`, …) needs a tenant role and a
//! service caller has none. The internal route is the one route that takes a POST from a service token: its handler
//! accepts ONLY a service caller (a person's token is 403), it names its tenant in the body, and it writes nothing but
//! the verdict log of the evaluation it was asked for.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit;
use crate::auth::{Caller, CallerKind};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::gates::{
    self, EvalInput, Evaluation, MAX_STATE_CHARS, POINTS, Question, RequestParts, request_text,
};
use crate::state::AppState;
use crate::tenancy::{JobAdmin, JobMember, begin_scoped};

/// Relative paths; nested under `/api/jobs/{job}` by `routes::all`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gates", get(list))
        .route("/gates/evaluate", post(playground))
        .route("/gates/verdicts", get(verdicts))
        .route("/gates/{key}", put(save).delete(remove))
}

/// Absolute path; merged by `routes::all`.
pub fn internal_routes() -> Router<AppState> {
    Router::new().route("/api/internal/gates/evaluate", post(internal_evaluate))
}

#[derive(Serialize)]
pub struct ProviderState {
    pub bound: bool,
    pub model: Option<String>,
}

#[derive(Serialize)]
pub struct GatesView {
    pub questions: Vec<Question>,
    pub provider: ProviderState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionInput {
    point: String,
    #[serde(rename = "type")]
    qtype: String,
    instructions: String,
    criteria: Value,
    fire_when: String,
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(default)]
    fire_values: Option<Vec<String>>,
    action: String,
    #[serde(default)]
    message: Option<String>,
    fail: String,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    ordinal: i32,
    /// `request` (default) or `message` (request point only).
    #[serde(default = "request_input")]
    input: String,
    /// A regex (Rust `regex` syntax, at most [`MAX_PRECHECK_CHARS`]) that must match before laya is asked.
    #[serde(default)]
    precheck: Option<String>,
}

fn yes() -> bool {
    true
}

fn request_input() -> String {
    "request".into()
}

pub const MAX_PRECHECK_CHARS: usize = 500;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaygroundInput {
    point: String,
    /// The customer's message (request point), else the state as the worker or the daemon sends it.
    state: String,
    #[serde(default)]
    tool: Option<String>,
    /// Request point: the request the message completes or the draft so far, so the request text is built as a live
    /// turn builds it. Absent: a turn with neither (`Customer message: …`).
    #[serde(default)]
    request: Option<PlaygroundRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaygroundRequest {
    #[serde(rename = "type")]
    request_type: String,
    /// The first message of the request; default: the message itself.
    #[serde(default)]
    first_message: Option<String>,
    /// The slots.
    #[serde(default)]
    details: serde_json::Map<String, Value>,
    /// Complete (`Details:`), else a draft (`Details so far:`).
    #[serde(default)]
    complete: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InternalInput {
    tenant_id: Uuid,
    job: String,
    point: String,
    state: String,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    case_id: Option<String>,
    #[serde(default)]
    tool: Option<String>,
}

#[derive(Deserialize)]
pub struct VerdictQuery {
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct VerdictView {
    pub id: i64,
    pub point: String,
    pub conversation_id: Option<String>,
    pub case_id: Option<String>,
    pub outcome: String,
    pub fired_keys: Value,
    pub action: Option<String>,
    pub answers: Option<Value>,
    pub latency_ms: Option<i32>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct VerdictList {
    pub items: Vec<VerdictView>,
}

fn bad(code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest(code, msg.into())
}

fn valid_key(k: &str) -> bool {
    let b = k.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn nonblank(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.trim().is_empty())
}

/// Laya's criteria rules: noul exactly `{true, false}`, choice 2–255 options, score 2–10 distinct levels.
pub fn check_criteria(qtype: &str, c: &Value) -> Result<(), &'static str> {
    match qtype {
        "noul" => {
            let o = c
                .as_object()
                .ok_or("noul criteria are an object {true, false}")?;
            if o.len() != 2 || !o.contains_key("true") || !o.contains_key("false") {
                return Err("noul criteria have exactly the keys true and false");
            }
            if !o.values().all(nonblank) {
                return Err("describe both the true and the false case");
            }
            Ok(())
        }
        "choice" => {
            let o = c
                .as_object()
                .ok_or("choice criteria are an object of option: description")?;
            if !(2..=255).contains(&o.len()) {
                return Err("a choice question has 2 to 255 options");
            }
            if o.keys()
                .any(|k| k.trim().is_empty() || k.chars().count() > 100)
            {
                return Err("every option needs a name of at most 100 characters");
            }
            if !o.values().all(nonblank) {
                return Err("every option needs a description");
            }
            Ok(())
        }
        "score" => {
            let a = c
                .as_array()
                .ok_or("score criteria are an ordered list of levels")?;
            if !(2..=10).contains(&a.len()) {
                return Err("a score question has 2 to 10 levels");
            }
            if !a.iter().all(nonblank) {
                return Err("every level needs a label");
            }
            let mut seen: Vec<&str> = a.iter().filter_map(Value::as_str).map(str::trim).collect();
            seen.sort_unstable();
            seen.dedup();
            if seen.len() != a.len() {
                return Err("level labels must be distinct");
            }
            Ok(())
        }
        _ => Err("type must be noul, choice or score"),
    }
}

/// A question ready to store: the input validated and normalised.
struct Checked {
    i: QuestionInput,
    message: Option<String>,
    fire_values: Option<Value>,
    tools: Option<Value>,
    precheck: Option<String>,
}

fn check(i: QuestionInput) -> ApiResult<Checked> {
    if !POINTS.contains(&i.point.as_str()) {
        return Err(bad(
            "invalid_point",
            "point must be request, tool or response.",
        ));
    }
    if !matches!(i.qtype.as_str(), "noul" | "choice" | "score") {
        return Err(bad("invalid_type", "type must be noul, choice or score."));
    }
    if !(1..=2000).contains(&i.instructions.trim().chars().count()) {
        return Err(bad(
            "invalid_instructions",
            "instructions must be 1 to 2000 characters.",
        ));
    }
    check_criteria(&i.qtype, &i.criteria).map_err(|m| bad("invalid_criteria", m))?;
    let fire_values = match i.fire_when.as_str() {
        "in" => {
            if i.qtype != "choice" {
                return Err(bad(
                    "invalid_rule",
                    "`in` applies to choice questions only.",
                ));
            }
            // An optional threshold is the minimum confidence in the chosen option.
            if i.threshold
                .is_some_and(|t| !t.is_finite() || !(0.0..=1.0).contains(&t))
            {
                return Err(bad(
                    "invalid_rule",
                    "The threshold is a minimum confidence between 0 and 1.",
                ));
            }
            let values = i.fire_values.clone().unwrap_or_default();
            let options = i.criteria.as_object().map(|o| o.len()).unwrap_or(0);
            if values.is_empty() || values.len() > options {
                return Err(bad(
                    "invalid_rule",
                    "`in` needs at least one option that fires.",
                ));
            }
            if let Some(unknown) = values.iter().find(|v| {
                !i.criteria
                    .as_object()
                    .is_some_and(|o| o.contains_key(v.as_str()))
            }) {
                return Err(bad("invalid_rule", format!("{unknown} is not an option.")));
            }
            Some(json!(values))
        }
        "gte" | "lt" => {
            if i.fire_values.as_ref().is_some_and(|v| !v.is_empty()) {
                return Err(bad(
                    "invalid_rule",
                    "gte/lt take a threshold, not fire_values.",
                ));
            }
            let t = i
                .threshold
                .filter(|t| t.is_finite())
                .ok_or_else(|| bad("invalid_rule", "Set a threshold."))?;
            if i.qtype == "score" {
                let levels = i.criteria.as_array().map(Vec::len).unwrap_or(0) as f64;
                if t.fract() != 0.0 || t < 0.0 || t >= levels {
                    return Err(bad("invalid_rule", "A score threshold is a level index."));
                }
            } else if !(0.0..=1.0).contains(&t) {
                return Err(bad(
                    "invalid_rule",
                    "The threshold is a probability between 0 and 1.",
                ));
            }
            None
        }
        _ => return Err(bad("invalid_rule", "fire_when must be gte, lt or in.")),
    };
    if !matches!(i.action.as_str(), "handoff" | "block" | "annotate") {
        return Err(bad(
            "invalid_action",
            "action must be handoff, block or annotate.",
        ));
    }
    if !matches!(i.fail.as_str(), "open" | "closed") {
        return Err(bad("invalid_fail", "fail must be open or closed."));
    }
    let message = match i.message.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(m) if m.chars().count() <= 1000 => Some(m.to_string()),
        Some(_) => {
            return Err(bad(
                "invalid_message",
                "message must be at most 1000 characters.",
            ));
        }
    };
    let tools = match (&i.tools, i.point.as_str()) {
        (None, _) => None,
        (Some(t), _) if t.is_empty() => None,
        (Some(_), p) if p != "tool" => {
            return Err(bad(
                "invalid_tools",
                "tools apply to tool-point questions only.",
            ));
        }
        (Some(t), _) => {
            if t.len() > 50 || !t.iter().all(|n| valid_key(n)) {
                return Err(bad(
                    "invalid_tools",
                    "tools are up to 50 MCP tool names (a-z, 0-9, _).",
                ));
            }
            Some(json!(t))
        }
    };
    if !(0..=10000).contains(&i.ordinal) {
        return Err(bad("invalid_ordinal", "ordinal must be 0 to 10000."));
    }
    match i.input.as_str() {
        "request" => {}
        "message" if i.point == "request" => {}
        "message" => {
            return Err(bad(
                "invalid_input",
                "Only request-point questions can read the customer message alone.",
            ));
        }
        _ => return Err(bad("invalid_input", "input must be message or request.")),
    }
    let precheck = check_precheck(i.precheck.as_deref())?;
    Ok(Checked {
        i,
        message,
        fire_values,
        tools,
        precheck,
    })
}

/// A precheck as stored: blank = none; else at most [`MAX_PRECHECK_CHARS`] characters that compile as a regex.
pub fn check_precheck(p: Option<&str>) -> ApiResult<Option<String>> {
    let p = match p.map(str::trim) {
        None | Some("") => return Ok(None),
        Some(p) => p,
    };
    if p.chars().count() > MAX_PRECHECK_CHARS {
        return Err(bad(
            "invalid_precheck",
            format!("precheck must be at most {MAX_PRECHECK_CHARS} characters."),
        ));
    }
    if let Err(e) = regex::Regex::new(p) {
        return Err(bad(
            "invalid_precheck",
            format!("precheck is not a valid regular expression: {e}"),
        ));
    }
    Ok(Some(p.to_string()))
}

fn check_state(point: &str, state: &str) -> ApiResult<()> {
    if !POINTS.contains(&point) {
        return Err(bad(
            "invalid_point",
            "point must be request, tool or response.",
        ));
    }
    if state.trim().is_empty() || state.chars().count() > MAX_STATE_CHARS {
        return Err(bad(
            "invalid_state",
            format!("state must be 1 to {MAX_STATE_CHARS} characters."),
        ));
    }
    Ok(())
}

async fn list(JobMember(ctx): JobMember, State(s): State<AppState>) -> ApiResult<Json<GatesView>> {
    let mut tx = ctx.begin(&s.db).await?;
    let questions: Vec<Question> = sqlx::query_as(concat!(
        "SELECT ",
        crate::gate_question_columns!(),
        " FROM gate_questions ORDER BY point, ordinal, key"
    ))
    .fetch_all(&mut *tx)
    .await?;
    let binding: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT u.model FROM llm_provider_uses u
           JOIN llm_providers p ON p.tenant_id = u.tenant_id AND p.id = u.provider_id
          WHERE u.\"use\" = 'gates' AND p.kind = 'laya'",
    )
    .fetch_optional(&mut *tx)
    .await?;
    Ok(Json(GatesView {
        questions,
        provider: ProviderState {
            bound: binding.is_some(),
            model: binding.and_then(|(m,)| m),
        },
    }))
}

async fn save(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
    ApiJson(input): ApiJson<QuestionInput>,
) -> ApiResult<Json<Question>> {
    if !valid_key(&key) {
        return Err(bad(
            "invalid_key",
            "key: lowercase letters, digits and _, starting with a letter (max 64).",
        ));
    }
    let c = check(input)?;
    let mut tx = ctx.begin(&s.db).await?;
    let q: Question = sqlx::query_as(concat!(
        "INSERT INTO gate_questions (key, point, \"type\", instructions, criteria, fire_when, threshold, fire_values,
                                     action, message, fail, tools, enabled, ordinal, updated_at, updated_by, input,
                                     precheck)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, now(), $15, $16, $17)
         ON CONFLICT (tenant_id, job_id, key) DO UPDATE
            SET point = EXCLUDED.point, \"type\" = EXCLUDED.\"type\", instructions = EXCLUDED.instructions,
                criteria = EXCLUDED.criteria, fire_when = EXCLUDED.fire_when, threshold = EXCLUDED.threshold,
                fire_values = EXCLUDED.fire_values, action = EXCLUDED.action, message = EXCLUDED.message,
                fail = EXCLUDED.fail, tools = EXCLUDED.tools, enabled = EXCLUDED.enabled,
                ordinal = EXCLUDED.ordinal, updated_at = now(), updated_by = EXCLUDED.updated_by,
                input = EXCLUDED.input, precheck = EXCLUDED.precheck
         RETURNING ",
        crate::gate_question_columns!()
    ))
    .bind(&key)
    .bind(&c.i.point)
    .bind(&c.i.qtype)
    .bind(c.i.instructions.trim())
    .bind(&c.i.criteria)
    .bind(&c.i.fire_when)
    .bind(c.i.threshold)
    .bind(&c.fire_values)
    .bind(&c.i.action)
    .bind(&c.message)
    .bind(&c.i.fail)
    .bind(&c.tools)
    .bind(c.i.enabled)
    .bind(c.i.ordinal)
    .bind(&ctx.t.caller.actor)
    .bind(&c.i.input)
    .bind(&c.precheck)
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "update",
        "gate_question",
        &key,
        true,
        json!({"point": q.point, "type": q.qtype, "action": q.action, "fail": q.fail, "enabled": q.enabled,
               "input": q.input, "precheck": q.precheck}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(q))
}

async fn remove(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    let mut tx = ctx.begin(&s.db).await?;
    let gone = sqlx::query("DELETE FROM gate_questions WHERE key = $1")
        .bind(&key)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if gone == 0 {
        return Err(ApiError::NotFound("gate question".into()));
    }
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "delete",
        "gate_question",
        &key,
        true,
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The playground: evaluates `state` at `point` like a live turn and logs a verdict with no conversation or case.
/// At the request point `state` is the customer's message: `input = 'message'` questions read it as is, the others
/// the request text a live turn builds from it (and `request`, when given). The answer's `texts` shows, per
/// question, what was sent. tenant_admin only, like the write routes: it spends a laya call on the tenant's token.
async fn playground(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(input): ApiJson<PlaygroundInput>,
) -> ApiResult<Json<Evaluation>> {
    check_state(&input.point, &input.state)?;
    if input.point != "request" && input.request.is_some() {
        return Err(bad(
            "invalid_request",
            "request applies to the request point only.",
        ));
    }
    let tool = input
        .tool
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let request_state = match (input.point.as_str(), &input.request) {
        ("request", None) => Some(request_text(&input.state, None)),
        ("request", Some(r)) => {
            let details = serde_json::to_string(&r.details).unwrap_or_default();
            Some(request_text(
                &input.state,
                Some(RequestParts {
                    first_message: r.first_message.as_deref().unwrap_or(&input.state),
                    request_type: &r.request_type,
                    details: &details,
                    complete: r.complete,
                }),
            ))
        }
        _ => None,
    };
    let message = (input.point == "request").then_some(input.state.as_str());
    let ev = gates::evaluate(
        &s,
        &EvalInput {
            tenant: ctx.t.tenant,
            job_id: ctx.job_id,
            point: &input.point,
            state: request_state.as_deref().unwrap_or(&input.state),
            message,
            tool,
            conversation_id: None,
            case_id: None,
        },
    )
    .await?;
    Ok(Json(ev))
}

/// The verdict log, newest first (`?limit=`, default 50, at most 200).
async fn verdicts(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<VerdictQuery>,
) -> ApiResult<Json<VerdictList>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let mut tx = ctx.begin(&s.db).await?;
    let items: Vec<VerdictView> = sqlx::query_as(
        "SELECT id, point, conversation_id, case_id, outcome, fired_keys, action, answers, latency_ms, error,
                created_at
           FROM gate_verdicts ORDER BY created_at DESC, id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(VerdictList { items }))
}

/// The worker's and the interaction daemon's gate call. Service token only; the tenant is named in the body.
async fn internal_evaluate(
    caller: Caller,
    State(s): State<AppState>,
    ApiJson(input): ApiJson<InternalInput>,
) -> ApiResult<Json<Evaluation>> {
    if !matches!(caller.kind, CallerKind::Service) {
        return Err(ApiError::Forbidden("service token"));
    }
    check_state(&input.point, &input.state)?;
    if input.tenant_id.is_nil() {
        return Err(bad("invalid_tenant", "tenant_id must be a non-nil uuid."));
    }
    let job_id = {
        let mut tx = begin_scoped(&s.db, input.tenant_id, None).await?;
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
            .bind(input.tenant_id)
            .fetch_optional(&mut *tx)
            .await?;
        match status.as_deref() {
            None => return Err(ApiError::NotFound("tenant".into())),
            Some("active") => {}
            Some(_) => return Err(ApiError::TenantSuspended),
        }
        let job: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
            .bind(&input.job)
            .fetch_optional(&mut *tx)
            .await?;
        job.ok_or_else(|| ApiError::NotFound("job".into()))?
    };
    s.licence.check(input.tenant_id).await?;
    let mut ev = gates::evaluate(
        &s,
        &EvalInput {
            tenant: input.tenant_id,
            job_id,
            point: &input.point,
            state: &input.state,
            message: None,
            tool: input.tool.as_deref(),
            conversation_id: input.conversation_id.as_deref(),
            case_id: input.case_id.as_deref(),
        },
    )
    .await?;
    // The texts are the playground's; the worker and the daemon sent their own state.
    ev.texts = None;
    Ok(Json(ev))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    #[test]
    fn criteria_follow_lay_as_rules() {
        assert!(check_criteria("noul", &json!({"true": "a", "false": "b"})).is_ok());
        assert!(check_criteria("noul", &json!({"true": "a", "false": "b", "maybe": "c"})).is_err());
        assert!(check_criteria("noul", &json!({"yes": "a", "no": "b"})).is_err());
        assert!(check_criteria("noul", &json!({"true": "a", "false": " "})).is_err());
        assert!(check_criteria("choice", &json!({"a": "x", "b": "y"})).is_ok());
        assert!(check_criteria("choice", &json!({"a": "x"})).is_err());
        let many: Map<String, Value> = (0..256).map(|i| (format!("o{i}"), json!("d"))).collect();
        assert!(check_criteria("choice", &Value::Object(many)).is_err());
        assert!(check_criteria("score", &json!(["Calm", "Frustrated"])).is_ok());
        assert!(check_criteria("score", &json!(["Calm"])).is_err());
        assert!(
            check_criteria(
                "score",
                &json!(["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"])
            )
            .is_err()
        );
        assert!(check_criteria("score", &json!(["Calm", "Calm"])).is_err());
    }

    #[test]
    fn a_precheck_is_a_short_valid_regex_or_nothing() {
        assert_eq!(check_precheck(None).unwrap(), None);
        assert_eq!(check_precheck(Some("  ")).unwrap(), None);
        assert_eq!(
            check_precheck(Some(r" (?i)\d{4} ")).unwrap().as_deref(),
            Some(r"(?i)\d{4}")
        );
        assert!(check_precheck(Some("(unclosed")).is_err());
        assert!(check_precheck(Some(&"a".repeat(MAX_PRECHECK_CHARS + 1))).is_err());
        assert!(check_precheck(Some(&"a".repeat(MAX_PRECHECK_CHARS))).is_ok());
    }

    #[test]
    fn keys_are_lowercase_identifiers() {
        assert!(valid_key("emergency") && valid_key("pii_leak2"));
        assert!(!valid_key("Emergency") && !valid_key("2x") && !valid_key("") && !valid_key("a-b"));
        assert!(!valid_key(&"a".repeat(65)));
    }
}
