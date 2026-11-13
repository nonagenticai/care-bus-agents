//! `/api/providers`: the tenant's LLM providers and the `qualify` / `gates` bindings (spec 2026-10-18 §2, D38).
//! Tokens are write-only; read = any tenant member, write = tenant_admin; rows are isolated per tenant by RLS; the
//! chat qualifier resolves the tenant's `qualify` binding (crate::chat_q::qualifier_for).

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use carebus_api::chat_q::{AnyQualifier, qualifier_for};
use serde_json::{Value, json};

use crate::common::{admin, user};

const TOKEN: &str = "tok-sekrit-4f1d9a";

fn input(name: &str, kind: &str, base_url: &str, token: Option<&str>) -> Value {
    let mut v = json!({"name": name, "kind": kind, "base_url": base_url});
    if let Some(t) = token {
        v["token"] = json!(t);
    }
    v
}

async fn create(e: &crate::common::TestEnv, body: Value) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json("POST", "/api/providers", Some(&admin()), body),
    )
    .await
}

async fn bind(
    e: &crate::common::TestEnv,
    use_name: &str,
    provider: Value,
    model: Value,
) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/uses/{use_name}"),
            Some(&admin()),
            json!({"provider_id": provider, "model": model}),
        ),
    )
    .await
}

async fn list(e: &crate::common::TestEnv, token: &str) -> (StatusCode, Value) {
    crate::common::send(&e.app, crate::common::get("/api/providers", Some(token))).await
}

/// Every audit row and stored ciphertext of the test database, as text.
async fn stored_text(e: &crate::common::TestEnv) -> String {
    let audit: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(a) FROM audit_events a")
        .fetch_all(crate::common::pool(e))
        .await
        .unwrap();
    let enc: Vec<Option<String>> = sqlx::query_scalar("SELECT secret_enc FROM llm_providers")
        .fetch_all(crate::common::pool(e))
        .await
        .unwrap();
    format!("{audit:?} {enc:?}")
}

#[tokio::test]
async fn crud_round_trip_never_returns_the_token() {
    let e = crate::common::env("llm-crud").await;
    let (s, empty) = list(&e, &admin()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(empty["providers"], json!([]));
    assert_eq!(
        empty["uses"],
        json!([{"use": "qualify", "provider_id": null, "model": null},
               {"use": "gates", "provider_id": null, "model": null},
               {"use": "research", "provider_id": null, "model": null},
               {"use": "research_write", "provider_id": null, "model": null}])
    );
    // tests never run the platform LLM
    assert_eq!(
        empty["platform_default"],
        json!({"base_url": null, "model": null, "configured": false})
    );

    let (s, p) = create(
        &e,
        input(
            " Local vLLM ",
            "openai",
            "https://llm.example.test/v1/",
            Some(TOKEN),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!(p["name"], "Local vLLM");
    assert_eq!(p["kind"], "openai");
    assert_eq!(p["base_url"], "https://llm.example.test/v1");
    assert_eq!(p["has_token"], true);
    assert_eq!(p["updated_by"], "u-admin");
    let mut keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "base_url",
            "has_token",
            "id",
            "kind",
            "name",
            "updated_at",
            "updated_by"
        ]
    );
    assert!(!p.to_string().contains(TOKEN));
    let id = p["id"].as_str().unwrap().to_string();

    let (_, all) = list(&e, &user()).await;
    assert_eq!(all["providers"].as_array().unwrap().len(), 1);
    assert!(!all.to_string().contains(TOKEN));

    // token absent = keep
    let (s, kept) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/{id}"),
            Some(&admin()),
            input("Renamed", "openai", "https://llm.example.test/v1", None),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{kept}");
    assert_eq!(kept["name"], "Renamed");
    assert_eq!(kept["base_url"], "https://llm.example.test/v1");
    assert_eq!(kept["has_token"], true);
    // ...but never to a new host: the stored token would be sent there
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/{id}"),
            Some(&admin()),
            input(
                "Renamed",
                "openai",
                "https://attacker.example.test/v1",
                None,
            ),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "token_required_for_new_base_url");
    let (_, still) = list(&e, &admin()).await;
    assert_eq!(
        still["providers"][0]["base_url"],
        "https://llm.example.test/v1"
    );
    // a new token replaces it, still never echoed
    let (s, replaced) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/{id}"),
            Some(&admin()),
            input(
                "Renamed",
                "openai",
                "https://llm2.example.test/v1",
                Some("tok-second-77a"),
            ),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(!replaced.to_string().contains("tok-second-77a"));
    // "" = clear
    let (s, cleared) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/{id}"),
            Some(&admin()),
            input(
                "Renamed",
                "openai",
                "https://llm2.example.test/v1",
                Some(""),
            ),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cleared["has_token"], false);

    // neither token is in an audit row or stored in clear
    let stored = stored_text(&e).await;
    assert!(!stored.contains(TOKEN) && !stored.contains("tok-second-77a"));
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action_type FROM audit_events WHERE resource_type = 'llm_provider' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(actions, ["create", "update", "update", "update"]);

    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "DELETE",
            &format!("/api/providers/{id}"),
            Some(&admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "DELETE",
            &format!("/api/providers/{id}"),
            Some(&admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (_, after) = list(&e, &admin()).await;
    assert_eq!(after["providers"], json!([]));
}

#[tokio::test]
async fn the_stored_token_is_sealed_and_opens_only_with_the_source_key() {
    let e = crate::common::env("llm-sealed").await;
    let (_, p) = create(
        &e,
        input("A", "openai", "https://a.example.test/v1", Some(TOKEN)),
    )
    .await;
    let id: uuid::Uuid = p["id"].as_str().unwrap().parse().unwrap();
    let enc: String = sqlx::query_scalar("SELECT secret_enc FROM llm_providers WHERE id = $1")
        .bind(id)
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert!(!enc.contains(TOKEN));
    let key = crate::common::source_key();
    assert_eq!(
        carebus_api::routes::llm_providers::open_token(&key, id, &enc).unwrap(),
        TOKEN
    );
    // bound to the provider id: the same ciphertext on another row does not open
    assert!(
        carebus_api::routes::llm_providers::open_token(&key, uuid::Uuid::new_v4(), &enc).is_err()
    );
}

#[tokio::test]
async fn input_is_validated() {
    let e = crate::common::env("llm-validate").await;
    for (body, code) in [
        (
            input("A", "anthropic", "https://a.example.test", None),
            "invalid_kind",
        ),
        (
            input("", "openai", "https://a.example.test", None),
            "invalid_name",
        ),
        (
            input("A", "openai", "ftp://a.example.test", None),
            "invalid_base_url",
        ),
        (
            input("A", "openai", "https://u:p@a.example.test/v1", None),
            "invalid_base_url",
        ),
        (
            input("A", "openai", "https://a.example.test/v1?key=x", None),
            "invalid_base_url",
        ),
        (
            input("A", "openai", "https://a.example.test/v1#x", None),
            "invalid_base_url",
        ),
    ] {
        let (s, b) = create(&e, body.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
        assert_eq!(b["error"]["code"], code, "{body}: {b}");
    }
    // only public hosts (422 base_url_not_allowed), on create and on update
    for url in [
        "http://127.0.0.1:8001/v1",
        "http://10.1.2.3/v1",
        "http://100.64.0.7/v1",
        "http://169.254.169.254/v1",
        "http://[fd00::1]/v1",
        "http://localhost:8001/v1",
        "http://vllm.internal:8001/v1",
        "http://llm.ns.svc.cluster.local/v1",
        "http://llm.svc/v1",
        "http://box.local/v1",
    ] {
        let (s, b) = create(&e, input("A", "openai", url, None)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {b}");
        assert_eq!(b["error"]["code"], "base_url_not_allowed", "{url}: {b}");
    }
    let (_, ok) = create(
        &e,
        input("Public", "openai", "https://a.example.test/v1", None),
    )
    .await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/{}", ok["id"].as_str().unwrap()),
            Some(&admin()),
            input("Public", "openai", "http://vllm.internal:8001/v1", None),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "base_url_not_allowed");
    // unknown fields are refused, so a misspelt `token` can never be stored somewhere else
    let (s, _) = create(
        &e,
        json!({"name": "A", "kind": "openai", "base_url": "https://a.example.test", "api_key": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, b) = bind(&e, "routing", Value::Null, Value::Null).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "unknown_use");
}

#[tokio::test]
async fn each_use_takes_only_its_kind_and_a_bound_provider_cannot_be_deleted() {
    let e = crate::common::env("llm-uses").await;
    let (_, openai) = create(
        &e,
        input("vLLM", "openai", "https://v.example.test/v1", None),
    )
    .await;
    let (_, laya) = create(
        &e,
        input("Laya", "laya", "https://l.example.test/laya", None),
    )
    .await;
    let (oid, lid) = (openai["id"].clone(), laya["id"].clone());

    let (s, b) = bind(&e, "gates", oid.clone(), Value::Null).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "provider_kind_mismatch");
    let (s, b) = bind(&e, "qualify", lid.clone(), Value::Null).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "provider_kind_mismatch");
    let (s, b) = bind(&e, "qualify", json!(uuid::Uuid::new_v4()), Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");

    let (s, b) = bind(&e, "qualify", oid.clone(), json!(" local-llm ")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"use": "qualify", "provider_id": oid, "model": "local-llm"})
    );
    let (s, b) = bind(&e, "gates", lid.clone(), json!("jev-latest")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, all) = list(&e, &user()).await;
    assert_eq!(
        all["uses"],
        json!([{"use": "qualify", "provider_id": oid, "model": "local-llm"},
               {"use": "gates", "provider_id": lid, "model": "jev-latest"},
               {"use": "research", "provider_id": null, "model": null},
               {"use": "research_write", "provider_id": null, "model": null}])
    );

    // bound: no delete, no kind change
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "DELETE",
            &format!("/api/providers/{}", oid.as_str().unwrap()),
            Some(&admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "provider_in_use");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/{}", oid.as_str().unwrap()),
            Some(&admin()),
            input("vLLM", "laya", "https://v.example.test/v1", None),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "provider_in_use");

    // unbound: delete works
    let (s, b) = bind(&e, "qualify", Value::Null, Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        b,
        json!({"use": "qualify", "provider_id": null, "model": null})
    );
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "DELETE",
            &format!("/api/providers/{}", oid.as_str().unwrap()),
            Some(&admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_tenant_user_reads_but_cannot_write() {
    let e = crate::common::env("llm-viewer").await;
    let (_, p) = create(&e, input("A", "openai", "https://a.example.test/v1", None)).await;
    let id = p["id"].as_str().unwrap().to_string();
    let viewer = user();
    assert_eq!(list(&e, &viewer).await.0, StatusCode::OK);
    for (method, path, body) in [
        (
            "POST",
            "/api/providers".to_string(),
            input("B", "openai", "https://b.example.test", None),
        ),
        (
            "PUT",
            format!("/api/providers/{id}"),
            input("B", "openai", "https://b.example.test", None),
        ),
        ("DELETE", format!("/api/providers/{id}"), json!({})),
        ("POST", format!("/api/providers/{id}/test"), json!({})),
        (
            "POST",
            "/api/providers/test".to_string(),
            json!({"kind": "openai", "base_url": "https://b.example.test", "token": "t"}),
        ),
        (
            "PUT",
            "/api/providers/uses/qualify".to_string(),
            json!({"provider_id": id, "model": null}),
        ),
        (
            "PUT",
            "/api/providers/settings".to_string(),
            json!({"external_llm_allowed": true}),
        ),
    ] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(method, &path, Some(&viewer), body),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {path}: {b}");
    }
    assert_eq!(
        crate::common::send(&e.app, crate::common::get("/api/providers", None))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn tenants_never_see_or_touch_each_others_providers() {
    let e = crate::common::env("llm-rls").await;
    let owner = crate::common::pool(&e);
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-acme', $1, 'tenant_admin', 'Acme admin', 'test')",
    )
    .bind(other)
    .execute(owner)
    .await
    .unwrap();
    let acme = crate::common::token_for("u-acme", &[]);

    let (_, boot) = create(
        &e,
        input(
            "Boot",
            "openai",
            "https://boot.example.test/v1",
            Some(TOKEN),
        ),
    )
    .await;
    let boot_id = boot["id"].as_str().unwrap().to_string();
    let (s, b) = bind(&e, "qualify", json!(boot_id), Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // OTHER sees nothing of the bootstrap tenant, not even its binding
    let (s, theirs) = list(&e, &acme).await;
    assert_eq!(s, StatusCode::OK, "{theirs}");
    assert_eq!(theirs["providers"], json!([]));
    assert_eq!(theirs["uses"][0]["provider_id"], Value::Null);
    // and reaches none of its rows by id
    for (method, path, body) in [
        (
            "PUT",
            format!("/api/providers/{boot_id}"),
            input("x", "openai", "https://x.example.test", None),
        ),
        ("DELETE", format!("/api/providers/{boot_id}"), json!({})),
        ("POST", format!("/api/providers/{boot_id}/test"), json!({})),
        (
            "PUT",
            "/api/providers/uses/qualify".to_string(),
            json!({"provider_id": boot_id, "model": null}),
        ),
    ] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(method, &path, Some(&acme), body),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{method} {path}: {b}");
    }
    // OTHER's own provider, same name, lives beside it
    let (s, mine) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/providers",
            Some(&acme),
            input("Boot", "openai", "https://acme.example.test/v1", None),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{mine}");
    let (_, boot_view) = list(&e, &admin()).await;
    let names: Vec<&str> = boot_view["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["base_url"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["https://boot.example.test/v1"]);
    // the runtime role itself sees only the tenant in its GUC
    let mut tx = carebus_api::tenancy::begin_scoped(crate::common::app_pool(&e), other, None)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_providers")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/// A provider's `/models`: records the Authorization header it was called with.
async fn models_stub(path: &'static str, body: Value) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().route(
        path,
        get(move |h: HeaderMap| {
            let (s, body) = (s.clone(), body.clone());
            async move {
                let auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                s.lock().unwrap().push(auth);
                axum::Json(body)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

#[tokio::test]
async fn test_probes_models_with_the_token_and_returns_names_only() {
    // the stubs listen on 127.0.0.1: the test-only loopback allowance
    let e = crate::common::env_with("llm-test", |c| c.source_fetch_allow_loopback = true).await;
    let (openai_url, openai_seen) = models_stub(
        "/v1/models",
        json!({"object": "list", "data": [{"id": "local-llm"}, {"id": "other"}]}),
    )
    .await;
    let (_, p) = create(
        &e,
        input("vLLM", "openai", &format!("{openai_url}/v1"), Some(TOKEN)),
    )
    .await;
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &format!("/api/providers/{}/test", p["id"].as_str().unwrap()),
            Some(&admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["ok"], true);
    assert_eq!(r["status"], 200);
    assert_eq!(r["models"], json!(["local-llm", "other"]));
    assert!(r["latency_ms"].is_u64());
    assert!(!r.to_string().contains(TOKEN));
    assert_eq!(*openai_seen.lock().unwrap(), [format!("Bearer {TOKEN}")]);
    crate::common::fixture("providers/llm_provider_test", &r);

    // laya: {base_url}/v1/models, no token stored = no Authorization header
    let (laya_url, laya_seen) =
        models_stub("/laya/v1/models", json!({"data": [{"id": "jev-latest"}]})).await;
    let (_, l) = create(&e, input("Laya", "laya", &format!("{laya_url}/laya"), None)).await;
    let (_, r) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &format!("/api/providers/{}/test", l["id"].as_str().unwrap()),
            Some(&admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["models"], json!(["jev-latest"]));
    assert_eq!(*laya_seen.lock().unwrap(), [String::new()]);

    // nothing listening: ok false, no status, no error text
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}/v1")
    };
    let (_, d) = create(&e, input("Dead", "openai", &dead, None)).await;
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &format!("/api/providers/{}/test", d["id"].as_str().unwrap()),
            Some(&admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["ok"], false);
    assert_eq!(r["status"], Value::Null);
    assert_eq!(r["models"], json!([]));
    assert!(!stored_text(&e).await.contains(TOKEN));
}

async fn test_unsaved(e: &crate::common::TestEnv, token: &str, body: Value) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json("POST", "/api/providers/test", Some(token), body),
    )
    .await
}

/// The add/edit form's Test (`POST /api/providers/test`): probes a provider as typed and stores nothing; a stored
/// token is used only for its own base_url.
#[tokio::test]
async fn an_unsaved_provider_is_tested_without_storing_it() {
    let e =
        crate::common::env_with("llm-test-unsaved", |c| c.source_fetch_allow_loopback = true).await;
    let (url, seen) = models_stub(
        "/v1/models",
        json!({"object": "list", "data": [{"id": "local-llm"}]}),
    )
    .await;
    let base_url = format!("{url}/v1");

    // a typed token reaches the stub; the response has the stored test's shape; nothing is stored
    let (s, r) = test_unsaved(
        &e,
        &admin(),
        json!({"kind": "openai", "base_url": format!("{base_url}/"), "token": TOKEN}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["ok"], true);
    assert_eq!(r["status"], 200);
    assert_eq!(r["models"], json!(["local-llm"]));
    assert!(r["latency_ms"].is_u64());
    // the stored test's shape: {ok, status, latency_ms, models} and nothing else
    assert_eq!(r.as_object().unwrap().len(), 4, "{r}");
    assert!(!r.to_string().contains(TOKEN));
    assert_eq!(*seen.lock().unwrap(), [format!("Bearer {TOKEN}")]);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_providers")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(stored, 0);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action_type = 'test' AND resource_id = 'unsaved'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // no token typed, no provider: no Authorization header
    let (s, r) = test_unsaved(
        &e,
        &admin(),
        json!({"kind": "openai", "base_url": base_url}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(seen.lock().unwrap().last().unwrap(), "");

    // editing a stored provider: same base_url, no token typed => its stored token
    let (_, p) = create(&e, input("vLLM", "openai", &base_url, Some(TOKEN))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let (s, r) = test_unsaved(
        &e,
        &admin(),
        json!({"kind": "openai", "base_url": base_url, "token": "", "provider_id": id}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["models"], json!(["local-llm"]));
    assert_eq!(
        seen.lock().unwrap().last().unwrap(),
        &format!("Bearer {TOKEN}")
    );

    // another base_url without a typed token: the stored token never goes to a new host
    let calls = seen.lock().unwrap().len();
    let (other, other_seen) = models_stub("/v1/models", json!({"data": []})).await;
    let (s, b) = test_unsaved(
        &e,
        &admin(),
        json!({"kind": "openai", "base_url": format!("{other}/v1"), "provider_id": id}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "token_required_for_new_base_url");
    assert!(other_seen.lock().unwrap().is_empty());
    assert_eq!(seen.lock().unwrap().len(), calls);
    // ... while a typed token for it is fine
    let (s, r) = test_unsaved(
        &e,
        &admin(),
        json!({"kind": "openai", "base_url": format!("{other}/v1"), "token": "fresh", "provider_id": id}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(*other_seen.lock().unwrap(), ["Bearer fresh".to_string()]);

    // the same input checks as a save
    let (s, b) = test_unsaved(
        &e,
        &admin(),
        json!({"kind": "openai", "base_url": "https://u:p@h.example.test/v1"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "invalid_base_url");
    let (s, b) = test_unsaved(&e, &admin(), json!({"kind": "gpt", "base_url": base_url})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "invalid_kind");
    let (s, b) = test_unsaved(
        &e,
        &admin(),
        json!({"kind": "openai", "base_url": "http://10.0.0.5/v1", "token": "t"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "base_url_not_allowed");
    assert!(!stored_text(&e).await.contains(TOKEN));

    // a tenant_user may not test
    let (s, _) = test_unsaved(
        &e,
        &user(),
        json!({"kind": "openai", "base_url": base_url, "token": TOKEN}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_qualifier_follows_the_tenants_qualify_binding() {
    // the qualify client resolves and pins its host when it is built: loopback literals (the test-only allowance),
    // since a test name does not resolve
    let e =
        crate::common::env_with("llm-qualifier", |c| c.source_fetch_allow_loopback = true).await;
    let tenant: uuid::Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    // unbound: the platform default (keyword rules in tests)
    let q = qualifier_for(&e.state, tenant).await.unwrap();
    assert!(Arc::ptr_eq(&q, &e.state.qualifier));

    let (_, p) = create(
        &e,
        input("vLLM", "openai", "http://127.0.0.1:9/v1", Some(TOKEN)),
    )
    .await;
    let id = p["id"].clone();
    // a gates binding does not change the qualifier
    let (_, l) = create(
        &e,
        input("Laya", "laya", "https://l.example.test/laya", None),
    )
    .await;
    bind(&e, "gates", l["id"].clone(), Value::Null).await;
    assert!(Arc::ptr_eq(
        &qualifier_for(&e.state, tenant).await.unwrap(),
        &e.state.qualifier
    ));

    let (s, _) = bind(&e, "qualify", id.clone(), json!("local-llm")).await;
    assert_eq!(s, StatusCode::OK);
    let bound = qualifier_for(&e.state, tenant).await.unwrap();
    assert!(matches!(*bound, AnyQualifier::Rig(_)));
    // cached while nothing changed
    assert!(Arc::ptr_eq(
        &bound,
        &qualifier_for(&e.state, tenant).await.unwrap()
    ));
    // a provider update (new updated_at) rebuilds it
    crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/{}", id.as_str().unwrap()),
            Some(&admin()),
            input("vLLM", "openai", "http://127.0.0.1:10/v1", Some(TOKEN)),
        ),
    )
    .await;
    let rebuilt = qualifier_for(&e.state, tenant).await.unwrap();
    assert!(matches!(*rebuilt, AnyQualifier::Rig(_)));
    assert!(!Arc::ptr_eq(&bound, &rebuilt));
    // so does a model change on the binding
    bind(&e, "qualify", id.clone(), json!("other-model")).await;
    let remodelled = qualifier_for(&e.state, tenant).await.unwrap();
    assert!(!Arc::ptr_eq(&rebuilt, &remodelled));
    // a stored base_url that is not callable (written past the API guard): no client is built for it
    sqlx::query(
        "UPDATE llm_providers SET base_url = 'http://10.0.0.5/v1', updated_at = now() WHERE id = $1",
    )
    .bind(id.as_str().unwrap().parse::<uuid::Uuid>().unwrap())
    .execute(crate::common::pool(&e))
    .await
    .unwrap();
    assert!(qualifier_for(&e.state, tenant).await.is_err());
    // unbound again: back to the platform default
    bind(&e, "qualify", Value::Null, Value::Null).await;
    assert!(Arc::ptr_eq(
        &qualifier_for(&e.state, tenant).await.unwrap(),
        &e.state.qualifier
    ));
}

/// `GET /api/providers` as the web reads it (web/src/lib/api/fixtures/providers/llm_providers.json): a provider with
/// and one without a token, `qualify` bound with a model, and a configured platform default.
#[tokio::test]
async fn the_providers_list_matches_the_web_fixture() {
    let e = crate::common::env("llm-fixture").await;
    let (_, p) = create(
        &e,
        input(
            "Local vLLM",
            "openai",
            "https://llm.example.test/v1",
            Some(TOKEN),
        ),
    )
    .await;
    create(
        &e,
        input("Laya EU", "laya", "https://laya.example.test", None),
    )
    .await;
    let (s, _) = bind(&e, "qualify", p["id"].clone(), json!("local-llm")).await;
    assert_eq!(s, StatusCode::OK);
    let mut st = e.state.clone();
    st.platform_llm = Arc::new(carebus_api::chat_q::PlatformLlm {
        endpoint: Some(("http://vllm.example:8001/v1".into(), "local-llm".into())),
    });
    let app = carebus_api::router(st);
    let (s, body) =
        crate::common::send(&app, crate::common::get("/api/providers", Some(&admin()))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    // the bound use first, so the shape check sees a bound provider_id and model
    assert_eq!(body["uses"][0]["use"], "qualify");
    assert!(!body.to_string().contains(TOKEN));
    crate::common::fixture("providers/llm_providers", &body);
}

async fn set_external(e: &crate::common::TestEnv, token: &str, body: Value) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json("PUT", "/api/providers/settings", Some(token), body),
    )
    .await
}

/// 0023: `external_llm_allowed` defaults to false, only a tenant_admin sets it, it is audited, and it is the
/// tenant's own (research runs only; the researcher reads it, see tests/researcher.rs).
#[tokio::test]
async fn external_llm_allowed_defaults_off_and_is_tenant_admin_only() {
    let e = crate::common::env("llm-external").await;
    let (s, b) = list(&e, &admin()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["external_llm_allowed"], false);
    let (s, b) = set_external(&e, &user(), json!({"external_llm_allowed": true})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    for bad in [
        json!({}),
        json!({"external_llm_allowed": "yes"}),
        json!({"external_llm_allowed": true, "x": 1}),
    ] {
        let (s, b) = set_external(&e, &admin(), bad.clone()).await;
        assert!(s.is_client_error(), "{bad}: {s} {b}");
    }
    let (s, b) = set_external(&e, &admin(), json!({"external_llm_allowed": true})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"external_llm_allowed": true}));
    assert_eq!(list(&e, &user()).await.1["external_llm_allowed"], true);
    let (s, b) = set_external(&e, &admin(), json!({"external_llm_allowed": false})).await;
    assert_eq!(
        (s, b),
        (StatusCode::OK, json!({"external_llm_allowed": false}))
    );
    assert_eq!(list(&e, &admin()).await.1["external_llm_allowed"], false);
    let audits: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE resource_type = 'tenant_llm_settings' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        audits,
        [
            json!({"external_llm_allowed": true}),
            json!({"external_llm_allowed": false})
        ]
    );

    // another tenant has its own (default) setting and cannot change ours
    let owner = crate::common::pool(&e);
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-acme', $1, 'tenant_admin', 'Acme admin', 'test')",
    )
    .bind(other)
    .execute(owner)
    .await
    .unwrap();
    let acme = crate::common::token_for("u-acme", &[]);
    let (s, _) = set_external(&e, &acme, json!({"external_llm_allowed": true})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list(&e, &acme).await.1["external_llm_allowed"], true);
    assert_eq!(list(&e, &admin()).await.1["external_llm_allowed"], false);
}

/// One request a [`chat_stub`] got: its `X-Effective-Tenant-Id` and `Authorization` (`None` = absent).
type Sent = (Option<String>, Option<String>);

/// A chat-completions model on 127.0.0.1 that records `X-Effective-Tenant-Id` and `Authorization` of every request
/// and answers 500: only what reached the model matters here, not its answer.
async fn chat_stub() -> (String, Arc<Mutex<Vec<Sent>>>) {
    let seen: Arc<Mutex<Vec<Sent>>> = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().fallback(move |h: HeaderMap| {
        let s = s.clone();
        async move {
            let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
            s.lock().unwrap().push((
                hv(carebus::interaction::EFFECTIVE_TENANT_HEADER),
                hv("authorization"),
            ));
            (StatusCode::INTERNAL_SERVER_ERROR, "stub")
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

fn sent(seen: &Arc<Mutex<Vec<Sent>>>) -> Vec<Sent> {
    std::mem::take(&mut *seen.lock().unwrap())
}

/// One chat turn in a new conversation of the bootstrap tenant, through a router over `state`.
async fn chat_turn(state: carebus_api::state::AppState) -> StatusCode {
    let app = carebus_api::router(state);
    let t = user();
    let (s, c) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j("/conversations"),
            Some(&t),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let id = c["conversation_id"].as_str().unwrap().to_string();
    let (s, _) = crate::common::send(
        &app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/conversations/{id}/messages")),
            Some(&t),
            json!({"text": "I want to return my headphones"}),
        ),
    )
    .await;
    s
}

/// On the wire, through `POST /conversations/{id}/messages` with the platform `RigQualifier` (`RigQualifier::new`)
/// pointed at a stub model: every model request of the turn carries `X-Effective-Tenant-Id` = the conversation's
/// tenant (never nil). Guards two lines: the `with_effective_tenant(cv.tenant, …)` wrap in
/// `routes::conversations::handle` (deleted: the task-local is unset, no header) and
/// `.with_middleware(EffectiveTenantHeader)` in `carebus::interaction::RigQualifier::new` (deleted: nothing sets the
/// header).
#[tokio::test]
async fn a_chat_turn_sends_the_conversations_tenant_to_the_platform_model() {
    let e = crate::common::env_with("llm-chat-eff-tenant", |c| {
        c.source_fetch_allow_loopback = true
    })
    .await;
    let tenant: uuid::Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    assert_ne!(tenant, uuid::Uuid::nil());
    let (url, seen) = chat_stub().await;
    let mut state = e.state.clone();
    state.qualifier = Arc::new(AnyQualifier::Rig(Box::new(
        carebus::interaction::RigQualifier::new("lic".into(), url, "local-llm".into()).unwrap(),
    )));
    // the stub answers 500: the turn fails (503); what reached the model is in the stub's record
    let s = chat_turn(state).await;
    assert_ne!(s, StatusCode::OK);
    let got = sent(&seen);
    assert!(!got.is_empty(), "the turn called the platform model");
    for (eff, auth) in &got {
        assert_eq!(eff.as_deref(), Some(tenant.to_string().as_str()), "{got:?}");
        assert_eq!(auth.as_deref(), Some("Bearer lic"), "{got:?}");
    }
}

/// On the wire: a tenant's own `qualify` provider (`chat_q::qualifier_for` → `RigQualifier::with_client`) gets no
/// `X-Effective-Tenant-Id`, even inside the turn's `with_effective_tenant` scope, and the platform model is not called.
/// Guards the BYO side of `RigQualifier::with_client` (no `EffectiveTenantHeader` middleware there): adding it, or
/// building the tenant client through `RigQualifier::new`, leaks the end tenant to a third party and fails this.
#[tokio::test]
async fn a_chat_turn_never_sends_the_effective_tenant_to_a_tenant_provider() {
    let e = crate::common::env_with("llm-chat-byo-no-tenant", |c| {
        c.source_fetch_allow_loopback = true
    })
    .await;
    let (platform_url, platform_seen) = chat_stub().await;
    let (byo_url, byo_seen) = chat_stub().await;
    let (s, p) = create(&e, input("Theirs", "openai", &byo_url, Some(TOKEN))).await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    let (s, b) = bind(&e, "qualify", p["id"].clone(), json!("their-model")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut state = e.state.clone();
    state.qualifier = Arc::new(AnyQualifier::Rig(Box::new(
        carebus::interaction::RigQualifier::new("lic".into(), platform_url, "local-llm".into())
            .unwrap(),
    )));
    let s = chat_turn(state).await;
    assert_ne!(s, StatusCode::OK);
    let got = sent(&byo_seen);
    assert!(!got.is_empty(), "the turn called the tenant's provider");
    let bearer = format!("Bearer {TOKEN}");
    for (eff, auth) in &got {
        assert_eq!(eff.as_deref(), None, "{got:?}");
        assert_eq!(auth.as_deref(), Some(bearer.as_str()), "{got:?}");
    }
    assert!(
        sent(&platform_seen).is_empty(),
        "the platform model is not called"
    );
}

/// On the wire, for research: the endpoint `resolve_endpoint` returns, sent by `OpenAiLlm::chat`, carries
/// `X-Effective-Tenant-Id` = the run's tenant to the platform model and never to a tenant's own model. Guards
/// `effective_tenant: Some(tenant)` (platform default) and `effective_tenant: is_platform_url(..).then_some(tenant)`
/// (bound provider) in `research::llm::resolve_endpoint`, and the header in `research::llm::chat_request`.
#[tokio::test]
async fn a_research_call_sends_the_tenant_only_to_the_platform_model() {
    use carebus_api::research::llm::{
        ChatRequest, DefaultLlm, Llm, OpenAiLlm, USE_RESEARCH, resolve_endpoint,
    };
    async fn call(e: &crate::common::TestEnv, tenant: uuid::Uuid, default: &DefaultLlm) {
        let ep = resolve_endpoint(
            &e.state.db,
            tenant,
            USE_RESEARCH,
            e.state.cfg.source_key.as_ref(),
            true,
            default,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        let req = ChatRequest {
            messages: vec![json!({"role": "user", "content": "q"})],
            tools: vec![],
            tool_choice: None,
        };
        // the stub answers 500
        assert!(OpenAiLlm.chat(&ep, &req).await.is_err());
    }
    let e = crate::common::env_with("llm-research-wire", |c| {
        c.source_fetch_allow_loopback = true
    })
    .await;
    let tenant: uuid::Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let (platform, platform_seen) = chat_stub().await;
    let (theirs, theirs_seen) = chat_stub().await;
    let default = DefaultLlm {
        base_url: platform.clone(),
        model: "local-llm".into(),
        token: Some("lic".into()),
    };
    let want = Some(tenant.to_string());

    // unbound: the platform default
    call(&e, tenant, &default).await;
    assert_eq!(
        sent(&platform_seen),
        vec![(want.clone(), Some("Bearer lic".into()))]
    );

    // a tenant provider at the platform's URL is still the platform model
    let (s, p) = create(&e, input("vLLM", "openai", &platform, Some(TOKEN))).await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    let id = p["id"].clone();
    let (s, b) = bind(&e, USE_RESEARCH, id.clone(), json!("local-llm")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    call(&e, tenant, &default).await;
    assert_eq!(
        sent(&platform_seen),
        vec![(want.clone(), Some(format!("Bearer {TOKEN}")))]
    );

    // the tenant's own model elsewhere: never the header
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/providers/{}", id.as_str().unwrap()),
            Some(&admin()),
            input("Theirs", "openai", &theirs, Some(TOKEN)),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    call(&e, tenant, &default).await;
    assert_eq!(
        sent(&theirs_seen),
        vec![(None, Some(format!("Bearer {TOKEN}")))]
    );
    assert!(sent(&platform_seen).is_empty());
}
