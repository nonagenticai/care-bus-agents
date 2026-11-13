pub mod audit;
pub mod auth;
pub mod chat_q;
pub mod config;
pub mod crypto;
pub mod error;
pub mod gates;
pub mod ingest;
pub mod licence;
pub mod mcp_binding;
pub mod mcp_credentials;
pub mod mcp_tasks;
pub mod metering;
pub mod netguard;
pub mod page;
pub mod recorder;
pub mod redis_admin;
pub mod refine;
pub mod release;
pub mod research;
pub mod routes;
pub mod runs;
pub mod secrets;
pub mod signup;
pub mod sources;
pub mod state;
pub mod strata;
pub mod strata_tokens;
pub mod tabular;
pub mod telegram;
pub mod tenancy;
pub mod upstream;
pub mod walls;

use axum::Router;
use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::CorsLayer;

pub fn router(state: state::AppState) -> Router {
    let cors = cors_layer(&state.cfg.cors_origins);
    let app = routes::all()
        .fallback(error::fallback)
        .method_not_allowed_fallback(error::method_not_allowed)
        .with_state(state);
    match cors {
        Some(layer) => app.layer(layer),
        None => app,
    }
}

/// `None` when no origin is configured: no CORS headers are ever sent.
fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    let allowed: Vec<HeaderValue> = origins.iter().filter_map(|o| o.parse().ok()).collect();
    if allowed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(allowed)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                HeaderName::from_static("authorization"),
                HeaderName::from_static("content-type"),
                HeaderName::from_static("last-event-id"),
                HeaderName::from_static("x-active-tenant-id"),
            ]),
    )
}
