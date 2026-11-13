//! Licence gate 2 (plan 15) over HTTP, against an in-process licences service + SA token endpoint.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Valid,
    WrongApp,
    NoLicence,
    Down,
}

struct Fake {
    mode: Mutex<Mode>,
    lookups: AtomicUsize,
}

type Shared = Arc<Fake>;

async fn token() -> Json<serde_json::Value> {
    Json(json!({"access_token": "sa-token", "expires_in": 300, "token_type": "Bearer"}))
}

async fn active(
    State(f): State<Shared>,
    headers: axum::http::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    f.lookups.fetch_add(1, Ordering::SeqCst);
    let authed =
        headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer sa-token");
    let tenant = q.get("tenant_id").cloned().unwrap_or_default();
    let selector_ok = q.get("app_key").map(String::as_str) == Some("care-bus")
        && q.get("subject_id") == Some(&format!("svc:{tenant}:care-bus"));
    if !authed || !selector_ok {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "bad_request"})),
        )
            .into_response();
    }
    let mode = *f.mode.lock().unwrap();
    let app = match mode {
        Mode::Valid => "care-bus",
        Mode::WrongApp => "commonpractice",
        Mode::NoLicence => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"detail": {"error": "no_active_license"}})),
            )
                .into_response();
        }
        Mode::Down => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
        }
    };
    Json(json!({
        "active": true, "tenant_id": tenant, "subject_id": "5b0c1e7a-0000-4000-8000-000000000001",
        "license_id": "5b0c1e7a-0000-4000-8000-000000000002", "app": {"key": app},
        "expires_at": "2999-01-01T00:00:00Z", "subject_match": true, "status": "active"
    }))
    .into_response()
}

async fn gated_env(test: &str, mode: Mode) -> (crate::common::TestEnv, Shared) {
    let f: Shared = Arc::new(Fake {
        mode: Mutex::new(mode),
        lookups: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/token", post(token))
        .route("/licenses/active", get(active))
        .with_state(f.clone());
    let url = crate::common::fakes::serve(app).await;
    let e = crate::common::env_with(test, |c| {
        c.licence.enabled = true;
        c.licence.admin_base_url = url.clone();
        c.licence.sa_token_url = format!("{url}/token");
        c.licence.sa_client_id = "care-bus-licence".into();
        c.licence.sa_client_secret = "test-secret".into();
    })
    .await;
    (e, f)
}

async fn procedures(e: &crate::common::TestEnv) -> (StatusCode, serde_json::Value) {
    crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures"),
            Some(&crate::common::user()),
        ),
    )
    .await
}

#[tokio::test]
async fn gate_off_admits_without_a_licences_service() {
    let e = crate::common::env("licence-off").await;
    let (s, b) = procedures(&e).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn valid_licence_admits_and_is_cached() {
    let (e, f) = gated_env("licence-valid", Mode::Valid).await;
    for _ in 0..2 {
        let (s, b) = procedures(&e).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    assert_eq!(
        f.lookups.load(Ordering::SeqCst),
        1,
        "a positive verdict is cached"
    );
    let boot: uuid::Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let cached = e.state.licence.cached(boot).expect("cached licence");
    assert_eq!(
        cached.license_id.as_deref(),
        Some("5b0c1e7a-0000-4000-8000-000000000002")
    );
}

#[tokio::test]
async fn no_licence_is_402_license_required_and_not_cached() {
    let (e, f) = gated_env("licence-none", Mode::NoLicence).await;
    for _ in 0..2 {
        let (s, b) = procedures(&e).await;
        assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{b}");
        assert_eq!(b["error"]["code"], "license_required");
    }
    assert_eq!(
        f.lookups.load(Ordering::SeqCst),
        2,
        "a negative verdict is not cached"
    );
    // the tenant buys a licence: admitted on the next request
    *f.mode.lock().unwrap() = Mode::Valid;
    let (s, _) = procedures(&e).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn outage_is_license_unavailable() {
    let (e, _f) = gated_env("licence-down", Mode::Down).await;
    let (s, b) = procedures(&e).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{b}");
    assert_eq!(b["error"]["code"], "license_unavailable");
    // an unreachable licences service also denies
    let e = crate::common::env_with("licence-unreachable", |c| {
        c.licence.enabled = true;
        c.licence.admin_base_url = "http://127.0.0.1:9".into();
        c.licence.sa_token_url = "http://127.0.0.1:9/token".into();
        c.licence.sa_client_id = "care-bus-licence".into();
        c.licence.sa_client_secret = "test-secret".into();
    })
    .await;
    let (s, b) = procedures(&e).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{b}");
    assert_eq!(b["error"]["code"], "license_unavailable");
}

#[tokio::test]
async fn wrong_app_is_license_required() {
    let (e, _f) = gated_env("licence-app", Mode::WrongApp).await;
    let (s, b) = procedures(&e).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{b}");
    assert_eq!(b["error"]["code"], "license_required");
}
