//! One integration-test binary per crate: a single link, and libtest runs every test in parallel.
mod ack;
mod common;
mod deadline;
mod effective_tenant;
mod executor;
mod faq_scope;
mod flow;
mod gates;
mod live_mcp;
mod live_mcp_tasks;
mod live_vllm;
mod mcp_http;
mod mcp_tasks;
mod procedures_http;
mod router_identity;
mod spaces_http;
mod supervisor;
