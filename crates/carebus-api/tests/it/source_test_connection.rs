use std::sync::atomic::Ordering;

use crate::common::fakes;
use axum::http::StatusCode;
use serde_json::{Value, json};

fn api(url: &str, auth: Value, secret: Option<&str>) -> Value {
    let mut b = json!({"name": format!("API {url} {}", auth["type"]), "type": "api", "config": {"url": url, "auth": auth},
                       "target_store_id": fakes::OTHER_LEAF});
    if let Some(s) = secret {
        b["credentials"] = json!({"secret": s});
    }
    b
}

async fn create(e: &crate::common::TestEnv, body: Value) -> String {
    let admin = crate::common::admin();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/sources"), Some(&admin), body),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    b["id"].as_str().unwrap().to_string()
}

async fn test(e: &crate::common::TestEnv, id: &str) -> (StatusCode, Value) {
    let admin = crate::common::admin();
    crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/sources/{id}/test")),
            Some(&admin),
            json!({}),
        ),
    )
    .await
}

#[tokio::test]
async fn test_connection_sends_the_credential_once_and_returns_a_sample() {
    let up = fakes::sample_api().await;
    let (e, _) =
        fakes::sources_env_with("src-test", |c| c.source_fetch_allow_loopback = true).await;
    let header = json!({"type": "header", "header_name": "X-Api-Key"});
    let id = create(
        &e,
        api(
            &format!("{}/customers", up.url),
            header.clone(),
            Some(fakes::API_KEY),
        ),
    )
    .await;
    let (s, b) = test(&e, &id).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["ok"].clone(), b["status"].clone()),
        (json!(true), json!(200))
    );
    assert_eq!(b["sample"]["customer_id"], "C-10482");
    assert_eq!(b["fields"], json!(["customer_id", "email", "name"]));
    assert_eq!(b["truncated"], false);
    assert_eq!(
        up.hits.load(Ordering::SeqCst),
        1,
        "exactly one outbound GET"
    );
    assert!(!crate::common::audit_text(&e).await.contains(fakes::API_KEY));

    let bearer = create(
        &e,
        api(
            &format!("{}/me", up.url),
            json!({"type": "bearer"}),
            Some(fakes::API_KEY),
        ),
    )
    .await;
    let (_, b) = test(&e, &bearer).await;
    assert_eq!(
        b["sample"],
        json!({"customer_id": "C-10482", "tier": "gold"})
    );

    let nokey = create(
        &e,
        api(
            &format!("{}/customers", up.url),
            json!({"type": "none"}),
            None,
        ),
    )
    .await;
    let (s, b) = test(&e, &nokey).await;
    assert_eq!(
        (s, b["ok"].clone(), b["status"].clone()),
        (StatusCode::OK, json!(false), json!(401))
    );

    let big = create(
        &e,
        api(&format!("{}/big", up.url), json!({"type": "none"}), None),
    )
    .await;
    let (_, b) = test(&e, &big).await;
    assert_eq!(b["truncated"], true, "capped at 256 KiB");
    assert_eq!(b["sample"].as_str().unwrap().len(), 2000);

    let moved = create(
        &e,
        api(&format!("{}/moved", up.url), json!({"type": "none"}), None),
    )
    .await;
    let (_, b) = test(&e, &moved).await;
    assert_eq!(
        (b["ok"].clone(), b["status"].clone()),
        (json!(false), json!(303)),
        "redirects are not followed"
    );

    let gone = create(
        &e,
        api("http://127.0.0.1:9/nothing", json!({"type": "none"}), None),
    )
    .await;
    let (s, b) = test(&e, &gone).await;
    assert_eq!(
        (s, b["ok"].clone(), b["detail"].clone()),
        (StatusCode::OK, json!(false), json!("unreachable"))
    );
    assert!(!b.to_string().contains("127.0.0.1"), "no URL echoed: {b}");
}

#[tokio::test]
async fn ssrf_targets_are_refused_at_save_and_again_at_test() {
    let up = fakes::sample_api().await;
    let (e, _) = fakes::sources_env("src-ssrf").await; // production rules: no loopback allowance
    let admin = crate::common::admin();
    for url in [
        format!("{}/customers", up.url),
        "http://10.0.0.1/x".into(),
        "http://169.254.169.254/latest/meta-data".into(),
        "http://100.64.0.1/".into(),
        "http://[::1]/".into(),
        "http://2130706433/".into(),
        "http://api.internal/x".into(),
        "http://carebus-api.some-ns.svc/x".into(),
        "http://localhost/x".into(),
        "ftp://example.com/x".into(),
        "http://user:pw@example.com/".into(),
    ] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                &crate::common::j("/sources"),
                Some(&admin),
                api(&url, json!({"type": "none"}), None),
            ),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::BAD_REQUEST, json!("invalid_url")),
            "{url}"
        );
        assert!(!b.to_string().contains("169.254"), "{b}");
    }
    // Rows that predate a rule (or were written by hand) are refused again before any connection is made.
    for url in [
        format!("{}/customers", up.url),
        "http://169.254.169.254/latest/meta-data".into(),
        "http://100.64.0.1/".into(),
        "http://api.internal/x".into(),
    ] {
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO sources (id, name, type, config, target_store_id, updated_by) VALUES ($1,$2,'api',$3,$4,'a')")
            .bind(id)
            .bind(format!("legacy {url}"))
            .bind(json!({"url": url, "auth": {"type": "none"}}))
            .bind(uuid::Uuid::parse_str(fakes::OTHER_LEAF).unwrap())
            .execute(crate::common::pool(&e))
            .await
            .unwrap();
        let (s, b) = test(&e, &id.to_string()).await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::BAD_REQUEST, json!("address_refused")),
            "{url}"
        );
    }
    assert_eq!(
        up.hits.load(Ordering::SeqCst),
        0,
        "the loopback target was never contacted"
    );
    let refused: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action_type='test' AND status='failure' AND details ? 'refused'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(refused, 4);
}

#[tokio::test]
async fn test_is_api_only_admin_only_and_fails_closed_on_bad_ciphertext() {
    let up = fakes::sample_api().await;
    let (e, _) =
        fakes::sources_env_with("src-test-guard", |c| c.source_fetch_allow_loopback = true).await;
    let web = {
        let admin = crate::common::admin();
        let (_, b) = crate::common::send(&e.app, crate::common::json("POST", &crate::common::j("/sources"), Some(&admin), json!({
            "name": "Help centre", "type": "website", "config": {"urls": ["https://help.example.nl/"]},
            "target_store_id": fakes::LEAF}))).await;
        b["id"].as_str().unwrap().to_string()
    };
    let (s, b) = test(&e, &web).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("test_not_supported"))
    );

    let id = create(
        &e,
        api(
            &format!("{}/customers", up.url),
            json!({"type": "bearer"}),
            Some("x-SECRETMARK"),
        ),
    )
    .await;
    let viewer = crate::common::user();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/sources/{id}/test")),
            Some(&viewer),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // ciphertext moved onto another row does not open there (the source id is the associated data)
    let other = create(
        &e,
        api(
            &format!("{}/me", up.url),
            json!({"type": "bearer"}),
            Some("y"),
        ),
    )
    .await;
    sqlx::query("UPDATE sources SET (credentials_enc, credentials_nonce) = (SELECT credentials_enc, credentials_nonce FROM sources WHERE id = $1::uuid) WHERE id = $2::uuid")
        .bind(&id)
        .bind(&other)
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    let (s, b) = test(&e, &other).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("credentials_unreadable"))
    );
    assert!(!b.to_string().contains("SECRETMARK"));
    assert_eq!(up.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_target_that_echoes_the_credential_never_returns_it() {
    let up = fakes::sample_api().await;
    let (e, _) =
        fakes::sources_env_with("src-test-echo", |c| c.source_fetch_allow_loopback = true).await;
    let mark = "x-SECRETMARK-0123456789";
    for (path, auth) in [
        ("echo", json!({"type": "bearer"})),
        (
            "echo",
            json!({"type": "header", "header_name": "X-Api-Key"}),
        ),
        ("echo-text", json!({"type": "bearer"})),
        (
            "echo-text",
            json!({"type": "header", "header_name": "X-Api-Key"}),
        ),
    ] {
        let id = create(&e, api(&format!("{}/{path}", up.url), auth, Some(mark))).await;
        let (s, b) = test(&e, &id).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let text = b.to_string();
        assert!(!text.contains("SECRETMARK"), "{path}: {text}");
        if path == "echo" {
            assert!(text.contains("[redacted]"), "{text}");
        }
    }
}

#[tokio::test]
async fn a_header_name_that_bypassed_save_is_refused_at_test() {
    let up = fakes::sample_api().await;
    let (e, _) =
        fakes::sources_env_with("src-test-hdr", |c| c.source_fetch_allow_loopback = true).await;
    for name in ["Host", "bad name", "Cookie"] {
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO sources (id, name, type, config, target_store_id, updated_by) VALUES ($1,$2,'api',$3,$4,'a')")
            .bind(id)
            .bind(format!("hdr {name}"))
            .bind(json!({"url": format!("{}/echo", up.url), "auth": {"type": "header", "header_name": name}}))
            .bind(uuid::Uuid::parse_str(fakes::OTHER_LEAF).unwrap())
            .execute(crate::common::pool(&e))
            .await
            .unwrap();
        let (s, b) = test(&e, &id.to_string()).await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::BAD_REQUEST, json!("invalid_config")),
            "{name}"
        );
    }
    assert_eq!(up.hits.load(Ordering::SeqCst), 0);
}

async fn post_as(
    e: &crate::common::TestEnv,
    role: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let tok = match role {
        "care-admin" => crate::common::admin(),
        "care-viewer" => crate::common::user(),
        _ => crate::common::token_for("u-unprovisioned", &[]),
    };
    crate::common::send(&e.app, crate::common::json("POST", path, Some(&tok), body)).await
}

fn unsaved(url: &str, auth: Value, secret: Option<&str>) -> Value {
    let mut b = json!({"type": "api", "config": {"url": url, "auth": auth}});
    if let Some(s) = secret {
        b["credentials"] = json!({"secret": s});
    }
    b
}

async fn source_count(e: &crate::common::TestEnv) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM sources")
        .fetch_one(crate::common::pool(e))
        .await
        .unwrap()
}

#[tokio::test]
async fn an_unsaved_api_source_is_tested_without_storing_anything() {
    let up = fakes::sample_api().await;
    let (e, _) =
        fakes::sources_env_with("src-unsaved", |c| c.source_fetch_allow_loopback = true).await;
    let header = json!({"type": "header", "header_name": "X-Api-Key"});
    let (s, b) = post_as(
        &e,
        "care-admin",
        &crate::common::j("/sources/test"),
        unsaved(
            &format!("{}/customers", up.url),
            header.clone(),
            Some(fakes::API_KEY),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["ok"].clone(), b["status"].clone()),
        (json!(true), json!(200))
    );
    assert_eq!(b["sample"]["customer_id"], "C-10482");
    assert_eq!(b["fields"], json!(["customer_id", "email", "name"]));
    assert_eq!(
        up.hits.load(Ordering::SeqCst),
        1,
        "exactly one outbound GET"
    );
    assert_eq!(source_count(&e).await, 0, "a test stores no source");

    // auth none, no secret: the upstream's own 401 is the result
    let (s, b) = post_as(
        &e,
        "care-admin",
        &crate::common::j("/sources/test"),
        unsaved(
            &format!("{}/customers", up.url),
            json!({"type": "none"}),
            None,
        ),
    )
    .await;
    assert_eq!(
        (s, b["ok"].clone(), b["status"].clone()),
        (StatusCode::OK, json!(false), json!(401))
    );

    // a target that echoes the credential never returns it, and the audit never holds it
    let mark = "x-SECRETMARK-unsaved-0123456789";
    for (path, auth) in [("echo", json!({"type": "bearer"})), ("echo-text", header)] {
        let (s, b) = post_as(
            &e,
            "care-admin",
            &crate::common::j("/sources/test"),
            unsaved(&format!("{}/{path}", up.url), auth, Some(mark)),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(!b.to_string().contains("SECRETMARK"), "{path}: {b}");
    }
    let audit = crate::common::audit_text(&e).await;
    assert!(!audit.contains("SECRETMARK") && !audit.contains(fakes::API_KEY));
    assert!(!audit.contains(&up.url), "no URL in the audit");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action_type='test' AND details->>'unsaved' = 'true'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(n, 4);
    assert_eq!(source_count(&e).await, 0);
}

#[tokio::test]
async fn an_unsaved_test_refuses_private_targets_and_bad_bodies() {
    let up = fakes::sample_api().await;
    let (e, _) = fakes::sources_env("src-unsaved-ssrf").await; // production rules: no loopback allowance
    for url in [
        format!("{}/customers", up.url),
        "http://169.254.169.254/latest/meta-data".into(),
        "http://10.0.0.1/x".into(),
        "http://api.internal/x".into(),
        "ftp://example.com/x".into(),
    ] {
        let (s, b) = post_as(
            &e,
            "care-admin",
            &crate::common::j("/sources/test"),
            unsaved(&url, json!({"type": "none"}), None),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::BAD_REQUEST, json!("address_refused")),
            "{url}"
        );
    }
    assert_eq!(up.hits.load(Ordering::SeqCst), 0, "nothing was contacted");

    let url = "https://api.example.nl/v2/customers";
    for (body, code) in [
        (
            unsaved(url, json!({"type": "magic"}), None),
            "invalid_config",
        ),
        (
            unsaved(
                url,
                json!({"type": "header", "header_name": "Host"}),
                Some("k"),
            ),
            "invalid_config",
        ),
        (
            json!({"type": "api", "config": {"auth": {"type": "none"}}}),
            "invalid_config",
        ),
        (
            json!({"type": "api", "config": {"url": url, "sheet": "x"}}),
            "invalid_config",
        ),
        (
            json!({"type": "api", "config": {"url": url, "auth": {"type": "bearer"}}, "credentials": {"secret": ""}}),
            "invalid_credentials",
        ),
        (
            unsaved(url, json!({"type": "none"}), Some("k")),
            "credentials_not_supported",
        ),
        (
            json!({"type": "website", "config": {"urls": [url]}}),
            "test_not_supported",
        ),
        (json!({"config": {"url": url}}), "invalid_type"),
        (
            json!({"type": "api", "config": {"url": url}, "name": "x"}),
            "invalid_body",
        ),
        (json!([1]), "invalid_body"),
    ] {
        let (s, b) = post_as(
            &e,
            "care-admin",
            &crate::common::j("/sources/test"),
            body.clone(),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::BAD_REQUEST, json!(code)),
            "{body}"
        );
    }
    let admin = crate::common::admin();
    let raw = axum::http::Request::builder()
        .method("POST")
        .uri(crate::common::j("/sources/test"))
        .header("authorization", format!("Bearer {admin}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{not json"))
        .unwrap();
    let (s, b) = crate::common::send(&e.app, raw).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_body"))
    );

    let (s, _) = post_as(
        &e,
        "care-viewer",
        &crate::common::j("/sources/test"),
        unsaved(url, json!({"type": "bearer"}), Some("x-SECRETMARK")),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(!crate::common::audit_text(&e).await.contains("SECRETMARK"));
    assert_eq!(source_count(&e).await, 0);
}

#[tokio::test]
async fn a_saved_source_is_tested_with_edits_and_keeps_its_secret_on_its_own_host() {
    let up = fakes::sample_api().await;
    let (e, _) =
        fakes::sources_env_with("src-override", |c| c.source_fetch_allow_loopback = true).await;
    let id = create(
        &e,
        api(
            &format!("{}/customers", up.url),
            json!({"type": "header", "header_name": "X-Api-Key"}),
            Some(fakes::API_KEY),
        ),
    )
    .await;
    let path = crate::common::j(&format!("/sources/{id}/test"));

    // edited path + auth, no credentials: the stored secret is used (same origin)
    let (s, b) = post_as(
        &e,
        "care-admin",
        &path,
        json!({"config": {"url": format!("{}/me", up.url), "auth": {"type": "bearer"}}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["sample"],
        json!({"customer_id": "C-10482", "tier": "gold"})
    );

    // the stored secret never follows the URL to another host
    let other = up.url.replace("127.0.0.1", "localhost");
    let (s, b) = post_as(
        &e,
        "care-admin",
        &path,
        json!({"config": {"url": format!("{other}/me"), "auth": {"type": "bearer"}}}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("credentials_required"))
    );
    assert_eq!(up.hits.load(Ordering::SeqCst), 1);

    // credentials null: sent without any secret
    let (s, b) = post_as(&e, "care-admin", &path, json!({"credentials": null})).await;
    assert_eq!(
        (s, b["ok"].clone(), b["status"].clone()),
        (StatusCode::OK, json!(false), json!(401))
    );

    // a typed secret replaces the stored one for this request only, and is not echoed
    let mark = "x-SECRETMARK-override-0123456789";
    let (s, b) = post_as(
        &e,
        "care-admin",
        &path,
        json!({"config": {"url": format!("{}/echo", up.url), "auth": {"type": "bearer"}},
               "credentials": {"secret": mark}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!b.to_string().contains("SECRETMARK"), "{b}");
    assert!(!crate::common::audit_text(&e).await.contains("SECRETMARK"));

    // the stored source is unchanged and the no-body test still uses it
    let (_, b) = test(&e, &id).await;
    assert_eq!(b["sample"]["customer_id"], "C-10482");
    let row: Value = sqlx::query_scalar("SELECT config FROM sources WHERE id = $1::uuid")
        .bind(&id)
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(row["url"], format!("{}/customers", up.url));
    let admin = crate::common::admin();
    let empty = axum::http::Request::builder()
        .method("POST")
        .uri(&path)
        .header("authorization", format!("Bearer {admin}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let (s, b) = crate::common::send(&e.app, empty).await;
    assert_eq!((s, b["ok"].clone()), (StatusCode::OK, json!(true)), "{b}");

    // overrides are validated like an unsaved test; a refused override is refused
    let (s, b) = post_as(
        &e,
        "care-admin",
        &path,
        json!({"config": {"url": format!("{}/x", up.url), "auth": {"type": "header", "header_name": "Cookie"}}}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_config"))
    );
    let (s, b) = post_as(
        &e,
        "care-admin",
        &path,
        json!({"config": {"url": "http://169.254.169.254/latest", "auth": {"type": "none"}}}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("address_refused"))
    );
    let (s, b) = post_as(&e, "care-admin", &path, json!({"name": "x"})).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_body"))
    );
    let (s, _) = post_as(&e, "care-viewer", &path, json!({"credentials": null})).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
