//! Integration harness: a throwaway Postgres 18, migrations applied, a seeded app.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use demo_backends::{AppState, Config, api, db, faults::FaultState, seed};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::core::IntoContainerPort;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tower::ServiceExt;

pub const TOKEN: &str = "test-token-demo";

pub struct TestDb {
    pub pool: PgPool,
    pub url: String,
    _c: Option<ContainerAsync<Postgres>>,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Scratch databases this process created on a shared `TEST_DATABASE_URL` server, dropped at process exit.
static SCRATCH_DBS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static SCRATCH_BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static CLEANUP_REGISTERED: std::sync::Once = std::sync::Once::new();

impl TestDb {
    pub async fn start() -> TestDb {
        if let Ok(base) = std::env::var("TEST_DATABASE_URL")
            && !base.is_empty()
        {
            return Self::start_in_existing(&base).await;
        }
        // CI pulls from the public registry unless TEST_PG_REGISTRY names another one.
        let reg = match std::env::var("TEST_PG_REGISTRY") {
            Ok(r) if !r.is_empty() => r,
            _ => "docker.io/library".to_string(),
        };
        let image = format!("{}/postgres", reg.trim_end_matches('/'));
        for attempt in 1..=3 {
            let port = free_port();
            let started = Postgres::default()
                .with_db_name("carebus_demo")
                .with_user("carebus_demo")
                .with_password("carebus_demo")
                .with_name(&image)
                .with_tag("18-alpine")
                .with_container_name(format!("cb02-pg-{port}"))
                .with_env_var("PGPORT", port.to_string())
                .with_mapped_port(port, port.tcp())
                .start()
                .await;
            let Ok(c) = started else {
                eprintln!("postgres start failed (attempt {attempt})");
                continue;
            };
            let url = format!("postgres://carebus_demo:carebus_demo@127.0.0.1:{port}/carebus_demo");
            if let Ok(pool) = db::connect(&url).await {
                db::migrate(&pool).await.expect("migrate");
                return TestDb {
                    pool,
                    url,
                    _c: Some(c),
                };
            }
        }
        panic!("could not start Postgres 18 for tests (is Docker running?)");
    }

    /// TEST_DATABASE_URL mode: no container; a uniquely named fresh database on the given server.
    async fn start_in_existing(base: &str) -> TestDb {
        let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("cb02_t_{}_{nanos}_{n}", std::process::id());
        // Register before the CREATE: a process that died between the CREATE and the registration would leak
        // the database. A SIGKILLed process still leaves its databases: nothing runs at exit.
        let _ = SCRATCH_BASE.set(base.to_string());
        CLEANUP_REGISTERED.call_once(|| {
            // SAFETY: registering a plain `extern "C" fn()` with atexit.
            unsafe { libc::atexit(drop_scratch_at_exit) };
        });
        let admin = db::connect(base).await.expect("connect TEST_DATABASE_URL");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await
            .expect("create test database");
        admin.close().await;
        if let Ok(mut dbs) = SCRATCH_DBS.lock() {
            dbs.push(name.clone());
        }
        let url = with_db_name(base, &name);
        let pool = db::connect(&url)
            .await
            .expect("connect fresh test database");
        db::migrate(&pool).await.expect("migrate");
        TestDb {
            pool,
            url,
            _c: None,
        }
    }
}

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

/// Drop the given scratch databases on `base`, returning how many were dropped.
pub async fn drop_scratch_databases(base: &str, names: &[String]) -> usize {
    let pool = match sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect(base)
        .await
    {
        Ok(pool) => pool,
        Err(e) => {
            eprintln!("scratch database cleanup: cannot connect to {base}: {e}");
            return 0;
        }
    };
    let mut dropped = 0;
    for name in names {
        let sql = format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)");
        match sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await {
            Ok(_) => dropped += 1,
            Err(e) => eprintln!("scratch database cleanup: {name}: {e}"),
        }
    }
    pool.close().await;
    dropped
}

/// Statics are never dropped and the harness runs no reaper, so drop this process's scratch databases at exit.
extern "C" fn drop_scratch_at_exit() {
    let Ok(names) = SCRATCH_DBS.lock().map(|dbs| dbs.clone()) else {
        return;
    };
    let Some(base) = SCRATCH_BASE.get() else {
        return;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let _ = rt.block_on(tokio::time::timeout(
        Duration::from_secs(5),
        drop_scratch_databases(base, &names),
    ));
}

pub fn test_config(url: &str) -> Config {
    Config {
        bind: "127.0.0.1:8096".parse().unwrap(),
        database_url: url.to_string(),
        tokens: vec![TOKEN.to_string()],
        today: chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap(),
        seed: 20261015,
        fault_sleep: Duration::from_millis(60),
    }
}

/// A seeded app with its own database. Keep `TestDb` alive for the test's duration.
pub async fn app(seed_value: u64) -> (Router, TestDb, AppState) {
    let db = TestDb::start().await;
    let cfg = Arc::new(test_config(&db.url));
    seed::reset(&db.pool, seed_value, cfg.today)
        .await
        .expect("seed");
    let state = AppState {
        pool: db.pool.clone(),
        cfg,
        faults: FaultState::default(),
    };
    (api::router(state.clone()), db, state)
}

pub async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}
