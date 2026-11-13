//! `researcher`: claims queued research runs (plan 18, migration 0020) and drives them to a verified report.
//! Ships in the carebus-api image. It runs no migrations (carebus-api does) and serves nothing.
//!
//! Env: `DATABASE_APP_URL` (the RLS runtime role, as carebus-api), `CAREBUS_SOURCE_KEY` (tenant provider tokens),
//! `STRATA_RAG_URL`, `MCP_URL` with `CAREBUS_API_URL` / `CAREBUS_API_TOKEN` (every MCP call is made as the run's
//! tenant, with that tenant's care-bus licence and strata-rag token from carebus-api
//! `POST /api/internal/mcp-credentials`; carebus::mcp_http; `strata_search` uses the same per-tenant strata-rag
//! token, so without `MCP_URL` there is no `strata_search`),
//! the platform model (`RESEARCH_LLM_*`, see research::llm::DefaultLlm), `RESEARCHER_CONCURRENCY` (default 2),
//! `RESEARCHER_LEASE_S` (default 600), `RESEARCHER_POLL_MS` (default 2000), `RESEARCH_LLM_TIMEOUT_S` (default 300),
//! `RESEARCHER_MAX_ATTEMPTS` (default 3: a run claimed more often fails with `too_many_attempts`),
//! `RESEARCHER_HEARTBEAT_S` (default lease / 3: lease renewal while a model or tool call is in flight),
//! `METERING_EMIT_ENABLED` (as carebus-api: `true` also writes the billing outbox; the usage ledger is always written),
//! `SERVICE_ACTOR_ID` (care-bus' service-account sub: `X-Actor-Id` of platform-model and strata_rag calls when the
//! run's creator is not a uuid subject; optional, malformed stops startup).
//!
//! MCP tasks (migration 0029, carebus_api::mcp_tasks): `MCP_TASK_TOOLS` (comma-separated MCP tool names started as
//! long-running tasks; default none), `MCP_TASK_LEASE_S` (default 30) and `MCP_TASK_BATCH` (default 20). With
//! `MCP_URL` set, one poller here polls every tenant's open tasks.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use carebus::mcp_http::HttpMcp;
use carebus_api::config::parse_source_key;
use carebus_api::mcp_tasks::TaskPoller;
use carebus_api::research::llm::{DefaultLlm, OpenAiLlm};
use carebus_api::research::runner::{Runner, RunnerConfig, owner_label};
use carebus_api::research::tools::{LiveTools, StrataCfg};
use sqlx::postgres::PgPoolOptions;
use tokio::sync::watch;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

fn num<T: std::str::FromStr>(k: &str, default: T) -> Result<T> {
    match env(k) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("{k} must be a number")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    carebus::init_tracing();
    let db_url = env("DATABASE_APP_URL")
        .or_else(|| env("CAREBUS_DATABASE_APP_URL"))
        .context("DATABASE_APP_URL is not set")?;
    let concurrency: usize = num("RESEARCHER_CONCURRENCY", 2usize)?.clamp(1, 32);
    let lease_s: i32 = num("RESEARCHER_LEASE_S", 600i32)?.clamp(30, 3600);
    let max_attempts: i32 = num("RESEARCHER_MAX_ATTEMPTS", 3i32)?.clamp(1, 100);
    let poll = Duration::from_millis(num("RESEARCHER_POLL_MS", 2000u64)?.max(100));
    let llm_timeout = Duration::from_secs(num("RESEARCH_LLM_TIMEOUT_S", 300u64)?.max(5));
    let heartbeat = match env("RESEARCHER_HEARTBEAT_S") {
        None => None,
        Some(_) => Some(Duration::from_secs(
            num("RESEARCHER_HEARTBEAT_S", 0u64)?.max(1),
        )),
    };
    if heartbeat.is_some_and(|h| h.as_secs() >= lease_s as u64 / 2) {
        anyhow::bail!("RESEARCHER_HEARTBEAT_S must be under half of RESEARCHER_LEASE_S");
    }
    let metering_emit = env("METERING_EMIT_ENABLED").as_deref() == Some("true");
    if llm_timeout.as_secs() >= lease_s as u64 {
        anyhow::bail!("RESEARCH_LLM_TIMEOUT_S must be shorter than RESEARCHER_LEASE_S");
    }

    let db = PgPoolOptions::new()
        .max_connections(u32::try_from(concurrency).unwrap_or(32) * 2 + 2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&db_url)
        .await
        .context("connect DATABASE_APP_URL (runtime role)")?;
    // Same refusal as carebus-api: RLS must apply to every query.
    let bypass: bool = sqlx::query_scalar(
        "SELECT r.rolsuper OR r.rolbypassrls OR EXISTS (
                    SELECT 1 FROM pg_tables t WHERE t.schemaname = 'public' AND t.tableowner = current_user)
           FROM pg_roles r WHERE r.rolname = current_user",
    )
    .fetch_one(&db)
    .await
    .context("read the runtime role's attributes")?;
    if bypass {
        anyhow::bail!(
            "DATABASE_APP_URL connects as a role that bypasses RLS; it must be carebus_app"
        );
    }

    let source_key = parse_source_key(env("CAREBUS_SOURCE_KEY"))?;
    let service_actor = carebus::identity::service_actor_from_env()?;
    let mcp = match env("MCP_URL") {
        None => {
            tracing::warn!(
                "MCP_URL is not set: no MCP tools, and legal citations cannot be re-resolved"
            );
            None
        }
        Some(_) => Some(Arc::new(HttpMcp::from_vars(env)?)),
    };
    // Each tenant's strata-rag token comes with its MCP credentials: no MCP, no strata_search.
    let strata = match (env("STRATA_RAG_URL"), mcp.is_some()) {
        (None, _) => None,
        (Some(_), false) => {
            tracing::warn!(
                "STRATA_RAG_URL is set but MCP_URL is not: strata_search is unavailable (each tenant's strata token \
                 comes from carebus-api's MCP credentials)"
            );
            None
        }
        (Some(url), true) => Some(StrataCfg { url, service_actor }),
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let task_tools =
        carebus::mcp_tasks::task_tools(env(carebus::mcp_tasks::TASK_TOOLS_VAR).as_deref());
    let task_lease_s: i32 = num("MCP_TASK_LEASE_S", 30i32)?.clamp(5, 3600);
    let task_batch: i32 = num("MCP_TASK_BATCH", 20i32)?.clamp(1, 100);
    // MCP tasks run on catalog tools, which answer only the BINDING caller (the tenant's mcp licence plus its
    // binding, provisioned by carebus-api: carebus_api::mcp_binding).
    let catalog = match mcp {
        Some(_) => Some(Arc::new(HttpMcp::catalog_from_vars(env)?)),
        None => None,
    };
    let poller = catalog.clone().map(|mcp| {
        Arc::new(TaskPoller {
            db: db.clone(),
            mcp,
            lease_s: task_lease_s,
            batch: task_batch,
        })
    });
    let mut tools = LiveTools::new(mcp, strata, http);
    match catalog {
        Some(c) => tools = tools.with_tasks(db.clone(), task_tools, c),
        None if !task_tools.is_empty() => {
            tracing::warn!("MCP_TASK_TOOLS is set but MCP_URL is not: no MCP tasks")
        }
        None => {}
    }
    let tools = Arc::new(tools);
    let default_llm = DefaultLlm::from_lookup(&|k| std::env::var(k).ok());
    if default_llm.token.is_none() {
        tracing::warn!(
            "neither RESEARCH_LLM_TOKEN nor VLLM_LICENSE_TOKEN is set: the platform model is called without a bearer"
        );
    }
    tracing::info!(concurrency, lease_s, metering_emit, base_url = %default_llm.base_url, model = %default_llm.model, "researcher starting");

    let (tx, rx) = watch::channel(false);
    let mut workers = tokio::task::JoinSet::new();
    for i in 0..concurrency {
        let mut cfg = RunnerConfig::new(owner_label(i), default_llm.clone());
        cfg.lease_s = lease_s;
        cfg.llm_timeout = llm_timeout;
        cfg.source_key = source_key.clone();
        cfg.max_attempts = max_attempts;
        cfg.heartbeat = heartbeat;
        cfg.metering_emit = metering_emit;
        cfg.service_actor = service_actor;
        let runner = Arc::new(Runner {
            db: db.clone(),
            cfg,
            llm: OpenAiLlm,
            tools: tools.clone(),
            resolver: tools.clone(),
        });
        workers.spawn(runner.work(rx.clone(), poll));
    }
    if let Some(p) = poller {
        workers.spawn(p.work(rx.clone(), poll));
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM: finishing the current steps, then releasing leases"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT: finishing the current steps, then releasing leases"),
    }
    let _ = tx.send(true);
    while workers.join_next().await.is_some() {}
    db.close().await;
    tracing::info!("researcher stopped");
    Ok(())
}
