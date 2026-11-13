//! Background recorder: consumes every scope's area and tool streams as group `carebus-api-recorder` and writes
//! `case_history` (from `request.qualified` / `request.resolved`) and one `execute` audit row per tool response.
//! It runs once per registered scope (`supervisor::run_per_scope`, plan 13 Task 4) and writes under that scope's
//! RLS GUCs, so a row lands in the tenant and job of the stream it came from (the bus already dead-letters an
//! envelope whose stamp differs from its stream's scope). Delivery is at-least-once, so every write is idempotent,
//! and two replicas record each entry once.
use std::time::{Duration, Instant};

use carebus::domain::{
    QualifiedRequest, REQUEST_QUALIFIED, REQUEST_RESOLVED, Resolution, TOOL_RESPONSE, TOOLS,
    ToolResponse,
};
use carebus::{Bus, ConsumeOptions, Envelope, Scope};
use chrono::{DateTime, Utc};
use redis::AsyncCommands;
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::tenancy::begin_scoped;

pub const GROUP: &str = "carebus-api-recorder";
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How often a scope re-reads its areas; a change restarts the scope on the new streams.
const AREAS_REFRESH: Duration = Duration::from_secs(30);

/// One scope as the recorder writes it: the tenant, the job's internal id (resolved once per scope task), and the
/// scoped bus its Redis reads go through.
#[derive(Clone)]
pub struct ScopeIds {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub bus: Bus,
}

impl ScopeIds {
    /// Looks the job up inside the tenant (RLS); 404 `job` when the tenant has no such job.
    pub async fn resolve(st: &AppState, bus: Bus, scope: &Scope) -> ApiResult<ScopeIds> {
        let mut tx = begin_scoped(&st.db, scope.tenant, None).await?;
        let job_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
            .bind(&scope.job)
            .fetch_optional(&mut *tx)
            .await?;
        drop(tx);
        let job_id = job_id.ok_or_else(|| ApiError::NotFound("job".into()))?;
        Ok(ScopeIds {
            tenant: scope.tenant,
            job_id,
            bus,
        })
    }

    /// Both GUCs set, the same contract as `JobCtx::begin`.
    pub async fn begin(&self, db: &PgPool) -> ApiResult<Transaction<'static, Postgres>> {
        begin_scoped(db, self.tenant, Some(self.job_id)).await
    }
}

async fn conversation_of(ids: &ScopeIds, case_id: &str) -> ApiResult<Option<String>> {
    let v: Option<String> = ids
        .bus
        .conn()
        .get(carebus::chat::case_conv_key(&ids.bus, case_id))
        .await?;
    Ok(v)
}

fn at(env: &Envelope) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(env.published_at_ms as i64).unwrap_or_else(Utc::now)
}

fn text<T: serde::Serialize>(v: &T) -> ApiResult<String> {
    Ok(serde_json::to_value(v)
        .map_err(anyhow::Error::from)?
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// `name@version` from the envelope, split at the last `@` (a version never contains one). Anything else is None.
fn procedure_of(env: &Envelope) -> Option<(&str, &str)> {
    env.procedure
        .as_deref()?
        .rsplit_once('@')
        .filter(|(n, v)| !n.is_empty() && !v.is_empty())
}

pub async fn handle_customer(s: &AppState, ids: &ScopeIds, env: &Envelope) -> ApiResult<()> {
    match env.event() {
        Some(REQUEST_QUALIFIED) => {
            let q: QualifiedRequest = env.payload_as()?;
            let conv = conversation_of(ids, &q.case_id).await?;
            let mut tx = ids.begin(&s.db).await?;
            sqlx::query(
                "INSERT INTO case_history
                   (case_id, conversation_id, customer_id, request_type, area, context_space, started_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7)
                 ON CONFLICT (case_id) DO UPDATE
                   SET conversation_id = COALESCE(case_history.conversation_id, EXCLUDED.conversation_id)",
            )
            .bind(&q.case_id)
            .bind(conv)
            .bind(&q.customer_id)
            .bind(text(&q.request_type)?)
            .bind(&q.area)
            .bind(&q.context_space)
            .bind(at(env))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        Some(REQUEST_RESOLVED) => {
            let r: Resolution = env.payload_as()?;
            let conv = conversation_of(ids, &r.case_id).await?;
            let work = carebus::case::CaseStore::new(ids.bus.clone())
                .work(&r.case_id)
                .await?;
            // work_items counts attempts: every retry is its own work item (`work_id` ends in the attempt)
            let retries = work
                .iter()
                .filter(|w| w["attempt"].as_u64().unwrap_or(1) > 1)
                .count() as i32;
            // The procedure version the orchestrator stamped (`name@version`); absent on older envelopes and on
            // cases that never got a plan, which stay NULL (never guessed from the context space).
            let (proc_name, proc_version) = match procedure_of(env) {
                Some((n, v)) => (Some(n), Some(v)),
                None => (None, None),
            };
            let mut tx = ids.begin(&s.db).await?;
            let done = sqlx::query(
                "UPDATE case_history
                   SET outcome=$2, reason=$3, work_items=$4, retries=$5, resolved_at=$6,
                       duration_ms = (EXTRACT(EPOCH FROM ($6 - started_at)) * 1000)::bigint,
                       conversation_id = COALESCE(conversation_id, $7),
                       procedure_name = $8, procedure_version = $9
                 WHERE case_id=$1 AND outcome IS NULL",
            )
            .bind(&r.case_id)
            .bind(text(&r.outcome)?)
            .bind(&r.reason)
            .bind(work.len() as i32)
            .bind(retries)
            .bind(at(env))
            .bind(conv)
            .bind(proc_name)
            .bind(proc_version)
            .execute(&mut *tx)
            .await?;
            if done.rows_affected() == 0 {
                // Already resolved (a redelivery) is fine; no row at all means the qualified event
                // has not been recorded yet, so leave this entry pending and retry.
                let known: Option<(String,)> =
                    sqlx::query_as("SELECT case_id FROM case_history WHERE case_id=$1")
                        .bind(&r.case_id)
                        .fetch_optional(&mut *tx)
                        .await?;
                if known.is_none() {
                    return Err(anyhow::anyhow!(
                        "request.resolved for case {} arrived before its case_history row",
                        r.case_id
                    )
                    .into());
                }
            }
            tx.commit().await?;
        }
        _ => {}
    }
    Ok(())
}

pub async fn handle_tool(s: &AppState, ids: &ScopeIds, env: &Envelope) -> ApiResult<()> {
    if env.event() != Some(TOOL_RESPONSE) {
        return Ok(());
    }
    let r: ToolResponse = env.payload_as()?;
    let mut tx = ids.begin(&s.db).await?;
    sqlx::query(
        "INSERT INTO audit_events
           (timestamp, actor_id, actor_type, action_type, resource_type, resource_id, status, details, request_id)
         VALUES ($1,'tool-executor','system','execute','tool',$2,$3,$4,$5)
         ON CONFLICT (tenant_id, request_id) WHERE action_type = 'execute' DO NOTHING",
    )
    .bind(at(env))
    .bind(&r.mcp_tool)
    .bind(if r.ok { "success" } else { "failure" })
    .bind(json!({"case_id": env.correlation_id, "latency_ms": r.latency_ms, "error": r.error}))
    .bind(&r.call_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Kind {
    Customer,
    Tool,
}

/// Consume one stream of one scope forever: after any error (or a clean return) wait 1 s, doubling to 30 s, and
/// start again. The delay resets once a run has lasted longer than the maximum delay.
async fn supervise(s: AppState, ids: ScopeIds, stream: String, consumer: String, kind: Kind) {
    let mut delay = BACKOFF_START;
    loop {
        let started = Instant::now();
        let (st, sc) = (s.clone(), ids.clone());
        let res = ids
            .bus
            .consume(
                &stream,
                GROUP,
                &consumer,
                ConsumeOptions::default(),
                move |env| {
                    let (st, sc) = (st.clone(), sc.clone());
                    async move {
                        match kind {
                            Kind::Customer => handle_customer(&st, &sc, &env).await,
                            Kind::Tool => handle_tool(&st, &sc, &env).await,
                        }
                        .map_err(anyhow::Error::from)
                    }
                },
            )
            .await;
        match res {
            Err(e) => {
                tracing::error!(%stream, error = %format!("{e:#}"), "recorder failed; restarting")
            }
            Ok(()) => tracing::warn!(%stream, "recorder returned; restarting"),
        }
        if started.elapsed() > BACKOFF_MAX {
            delay = BACKOFF_START;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(BACKOFF_MAX);
    }
}

/// The scope's areas: the distinct `area` of its effective context spaces, in order of first appearance by key.
pub async fn scope_areas(s: &AppState, ids: &ScopeIds) -> ApiResult<Vec<String>> {
    let mut tx = ids.begin(&s.db).await?;
    let areas: Vec<String> =
        sqlx::query_scalar("SELECT area FROM eff_context_spaces GROUP BY area ORDER BY min(key)")
            .fetch_all(&mut *tx)
            .await?;
    Ok(areas)
}

/// One scope: resolve its job once, then one task per area stream (the areas of its context spaces) and per tool
/// stream of the scoped bus. The tasks live in a `JoinSet` owned by this future, so when the supervisor aborts the
/// scope (it was unregistered) dropping the future aborts every stream task with it. The areas are re-read every
/// 30 s; when they change this returns and the supervisor restarts the scope on the new streams.
async fn run_scope(s: AppState, bus: Bus, scope: Scope, consumer: String) -> anyhow::Result<()> {
    let ids = ScopeIds::resolve(&s, bus, &scope)
        .await
        .map_err(|e| anyhow::anyhow!("recorder scope {scope}: {e}"))?;
    let areas = scope_areas(&s, &ids)
        .await
        .map_err(|e| anyhow::anyhow!("recorder scope {scope}: {e}"))?;
    let streams = areas
        .iter()
        .map(|a| (ids.bus.streams.customer(a), Kind::Customer))
        .chain(TOOLS.iter().map(|t| (ids.bus.streams.tool(t), Kind::Tool)));
    let mut set = JoinSet::new();
    for (stream, kind) in streams {
        set.spawn(supervise(
            s.clone(),
            ids.clone(),
            stream,
            consumer.clone(),
            kind,
        ));
    }
    let mut tick = tokio::time::interval(AREAS_REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            j = set.join_next() => {
                if j.is_none() {
                    anyhow::bail!("recorder scope {scope}: every stream task ended");
                }
            }
            _ = tick.tick() => match scope_areas(&s, &ids).await {
                Ok(now) if now != areas => {
                    tracing::info!(%scope, old = ?areas, new = ?now, "recorder: areas changed; restarting the scope");
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(%scope, "recorder: re-reading the areas failed; keeping the streams: {e}"),
            },
        }
    }
}

/// The recorder for every registered scope, following the registry. Spawned by `main`; tests call the handlers or
/// spawn it against their own prefix. Aborting the handle stops every scope (the supervisor owns their tasks).
pub fn spawn(s: AppState, consumer: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let root = s.bus.clone();
        let res = carebus::supervisor::run_per_scope(root, "recorder", move |bus, scope| {
            run_scope(s.clone(), bus, scope, consumer.clone())
        })
        .await;
        if let Err(e) = res {
            tracing::error!(error = %format!("{e:#}"), "recorder supervisor ended");
        }
    })
}
