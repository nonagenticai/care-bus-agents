//! `worker <tool>` — one process per tool, serving every registered scope; scale by running more of the same.
use std::sync::Arc;

use anyhow::Context;
use carebus::spaces::{EnvSpaces, REFRESH, run_with_catalog};
use carebus::supervisor::run_per_scope;
use carebus::worker::Worker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    carebus::init_tracing();
    let tool = std::env::args()
        .nth(1)
        .context("usage: worker <crm|order|delivery|faq|business-rules|procedures>")?;
    let root = carebus::bus_from_env().await?;
    let consumer = carebus::consumer_name(&format!("worker-{tool}"));
    // D5: the faq tool gets TOOL_TIMEOUT_MS_FAQ (strata_rag generation); others TOOL_TIMEOUT_MS.
    let timeout = carebus::executor::tool_timeout(&tool, |k| std::env::var(k).ok());
    tracing::info!(%consumer, "worker consuming bus:worker:*:{tool} and tool.response on bus:tool:{tool}, per scope");
    let role = format!("worker-{tool}");
    // Each scope's context spaces (work streams per area, routing per request type): from carebus-api when CAREBUS_API_URL is set, else the bootstrap job's.
    let spaces = EnvSpaces::from_env()?;
    // Tool gates (spec 2026-10-18 §3) through carebus-api: CAREBUS_API_URL + CAREBUS_API_TOKEN required unless
    // CAREBUS_GATES=off.
    let gates = carebus::gates::GateClient::from_env()?;
    run_per_scope(root, &role, move |bus, s| {
        let mut w = Worker::new(bus, tool.clone());
        w.timeout = timeout;
        w.gates = gates.clone();
        let src = spaces.for_scope(&s);
        let consumer = consumer.clone();
        async move {
            run_with_catalog(&src, REFRESH, |spaces| {
                Arc::new(w.with_spaces(spaces)).run(&consumer)
            })
            .await
        }
    })
    .await
}
