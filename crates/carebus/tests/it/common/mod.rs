#![allow(dead_code)]
use std::time::{Duration, Instant};

use carebus::Scope;
use carebus::bus::{Bus, ConsumeOptions, Envelope};
use redis::streams::StreamRangeReply;

pub fn fast() -> ConsumeOptions {
    ConsumeOptions {
        batch: 16,
        block: Duration::from_millis(100),
        reclaim_idle: Duration::from_millis(300),
        max_deliveries: 3,
    }
}

/// `fast()` polling for a tool executor, with a reclaim window its own budgets accept
/// (a call must never be reclaimed while it runs). Call after setting the budgets.
pub fn executor_fast<M: carebus::mcp::McpClient>(
    ex: &carebus::executor::ToolExecutor<M>,
) -> ConsumeOptions {
    ConsumeOptions {
        batch: 1,
        reclaim_idle: ex.timeout.max(ex.faq_timeout)
            + carebus::executor::RECLAIM_SLACK
            + Duration::from_secs(1),
        ..fast()
    }
}

/// The root (unscoped) bus under a fresh per-test environment prefix: only the scope registry.
pub async fn root(test: &str) -> Bus {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6391".into());
    let prefix = format!("test:{test}:{}:", uuid::Uuid::new_v4().simple());
    Bus::connect(&url, &prefix)
        .await
        .expect("Redis must be reachable: docker compose up -d")
}

/// `root(test)` scoped to `scope_a()`: what every bus test runs on.
pub async fn bus(test: &str) -> Bus {
    root(test).await.scoped(&scope_a())
}

pub const BOOT_TENANT: &str = "0195f48b-d000-7000-8000-000000000099";
pub const OTHER_TENANT: &str = "0195f48b-d000-7000-8000-0000000000aa";

/// The bootstrap tenant's `customer_care_agent` job.
pub fn scope_a() -> Scope {
    Scope::parse(&format!("{BOOT_TENANT}/customer_care_agent")).unwrap()
}

/// Another tenant, same job key.
pub fn scope_b() -> Scope {
    Scope::parse(&format!("{OTHER_TENANT}/customer_care_agent")).unwrap()
}

/// The environment prefix of a root bus (on a scoped bus the scope follows it).
pub fn prefix(root: &Bus) -> String {
    assert!(root.scope().is_none(), "prefix() takes the root bus");
    root.streams.prefix().to_string()
}

/// Poll `f` every 50 ms until it holds; panic after 5 s.
pub async fn eventually(f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "condition not met within 5 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub async fn entries(bus: &Bus, stream: &str) -> Vec<Envelope> {
    let reply: StreamRangeReply = redis::cmd("XRANGE")
        .arg(stream)
        .arg("-")
        .arg("+")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    reply
        .ids
        .iter()
        .map(|e| serde_json::from_str(&e.get::<String>("envelope").unwrap()).unwrap())
        .collect()
}

/// Poll until `n` entries on `stream` match `pred` (10 s limit).
pub async fn wait_entries(
    bus: &Bus,
    stream: &str,
    n: usize,
    pred: impl Fn(&Envelope) -> bool,
) -> Vec<Envelope> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found: Vec<Envelope> = entries(bus, stream)
            .await
            .into_iter()
            .filter(|e| pred(e))
            .collect();
        if found.len() >= n {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {n} entries on {stream}",
            found.len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
