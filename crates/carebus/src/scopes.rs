//! The scope registry: the SET of scopes the agents serve, at the environment root (`{prefix}scopes`), with a
//! Pub/Sub nudge (`{prefix}scopes:changed`) on every change. The API maintains it from the `jobs` table; the
//! supervisor in every agent process follows it. The root bus touches nothing else.

use anyhow::{Context, Result};
use redis::AsyncCommands;

use crate::bus::Bus;
use crate::scope::Scope;

/// `root_key` -> SET of `<tenant>/<job>`.
pub const SCOPES_KEY: &str = "scopes";
/// `root_key` -> Pub/Sub channel; the payload is ignored (subscribers re-read the SET).
pub const SCOPES_CHANNEL: &str = "scopes:changed";

/// Every registered scope, sorted. Unparseable members are logged and skipped.
pub async fn list(root: &Bus) -> Result<Vec<Scope>> {
    let key = root.root_key(SCOPES_KEY);
    let mut c = root.conn();
    let members: Vec<String> = c
        .smembers(&key)
        .await
        .with_context(|| format!("SMEMBERS {key}"))?;
    let mut all: Vec<Scope> = members
        .iter()
        .filter_map(|m| match Scope::parse(m) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(member = %m, key = %key, "skipping unparseable scope: {e:#}");
                None
            }
        })
        .collect();
    all.sort_by_key(|s| s.to_string());
    Ok(all)
}

/// Register `s` and nudge the supervisors.
pub async fn add(root: &Bus, s: &Scope) -> Result<()> {
    let key = root.root_key(SCOPES_KEY);
    let mut c = root.conn();
    let _: i64 = c
        .sadd(&key, s.to_string())
        .await
        .with_context(|| format!("SADD {key} {s}"))?;
    notify(root).await
}

/// Unregister `s` and nudge the supervisors.
pub async fn remove(root: &Bus, s: &Scope) -> Result<()> {
    let key = root.root_key(SCOPES_KEY);
    let mut c = root.conn();
    let _: i64 = c
        .srem(&key, s.to_string())
        .await
        .with_context(|| format!("SREM {key} {s}"))?;
    notify(root).await
}

/// Replace the whole registry atomically (MULTI: build a temp SET, RENAME it over the registry), then nudge.
/// An empty `all` deletes the registry.
pub async fn replace_all(root: &Bus, all: &[Scope]) -> Result<()> {
    let key = root.root_key(SCOPES_KEY);
    let mut members: Vec<String> = all.iter().map(Scope::to_string).collect();
    members.sort();
    members.dedup();
    let mut pipe = redis::pipe();
    pipe.atomic();
    if members.is_empty() {
        pipe.cmd("DEL").arg(&key).ignore();
    } else {
        let tmp = root.root_key(&format!(
            "{SCOPES_KEY}:tmp:{}",
            uuid::Uuid::new_v4().simple()
        ));
        pipe.cmd("DEL").arg(&tmp).ignore();
        pipe.cmd("SADD").arg(&tmp).arg(&members).ignore();
        pipe.cmd("RENAME").arg(&tmp).arg(&key).ignore();
    }
    let mut c = root.conn();
    pipe.query_async::<()>(&mut c)
        .await
        .with_context(|| format!("replace {key} with {} scopes", members.len()))?;
    notify(root).await
}

async fn notify(root: &Bus) -> Result<()> {
    let channel = root.root_key(SCOPES_CHANNEL);
    let mut c = root.conn();
    let _: i64 = c
        .publish(&channel, "changed")
        .await
        .with_context(|| format!("PUBLISH {channel}"))?;
    Ok(())
}
