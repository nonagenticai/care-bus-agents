use axum::Router;

use crate::state::AppState;

pub mod agents;
pub mod analytics;
pub mod cases;
pub mod connectors;
pub mod conversations;
pub mod demo;
pub mod events;
pub mod gates;
pub mod health;
pub mod ingest;
pub mod knowledge;
pub mod knowledge_eval;
pub mod knowledge_stores;
pub mod legal_matters;
pub mod llm_providers;
pub mod mcp_credentials;
pub mod mcp_tasks;
pub mod ontology;
pub mod overview;
pub mod procedures;
pub mod providers;
pub mod refinements;
pub mod releases;
pub mod research;
pub mod settings;
pub mod signups;
pub mod sources;
pub mod streams;
pub mod telegram;
pub mod tenancy;
pub mod tools;
pub mod usage;

/// Job-scoped routes live under `/api/jobs/{job}`; the files below declare their paths RELATIVE to that prefix
/// (`/connectors/{id}`), and their guards (`JobMember`, `JobAdmin`, `ProcedureReader`) read the `{job}` segment.
/// Tenant-level routes keep absolute paths and are merged.
pub fn all() -> Router<AppState> {
    Router::new()
        .nest("/api/jobs/{job}", job_routes())
        .merge(gates::internal_routes())
        .merge(knowledge::internal_routes())
        .merge(mcp_credentials::internal_routes())
        .merge(health::routes())
        .merge(crate::audit::routes())
        .merge(ingest::routes())
        .merge(knowledge::routes())
        .merge(knowledge_eval::routes())
        .merge(llm_providers::routes())
        .merge(providers::routes())
        .merge(tenancy::routes())
        .merge(telegram::routes())
        .merge(signups::routes())
}

fn job_routes() -> Router<AppState> {
    Router::new()
        .merge(agents::routes())
        .merge(analytics::routes())
        .merge(cases::routes())
        .merge(connectors::routes())
        .merge(conversations::routes())
        .merge(demo::routes())
        .merge(events::routes())
        .merge(gates::routes())
        .merge(knowledge_stores::routes())
        .merge(legal_matters::routes())
        .merge(mcp_tasks::routes())
        .merge(ontology::routes())
        .merge(overview::routes())
        .merge(procedures::routes())
        .merge(refinements::routes())
        .merge(settings::routes())
        .merge(releases::routes())
        .merge(research::routes())
        .merge(sources::routes())
        .merge(streams::routes())
        .merge(tools::routes())
        .merge(usage::routes())
}
