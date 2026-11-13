//! Router. `/health` is open; everything under `/api` sits behind the bearer layer (Task 5).
pub mod actions;
pub mod admin;
pub mod customers;
pub mod orders;
pub mod records;
pub mod rules;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use serde_json::{Value, json};

use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::{AppState, DemoError};

/// Every protected route, relative to the `/api` nest.
pub fn protected() -> Router<AppState> {
    Router::new()
        .route("/customers", get(customers::search))
        .route("/customers/{id}", get(customers::get_customer))
        .route("/customers/{id}/start-data", get(customers::start_data))
        .route("/orders", get(orders::list))
        .route("/orders/{id}", get(orders::get_order))
        .route("/orders/{id}/payments", get(orders::payments))
        .route("/shipments/{id}", get(orders::shipment))
        .route("/orders/{id}/rmas", post(actions::create_rma))
        .route("/orders/{id}/warranty-claims", post(actions::create_claim))
        .route("/payments/{id}/refunds", post(actions::create_refund))
        .route(
            "/admin/faults",
            get(admin::get_faults).put(admin::put_fault),
        )
        .route("/admin/reset", post(admin::reset))
        .route("/admin/scenarios", get(admin::list_scenarios))
        .route("/admin/scenarios/{name}", post(admin::apply_scenario))
        .route("/admin/records/{table}", get(records::list))
        .route(
            "/admin/records/{table}/{id}",
            get(records::get_one).put(records::update),
        )
        .route("/rules/return-window", get(rules::return_window))
        .route("/rules/warranty", get(rules::warranty))
        .route("/rules/payment-issue", get(rules::payment_issue))
}

pub fn router(state: AppState) -> Router {
    // `layer` (not `route_layer`) so unknown paths and wrong methods under /api are authenticated too.
    let api = protected()
        .fallback(api_not_found)
        .method_not_allowed_fallback(api_method_not_allowed)
        .layer(from_fn_with_state(
            state.clone(),
            crate::auth::require_bearer,
        ));
    Router::new()
        .route("/health", get(health))
        .nest("/api", api)
        .with_state(state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
}

async fn api_not_found() -> DemoError {
    DemoError::NotFound("route".into())
}

async fn api_method_not_allowed() -> DemoError {
    DemoError::MethodNotAllowed
}

/// 200 `{"db":"ok"}` when `SELECT 1` succeeds, 503 `{"db":"down"}` otherwise.
async fn health(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&s.pool)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({"db": "ok"}))),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"db": "down"}))),
    }
}

pub async fn serve(state: AppState, bind: std::net::SocketAddr) -> crate::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await.map_err(|e| {
        crate::DemoError::Config(format!(
            "cannot bind {bind}: {e}. Set DEMO_BACKENDS_BIND to a free port (never 8000)"
        ))
    })?;
    tracing::info!(%bind, "demo-backends listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| crate::DemoError::Config(e.to_string()))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for ctrl-c; ignoring it");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot register SIGTERM handler; ignoring it");
                std::future::pending::<()>().await
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    use super::router;
    use crate::{AppState, Config, faults::FaultState};

    #[tokio::test]
    async fn health_is_503_db_down_when_select_1_fails() {
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(1))
            .connect_lazy("postgres://x:x@127.0.0.1:1/x")
            .unwrap();
        let cfg = Config::from_map(|k| match k {
            "DATABASE_URL" => Some("postgres://x:x@127.0.0.1:1/x".into()),
            "DEMO_BACKENDS_TOKEN" => Some("t".into()),
            _ => None,
        })
        .unwrap();
        let state = AppState {
            pool,
            cfg: Arc::new(cfg),
            faults: FaultState::default(),
        };
        let res = router(state)
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(v["db"], "down");
    }
}
