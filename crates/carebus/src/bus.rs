//! Redis Streams bus: XADD to publish, consumer groups to share work,
//! XACK on success, XAUTOCLAIM to take over entries a dead consumer left
//! pending, and a dead-letter stream after `max_deliveries`.
//!
//! Delivery is at-least-once. Every handler must be idempotent.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use redis::aio::{ConnectionManager, ConnectionManagerConfig, MultiplexedConnection};
use redis::streams::{
    StreamAutoClaimReply, StreamId, StreamInfoGroupsReply, StreamPendingCountReply, StreamReadReply,
};
use redis::{AsyncCommands, AsyncConnectionConfig, Client};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::scope::Scope;

/// The one message shape on every stream. Stored as a single JSON field.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub topic: String,
    pub from: String,
    pub correlation_id: String,
    pub published_at_ms: u64,
    pub payload: serde_json::Value,
    /// `name@version` of the procedure version running the case, on the orchestrator's `work.requested` and
    /// `request.resolved`. Absent on every other envelope and on entries written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procedure: Option<String>,
    /// The scope the envelope was published in, stamped by `Bus::publish` (only a scoped bus publishes, so on
    /// the wire both are always present). `Option` because `Envelope::new` is scope-agnostic.
    #[serde(default)]
    pub tenant_id: Option<Uuid>,
    #[serde(default)]
    pub job: Option<String>,
}

impl Envelope {
    pub fn new(
        topic: &str,
        from: &str,
        correlation_id: &str,
        payload: impl Serialize,
    ) -> Result<Self> {
        Ok(Self {
            id: uuid::Uuid::new_v4().to_string(),
            topic: topic.into(),
            from: from.into(),
            correlation_id: correlation_id.into(),
            published_at_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
            payload: serde_json::to_value(payload)?,
            procedure: None,
            tenant_id: None,
            job: None,
        })
    }

    /// Stamp the procedure version (`name@version`) the case runs.
    pub fn with_procedure(mut self, procedure: Option<&str>) -> Self {
        self.procedure = procedure.map(String::from);
        self
    }

    pub fn payload_as<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.payload.clone())
            .with_context(|| format!("payload of {} is not the expected shape", self.topic))
    }

    /// The event this envelope carries, ignoring the context-space prefix.
    pub fn event(&self) -> Option<&'static str> {
        crate::domain::event_of(&self.topic)
    }
}

/// Stream and key names. The prefix isolates environments and test runs.
#[derive(Clone, Debug)]
pub struct Streams {
    prefix: String,
}

impl Streams {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }
    /// interaction <-> orchestrator for one department: `request.qualified` down, `request.resolved` back.
    pub fn customer(&self, area: &str) -> String {
        format!("{}bus:customer:{area}", self.prefix)
    }
    /// orchestrator -> worker, one stream per department and tool.
    pub fn worker(&self, area: &str, tool: &str) -> String {
        format!("{}bus:worker:{area}:{tool}", self.prefix)
    }
    /// workers -> orchestrator: `work.succeeded` / `work.failed` for one department.
    pub fn worker_results(&self, area: &str) -> String {
        format!("{}bus:worker:{area}:results", self.prefix)
    }
    /// worker <-> tool-executor for one tool: `tool.call` down, `tool.response` back.
    pub fn tool(&self, tool: &str) -> String {
        format!("{}bus:tool:{tool}", self.prefix)
    }
    pub fn dead(&self) -> String {
        format!("{}bus:dead", self.prefix)
    }
    /// Pub/Sub channel mirroring every publish, for a lossy live console.
    pub fn console(&self) -> String {
        format!("{}console", self.prefix)
    }
    pub fn key(&self, k: &str) -> String {
        format!("{}{k}", self.prefix)
    }
    /// The prefix every key above starts with (environment prefix, plus the scope on a scoped bus).
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The consumer group an entry is FOR: the one whose handler acts on `topic` on `stream` (prefixed or not).
    /// Several groups read some streams (orchestrator and interaction both read `bus:customer:{area}`, the
    /// worker and the executor both read `bus:tool:{tool}`) and every one of them XACKs every entry, so only
    /// the recipient's ack says the entry was handled. `None` for the dead-letter stream and unknown topics.
    /// This is the only copy of the stream -> group topology outside the agents' own `consume` calls.
    pub fn consumer_group(&self, stream: &str, topic: &str) -> Option<String> {
        use crate::domain as d;
        let key = stream.strip_prefix(self.prefix.as_str()).unwrap_or(stream);
        let parts: Vec<&str> = key.split(':').collect();
        let event = d::event_of(topic)?;
        let group = match (parts.as_slice(), event) {
            (["bus", "customer", _], d::REQUEST_QUALIFIED) => {
                crate::orchestrator::GROUP.to_string()
            }
            (["bus", "customer", _], d::REQUEST_RESOLVED) => crate::interaction::GROUP.to_string(),
            (["bus", "worker", _, "results"], d::WORK_SUCCEEDED | d::WORK_FAILED) => {
                crate::orchestrator::GROUP.to_string()
            }
            (["bus", "worker", _, tool], d::WORK_REQUESTED) => crate::worker::group_for(tool),
            (["bus", "tool", _], d::TOOL_CALL) => crate::executor::GROUP.to_string(),
            (["bus", "tool", tool], d::TOOL_RESPONSE) => crate::worker::group_for(tool),
            _ => return None,
        };
        Some(group)
    }
}

/// `a <= b` for stream entry ids `<ms>-<seq>`; an unparseable id compares as not-before.
fn id_le(a: &str, b: &str) -> bool {
    let p = |s: &str| -> Option<(u64, u64)> {
        let (ms, seq) = s.split_once('-')?;
        Some((ms.parse().ok()?, seq.parse().ok()?))
    };
    matches!((p(a), p(b)), (Some(a), Some(b)) if a <= b)
}

#[derive(Clone, Debug)]
pub struct ConsumeOptions {
    pub batch: usize,
    pub block: Duration,
    /// An entry pending longer than this is taken over from its consumer.
    pub reclaim_idle: Duration,
    /// How often the takeover sweep (`XAUTOCLAIM`) runs. Between sweeps the consumer only reads new
    /// entries, so an idle-then-busy stream does not pay a sweep round trip on every read.
    pub reclaim_every: Duration,
    pub max_deliveries: u64,
}

impl Default for ConsumeOptions {
    fn default() -> Self {
        Self {
            batch: 16,
            block: Duration::from_secs(2),
            reclaim_idle: Duration::from_secs(30),
            reclaim_every: Duration::from_secs(1),
            max_deliveries: 5,
        }
    }
}

#[derive(Clone)]
pub struct Bus {
    client: Client,
    conn: ConnectionManager,
    /// Key names. On a scoped bus the prefix is `{root_prefix}s:{tenant}:{job}:`.
    pub streams: Streams,
    maxlen: usize,
    /// The environment prefix (`CAREBUS_PREFIX`), kept apart from the scope.
    root_prefix: String,
    /// `None` on the root bus, which may only touch the scope registry (`root_key`).
    scope: Option<Scope>,
    /// The console live mirror, queued off the caller's critical path.
    mirror: Arc<Mirror>,
}

/// The console live mirror: every publish and ack frame is queued and flushed by one background
/// task, so mirroring never costs a caller a round trip. Lossy by design, like the mirror itself:
/// a full queue, or a connection that will not open, drops frames. The channel rides each frame
/// because one root bus serves every scope, and a scoped bus mirrors to its scoped console.
struct Mirror {
    tx: tokio::sync::mpsc::Sender<(String, String)>,
}

/// One entry whose handler succeeded, waiting for the batch's `XACK`.
struct Acked {
    entry_id: String,
    counter: String,
    env: Envelope,
}

impl Mirror {
    fn new(client: Client) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, String)>(1024);
        tokio::spawn(async move {
            let mut conn: Option<MultiplexedConnection> = None;
            while let Some(first) = rx.recv().await {
                // One SUBSCRIBE-channel PUBLISH per frame, flushed as one pipeline: frames queued
                // while the last flush was in flight ride the next.
                let mut frames = vec![first];
                while let Ok(frame) = rx.try_recv() {
                    frames.push(frame);
                }
                let mut c = match conn.clone() {
                    Some(c) => c,
                    None => match client.get_multiplexed_async_connection().await {
                        Ok(c) => {
                            conn = Some(c.clone());
                            c
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "console mirror connection failed; frames dropped");
                            continue;
                        }
                    },
                };
                let mut pipe = redis::pipe();
                for (channel, frame) in &frames {
                    pipe.cmd("PUBLISH").arg(channel).arg(frame);
                }
                let flushed: redis::RedisResult<Vec<i64>> = pipe.query_async(&mut c).await;
                if let Err(e) = flushed {
                    tracing::warn!(error = %e, "console mirror PUBLISH failed; frames dropped");
                    conn = None;
                }
            }
        });
        Self { tx }
    }

    /// Queue a frame; a full or closed queue drops it, which is what the mirror already promised.
    fn send(&self, channel: &str, frame: String) {
        let _ = self.tx.try_send((channel.to_string(), frame));
    }
}

/// Per-command Redis response budget. The `redis` crate defaults to 500 ms, which a cross-site round trip
/// (home site → worker3) exceeds often enough to fail ~0.5% of the 5 s deadline sweep. 10 s because the
/// stall that matters is not a slow command but a whole-tunnel re-handshake: on hp1 (2026-11-18) a
/// tailscale0 queue stall dropped every cross-site packet for several seconds at a time, and both the
/// Redis sweeps and agent-api's Postgres pool timed out together while the Redis server itself was idle
/// and healthy. Sweeps are lease-based and idempotent, so a late reply is safe; the budget only has to
/// outlast the stall. Bounded on purpose — a real outage still surfaces, just 10 s later.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on establishing the blocking consumer's own connection: a stalled tunnel breaks the connect too.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Slack above one `BLOCK` window for the blocking read. A live block returns within [`ConsumeOptions::block`]; a
/// black-holed connection does not return at all, and without a bound the consumer waits for the kernel's
/// retransmission timeout (~15 min on Linux) with no error, no reconnect and no log.
pub const CONSUME_MARGIN: Duration = Duration::from_secs(10);

/// First reconnect delay after a transport failure. Doubles per attempt, capped by [`RETRY_MAX`], and carries
/// jitter so consumers that lost the same tunnel together do not reconnect as one wave.
pub const RETRY_BASE: Duration = Duration::from_secs(1);
/// Ceiling on the reconnect delay: a recovered tunnel is retried at least twice a minute.
pub const RETRY_MAX: Duration = Duration::from_secs(30);

/// `true` for the failures a stalled tunnel produces (I/O, connect and response timeouts) and `false` for the ones
/// a retry cannot fix (a wrong password, a wrong key type, an unscoped bus). Only the former are retried.
fn is_transport(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<redis::RedisError>()
            .is_some_and(|r| r.is_io_error() || r.is_timeout())
    })
}

/// The delay before reconnect attempt `attempt` (1-based): [`RETRY_BASE`] doubled per attempt up to [`RETRY_MAX`],
/// plus up to a quarter of that in jitter taken from the clock (no extra dependency).
fn retry_delay(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.saturating_sub(1).min(5);
    let base = RETRY_BASE.saturating_mul(factor).min(RETRY_MAX);
    let span = u64::try_from(base.as_millis() / 4).unwrap_or(1).max(1);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()))
        % span;
    base + Duration::from_millis(jitter)
}

impl Bus {
    pub async fn connect(url: &str, prefix: &str) -> Result<Self> {
        let client = Client::open(url).with_context(|| format!("bad redis url {url}"))?;
        let cfg = ConnectionManagerConfig::new().set_response_timeout(Some(RESPONSE_TIMEOUT));
        let conn = ConnectionManager::new_with_config(client.clone(), cfg)
            .await
            .with_context(|| format!("connect {url}"))?;
        let streams = Streams::new(prefix);
        let mirror = Arc::new(Mirror::new(client.clone()));
        Ok(Self {
            client,
            conn,
            streams,
            maxlen: 100_000,
            root_prefix: prefix.to_string(),
            scope: None,
            mirror,
        })
    }

    /// The same client and connection, with every key under `{root_prefix}s:{tenant}:{job}:`.
    pub fn scoped(&self, s: &Scope) -> Bus {
        Bus {
            client: self.client.clone(),
            conn: self.conn.clone(),
            streams: Streams::new(format!("{}s:{}:{}:", self.root_prefix, s.tenant, s.job)),
            maxlen: self.maxlen,
            root_prefix: self.root_prefix.clone(),
            scope: Some(s.clone()),
            mirror: self.mirror.clone(),
        }
    }

    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }

    /// `{root_prefix}{k}`, unscoped: for the scope registry only.
    pub fn root_key(&self, k: &str) -> String {
        format!("{}{k}", self.root_prefix)
    }

    /// The scope, or `bus is not scoped` naming what was refused (in the message itself, not a context layer,
    /// so the plain `to_string()` of the error says it).
    fn require_scope(&self, what: &str) -> Result<&Scope> {
        self.scope
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("bus is not scoped: refusing {what}"))
    }

    /// The underlying client, for dedicated connections (Pub/Sub subscriptions).
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Shared connection for non-blocking commands.
    pub fn conn(&self) -> ConnectionManager {
        self.conn.clone()
    }

    /// XADD `env` stamped with this bus's scope. A root bus refuses.
    pub async fn publish(&self, stream: &str, env: &Envelope) -> Result<String> {
        let scope = self.require_scope(&format!("XADD {stream}"))?;
        let mut env = env.clone();
        env.tenant_id = Some(scope.tenant);
        env.job = Some(scope.job.clone());
        let env = &env;
        let json = serde_json::to_string(env)?;
        let mut c = self.conn.clone();
        let id: String = redis::cmd("XADD")
            .arg(stream)
            .arg("MAXLEN")
            .arg("~")
            .arg(self.maxlen)
            .arg("*")
            .arg("envelope")
            .arg(&json)
            .query_async(&mut c)
            .await
            .with_context(|| format!("XADD {stream}"))?;
        // `acked` is false at XADD time; the recipient's XACK follows as a `kind: "ack"` frame (see `mirror_ack`).
        let group = self.streams.consumer_group(stream, &env.topic);
        let acked = group.as_ref().map(|_| false);
        let live = serde_json::json!({
            "stream": stream,
            "entry_id": id,
            "envelope": env,
            "consumer_group": group,
            "acked": acked,
        })
        .to_string();
        // The console mirror is lossy by design: the entry is already on the stream, so a dropped
        // frame must not make the caller think nothing was published. It is queued, never awaited:
        // a publish costs one round trip, the `XADD`'s.
        self.mirror.send(&self.streams.console(), live);
        Ok(id)
    }

    /// Whether `group` has acknowledged `entry_id` on `stream` (full key), computed from Redis itself, so a
    /// reader that missed the live `ack` frame gets the truth: the group has delivered the entry (its
    /// last-delivered-id is at or past it) and the entry is no longer in the group's pending list. A missing
    /// stream or group means nobody has read it: not acked.
    pub async fn is_acked(&self, stream: &str, group: &str, entry_id: &str) -> Result<bool> {
        let mut c = self.conn.clone();
        let groups: redis::RedisResult<StreamInfoGroupsReply> = redis::cmd("XINFO")
            .arg("GROUPS")
            .arg(stream)
            .query_async(&mut c)
            .await;
        let groups = match groups {
            Ok(g) => g,
            Err(e) if e.to_string().contains("no such key") => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("XINFO GROUPS {stream}")),
        };
        let Some(g) = groups.groups.iter().find(|g| g.name == group) else {
            return Ok(false);
        };
        if !id_le(entry_id, &g.last_delivered_id) {
            return Ok(false);
        }
        let pending: StreamPendingCountReply = redis::cmd("XPENDING")
            .arg(stream)
            .arg(group)
            .arg(entry_id)
            .arg(entry_id)
            .arg(1)
            .query_async(&mut c)
            .await
            .with_context(|| format!("XPENDING {stream} {group} {entry_id}"))?;
        Ok(pending.ids.is_empty())
    }

    /// The console frame mirroring the recipient group's XACK, lossy like the publish mirror, or
    /// `None` for a group the entry was not for. Acks by the other groups that read the same
    /// stream (and skip the entry) are not mirrored: they say nothing about handling.
    fn ack_frame(
        &self,
        stream: &str,
        group: &str,
        entry_id: &str,
        env: &Envelope,
    ) -> Option<String> {
        if self.streams.consumer_group(stream, &env.topic).as_deref() != Some(group) {
            return None;
        }
        Some(
            serde_json::json!({
                "kind": "ack",
                "stream": stream,
                "entry_id": entry_id,
                "consumer_group": group,
                "correlation_id": env.correlation_id,
            })
            .to_string(),
        )
    }

    pub async fn ensure_group(&self, stream: &str, group: &str) -> Result<()> {
        self.require_scope(&format!("XGROUP CREATE {stream} {group}"))?;
        let mut c = self.conn.clone();
        let r: redis::RedisResult<()> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(stream)
            .arg(group)
            .arg("0")
            .arg("MKSTREAM")
            .query_async(&mut c)
            .await;
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.code() == Some("BUSYGROUP") => Ok(()),
            Err(e) => Err(e).with_context(|| format!("XGROUP CREATE {stream} {group}")),
        }
    }

    /// Consume `stream` as `consumer` in `group` until the task is dropped.
    /// `Ok` acks the entry; `Err` leaves it pending for redelivery.
    ///
    /// A transport failure (a stalled tunnel, or a connection the peer closed) is not returned: the session is
    /// rebuilt with capped, jittered backoff, so one stream's dead connection cannot take down the pod's other
    /// consumers through `consume_all`'s `try_join_all`. Only an error a retry cannot fix is returned.
    pub async fn consume<F, Fut>(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        opts: ConsumeOptions,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(Envelope) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let mut attempt = 0u32;
        loop {
            let started = tokio::time::Instant::now();
            match self
                .consume_session(stream, group, consumer, &opts, &handler)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) if is_transport(&e) => {
                    // A session that stayed up was healthy: start the backoff over instead of waiting the cap.
                    if started.elapsed() >= RETRY_MAX {
                        attempt = 0;
                    }
                    attempt = attempt.saturating_add(1);
                    let delay = retry_delay(attempt);
                    tracing::warn!(
                        stream,
                        group,
                        consumer,
                        attempt,
                        delay_ms = delay.as_millis(),
                        "redis transport error; reconnecting: {e:#}"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One reader connection's lifetime: `ensure_group`, then the blocking read loop. Returns only on an error
    /// (the caller decides whether to retry) or when the task is dropped.
    async fn consume_session<F, Fut>(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        opts: &ConsumeOptions,
        handler: &F,
    ) -> Result<()>
    where
        F: Fn(Envelope) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.ensure_group(stream, group).await?;
        // A blocking read needs its own connection: on a shared multiplexed
        // connection BLOCK stalls every other command. Bounded on purpose, not
        // unbounded: one BLOCK window plus a margin, so a black-holed tunnel
        // errors and reconnects instead of parking the consumer until the kernel gives up.
        let cfg = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(CONNECT_TIMEOUT))
            .set_response_timeout(Some(opts.block + CONSUME_MARGIN));
        let mut reader: MultiplexedConnection = self
            .client
            .get_multiplexed_async_connection_with_config(&cfg)
            .await?;
        // Delivery counters and acks are not blocking: they ride the shared connection.
        let mut admin = self.conn.clone();
        // The takeover sweep walks the pending list with a cursor and runs at most every
        // `reclaim_every`: a busy stream does not pay a sweep round trip per read.
        let mut reclaim_at = "0-0".to_string();
        let mut last_reclaim: Option<Instant> = None;
        loop {
            let mut entries: Vec<StreamId> = Vec::new();
            if last_reclaim.is_none_or(|at: Instant| at.elapsed() >= opts.reclaim_every) {
                last_reclaim = Some(Instant::now());
                let claimed: StreamAutoClaimReply = redis::cmd("XAUTOCLAIM")
                    .arg(stream)
                    .arg(group)
                    .arg(consumer)
                    .arg(opts.reclaim_idle.as_millis() as u64)
                    .arg(&reclaim_at)
                    .arg("COUNT")
                    .arg(opts.batch)
                    .query_async(&mut reader)
                    .await?;
                reclaim_at = claimed.next_stream_id.clone();
                entries.extend(claimed.claimed);
            }
            let read: Option<StreamReadReply> = redis::cmd("XREADGROUP")
                .arg("GROUP")
                .arg(group)
                .arg(consumer)
                .arg("COUNT")
                .arg(opts.batch)
                .arg("BLOCK")
                .arg(opts.block.as_millis() as u64)
                .arg("STREAMS")
                .arg(stream)
                .arg(">")
                .query_async(&mut reader)
                .await?;
            for key in read.map(|r| r.keys).unwrap_or_default() {
                entries.extend(key.ids);
            }
            if entries.is_empty() {
                continue;
            }

            // Every delivery counter in one pipeline: the batch pays one round trip, not one each.
            let counters: Vec<String> = entries
                .iter()
                .map(|e| {
                    self.streams
                        .key(&format!("deliveries:{stream}:{group}:{}", e.id))
                })
                .collect();
            let mut pipe = redis::pipe();
            for counter in &counters {
                pipe.cmd("INCR").arg(counter);
                pipe.cmd("EXPIRE").arg(counter).arg(86_400);
            }
            let counts: Vec<i64> = pipe
                .query_async(&mut admin)
                .await
                .with_context(|| format!("INCR deliveries {stream} {group}"))?;

            let mut acks: Vec<Acked> = Vec::new();
            for (i, entry) in entries.iter().enumerate() {
                let deliveries = counts.get(2 * i).copied().unwrap_or(1).max(1) as u64;
                if let Some(acked) = self
                    .handle_entry(stream, group, entry, deliveries, opts, handler)
                    .await?
                {
                    acks.push(acked);
                }
            }
            // XACK, the counter DELs and the console ack frames in one pipeline: one round trip for
            // the whole batch. At-least-once is unchanged: a crash before this flush redelivers the
            // entries, and every handler is idempotent.
            if !acks.is_empty() {
                self.ack_batch(stream, group, &acks).await?;
            }
        }
    }

    /// `consume` on several streams at once with the same group and handler.
    /// Returns when any of them fails.
    pub async fn consume_all<F, Fut>(
        &self,
        streams: Vec<String>,
        group: &str,
        consumer: &str,
        opts: ConsumeOptions,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(Envelope) -> Fut + Clone,
        Fut: Future<Output = Result<()>>,
    {
        self.require_scope(&format!("consume_all as {group}"))?;
        let runs = streams
            .iter()
            .map(|s| self.consume(s, group, consumer, opts.clone(), handler.clone()));
        futures_util::future::try_join_all(runs).await?;
        Ok(())
    }

    /// Run one delivered entry's handler. `Ok(Some(_))` = handled, to be acked with the batch;
    /// `Ok(None)` = dead-lettered, or the handler failed and the entry stays pending.
    async fn handle_entry<F, Fut>(
        &self,
        stream: &str,
        group: &str,
        entry: &StreamId,
        deliveries: u64,
        opts: &ConsumeOptions,
        handler: &F,
    ) -> Result<Option<Acked>>
    where
        F: Fn(Envelope) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let counter = self
            .streams
            .key(&format!("deliveries:{stream}:{group}:{}", entry.id));
        let env = entry
            .get::<String>("envelope")
            .and_then(|raw| serde_json::from_str::<Envelope>(&raw).ok());
        let Some(env) = env else {
            self.dead_letter(
                stream,
                group,
                entry,
                deliveries,
                "unparseable envelope",
                None,
                &counter,
            )
            .await?;
            return Ok(None);
        };
        let scope = self.require_scope(&format!("consume {stream}"))?;
        if env.tenant_id != Some(scope.tenant) || env.job.as_deref() != Some(scope.job.as_str()) {
            let stamped = match (&env.tenant_id, &env.job) {
                (Some(t), Some(j)) => format!("{t}/{j}"),
                _ => "no scope".to_string(),
            };
            let reason = format!("scope_mismatch: stamped {stamped}, stream is {scope}");
            self.dead_letter(
                stream,
                group,
                entry,
                deliveries,
                &reason,
                Some(&env),
                &counter,
            )
            .await?;
            return Ok(None);
        }
        if deliveries > opts.max_deliveries {
            let reason = format!("gave up after {} deliveries", deliveries - 1);
            self.dead_letter(
                stream,
                group,
                entry,
                deliveries,
                &reason,
                Some(&env),
                &counter,
            )
            .await?;
            return Ok(None);
        }
        match handler(env.clone()).await {
            Ok(()) => Ok(Some(Acked {
                entry_id: entry.id.clone(),
                counter,
                env,
            })),
            Err(e) => {
                tracing::warn!(stream, group, entry = %entry.id, topic = %env.topic, deliveries, "handler failed, left pending: {e:#}");
                Ok(None)
            }
        }
    }

    /// Acknowledge a read batch in one round trip: `XACK` for every entry, the counter `DEL`s, and
    /// the console ack frames, all in one pipeline. Non-atomic like the per-entry commands were: a
    /// dropped connection mid-pipeline redelivers whatever it did not ack.
    async fn ack_batch(&self, stream: &str, group: &str, acks: &[Acked]) -> Result<()> {
        let ids: Vec<&str> = acks.iter().map(|a| a.entry_id.as_str()).collect();
        let counters: Vec<&str> = acks.iter().map(|a| a.counter.as_str()).collect();
        let mut c = self.conn.clone();
        let mut pipe = redis::pipe();
        pipe.cmd("XACK").arg(stream).arg(group).arg(&ids);
        pipe.cmd("DEL").arg(&counters);
        for acked in acks {
            if let Some(frame) = self.ack_frame(stream, group, &acked.entry_id, &acked.env) {
                pipe.cmd("PUBLISH").arg(self.streams.console()).arg(frame);
            }
        }
        let _: Vec<redis::Value> = pipe
            .query_async(&mut c)
            .await
            .with_context(|| format!("XACK {stream} {group} ({} entries)", ids.len()))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn dead_letter(
        &self,
        stream: &str,
        group: &str,
        entry: &StreamId,
        deliveries: u64,
        reason: &str,
        env: Option<&Envelope>,
        counter: &str,
    ) -> Result<()> {
        let cid = env.map(|e| e.correlation_id.as_str()).unwrap_or("unknown");
        let dead = Envelope::new(
            "dead.letter",
            group,
            cid,
            serde_json::json!({ "stream": stream, "entry_id": entry.id, "deliveries": deliveries, "reason": reason, "original": env }),
        )?;
        self.publish(&self.streams.dead(), &dead).await?;
        let mut c = self.conn.clone();
        let _: i64 = c.xack(stream, group, &[&entry.id]).await?;
        let _: i64 = c.del(counter).await?;
        if let Some(env) = env
            && let Some(frame) = self.ack_frame(stream, group, &entry.id, env)
        {
            self.mirror.send(&self.streams.console(), frame);
        }
        tracing::error!(stream, group, entry = %entry.id, reason, "dead-lettered");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Streams;

    #[test]
    fn stream_keys_follow_the_department_contract() {
        let s = Streams::new("p:");
        assert_eq!(s.customer("payments"), "p:bus:customer:payments");
        assert_eq!(
            s.worker("logistics", "order"),
            "p:bus:worker:logistics:order"
        );
        assert_eq!(s.worker_results("faq"), "p:bus:worker:faq:results");
        assert_eq!(s.tool("business-rules"), "p:bus:tool:business-rules");
        assert_eq!(s.dead(), "p:bus:dead");
        assert_eq!(s.console(), "p:console");
    }

    #[test]
    fn consumer_group_is_the_recipient_of_the_topic_on_that_stream() {
        let s = Streams::new("p:");
        let g = |stream: &str, topic: &str| s.consumer_group(stream, topic);
        let some = |v: &str| Some(v.to_string());
        // prefixed and unprefixed keys give the same answer
        for pre in ["p:", ""] {
            let k = |k: &str| format!("{pre}{k}");
            assert_eq!(
                g(
                    &k("bus:customer:order-support"),
                    "order-support.returns.request.qualified"
                ),
                some("orchestrator")
            );
            assert_eq!(
                g(&k("bus:customer:faq"), "faq.general.request.resolved"),
                some("interaction")
            );
            assert_eq!(
                g(
                    &k("bus:worker:logistics:order"),
                    "logistics.delivery-status.work.requested"
                ),
                some("worker-order")
            );
            assert_eq!(
                g(
                    &k("bus:worker:payments:results"),
                    "payments.payment-errors.work.failed"
                ),
                some("orchestrator")
            );
            assert_eq!(
                g(&k("bus:tool:crm"), "x.y.tool.call"),
                some("tool-executor")
            );
            assert_eq!(
                g(&k("bus:tool:crm"), "x.y.tool.response"),
                some("worker-crm")
            );
        }
        // a topic on the wrong stream, the dead-letter stream, and unknown topics have no recipient
        assert_eq!(g("p:bus:tool:crm", "x.y.work.requested"), None);
        assert_eq!(g("p:bus:dead", "dead.letter"), None);
        assert_eq!(g("p:bus:customer:faq", "faq.general.marker"), None);
    }

    #[test]
    fn entry_ids_compare_numerically_not_as_strings() {
        assert!(super::id_le("9-0", "10-0"));
        assert!(super::id_le("10-2", "10-2"));
        assert!(!super::id_le("10-10", "10-9"));
        assert!(!super::id_le("1-0", "0-0"));
        assert!(!super::id_le("x", "10-0"));
    }
}
