//! Orchestrator case state in one Redis hash per case, `case:{id}`.
//!
//! Fields: `request`, `plan`, `phase`, `pending:{phase}` (steps still open),
//! `result:{as}`, `work:{work_id}` (status, for outstanding/finished views),
//! `resolved`. Every write is safe to repeat, because a redelivered message
//! replays the handler that made it.

use std::time::Duration;

use anyhow::{Context, Result};
use redis::AsyncCommands;
use redis::Script;
use serde_json::{Map, Value};

use crate::bus::Bus;
use crate::domain::{Plan, QualifiedRequest, Resolution};

/// Store a result once and count the phase down once.
/// Returns the remaining count, or -1 if this result was already recorded.
const RECORD_RESULT: &str = r#"
if redis.call('HSETNX', KEYS[1], ARGV[1], ARGV[2]) == 1 then
  return redis.call('HINCRBY', KEYS[1], ARGV[3], -1)
end
return -1
"#;

/// Move `phase` from ARGV[1] to ARGV[2] only if it is still ARGV[1].
const ADVANCE_PHASE: &str = r#"
if redis.call('HGET', KEYS[1], 'phase') == ARGV[1] then
  redis.call('HSET', KEYS[1], 'phase', ARGV[2])
  return 1
end
return 0
"#;

/// Create the case hash and its deadline entry in one step, so a crash cannot leave a case with no
/// deadline. KEYS: case hash, deadline zset. ARGV: request json, due ms, ttl s, case id. Returns 1 if created.
const CREATE_CASE: &str = r#"
if redis.call('HSETNX', KEYS[1], 'request', ARGV[1]) == 1 then
  redis.call('HSETNX', KEYS[1], 'phase', 0)
  redis.call('EXPIRE', KEYS[1], ARGV[3])
  redis.call('ZADD', KEYS[2], 'NX', ARGV[2], ARGV[4])
  return 1
end
return 0
"#;

/// Lease an overdue case: only if its score is due, push the score `lease` ms out and return 1.
/// KEYS: deadline zset. ARGV: case id, now ms, lease ms.
const LEASE_OVERDUE: &str = r#"
local s = redis.call('ZSCORE', KEYS[1], ARGV[1])
if s and tonumber(s) <= tonumber(ARGV[2]) then
  redis.call('ZADD', KEYS[1], tonumber(ARGV[2]) + tonumber(ARGV[3]), ARGV[1])
  return 1
end
return 0
"#;

/// How long a replica owns the sweep of one case; a crashed sweep is retried after this.
pub const SWEEP_LEASE: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct CaseStore {
    bus: Bus,
    /// How long a newly created case may stay unresolved before the sweeper fails it.
    deadline: Duration,
}

/// Default case deadline (`CASE_DEADLINE_MS`).
pub const DEFAULT_CASE_DEADLINE: Duration = Duration::from_secs(120);

impl CaseStore {
    pub fn new(bus: Bus) -> Self {
        Self {
            bus,
            deadline: DEFAULT_CASE_DEADLINE,
        }
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Sorted set of unresolved case ids, scored by deadline (epoch ms).
    fn deadline_key(&self) -> String {
        self.bus.streams.key("cases:deadline")
    }

    /// Redis server time in epoch ms, so replicas agree despite clock skew.
    pub async fn now_ms(&self) -> Result<i64> {
        let (secs, micros): (i64, i64) =
            redis::cmd("TIME").query_async(&mut self.bus.conn()).await?;
        Ok(secs * 1000 + micros / 1000)
    }

    /// Unresolved cases whose deadline is at or before `now_ms` (oldest first).
    pub async fn overdue(&self, now_ms: i64, limit: usize) -> Result<Vec<String>> {
        Ok(self
            .bus
            .conn()
            .zrangebyscore_limit(self.deadline_key(), "-inf", now_ms, 0, limit as isize)
            .await?)
    }

    /// Lease an overdue case for `SWEEP_LEASE`: true for exactly one caller. The entry stays in the set
    /// until the case is resolved, so a sweep that dies mid-way is retried when the lease runs out.
    pub async fn claim_overdue(&self, case_id: &str, now_ms: i64) -> Result<bool> {
        let leased: i64 = Script::new(LEASE_OVERDUE)
            .key(self.deadline_key())
            .arg(case_id)
            .arg(now_ms)
            .arg(SWEEP_LEASE.as_millis() as i64)
            .invoke_async(&mut self.bus.conn())
            .await?;
        Ok(leased == 1)
    }

    /// Stop tracking a case (resolved, or its hash is gone).
    pub async fn drop_deadline(&self, case_id: &str) -> Result<()> {
        let _: i64 = self.bus.conn().zrem(self.deadline_key(), case_id).await?;
        Ok(())
    }

    /// First resolution wins. Returns the stored one, which every caller must publish.
    pub async fn set_resolution(&self, case_id: &str, res: &Resolution) -> Result<Resolution> {
        let mut c = self.bus.conn();
        let key = self.key(case_id);
        let _: bool = c
            .hset_nx(&key, "resolution", serde_json::to_string(res)?)
            .await?;
        let raw: String = c.hget(&key, "resolution").await?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn key(&self, case_id: &str) -> String {
        self.bus.streams.key(&format!("case:{case_id}"))
    }

    /// Returns false when the case already existed (a duplicate delivery). Hash, phase, expiry and
    /// deadline entry are written by one Lua script, so a crash cannot leave a case without a deadline.
    pub async fn create(&self, req: &QualifiedRequest) -> Result<bool> {
        let due = self.now_ms().await? + self.deadline.as_millis() as i64;
        let created: i64 = Script::new(CREATE_CASE)
            .key(self.key(&req.case_id))
            .key(self.deadline_key())
            .arg(serde_json::to_string(req)?)
            .arg(due)
            .arg(7 * 86_400)
            .arg(&req.case_id)
            .invoke_async(&mut self.bus.conn())
            .await?;
        Ok(created == 1)
    }

    pub async fn request(&self, case_id: &str) -> Result<QualifiedRequest> {
        let raw: Option<String> = self.bus.conn().hget(self.key(case_id), "request").await?;
        serde_json::from_str(&raw.with_context(|| format!("unknown case {case_id}"))?)
            .map_err(Into::into)
    }

    pub async fn plan(&self, case_id: &str) -> Result<Option<Plan>> {
        let raw: Option<String> = self.bus.conn().hget(self.key(case_id), "plan").await?;
        raw.map(|r| serde_json::from_str(&r))
            .transpose()
            .map_err(Into::into)
    }

    /// First plan wins, so two orchestrators racing on one case agree.
    pub async fn set_plan(&self, case_id: &str, plan: &Plan) -> Result<Plan> {
        let mut c = self.bus.conn();
        let _: bool = c
            .hset_nx(self.key(case_id), "plan", serde_json::to_string(plan)?)
            .await?;
        self.plan(case_id).await?.context("plan vanished")
    }

    pub async fn phase(&self, case_id: &str) -> Result<usize> {
        let p: Option<usize> = self.bus.conn().hget(self.key(case_id), "phase").await?;
        Ok(p.unwrap_or(0))
    }

    pub async fn advance_phase(&self, case_id: &str, from: usize, to: usize) -> Result<bool> {
        let mut c = self.bus.conn();
        let moved: i64 = Script::new(ADVANCE_PHASE)
            .key(self.key(case_id))
            .arg(from)
            .arg(to)
            .invoke_async(&mut c)
            .await?;
        Ok(moved == 1)
    }

    pub async fn init_pending(&self, case_id: &str, phase: usize, steps: usize) -> Result<()> {
        let _: bool = self
            .bus
            .conn()
            .hset_nx(self.key(case_id), format!("pending:{phase}"), steps)
            .await?;
        Ok(())
    }

    pub async fn remaining(&self, case_id: &str, phase: usize) -> Result<i64> {
        let r: Option<i64> = self
            .bus
            .conn()
            .hget(self.key(case_id), format!("pending:{phase}"))
            .await?;
        Ok(r.unwrap_or(i64::MAX))
    }

    pub async fn has_result(&self, case_id: &str, as_name: &str) -> Result<bool> {
        Ok(self
            .bus
            .conn()
            .hexists(self.key(case_id), format!("result:{as_name}"))
            .await?)
    }

    pub async fn record_result(
        &self,
        case_id: &str,
        phase: usize,
        as_name: &str,
        result: &Value,
    ) -> Result<i64> {
        let mut c = self.bus.conn();
        let remaining: i64 = Script::new(RECORD_RESULT)
            .key(self.key(case_id))
            .arg(format!("result:{as_name}"))
            .arg(serde_json::to_string(result)?)
            .arg(format!("pending:{phase}"))
            .invoke_async(&mut c)
            .await?;
        Ok(remaining)
    }

    /// `request` (slots + customer_id) plus every recorded result by name.
    pub async fn context(&self, req: &QualifiedRequest) -> Result<Map<String, Value>> {
        let mut ctx = self.results(&req.case_id).await?;
        let mut request = serde_json::to_value(&req.slots)?;
        request["customer_id"] = Value::String(req.customer_id.clone());
        ctx.insert("request".into(), request);
        Ok(ctx)
    }

    pub async fn results(&self, case_id: &str) -> Result<Map<String, Value>> {
        let all: Vec<(String, String)> = self.bus.conn().hgetall(self.key(case_id)).await?;
        let mut out = Map::new();
        for (field, raw) in all {
            if let Some(name) = field.strip_prefix("result:") {
                out.insert(name.into(), serde_json::from_str(&raw)?);
            }
        }
        Ok(out)
    }

    pub async fn set_work_status(
        &self,
        case_id: &str,
        work_id: &str,
        status: &Value,
    ) -> Result<()> {
        let _: () = self
            .bus
            .conn()
            .hset(
                self.key(case_id),
                format!("work:{work_id}"),
                status.to_string(),
            )
            .await?;
        Ok(())
    }

    /// Work items with their latest status, for outstanding/finished views.
    pub async fn work(&self, case_id: &str) -> Result<Vec<Value>> {
        let all: Vec<(String, String)> = self.bus.conn().hgetall(self.key(case_id)).await?;
        all.into_iter()
            .filter(|(f, _)| f.starts_with("work:"))
            .map(|(_, raw)| serde_json::from_str(&raw).map_err(Into::into))
            .collect()
    }

    /// Raw infrastructure error that failed the case (operators only; the customer gets a short category).
    pub async fn set_error(&self, case_id: &str, error: &str) -> Result<()> {
        let _: () = self
            .bus
            .conn()
            .hset(self.key(case_id), "error", error)
            .await?;
        Ok(())
    }

    pub async fn error(&self, case_id: &str) -> Result<Option<String>> {
        Ok(self.bus.conn().hget(self.key(case_id), "error").await?)
    }

    pub async fn is_resolved(&self, case_id: &str) -> Result<bool> {
        Ok(self
            .bus
            .conn()
            .hexists(self.key(case_id), "resolved")
            .await?)
    }

    pub async fn mark_resolved(&self, case_id: &str, outcome: &str) -> Result<()> {
        let _: bool = self
            .bus
            .conn()
            .hset_nx(self.key(case_id), "resolved", outcome)
            .await?;
        let _: i64 = self.bus.conn().zrem(self.deadline_key(), case_id).await?;
        Ok(())
    }
}
