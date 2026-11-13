//! `tool-executor` — consumes tool.call on bus:tool:* of every registered scope, calls MCP, publishes tool.response.
//! Calls the real MCP server when `MCP_URL` is set; otherwise serves the in-process fake tools (local runs and demos).
//! Real MCP: one client, one session PER TENANT, every call made with the scope tenant's own care-bus licence and
//! strata-rag token, fetched from carebus-api (`CAREBUS_API_URL` / `CAREBUS_API_TOKEN`, crate::mcp_http). A tenant
//! whose credentials cannot be fetched gets no MCP call.
use std::future::Future;
use std::sync::Arc;

use carebus::Scope;
use carebus::executor::ToolExecutor;
use carebus::mcp::{FakeMcp, McpClient};
use carebus::mcp_http::HttpMcp;
use carebus::supervisor::run_per_scope;
use carebus::tools::{FakeTools, ToolError};
use serde_json::Value;

/// One fake MCP shared by every scope's executor.
struct Shared<M>(Arc<M>);

impl<M: McpClient> McpClient for Shared<M> {
    fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> impl Future<Output = Result<Value, ToolError>> + Send {
        self.0.call_tool(name, arguments)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    carebus::init_tracing();
    let root = carebus::bus_from_env().await?;
    let consumer = carebus::consumer_name("tool-executor");
    if carebus::optional_var("MCP_URL").is_some() {
        tracing::info!(%consumer, "tool-executor consuming bus:tool:* per scope -> MCP_URL, as each scope's tenant");
        let mcp = Arc::new(HttpMcp::from_env()?);
        serve(root, consumer, move |s: &Scope| mcp.for_tenant(s.tenant)).await
    } else {
        tracing::info!(%consumer, "tool-executor consuming bus:tool:* per scope (fake MCP; set MCP_URL for the real server)");
        let fake = Arc::new(FakeMcp::new(FakeTools::default()));
        serve(root, consumer, move |_: &Scope| Shared(fake.clone())).await
    }
}

/// One executor per scope, with the MCP client `client` builds for that scope.
async fn serve<M, F>(root: carebus::Bus, consumer: String, client: F) -> anyhow::Result<()>
where
    M: McpClient,
    F: Fn(&Scope) -> M + Send + Sync + 'static,
{
    run_per_scope(root, "tool-executor", move |bus, s| {
        let ex = ToolExecutor::new(bus, client(&s));
        let consumer = consumer.clone();
        async move { Arc::new(ex).run(&consumer).await }
    })
    .await
}
