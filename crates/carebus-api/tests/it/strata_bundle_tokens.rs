//! care-bus 17 C1: with `CAREBUS_STRATA_BUNDLE_TOKENS` on, every strata call carries a short-lived licence token
//! minted for the request's tenant (`POST /internal/tenants/{tenant}/bundle-token?app=…` as the bundle minter SA),
//! cached per (tenant, app); the static tokens are never sent. A tenant without the licence is 409
//! `strata_not_licensed_for_tenant` and strata is never called; a minter licences refuses is 503
//! `strata_licence_refused`. One in-process fake plays Keycloak (token), licences and both strata services.
//!
//! The same minter backs `POST /api/internal/mcp-credentials` (service token only): a tenant's MCP credentials are its
//! tenant-level care-bus licence (`app=care-bus&bundle=care-bus`) and its strata-rag token, per tenant, cached.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use carebus_api::signup::config::ClientCredentials;
use carebus_api::strata_tokens::BundleTokenConfig;
use serde_json::{Value, json};
use uuid::Uuid;

const STATIC_TOKEN: &str = "static-strata-token";
const SA_TOKEN: &str = "minter-sa-token";

#[derive(Default)]
struct Mock {
    /// client_id of every SA token request
    sa_clients: Vec<String>,
    /// (tenant, app, ttl_minutes) of every mint
    mints: Vec<(String, String, String)>,
    /// (tenant, app, bundle) of every mint
    bundles: Vec<(String, String, Option<String>)>,
    /// (service, bearer, x-tenant-id) of every strata call
    calls: Vec<(String, String, String)>,
    /// tenant -> licences status to refuse its mint with
    refuse: HashMap<String, u16>,
}

type Shared = Arc<Mutex<Mock>>;

fn lock(m: &Shared) -> std::sync::MutexGuard<'_, Mock> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

async fn sa_token(State(m): State<Shared>, Form(f): Form<HashMap<String, String>>) -> Json<Value> {
    lock(&m)
        .sa_clients
        .push(f.get("client_id").cloned().unwrap_or_default());
    Json(json!({"access_token": SA_TOKEN, "expires_in": 300}))
}

async fn mint(
    State(m): State<Shared>,
    Path(tenant): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    h: HeaderMap,
) -> Response {
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if bearer != format!("Bearer {SA_TOKEN}") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail": "Role required: bundle-token-minter"})),
        )
            .into_response();
    }
    let app = q.get("app").cloned().unwrap_or_default();
    let mut g = lock(&m);
    match g.refuse.get(&tenant).copied() {
        Some(404) => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    json!({"detail": {"error": "no_app_licence", "tenant_id": tenant, "app": app,
                                       "reason": "no_active_per_app_licence_in_bundle"}}),
                ),
            )
                .into_response();
        }
        Some(403) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"detail": {"error": "unknown_api_client"}})),
            )
                .into_response();
        }
        _ => {}
    }
    g.mints.push((
        tenant.clone(),
        app.clone(),
        q.get("ttl_minutes").cloned().unwrap_or_default(),
    ));
    g.bundles
        .push((tenant.clone(), app.clone(), q.get("bundle").cloned()));
    let n = g.mints.len();
    let expires = chrono::Utc::now() + chrono::Duration::minutes(15);
    (
        StatusCode::CREATED,
        Json(json!({
            "token": format!("tok:{tenant}:{app}:{n}"),
            "expires_at": expires.to_rfc3339(),
            "tenant_id": tenant, "bundle": "care-bus", "app": app,
            "license_id": format!("lic-{app}"), "token_id": format!("t-{n}"),
        })),
    )
        .into_response()
}

/// The licence token a strata service accepts: minted for the `X-Tenant-Id` tenant and for `app`.
fn strata_call(m: &Shared, service: &str, app: &str, h: &HeaderMap) -> bool {
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    let tenant = h
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let ok = bearer.starts_with(&format!("tok:{tenant}:{app}:"));
    lock(m).calls.push((service.into(), bearer, tenant));
    ok
}

async fn rag_stores(State(m): State<Shared>, h: HeaderMap) -> Response {
    if !strata_call(&m, "rag", "strata-rag", &h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({"stores": [{"id": "s1", "name": "care", "kind": "router", "children": []}]}))
        .into_response()
}

async fn ingest_jobs(State(m): State<Shared>, h: HeaderMap) -> Response {
    if !strata_call(&m, "ingest", "strata-ingest", &h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({"jobs": [], "next": null})).into_response()
}

async fn fake() -> (String, Shared) {
    let m: Shared = Arc::new(Mutex::new(Mock::default()));
    let router = Router::new()
        .route("/token", post(sa_token))
        .route("/lic/internal/tenants/{tenant}/bundle-token", post(mint))
        .route("/rag/api/stores", get(rag_stores))
        .route("/ingest/api/ingest/jobs", get(ingest_jobs))
        .with_state(m.clone());
    (crate::common::fakes::serve(router).await, m)
}

async fn env_minting(test: &str) -> (crate::common::TestEnv, Shared) {
    let (base, m) = fake().await;
    let e = crate::common::env_with(test, |c| {
        c.strata_rag_url = Some(format!("{base}/rag"));
        c.strata_ingest_url = Some(format!("{base}/ingest"));
        // set, and never sent: minting replaces the static tokens for every tenant
        c.strata_rag_token = Some(STATIC_TOKEN.into());
        c.strata_ingest_token = Some(STATIC_TOKEN.into());
        c.strata_tenant = None;
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
    (e, m)
}

/// `u-other`: tenant_user of a second tenant.
async fn other_tenant(e: &crate::common::TestEnv) -> Uuid {
    let t = crate::common::insert_tenant(&e.owner, crate::common::OTHER_TENANT).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, updated_by) VALUES ('u-other', $1, 'tenant_user', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    t
}

#[tokio::test]
async fn each_tenant_gets_its_own_minted_token_cached_per_app() {
    let (e, m) = env_minting("strata-bundle-tokens").await;
    let other = other_tenant(&e).await;
    let boot = crate::common::BOOT_TENANT;
    for _ in 0..2 {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::get("/api/knowledge/stores", Some(&crate::common::user())),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["items"][0]["name"], "care");
    }
    let other_user = crate::common::token_for("u-other", &[]);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores", Some(&other_user)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/ingest/jobs", Some(&crate::common::user())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let g = lock(&m);
    // one mint per (tenant, app): the second boot call reused the cached token
    let mut mints: Vec<(String, String)> = g
        .mints
        .iter()
        .map(|(t, a, ttl)| {
            assert_eq!(ttl, "15");
            (t.clone(), a.clone())
        })
        .collect();
    mints.sort();
    let mut want = vec![
        (boot.to_string(), "strata-rag".to_string()),
        (boot.to_string(), "strata-ingest".to_string()),
        (other.to_string(), "strata-rag".to_string()),
    ];
    want.sort();
    assert_eq!(mints, want);
    assert!(g.sa_clients.iter().all(|c| c == "care-bus-bundle-minter"));
    assert_eq!(g.calls.len(), 4);
    for (service, bearer, tenant) in &g.calls {
        assert_ne!(
            bearer, STATIC_TOKEN,
            "{service}: the static token is never sent"
        );
        assert!(
            bearer.starts_with(&format!("tok:{tenant}:")),
            "{service}: {bearer}"
        );
    }
    let boot_rag: Vec<&String> = g
        .calls
        .iter()
        .filter(|(s, _, t)| s == "rag" && t == boot)
        .map(|(_, b, _)| b)
        .collect();
    assert_eq!(boot_rag.len(), 2);
    assert_eq!(boot_rag[0], boot_rag[1], "the cached token is reused");
}

#[tokio::test]
async fn a_tenant_without_its_licence_is_409_and_strata_is_never_called() {
    let (e, m) = env_minting("strata-bundle-unlicensed").await;
    lock(&m)
        .refuse
        .insert(crate::common::BOOT_TENANT.into(), 404);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores", Some(&crate::common::user())),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "strata_not_licensed_for_tenant");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no_app_licence")
    );
    // a write is refused before it is sent (or audited)
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/knowledge/stores",
            Some(&crate::common::admin()),
            json!({"name": "x", "kind": "leaf"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "strata_not_licensed_for_tenant");
    // the minter itself refused (licences 403): an operator problem, 503
    lock(&m)
        .refuse
        .insert(crate::common::BOOT_TENANT.into(), 403);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/ingest/jobs", Some(&crate::common::user())),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "strata_licence_refused");
    let g = lock(&m);
    assert!(g.calls.is_empty(), "nothing reached strata: {:?}", g.calls);
    assert!(g.mints.is_empty());
}

const SVC: &str = "svc-test-token";
const MCP_CREDENTIALS: &str = "/api/internal/mcp-credentials";

async fn mcp_credentials(
    e: &crate::common::TestEnv,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value, HeaderMap) {
    use tower::ServiceExt;
    let req = crate::common::json("POST", MCP_CREDENTIALS, token, body);
    let resp = e.app.clone().oneshot(req).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v, headers)
}

/// A second active care-bus tenant: a tenant row with one job, so it has a scope.
async fn other_scoped_tenant(e: &crate::common::TestEnv) -> Uuid {
    let t = other_tenant(e).await;
    crate::common::insert_job(&e.owner, t, "support").await;
    t
}

/// The fix for the care-bus#36 audit: MCP is called as the bus tenant. Each tenant gets ITS care-bus licence (minted
/// on the care-bus bundle, i.e. the tenant-level `svc:` licence, never a person's) and ITS strata-rag token; a second
/// ask is served from the cache; `fresh` mints both anew. Nothing is cached in the client's favour across tenants.
#[tokio::test]
async fn mcp_credentials_are_the_tenants_own_minted_licences() {
    let (e, m) = env_minting("mcp-credentials").await;
    let other = other_scoped_tenant(&e).await;
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();

    let (s, a1, h) = mcp_credentials(&e, Some(SVC), json!({"tenant_id": boot})).await;
    assert_eq!(s, StatusCode::OK, "{a1}");
    assert_eq!(h.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert_eq!(a1["tenant_id"], json!(boot));
    let lic = a1["licence_token"].as_str().unwrap();
    let rag = a1["strata_token"].as_str().unwrap();
    assert!(lic.starts_with(&format!("tok:{boot}:care-bus:")), "{lic}");
    assert!(rag.starts_with(&format!("tok:{boot}:strata-rag:")), "{rag}");
    let left = a1["expires_in"].as_u64().unwrap();
    assert!(left > 0 && left <= 15 * 60, "{left}");
    assert!(a1["expires_at"].as_str().is_some());

    // cached: the same pair, no new mint
    let (_, a2, _) = mcp_credentials(&e, Some(SVC), json!({"tenant_id": boot})).await;
    assert_eq!(
        (&a2["licence_token"], &a2["strata_token"]),
        (&a1["licence_token"], &a1["strata_token"])
    );
    assert_eq!(lock(&m).mints.len(), 2);

    // tenant B: its own pair, never A's
    let (s, b, _) = mcp_credentials(&e, Some(SVC), json!({"tenant_id": other})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant_id"], json!(other));
    for k in ["licence_token", "strata_token"] {
        let t = b[k].as_str().unwrap();
        assert!(t.starts_with(&format!("tok:{other}:")), "{k}: {t}");
        assert_ne!(b[k], a1[k]);
    }

    // refused by the MCP server: fresh mints both anew
    let (s, a3, _) =
        mcp_credentials(&e, Some(SVC), json!({"tenant_id": boot, "fresh": true})).await;
    assert_eq!(s, StatusCode::OK, "{a3}");
    assert_ne!(a3["licence_token"], a1["licence_token"]);
    assert_ne!(a3["strata_token"], a1["strata_token"]);

    let g = lock(&m);
    let mut bundles = g.bundles.clone();
    bundles.sort();
    let care = Some("care-bus".to_string());
    let mut want = vec![
        (boot.to_string(), "care-bus".to_string(), care.clone()),
        (boot.to_string(), "care-bus".to_string(), care.clone()),
        (boot.to_string(), "strata-rag".to_string(), None),
        (boot.to_string(), "strata-rag".to_string(), None),
        (other.to_string(), "care-bus".to_string(), care.clone()),
        (other.to_string(), "strata-rag".to_string(), None),
    ];
    want.sort();
    assert_eq!(
        bundles, want,
        "the care-bus licence is minted on the care-bus bundle"
    );
    assert!(g.calls.is_empty(), "no strata call was needed");
}

/// Only the service token the agents hold reaches the route: a person (any role) is 403, no token 401, and nothing is
/// minted for them.
#[tokio::test]
async fn only_the_service_token_reaches_mcp_credentials() {
    let (e, m) = env_minting("mcp-credentials-auth").await;
    let body = json!({"tenant_id": crate::common::BOOT_TENANT});
    let (s, _, _) = mcp_credentials(&e, None, body.clone()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = mcp_credentials(&e, Some("not-a-service-token"), body.clone()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // a secure-apps service account, even the bundle minter itself, is not the agents' service token
    let sa = crate::common::service(
        "care-bus-bundle-minter",
        &["care-bus"],
        &["bundle-token-minter"],
    );
    let (s, _, _) = mcp_credentials(&e, Some(&sa), body.clone()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    for person in [
        crate::common::user(),
        crate::common::admin(),
        crate::common::super_admin(),
    ] {
        let (s, b, _) = mcp_credentials(&e, Some(&person), body.clone()).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert!(b.get("licence_token").is_none());
    }
    assert!(
        lock(&m).mints.is_empty(),
        "nothing minted for a non-service caller"
    );
}

/// No credentials unless the tenant is an active care-bus tenant that holds the licence, and never without a minter.
#[tokio::test]
async fn mcp_credentials_fail_closed() {
    let (e, m) = env_minting("mcp-credentials-closed").await;
    // a tenant care-bus does not serve
    let (s, b, _) = mcp_credentials(&e, Some(SVC), json!({"tenant_id": Uuid::new_v4()})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, _, _) = mcp_credentials(&e, Some(SVC), json!({"tenant_id": Uuid::nil()})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, _) = mcp_credentials(
        &e,
        Some(SVC),
        json!({"tenant_id": crate::common::BOOT_TENANT, "extra": 1}),
    )
    .await;
    assert!(s.is_client_error(), "{s}");
    // a suspended tenant has no scope
    let other = other_scoped_tenant(&e).await;
    sqlx::query("UPDATE tenants SET status = 'suspended' WHERE id = $1")
        .bind(other)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, _, _) = mcp_credentials(&e, Some(SVC), json!({"tenant_id": other})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(lock(&m).mints.is_empty());
    // the tenant holds no licence: 409, nothing returned
    lock(&m)
        .refuse
        .insert(crate::common::BOOT_TENANT.into(), 404);
    let (s, b, _) = mcp_credentials(
        &e,
        Some(SVC),
        json!({"tenant_id": crate::common::BOOT_TENANT}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "not_licensed_for_tenant");
    assert!(b.get("licence_token").is_none());

    // no minter configured: 503, never a static or shared credential
    let e = crate::common::env("mcp-credentials-nominter").await;
    let (s, b, _) = mcp_credentials(
        &e,
        Some(SVC),
        json!({"tenant_id": crate::common::BOOT_TENANT}),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "mcp_credentials_unconfigured");
}
