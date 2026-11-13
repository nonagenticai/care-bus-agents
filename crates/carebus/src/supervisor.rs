//! One copy of an agent's run loop per registered scope.
//!
//! The supervisor reads the registry (`scopes::list`) at start, follows `scopes:changed` on a dedicated Pub/Sub
//! connection, and also resyncs every 30 s, so a missed message only delays a change. A new scope gets
//! `start(root.scoped(&s), s)` in its own task; a removed scope's task is aborted. A task that returns (with an
//! error or not) or panics is restarted after 5 s. A lost subscription reconnects after the same 5 s.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{FutureExt, StreamExt};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::bus::Bus;
use crate::scope::Scope;
use crate::scopes;

/// Periodic resync, in case a `scopes:changed` message was missed.
pub const RESYNC: Duration = Duration::from_secs(30);
/// Delay before restarting a failed scope task or reconnecting the subscription.
pub const BACKOFF: Duration = Duration::from_secs(5);

/// The running scope tasks. Dropping it (the supervisor's own task aborted) aborts them all, so no scope keeps
/// running unsupervised.
struct Running(HashMap<Scope, JoinHandle<()>>);

impl Drop for Running {
    fn drop(&mut self) {
        for h in self.0.values() {
            h.abort();
        }
    }
}

/// Run `start` once per registered scope until the process ends. Never returns `Ok`.
pub async fn run_per_scope<F, Fut>(root: Bus, role: &str, start: F) -> Result<()>
where
    F: Fn(Bus, Scope) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let start = Arc::new(start);
    let role: Arc<str> = Arc::from(role);
    let mut running = Running(HashMap::new());
    let channel = root.root_key(scopes::SCOPES_CHANNEL);
    let mut tick = tokio::time::interval(RESYNC);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        // Subscribe before reading the registry, so a change between the two is not lost.
        let subscribed = async {
            let mut ps = root.client().get_async_pubsub().await?;
            ps.subscribe(&channel).await?;
            Ok::<_, redis::RedisError>(ps)
        }
        .await;
        sync(&root, &role, &start, &mut running).await;
        let mut messages = match subscribed {
            Ok(ps) => ps.into_on_message(),
            Err(e) => {
                tracing::warn!(role = %role, channel = %channel, error = %e, "scope subscription failed; retrying in 5 s");
                tokio::time::sleep(BACKOFF).await;
                continue;
            }
        };
        tracing::info!(role = %role, channel = %channel, "following the scope registry");
        loop {
            tokio::select! {
                m = messages.next() => match m {
                    Some(_) => sync(&root, &role, &start, &mut running).await,
                    None => {
                        tracing::warn!(role = %role, channel = %channel, "scope subscription closed; reconnecting in 5 s");
                        break;
                    }
                },
                _ = tick.tick() => sync(&root, &role, &start, &mut running).await,
            }
        }
        tokio::time::sleep(BACKOFF).await;
    }
}

/// Start the registered scopes that are not running and abort the running ones that are no longer registered.
/// A registry read error keeps the current set.
async fn sync<F, Fut>(root: &Bus, role: &Arc<str>, start: &Arc<F>, running: &mut Running)
where
    F: Fn(Bus, Scope) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let want: HashSet<Scope> = match scopes::list(root).await {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            tracing::warn!(role = %role, "reading the scope registry failed; keeping the running scopes: {e:#}");
            return;
        }
    };
    running.0.retain(|s, h| {
        let keep = want.contains(s);
        if !keep {
            h.abort();
            tracing::info!(role = %role, scope = %s, "scope removed; stopped");
        }
        keep
    });
    for s in want {
        if running.0.contains_key(&s) {
            continue;
        }
        tracing::info!(role = %role, scope = %s, "scope registered; starting");
        let h = spawn_scope(root.scoped(&s), s.clone(), role.clone(), start.clone());
        running.0.insert(s, h);
    }
}

/// `start(bus, scope)` in a task that restarts it 5 s after it returns or panics, until aborted.
fn spawn_scope<F, Fut>(bus: Bus, scope: Scope, role: Arc<str>, start: Arc<F>) -> JoinHandle<()>
where
    F: Fn(Bus, Scope) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let run = AssertUnwindSafe((*start)(bus.clone(), scope.clone()))
                .catch_unwind()
                .await;
            match run {
                Ok(Ok(())) => {
                    tracing::warn!(role = %role, scope = %scope, "scope task returned; restarting in 5 s")
                }
                Ok(Err(e)) => {
                    tracing::error!(role = %role, scope = %scope, "scope task failed; restarting in 5 s: {e:#}")
                }
                Err(_) => {
                    tracing::error!(role = %role, scope = %scope, "scope task panicked; restarting in 5 s")
                }
            }
            tokio::time::sleep(BACKOFF).await;
        }
    })
}
