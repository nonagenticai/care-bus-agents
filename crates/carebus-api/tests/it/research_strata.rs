//! The researcher's `strata_search` and wall-check store tree (research::tools::LiveTools) search strata as the run's
//! tenant with THAT tenant's strata-rag token from its MCP credentials: no static or pinned token, any tenant; a 401
//! renews the credentials and asks once more; without MCP there is no strata at all.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use carebus::mcp_http::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use carebus_api::research::Profile;
use carebus_api::research::tools::{
    LiveTools, STRATA_SEARCH, StepStatus, StrataCfg, ToolCtx, ToolSet,
};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Default)]
struct Fake {
    /// (path, bearer, x-tenant-id) of every strata call
    calls: Vec<(String, String, String)>,
    /// (x-app-id, x-actor-id) of every strata call
    identity: Vec<(String, String)>,
    /// strata calls to refuse with 401 whatever the token
    refuse_next: usize,
    /// (tenant, fresh) of every credentials fetch
    fetches: Vec<(Uuid, bool)>,
    /// tenant -> credentials fetched so far
    issued: HashMap<Uuid, usize>,
}

type Shared = Arc<Mutex<Fake>>;

fn strata_token(tenant: Uuid, n: usize) -> String {
    format!("str-{tenant}-{n}")
}

/// One strata call: accepted when its bearer is the `X-Tenant-Id` tenant's latest strata token.
fn strata_ok(m: &Shared, path: &str, h: &HeaderMap) -> bool {
    let header = |k: &str| {
        h.get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let (bearer, tenant) = (header("authorization"), header("x-tenant-id"));
    let mut g = m.lock().unwrap();
    g.calls.push((path.into(), bearer.clone(), tenant.clone()));
    g.identity.push((header("x-app-id"), header("x-actor-id")));
    let latest = tenant
        .parse::<Uuid>()
        .ok()
        .and_then(|t| g.issued.get(&t).map(|n| strata_token(t, *n)));
    if g.refuse_next > 0 {
        g.refuse_next -= 1;
        return false;
    }
    latest.is_some_and(|t| bearer == format!("Bearer {t}"))
}

async fn fake_strata(m: Shared) -> String {
    let (q, s) = (m.clone(), m.clone());
    let app = Router::new()
        .route(
            "/api/query",
            post(move |h: HeaderMap| {
                let m = q.clone();
                async move {
                    if !strata_ok(&m, "/api/query", &h) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    axum::Json(json!({"results": [{"chunk_id": "c-1", "document_id": "d-1",
                                                    "title": "T", "source": "s", "text": "Ala"}]}))
                    .into_response()
                }
            }),
        )
        .route(
            "/api/stores",
            get(move |h: HeaderMap| {
                let m = s.clone();
                async move {
                    if !strata_ok(&m, "/api/stores", &h) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    axum::Json(json!({"stores": []})).into_response()
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

/// Per-tenant credentials `lic-<tenant>-<n>` / `str-<tenant>-<n>`, n counting that tenant's fetches. The MCP URL is
/// never contacted: these tests only read the credentials.
fn mcp(m: &Shared) -> Arc<HttpMcp> {
    let m = m.clone();
    let creds = TenantCredentials::new(FnSource(move |tenant: Uuid, fresh: bool| {
        let m = m.clone();
        async move {
            let n = {
                let mut g = m.lock().unwrap();
                g.fetches.push((tenant, fresh));
                let n = g.issued.entry(tenant).or_default();
                *n += 1;
                *n
            };
            McpCredentials::new(
                &format!("lic-{tenant}-{n}"),
                &strata_token(tenant, n),
                Instant::now() + Duration::from_secs(600),
            )
        }
    }));
    Arc::new(HttpMcp::new("http://127.0.0.1:9/mcp".into(), creds))
}

async fn tools(with_mcp: bool) -> (LiveTools, Shared) {
    tools_as(with_mcp, None).await
}

async fn tools_as(with_mcp: bool, service_actor: Option<Uuid>) -> (LiveTools, Shared) {
    let m: Shared = Arc::default();
    let url = fake_strata(m.clone()).await;
    let mcp = with_mcp.then(|| mcp(&m));
    (
        LiveTools::new(
            mcp,
            Some(StrataCfg { url, service_actor }),
            reqwest::Client::new(),
        ),
        m,
    )
}

/// strata_rag learns which app asks and who acted: `X-App-Id: care-bus`, and `X-Actor-Id` = the run's creator,
/// else care-bus' service account (strata relays both to its metering and to inference-router).
#[tokio::test]
async fn a_strata_search_names_care_bus_and_the_runs_creator_else_the_service_actor() {
    let service = Uuid::new_v4();
    let creator = Uuid::new_v4();
    let (t, m) = tools_as(true, Some(service)).await;
    let stores = vec![Uuid::new_v4().to_string()];
    let tenant = Uuid::new_v4();
    for actor in [Some(creator), None] {
        let ctx = ToolCtx {
            tenant,
            profile: Profile::Research,
            store_ids: &stores,
            actor,
            run: None,
            created_by: "",
        };
        let out = t.call(&ctx, STRATA_SEARCH, &json!({"query": "x"})).await;
        assert_eq!(out.status, StepStatus::Ok, "{}", out.text);
    }
    let g = m.lock().unwrap();
    assert_eq!(
        g.identity,
        vec![
            ("care-bus".to_string(), creator.to_string()),
            ("care-bus".to_string(), service.to_string()),
        ]
    );
}

#[tokio::test]
async fn each_tenant_searches_strata_with_its_own_token() {
    let (t, m) = tools(true).await;
    let stores = vec![Uuid::new_v4().to_string()];
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    for tenant in [a, b, a] {
        let ctx = ToolCtx {
            tenant,
            profile: Profile::Research,
            store_ids: &stores,
            actor: None,
            run: None,
            created_by: "",
        };
        let out = t
            .call(&ctx, STRATA_SEARCH, &json!({"query": "alimenty"}))
            .await;
        assert_eq!(out.status, StepStatus::Ok, "{}", out.text);
        assert!(out.text.contains("c-1"), "{}", out.text);
    }
    let g = m.lock().unwrap();
    let seen: Vec<(String, String)> = g
        .calls
        .iter()
        .map(|(_, bearer, tenant)| (bearer.clone(), tenant.clone()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (format!("Bearer {}", strata_token(a, 1)), a.to_string()),
            (format!("Bearer {}", strata_token(b, 1)), b.to_string()),
            (format!("Bearer {}", strata_token(a, 1)), a.to_string()),
        ]
    );
    // cached per tenant: one fetch each
    assert_eq!(g.fetches, vec![(a, false), (b, false)]);
}

#[tokio::test]
async fn a_refused_strata_token_is_renewed_and_asked_once_more() {
    let (t, m) = tools(true).await;
    let stores = vec![Uuid::new_v4().to_string()];
    let a = Uuid::new_v4();
    let ctx = ToolCtx {
        tenant: a,
        profile: Profile::Research,
        store_ids: &stores,
        actor: None,
        run: None,
        created_by: "",
    };
    m.lock().unwrap().refuse_next = 1;
    let out = t.call(&ctx, STRATA_SEARCH, &json!({"query": "x"})).await;
    assert_eq!(out.status, StepStatus::Ok, "{}", out.text);
    {
        let g = m.lock().unwrap();
        assert_eq!(g.fetches, vec![(a, false), (a, true)]);
        assert_eq!(g.calls.len(), 2);
        assert_eq!(g.calls[1].1, format!("Bearer {}", strata_token(a, 2)));
    }
    // once only: refused again after the renewal is an error, not a loop
    m.lock().unwrap().refuse_next = 2;
    let out = t.call(&ctx, STRATA_SEARCH, &json!({"query": "x"})).await;
    assert_eq!(out.status, StepStatus::Error, "{}", out.text);
    assert!(
        !out.text.contains("str-"),
        "no token in the error: {}",
        out.text
    );
    let g = m.lock().unwrap();
    assert_eq!(g.calls.len(), 4);
    assert_eq!(g.fetches.len(), 3);
}

#[tokio::test]
async fn the_wall_check_reads_the_store_tree_as_the_runs_tenant() {
    let (t, m) = tools(true).await;
    let a = Uuid::new_v4();
    assert_eq!(t.store_tree(a).await.unwrap(), Some(json!({"stores": []})));
    let g = m.lock().unwrap();
    assert_eq!(
        g.calls,
        vec![(
            "/api/stores".to_string(),
            format!("Bearer {}", strata_token(a, 1)),
            a.to_string()
        )]
    );
}

#[tokio::test]
async fn without_mcp_there_is_no_strata() {
    let (t, m) = tools(false).await;
    let stores = vec![Uuid::new_v4().to_string()];
    let ctx = ToolCtx {
        tenant: Uuid::new_v4(),
        profile: Profile::Research,
        store_ids: &stores,
        actor: None,
        run: None,
        created_by: "",
    };
    let out = t.call(&ctx, STRATA_SEARCH, &json!({"query": "x"})).await;
    assert_eq!(out.status, StepStatus::Denied, "{}", out.text);
    assert_eq!(t.store_tree(ctx.tenant).await, Ok(None::<Value>));
    assert!(t.specs(&ctx).await.is_empty());
    assert!(m.lock().unwrap().calls.is_empty(), "strata is never called");
}
