//! Tool executor: the MCP bridge. Consumes `tool.call` on every `bus:tool:{tool}`
//! (group `tool-executor`), calls the MCP server with a time limit, and publishes
//! `tool.response` on the same stream for the worker group.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use redis::AsyncCommands;

use crate::bus::{Bus, ConsumeOptions, Envelope};
use crate::domain::{TOOL_CALL, TOOL_RESPONSE, TOOLS, ToolCall, ToolResponse, with_event};
use crate::mcp::{McpClient, unconfirmed_scope};

pub const GROUP: &str = "tool-executor";

/// Start of the error text of a call the executor cut off at its time limit.
pub const TIMEOUT_ERROR: &str = "timeout after ";

/// Whether a tool error is the executor's time limit (as opposed to the tool failing).
pub fn is_timeout_error(error: &str) -> bool {
    error.starts_with(TIMEOUT_ERROR)
}

pub struct ToolExecutor<M: McpClient> {
    bus: Bus,
    mcp: M,
    /// Upper bound per call for every tool except `faq`; a shorter `timeout_ms` on the call wins.
    pub timeout: Duration,
    /// Upper bound per call for the `faq` tool (strata_rag `/api/ask` runs a generation).
    pub faq_timeout: Duration,
    pub consume: ConsumeOptions,
}

/// Decision D5: per-tool budget. `TOOL_TIMEOUT_MS_FAQ` (default 20000) for `faq`, `TOOL_TIMEOUT_MS` (default 3000) otherwise.
pub fn tool_timeout(tool: &str, get: impl Fn(&str) -> Option<String>) -> Duration {
    let (key, default) = if tool == "faq" {
        ("TOOL_TIMEOUT_MS_FAQ", 20_000)
    } else {
        ("TOOL_TIMEOUT_MS", 3_000)
    };
    Duration::from_millis(get(key).and_then(|v| v.parse().ok()).unwrap_or(default))
}

/// Extra time an entry may stay pending beyond the largest tool budget before another
/// replica may reclaim it (Redis round trips, publishing the response).
pub const RECLAIM_SLACK: Duration = Duration::from_secs(5);

/// An entry is pending for as long as its call runs. If `reclaim_idle` is not longer than
/// the largest budget plus `RECLAIM_SLACK`, other replicas take over healthy calls
/// mid-flight: duplicate responses, rising delivery counters, dead-lettered calls.
pub fn check_reclaim(
    reclaim_idle: Duration,
    timeout: Duration,
    faq_timeout: Duration,
) -> Result<()> {
    let budget = timeout.max(faq_timeout);
    anyhow::ensure!(
        reclaim_idle > budget + RECLAIM_SLACK,
        "tool-executor: consume reclaim_idle {} ms must exceed the largest tool budget {} ms + {} ms slack \
         (TOOL_TIMEOUT_MS / TOOL_TIMEOUT_MS_FAQ); raise reclaim_idle or lower the budgets",
        reclaim_idle.as_millis(),
        budget.as_millis(),
        RECLAIM_SLACK.as_millis()
    );
    Ok(())
}

/// The executor's reclaim window, derived from its own budgets so that raising
/// `TOOL_TIMEOUT_MS_FAQ` (a slower `local-llm` behind strata_rag `/api/ask`) never needs a
/// matching code change: the largest budget plus twice `RECLAIM_SLACK`, and never below the
/// bus default. A crashed replica's call is reclaimed this long after it stalled — the price of
/// not reclaiming healthy calls mid-flight.
pub fn reclaim_idle_for(timeout: Duration, faq_timeout: Duration) -> Duration {
    let derived = timeout.max(faq_timeout) + RECLAIM_SLACK + RECLAIM_SLACK;
    derived.max(ConsumeOptions::default().reclaim_idle)
}

impl<M: McpClient> ToolExecutor<M> {
    pub fn check_reclaim(&self) -> Result<()> {
        check_reclaim(self.consume.reclaim_idle, self.timeout, self.faq_timeout)
    }

    pub fn new(bus: Bus, mcp: M) -> Self {
        let env = |k: &str| std::env::var(k).ok();
        let timeout = tool_timeout("order", env);
        let faq_timeout = tool_timeout("faq", env);
        Self {
            bus,
            mcp,
            timeout,
            faq_timeout,
            // One entry per read: an entry is only pending while its own call runs, never
            // while it waits behind others in a batch (where other replicas would reclaim it).
            consume: ConsumeOptions {
                batch: 1,
                reclaim_idle: reclaim_idle_for(timeout, faq_timeout),
                ..ConsumeOptions::default()
            },
        }
    }

    /// Refuses to start (see `check_reclaim`) before consuming anything.
    pub async fn run(self: Arc<Self>, consumer: &str) -> Result<()> {
        self.check_reclaim()?;
        let streams: Vec<String> = TOOLS.iter().map(|t| self.bus.streams.tool(t)).collect();
        let me = self.clone();
        self.bus
            .consume_all(streams, GROUP, consumer, self.consume.clone(), move |env| {
                let me = me.clone();
                async move { me.on_call(env).await }
            })
            .await
    }

    pub async fn on_call(&self, env: Envelope) -> Result<()> {
        if env.event() != Some(TOOL_CALL) {
            return Ok(()); // tool.response is for the worker group
        }
        let call: ToolCall = env.payload_as()?;
        // The response goes out in the call's context space: its topic prefix, no catalog lookup needed.
        let topic = with_event(&env.topic, TOOL_RESPONSE);
        let done = self.bus.streams.key(&format!("done:call:{}", call.call_id));
        let inflight = self
            .bus
            .streams
            .key(&format!("inflight:call:{}", call.call_id));
        let mut c = self.bus.conn();
        let resp = match c.get::<_, Option<String>>(&done).await? {
            // A redelivery: answer with what the first delivery got, never call MCP again.
            Some(prev) => serde_json::from_str::<ToolResponse>(&prev)?,
            None => {
                // Claim the call before calling MCP. The claim outlives the call's own
                // time limit, so a crashed claimant cannot block a redelivery for long.
                let ttl = self.limit_for(&call).as_secs() + 5;
                let claimed: Option<String> = redis::cmd("SET")
                    .arg(&inflight)
                    .arg("1")
                    .arg("NX")
                    .arg("EX")
                    .arg(ttl)
                    .query_async(&mut c)
                    .await?;
                if claimed.is_some() {
                    let resp = self.execute(&call).await;
                    let _: () = c
                        .set_ex(&done, serde_json::to_string(&resp)?, 86_400)
                        .await?;
                    resp
                } else {
                    // Another replica is executing this call. Answer if it has finished;
                    // otherwise fail this delivery so the entry stays pending and a later
                    // redelivery finds the stored response.
                    match c.get::<_, Option<String>>(&done).await? {
                        Some(prev) => serde_json::from_str::<ToolResponse>(&prev)?,
                        None => anyhow::bail!("call {} is in flight elsewhere", call.call_id),
                    }
                }
            }
        };
        let w = &call.work;
        self.bus
            .publish(
                &self.bus.streams.tool(&w.worker),
                &Envelope::new(&topic, &format!("mcp:{}", w.worker), &w.case_id, &resp)?,
            )
            .await?;
        Ok(())
    }

    /// The call's time limit: the tool budget, or the call's own `timeout_ms` if shorter.
    fn limit_for(&self, call: &ToolCall) -> Duration {
        let budget = if call.work.worker == "faq" {
            self.faq_timeout
        } else {
            self.timeout
        };
        budget.min(Duration::from_millis(call.timeout_ms.max(1)))
    }

    async fn execute(&self, call: &ToolCall) -> ToolResponse {
        let limit = self.limit_for(call);
        let started = Instant::now();
        let res = tokio::time::timeout(
            limit,
            self.mcp.call_tool(&call.mcp_tool, call.arguments.clone()),
        )
        .await;
        let latency_ms = started.elapsed().as_millis() as u64;
        let (ok, result, error, retryable) = match res {
            // Checked before the answer is stored or published: an unconfirmed store filter drops it.
            Ok(Ok(v)) => match unconfirmed_scope(&call.arguments, &v) {
                None => (true, Some(v), None, false),
                Some(e) => {
                    tracing::warn!(call = %call.call_id, tool = %call.mcp_tool, "{e}");
                    (false, None, Some(e.to_string()), false)
                }
            },
            Ok(Err(e)) => (false, None, Some(e.message), e.retryable),
            Err(_) => (
                false,
                None,
                Some(format!("{TIMEOUT_ERROR}{} ms", limit.as_millis())),
                true,
            ),
        };
        ToolResponse {
            call_id: call.call_id.clone(),
            mcp_tool: call.mcp_tool.clone(),
            ok,
            result,
            error,
            retryable,
            latency_ms,
        }
    }
}
