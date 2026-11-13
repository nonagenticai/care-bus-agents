//! Metering (W3 plan 15): the model usage of every chat turn goes into `usage_ledger` under the job's RLS GUCs
//! (migration 0011), and, only with `METERING_EMIT_ENABLED=true`, into `metering_outbox` for the billing drainer.
//! Ports commonpractice `host/metering-emit.ts`.
//!
//! Owner decision 2026-10-18: license-authz meters every LLM call and is THE billing meter. care-bus never bills.
//! The ledger exists so the owner can see token usage (`GET /api/jobs/{job}/usage`), for when the model moves to a
//! commercial GPU service, and it is written for every recorded turn, unconditionally.
//!
//! `METERING_EMIT_ENABLED` turns on the outbox rows and the drainer to the billing `billing:costs` stream. It MUST
//! stay off while license-authz meters the LLM URL: with both on, every call is billed twice.
//!
//! The chat model call runs in this API: `routes/conversations.rs` qualifies the turn with
//! `carebus::interaction::RigQualifier`, which records its token usage into the `carebus::interaction::metered`
//! sink. The `researcher` bin (plan 18) records each of its model calls through [`record_with`] (source `research`,
//! the run id as correlation). The write happens right after the call returns. No transaction is held across the
//! model's HTTP call. With emit on, the ledger row and its outbox row commit together or not at all.
use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use carebus::interaction::LlmUsage;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::licence::ActiveLicence;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// billing `cost_source` and `service_key` (EMITTER_GUIDE.md registry).
pub const COST_SOURCE: &str = "care-bus";
pub const SERVICE_KEY: &str = "care-bus";
/// `stage`: the operation that spent the tokens (the chat turn's qualification).
pub const STAGE_QUALIFY: &str = "qualify";
/// `stage` of a research run's model call (plan 18).
pub const STAGE_RESEARCH: &str = "research";
pub const DEFAULT_COSTS_STREAM: &str = "billing:costs";
pub const DEFAULT_OUTBOX_INTERVAL_MS: u64 = 30_000;

#[derive(Clone)]
pub struct MeteringConfig {
    /// `METERING_EMIT_ENABLED=true`: also write the outbox and run the drainer to billing. Off while license-authz
    /// meters the LLM URL (it is the billing meter; both on bills every call twice). The ledger is written regardless.
    pub emit_enabled: bool,
    /// `METERING_REDIS_URL`: the billing cost-bus Redis (not the care-bus Redis). Required when emitting.
    pub redis_url: String,
    /// `METERING_COSTS_STREAM` (default `billing:costs`).
    pub costs_stream: String,
    /// `METERING_OUTBOX_INTERVAL_MS` (default 30 000, at least 1 000): the drainer's tick per scope.
    pub outbox_interval: Duration,
}

impl Default for MeteringConfig {
    /// Off.
    fn default() -> Self {
        Self {
            emit_enabled: false,
            redis_url: String::new(),
            costs_stream: DEFAULT_COSTS_STREAM.into(),
            outbox_interval: Duration::from_millis(DEFAULT_OUTBOX_INTERVAL_MS),
        }
    }
}

/// The Redis URL may carry a password: never printed.
impl std::fmt::Debug for MeteringConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeteringConfig")
            .field("emit_enabled", &self.emit_enabled)
            .field("redis_url", &"<redacted>")
            .field("costs_stream", &self.costs_stream)
            .field("outbox_interval", &self.outbox_interval)
            .finish()
    }
}

impl MeteringConfig {
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let emit_enabled = get("METERING_EMIT_ENABLED").as_deref() == Some("true");
        let redis_url = get("METERING_REDIS_URL")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if emit_enabled && redis_url.is_empty() {
            anyhow::bail!(
                "METERING_EMIT_ENABLED=true needs METERING_REDIS_URL (the billing cost-bus Redis)"
            );
        }
        let costs_stream = get("METERING_COSTS_STREAM")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_COSTS_STREAM.into());
        let interval = match get("METERING_OUTBOX_INTERVAL_MS").filter(|v| !v.trim().is_empty()) {
            None => DEFAULT_OUTBOX_INTERVAL_MS,
            Some(v) => v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|ms| *ms >= 1_000)
                .context(
                    "METERING_OUTBOX_INTERVAL_MS must be a number of milliseconds, at least 1000",
                )?,
        };
        Ok(Self {
            emit_enabled,
            redis_url,
            costs_stream,
            outbox_interval: Duration::from_millis(interval),
        })
    }
}

/// What spent the tokens: a chat turn (its conversation) or a research run (plan 18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageSource<'a> {
    Chat { conversation_id: &'a str },
    Research { run_id: Uuid },
}

impl UsageSource<'_> {
    /// `usage_ledger.source`.
    pub fn ledger_source(&self) -> &'static str {
        match self {
            Self::Chat { .. } => "chat_qualifier",
            Self::Research { .. } => "research",
        }
    }
    /// The cost event's `stage`.
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Chat { .. } => STAGE_QUALIFY,
            Self::Research { .. } => STAGE_RESEARCH,
        }
    }
    /// The cost event's `correlation_id`: the conversation, or the research run.
    pub fn correlation_id(&self) -> String {
        match self {
            Self::Chat { conversation_id } => conversation_id.to_string(),
            Self::Research { run_id } => run_id.to_string(),
        }
    }
}

/// The scope and the turn or run a call's usage is attributed to.
pub struct Attribution<'a> {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub job_key: &'a str,
    pub source: UsageSource<'a>,
}

/// The flat string map of one event on `billing:costs` (EMITTER_GUIDE.md "The event shape"; CP
/// `buildCostEventFields`). USD is billing's to compute from `provider_pricing`, so `cost_usd` is `0`.
pub fn cost_event_fields(
    event_key: Uuid,
    at: DateTime<Utc>,
    a: &Attribution<'_>,
    u: &LlmUsage,
    licence: Option<&ActiveLicence>,
) -> BTreeMap<String, String> {
    let s = |v: &str| v.to_string();
    let licence_id = licence
        .and_then(|l| l.license_id.clone())
        .unwrap_or_default();
    let subject_id = licence
        .and_then(|l| l.subject_id.clone())
        .unwrap_or_default();
    BTreeMap::from([
        (s("tenant_id"), a.tenant.to_string()),
        (s("job"), s(a.job_key)),
        (s("license_id"), licence_id),
        (s("subject_id"), subject_id),
        (s("actor_id"), String::new()),
        (s("correlation_id"), a.source.correlation_id()),
        (s("stage"), s(a.source.stage())),
        (s("app_id"), String::new()),
        (s("service_key"), s(SERVICE_KEY)),
        (s("cost_source"), s(COST_SOURCE)),
        (s("provider"), u.provider.clone()),
        (s("model"), u.model.clone()),
        (s("served_model"), u.model.clone()),
        (s("input_tokens"), u.input_tokens.to_string()),
        (s("output_tokens"), u.output_tokens.to_string()),
        (s("cache_creation_input_tokens"), s("0")),
        (s("cache_read_input_tokens"), s("0")),
        (s("cost_usd"), s("0.00000000")),
        (s("latency_ms"), u.latency_ms.to_string()),
        (s("provider_request_id"), String::new()),
        (s("event_key"), event_key.to_string()),
        (s("usage_extra"), json!({"job": a.job_key}).to_string()),
        (
            s("timestamp"),
            at.to_rfc3339_opts(SecondsFormat::Millis, true),
        ),
    ])
}

fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// One ledger row per recorded call, always; plus its outbox row when emitting. One transaction under the job's GUCs.
pub async fn record(st: &AppState, a: &Attribution<'_>, usage: &[LlmUsage]) -> ApiResult<()> {
    let licence = st.licence.cached(a.tenant);
    record_with(
        &st.db,
        st.cfg.metering.emit_enabled,
        licence.as_ref(),
        a,
        usage,
    )
    .await
}

/// [`record`] without an `AppState` (the `researcher` bin): the runtime-role pool, the emit switch and the tenant's
/// licence if one is known (`None` leaves `license_id` / `subject_id` empty).
pub async fn record_with(
    db: &PgPool,
    emit: bool,
    licence: Option<&ActiveLicence>,
    a: &Attribution<'_>,
    usage: &[LlmUsage],
) -> ApiResult<()> {
    if usage.is_empty() {
        return Ok(());
    }
    let (conversation_id, research_run_id) = match a.source {
        UsageSource::Chat { conversation_id } => (Some(conversation_id), None),
        UsageSource::Research { run_id } => (None, Some(run_id)),
    };
    let mut tx = begin_scoped(db, a.tenant, Some(a.job_id)).await?;
    for u in usage {
        let key = Uuid::new_v4();
        let at = Utc::now();
        sqlx::query(
            "INSERT INTO usage_ledger
               (event_key, occurred_at, source, conversation_id, research_run_id, provider, model, input_tokens,
                output_tokens, calls, latency_ms, license_id, subject_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(key)
        .bind(at)
        .bind(a.source.ledger_source())
        .bind(conversation_id)
        .bind(research_run_id)
        .bind(&u.provider)
        .bind(&u.model)
        .bind(int(u.input_tokens))
        .bind(int(u.output_tokens))
        .bind(i32::try_from(u.calls).unwrap_or(i32::MAX))
        .bind(int(u.latency_ms))
        .bind(licence.and_then(|l| l.license_id.as_deref()))
        .bind(licence.and_then(|l| l.subject_id.as_deref()))
        .execute(&mut *tx)
        .await?;
        if !emit {
            continue;
        }
        let fields = cost_event_fields(key, at, a, u, licence);
        sqlx::query("INSERT INTO metering_outbox (event_key, fields) VALUES ($1, $2)")
            .bind(key)
            .bind(json!(fields))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// [`record`] for a chat turn that has already happened (its case may be on the bus): a failure is logged, never
/// returned, so metering cannot turn a completed turn into an error.
pub async fn record_turn(st: &AppState, a: &Attribution<'_>, usage: &[LlmUsage]) {
    if let Err(e) = record(st, a, usage).await {
        tracing::error!(
            tenant = %a.tenant, job = a.job_key, correlation = %a.source.correlation_id(), calls = usage.len(),
            error = ?e, "recording model usage failed; this turn is not metered"
        );
    }
}

/// Rows sent per scope per tick (CP `BATCH`).
const BATCH: i64 = 500;
const BACKOFF_BASE_SECS: f64 = 15.0;
const BACKOFF_MAX_SECS: f64 = 30.0 * 60.0;
/// MAXLEN for the costs stream, as CP's emitter.
const STREAM_MAXLEN: usize = 100_000;

/// CP `outboxBackoffMs`: 15 s doubling per failed attempt, capped at 30 min.
pub fn backoff_secs(attempts: i32) -> f64 {
    let exp = attempts.saturating_sub(1).clamp(0, 30);
    (BACKOFF_BASE_SECS * 2f64.powi(exp)).min(BACKOFF_MAX_SECS)
}

/// What one [`drain_once`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub sent: usize,
    pub failed: usize,
}

async fn xadd(
    conn: &mut redis::aio::ConnectionManager,
    stream: &str,
    fields: &serde_json::Value,
) -> anyhow::Result<()> {
    let obj = fields
        .as_object()
        .context("outbox fields are not an object")?;
    let mut cmd = redis::cmd("XADD");
    cmd.arg(stream)
        .arg("MAXLEN")
        .arg("~")
        .arg(STREAM_MAXLEN)
        .arg("*");
    for (k, v) in obj {
        cmd.arg(k.as_str()).arg(v.as_str().unwrap_or_default());
    }
    let _id: String = cmd.query_async(conn).await?;
    Ok(())
}

/// Send the scope's due outbox rows to the costs stream and mark them sent; a failed send backs off. The rows are
/// locked `FOR UPDATE SKIP LOCKED` for the tick, so two replicas never send the same row; a crash between XADD and
/// COMMIT re-sends it, which billing dedups on `event_key` (at-least-once).
pub async fn drain_once(
    st: &AppState,
    tenant: Uuid,
    job_id: Uuid,
    conn: &mut redis::aio::ConnectionManager,
) -> ApiResult<Drained> {
    let stream = st.cfg.metering.costs_stream.as_str();
    let mut tx = begin_scoped(&st.db, tenant, Some(job_id)).await?;
    let due: Vec<(i64, serde_json::Value, i32)> = sqlx::query_as(
        "SELECT id, fields, attempts FROM metering_outbox
          WHERE status = 'pending' AND next_attempt_at <= now()
          ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(BATCH)
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Drained::default();
    for (id, fields, attempts) in due {
        match xadd(conn, stream, &fields).await {
            Ok(()) => {
                sqlx::query(
                    "UPDATE metering_outbox SET status = 'sent', sent_at = now(), last_error = NULL
                      WHERE id = $1 AND status = 'pending'",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
                out.sent += 1;
            }
            Err(e) => {
                let attempts = attempts.saturating_add(1);
                tracing::warn!(%tenant, outbox_id = id, attempts, error = %format!("{e:#}"), "sending a cost event failed; backing off");
                sqlx::query(
                    "UPDATE metering_outbox
                        SET attempts = $2, last_error = $3, next_attempt_at = now() + make_interval(secs => $4)
                      WHERE id = $1 AND status = 'pending'",
                )
                .bind(id)
                .bind(attempts)
                .bind(format!("send failed: {e:#}"))
                .bind(backoff_secs(attempts))
                .execute(&mut *tx)
                .await?;
                out.failed += 1;
            }
        }
    }
    tx.commit().await?;
    Ok(out)
}

/// One scope: resolve its job once, then drain every `outbox_interval`. Errors are logged and the loop goes on.
async fn drain_scope(
    st: AppState,
    bus: carebus::Bus,
    scope: carebus::Scope,
    mut conn: redis::aio::ConnectionManager,
) -> anyhow::Result<()> {
    let ids = crate::recorder::ScopeIds::resolve(&st, bus, &scope)
        .await
        .map_err(|e| anyhow::anyhow!("metering drainer scope {scope}: {e}"))?;
    let mut tick = tokio::time::interval(st.cfg.metering.outbox_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match drain_once(&st, ids.tenant, ids.job_id, &mut conn).await {
            Ok(d) if d.sent > 0 || d.failed > 0 => {
                tracing::info!(scope = %scope, sent = d.sent, failed = d.failed, "metering outbox drained")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(scope = %scope, error = ?e, "draining the metering outbox failed")
            }
        }
    }
}

/// The outbox drainer for every registered scope (supervisor, as the recorder), sending to `METERING_REDIS_URL`.
/// `None` when METERING_EMIT_ENABLED is off: nothing is spawned and no connection is opened.
pub fn spawn_drainer(st: AppState) -> Option<tokio::task::JoinHandle<()>> {
    if !st.cfg.metering.emit_enabled {
        tracing::info!("metering outbox drainer disabled (METERING_EMIT_ENABLED is not true)");
        return None;
    }
    Some(tokio::spawn(async move {
        let conn = loop {
            let c = match redis::Client::open(st.cfg.metering.redis_url.as_str()) {
                Ok(client) => redis::aio::ConnectionManager::new(client).await,
                Err(e) => Err(e),
            };
            match c {
                Ok(c) => break c,
                Err(e) => {
                    tracing::error!(error = %e, "connecting to METERING_REDIS_URL failed; retrying in 30 s");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        };
        let root = st.bus.clone();
        let r = carebus::supervisor::run_per_scope(root, "metering-drainer", move |bus, scope| {
            drain_scope(st.clone(), bus, scope, conn.clone())
        })
        .await;
        if let Err(e) = r {
            tracing::error!(error = %format!("{e:#}"), "the metering drainer supervisor stopped");
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_15_s_and_caps_at_30_min() {
        assert_eq!(backoff_secs(1), 15.0);
        assert_eq!(backoff_secs(2), 30.0);
        assert_eq!(backoff_secs(3), 60.0);
        assert_eq!(backoff_secs(20), 1800.0);
        assert_eq!(backoff_secs(i32::MAX), 1800.0);
    }

    #[test]
    fn emitting_needs_a_redis_url_and_defaults_apply() {
        let off = MeteringConfig::from_lookup(&|_: &str| None).unwrap();
        assert!(!off.emit_enabled);
        assert_eq!(off.costs_stream, "billing:costs");
        assert_eq!(off.outbox_interval, Duration::from_millis(30_000));
        let err = MeteringConfig::from_lookup(&|k: &str| {
            (k == "METERING_EMIT_ENABLED").then(|| "true".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("METERING_REDIS_URL"), "{err}");
        let on = MeteringConfig::from_lookup(&|k: &str| match k {
            "METERING_EMIT_ENABLED" => Some("true".to_string()),
            "METERING_REDIS_URL" => Some("redis://:pw@billing-redis:6379/0".to_string()),
            _ => None,
        })
        .unwrap();
        assert!(on.emit_enabled);
        assert!(!format!("{on:?}").contains("pw@"));
    }

    #[test]
    fn cost_event_carries_tenant_job_and_the_ledger_key() {
        let key = Uuid::new_v4();
        let tenant: Uuid = "0195f48b-d000-7000-8000-000000000099".parse().unwrap();
        let a = Attribution {
            tenant,
            job_id: Uuid::new_v4(),
            job_key: "customer_care_agent",
            source: UsageSource::Chat {
                conversation_id: "conv-1",
            },
        };
        let u = LlmUsage {
            provider: "vllm".into(),
            model: "local-llm".into(),
            input_tokens: 812,
            output_tokens: 64,
            calls: 1,
            latency_ms: 900,
        };
        let f = cost_event_fields(key, Utc::now(), &a, &u, None);
        assert_eq!(f["tenant_id"], tenant.to_string());
        assert_eq!(f["job"], "customer_care_agent");
        assert_eq!(f["event_key"], key.to_string());
        assert_eq!(f["service_key"], "care-bus");
        assert_eq!(f["cost_source"], "care-bus");
        assert_eq!(f["input_tokens"], "812");
        assert_eq!(f["license_id"], "");
        assert_eq!(f["correlation_id"], "conv-1");
        assert_eq!(f["stage"], "qualify");
        let extra: serde_json::Value = serde_json::from_str(&f["usage_extra"]).unwrap();
        assert_eq!(extra["job"], "customer_care_agent");
    }

    #[test]
    fn a_research_call_is_staged_and_correlated_by_its_run() {
        let run = Uuid::new_v4();
        let a = Attribution {
            tenant: Uuid::new_v4(),
            job_id: Uuid::new_v4(),
            job_key: "legal_assistant",
            source: UsageSource::Research { run_id: run },
        };
        assert_eq!(a.source.ledger_source(), "research");
        let f = cost_event_fields(Uuid::new_v4(), Utc::now(), &a, &LlmUsage::default(), None);
        assert_eq!(f["stage"], "research");
        assert_eq!(f["correlation_id"], run.to_string());
        assert_eq!(
            UsageSource::Chat {
                conversation_id: "c"
            }
            .ledger_source(),
            "chat_qualifier"
        );
    }
}
