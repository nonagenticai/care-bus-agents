//! Worker agent: one group per tool. Takes `work.requested` from
//! `bus:worker:{area}:{tool}`, sends `tool.call` on `bus:tool:{tool}`, and on the
//! matching `tool.response` publishes `work.succeeded` / `work.failed` to
//! `bus:worker:{area}:results`. State lives in Redis, so any replica can take the response.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use redis::AsyncCommands;
use serde_json::Value;

use crate::bus::{Bus, ConsumeOptions, Envelope};
use crate::domain::QualifiedRequest;
use crate::domain::{
    RequestType, TOOL_CALL, TOOL_RESPONSE, ToolCall, ToolResponse, WORK_FAILED, WORK_REQUESTED,
    WORK_SUCCEEDED, WorkOutcome, WorkRequest,
};
use crate::gates::{GateClient, Verdict, tool_state};
use crate::mcp::{FAQ_STORE_ARGS, is_side_effect, mcp_arguments, mcp_tool_name};
use crate::spaces::{ContextSpace, SpaceCatalog};
use crate::tools::requirements;

/// The error of a `faq.ask` step when carebus-api cannot say what the chat may search (non-retryable).
pub const FAQ_SCOPE_UNAVAILABLE: &str =
    "faq_scope_unavailable: carebus-api could not say which stores chat may search";
/// The error of a `faq.ask` step when every store of the tenant is walled for a legal matter (non-retryable).
pub const FAQ_NO_STORES: &str = "faq_no_stores: every knowledge store is walled for a legal matter";

/// The consumer group of the worker for `tool`; also what `Streams::consumer_group` reports.
pub fn group_for(tool: &str) -> String {
    format!("worker-{tool}")
}

pub struct Worker {
    bus: Bus,
    pub tool: String,
    /// Sent as `timeout_ms` on each call; the executor enforces it.
    pub timeout: Duration,
    pub consume: ConsumeOptions,
    /// The scope's context spaces: one `bus:worker:{area}:{tool}` stream per area; a work item's request type
    /// names the space its call and outcome are routed in.
    spaces: SpaceCatalog,
    /// Tool gates through carebus-api (spec 2026-10-18 §3); `None`: no gate is asked.
    pub gates: Option<GateClient>,
}

impl Worker {
    pub fn new(bus: Bus, tool: impl Into<String>) -> Self {
        Self {
            bus,
            tool: tool.into(),
            timeout: Duration::from_secs(3),
            consume: ConsumeOptions::default(),
            spaces: SpaceCatalog::bootstrap(),
            gates: None,
        }
    }

    /// Serve these context spaces (the scope's) instead of the bootstrap job's.
    pub fn with_spaces(mut self, spaces: SpaceCatalog) -> Self {
        self.spaces = spaces;
        self
    }

    /// The space of `rt`. A request type this scope does not have is an error: the entry stays pending and is
    /// dead-lettered after its deliveries run out (a space added since this task read the catalog is picked up when
    /// the task restarts on the new catalog, well within that).
    fn space_of(&self, rt: &RequestType) -> Result<&ContextSpace> {
        self.spaces
            .space_of(rt)
            .with_context(|| format!("request type {rt} is not one of this scope's context spaces"))
    }

    fn name(&self) -> String {
        format!("worker:{}", self.tool)
    }

    fn group(&self) -> String {
        group_for(&self.tool)
    }

    fn call_key(&self, call_id: &str) -> String {
        self.bus.streams.key(&format!("call:{call_id}"))
    }

    fn done_key(&self, work_id: &str) -> String {
        self.bus.streams.key(&format!("done:work:{work_id}"))
    }

    pub async fn run(self: Arc<Self>, consumer: &str) -> Result<()> {
        let work: Vec<String> = self
            .spaces
            .areas()
            .iter()
            .map(|a| self.bus.streams.worker(a, &self.tool))
            .collect();
        let tool_stream = self.bus.streams.tool(&self.tool);
        let group = self.group();
        let (a, b) = (self.clone(), self.clone());
        tokio::try_join!(
            self.bus
                .consume_all(work, &group, consumer, self.consume.clone(), move |env| {
                    let a = a.clone();
                    async move { a.on_work(env).await }
                }),
            self.bus.consume(
                &tool_stream,
                &group,
                consumer,
                self.consume.clone(),
                move |env| {
                    let b = b.clone();
                    async move { b.on_response(env).await }
                }
            ),
        )?;
        Ok(())
    }

    pub async fn on_work(&self, env: Envelope) -> Result<()> {
        if env.event() != Some(WORK_REQUESTED) {
            return Ok(());
        }
        let w: WorkRequest = env.payload_as()?;
        let space = self.space_of(&w.request_type)?;
        let mut c = self.bus.conn();
        // Already finished: publish the stored outcome again in case the
        // previous delivery died between storing and publishing.
        if let Some(prev) = c
            .get::<_, Option<String>>(self.done_key(&w.work_id))
            .await?
        {
            let o: WorkOutcome = serde_json::from_str(&prev)?;
            return self.publish_outcome(&o).await;
        }
        if let Err(msg) = check_requirements(&w) {
            return self.finish(outcome(&w, Err((msg, false)))).await;
        }
        if let Some(stopped) = self.tool_gate(&w).await {
            // Non-retryable: the orchestrator resolves the case as failed, which the customer reads as a handoff.
            return self
                .finish(outcome(&w, Err((stopped.reason(), false))))
                .await;
        }
        let w = match self.faq_scoped(&w).await {
            Ok(w) => w,
            Err(msg) => return self.finish(outcome(&w, Err((msg, false)))).await,
        };
        let call = ToolCall {
            call_id: w.work_id.clone(),
            mcp_tool: mcp_tool_name(&w.worker, &w.action),
            arguments: mcp_arguments(
                &w.worker,
                &w.action,
                &w.args,
                &w.idempotency_key,
                w.side_effect,
            ),
            timeout_ms: self.timeout.as_millis() as u64,
            work: w.clone(),
        };
        let key = self.call_key(&call.call_id);
        // One command: a crash can never leave the record without its expiry.
        let _: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg(serde_json::to_string(&w)?)
            .arg("NX")
            .arg("EX")
            .arg(86_400)
            .query_async(&mut c)
            .await?;
        self.bus
            .publish(
                &self.bus.streams.tool(&w.worker),
                &Envelope::new(&space.topic(TOOL_CALL), &self.name(), &w.case_id, &call)?,
            )
            .await?;
        Ok(())
    }

    /// The tool point, for side-effect calls only (`mcp::is_side_effect`; carebus-api further narrows to each
    /// question's `tools`). `Some(verdict)` when the call must not happen: fired with handoff/block, failed closed,
    /// or carebus-api unreachable (a tool gate is a safety gate, so a transport error counts as failed closed).
    async fn tool_gate(&self, w: &WorkRequest) -> Option<Verdict> {
        let (gates, scope) = (self.gates.as_ref()?, self.bus.scope()?);
        if !is_side_effect(&w.worker, &w.action, w.side_effect) {
            return None;
        }
        let tool = mcp_tool_name(&w.worker, &w.action);
        // The customer's own words only; the tool is named separately (`tool`), never in the state.
        let state = tool_state(&self.utterance(&w.case_id).await);
        let v = match gates
            .evaluate(scope, "tool", &state, Some(&w.case_id), Some(&tool))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(case = %w.case_id, %tool, error = %format!("{e:#}"), "tool gate unreachable; failing closed");
                Verdict::failed_closed("carebus-api unreachable")
            }
        };
        v.stops().then_some(v)
    }

    /// `faq.ask` searches only what the customer chat may see (legal-matter walls, carebus-api crate::walls): store
    /// arguments a plan step set are dropped ([`FAQ_STORE_ARGS`]), and for a tenant with walled stores the call
    /// carries carebus-api's `leaf_ids`, which the executor requires the MCP server to confirm. `Err` (the step fails,
    /// non-retryable, so no store is searched) when carebus-api cannot say or chat may search nothing. Without a gate
    /// client (`CAREBUS_GATES=off`: no carebus-api) or a scope there is nothing to ask, and nothing is restricted.
    async fn faq_scoped(&self, w: &WorkRequest) -> Result<WorkRequest, String> {
        if w.worker != "faq" || w.action != "ask" {
            return Ok(w.clone());
        }
        let mut w = w.clone();
        if let Value::Object(args) = &mut w.args {
            for k in FAQ_STORE_ARGS {
                args.remove(k);
            }
        }
        let (Some(gates), Some(scope)) = (self.gates.as_ref(), self.bus.scope()) else {
            return Ok(w);
        };
        match gates.faq_scope(scope).await {
            Ok(sc) if !sc.restricted => Ok(w),
            Ok(sc) if sc.leaf_ids.is_empty() => Err(FAQ_NO_STORES.into()),
            // never an unfiltered call: arguments that are not an object cannot carry the filter
            Ok(_) if !w.args.is_object() => Err(FAQ_SCOPE_UNAVAILABLE.into()),
            Ok(sc) => {
                w.args["leaf_ids"] = Value::from(sc.leaf_ids);
                Ok(w)
            }
            Err(e) => {
                tracing::warn!(case = %w.case_id, error = %format!("{e:#}"), "faq scope unavailable; not searching");
                Err(FAQ_SCOPE_UNAVAILABLE.into())
            }
        }
    }

    /// The customer's request text of `case` from the orchestrator's case hash (`case:{id}` field `request`);
    /// empty when it cannot be read.
    async fn utterance(&self, case_id: &str) -> String {
        let key = self.bus.streams.key(&format!("case:{case_id}"));
        let raw: Option<String> = self
            .bus
            .conn()
            .hget::<_, _, Option<String>>(key, "request")
            .await
            .ok()
            .flatten();
        raw.and_then(|r| serde_json::from_str::<QualifiedRequest>(&r).ok())
            .map(|q| q.utterance)
            .unwrap_or_default()
    }

    pub async fn on_response(&self, env: Envelope) -> Result<()> {
        if env.event() != Some(TOOL_RESPONSE) {
            return Ok(()); // tool.call on this stream is the executor's
        }
        let r: ToolResponse = env.payload_as()?;
        let raw: Option<String> = self.bus.conn().get(self.call_key(&r.call_id)).await?;
        let Some(raw) = raw else {
            tracing::warn!(call_id = %r.call_id, "tool.response for an unknown or expired call, dropped");
            return Ok(());
        };
        let w: WorkRequest = serde_json::from_str(&raw)?;
        let res = if r.ok {
            Ok(r.result.unwrap_or(Value::Null))
        } else {
            Err((r.error.unwrap_or_else(|| "tool failed".into()), r.retryable))
        };
        self.finish(outcome(&w, res)).await
    }

    /// First outcome for a work_id wins (`SET NX`); a duplicate or conflicting
    /// `tool.response` republishes the stored one instead of a new, different outcome.
    async fn finish(&self, o: WorkOutcome) -> Result<()> {
        let key = self.done_key(&o.work_id);
        let mut c = self.bus.conn();
        let stored: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg(serde_json::to_string(&o)?)
            .arg("NX")
            .arg("GET")
            .arg("EX")
            .arg(86_400)
            .query_async(&mut c)
            .await?;
        match stored {
            None => self.publish_outcome(&o).await,
            Some(prev) => self.publish_outcome(&serde_json::from_str(&prev)?).await,
        }
    }

    async fn publish_outcome(&self, o: &WorkOutcome) -> Result<()> {
        let space = self.space_of(&o.request_type)?;
        let topic = space.topic(if o.ok { WORK_SUCCEEDED } else { WORK_FAILED });
        self.bus
            .publish(
                &self.bus.streams.worker_results(&space.area),
                &Envelope::new(&topic, &self.name(), &o.case_id, o)?,
            )
            .await?;
        Ok(())
    }
}

fn check_requirements(w: &WorkRequest) -> Result<(), String> {
    let Some(req) = requirements(&w.worker, &w.action) else {
        return Err(format!("{} has no action {}", w.worker, w.action));
    };
    let missing: Vec<&str> = req
        .iter()
        .copied()
        .filter(|k| w.args.get(*k).is_none_or(Value::is_null))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing required params: {}", missing.join(", ")))
    }
}

fn outcome(w: &WorkRequest, res: Result<Value, (String, bool)>) -> WorkOutcome {
    let (ok, result, error, retryable) = match res {
        Ok(v) => (true, Some(v), None, false),
        Err((m, r)) => (false, None, Some(m), r),
    };
    WorkOutcome {
        work_id: w.work_id.clone(),
        case_id: w.case_id.clone(),
        phase: w.phase,
        as_name: w.as_name.clone(),
        worker: w.worker.clone(),
        action: w.action.clone(),
        attempt: w.attempt,
        ok,
        result,
        error,
        retryable,
        request_type: w.request_type.clone(),
    }
}
