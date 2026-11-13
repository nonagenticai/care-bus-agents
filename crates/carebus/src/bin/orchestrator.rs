use std::sync::Arc;
use std::time::Duration;

use carebus::case::DEFAULT_CASE_DEADLINE;
use carebus::domain::StaticProcedures;
use carebus::orchestrator::Orchestrator;
use carebus::procedures::HttpProcedures;
use carebus::spaces::{EnvSpaces, REFRESH, run_with_catalog};
use carebus::supervisor::run_per_scope;

/// One orchestrator per registered scope (tenant, job), each planning with its own scope's procedures.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    carebus::init_tracing();
    let root = carebus::bus_from_env().await?;
    let consumer = carebus::consumer_name("orchestrator");
    let deadline = case_deadline()?;
    if carebus::optional_var("CAREBUS_API_URL").is_some() {
        tracing::info!(%consumer, "orchestrator: procedures from carebus-api, per scope");
        let procs = HttpProcedures::from_env()?;
        let spaces = EnvSpaces::from_env()?;
        run_per_scope(root, "orchestrator", move |bus, s| {
            let o = Orchestrator::new(bus, procs.for_scope(&s)).with_case_deadline(deadline);
            let src = spaces.for_scope(&s);
            let consumer = consumer.clone();
            async move {
                run_with_catalog(&src, REFRESH, |spaces| {
                    Arc::new(o.with_spaces(spaces)).run(&consumer)
                })
                .await
            }
        })
        .await
    } else {
        tracing::info!(%consumer, "orchestrator: built-in procedures (set CAREBUS_API_URL for the registry)");
        run_per_scope(root, "orchestrator", move |bus, _s| {
            let o = Orchestrator::new(bus, StaticProcedures).with_case_deadline(deadline);
            let consumer = consumer.clone();
            async move { Arc::new(o).run(&consumer).await }
        })
        .await
    }
}

/// `CASE_DEADLINE_MS`: optional; unset or empty keeps the default, anything else must be a positive integer.
fn case_deadline() -> anyhow::Result<Duration> {
    match carebus::optional_var("CASE_DEADLINE_MS") {
        None => Ok(DEFAULT_CASE_DEADLINE),
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => Ok(Duration::from_millis(ms)),
            _ => anyhow::bail!("CASE_DEADLINE_MS must be a positive integer (milliseconds)"),
        },
    }
}
