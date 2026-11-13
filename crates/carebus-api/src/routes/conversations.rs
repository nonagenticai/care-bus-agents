//! Conversations of one job (`/api/jobs/{job}/conversations`). Every Redis key is the job's scoped key, and the
//! one database write (stamping `case_history.conversation_id`) runs under the job's RLS GUCs.
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use carebus::chat::{self, ChatEvent};
use carebus::interaction::{Draft, Extraction, Interaction, Qualifier, StartData, Turn};
use carebus::spaces::{ContextSpace, SpaceCatalog};
use carebus::{Bus, Scope};
use redis::AsyncCommands;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    chat_q::AnyQualifier,
    error::{ApiError, ApiJson, ApiPath, ApiResult},
    state::AppState,
    tenancy::{HumanMember, JobCtx, JobMember, begin_scoped},
};

/// Relative paths; nested under `/api/jobs/{job}` by `routes::all`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/conversations", post(create))
        .route("/conversations/{id}", get(one))
        .route("/conversations/{id}/messages", post(message))
}

#[derive(Deserialize)]
struct IdPath {
    id: String,
}

/// Everything a turn needs: the state, the job's scoped bus, and the job (for the RLS-scoped write). Built from the
/// request's `JobCtx`, or by the inbox sweeper from its scope.
#[derive(Clone)]
pub(crate) struct Conv {
    bus: Bus,
    tenant: Uuid,
    job_id: Uuid,
    st: AppState,
}

impl Conv {
    fn of(st: AppState, ctx: &JobCtx) -> Self {
        Conv {
            bus: st.bus.scoped(&ctx.scope()),
            tenant: ctx.t.tenant,
            job_id: ctx.job_id,
            st,
        }
    }

    /// A turn context for `scope` outside a request (the Telegram channel): the job id is read under the scope's
    /// tenant GUC.
    pub(crate) async fn for_scope(st: &AppState, scope: &Scope) -> anyhow::Result<Self> {
        Ok(Conv {
            bus: st.bus.scoped(scope),
            tenant: scope.tenant,
            job_id: job_id(st, scope).await?,
            st: st.clone(),
        })
    }
}

/// The customer every console (and Telegram) conversation acts as, unless configured otherwise.
pub const DEMO_CUSTOMER: &str = "C-10482";

/// A new conversation in `bus`'s scope for `customer`; returns its id.
pub(crate) async fn new_conversation(bus: &Bus, customer: &str) -> ApiResult<String> {
    let id = format!("conv-{}", uuid::Uuid::new_v4().simple());
    let key = chat::conv_key(bus, &id);
    let mut c = bus.conn();
    let _: () = c
        .hset_multiple(&key, &[("customer_id", customer), ("draft", "null")])
        .await?;
    let _: () = c.expire(&key, chat::TTL_SECS as i64).await?;
    Ok(id)
}

async fn create(
    _: HumanMember,
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let bus = s.bus.scoped(&ctx.scope());
    let id = new_conversation(&bus, DEMO_CUSTOMER).await?;
    let (name, tier) = customer_profile(&s, DEMO_CUSTOMER).await;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "conversation_id": id,
            "customer_id": DEMO_CUSTOMER,
            "customer_name": name,
            "customer_tier": tier,
        })),
    ))
}

/// Upper bound on the profile lookup: a slow demo-backends must not hold up the chat.
const PROFILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The customer's name and tier from the demo customers table (demo-backends, the same records the demo-data UI
/// edits). Best effort: any failure is logged and gives nulls, the conversation still starts.
async fn customer_profile(s: &AppState, customer: &str) -> (Option<String>, Option<String>) {
    let path = match crate::upstream::segment(customer, true) {
        Ok(seg) => format!("/api/admin/records/customers/{seg}"),
        Err(_) => return (None, None),
    };
    let call = crate::upstream::call(
        &s.http,
        "demo-backends",
        s.cfg.demo_backends_url.as_deref(),
        crate::upstream::Auth {
            token: s.cfg.demo_backends_token.as_deref(),
            ..crate::upstream::Auth::default()
        },
        reqwest::Method::GET,
        &path,
        None,
    );
    let body = match tokio::time::timeout(PROFILE_TIMEOUT, call).await {
        Ok(Ok(r)) if r.status.is_success() => r.body,
        Ok(Ok(r)) => {
            tracing::warn!(customer, status = %r.status, "customer profile lookup rejected; starting without it");
            return (None, None);
        }
        Ok(Err(e)) => {
            tracing::warn!(customer, error = %e, "customer profile lookup failed; starting without it");
            return (None, None);
        }
        Err(_) => {
            tracing::warn!(
                customer,
                "customer profile lookup timed out; starting without it"
            );
            return (None, None);
        }
    };
    let text = |k: &str| body[k].as_str().filter(|v| !v.is_empty()).map(String::from);
    (text("name"), text("tier"))
}

async fn exists(bus: &Bus, id: &str) -> ApiResult<()> {
    let e: bool = bus.conn().exists(chat::conv_key(bus, id)).await?;
    if e {
        Ok(())
    } else {
        Err(ApiError::NotFound(format!("conversation {id}")))
    }
}

async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let bus = s.bus.scoped(&ctx.scope());
    exists(&bus, &id).await?;
    let mut c = bus.conn();
    let raw: Vec<String> = c.lrange(chat::log_key(&bus, &id), 0, -1).await?;
    let messages: Vec<ChatEvent> = raw
        .iter()
        .filter_map(|r| match serde_json::from_str(r) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!(conversation = %id, error = %e, "skipping a corrupt chat log entry");
                None
            }
        })
        .collect();
    let cases: Vec<String> = c.lrange(chat::cases_key(&bus, &id), 0, -1).await?;
    let customer: Option<String> = c.hget(chat::conv_key(&bus, &id), "customer_id").await?;
    Ok(Json(json!({
        "conversation_id": id,
        "customer_id": customer.unwrap_or_else(|| DEMO_CUSTOMER.into()),
        "messages": messages,
        "case_ids": cases,
    })))
}

#[derive(Deserialize)]
struct Msg {
    text: String,
}

/// Accepts every message; one turn runs at a time per conversation. The message is logged and queued at once.
/// The sender that takes the lock drains the queue: everything waiting is qualified as ONE text (joined with
/// "\n"), repeatedly until nothing is left. Its response is the first drain's turn (200); a sender that finds
/// the conversation busy gets 202 `queued`, and the reply to its text arrives as a `chat` event.
async fn message(
    _: HumanMember,
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiJson(m): ApiJson<Msg>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let text = m.text.trim().to_string();
    if text.is_empty() {
        return Err(ApiError::BadRequest(
            "empty_message",
            "Type a message first.".into(),
        ));
    }
    let conv = Conv::of(s, &ctx);
    exists(&conv.bus, &id).await?;
    submit(conv, id, text).await
}

/// Accept `text` (non-empty, trimmed) into conversation `id`, which exists, and run the turn: the POST's internal
/// path, shared with the Telegram channel. 200 with the turn when this sender drains, 202 `queued` when busy.
pub(crate) async fn submit(conv: Conv, id: String, text: String) -> Reply {
    // Everything runs in its own task: a client disconnect drops this handler's future, and must not lose the
    // accepted text, abandon the lock, or stop between publishing the case and clearing the draft. The task
    // keeps draining after this response is sent.
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let first = accept(&conv, &id, &text).await;
        let token = match first {
            Ok(Some(token)) => token,
            Ok(None) => {
                let _ = tx.send(Ok(queued()));
                return;
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        drain(&conv, &id, token, Some(tx)).await;
    });
    rx.await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("conversation task failed: {e}")))?
}

fn queued() -> (StatusCode, Json<Value>) {
    (
        StatusCode::ACCEPTED,
        Json(json!({"turn": "queued", "text": "", "case_id": null})),
    )
}

/// Log + queue the text, then try to become the drainer. `Some(token)` when this sender holds the lock.
async fn accept(c: &Conv, id: &str, text: &str) -> ApiResult<Option<String>> {
    chat::accept_customer(&c.bus, &ChatEvent::new(id, "customer", text, None))
        .await
        .map_err(|e| redis_or(e, ApiError::Internal))?;
    Ok(chat::acquire_lock(&c.bus, id).await?)
}

type Reply = ApiResult<(StatusCode, Json<Value>)>;

/// Logged to the conversation when a turn fails (mirrors the POST's 503 "llm is unavailable").
pub const TURN_FAILED: &str = "Sorry, I could not process your last message: the assistant is unavailable right now. Please send it again.";

/// How often [`spawn_inbox_sweeper`] looks for stranded inboxes, and how many it drains per tick (per scope).
pub const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
pub const SWEEP_LIMIT: usize = 50;

/// The internal id of `scope`'s job, read under the scope's tenant GUC.
async fn job_id(s: &AppState, scope: &Scope) -> anyhow::Result<Uuid> {
    let mut tx = begin_scoped(&s.db, scope.tenant, None).await?;
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
        .bind(&scope.job)
        .fetch_optional(&mut *tx)
        .await?;
    id.ok_or_else(|| anyhow::anyhow!("scope {scope}: the tenant has no such job"))
}

/// Drains `scope`'s conversations whose texts wait with no lock holder (the holder's process died, or its lock
/// expired mid-turn). Returns how many conversations this tick drained.
pub async fn sweep_inboxes(s: &AppState, scope: &Scope, limit: usize) -> anyhow::Result<usize> {
    let bus = s.bus.scoped(scope);
    let stranded = chat::stranded_inboxes(&bus, limit).await?;
    if stranded.is_empty() {
        return Ok(0); // the common case: no database round trip
    }
    let conv = Conv {
        bus,
        tenant: scope.tenant,
        job_id: job_id(s, scope).await?,
        st: s.clone(),
    };
    let mut set = tokio::task::JoinSet::new();
    for id in stranded {
        // A sender may take the lock between the scan and here; then it drains, and this skips.
        if let Some(token) = chat::acquire_lock(&conv.bus, &id).await? {
            let c = conv.clone();
            set.spawn(async move { drain(&c, &id, token, None).await });
        }
    }
    let n = set.len();
    while set.join_next().await.is_some() {}
    Ok(n)
}

/// One scope's sweep loop; never returns.
async fn sweep_scope(s: AppState, scope: Scope) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        match sweep_inboxes(&s, &scope, SWEEP_LIMIT).await {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(scope = %scope, conversations = n, "drained stranded conversation inboxes")
            }
            Err(e) => {
                tracing::warn!(scope = %scope, error = %format!("{e:#}"), "sweeping conversation inboxes failed")
            }
        }
    }
}

/// The inbox sweeper, once per registered scope (carebus supervisor: follows the scope registry, restarts a
/// failed scope loop after 5 s).
pub fn spawn_inbox_sweeper(s: AppState) -> tokio::task::JoinHandle<()> {
    let root = s.bus.clone();
    tokio::spawn(async move {
        let r = carebus::supervisor::run_per_scope(root, "inbox-sweeper", move |_bus, scope| {
            sweep_scope(s.clone(), scope)
        })
        .await;
        if let Err(e) = r {
            tracing::error!(error = %format!("{e:#}"), "the inbox sweeper supervisor stopped");
        }
    })
}

/// Lock holder: qualify everything queued as one text, until the inbox stays empty. The first drain answers
/// `reply` (the holder's own POST); later drains reach the customer through the chat log and channel only.
async fn drain(
    c: &Conv,
    id: &str,
    mut token: String,
    mut reply: Option<tokio::sync::oneshot::Sender<Reply>>,
) {
    loop {
        let result: Reply = match chat::take_inbox(&c.bus, id).await {
            // Another holder already answered these texts (it drained them before this sender got the lock).
            Ok(texts) if texts.is_empty() => Ok(queued()),
            Ok(texts) => handle(c, id, &texts.join("\n"))
                .await
                .map(|j| (StatusCode::OK, j)),
            Err(e) => Err(redis_or(e, ApiError::Internal)),
        };
        if let Err(e) = &result {
            // The drained texts may belong to other senders (who got 202): tell the conversation, not only
            // the holder's POST. They are not requeued, so a model outage cannot loop.
            tracing::warn!(conversation = %id, error = ?e, "a turn failed; asking the customer to resend");
            if let Err(e) =
                chat::append_and_publish(&c.bus, &ChatEvent::new(id, "agent", TURN_FAILED, None))
                    .await
            {
                tracing::warn!(conversation = %id, error = %format!("{e:#}"), "logging the failed-turn notice failed");
            }
        }
        if let Some(tx) = reply.take() {
            let _ = tx.send(result);
        }
        match chat::finish_turn(&c.bus, id, &token).await {
            Ok(chat::Finish::Pending) => continue,
            Ok(chat::Finish::Released) => return,
            Ok(chat::Finish::Lost) => {
                // The lock expired under a long turn. Texts left behind need a holder: take it back if it is free.
                tracing::warn!(conversation = %id, "the conversation lock expired during a turn");
                match chat::acquire_lock(&c.bus, id).await {
                    Ok(Some(t)) => token = t,
                    _ => return,
                }
            }
            Err(e) => {
                tracing::warn!(conversation = %id, error = %format!("{e:#}"), "finishing the turn failed; the lock expires on its own");
                return;
            }
        }
    }
}

async fn handle(cv: &Conv, id: &str, text: &str) -> ApiResult<Json<Value>> {
    let mut c = cv.bus.conn();
    let raw: Option<String> = c.hget(chat::conv_key(&cv.bus, id), "draft").await?;
    let mut draft: Option<Draft> = match raw.as_deref().map(serde_json::from_str::<Option<Draft>>) {
        None => None,
        Some(Ok(d)) => d,
        Some(Err(e)) => {
            tracing::warn!(conversation = %id, error = %e, "stored draft is corrupt; starting a new one");
            None
        }
    };
    // The customer event is already logged (accept_customer); `text` may join several of them.
    // Qualified against the job's effective context spaces, read per turn: a space added in a release is a
    // request type the next message can be, without a redeploy.
    let spaces = scope_spaces(cv).await?;
    // The tenant's `qualify` binding, else the platform default (chat_q::qualifier_for).
    let qualifier = crate::chat_q::qualifier_for(&cv.st, cv.tenant).await?;
    // The request gate (spec 2026-10-18 §3): after qualifying, before `request.qualified` is published.
    let gate = std::sync::Arc::new(crate::gates::TurnGate {
        st: cv.st.clone(),
        tenant: cv.tenant,
        job_id: cv.job_id,
        conversation_id: id.to_string(),
    });
    let agent = Interaction::new(cv.bus.clone(), QualifierRef(qualifier))
        .with_spaces(spaces)
        .with_request_gate(gate);
    // Model usage of this turn into the usage ledger (always) and the billing outbox (only with METERING_EMIT_ENABLED).
    // The platform qualifier's calls carry this tenant (X-Effective-Tenant-Id) so its cost is billed to it; a
    // tenant's own `qualify` provider never gets the header (RigQualifier::with_client).
    let (turn, usage) = carebus::interaction::with_effective_tenant(
        cv.tenant,
        carebus::interaction::metered(agent.handle(&mut draft, text)),
    )
    .await;
    if !usage.is_empty() {
        let job_key = cv.bus.scope().map(|s| s.job.as_str()).unwrap_or_default();
        let a = crate::metering::Attribution {
            tenant: cv.tenant,
            job_id: cv.job_id,
            job_key,
            source: crate::metering::UsageSource::Chat {
                conversation_id: id,
            },
        };
        crate::metering::record_turn(&cv.st, &a, &usage).await;
    }
    let turn = turn.map_err(|e| {
        redis_or(e, |e| {
            tracing::warn!(conversation = %id, error = %format!("{e:#}"), "qualifying the message failed");
            ApiError::Upstream("llm")
        })
    })?;
    let draft_json = serde_json::to_string(&draft).map_err(anyhow::Error::from)?;
    if matches!(turn, Turn::Published(_)) {
        // The case is already on the bus: nothing below may turn this into an error response. Clear the
        // draft (retrying) so the next message cannot republish it, and carry on to link the case anyway.
        save_draft_published(&cv.bus, id, &draft_json).await;
    } else {
        let _: () = c
            .hset(chat::conv_key(&cv.bus, id), "draft", &draft_json)
            .await?;
    }
    let (kind, reply, case_id) = match turn {
        Turn::Ask(q) => ("ask", q, None),
        Turn::Unclear(q) => ("unclear", q, None),
        // A request gate handed off or blocked the request: nothing was published. The web's turn kinds have no
        // separate value for this; `ask` = the agent answered and the conversation stays open.
        Turn::Gated(q) => ("ask", q, None),
        Turn::Published(case) => {
            // The case IS published: nothing below may turn this into an error response.
            let mut linked = false;
            for attempt in 0..3 {
                match chat::link_case(&cv.bus, id, &case).await {
                    Ok(()) => {
                        linked = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(conversation = %id, case = %case, attempt, error = %format!("{e:#}"), "linking the case to the conversation failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            if !linked {
                tracing::error!(conversation = %id, case = %case, "case published but never linked; its reply will not reach this conversation");
            }
            // The recorder may have read the (then missing) link and still be inserting the row: stamp in the
            // background until the row is there, so the reply is not held up by it.
            let (c, conv_id, case_id) = (cv.clone(), id.to_string(), case.clone());
            tokio::spawn(async move { stamp_conversation(&c, &conv_id, &case_id).await });
            (
                "published",
                "Thanks, I have everything I need. Checking this for you now.".to_string(),
                Some(case),
            )
        }
    };
    if let Err(e) = chat::append_and_publish(
        &cv.bus,
        &ChatEvent::new(id, "agent", &reply, case_id.clone()),
    )
    .await
    {
        if case_id.is_none() {
            return Err(redis_or(e, ApiError::Internal));
        }
        tracing::warn!(conversation = %id, error = %format!("{e:#}"), "logging the agent reply failed after publishing");
    }
    Ok(Json(
        json!({"turn": kind, "text": reply, "case_id": case_id}),
    ))
}

/// How long [`stamp_conversation`] waits for the recorder's row: attempts x pause.
const STAMP_ATTEMPTS: u32 = 50;
const STAMP_PAUSE: std::time::Duration = std::time::Duration::from_millis(200);

/// `case_history.conversation_id` for a just-published case, in the job's RLS scope. The recorder inserts the row
/// concurrently and may have read the conversation link before it was written; if its INSERT commits after a
/// single UPDATE here, nothing would ever stamp the row (an open case gets no `request.resolved` COALESCE). So
/// retry until the row carries a conversation: stamped here, or by the recorder that already had the link.
async fn stamp_conversation(c: &Conv, id: &str, case: &str) {
    for attempt in 1..=STAMP_ATTEMPTS {
        match stamp_once(c, id, case).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(conversation = %id, case = %case, attempt, error = %e, "stamping case_history.conversation_id failed")
            }
        }
        tokio::time::sleep(STAMP_PAUSE).await;
    }
    tracing::warn!(conversation = %id, case = %case, "case_history row never carried a conversation; left to the recorder");
}

/// One attempt: `true` once the case's row carries a conversation. The UPDATE and the read are separate statements
/// (READ COMMITTED): a recorder INSERT that commits between them is seen by the read with `conversation_id` still
/// NULL, which is a retry, never a stop.
async fn stamp_once(c: &Conv, id: &str, case: &str) -> ApiResult<bool> {
    let mut tx = begin_scoped(&c.st.db, c.tenant, Some(c.job_id)).await?;
    sqlx::query(
        "UPDATE case_history SET conversation_id = $1 WHERE case_id = $2 AND conversation_id IS NULL",
    )
    .bind(id)
    .bind(case)
    .execute(&mut *tx)
    .await?;
    let carried: Option<bool> = sqlx::query_scalar(
        "SELECT conversation_id IS NOT NULL FROM case_history WHERE case_id = $1",
    )
    .bind(case)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(carried == Some(true))
}

/// A Redis error inside `e` maps like any Redis error (503 when Redis is unreachable); anything else is `other(e)`.
fn redis_or(e: anyhow::Error, other: impl FnOnce(anyhow::Error) -> ApiError) -> ApiError {
    match e.downcast::<redis::RedisError>() {
        Ok(r) => r.into(),
        Err(e) => other(e),
    }
}

/// Persist the cleared draft after a `Published` turn: 3 attempts, then log. Never fails the turn.
async fn save_draft_published(bus: &Bus, id: &str, draft_json: &str) {
    for attempt in 1..=3 {
        let r: redis::RedisResult<()> = bus
            .conn()
            .hset(chat::conv_key(bus, id), "draft", draft_json)
            .await;
        match r {
            Ok(()) => return,
            Err(e) if attempt < 3 => {
                tracing::warn!(conversation = %id, attempt, error = %e, "clearing the draft after publishing failed; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => {
                tracing::error!(conversation = %id, error = %e, "case published but the draft was never cleared; the next message may publish it again");
            }
        }
    }
}

/// The conversation's job's effective context spaces (both scope GUCs set), as the agents read them through
/// `GET /api/jobs/{job}/context-spaces`.
async fn scope_spaces(cv: &Conv) -> ApiResult<SpaceCatalog> {
    #[derive(sqlx::FromRow)]
    struct Row {
        key: String,
        area: String,
        request_type: String,
        required: Vec<String>,
        procedure: String,
    }
    let mut tx = begin_scoped(&cv.st.db, cv.tenant, Some(cv.job_id)).await?;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT key, area, request_type, required, procedure FROM eff_context_spaces ORDER BY key",
    )
    .fetch_all(&mut *tx)
    .await?;
    Ok(SpaceCatalog::new(
        rows.into_iter()
            .map(|r| ContextSpace {
                key: r.key,
                area: r.area,
                request_type: r.request_type,
                required: r.required,
                procedure: r.procedure,
            })
            .collect(),
    ))
}

/// `Interaction<Q>` takes the qualifier by value; share the resolved one.
struct QualifierRef(std::sync::Arc<AnyQualifier>);

impl Qualifier for QualifierRef {
    async fn extract(
        &self,
        text: &str,
        draft: Option<&Draft>,
        start: &StartData,
        spaces: &SpaceCatalog,
    ) -> anyhow::Result<Extraction> {
        self.0.extract(text, draft, start, spaces).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_errors_keep_their_mapping_and_the_rest_is_the_fallback() {
        let io =
            redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        let e = redis_or(anyhow::Error::from(io).context("publishing"), |_| {
            ApiError::Upstream("llm")
        });
        assert!(matches!(e, ApiError::Upstream("redis")), "{e:?}");
        let e = redis_or(anyhow::anyhow!("model timed out"), |_| {
            ApiError::Upstream("llm")
        });
        assert!(matches!(e, ApiError::Upstream("llm")), "{e:?}");
    }
}
