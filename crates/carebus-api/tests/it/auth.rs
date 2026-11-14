use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn no_token_is_401_with_error_shape() {
    let e = crate::common::env("auth401").await;
    let (s, b) = crate::common::send(&e.app, crate::common::get("/api/audit", None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(b["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn unknown_service_token_is_401() {
    let e = crate::common::env("authsvc401").await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get("/api/audit", Some("svc-test-token-x")),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(b["error"]["code"], "unauthorized");
}

/// An unprovisioned subject holding the old realm role: realm roles are never read, so it gets nothing.
#[tokio::test]
async fn realm_roles_grant_nothing() {
    let e = crate::common::env("authrealm").await;
    let t = crate::common::token_with_realm("u-x", &["care-admin"]);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/connectors"), Some(&t)),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("not_provisioned")),
        "{b}"
    );
    // a provisioned person keeps exactly their tenant role, whatever realm roles the token carries
    let t = crate::common::token_with_realm("u-user", &["care-admin", "super_admin"]);
    let (s, _) = crate::common::send(&e.app, put_route(&t)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unprovisioned_sub_is_403() {
    let e = crate::common::env("authunprov").await;
    let nobody = crate::common::token_for("nobody", &[]);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/procedures"), Some(&nobody)),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["error"]["code"], "not_provisioned");
    let (s, b) = crate::common::send(&e.app, crate::common::get("/api/audit", Some(&nobody))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["error"]["code"], "not_provisioned");
    // the one route an unprovisioned person may call
    let (s, me) = crate::common::send(&e.app, crate::common::get("/api/me", Some(&nobody))).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert!(me["tenant"].is_null());
}

/// An admin-gated write with an incomplete body: 403 below tenant_admin, 400 `invalid_body` at it.
fn put_route(t: &str) -> axum::http::Request<axum::body::Body> {
    crate::common::json(
        "POST",
        &crate::common::j("/connectors"),
        Some(t),
        json!({"name":"CRM"}),
    )
}

/// tenant_user reads what tenant_admin reads (Q2 default) but cannot write.
#[tokio::test]
async fn user_reads_admin_writes() {
    let e = crate::common::env("authroles").await;
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get("/api/audit", Some(&crate::common::user()))
        )
        .await
        .0,
        StatusCode::OK
    );
    let (s, b) = crate::common::send(&e.app, put_route(&crate::common::user())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["error"]["code"], "forbidden");
    // tenant_admin passes the admin gate (the incomplete body then fails validation: 400, not 403)
    let (s, b) = crate::common::send(&e.app, put_route(&crate::common::admin())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "invalid_body");
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get("/api/audit", Some(&crate::common::admin()))
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn suspended_tenant_is_403() {
    let e = crate::common::env("authsuspended").await;
    sqlx::query("UPDATE tenants SET status = 'suspended' WHERE id = $1::uuid")
        .bind(crate::common::BOOT_TENANT)
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    for t in [crate::common::admin(), crate::common::user()] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j("/cases"), Some(&t)),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert_eq!(b["error"]["code"], "tenant_suspended");
    }
}

#[tokio::test]
async fn wrong_audience_wrong_issuer_or_expired_token_is_401() {
    let e = crate::common::env("authbad").await;
    let ok_exp = (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp();
    let cases = [
        (
            "wrong audience",
            crate::common::token_with(&[], crate::common::TEST_ISSUER, "someone-else", ok_exp),
        ),
        (
            "wrong issuer",
            crate::common::token_with(
                &[],
                "https://evil.test/realms/portal",
                crate::common::TEST_AUDIENCE,
                ok_exp,
            ),
        ),
        (
            "expired",
            crate::common::token_with(
                &[],
                crate::common::TEST_ISSUER,
                crate::common::TEST_AUDIENCE,
                1,
            ),
        ),
    ];
    for (name, t) in cases {
        let (s, b) = crate::common::send(&e.app, crate::common::get("/api/audit", Some(&t))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{name}");
        assert_eq!(b["error"]["code"], "unauthorized", "{name}");
    }
    // control: the same helper with good claims is accepted
    let good = crate::common::token_with(
        &[],
        crate::common::TEST_ISSUER,
        crate::common::TEST_AUDIENCE,
        ok_exp,
    );
    assert_eq!(
        crate::common::send(&e.app, crate::common::get("/api/audit", Some(&good)))
            .await
            .0,
        StatusCode::OK
    );
}

/// Every claim the verifier relies on is mandatory: a token missing (or emptying) one is 401 `unauthorized`.
#[tokio::test]
async fn missing_claims_or_alg_none_is_401() {
    use base64::Engine;
    use jsonwebtoken::{EncodingKey, Header, encode};
    let e = crate::common::env("authclaims").await;
    let exp = (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp();
    let aud = crate::common::TEST_AUDIENCE;
    let good = json!({
        "sub": "u-admin", "preferred_username": "a.bakker", "exp": exp,
        "iss": crate::common::TEST_ISSUER, "aud": crate::common::TEST_AUDIENCE,
        "resource_access": {aud: {"roles": []}}
    });
    let sign = |c: &serde_json::Value| {
        encode(
            &Header::default(),
            c,
            &EncodingKey::from_secret(crate::common::TEST_SECRET)
                .expect("TEST_SECRET is a non-empty literal"),
        )
        .unwrap()
    };
    let without = |field: &str| {
        let mut c = good.clone();
        c.as_object_mut().unwrap().remove(field);
        sign(&c)
    };
    let mut empty_sub = good.clone();
    empty_sub["sub"] = json!(" ");
    let b64 = |v: &serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
    };
    let alg_none = format!(
        "{}.{}.",
        b64(&json!({"alg": "none", "typ": "JWT"})),
        b64(&good)
    );
    let cases = [
        ("no exp", without("exp")),
        ("no aud", without("aud")),
        ("no iss", without("iss")),
        ("no sub", without("sub")),
        ("blank sub", sign(&empty_sub)),
        ("alg none", alg_none),
    ];
    for (name, t) in cases {
        let (s, b) = crate::common::send(&e.app, crate::common::get("/api/audit", Some(&t))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{name}: {b}");
        assert_eq!(b["error"]["code"], "unauthorized", "{name}");
    }
    // control: the same claims, signed, pass
    let (s, b) =
        crate::common::send(&e.app, crate::common::get("/api/audit", Some(&sign(&good)))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
