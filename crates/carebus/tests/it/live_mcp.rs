//! Against a real nonagenticai_mcp_server with the care-bus tools (plan 03), as one bus tenant whose credentials
//! carebus-api hands out.
//! Run: MCP_URL=… CAREBUS_API_URL=… CAREBUS_API_TOKEN=… LIVE_TENANT_ID=<uuid> cargo test -p carebus --test it live_mcp -- --ignored
use std::sync::Arc;

use carebus::mcp::McpClient;
use carebus::mcp_http::HttpMcp;

#[tokio::test]
#[ignore]
async fn live_order_get_order() {
    let (Ok(_), Ok(tenant)) = (std::env::var("MCP_URL"), std::env::var("LIVE_TENANT_ID")) else {
        eprintln!("MCP_URL or LIVE_TENANT_ID not set; skipping");
        return;
    };
    let mcp = Arc::new(HttpMcp::from_env().unwrap());
    let v = mcp
        .for_tenant(tenant.parse().unwrap())
        .call_tool(
            "order_get_order",
            serde_json::json!({"order_id": "ORD-58213"}),
        )
        .await
        .unwrap();
    assert_eq!(v["order_id"], "ORD-58213");
}
