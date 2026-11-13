#![allow(dead_code)]
pub mod fakes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use carebus_api::{config::Config, state::AppState};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::sync::OnceCell;
use tower::ServiceExt;

pub struct TestEnv {
    pub state: AppState,
    pub app: Router,
    /// Superuser pool on the test database (RLS does not apply). Its connections carry the bootstrap tenant and job
    /// as session GUCs, so fixture inserts that omit `tenant_id` / `job_id` land in the bootstrap scope.
    pub owner: PgPool,
}

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Swap the database name (path segment) in a postgres URL, keeping any query string.
fn with_db_name(url: &str, name: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (url, None),
    };
    let scheme_end = head.find("://").map(|i| i + 3).unwrap_or(0);
    let base = match head[scheme_end..].find('/') {
        Some(i) => &head[..scheme_end + i],
        None => head,
    };
    match query {
        Some(q) => format!("{base}/{name}?{q}"),
        None => format!("{base}/{name}"),
    }
}

/// One server per test binary: `TEST_DATABASE_URL` when set, otherwise a single throwaway Postgres 18 from the
/// registry named by `TEST_PG_REGISTRY` (default `docker.io/library`). Starting a container per
/// test made a full run depend on N concurrent `docker run`s under load; this starts exactly one per binary and every
/// test gets its own fresh database inside it. The container lives in a static, so it is removed by an `atexit` hook
/// when the test process exits.
struct Server {
    base: String,
    _container: Option<ContainerAsync<Postgres>>,
}

static SERVER: OnceCell<Server> = OnceCell::const_new();
static CONTAINER_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Statics are never dropped and the harness runs no reaper, so remove the shared container when the process exits.
extern "C" fn remove_container() {
    if let Some(id) = CONTAINER_ID.get() {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", "-v", id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

async fn start_container() -> (String, ContainerAsync<Postgres>) {
    let reg = match std::env::var("TEST_PG_REGISTRY") {
        Ok(r) if !r.is_empty() => r,
        _ => "docker.io/library".to_string(),
    };
    let image = format!("{}/postgres", reg.trim_end_matches('/'));
    let mut last = String::new();
    for attempt in 1..=3 {
        match Postgres::default()
            .with_name(&image)
            .with_tag("18-alpine")
            .with_cmd(["postgres", "-c", "max_connections=500", "-c", "fsync=off"])
            .start()
            .await
        {
            Ok(pg) => {
                // Docker can report the container started before the host port mapping is published
                // (`PortNotExposed`, the R18 flake under load): poll for it instead of failing.
                let mut port = None;
                for _ in 0..40 {
                    match pg.get_host_port_ipv4(5432).await {
                        Ok(p) => {
                            port = Some(p);
                            break;
                        }
                        Err(e) => last = format!("{e:?}"),
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                if let Some(port) = port {
                    return (
                        format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"),
                        pg,
                    );
                }
                eprintln!("postgres container port never published (attempt {attempt}/3): {last}");
                continue;
            }
            Err(e) => {
                last = format!("{e:?}");
                eprintln!("postgres container start failed (attempt {attempt}/3): {last}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
    panic!("could not start postgres 18 (is Docker running?): {last}");
}

async fn server() -> &'static Server {
    SERVER
        .get_or_init(|| async {
            if let Ok(base) = std::env::var("TEST_DATABASE_URL")
                && !base.is_empty()
            {
                ensure_app_role(&base).await;
                return Server {
                    base,
                    _container: None,
                };
            }
            let (base, c) = start_container().await;
            if CONTAINER_ID.set(c.id().to_string()).is_ok() {
                // SAFETY: registering a plain `extern "C" fn()` with atexit.
                unsafe { libc::atexit(remove_container) };
            }
            ensure_app_role(&base).await;
            Server {
                base,
                _container: Some(c),
            }
        })
        .await
}

/// The runtime role's test-only password. Production creates `carebus_app` LOGIN through the gitops provisioning
/// script with its own secret; migration 0007 creates it NOLOGIN when it is missing.
pub const APP_ROLE_PASSWORD: &str = "carebus_app";

/// Once per server, before any database is migrated: `carebus_app` exists, may log in, and has the test password.
/// A server reused through `TEST_DATABASE_URL` may hold it NOLOGIN from an earlier 0007 run, hence the ALTER.
async fn ensure_app_role(base: &str) {
    let admin = PgPool::connect(base)
        .await
        .expect("connect to the test postgres server");
    sqlx::query(
        "DO $$ BEGIN
             IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'carebus_app') THEN
                 CREATE ROLE carebus_app LOGIN PASSWORD 'carebus_app' NOSUPERUSER NOBYPASSRLS;
             ELSE
                 ALTER ROLE carebus_app LOGIN PASSWORD 'carebus_app' NOSUPERUSER NOBYPASSRLS;
             END IF;
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
         END $$",
    )
    .execute(&admin)
    .await
    .expect("create the carebus_app test role");
    admin.close().await;
}

/// The same URL with another user and password (the runtime role's connection to the same database).
fn with_user(url: &str, user: &str, password: &str) -> String {
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let rest = &url[scheme_end..];
    let host_start = match (rest.find('@'), rest.find('/')) {
        (Some(at), Some(slash)) if at < slash => at + 1,
        (Some(at), None) => at + 1,
        _ => 0,
    };
    format!(
        "{}{user}:{password}@{}",
        &url[..scheme_end],
        &rest[host_start..]
    )
}

/// A fresh, empty database on the shared server; returns its URL.
async fn database() -> String {
    let srv = server().await;
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("cb05_t_{}_{nanos}_{n}", std::process::id());
    let admin = PgPool::connect(&srv.base)
        .await
        .expect("connect to the test postgres server");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create test database");
    admin.close().await;
    with_db_name(&srv.base, &name)
}

/// A fixed AES-256 test key (bytes 0..32), encoded at runtime so no key literal is committed.
/// Production reads CAREBUS_SOURCE_KEY from a SOPS secret.
pub fn source_key() -> carebus_api::crypto::SourceKey {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.encode((0u8..32).collect::<Vec<u8>>());
    carebus_api::crypto::SourceKey::from_base64(&raw).unwrap()
}

/// The bootstrap tenant migration 0006 adopts every pre-W3 row into (minted outside care-bus). The static strata
/// tokens are pinned to it (`strata_tenant`), and the strata fakes refuse a request whose `X-Tenant-Id` is not this.
pub const BOOT_TENANT: &str = "0195f48b-d000-7000-8000-000000000099";
/// The bootstrap job's external key (migration 0006).
pub const BOOT_JOB: &str = "customer_care_agent";
/// A second tenant for isolation tests; never provisioned by a migration.
pub const OTHER_TENANT: &str = "7f1c9a2e-0000-4000-8000-00000000fe12";
/// Every table 0006 gave a `tenant_id` + `job_id` (users and jobs carry only `tenant_id`).
pub const TENANT_TABLES: [&str; 17] = [
    "connectors",
    "connector_field_mappings",
    "context_spaces",
    "ontology_entities",
    "ontology_attributes",
    "ontology_relations",
    "procedures",
    "procedure_steps",
    "procedure_executions",
    "audit_events",
    "case_history",
    "sources",
    "source_runs",
    "usage_ledger",
    "metering_outbox",
    "gate_questions",
    "gate_verdicts",
];

/// Inserts a tenant through an owner/superuser pool (RLS does not apply to it); returns its id.
pub async fn insert_tenant(db: &PgPool, id: &str) -> uuid::Uuid {
    let id: uuid::Uuid = id.parse().unwrap();
    sqlx::query("INSERT INTO tenants (id, name, updated_by) VALUES ($1, 'Test tenant', 'test')")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    id
}

/// Inserts a job in `tenant` through an owner/superuser pool; returns its internal id.
pub async fn insert_job(db: &PgPool, tenant: uuid::Uuid, key: &str) -> uuid::Uuid {
    sqlx::query_scalar("INSERT INTO jobs (tenant_id, key, label) VALUES ($1, $2, $2) RETURNING id")
        .bind(tenant)
        .bind(key)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Inserts one tenant connector `id` in (tenant, job) through an owner/superuser pool.
pub async fn insert_connector(db: &PgPool, tenant: uuid::Uuid, job: uuid::Uuid, id: &str) {
    sqlx::query(
        "INSERT INTO connectors (tenant_id, job_id, id, name, system, base_url, auth_type)
         VALUES ($1, $2, $3, $3, 'test', 'https://example.test', 'none')",
    )
    .bind(tenant)
    .bind(job)
    .bind(id)
    .execute(db)
    .await
    .unwrap();
}

/// Binds `use_name` (`qualify` / `gates`) of the bootstrap tenant to a fresh token-less provider (kind `openai` for
/// qualify, `laya` for gates) through an owner/superuser pool; returns the provider id.
pub async fn bind_llm_use(db: &PgPool, use_name: &str) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    let kind = if use_name == "gates" {
        "laya"
    } else {
        "openai"
    };
    sqlx::query(
        "INSERT INTO llm_providers (tenant_id, id, name, kind, base_url, updated_by)
         VALUES ($1::uuid, $2, $3, $4, 'https://llm.example.test/v1', 'test')",
    )
    .bind(BOOT_TENANT)
    .bind(id)
    .bind(format!("{use_name} provider"))
    .bind(kind)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO llm_provider_uses (tenant_id, \"use\", provider_id, model, updated_by)
         VALUES ($1::uuid, $2, $3, 'test-model', 'test')",
    )
    .bind(BOOT_TENANT)
    .bind(use_name)
    .bind(id)
    .execute(db)
    .await
    .unwrap();
    id
}

/// `database_url` is the superuser (migration owner); the app pool connects to the same database as `carebus_app`.
pub fn test_config(database_url: String, test: &str) -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        database_app_url: with_user(&database_url, "carebus_app", APP_ROLE_PASSWORD),
        database_url,
        // CI's rust-library archetype exports REDIS_URL; same lookup as crates/carebus tests.
        redis_url: std::env::var("TEST_REDIS_URL")
            .or_else(|_| std::env::var("REDIS_URL"))
            .unwrap_or_else(|_| "redis://127.0.0.1:6391".into()),
        prefix: format!("test:api:{test}:{}:", uuid::Uuid::new_v4().simple()),
        keycloak_issuer: TEST_ISSUER.into(),
        keycloak_audience: TEST_AUDIENCE.into(),
        keycloak_trusted_issuers: vec![TEST_ISSUER.into(), TEST_SERVICE_ISSUER.into()],
        keycloak_service_only_realms: vec!["secure-apps".into()],
        signup: Default::default(),
        strata_rag_url: None,
        strata_rag_token: None,
        strata_ingest_url: None,
        strata_ingest_token: None,
        strata_tenant: Some(BOOT_TENANT.parse().unwrap()),
        strata_bundle: None,
        source_key: Some(source_key()),
        source_fetch_allow_loopback: false,
        demo_backends_url: None,
        demo_backends_token: None,
        mcp_url: None,
        mcp_internal_url: None,
        service_tokens: vec!["svc-test-token".into()],
        cors_origins: vec![],
        licence: Default::default(),
        metering: Default::default(),
        telegram: None,
        service_actor: None,
    }
}

pub async fn env(test: &str) -> TestEnv {
    env_with(test, |_| {}).await
}

/// A fresh database and its test config, adjusted by `tweak`; nothing is built yet.
pub async fn config_with(test: &str, tweak: impl FnOnce(&mut Config)) -> Config {
    let url = database().await;
    let mut cfg = test_config(url, test);
    tweak(&mut cfg);
    cfg
}

/// Like `env`, with a hook to adjust the config before the state is built.
pub async fn env_with(test: &str, tweak: impl FnOnce(&mut Config)) -> TestEnv {
    let cfg = config_with(test, tweak).await;
    let owner_url = cfg.database_url.clone();
    let state = carebus_api::state::build_for_tests(cfg)
        .await
        .expect("state");
    let owner = owner_pool(&owner_url).await;
    provision_users(&owner).await;
    let app = carebus_api::router(state.clone());
    TestEnv { state, app, owner }
}

/// `u-admin` (tenant_admin) and `u-user` (tenant_user) in the bootstrap tenant, as provision-down would write them.
async fn provision_users(owner: &PgPool) {
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-admin', $1::uuid, 'tenant_admin', 'A. Bakker', 'test'),
                ('u-user', $1::uuid, 'tenant_user', 'U. Jansen', 'test')",
    )
    .bind(BOOT_TENANT)
    .execute(owner)
    .await
    .expect("provision test users");
}

/// The superuser pool, with the bootstrap tenant/job as session GUCs (column defaults, migration 0007).
async fn owner_pool(url: &str) -> PgPool {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    let plain = PgPool::connect(url).await.expect("owner connect");
    let job: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1::uuid AND key = $2")
            .bind(BOOT_TENANT)
            .bind(BOOT_JOB)
            .fetch_one(&plain)
            .await
            .expect("bootstrap job");
    plain.close().await;
    let opts: PgConnectOptions = url.parse().expect("owner url");
    let opts = opts.options([
        ("app.current_tenant", BOOT_TENANT.to_string()),
        ("app.current_job", job.to_string()),
    ]);
    PgPoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .expect("owner pool")
}

/// The superuser pool (fixtures and assertions; RLS does not apply).
pub fn pool(e: &TestEnv) -> &PgPool {
    &e.owner
}

/// The runtime pool the API serves through (`carebus_app`, under RLS).
pub fn app_pool(e: &TestEnv) -> &PgPool {
    &e.state.db
}

pub async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(req)
        .await
        .expect("router is infallible");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, v)
}

pub fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::get(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

pub fn json(method: &str, uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

pub const TEST_SECRET: &[u8] = b"carebus-api-test-secret";
pub const TEST_ISSUER: &str = "https://issuer.test/realms/portal";
pub const TEST_AUDIENCE: &str = "care-bus";
/// The service-account realm (secure-apps): verifies on the signup doors only.
pub const TEST_SERVICE_ISSUER: &str = "https://issuer.test/realms/secure-apps";

/// A client-credentials token from secure-apps: `azp`, `aud`, realm roles.
pub fn service(azp: &str, aud: &[&str], realm_roles: &[&str]) -> String {
    sign(&json!({
        "sub": format!("service-account-{azp}"), "azp": azp, "exp": in_five_minutes(),
        "iss": TEST_SERVICE_ISSUER, "aud": aud,
        "realm_access": {"roles": realm_roles}
    }))
}

fn sign(claims: &Value) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(TEST_SECRET),
    )
    .unwrap()
}

fn in_five_minutes() -> i64 {
    (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp()
}

/// HS256 token for the provisioned tenant admin `u-admin`, with `client_roles` on the `care-bus` client;
/// `iss`/`aud`/`exp` overridable for the negative tests.
pub fn token_with(client_roles: &[&str], iss: &str, aud: &str, exp: i64) -> String {
    sign(&json!({
        "sub": "u-admin", "preferred_username": "a.bakker", "exp": exp,
        "iss": iss, "aud": aud,
        "resource_access": {TEST_AUDIENCE: {"roles": client_roles}}
    }))
}

/// A valid token for `sub` carrying `client_roles` on the `care-bus` client (no `realm_access`).
pub fn token_for(sub: &str, client_roles: &[&str]) -> String {
    sign(&json!({
        "sub": sub, "preferred_username": sub, "exp": in_five_minutes(),
        "iss": TEST_ISSUER, "aud": TEST_AUDIENCE,
        "resource_access": {TEST_AUDIENCE: {"roles": client_roles}}
    }))
}

/// A valid token whose roles are realm roles only (the pre-W3 shape): they must grant nothing.
pub fn token_with_realm(sub: &str, realm_roles: &[&str]) -> String {
    sign(&json!({
        "sub": sub, "preferred_username": sub, "exp": in_five_minutes(),
        "iss": TEST_ISSUER, "aud": TEST_AUDIENCE,
        "realm_access": {"roles": realm_roles}
    }))
}

/// `u-admin`: tenant_admin of the bootstrap tenant (provisioned by `env_with`).
pub fn admin() -> String {
    token_for("u-admin", &[])
}
/// `u-user`: tenant_user of the bootstrap tenant (provisioned by `env_with`).
pub fn user() -> String {
    token_for("u-user", &[])
}
/// `u-super`: client role `super_admin`, no `users` row; must name a tenant with `X-Active-Tenant-Id`.
pub fn super_admin() -> String {
    token_for("u-super", &["super_admin"])
}

/// Transitional: the pre-W3 realm-role helper, mapped onto the provisioned users so test files not yet moved to
/// `admin()` / `user()` keep compiling (`care-admin` → `admin()`, `care-viewer` → `user()`, anything else → an
/// unprovisioned subject). Plan 10 Task 7 replaces every call site.
pub fn token(roles: &[&str]) -> String {
    if roles.contains(&"care-admin") {
        admin()
    } else if roles.contains(&"care-viewer") {
        user()
    } else {
        token_for("u-unprovisioned", &[])
    }
}

/// Adds `X-Active-Tenant-Id: <tenant>` to a request.
pub fn with_tenant(mut req: Request<Body>, tenant: &str) -> Request<Body> {
    req.headers_mut()
        .insert("x-active-tenant-id", tenant.parse().unwrap());
    req
}

/// A job-scoped API path in the bootstrap job: `j("/procedures")` = `/api/jobs/customer_care_agent/procedures`.
pub fn j(path: &str) -> String {
    format!("/api/jobs/{BOOT_JOB}{path}")
}

// ---------------------------------------------------------------------------------------------------------------
// Contract fixtures (plan 05 Task 15, decision D2)
// ---------------------------------------------------------------------------------------------------------------

/// The Redis key prefix of the running test. It is removed from every string before a fixture is written,
/// because production runs with the empty default prefix and the web must see production-shaped keys.
pub static FIXTURE_PREFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/lib/api/fixtures")
}

/// With UPDATE_FIXTURES=1, write the normalised `v` to web/src/lib/api/fixtures/{name}.json (pretty, trailing
/// newline). Otherwise compare the SHAPE of `v` (object keys and JSON value kinds, arrays by their first element)
/// with the checked-in file and fail on any difference, so a struct change cannot silently drift from the web.
pub fn fixture(name: &str, v: &Value) {
    let dir = fixture_dir();
    let path = dir.join(format!("{name}.json"));
    if std::env::var("UPDATE_FIXTURES").as_deref() == Ok("1") {
        std::fs::create_dir_all(&dir).unwrap();
        let out = normalize(v, FIXTURE_PREFIX.get().map(String::as_str).unwrap_or(""));
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&out).unwrap()),
        )
        .unwrap();
        return;
    }
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing fixture {name}.json: run UPDATE_FIXTURES=1 cargo test -p carebus-api --test it contract_fixtures")
    }))
    .unwrap();
    assert_eq!(
        shape(v),
        shape(&stored),
        "response shape of `{name}` changed: regenerate fixtures and update web types.ts"
    );
}

pub fn shape(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), shape(x))).collect()),
        Value::Array(a) => Value::Array(a.first().map(shape).into_iter().collect()),
        Value::String(_) => Value::String("string".into()),
        Value::Number(_) => Value::String("number".into()),
        Value::Bool(_) => Value::String("bool".into()),
        Value::Null => Value::String("null".into()),
    }
}

/// The fake upstreams' own ids (strata_rag stores/documents) are fixed on purpose and kept verbatim.
const FAKE_NS: &str = "7f1c9a2e-";

#[derive(PartialEq)]
enum Kind {
    Ts(chrono::DateTime<chrono::Utc>),
    Day(chrono::NaiveDate),
    EntryId((u64, u64)),
    Uuid,
    Case,
    Conv,
    Local,
    Plain,
}

fn classify(s: &str) -> Kind {
    let hex = |t: &str| {
        t.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    };
    if let Some((a, b)) = s.split_once('-')
        && a.len() >= 13
        && a.bytes().all(|c| c.is_ascii_digit())
        && !b.is_empty()
        && b.bytes().all(|c| c.is_ascii_digit())
    {
        return Kind::EntryId((a.parse().unwrap_or(0), b.parse().unwrap_or(0)));
    }
    if s.len() >= 20
        && s.contains('T')
        && let Ok(t) = chrono::DateTime::parse_from_rfc3339(s)
    {
        return Kind::Ts(t.with_timezone(&chrono::Utc));
    }
    if s.len() == 10
        && let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
    {
        return Kind::Day(d);
    }
    if s.len() == 36 && !s.starts_with(FAKE_NS) && uuid::Uuid::parse_str(s).is_ok() {
        return Kind::Uuid;
    }
    if s.len() == 13 && s.starts_with("CASE-") && hex(&s[5..]) {
        return Kind::Case;
    }
    if s.len() == 37 && s.starts_with("conv-") && hex(&s[5..]) {
        return Kind::Conv;
    }
    if s.starts_with("http://127.0.0.1:") {
        return Kind::Local;
    }
    Kind::Plain
}

#[derive(Default)]
struct Seen {
    ts: std::collections::BTreeSet<chrono::DateTime<chrono::Utc>>,
    /// Every millisecond-number occurrence in traversal order. Equal values are NOT merged: two envelopes made in
    /// the same millisecond must map the same way as two made in different ones.
    ms: Vec<u64>,
    /// `ms_rank[i]` = rank of the i-th occurrence when sorted by (value, traversal index).
    ms_rank: Vec<usize>,
    ids: std::collections::BTreeSet<(u64, u64)>,
    days: std::collections::BTreeSet<chrono::NaiveDate>,
    uuids: Vec<String>,
    cases: Vec<String>,
    convs: Vec<String>,
}

const MS_KEYS: [&str; 2] = ["published_at_ms", "at_ms"];

fn strip_prefix(v: &Value, prefix: &str) -> Value {
    match v {
        Value::String(s) if !prefix.is_empty() => {
            Value::String(s.strip_prefix(prefix).unwrap_or(s).to_string())
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| strip_prefix(x, prefix)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), strip_prefix(x, prefix)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn collect(v: &Value, key: Option<&str>, seen: &mut Seen) {
    match v {
        Value::String(s) => match classify(s) {
            Kind::Ts(t) => {
                seen.ts.insert(t);
            }
            Kind::Day(d) => {
                seen.days.insert(d);
            }
            Kind::EntryId(ms) => {
                seen.ids.insert(ms);
            }
            Kind::Uuid if !seen.uuids.contains(s) => seen.uuids.push(s.clone()),
            Kind::Case if !seen.cases.contains(s) => seen.cases.push(s.clone()),
            Kind::Conv if !seen.convs.contains(s) => seen.convs.push(s.clone()),
            _ => {}
        },
        Value::Number(n) if key.is_some_and(|k| MS_KEYS.contains(&k)) => {
            if let Some(ms) = n.as_u64() {
                seen.ms.push(ms);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect(x, key, seen)),
        Value::Object(m) => m.iter().for_each(|(k, x)| collect(x, Some(k), seen)),
        _ => {}
    }
}

fn rank<T: Ord>(set: &std::collections::BTreeSet<T>, x: &T) -> usize {
    set.iter().position(|y| y == x).unwrap_or(0)
}

fn replace(
    v: &Value,
    key: Option<&str>,
    seen: &Seen,
    base_ms: i64,
    next: &std::cell::Cell<usize>,
) -> Value {
    use chrono::{Duration, SecondsFormat, TimeZone, Utc};
    let base = Utc.timestamp_millis_opt(base_ms).unwrap();
    match v {
        Value::String(s) => Value::String(match classify(s) {
            Kind::Ts(t) => (base + Duration::seconds(rank(&seen.ts, &t) as i64))
                .to_rfc3339_opts(SecondsFormat::Micros, true),
            Kind::Day(d) => {
                // the last (newest) day is the fixed "today"; earlier ones count back from it
                let last = base.date_naive();
                let from_end = seen.days.len() - 1 - rank(&seen.days, &d);
                (last - Duration::days(from_end as i64))
                    .format("%Y-%m-%d")
                    .to_string()
            }
            Kind::EntryId(id) => format!("{}-0", base_ms + 1000 * rank(&seen.ids, &id) as i64),
            Kind::Uuid => format!(
                "00000000-0000-4000-8000-{:012x}",
                seen.uuids.iter().position(|u| u == s).unwrap() + 1
            ),
            Kind::Case => format!(
                "CASE-{:08x}",
                0x5eed_0001 + seen.cases.iter().position(|u| u == s).unwrap()
            ),
            Kind::Conv => format!(
                "conv-{:032x}",
                seen.convs.iter().position(|u| u == s).unwrap() + 1
            ),
            // a stub server's random port must not reach the checked-in files
            Kind::Local => {
                let rest = &s["http://127.0.0.1:".len()..];
                format!(
                    "https://oms.example{}",
                    rest.find('/').map_or("", |i| &rest[i..])
                )
            }
            Kind::Plain => s.clone(),
        }),
        Value::Number(n) if key.is_some_and(|k| MS_KEYS.contains(&k)) && n.is_u64() => {
            let i = next.get();
            next.set(i + 1);
            json!(base_ms as u64 + 1000 * seen.ms_rank[i] as u64)
        }
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| replace(x, key, seen, base_ms, next))
                .collect(),
        ),
        Value::Object(m) => {
            let mut entries: Vec<(&String, &Value)> = m.iter().collect();
            // Redis returns a big hash in its own order, which differs per server process.
            if key == Some("hash") {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, x)| (k.clone(), replace(x, Some(k), seen, base_ms, next)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// Makes a captured response reproducible: the test's Redis prefix is dropped, and every time, stream entry id,
/// random uuid, random case/conversation id and stub-server port is replaced by a stable value that keeps the
/// ORDER of the original (sorted timestamps stay sorted, first-seen ids stay first-seen). Keys, nesting and value
/// kinds are untouched, so the file is exactly the shape the route returned.
pub fn normalize(v: &Value, prefix: &str) -> Value {
    use chrono::{TimeZone, Utc};
    let v = strip_prefix(v, prefix);
    let mut seen = Seen::default();
    collect(&v, None, &mut seen);
    let mut order: Vec<usize> = (0..seen.ms.len()).collect();
    order.sort_by_key(|&i| (seen.ms[i], i));
    seen.ms_rank = vec![0; order.len()];
    for (rank, &i) in order.iter().enumerate() {
        seen.ms_rank[i] = rank;
    }
    let base_ms = Utc
        .with_ymd_and_hms(2026, 10, 15, 10, 0, 0)
        .unwrap()
        .timestamp_millis();
    replace(&v, None, &seen, base_ms, &std::cell::Cell::new(0))
}

/// A local server whose `/health` answers `303 See Other, Location: /elsewhere`, and `/elsewhere` answers 200.
/// A probe that follows redirects would report it reachable. Returns `http://127.0.0.1:<port>`.
pub async fn redirecting_upstream() -> String {
    use axum::response::Redirect;
    use axum::routing::get;
    let app = Router::new()
        .route("/health", get(|| async { Redirect::to("/elsewhere") }))
        .route("/elsewhere", get(|| async { "fine" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

/// One multipart part: `file_name` makes it a file part.
pub struct Part<'a> {
    pub name: &'a str,
    pub file_name: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
}

pub fn multipart(uri: &str, token: Option<&str>, parts: &[Part<'_>]) -> Request<Body> {
    const B: &str = "carebus-test-boundary-7MA4YWxkTrZu0gW";
    let mut body = Vec::new();
    for p in parts {
        body.extend_from_slice(
            format!(
                "--{B}\r\nContent-Disposition: form-data; name=\"{}\"",
                p.name
            )
            .as_bytes(),
        );
        if let Some(f) = p.file_name {
            body.extend_from_slice(format!("; filename=\"{f}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        if let Some(ct) = p.content_type {
            body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(p.body);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{B}--\r\n").as_bytes());
    let mut b =
        Request::post(uri).header("content-type", format!("multipart/form-data; boundary={B}"));
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body)).unwrap()
}

/// Every audit row as one JSON string, for "this text never appears in the audit log" assertions.
pub async fn audit_text(e: &TestEnv) -> String {
    let rows: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(a) FROM audit_events a ORDER BY id")
        .fetch_all(pool(e))
        .await
        .unwrap();
    Value::Array(rows).to_string()
}
