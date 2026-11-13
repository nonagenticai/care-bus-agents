use axum::extract::{Path, RawQuery};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::{Value, json};

const RAG_TOKEN: &str = "rag-secret-token";
const DEMO_TOKEN: &str = "demo-secret-token";

fn bearer_is(h: &HeaderMap, want: &str) -> bool {
    h.get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {want}"))
}

/// strata_rag: the bearer AND `X-Tenant-Id` naming the configured tenant (a pinned token refuses any other).
fn rag_auth(h: &HeaderMap) -> bool {
    bearer_is(h, RAG_TOKEN)
        && h.get("x-tenant-id").and_then(|v| v.to_str().ok()) == Some(crate::common::BOOT_TENANT)
}

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

/// Raw queries the fake strata_rag received on its documents route.
type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

fn doc(id: &str) -> Value {
    json!({"id": id, "store_id": "known", "source": "faq.md", "title": format!("Doc {id}"),
        "content_hash": "abc123", "token_count": 120, "chunk_count": 3,
        "created_at": "2026-10-15T10:00:00Z"})
}

/// strata_rag's real contract: `GET /api/stores/{id}/documents?limit&cursor` -> `{items, next}`.
/// Store ids pick the behaviour: `known` pages, `last` ends with `next:null`, `noitems` is malformed,
/// `denied` is 401; anything else has no route (404). Everything needs the bearer.
async fn fake_strata() -> String {
    fake_strata_logged().await.0
}

async fn fake_strata_logged() -> (String, Seen) {
    let seen: Seen = Default::default();
    let log = seen.clone();
    let url = serve(
        Router::new()
            .route(
                "/api/stores",
                get(|h: HeaderMap| async move {
                    if !rag_auth(&h) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    Json(
                        json!({"stores":[{"id":"s1","name":"care","kind":"router","children":[]}]}),
                    )
                    .into_response()
                }),
            )
            .route(
                "/api/query",
                post(|h: HeaderMap, Json(b): Json<Value>| async move {
                    if !rag_auth(&h) {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    if b["query"] == "bad" {
                        return (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            Json(json!({"detail":"nope"})),
                        )
                            .into_response();
                    }
                    Json(json!({"echo": b["query"], "results": []})).into_response()
                }),
            )
            .route(
                "/api/stores/{id}/documents",
                get(
                    move |h: HeaderMap, Path(id): Path<String>, RawQuery(q): RawQuery| {
                        let log = log.clone();
                        async move {
                            if !rag_auth(&h) {
                                return StatusCode::UNAUTHORIZED.into_response();
                            }
                            log.lock().unwrap().push(q.unwrap_or_default());
                            match id.as_str() {
                                "known" => {
                                    Json(json!({"items":[doc("d1"), doc("d2")], "next":"n2"}))
                                        .into_response()
                                }
                                "last" => {
                                    Json(json!({"items":[doc("d9")], "next": null})).into_response()
                                }
                                "noitems" => {
                                    Json(json!({"documents":[doc("d1")], "next_before":"x"}))
                                        .into_response()
                                }
                                "badnext" => Json(json!({"items":[], "next": 7})).into_response(),
                                "denied" => StatusCode::FORBIDDEN.into_response(),
                                _ => StatusCode::NOT_FOUND.into_response(),
                            }
                        }
                    },
                ),
            ),
    )
    .await;
    (url, seen)
}

/// demo-backends with its real response shapes (crates/demo-backends/src/api).
async fn fake_demo() -> String {
    serve(
        Router::new()
            .route(
                "/api/admin/scenarios/{name}",
                post(|h: HeaderMap, Path(n): Path<String>| async move {
                    assert!(bearer_is(&h, DEMO_TOKEN), "demo bearer sent");
                    assert!(h.get("x-tenant-id").is_none(), "demo-backends is not a strata service");
                    Json(json!({"applied": n, "changes": ["PAY-3399 captured again"]}))
                }),
            )
            .route(
                "/api/admin/scenarios",
                get(|| async {
                    Json(json!({"scenarios":[
                        {"name":"duplicate-charge","description":"Restore Anna's duplicate capture"},
                        {"name":"late-parcel","description":"The airfryer parcel is delayed"}]}))
                }),
            )
            // Body optional, like the real handler.
            .route(
                "/api/admin/reset",
                post(|body: String| async move {
                    let seed = serde_json::from_str::<Value>(&body)
                        .ok()
                        .map(|v| v["seed"].clone())
                        .unwrap_or(Value::Null);
                    Json(json!({"customers": 501, "orders": 1240, "shipments": 1180, "payments": 1302, "seed_seen": seed}))
                }),
            )
            .route(
                "/api/admin/faults",
                get(|| async { Json(json!({"faults": {}})) }).put(|Json(b): Json<Value>| async move {
                    let tool = b["tool"].as_str().unwrap_or_default().to_string();
                    Json(json!({"faults": {tool: {"mode": b["mode"], "remaining": b["count"]}}}))
                }),
            )
            .route(
                "/api/admin/records/{table}",
                get(|Path(t): Path<String>, RawQuery(q): RawQuery| async move {
                    Json(json!({"table": t, "primary_key": "order_id", "editable": ["status"],
                        "columns": ["order_id","customer_id","status"],
                        "rows": [{"order_id":"ORD-58790","customer_id":"C-10482","status":"in_transit"}],
                        "total": 1, "query_seen": q}))
                }),
            )
            .route(
                "/api/admin/records/{table}/{id}",
                get(|Path((_t, id)): Path<(String, String)>| async move {
                    if id == "missing" {
                        (StatusCode::NOT_FOUND, Json(json!({"error":{"code":"not_found","message":"no such record"}})))
                    } else {
                        (StatusCode::OK, Json(json!({"order_id": id, "customer_id": "C-10482", "status": "in_transit"})))
                    }
                })
                .put(|Path((_t, id)): Path<(String, String)>, Json(b): Json<Value>| async move {
                    Json(json!({"order_id": id, "customer_id": "C-10482", "status": b["status"]}))
                }),
            ),
    )
    .await
}

fn body_less_post(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::post(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

#[tokio::test]
async fn knowledge_proxy_normalizes_and_maps_failures() {
    let rag = fake_strata().await;
    let e = crate::common::env_with("knowledge", |c| {
        c.strata_rag_url = Some(rag.clone());
        c.strata_rag_token = Some(RAG_TOKEN.into());
    })
    .await;
    let viewer = crate::common::user();

    // the bearer is sent (the stub 401s without it); the list is a Paged
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores", Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["items"][0]["name"], "care");
    assert_eq!(b["items"][0]["children"], json!([]));
    assert_eq!(b["next"], Value::Null);
    assert!(b.get("stores").is_none(), "normalized, not passed through");

    let (_, q) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/knowledge/query",
            Some(&viewer),
            json!({"query":"refund time","route_mode":"hybrid"}),
        ),
    )
    .await;
    assert_eq!(q["echo"], "refund time");

    // other 4xx: status passed through, inside the envelope
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/knowledge/query",
            Some(&viewer),
            json!({"query":"bad"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "upstream_rejected");
    assert_eq!(
        b["error"]["message"],
        "strata_rag rejected the request (HTTP 422)."
    );
    let mut keys: Vec<&String> = b["error"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["code", "message"],
        "the binding error body has exactly code and message"
    );

    // upstream 404 (route not merged yet): 503, never an empty Paged
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores/s1/documents", Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["error"]["code"], "upstream_unavailable");
    assert_eq!(b["error"]["message"], "strata_rag is unavailable");
    // {id} cannot escape
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores/..%2Fadmin/documents", Some(&viewer)),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_path"))
    );
    assert_eq!(
        crate::common::send(&e.app, crate::common::get("/api/knowledge/stores", None))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn knowledge_documents_map_limit_before_and_next_onto_the_strata_contract() {
    let (rag, seen) = fake_strata_logged().await;
    let e = crate::common::env_with("knowledge-docs", |c| {
        c.strata_rag_url = Some(rag.clone());
        c.strata_rag_token = Some(RAG_TOKEN.into());
    })
    .await;
    let v = crate::common::user();
    let get = |uri: &'static str| crate::common::get(uri, Some(&v));

    // 1. limit/before -> limit/cursor; bearer sent (stub 401s without it); items + next pass through
    let (s, b) = crate::common::send(
        &e.app,
        get("/api/knowledge/stores/known/documents?limit=2&before=abc"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, json!({"items":[doc("d1"), doc("d2")], "next":"n2"}));
    assert_eq!(seen.lock().unwrap().pop().unwrap(), "limit=2&cursor=abc");

    // 2. clamp before forwarding; no `before` -> no cursor; default limit 50
    let (s, _) = crate::common::send(
        &e.app,
        get("/api/knowledge/stores/known/documents?limit=9999"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(seen.lock().unwrap().pop().unwrap(), "limit=500");
    crate::common::send(&e.app, get("/api/knowledge/stores/known/documents")).await;
    assert_eq!(seen.lock().unwrap().pop().unwrap(), "limit=50");

    // an opaque cursor is URL-encoded on the way through
    crate::common::send(
        &e.app,
        get("/api/knowledge/stores/known/documents?before=a%26b%3Dc%20%2B%2F"),
    )
    .await;
    assert_eq!(
        seen.lock().unwrap().pop().unwrap(),
        "limit=50&cursor=a%26b%3Dc%20%2B%2F"
    );

    // 3. next:null stays null
    let (s, b) = crate::common::send(&e.app, get("/api/knowledge/stores/last/documents")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, json!({"items":[doc("d9")], "next": null}));

    // 4. upstream 404 / 401 / 403 -> 503 upstream_unavailable
    for id in ["gone", "denied"] {
        let uri = format!("/api/knowledge/stores/{id}/documents");
        let (s, b) = crate::common::send(&e.app, crate::common::get(&uri, Some(&v))).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{id}");
        assert_eq!(b["error"]["code"], "upstream_unavailable", "{id}");
    }

    // 5. malformed bodies (no `items`, non-string `next`) -> 503, never an empty list
    for id in ["noitems", "badnext"] {
        let uri = format!("/api/knowledge/stores/{id}/documents");
        let (s, b) = crate::common::send(&e.app, crate::common::get(&uri, Some(&v))).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{id}");
        assert_eq!(b["error"]["code"], "upstream_unavailable", "{id}");
    }
}

#[tokio::test]
async fn a_tenant_the_token_is_not_pinned_to_is_409_and_never_sent() {
    // The static strata tokens name the tenant they act for; a request in any other tenant is refused here
    // (409 strata_not_provisioned_for_tenant), never sent with a token that acts for someone else.
    let rag = fake_strata().await;
    let e = crate::common::env_with("knowledge-othertenant", |c| {
        c.strata_rag_url = Some(rag.clone());
        c.strata_rag_token = Some(RAG_TOKEN.into());
        c.strata_tenant = Some(uuid::Uuid::new_v4());
    })
    .await;
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores", Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "strata_not_provisioned_for_tenant");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/knowledge/query",
            Some(&t),
            json!({"query":"x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "strata_not_provisioned_for_tenant");
}

#[tokio::test]
async fn knowledge_upstream_401_403_and_unconfigured_are_503() {
    let rag = fake_strata().await;
    // wrong token: the stub answers 401 / 403
    let e = crate::common::env_with("knowledge-badtoken", |c| {
        c.strata_rag_url = Some(rag.clone());
        c.strata_rag_token = Some("wrong".into());
    })
    .await;
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores", Some(&t)),
    )
    .await;
    assert_eq!(
        (s, b["error"]["message"].clone()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!("strata_rag is unavailable")
        )
    );
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores/known/documents", Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/knowledge/query",
            Some(&t),
            json!({"query":"x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);

    // no URL configured
    let e = crate::common::env("knowledge-unset").await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/knowledge/stores", Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["error"]["message"], "strata_rag is unavailable");
}

/// Demo routes are job-scoped, and their writes need a super_admin, who names the tenant with the header (a
/// tenant member's own tenant in the header is ignored, so every demo request carries it).
async fn send_boot(
    app: &Router,
    req: axum::http::Request<axum::body::Body>,
) -> (StatusCode, Value) {
    crate::common::send(
        app,
        crate::common::with_tenant(req, crate::common::BOOT_TENANT),
    )
    .await
}

#[tokio::test]
async fn demo_proxy_passes_shapes_audits_writes_and_names_the_upstream_when_down() {
    let demo = fake_demo().await;
    let e = crate::common::env_with("demo", |c| {
        c.demo_backends_url = Some(demo.clone());
        c.demo_backends_token = Some(DEMO_TOKEN.into());
    })
    .await;
    let admin = crate::common::super_admin();
    let viewer = crate::common::user();

    let (_, d) = send_boot(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/demo/scenarios/duplicate-charge"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(d["applied"], "duplicate-charge");
    assert!(d["changes"].is_array());

    let (s, r) = send_boot(
        &e.app,
        crate::common::get(
            &crate::common::j("/demo/records/orders?q=C-10482&limit=50&offset=0"),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["query_seen"], "q=C-10482&limit=50&offset=0",
        "query string passed through unchanged"
    );
    assert_eq!(
        (r["primary_key"].clone(), r["total"].clone()),
        (json!("order_id"), json!(1))
    );
    assert_eq!(r["rows"][0]["order_id"], "ORD-58790");
    assert_eq!(
        send_boot(
            &e.app,
            crate::common::get(
                &crate::common::j("/demo/records/orders/ORD-58790"),
                Some(&viewer)
            )
        )
        .await
        .1["order_id"],
        "ORD-58790"
    );
    let (s, nf) = send_boot(
        &e.app,
        crate::common::get(
            &crate::common::j("/demo/records/orders/missing"),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "upstream 4xx passes through");
    assert_eq!(nf["error"]["message"], "no such record");
    assert_eq!(
        send_boot(
            &e.app,
            crate::common::get(
                &crate::common::j("/demo/records/..%2Fsecrets"),
                Some(&viewer)
            )
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send_boot(
            &e.app,
            crate::common::get(
                &crate::common::j("/demo/records/orders/a..b"),
                Some(&viewer)
            )
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send_boot(
            &e.app,
            crate::common::json(
                "PUT",
                &crate::common::j("/demo/records/orders/ORD-58790"),
                Some(&viewer),
                json!({"status":"lost"})
            )
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, p) = send_boot(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/demo/records/orders/ORD-58790"),
            Some(&admin),
            json!({"status":"lost"}),
        ),
    )
    .await;
    assert_eq!(p["status"], "lost");
    assert_eq!(
        send_boot(
            &e.app,
            crate::common::get(&crate::common::j("/demo/scenarios"), Some(&viewer))
        )
        .await
        .1["scenarios"][0]["name"],
        "duplicate-charge"
    );

    let (_, r1) = send_boot(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/demo/reset"),
            Some(&admin),
            json!({"seed": 42}),
        ),
    )
    .await;
    assert_eq!(
        (r1["customers"].clone(), r1["seed_seen"].clone()),
        (json!(501), json!(42)),
        "body passed through"
    );
    let (s, r2) = send_boot(
        &e.app,
        body_less_post(&crate::common::j("/demo/reset"), &admin),
    )
    .await;
    assert_eq!(
        (s, r2["customers"].clone(), r2["seed_seen"].clone()),
        (StatusCode::OK, json!(501), Value::Null),
        "body-less reset accepted"
    );
    assert_eq!(
        send_boot(
            &e.app,
            body_less_post(&crate::common::j("/demo/reset"), &viewer)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    assert_eq!(
        send_boot(
            &e.app,
            crate::common::get(&crate::common::j("/demo/faults"), Some(&viewer))
        )
        .await
        .1["faults"],
        json!({})
    );
    let (_, f) = send_boot(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/demo/faults"),
            Some(&admin),
            json!({"tool":"delivery_track_shipment","mode":"timeout","count":1}),
        ),
    )
    .await;
    assert_eq!(
        f["faults"]["delivery_track_shipment"],
        json!({"mode":"timeout","remaining":1})
    );

    let actions: Vec<String> = sqlx::query_scalar("SELECT action_type FROM audit_events WHERE resource_type='demo' AND actor_type='human' ORDER BY id")
        .fetch_all(crate::common::pool(&e)).await.unwrap();
    assert_eq!(
        actions,
        [
            "apply_scenario",
            "update_record",
            "reset",
            "reset",
            "set_faults"
        ],
        "every write audited, no read, no refused write"
    );
}

#[tokio::test]
async fn demo_unreachable_or_unconfigured_is_503_naming_demo_backends() {
    let admin = crate::common::super_admin();
    // a closed port: connection refused
    let e = crate::common::env_with("demo-down", |c| {
        c.demo_backends_url = Some("http://127.0.0.1:9".into())
    })
    .await;
    let (s, b) = send_boot(
        &e.app,
        crate::common::get(&crate::common::j("/demo/scenarios"), Some(&admin)),
    )
    .await;
    assert_eq!(
        (s, b["error"]["message"].clone()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!("demo-backends is unavailable")
        )
    );
    let e = crate::common::env("demo-unset").await;
    let (s, b) = send_boot(
        &e.app,
        body_less_post(&crate::common::j("/demo/reset"), &admin),
    )
    .await;
    assert_eq!(
        (s, b["error"]["message"].clone()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!("demo-backends is unavailable")
        )
    );
}

#[tokio::test]
async fn demo_write_with_unavailable_upstream_is_still_audited_as_failure() {
    let admin = crate::common::super_admin();
    let e = crate::common::env_with("demo-audit-down", |c| {
        c.demo_backends_url = Some("http://127.0.0.1:9".into())
    })
    .await;
    let (s, _) = send_boot(
        &e.app,
        body_less_post(&crate::common::j("/demo/reset"), &admin),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT action_type, status, details FROM audit_events WHERE resource_type='demo'",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str()),
        ("reset", "failure")
    );
    assert_eq!(rows[0].2["upstream"], "unavailable");

    // record PUT, faults PUT and scenario POST too
    let put = crate::common::json(
        "PUT",
        &crate::common::j("/demo/records/orders/ORD-58790"),
        Some(&admin),
        json!({"status":"lost"}),
    );
    assert_eq!(
        send_boot(&e.app, put).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let f = crate::common::json(
        "PUT",
        &crate::common::j("/demo/faults"),
        Some(&admin),
        json!({"tool":"t","mode":"timeout","count":1}),
    );
    assert_eq!(
        send_boot(&e.app, f).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let sc = crate::common::json(
        "POST",
        &crate::common::j("/demo/scenarios/late-parcel"),
        Some(&admin),
        json!({}),
    );
    assert_eq!(
        send_boot(&e.app, sc).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT action_type, resource_id, status FROM audit_events WHERE resource_type='demo' ORDER BY id")
        .fetch_all(crate::common::pool(&e)).await.unwrap();
    assert_eq!(
        rows,
        [
            (
                "reset".to_string(),
                "all".to_string(),
                "failure".to_string()
            ),
            (
                "update_record".to_string(),
                "orders/ORD-58790".to_string(),
                "failure".to_string()
            ),
            (
                "set_faults".to_string(),
                "faults".to_string(),
                "failure".to_string()
            ),
            (
                "apply_scenario".to_string(),
                "late-parcel".to_string(),
                "failure".to_string()
            ),
        ]
    );
}

#[tokio::test]
async fn demo_upstream_401_and_5xx_are_503_and_non_json_4xx_gets_an_envelope() {
    let url = serve(
        Router::new()
            .route(
                "/api/admin/scenarios",
                get(|| async { StatusCode::UNAUTHORIZED }),
            )
            .route(
                "/api/admin/faults",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .route(
                "/api/admin/records/{table}",
                get(|| async { (StatusCode::BAD_REQUEST, "plain text") }),
            ),
    )
    .await;
    let e =
        crate::common::env_with("demo-statuses", |c| c.demo_backends_url = Some(url.clone())).await;
    let t = crate::common::user();
    for uri in [
        crate::common::j("/demo/scenarios"),
        crate::common::j("/demo/faults"),
    ] {
        let (s, b) = send_boot(&e.app, crate::common::get(&uri, Some(&t))).await;
        assert_eq!(
            (s, b["error"]["message"].clone()),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!("demo-backends is unavailable")
            ),
            "{uri}"
        );
    }
    let (s, b) = send_boot(
        &e.app,
        crate::common::get(&crate::common::j("/demo/records/orders"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "upstream_rejected");
    assert_eq!(
        b["error"]["message"],
        "demo-backends rejected the request (HTTP 400)."
    );
}
