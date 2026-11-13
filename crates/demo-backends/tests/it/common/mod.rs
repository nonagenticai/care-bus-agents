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
        let admin = db::connect(base).await.expect("connect TEST_DATABASE_URL");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await
            .expect("create test database");
        admin.close().await;
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
