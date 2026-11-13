//! Early-access signup (care-bus 17; ported from agent `tests/signups.rs`): the intake door, validation, de-dupe,
//! the Telegram notice (send-only, C1 buttons `ea1:carebus`), the journaled approve (login -> tenant -> seat ->
//! base release -> licences -> member licence -> ONE email, last), the tick, the Telegram decision door (C3,
//! idempotent), reject repair (licence set revoked, tool dependencies released), and licence on login. Real
//! Postgres 18; one in-process fake for Keycloak, licences and Telegram. RLS tables are read as the runtime role
//! inside the tenant context (lesson #1).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::common::{
    TEST_SERVICE_ISSUER, TestEnv, get as get_req, json, send, service, super_admin,
};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use carebus_api::signup::config::{
    ClientCredentials, KeycloakAdminConfig, LicencesIssueConfig, SignupConfig, SignupTelegram,
};
use serde_json::{Value, json};
use uuid::Uuid;

const INTAKE: &str = "/api/signup-approvals/intake";
const INVITE: &str = "/api/signup-approvals/invite";
const INTAKE_CLIENT: &str = "care-bus-signup-intake";
const DECIDER: &str = "chat-bridge-signup-decider-care-bus";
const PUBLIC: &str = "https://cust.test";

#[derive(Default)]
struct Mock {
    next_id: u32,
    /// id -> representation
    users: HashMap<String, Value>,
    /// subs whose credentials list is non-empty
    with_password: Vec<String>,
    /// (sub, query) of every execute-actions-email
    emails: Vec<(String, HashMap<String, String>, Value)>,
    registered: Vec<(String, Value)>,
    bundles: Vec<(String, String, Value)>,
    /// (tenant header, sub, body)
    member_licences: Vec<(String, String, Value)>,
    revoked: Vec<String>,
    telegram: Vec<Value>,
    fail_member_licence: bool,
    /// Every tenant holds its bundle licence `lic-bundle-<tenant>` (GET /licenses/active answers it).
    bundle_held: bool,
    fail_revoke: bool,
    fail_bundle: bool,
    /// tenant -> its licences owner when that is not care-bus but care-bus holds a grant (PUT answers 200 with it;
    /// the bundle ensure answers 403 `tenant_owner_only`).
    granted: HashMap<String, String>,
    /// Tenants another app owns without a grant (PUT answers 409 `tenant_owned_by_other_app`).
    foreign: Vec<String>,
    /// Tenants whose bundle licence `lic-bundle-<tenant>` is held (as `bundle_held`, per tenant).
    held: Vec<String>,
    /// Every bundle ensure asked for, refused or not.
    ensure_calls: Vec<String>,
    /// Tenants archived (`PATCH /tenants/{id}/archive`).
    archived: Vec<String>,
    /// The archive answers this status instead (503 transient, 403 refusal).
    archive_status: Option<u16>,
    /// `GET /licenses` answers 503.
    fail_list: bool,
    /// `GET /licenses` also lists a licence of another tenant.
    list_foreign: bool,
    /// Revoking the bundle licence does NOT release the dependency licences (they stay live until revoked).
    keep_deps: bool,
    /// A dependency licence revoke answers this status (409 `dependency_still_required`, 503, …).
    dep_revoke_status: Option<u16>,
    /// A member licence revoke answers 200 `{revoked: false, "License not found"}` and revokes nothing.
    own_revoke_noop: bool,
}

/// The tool dependency licences licences releases with the last care-bus licence of a tenant (licences #303).
const DEPS: [&str; 3] = ["dep-mcp", "dep-strata-rag", "dep-strata-ingest"];

type Shared = Arc<Mutex<Mock>>;

fn lock(m: &Shared) -> std::sync::MutexGuard<'_, Mock> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

async fn token() -> Json<Value> {
    Json(json!({"access_token": "fake-admin-token", "expires_in": 300}))
}

async fn find_users(
    State(m): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let g = lock(&m);
    let want = q
        .get("email")
        .or_else(|| q.get("username"))
        .cloned()
        .unwrap_or_default();
    let hits: Vec<Value> = g
        .users
        .values()
        .filter(|u| {
            u["email"]
                .as_str()
                .is_some_and(|e| e.eq_ignore_ascii_case(&want))
        })
        .cloned()
        .collect();
    Json(Value::Array(hits))
}

async fn create_user(State(m): State<Shared>, Json(mut rep): Json<Value>) -> Response {
    let mut g = lock(&m);
    let email = rep["email"].as_str().unwrap_or_default().to_string();
    if g.users.values().any(|u| u["email"] == email) {
        return StatusCode::CONFLICT.into_response();
    }
    g.next_id += 1;
    let id = format!("kc-{}", g.next_id);
    rep["id"] = json!(id);
    g.users.insert(id.clone(), rep);
    (
        StatusCode::CREATED,
        [(
            "location",
            format!("http://kc/admin/realms/care-bus/users/{id}"),
        )],
    )
        .into_response()
}

async fn read_user(State(m): State<Shared>, Path(id): Path<String>) -> Response {
    match lock(&m).users.get(&id) {
        Some(u) => Json(u.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn write_user(
    State(m): State<Shared>,
    Path(id): Path<String>,
    Json(rep): Json<Value>,
) -> StatusCode {
    let mut g = lock(&m);
    if !g.users.contains_key(&id) {
        return StatusCode::NOT_FOUND;
    }
    g.users.insert(id, rep);
    StatusCode::NO_CONTENT
}

async fn delete_user(State(m): State<Shared>, Path(id): Path<String>) -> StatusCode {
    if lock(&m).users.remove(&id).is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn credentials(State(m): State<Shared>, Path(id): Path<String>) -> Json<Value> {
    let has = lock(&m).with_password.contains(&id);
    Json(if has {
        json!([{"type": "password"}])
    } else {
        json!([])
    })
}

async fn links() -> Json<Value> {
    Json(json!([]))
}

async fn actions_email(
    State(m): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    Json(actions): Json<Value>,
) -> StatusCode {
    lock(&m).emails.push((id, q, actions));
    StatusCode::NO_CONTENT
}

async fn app() -> Json<Value> {
    Json(json!({"id": "app-care-bus", "key": "care-bus"}))
}

async fn plans() -> Json<Value> {
    Json(json!([{"id": "plan-standard", "code": "standard"}, {"id": "plan-x", "code": "x"}]))
}

async fn register(
    State(m): State<Shared>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let mut g = lock(&m);
    g.registered.push((id.clone(), b));
    if g.foreign.contains(&id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail": {"error": "tenant_owned_by_other_app",
                                   "message": "this tenant id is registered to a different owning app"}})),
        )
            .into_response();
    }
    if let Some(owner) = g.granted.get(&id) {
        // licences `_reregister_existing`: the row unchanged, its owner named
        return Json(json!({"id": id, "name": "stored", "owner_key": owner})).into_response();
    }
    StatusCode::CREATED.into_response()
}

async fn bundle(
    State(m): State<Shared>,
    Path((id, key)): Path<(String, String)>,
    Json(b): Json<Value>,
) -> Response {
    let mut g = lock(&m);
    g.ensure_calls.push(id.clone());
    if g.fail_bundle {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if g.granted.contains_key(&id) || g.foreign.contains(&id) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail": {"error": "tenant_owner_only"}})),
        )
            .into_response();
    }
    g.bundles.push((id, key, b));
    Json(json!({"tenant_id": "t", "bundle": "care-bus", "apps": {}, "refused": []})).into_response()
}

async fn member_licence(
    State(m): State<Shared>,
    Path(sub): Path<String>,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    let mut g = lock(&m);
    if g.fail_member_licence {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let tenant = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    g.member_licences.push((tenant, sub.clone(), b));
    (
        StatusCode::CREATED,
        Json(json!({"id": format!("lic-{sub}")})),
    )
        .into_response()
}

/// `POST /license/revoke`, licences' real answers: 200 `{revoked: true, released_dependency_ids}`; 200 `{revoked:
/// false, message: "License not found"}` for an id it does not revoke (unknown, already revoked); 409
/// `dependency_still_required` for a dependency licence another live licence still requires.
async fn revoke(State(m): State<Shared>, Json(b): Json<Value>) -> Response {
    let mut g = lock(&m);
    if g.fail_revoke {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let id = b["license_id"].as_str().unwrap_or_default().to_string();
    let not_found =
        || Json(json!({"revoked": false, "message": "License not found"})).into_response();
    if g.revoked.contains(&id) {
        return not_found();
    }
    if DEPS.iter().any(|d| id.starts_with(&format!("{d}-"))) {
        if let Some(st) = g.dep_revoke_status {
            let body = if st == 409 {
                json!({"detail": {"error": "dependency_still_required", "code": "dependency_still_required",
                                  "message": "a remaining active licence still requires this app"}})
            } else {
                json!({"detail": "unavailable"})
            };
            return (StatusCode::from_u16(st).unwrap(), Json(body)).into_response();
        }
        g.revoked.push(id);
        return Json(
            json!({"revoked": true, "message": "License revoked", "released_dependency_ids": []}),
        )
        .into_response();
    }
    if id.starts_with("lic-") && !id.starts_with("lic-bundle-") && g.own_revoke_noop {
        return not_found();
    }
    let released: Vec<&str> = if id.starts_with("lic-bundle-") && !g.keep_deps {
        DEPS.to_vec()
    } else {
        vec![]
    };
    g.revoked.push(id);
    Json(json!({"revoked": true, "message": "ok", "released_dependency_ids": released}))
        .into_response()
}

async fn active(State(m): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let tenant = q.get("tenant_id").cloned().unwrap_or_default();
    let subject_ok = q.get("subject_id") == Some(&format!("svc:{tenant}:care-bus"));
    let held = {
        let g = lock(&m);
        g.bundle_held || g.held.contains(&tenant)
    };
    if !held || !subject_ok || q.get("app_key").map(String::as_str) != Some("care-bus") {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": {"error": "no_active_license"}})),
        )
            .into_response();
    }
    Json(
        json!({"active": true, "tenant_id": tenant, "subject_id": format!("svc:{tenant}:care-bus"),
                "license_id": format!("lic-bundle-{tenant}"), "app": {"key": "care-bus"},
                "subject_match": true}),
    )
    .into_response()
}

/// `GET /licenses` (`X-Tenant-Id`): the tenant's member licences, its bundle licence once ensured (adopted by the
/// bundle: `bundle_id`) and the three dependency licences the bundle brought, live until the bundle licence goes.
async fn list_licences(State(m): State<Shared>, headers: HeaderMap) -> Response {
    let g = lock(&m);
    if g.fail_list {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let tenant = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let row = |id: String, dep: bool, bundle: Option<&str>, revoked: bool| {
        json!({"id": id, "tenant_id": tenant, "status": if revoked { "revoked" } else { "active" },
               "revoked_at": if revoked { json!("2026-10-25T00:00:00Z") } else { Value::Null },
               "is_dependency": dep, "bundle_id": bundle})
    };
    let mut out: Vec<Value> = g
        .member_licences
        .iter()
        .filter(|(t, _, _)| *t == tenant)
        .map(|(_, sub, _)| {
            let id = format!("lic-{sub}");
            let r = g.revoked.contains(&id);
            row(id, false, None, r)
        })
        .collect();
    if g.bundles.iter().any(|(t, _, _)| *t == tenant) {
        let bundle = format!("lic-bundle-{tenant}");
        let gone = g.revoked.contains(&bundle);
        out.push(row(bundle, false, Some("bundle-care-bus"), gone));
        for d in DEPS {
            let id = format!("{d}-{tenant}");
            let r = (gone && !g.keep_deps) || g.revoked.contains(&id);
            out.push(row(id, true, None, r));
        }
    }
    if g.list_foreign {
        out.push(
            json!({"id": "lic-foreign", "tenant_id": crate::common::OTHER_TENANT, "status": "active",
                        "revoked_at": null, "is_dependency": false, "bundle_id": null}),
        );
    }
    Json(Value::Array(out)).into_response()
}

async fn archive(
    State(m): State<Shared>,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    let mut g = lock(&m);
    if let Some(s) = g.archive_status {
        return (
            StatusCode::from_u16(s).unwrap(),
            Json(json!({"detail": {"error": "tenant_owner_only"}})),
        )
            .into_response();
    }
    assert_eq!(b, json!({"archived": true}));
    if !g.archived.contains(&id) {
        g.archived.push(id.clone());
    }
    Json(json!({"id": id, "archived_at": "2026-10-25T00:00:00Z"})).into_response()
}

async fn telegram(State(m): State<Shared>, Json(b): Json<Value>) -> Json<Value> {
    lock(&m).telegram.push(b);
    Json(json!({"ok": true}))
}

async fn fake() -> (String, Shared) {
    let m: Shared = Arc::new(Mutex::new(Mock::default()));
    let kc = "/admin/realms/care-bus";
    let router = Router::new()
        .route("/token", post(token))
        .route(&format!("{kc}/users"), get(find_users).post(create_user))
        .route(
            &format!("{kc}/users/{{id}}"),
            get(read_user).put(write_user).delete(delete_user),
        )
        .route(&format!("{kc}/users/{{id}}/credentials"), get(credentials))
        .route(&format!("{kc}/users/{{id}}/federated-identity"), get(links))
        .route(
            &format!("{kc}/users/{{id}}/execute-actions-email"),
            put(actions_email),
        )
        .route("/lic/apps/care-bus", get(app))
        .route("/lic/apps/care-bus/plans", get(plans))
        .route("/lic/tenants/{id}", put(register))
        .route("/lic/tenants/{id}/archive", patch(archive))
        .route("/lic/licenses", get(list_licences))
        .route(
            "/lic/internal/tenants/{id}/bundles/{key}/ensure",
            post(bundle),
        )
        .route("/lic/licenses/user/{sub}", post(member_licence))
        .route("/lic/license/revoke", post(revoke))
        .route("/lic/licenses/active", get(active))
        .route("/tg/{bot}/sendMessage", post(telegram))
        .with_state(m.clone());
    (crate::common::fakes::serve(router).await, m)
}

fn signup_cfg(base: &str, login_licence: bool) -> SignupConfig {
    let creds = |id: &str| ClientCredentials {
        token_url: format!("{base}/token"),
        client_id: id.into(),
        client_secret: "test-secret".into(),
    };
    SignupConfig {
        enabled: true,
        intake_issuer: TEST_SERVICE_ISSUER.into(),
        decider_issuer: TEST_SERVICE_ISSUER.into(),
        public_url: Some(PUBLIC.into()),
        keycloak_admin: Some(KeycloakAdminConfig {
            creds: creds("care-bus-admin"),
            api_url: format!("{base}/admin/realms/care-bus"),
        }),
        licences: Some(LicencesIssueConfig {
            creds: creds("care-bus-licenses-admin"),
            url: format!("{base}/lic"),
            owner_key: "care-bus".into(),
            app_key: "care-bus".into(),
            plan_code: "standard".into(),
            subject_realm: "care-bus".into(),
            bundle_key: "care-bus".into(),
            bundle_plans: vec!["standard".into()],
        }),
        telegram: Some(SignupTelegram {
            bot_token: "123:bot".into(),
            chat_id: "-1003934173351".into(),
            thread_id: 4242,
            api_base: format!("{base}/tg"),
        }),
        login_licence,
        ..Default::default()
    }
}

async fn env_on(test: &str) -> (TestEnv, Shared) {
    let (base, m) = fake().await;
    let e = crate::common::env_with(test, |c| c.signup = signup_cfg(&base, false)).await;
    (e, m)
}

fn intake_token() -> String {
    service(INTAKE_CLIENT, &[INTAKE_CLIENT], &[INTAKE_CLIENT])
}

fn decider_token() -> String {
    service(DECIDER, &[DECIDER], &["care-bus-signup-decide"])
}

fn body(email: &str) -> Value {
    json!({"email": email, "name": "Ann van der Berg", "company": "Globex", "source": "web",
           "message": "We run\n12 clinics", "locale": "de-DE"})
}

async fn submit(e: &TestEnv, email: &str) -> Uuid {
    let (s, b) = send(
        &e.app,
        json("POST", INTAKE, Some(&intake_token()), body(email)),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["status"], "queued");
    sqlx::query_scalar(
        "SELECT id FROM signup_approvals WHERE lower(email) = lower($1) AND status = 'pending'",
    )
    .bind(email)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn steps(e: &TestEnv, id: Uuid) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as("SELECT step, status, code FROM signup_provisioning_steps WHERE signup_id = $1 ORDER BY ord")
        .bind(id)
        .fetch_all(&e.owner)
        .await
        .unwrap()
}

async fn telegram_count(m: &Shared, want: usize) -> Vec<Value> {
    for _ in 0..50 {
        let t = lock(m).telegram.clone();
        if t.len() >= want {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    lock(m).telegram.clone()
}

fn approve_uri(id: Uuid) -> String {
    format!("/api/signup-approvals/{id}/approve")
}

fn decision_uri(id: Uuid) -> String {
    format!("/api/signup-approvals/{id}/telegram-decision")
}

/// The member licence row of `sub`, read as the runtime role inside `tenant` (lesson #1).
async fn member_licence_in(e: &TestEnv, tenant: Uuid, sub: &str) -> Option<String> {
    let mut tx = carebus_api::tenancy::begin_scoped(&e.state.db, tenant, None)
        .await
        .unwrap();
    let id: Option<String> =
        sqlx::query_scalar("SELECT license_id FROM member_licences WHERE sub = $1")
            .bind(sub)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    id
}

/// The founded workspace's job serves what the bootstrap job serves: the bootstrap's current release is current
/// for it, every effective view has the same rows (by natural key), and the procedure steps came along under new
/// procedure ids. Read as the runtime role inside each tenant + job (lesson #1), so RLS and the views both apply.
async fn assert_base_like_bootstrap(e: &TestEnv, tenant: Uuid) {
    async fn scoped(e: &TestEnv, tenant: Uuid) -> Vec<(String, i64)> {
        let job: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
            .bind(tenant)
            .bind(crate::common::BOOT_JOB)
            .fetch_one(&e.owner)
            .await
            .unwrap();
        let mut tx = carebus_api::tenancy::begin_scoped(&e.state.db, tenant, Some(job))
            .await
            .unwrap();
        let mut out = Vec::new();
        for v in [
            "job_releases WHERE is_current AND release_id = 'bootstrap-0009'",
            "eff_context_spaces",
            "eff_ontology_entities",
            "eff_ontology_attributes",
            "eff_ontology_relations",
            "eff_procedures",
            "eff_procedure_steps",
            "eff_connectors",
            "eff_connector_field_mappings",
            "eff_knowledge_stores",
        ] {
            let n: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {v}")))
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
            out.push((v.to_string(), n));
        }
        tx.commit().await.unwrap();
        out
    }
    let boot = scoped(e, crate::common::BOOT_TENANT.parse().unwrap()).await;
    let new = scoped(e, tenant).await;
    assert_eq!(new, boot);
    assert_eq!(
        new[0].1, 1,
        "the bootstrap release is current for the new job"
    );
    assert!(
        new[5].1 > 0 && new[6].1 > 0,
        "procedures and their steps: {new:?}"
    );
    let shared: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM procedures n JOIN procedures b ON b.id = n.id \
          WHERE n.tenant_id = $1 AND b.tenant_id = $2::uuid",
    )
    .bind(tenant)
    .bind(crate::common::BOOT_TENANT)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(shared, 0, "copied procedures get ids of their own");
}

#[tokio::test]
async fn without_a_base_release_the_step_is_skipped_and_the_signup_still_completes() {
    let (base, m) = fake().await;
    let e = crate::common::env_with("signup-no-base", |c| {
        c.signup = signup_cfg(&base, false);
        // a source tenant that does not exist has no base release
        c.signup.base_tenant = Uuid::new_v4();
    })
    .await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for (step, status, code) in steps(&e, id).await {
        if step == "base_release" {
            assert_eq!(
                (status.as_str(), code.as_deref()),
                ("skipped", Some("no_base_release"))
            );
        } else {
            assert_eq!(status, "ok", "{step}: {code:?}");
        }
    }
    assert_eq!(
        lock(&m).emails.len(),
        1,
        "a skipped step does not hold the email"
    );
    let tenant: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
    let releases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM job_releases WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(releases, 0);
}

#[tokio::test]
async fn off_by_default_every_signup_route_is_503_signup_not_configured() {
    let e = crate::common::env("signup-off").await;
    for (m, uri, token) in [
        ("POST", INTAKE.to_string(), intake_token()),
        ("GET", "/api/signup-approvals".to_string(), super_admin()),
        ("POST", approve_uri(Uuid::new_v4()), super_admin()),
        ("POST", decision_uri(Uuid::new_v4()), decider_token()),
    ] {
        let req = if m == "GET" {
            get_req(&uri, Some(&token))
        } else {
            json(
                m,
                &uri,
                Some(&token),
                json!({"action": "approve", "telegramUserId": 1}),
            )
        };
        let (s, b) = send(&e.app, req).await;
        if uri == INTAKE {
            // the intake body is strict, so this one is decided by the switch, not the shape
            assert!(
                matches!(s, StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_REQUEST),
                "{m} {uri} {b}"
            );
            continue;
        }
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{m} {uri} {b}");
        assert_eq!(b["error"]["code"], "signup_not_configured");
    }
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            INTAKE,
            Some(&intake_token()),
            body("ann@globex.com"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "signup_not_configured");
}

#[tokio::test]
async fn intake_takes_only_the_intake_service_account() {
    let (e, _m) = env_on("signup-intake-door").await;
    let b = body("ann@globex.com");
    assert_eq!(
        send(&e.app, json("POST", INTAKE, None, b.clone())).await.0,
        StatusCode::UNAUTHORIZED
    );
    for (why, token) in [
        ("operator session (portal realm)", super_admin()),
        (
            "intake client with super_admin",
            service(
                INTAKE_CLIENT,
                &[INTAKE_CLIENT],
                &[INTAKE_CLIENT, "super_admin"],
            ),
        ),
        (
            "another app's intake client",
            service("cp-signup-intake", &["cp-signup-intake"], &[INTAKE_CLIENT]),
        ),
        (
            "aud without the azp",
            service(INTAKE_CLIENT, &["care-bus"], &[INTAKE_CLIENT]),
        ),
        ("no role", service(INTAKE_CLIENT, &[INTAKE_CLIENT], &[])),
        ("the decider", decider_token()),
    ] {
        let (s, r) = send(&e.app, json("POST", INTAKE, Some(&token), b.clone())).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{why}: {r}");
        assert_eq!(r["error"]["code"], "signup_intake_forbidden", "{why}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM signup_approvals")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    // a secure-apps token never opens a person route
    let (s, _) = send(&e.app, get_req("/api/me", Some(&intake_token()))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn intake_validates_normalises_queues_once_and_notifies_telegram() {
    let (e, m) = env_on("signup-intake").await;
    let id = submit(&e, "Ann@Globex.com").await;
    let (locale, source, client): (Option<String>, String, Option<String>) =
        sqlx::query_as("SELECT locale, source, intake_client FROM signup_approvals WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(locale.as_deref(), Some("de"));
    assert_eq!(source, "web");
    assert_eq!(client.as_deref(), Some(INTAKE_CLIENT));
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            INTAKE,
            Some(&intake_token()),
            body("ann@globex.com"),
        ),
    )
    .await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::ACCEPTED, json!("already_pending"))
    );
    let mut extra = body("bob@globex.com");
    extra["unknown"] = json!(1);
    let (s, _) = send(&e.app, json("POST", INTAKE, Some(&intake_token()), extra)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let mut bad = body("bob@globex.com");
    bad["source"] = json!("fax");
    assert_eq!(
        send(&e.app, json("POST", INTAKE, Some(&intake_token()), bad))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let t = telegram_count(&m, 1).await;
    assert_eq!(t.len(), 1, "one notice for one queued signup");
    let msg = &t[0];
    assert_eq!(msg["message_thread_id"], 4242);
    assert!(msg.get("parse_mode").is_none());
    let kb = &msg["reply_markup"]["inline_keyboard"];
    assert_eq!(kb[0][0]["callback_data"], format!("ea1:carebus:a:{id}"));
    assert_eq!(kb[0][1]["callback_data"], format!("ea1:carebus:r:{id}"));
    assert_eq!(
        kb[1][0]["url"],
        format!("{PUBLIC}/admin/onboarding?signup={id}")
    );
    assert!(msg["text"].as_str().unwrap().contains("Ann van der Berg"));
}

#[tokio::test]
async fn approve_provisions_every_step_and_sends_one_email_last() {
    let (e, m) = env_on("signup-approve").await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["signup"]["status"], "approved");
    assert_eq!(b["tenant"]["name"], "Globex");
    let tenant: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
    for (step, status, code) in steps(&e, id).await {
        assert_eq!(status, "ok", "{step}: {code:?}");
    }
    // the login: names by fr's rule, stamped, locale + signup_app, ONE email to the app
    let sub = {
        let g = lock(&m);
        assert_eq!(g.users.len(), 1);
        let (sub, u) = g
            .users
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v.clone()))
            .unwrap();
        assert_eq!(u["firstName"], "Ann");
        assert_eq!(u["lastName"], "van der Berg");
        assert_eq!(u["attributes"]["carebus_signup_id"][0], id.to_string());
        assert_eq!(u["attributes"]["locale"][0], "de");
        assert_eq!(u["attributes"]["signup_app"][0], "care-bus");
        assert_eq!(g.emails.len(), 1);
        let (to, q, actions) = &g.emails[0];
        assert_eq!(to, &sub);
        assert_eq!(q["client_id"], "care-bus");
        assert_eq!(q["redirect_uri"], format!("{PUBLIC}/"));
        assert_eq!(actions, &json!(["UPDATE_PASSWORD", "VERIFY_EMAIL"]));
        // licences: tenant registered, its licence set ensured, the member licence in realm care-bus
        assert_eq!(g.registered.len(), 1);
        assert_eq!(g.registered[0].0, tenant.to_string());
        assert_eq!(g.bundles[0].1, "care-bus");
        let (lt, lsub, lbody) = &g.member_licences[0];
        assert_eq!(
            (lt.as_str(), lsub.as_str()),
            (tenant.to_string().as_str(), sub.as_str())
        );
        assert_eq!(lbody["subject_realm"], "care-bus");
        assert_eq!(lbody["plan_id"], "plan-standard");
        sub
    };
    // the workspace, its job and its tenant_admin, founded by this signup
    let (by, role): (String, String) = sqlx::query_as(
        "SELECT t.updated_by, u.tenant_role FROM tenants t JOIN users u ON u.tenant_id = t.id WHERE t.id = $1",
    )
    .bind(tenant)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(by, format!("signup:{id}"));
    assert_eq!(role, "tenant_admin");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(jobs, 1);
    // the job starts on the base release the bootstrap job serves: the same effective content, its own rows
    assert_base_like_bootstrap(&e, tenant).await;
    let again: String = sqlx::query_scalar(
        "SELECT adopt_outcome FROM carebus_adopt_base_release($1, 'customer_care_agent', $2::uuid, 'test')",
    )
    .bind(tenant)
    .bind(crate::common::BOOT_TENANT)
    .fetch_one(&e.state.db)
    .await
    .unwrap();
    assert_eq!(again, "already_current", "idempotent, never re-applied");
    // guarded inside the definer: only an approved signup's workspace, only from a base that is not one
    let other = crate::common::insert_tenant(&e.owner, crate::common::OTHER_TENANT).await;
    for (target, source, why) in [
        (
            other,
            crate::common::BOOT_TENANT.parse::<Uuid>().unwrap(),
            "target not founded by a signup",
        ),
        (tenant, tenant, "source is the target"),
        (
            other,
            tenant,
            "target not a signup workspace, source a signup workspace",
        ),
        (
            crate::common::BOOT_TENANT.parse().unwrap(),
            tenant,
            "source is a signup workspace",
        ),
    ] {
        let r = sqlx::query_scalar::<_, String>(
            "SELECT adopt_outcome FROM carebus_adopt_base_release($1, 'customer_care_agent', $2, 'test')",
        )
        .bind(target)
        .bind(source)
        .fetch_one(&e.state.db)
        .await;
        let err = r.expect_err(why);
        assert_eq!(
            err.as_database_error().and_then(|d| d.code()).as_deref(),
            Some("42501"),
            "{why}: {err}"
        );
    }
    let copied: i64 = sqlx::query_scalar("SELECT count(*) FROM job_releases WHERE tenant_id = $1")
        .bind(other)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(copied, 0, "a refused adopt writes nothing");
    // lesson #1: the runtime role sees the member licence inside the tenant, and nowhere else
    assert_eq!(
        member_licence_in(&e, tenant, &sub).await,
        Some(format!("lic-{sub}"))
    );
    let outside: i64 = sqlx::query_scalar("SELECT count(*) FROM member_licences")
        .fetch_one(&e.state.db)
        .await
        .unwrap();
    assert_eq!(
        outside, 0,
        "RLS hides member_licences without a tenant context"
    );
    // a second approve changes nothing
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("signup_not_pending"))
    );
    assert_eq!(lock(&m).emails.len(), 1);
}

#[tokio::test]
async fn removing_a_member_revokes_its_member_licence_first() {
    let (e, m) = env_on("signup-remove-member").await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let tenant: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
    let sub = lock(&m).users.keys().next().unwrap().clone();
    let licence = format!("lic-{sub}");
    assert_eq!(
        member_licence_in(&e, tenant, &sub).await.as_deref(),
        Some(licence.as_str())
    );
    let uri = format!("/api/tenants/{tenant}/users/{sub}");
    // licences down: 502, nothing removed, the licence still recorded
    lock(&m).fail_revoke = true;
    let (s, b) = send(
        &e.app,
        json("DELETE", &uri, Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(
        member_licence_in(&e, tenant, &sub).await.as_deref(),
        Some(licence.as_str())
    );
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE sub = $1")
        .bind(&sub)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(members, 1);
    // the retry revokes, then removes
    lock(&m).fail_revoke = false;
    let (s, b) = send(
        &e.app,
        json("DELETE", &uri, Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(lock(&m).revoked, vec![licence.clone()]);
    assert_eq!(member_licence_in(&e, tenant, &sub).await, None);
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE sub = $1")
        .bind(&sub)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(members, 0);
    // idempotent: gone is 404, and licences is not called again
    let (s, _) = send(
        &e.app,
        json("DELETE", &uri, Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(lock(&m).revoked.len(), 1);
}

#[tokio::test]
async fn the_email_waits_for_the_licence_and_the_tick_finishes_the_job() {
    let (e, m) = env_on("signup-tick").await;
    lock(&m).fail_member_licence = true;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "approved once login, workspace and seat landed: {b}"
    );
    let st: HashMap<String, String> = steps(&e, id)
        .await
        .into_iter()
        .map(|(s, st, _)| (s, st))
        .collect();
    assert_eq!(st["member_licence"], "pending");
    assert_eq!(st["invite_email"], "pending");
    assert!(lock(&m).emails.is_empty(), "no email before the licence");
    lock(&m).fail_member_licence = false;
    sqlx::query(
        "UPDATE signup_provisioning_steps SET next_attempt_at = now() WHERE signup_id = $1",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    let sg = e.state.signup.clone();
    carebus_api::signup::provisioning::tick(&e.state, &sg)
        .await
        .unwrap();
    for (step, status, code) in steps(&e, id).await {
        assert_eq!(status, "ok", "{step}: {code:?}");
    }
    assert_eq!(lock(&m).emails.len(), 1);
    // nothing left: a second tick sends nothing
    carebus_api::signup::provisioning::tick(&e.state, &sg)
        .await
        .unwrap();
    assert_eq!(lock(&m).emails.len(), 1);
}

#[tokio::test]
async fn a_login_this_signup_did_not_create_is_refused_before_anything_is_founded() {
    let (e, m) = env_on("signup-existing").await;
    lock(&m).users.insert(
        "kc-old".into(),
        json!({"id": "kc-old", "email": "ann@globex.com", "username": "ann@globex.com"}),
    );
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "identity_exists");
    assert!(b["error"]["steps"].is_array());
    let tenants: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenants WHERE updated_by LIKE 'signup:%'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(tenants, 0);
    let status: String = sqlx::query_scalar("SELECT status FROM signup_approvals WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    assert!(lock(&m).emails.is_empty());
    assert!(
        lock(&m).users["kc-old"].get("attributes").is_none(),
        "never written to"
    );
}

#[tokio::test]
async fn reject_after_an_aborted_approve_suspends_its_workspace_and_retires_its_login() {
    let (e, m) = env_on("signup-reject-repair").await;
    let id = submit(&e, "ann@globex.com").await;
    // the login the fake will mint (kc-1) already sits in the bootstrap tenant: the seat step refuses
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, updated_by) VALUES ('kc-1', $1::uuid, 'tenant_user', 'test')",
    )
    .bind(crate::common::BOOT_TENANT)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_belongs_to_another_workspace");
    // the aborted workspace holds a licence set: a member licence and the tenant's bundle licence
    let planned: Uuid = sqlx::query_scalar(
        "SELECT (data->>'tenant_id')::uuid FROM signup_provisioning_steps WHERE signup_id = $1 AND step = 'tenant'",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    sqlx::query("INSERT INTO member_licences (sub, tenant_id, license_id) VALUES ('kc-1', $1, 'lic-member-kc-1')")
        .bind(planned)
        .execute(&e.owner)
        .await
        .unwrap();
    {
        let mut g = lock(&m);
        g.bundle_held = true;
        g.fail_revoke = true;
    }
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{id}/reject"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "rejected");
    // licences down: nothing is suspended before the licence set is revoked; the tick retries it
    assert!(
        b["reject_repair"]["workspace"]
            .as_str()
            .unwrap()
            .starts_with("failed"),
        "{b}"
    );
    let tenant_status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(planned)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(tenant_status, "active");
    lock(&m).fail_revoke = false;
    sqlx::query("UPDATE signup_approvals SET repair_next_at = now() WHERE id = $1")
        .bind(id)
        .execute(&e.owner)
        .await
        .unwrap();
    let sg = e.state.signup.clone();
    carebus_api::signup::provisioning::tick(&e.state, &sg)
        .await
        .unwrap();
    let repair: Value =
        sqlx::query_scalar("SELECT reject_repair FROM signup_approvals WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(repair["workspace"], "suspended", "{repair}");
    assert_eq!(repair["identity"], "kept_in_use");
    let bundle = format!("lic-bundle-{planned}");
    assert_eq!(
        repair["licences"]["revoked"],
        json!(["lic-member-kc-1", &bundle])
    );
    let mut released: Vec<String> =
        serde_json::from_value(repair["licences"]["released_dependency_ids"].clone()).unwrap();
    released.sort();
    let mut want: Vec<String> = DEPS.iter().map(|d| d.to_string()).collect();
    want.sort();
    assert_eq!(released, want, "the tool dependency licences are released");
    {
        let g = lock(&m);
        assert!(g.revoked.contains(&"lic-member-kc-1".to_string()));
        assert!(g.revoked.contains(&bundle));
    }
    let status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE updated_by = $1")
        .bind(format!("signup:{id}:rejected"))
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "suspended");
    assert!(
        lock(&m).users.contains_key("kc-1"),
        "a login seated elsewhere is kept"
    );
    // a decided signup cannot be decided again
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "approve", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["status"], "already_decided");
    assert_eq!(b["outcome"], "rejected");
}

#[tokio::test]
async fn reject_of_a_never_approved_signup_creates_and_touches_nothing() {
    let (e, m) = env_on("signup-reject").await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "reject", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "rejected");
    assert!(lock(&m).users.is_empty());
    let by: String = sqlx::query_scalar("SELECT decided_by FROM signup_approvals WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(by, "telegram:7");
}

#[tokio::test]
async fn telegram_decision_door_is_c3_and_idempotent() {
    let (e, m) = env_on("signup-c3").await;
    let id = submit(&e, "ann@globex.com").await;
    let body = json!({"action": "approve", "telegramUserId": 42});
    // only the decider: the intake client, an operator session, a client-role decide role are refused
    for token in [
        intake_token(),
        super_admin(),
        service(DECIDER, &[DECIDER], &[]),
        service(
            "chat-bridge-signup-decider",
            &["chat-bridge-signup-decider"],
            &["care-bus-signup-decide"],
        ),
    ] {
        let (s, r) = send(
            &e.app,
            json("POST", &decision_uri(id), Some(&token), body.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{r}");
        assert_eq!(r["error"]["code"], "signup_decider_forbidden");
    }
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "approve"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "strict body");
    let (s, r) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["status"], "approved");
    assert!(r["tenantId"].is_string());
    assert!(r["summary"].as_str().unwrap().chars().count() <= 200);
    let (s, r2) = send(
        &e.app,
        json("POST", &decision_uri(id), Some(&decider_token()), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r2["status"], "already_decided");
    assert_eq!(r2["tenantId"], r["tenantId"]);
    assert_eq!(lock(&m).emails.len(), 1, "one email for two taps");
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(Uuid::new_v4()),
            Some(&decider_token()),
            json!({"action": "reject", "telegramUserId": 42}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn concurrent_approvals_create_one_tenant() {
    let (e, m) = env_on("signup-race").await;
    let id = submit(&e, "ann@globex.com").await;
    let reqs = (0..4).map(|_| {
        let app = e.app.clone();
        tokio::spawn(async move {
            send(
                &app,
                json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
            )
            .await
            .0
        })
    });
    let mut ok = 0;
    for r in reqs.collect::<Vec<_>>() {
        if r.await.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 1, "exactly one approver wins");
    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants WHERE updated_by = $1")
        .bind(format!("signup:{id}"))
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(tenants, 1);
    assert_eq!(lock(&m).emails.len(), 1);
}

#[tokio::test]
async fn invite_queue_get_and_retry() {
    let (e, _m) = env_on("signup-invite").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            INVITE,
            Some(&super_admin()),
            json!({"email": "zoe@initech.com", "name": "Zoe", "locale": "nl"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["signup"]["source"], "invite");
    assert_eq!(b["signup"]["status"], "approved");
    let id = b["signup"]["id"].as_str().unwrap().to_string();
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            INVITE,
            Some(&super_admin()),
            json!({"email": "x@y.co", "name": "X", "locale": "pt"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, l) = send(
        &e.app,
        get_req(
            "/api/signup-approvals?status=approved",
            Some(&super_admin()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{l}");
    assert_eq!(l["items"][0]["id"], id);
    assert_eq!(l["items"][0]["steps"].as_array().unwrap().len(), 7);
    let (s, one) = send(
        &e.app,
        get_req(&format!("/api/signup-approvals/{id}"), Some(&super_admin())),
    )
    .await;
    assert_eq!((s, one["locale"].clone()), (StatusCode::OK, json!("nl")));
    // only a super_admin reads the queue
    let (s, _) = send(
        &e.app,
        get_req("/api/signup-approvals", Some(&crate::common::admin())),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // retry of an approved signup with nothing failed is a no-op; of a pending one, 409
    let (s, r) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{id}/retry"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["requeued"], json!([]));
    let pending = submit(&e, "p@globex.com").await;
    let (s, r) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{pending}/retry"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        (s, r["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("signup_not_approved"))
    );
}

#[tokio::test]
async fn a_login_gets_its_member_licence_once() {
    let (base, m) = fake().await;
    let e = crate::common::env_with("signup-login-licence", |c| {
        c.signup = signup_cfg(&base, true);
        c.signup.enabled = false;
    })
    .await;
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let (s, _) = send(&e.app, get_req("/api/me", Some(&crate::common::admin()))).await;
    assert_eq!(s, StatusCode::OK);
    let mut found = None;
    for _ in 0..50 {
        found = member_licence_in(&e, boot, "u-admin").await;
        if found.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(found.as_deref(), Some("lic-u-admin"));
    {
        let g = lock(&m);
        assert_eq!(g.registered[0].0, crate::common::BOOT_TENANT);
        assert_eq!(g.member_licences.len(), 1);
        assert_eq!(g.member_licences[0].2["subject_realm"], "care-bus");
    }
    // recorded: the next requests make no licences call
    for _ in 0..3 {
        send(&e.app, get_req("/api/me", Some(&crate::common::admin()))).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lock(&m).member_licences.len(), 1);
}

#[tokio::test]
async fn every_active_tenant_gets_its_licence_set_once_retried_on_failure() {
    let (base, m) = fake().await;
    let e = crate::common::env_with("signup-bundle-reconcile", |c| {
        c.signup = signup_cfg(&base, false);
        c.signup.enabled = false; // runs on its own flag
        c.signup.bundle_reconcile = true;
    })
    .await;
    let other = crate::common::insert_tenant(&e.owner, crate::common::OTHER_TENANT).await;
    let suspended =
        crate::common::insert_tenant(&e.owner, "7f1c9a2e-0000-4000-8000-00000000fe13").await;
    sqlx::query("UPDATE tenants SET status = 'suspended' WHERE id = $1")
        .bind(suspended)
        .execute(&e.owner)
        .await
        .unwrap();
    // licences down: both active tenants are attempted, kept pending with a backoff
    lock(&m).fail_bundle = true;
    assert_eq!(reconcile(&e).await, 2);
    let rows: Vec<(Uuid, String, i32)> = sqlx::query_as(
        "SELECT tenant_id, status, attempts FROM tenant_bundle_reconcile ORDER BY tenant_id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "a suspended tenant is not reconciled: {rows:?}"
    );
    assert!(
        rows.iter().all(|(_, s, a)| s == "pending" && *a == 1),
        "{rows:?}"
    );
    assert_eq!(reconcile(&e).await, 0, "not due before its backoff");
    lock(&m).fail_bundle = false;
    sqlx::query("UPDATE tenant_bundle_reconcile SET next_attempt_at = now()")
        .execute(&e.owner)
        .await
        .unwrap();
    assert_eq!(reconcile(&e).await, 2);
    let ok: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_bundle_reconcile WHERE status = 'ok'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(ok, 2);
    // once: a later pass calls licences for nobody, and opens no tenant transaction (nothing is due)
    assert_eq!(reconcile(&e).await, 0);
    let due: i64 = sqlx::query_scalar("SELECT count(*) FROM carebus_bundle_reconcile_due(100)")
        .fetch_one(&e.state.db)
        .await
        .unwrap();
    assert_eq!(due, 0, "an ok tenant is never selected again");
    let g = lock(&m);
    let mut registered: Vec<String> = g.registered.iter().map(|(t, _)| t.clone()).collect();
    registered.sort();
    registered.dedup();
    let mut want = vec![crate::common::BOOT_TENANT.to_string(), other.to_string()];
    want.sort();
    assert_eq!(registered, want);
    let mut ensured: Vec<(String, String, Value)> = g.bundles.clone();
    ensured.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(ensured.len(), 2);
    for (t, key, body) in &ensured {
        assert!(want.contains(t));
        assert_eq!(key, "care-bus");
        assert_eq!(body, &json!({"plans": ["standard"]}));
    }
}

async fn reconcile_rows(e: &TestEnv) -> Vec<(Uuid, String, i32, Option<String>)> {
    sqlx::query_as(
        "SELECT tenant_id, status, attempts, message FROM tenant_bundle_reconcile ORDER BY tenant_id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

const GRANTED_HELD: &str = "7f1c9a2e-0000-4000-8000-00000000fe21";
const GRANTED_BARE: &str = "7f1c9a2e-0000-4000-8000-00000000fe22";
const FOREIGN: &str = "7f1c9a2e-0000-4000-8000-00000000fe23";

#[tokio::test]
async fn a_tenant_care_bus_does_not_own_is_never_ensured_and_settles_ok_or_not_owner() {
    let (base, m) = fake().await;
    let e = crate::common::env_with("signup-bundle-reconcile-owner", |c| {
        c.signup = signup_cfg(&base, false);
        c.signup.enabled = false;
        c.signup.bundle_reconcile = true;
    })
    .await;
    for t in [GRANTED_HELD, GRANTED_BARE, FOREIGN] {
        crate::common::insert_tenant(&e.owner, t).await;
    }
    {
        let mut g = lock(&m);
        // the bootstrap tenant: owned by care-bus here (201), so ensured as before
        g.granted.insert(GRANTED_HELD.into(), "platform".into());
        g.granted.insert(GRANTED_BARE.into(), "platform".into());
        g.foreign.push(FOREIGN.into());
        g.held.push(GRANTED_HELD.into());
    }
    assert_eq!(reconcile(&e).await, 4);
    let rows = reconcile_rows(&e).await;
    let by: HashMap<String, (String, i32, Option<String>)> = rows
        .into_iter()
        .map(|(t, s, a, msg)| (t.to_string(), (s, a, msg)))
        .collect();
    assert_eq!(by[crate::common::BOOT_TENANT].0, "ok", "owner: ensured");
    assert_eq!(by[GRANTED_HELD].0, "ok", "grantee, bundle active: {by:?}");
    assert!(
        by[GRANTED_HELD]
            .2
            .as_deref()
            .is_some_and(|m| m.contains("platform")),
        "ok on a grantee tenant records who ensured the bundle: {by:?}"
    );
    assert_eq!(by[GRANTED_BARE].0, "not_owner", "{by:?}");
    assert!(
        by[GRANTED_BARE].2.as_deref().unwrap().contains("platform"),
        "names the owner: {by:?}"
    );
    assert_eq!(by[FOREIGN].0, "not_owner", "409 is terminal: {by:?}");
    assert!(
        by[FOREIGN]
            .2
            .as_deref()
            .unwrap()
            .contains("tenant_owned_by_other_app")
    );
    assert!(by.values().all(|(_, a, _)| *a == 1), "{by:?}");
    {
        let g = lock(&m);
        assert_eq!(
            g.ensure_calls,
            vec![crate::common::BOOT_TENANT.to_string()],
            "care-bus ensures only the tenant it owns"
        );
    }
    // terminal: nothing is due, a later pass calls licences for nobody
    let due: i64 = sqlx::query_scalar("SELECT count(*) FROM carebus_bundle_reconcile_due(100)")
        .fetch_one(&e.state.db)
        .await
        .unwrap();
    assert_eq!(due, 0, "not_owner is never selected again");
    let calls = lock(&m).registered.len();
    assert_eq!(reconcile(&e).await, 0);
    assert_eq!(lock(&m).registered.len(), calls);
}

#[tokio::test]
async fn migration_0025_requeues_only_the_rows_that_failed_for_ownership() {
    let (base, _m) = fake().await;
    let e = crate::common::env_with("signup-bundle-reconcile-0025", |c| {
        c.signup = signup_cfg(&base, false);
    })
    .await;
    for t in [GRANTED_HELD, GRANTED_BARE] {
        crate::common::insert_tenant(&e.owner, t).await;
    }
    for (t, status, msg) in [
        (
            crate::common::BOOT_TENANT,
            "failed",
            "licences ensure bundle failed: HTTP 403 Forbidden: tenant_owner_only",
        ),
        (
            GRANTED_HELD,
            "failed",
            "licences register tenant failed: HTTP 409 Conflict: tenant_owned_by_other_app",
        ),
        (
            GRANTED_BARE,
            "failed",
            "licences ensure bundle failed: HTTP 400 Bad Request",
        ),
    ] {
        sqlx::query(
            "INSERT INTO tenant_bundle_reconcile (tenant_id, status, attempts, message) VALUES ($1, $2, 3, $3)",
        )
        .bind(t.parse::<Uuid>().unwrap())
        .bind(status)
        .bind(msg)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    // the migration's body again, as the owner role (it ran already, on an empty table, at startup)
    sqlx::raw_sql(include_str!(
        "../../migrations/0025_bundle_reconcile_not_owner.sql"
    ))
    .execute(&e.owner)
    .await
    .unwrap();
    let rows: HashMap<String, (String, i32)> = reconcile_rows(&e)
        .await
        .into_iter()
        .map(|(t, s, a, _)| (t.to_string(), (s, a)))
        .collect();
    assert_eq!(rows[crate::common::BOOT_TENANT], ("pending".into(), 0));
    assert_eq!(rows[GRANTED_HELD], ("pending".into(), 0));
    assert_eq!(
        rows[GRANTED_BARE],
        ("failed".into(), 3),
        "another failure stays"
    );
    let due: i64 = sqlx::query_scalar("SELECT count(*) FROM carebus_bundle_reconcile_due(100)")
        .fetch_one(&e.state.db)
        .await
        .unwrap();
    assert_eq!(due, 2);
    sqlx::query("UPDATE tenant_bundle_reconcile SET status = 'not_owner' WHERE tenant_id = $1")
        .bind(GRANTED_BARE.parse::<Uuid>().unwrap())
        .execute(&e.owner)
        .await
        .expect("not_owner is an allowed status");
}

async fn reconcile(e: &TestEnv) -> usize {
    let sg = e.state.signup.clone();
    carebus_api::signup::provisioning::reconcile_bundles(&e.state, &sg)
        .await
        .unwrap()
}

// ---- removal of an approved signup (crate::signup::removal) ----------------------------------------------------

/// Approve `email`; (signup id, tenant id, the minted login, the workspace slug from `GET …/{id}`).
async fn approved(e: &TestEnv, m: &Shared, email: &str) -> (Uuid, Uuid, String, String) {
    let id = submit(e, email).await;
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let tenant: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
    let sub = lock(m)
        .users
        .iter()
        .find(|(_, u)| u["email"] == email)
        .map(|(k, _)| k.clone())
        .unwrap();
    let (s, d) = send(
        &e.app,
        get_req(&format!("/api/signup-approvals/{id}"), Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["removal"], Value::Null);
    let slug = d["tenant_slug"].as_str().unwrap().to_string();
    (id, tenant, sub, slug)
}

fn remove_uri(id: Uuid) -> String {
    format!("/api/signup-approvals/{id}/remove")
}

fn removal_retry_uri(id: Uuid) -> String {
    format!("/api/signup-approvals/{id}/removal/retry")
}

async fn removal_steps(e: &TestEnv, id: Uuid) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as(
        "SELECT step, status, code FROM signup_removal_steps WHERE signup_id = $1 ORDER BY ord",
    )
    .bind(id)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

async fn signup_row(e: &TestEnv, id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, removal_state FROM signup_approvals WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn tenant_status(e: &TestEnv, tenant: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn members_of(e: &TestEnv, tenant: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM users WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// The workspace's audit actions, oldest first.
async fn audit_actions(e: &TestEnv, tenant: Uuid) -> Vec<(String, String)> {
    sqlx::query_as("SELECT action_type, status FROM audit_events WHERE tenant_id = $1 ORDER BY id")
        .bind(tenant)
        .fetch_all(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn removing_an_approved_signup_offboards_everything_and_frees_the_address() {
    let (e, m) = env_on("signup-removal").await;
    let (id, tenant, sub, slug) = approved(&e, &m, "ann@globex.com").await;
    assert!(
        slug.starts_with("globex-") && slug.ends_with(&tenant.simple().to_string()[24..]),
        "{slug}"
    );
    // a second member of the workspace goes too
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, updated_by) VALUES ('kc-colleague', $1, 'tenant_user', 't')",
    )
    .bind(tenant)
    .execute(&e.owner)
    .await
    .unwrap();

    // only a super_admin
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&crate::common::admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // the wrong slug (or none) changes nothing
    for body in [
        json!({"confirm": "globex"}),
        json!({"confirm": tenant.to_string()}),
    ] {
        let (s, b) = send(
            &e.app,
            json("POST", &remove_uri(id), Some(&super_admin()), body),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], "removal_confirm_mismatch");
    }
    let (s, _) = send(
        &e.app,
        json("POST", &remove_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert!(s.is_client_error(), "a missing confirm is refused: {s}");
    assert_eq!(signup_row(&e, id).await, ("approved".into(), None));
    assert_eq!(tenant_status(&e, tenant).await, "active");
    assert!(lock(&m).revoked.is_empty());

    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": format!(" {slug} ")}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "removed");
    assert_eq!(b["removal"]["state"], "done", "{b}");
    assert!(b["removal"]["done_at"].is_string());
    let steps: Vec<(String, String)> = b["removal"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["step"].as_str().unwrap().into(),
                r["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            "suspend",
            "members",
            "licences",
            "identity",
            "licences_tenant"
        ]
        .iter()
        .map(|s| (s.to_string(), "ok".to_string()))
        .collect::<Vec<_>>()
    );
    // tenant suspended, every member gone, every own licence revoked (deps released), tenant archived, login deleted
    assert_eq!(tenant_status(&e, tenant).await, "suspended");
    assert_eq!(members_of(&e, tenant).await, 0);
    let licences = &b["removal"]["steps"][2]["result"];
    let mut revoked: Vec<String> = serde_json::from_value(licences["revoked"].clone()).unwrap();
    revoked.sort();
    let mut want = vec![format!("lic-{sub}"), format!("lic-bundle-{tenant}")];
    want.sort();
    assert_eq!(revoked, want, "{licences}");
    assert_eq!(licences["kept_dependencies"], json!([]));
    assert_eq!(
        licences["released_dependency_ids"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    {
        let g = lock(&m);
        assert_eq!(g.archived, vec![tenant.to_string()]);
        assert!(!g.users.contains_key(&sub), "the minted login is deleted");
    }
    assert_eq!(b["removal"]["steps"][3]["result"]["login"], "deleted");
    assert_eq!(b["removal"]["steps"][1]["result"]["deleted"], 2);
    // audited in the workspace's trail: the action, each step, the domain writes, done
    let actions: Vec<String> = audit_actions(&e, tenant)
        .await
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    let from = actions.iter().position(|a| a == "signup.remove").unwrap();
    assert_eq!(
        actions[from..],
        [
            "signup.remove",
            "tenant.offboard",
            "signup.remove.suspend",
            "user.delete",
            "user.delete",
            "signup.remove.members",
            "signup.remove.licences",
            "signup.remove.identity",
            "signup.remove.licences_tenant",
            "signup.remove.done",
        ]
    );
    // the address is free: the same person can sign up and be approved again (no identity_exists)
    let (id2, tenant2, _, _) = approved(&e, &m, "ann@globex.com").await;
    assert_ne!((id2, tenant2), (id, tenant));
    // a removed signup is not removed, approved, rejected or retried again
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("removal_exists"))
    );
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("removal_done"))
    );
    let (s, b) = send(
        &e.app,
        json("POST", &approve_uri(id), Some(&super_admin()), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("signup_not_pending"))
    );
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{id}/retry"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("signup_not_approved"))
    );
    // the list knows the status
    let (s, b) = send(
        &e.app,
        get_req("/api/signup-approvals?status=removed", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 1);
    assert_eq!(b["items"][0]["removal_state"], "done");
    assert_eq!(b["counts"]["removed"], 1);
}

#[tokio::test]
async fn a_failed_removal_stays_visible_and_retry_finishes_only_what_is_left() {
    let (e, m) = env_on("signup-removal-retry").await;
    let (id, tenant, sub, slug) = approved(&e, &m, "ann@globex.com").await;
    // licences refuses the archive (403): the step and the removal are `failed`, everything before it landed
    lock(&m).archive_status = Some(403);
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "failed", "{b}");
    assert_eq!(
        removal_steps(&e, id).await.last().unwrap(),
        &(
            "licences_tenant".into(),
            "failed".into(),
            Some("licences_archive_failed".into())
        )
    );
    assert_eq!(
        signup_row(&e, id).await,
        ("removed".into(), Some("failed".into()))
    );
    assert!(!lock(&m).users.contains_key(&sub));
    // the tick never runs a failed removal by itself
    sqlx::query("UPDATE signup_approvals SET removal_next_at = now() WHERE id = $1")
        .bind(id)
        .execute(&e.owner)
        .await
        .unwrap();
    let sg = e.state.signup.clone();
    assert_eq!(
        carebus_api::signup::removal::tick(&e.state, &sg)
            .await
            .unwrap(),
        0
    );
    // Retry while licences still refuses: failed again, attempts start over
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "failed");
    assert_eq!(b["removal"]["steps"][4]["attempts"], 1);
    // a transient failure (503) is `pending` with backoff and the tick finishes it once it is due
    lock(&m).archive_status = Some(503);
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "pending");
    assert_eq!(b["removal"]["steps"][4]["status"], "pending");
    assert_eq!(b["removal"]["steps"][4]["code"], "licences_archive_failed");
    lock(&m).archive_status = None;
    assert_eq!(
        carebus_api::signup::removal::tick(&e.state, &sg)
            .await
            .unwrap(),
        0,
        "not due yet"
    );
    sqlx::query("UPDATE signup_approvals SET removal_next_at = now() WHERE id = $1")
        .bind(id)
        .execute(&e.owner)
        .await
        .unwrap();
    assert_eq!(
        carebus_api::signup::removal::tick(&e.state, &sg)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        signup_row(&e, id).await,
        ("removed".into(), Some("done".into()))
    );
    assert_eq!(lock(&m).archived, vec![tenant.to_string()]);
    // the steps that had landed ran once: one suspend, one member delete, each licence revoked once
    let actions = audit_actions(&e, tenant).await;
    let count = |a: &str| actions.iter().filter(|(x, _)| x == a).count();
    assert_eq!(count("tenant.offboard"), 1);
    assert_eq!(count("signup.remove.suspend"), 1);
    assert_eq!(count("signup.remove.licences"), 1);
    assert_eq!(count("signup.remove.retry"), 2);
    assert_eq!(
        count("signup.remove.licences_tenant"),
        4,
        "3 failed attempts + the one that landed"
    );
    assert!(
        actions
            .iter()
            .any(|(a, s)| a == "signup.remove.licences_tenant" && s == "failure")
    );
    let g = lock(&m);
    assert_eq!(
        g.revoked
            .iter()
            .filter(|r| **r == format!("lic-{sub}"))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_licences_outage_mid_removal_is_retried_and_a_foreign_listing_blocks() {
    let (e, m) = env_on("signup-removal-licences").await;
    let (id, tenant, _, slug) = approved(&e, &m, "ann@globex.com").await;
    lock(&m).fail_revoke = true;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "pending");
    assert_eq!(
        removal_steps(&e, id).await,
        vec![
            ("suspend".into(), "ok".into(), None),
            ("members".into(), "ok".into(), None),
            (
                "licences".into(),
                "pending".into(),
                Some("licence_revoke_failed".into())
            ),
            ("identity".into(), "pending".into(), None),
            ("licences_tenant".into(), "pending".into(), None),
        ]
    );
    assert_eq!(tenant_status(&e, tenant).await, "suspended");
    // a listing that names another tenant's licence blocks with zero revokes
    {
        let mut g = lock(&m);
        g.fail_revoke = false;
        g.list_foreign = true;
    }
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "blocked");
    assert_eq!(b["removal"]["steps"][2]["code"], "tenant_mismatch");
    assert!(lock(&m).revoked.is_empty());
    lock(&m).list_foreign = false;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "done", "{b}");
}

#[tokio::test]
async fn only_an_approved_signup_can_be_removed() {
    let (e, m) = env_on("signup-removal-refused").await;
    let pending = submit(&e, "pending@globex.com").await;
    let rejected = submit(&e, "rejected@globex.com").await;
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{rejected}/reject"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    for id in [pending, rejected] {
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                &remove_uri(id),
                Some(&super_admin()),
                json!({"confirm": "x"}),
            ),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::CONFLICT, json!("signup_not_approved")),
            "{b}"
        );
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                &removal_retry_uri(id),
                Some(&super_admin()),
                json!({}),
            ),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::CONFLICT, json!("signup_not_removed")),
            "{b}"
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM signup_removal_steps WHERE signup_id = $1")
                .bind(id)
                .fetch_one(&e.owner)
                .await
                .unwrap();
        assert_eq!(n, 0);
    }
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(Uuid::new_v4()),
            Some(&super_admin()),
            json!({"confirm": "x"}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("signup_not_found"))
    );
    // a workspace a super_admin re-pointed is not the one the approve founded: refused, nothing touched
    let (id, tenant, _, slug) = approved(&e, &m, "ann@globex.com").await;
    let other = crate::common::insert_tenant(&e.owner, crate::common::OTHER_TENANT).await;
    sqlx::query("UPDATE signup_approvals SET approved_tenant_id = $2 WHERE id = $1")
        .bind(id)
        .bind(other)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("tenant_mismatch")),
        "{b}"
    );
    assert_eq!(tenant_status(&e, tenant).await, "active");
    assert_eq!(tenant_status(&e, other).await, "active");
}

#[tokio::test]
async fn a_login_without_this_signups_stamp_is_never_deleted_and_blocks_the_removal() {
    let (e, m) = env_on("signup-removal-stamp").await;
    let (id, tenant, sub, slug) = approved(&e, &m, "ann@globex.com").await;
    // somebody re-stamped the login (another flow owns it now)
    lock(&m).users.get_mut(&sub).unwrap()["attributes"]["carebus_signup_id"] =
        json!(["someone-else"]);
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "blocked", "{b}");
    assert_eq!(
        removal_steps(&e, id).await[3..],
        [
            (
                "identity".to_string(),
                "blocked".to_string(),
                Some("identity_not_owned".to_string())
            ),
            ("licences_tenant".to_string(), "pending".to_string(), None),
        ]
    );
    assert!(
        lock(&m).users.contains_key(&sub),
        "an unstamped login is never deleted"
    );
    assert!(lock(&m).archived.is_empty(), "nothing after the block ran");
    // Retry while it is still not ours: blocked again, still not deleted
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "blocked");
    assert!(lock(&m).users.contains_key(&sub));
    // the operator restores the stamp (or deletes the login by hand): Retry finishes
    lock(&m).users.get_mut(&sub).unwrap()["attributes"]["carebus_signup_id"] =
        json!([id.to_string()]);
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "done", "{b}");
    assert!(!lock(&m).users.contains_key(&sub));
    assert_eq!(lock(&m).archived, vec![tenant.to_string()]);
}

#[tokio::test]
async fn licences_count_only_what_a_fresh_listing_shows_revoked_and_retry_dependency_outages() {
    let (e, m) = env_on("signup-removal-deps").await;
    // (a) licences answers 200 {revoked:false} for the member licence: not counted, the step is retried
    let (id, tenant, sub, slug) = approved(&e, &m, "ann@globex.com").await;
    lock(&m).own_revoke_noop = true;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "pending", "{b}");
    assert_eq!(b["removal"]["steps"][2]["status"], "pending");
    assert_eq!(b["removal"]["steps"][2]["code"], "licence_still_live");
    assert_eq!(
        b["removal"]["steps"][2]["result"],
        json!({}),
        "nothing is claimed revoked"
    );
    lock(&m).own_revoke_noop = false;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "done", "{b}");
    // the bundle licence went on the first attempt (already revoked now); the member licence only on the Retry
    let r = &b["removal"]["steps"][2]["result"];
    assert_eq!(r["revoked"], json!([format!("lic-{sub}")]), "{r}");
    assert!(
        r["already_revoked"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("lic-bundle-{tenant}"))),
        "{r}"
    );

    // (b) the dependency licences stay live after the bundle goes; their revoke answers 503: retried, not kept
    let (id, tenant, _, slug) = approved(&e, &m, "bob@initech.com").await;
    {
        let mut g = lock(&m);
        g.keep_deps = true;
        g.dep_revoke_status = Some(503);
    }
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "pending", "{b}");
    assert_eq!(b["removal"]["steps"][2]["code"], "dependency_revoke_failed");
    assert_eq!(
        b["removal"]["steps"][3]["status"], "pending",
        "nothing after it ran"
    );
    // (c) licences refuses them (409 dependency_still_required): recorded as kept, NOT as revoked; the archive
    // still runs (it stops them authorizing)
    lock(&m).dep_revoke_status = Some(409);
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &removal_retry_uri(id),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "done", "{b}");
    let r = &b["removal"]["steps"][2]["result"];
    let kept: Vec<&str> = r["kept_dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["license_id"].as_str().unwrap())
        .collect();
    let deps: Vec<String> = DEPS.iter().map(|d| format!("{d}-{tenant}")).collect();
    assert_eq!(
        kept,
        deps.iter().map(String::as_str).collect::<Vec<_>>(),
        "{r}"
    );
    assert!(r["kept_dependencies"][0]["status"] == 409, "{r}");
    assert!(
        !r["revoked"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().starts_with("dep-")),
        "a refused dependency is never reported revoked: {r}"
    );
    assert!(lock(&m).archived.contains(&tenant.to_string()));

    // (d) the dependency revokes succeed: counted, because the listing no longer shows them live
    let (id, tenant, _, slug) = approved(&e, &m, "cy@hooli.com").await;
    lock(&m).dep_revoke_status = None;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &remove_uri(id),
            Some(&super_admin()),
            json!({"confirm": slug}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["removal"]["state"], "done", "{b}");
    let r = &b["removal"]["steps"][2]["result"];
    for d in DEPS {
        let dep = format!("{d}-{tenant}");
        assert!(
            r["revoked"].as_array().unwrap().contains(&json!(dep)),
            "{r}"
        );
    }
    assert_eq!(r["kept_dependencies"], json!([]));
}
