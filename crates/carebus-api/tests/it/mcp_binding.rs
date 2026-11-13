//! MCP catalog bindings (carebus_api::mcp_binding, migration 0030) through `POST /api/internal/mcp-credentials
//! {kind: binding}`: the tenant's mcp licence is minted on bundle care-bus, the binding is provisioned once with
//! `bindings/ensure` as the minter's service account and kept sealed, a lost token is rotated, a 401 is retried with a
//! fresh service-account token, two refusals rotate, and a refused ensure is a 503 that names the cause. One fake plays
//! Keycloak, licences and the MCP server's internal route.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use carebus_api::crypto::SourceKey;
use carebus_api::mcp_binding::{Bindings, credentials};
use carebus_api::signup::config::ClientCredentials;
use carebus_api::strata_tokens::BundleTokenConfig;
use serde_json::{Value, json};
use uuid::Uuid;

const SVC: &str = "svc-test-token";
const LICENCE_ID: Uuid = Uuid::from_u128(0x10c);
const BINDING_ID: Uuid = Uuid::from_u128(0xb1);

#[derive(Default)]
struct Mock {
    /// Every service-account token issued (`sa-1`, `sa-2`, ...).
    sa_tokens: usize,
    /// (app, bundle) of every licences mint.
    mints: Vec<(String, Option<String>)>,
    /// (bearer, body) of every bindings/ensure.
    ensures: Vec<(String, Value)>,
    /// tenant -> tokens issued so far (a binding exists once this is > 0).
    issued: HashMap<String, usize>,
    /// Answer the next ensure 401 (an expired service-account token).
    refuse_401_once: bool,
    /// Answer every ensure with this status.
    refuse: Option<u16>,
}

type Shared = Arc<Mutex<Mock>>;

fn lock(m: &Shared) -> std::sync::MutexGuard<'_, Mock> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

async fn sa_token(State(m): State<Shared>) -> Json<Value> {
    let n = {
        let mut g = lock(&m);
        g.sa_tokens += 1;
        g.sa_tokens
    };
    Json(json!({"access_token": format!("sa-{n}"), "expires_in": 300}))
}

async fn mint(
    State(m): State<Shared>,
    Path(tenant): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let app = q.get("app").cloned().unwrap_or_default();
    let n = {
        let mut g = lock(&m);
        g.mints.push((app.clone(), q.get("bundle").cloned()));
        g.mints.len()
    };
    let expires = chrono::Utc::now() + chrono::Duration::minutes(15);
    (
        StatusCode::CREATED,
        Json(
            json!({"token": format!("tok:{app}:{n}"), "expires_at": expires.to_rfc3339(),
                    "tenant_id": tenant, "bundle": "care-bus", "app": app,
                    "license_id": LICENCE_ID, "token_id": format!("t-{n}")}),
        ),
    )
        .into_response()
}

async fn ensure(
    State(m): State<Shared>,
    Path(tenant): Path<String>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut g = lock(&m);
    g.ensures.push((bearer, body.clone()));
    if g.refuse_401_once {
        g.refuse_401_once = false;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(s) = g.refuse {
        return (
            StatusCode::from_u16(s).unwrap(),
            Json(json!({"detail": "refused"})),
        )
            .into_response();
    }
    let rotate = body["rotate"].as_bool().unwrap_or(false);
    let n = g.issued.entry(tenant.clone()).or_default();
    let base = json!({"binding_id": BINDING_ID, "tenant_id": tenant, "product": "care-bus",
                      "gateway_license_id": LICENCE_ID, "toolkits": [], "skipped": []});
    if *n > 0 && !rotate {
        let mut b = base;
        b["token_already_issued"] = json!(true);
        b["status"] = json!("unchanged");
        return (StatusCode::OK, Json(b)).into_response();
    }
    *n += 1;
    let mut b = base;
    b["token"] = json!(format!("binding:{tenant}:{n}"));
    b["token_already_issued"] = json!(false);
    b["status"] = json!(if rotate { "rotated" } else { "created" });
    (StatusCode::CREATED, Json(b)).into_response()
}

async fn fake() -> (String, Shared) {
    let m: Shared = Arc::new(Mutex::new(Mock::default()));
    let router = Router::new()
        .route("/token", post(sa_token))
        .route("/lic/internal/tenants/{tenant}/bundle-token", post(mint))
        .route(
            "/mcp/internal/tenants/{tenant}/bindings/ensure",
            post(ensure),
        )
        .with_state(m.clone());
    (crate::common::fakes::serve(router).await, m)
}

fn key() -> SourceKey {
    use base64::Engine;
    SourceKey::from_base64(&base64::engine::general_purpose::STANDARD.encode([3u8; 32])).unwrap()
}

/// carebus-api with the minter and the binding caller wired to the fake (build_for_tests leaves them off).
async fn env(test: &str) -> (crate::common::TestEnv, axum::Router, Shared, Arc<Bindings>) {
    let (base, m) = fake().await;
    let mut e = crate::common::env_with(test, |c| {
        c.strata_bundle = Some(BundleTokenConfig {
            url: format!("{base}/lic"),
            creds: ClientCredentials {
                token_url: format!("{base}/token"),
                client_id: "care-bus-bundle-minter".into(),
                client_secret: "minter-secret".into(),
            },
            ttl_minutes: 15,
        });
    })
    .await;
    let b = Arc::new(Bindings::new(
        e.state.strata_tokens.clone(),
        reqwest::Client::new(),
        base.clone(),
        key(),
        e.state.db.clone(),
    ));
    e.state.bindings = Some(b.clone());
    e.state.mcp_catalog = Some(Arc::new(carebus::mcp_http::HttpMcp::new(
        format!("{base}/mcp"),
        credentials(b.clone()),
    )));
    let app = carebus_api::router(e.state.clone());
    (e, app, m, b)
}

async fn ask(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    crate::common::send(
        app,
        crate::common::json("POST", "/api/internal/mcp-credentials", Some(SVC), body),
    )
    .await
}

fn boot() -> Uuid {
    crate::common::BOOT_TENANT.parse().unwrap()
}

#[tokio::test]
async fn a_tenants_binding_is_provisioned_once_kept_sealed_and_served_with_its_mcp_licence() {
    let (e, app, m, _) = env("mcp-binding-provision").await;
    let (s, b) = ask(&app, json!({"tenant_id": boot(), "kind": "binding"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let token = format!("binding:{}:1", boot());
    assert_eq!(b["binding_token"], token);
    assert!(
        b["licence_token"].as_str().unwrap().starts_with("tok:mcp:"),
        "the gateway licence is the tenant's mcp licence: {b}"
    );
    assert!(
        b["strata_token"]
            .as_str()
            .unwrap()
            .starts_with("tok:strata-rag:")
    );
    {
        let g = lock(&m);
        assert!(
            g.mints.contains(&("mcp".into(), Some("care-bus".into()))),
            "{:?}",
            g.mints
        );
        assert_eq!(g.ensures.len(), 1);
        let (bearer, body) = &g.ensures[0];
        assert_eq!(bearer, "Bearer sa-1", "as the minter's service account");
        assert_eq!(body["product"], "care-bus");
        assert_eq!(body["licence_id"], json!(LICENCE_ID));
        assert_eq!(body["licence_token"], b["licence_token"]);
        assert_eq!(body["rotate"], false);
    }
    // Stored sealed: the row holds no plaintext token.
    let (binding, licence, enc): (Uuid, Uuid, String) = sqlx::query_as(
        "SELECT binding_id, licence_id, token_enc FROM mcp_bindings WHERE tenant_id = $1 AND product = 'care-bus'",
    )
    .bind(boot())
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!((binding, licence), (BINDING_ID, LICENCE_ID));
    assert!(!enc.contains("binding:"), "sealed");

    // The next ask is served from the stored binding: no second ensure.
    let (s, b) = ask(&app, json!({"tenant_id": boot(), "kind": "binding"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["binding_token"], token);
    assert_eq!(lock(&m).ensures.len(), 1);

    // The builtin caller is unchanged: the care-bus licence, no binding.
    let (s, b) = ask(&app, json!({"tenant_id": boot()})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b.get("binding_token").is_none());
    assert!(
        b["licence_token"]
            .as_str()
            .unwrap()
            .starts_with("tok:care-bus:")
    );
}

#[tokio::test]
async fn a_binding_care_bus_lost_is_rotated_and_repeated_refusals_rotate() {
    let (e, app, m, _) = env("mcp-binding-rotate").await;
    let (s, _) = ask(&app, json!({"tenant_id": boot(), "kind": "binding"})).await;
    assert_eq!(s, StatusCode::OK);
    // The stored token is lost: the server says one was already issued, so care-bus rotates.
    sqlx::query("DELETE FROM mcp_bindings")
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    let (s, b) = ask(&app, json!({"tenant_id": boot(), "kind": "binding"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["binding_token"], format!("binding:{}:2", boot()));
    let rotates: Vec<bool> = lock(&m)
        .ensures
        .iter()
        .map(|(_, b)| b["rotate"].as_bool().unwrap())
        .collect();
    assert_eq!(rotates, [false, false, true]);

    // One refusal: fresh licences, same binding. A second within the window: the binding is rotated.
    let (_, b) = ask(
        &app,
        json!({"tenant_id": boot(), "kind": "binding", "fresh": true}),
    )
    .await;
    assert_eq!(b["binding_token"], format!("binding:{}:2", boot()));
    assert_eq!(lock(&m).ensures.len(), 3);
    let (_, b) = ask(
        &app,
        json!({"tenant_id": boot(), "kind": "binding", "fresh": true}),
    )
    .await;
    assert_eq!(b["binding_token"], format!("binding:{}:3", boot()));
    assert_eq!(lock(&m).ensures.last().unwrap().1["rotate"], true);
}

#[tokio::test]
async fn an_expired_service_token_is_renewed_once_and_a_refusal_is_a_503_naming_it() {
    let (_e, app, m, _) = env("mcp-binding-refused").await;
    lock(&m).refuse_401_once = true;
    let (s, b) = ask(&app, json!({"tenant_id": boot(), "kind": "binding"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let bearers: Vec<String> = lock(&m).ensures.iter().map(|(b, _)| b.clone()).collect();
    assert_eq!(
        bearers,
        ["Bearer sa-1", "Bearer sa-2"],
        "retried with a fresh SA token"
    );

    let (_e2, app2, m2, _) = env("mcp-binding-refused-403").await;
    lock(&m2).refuse = Some(403);
    let (s, b) = ask(&app2, json!({"tenant_id": boot(), "kind": "binding"})).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "mcp_binding_refused");
    lock(&m2).refuse = Some(404);
    let (s, b) = ask(&app2, json!({"tenant_id": boot(), "kind": "binding"})).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "mcp_binding_product_unknown");
}

#[tokio::test]
async fn without_bindings_configured_the_binding_kind_is_a_503() {
    let e = crate::common::env("mcp-binding-off").await;
    let (s, b) = ask(&e.app, json!({"tenant_id": boot(), "kind": "binding"})).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "mcp_binding_unconfigured");
}
