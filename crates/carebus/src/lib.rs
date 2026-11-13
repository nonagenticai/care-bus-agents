//! Customer-care agents decoupled by Redis Streams, one set of streams per department.
//!
//! interaction -> bus:customer:{area} (request.qualified) -> orchestrator
//! -> bus:worker:{area}:{tool} (work.requested) -> worker
//! -> bus:tool:{tool} (tool.call) -> tool-executor -> MCP
//! tool-executor -> bus:tool:{tool} (tool.response) -> worker
//! -> bus:worker:{area}:results -> orchestrator
//! -> bus:customer:{area} (request.resolved) -> interaction
//!
//! Every publish is mirrored on the Pub/Sub channel `console`.
//!
//! All of the above exists once per scope (tenant, job): a scoped `Bus` puts every key under
//! `{CAREBUS_PREFIX}s:{tenant}:{job}:` and stamps the scope on every envelope it publishes.

pub mod bus;
pub mod case;
pub mod chat;
pub mod domain;
pub mod executor;
pub mod gates;
pub mod identity;
pub mod interaction;
pub mod mcp;
pub mod mcp_http;
pub mod mcp_tasks;
pub mod orchestrator;
pub mod procedures;
pub mod scope;
pub mod scopes;
pub mod spaces;
pub mod supervisor;
pub mod tools;
pub mod worker;

pub use bus::{Bus, ConsumeOptions, Envelope, Streams};
pub use scope::Scope;

/// The root (unscoped) bus from `REDIS_URL` (default the local compose Redis) and `CAREBUS_PREFIX`. It only
/// reaches the scope registry; agents run on `root.scoped(&s)` via `supervisor::run_per_scope`.
pub async fn bus_from_env() -> anyhow::Result<Bus> {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6391".into());
    let prefix = std::env::var("CAREBUS_PREFIX").unwrap_or_default();
    Bus::connect(&url, &prefix).await
}

/// A required setting: unset and empty are both errors, with the variable's name.
pub(crate) fn required_var(name: &str, value: Option<String>) -> anyhow::Result<String> {
    match value {
        Some(v) if !v.is_empty() => Ok(v),
        Some(_) => anyhow::bail!("{name} is set but empty"),
        None => anyhow::bail!("{name} not set"),
    }
}

/// An optional setting: empty counts as unset.
pub fn optional_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn consumer_name(role: &str) -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into());
    format!("{role}-{host}-{}", std::process::id())
}

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
}
