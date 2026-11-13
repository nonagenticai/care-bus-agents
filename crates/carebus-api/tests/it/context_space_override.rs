//! `PUT /api/jobs/{job}/context-spaces/{key}`: a tenant override of a base context space (label, required fields,
//! entities), with the same copy-on-write overlay as the knowledge-store PUT: the effective read serves it, revert
//! restores the base, and it stays inside its tenant.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgPool};

const FAQ: &str = "retail:faq:general";

async fn n(owner: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(owner)
        .await
        .unwrap()
}

fn put_req(token: &str, key: &str, body: Value) -> axum::http::Request<axum::body::Body> {
    crate::common::json(
        "PUT",
        &crate::common::j(&format!("/context-spaces/{key}")),
        Some(token),
        body,
    )
}

async fn put(
    e: &crate::common::TestEnv,
    token: &str,
    key: &str,
    body: Value,
) -> (StatusCode, Value) {
    crate::common::send(&e.app, put_req(token, key, body)).await
}

async fn refine(e: &crate::common::TestEnv, op: &str) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/refinements/{op}")),
            Some(&crate::common::admin()),
            json!({"object": "context_space", "key": {"key": FAQ}}),
        ),
    )
    .await
}

/// The FAQ space of `layer` ("" = effective) as `token` reads it, or None.
async fn faq(e: &crate::common::TestEnv, token: &str, layer: &str) -> Option<Value> {
    let q = if layer.is_empty() {
        String::new()
    } else {
        format!("?layer={layer}")
    };
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/context-spaces{q}")),
            Some(token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == FAQ)
        .cloned()
}

#[tokio::test]
async fn put_overrides_the_effective_space_and_revert_restores_the_base() {
    let e = crate::common::env("cs-override").await;
    let owner = crate::common::pool(&e);
    let a = crate::common::admin();
    let (s, b) = put(
        &e,
        &a,
        FAQ,
        json!({"label": "Vragen", "required": ["question", "order_id"], "entities": ["Policy", "Order"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (
            b["label"].as_str(),
            b["origin"].as_str(),
            b["overridden"].as_bool()
        ),
        (Some("Vragen"), Some("tenant"), Some(true))
    );
    assert_eq!(b["required"], json!(["question", "order_id"]));
    assert_eq!(b["entities"], json!(["Policy", "Order"]));
    // routing fields are the base's
    assert_eq!(
        (
            b["request_type"].as_str(),
            b["area"].as_str(),
            b["procedure"].as_str()
        ),
        (Some("faq"), Some("faq"), Some("faq_answer"))
    );
    assert_eq!(b["streams"]["customer"], "bus:customer:faq");

    // the effective read (what agents route with) serves the override; the base layer is untouched
    let eff = faq(&e, &crate::common::user(), "").await.unwrap();
    assert_eq!(
        (eff["label"].as_str(), eff["origin"].as_str()),
        (Some("Vragen"), Some("tenant"))
    );
    assert_eq!(eff["required"], json!(["question", "order_id"]));
    let base = faq(&e, &a, "base").await.unwrap();
    assert_eq!(base["label"], "General questions");
    assert_eq!(base["required"], json!(["question"]));
    assert_eq!(
        faq(&e, &a, "tenant").await.unwrap()["overridden"],
        json!(true)
    );
    let h: String = sqlx::query_scalar(
        "SELECT base_hash FROM context_spaces WHERE origin = 'tenant' AND key = $1",
    )
    .bind(FAQ)
    .fetch_one(owner)
    .await
    .unwrap();
    assert_eq!(
        h, "bootstrap",
        "written against the bootstrap base's content_hash"
    );
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM context_spaces WHERE origin = 'base' AND label = 'Vragen'"
        )
        .await,
        0,
        "no base row changed"
    );

    // a partial body changes only what it names
    let (s, b) = put(&e, &a, FAQ, json!({"label": "Veelgestelde vragen"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["required"], json!(["question", "order_id"]));
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM context_spaces WHERE origin = 'tenant'"
        )
        .await,
        1
    );
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "SELECT resource_id, details FROM audit_events WHERE resource_type = 'context_space' ORDER BY id",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].0, FAQ);
    assert_eq!(audit[0].1["refinement"], "override");
    assert_eq!(
        audit[0].1["fields"],
        json!(["label", "required", "entities"])
    );
    assert_eq!(audit[1].1["fields"], json!(["label"]));

    // what cannot be refined, and bad input
    for (body, code) in [
        (
            json!({"label": "x", "request_type": "faq2"}),
            "invalid_body",
        ),
        (json!({"label": "x", "area": "help"}), "invalid_body"),
        (
            json!({"label": "x", "key": "retail:faq:other"}),
            "invalid_body",
        ),
        (json!({"label": "x", "procedure": "return"}), "invalid_body"),
        (json!({}), "invalid_body"),
        (json!({"label": " "}), "invalid_body"),
        (json!({"required": ["Order Id"]}), "invalid_body"),
        (json!({"colour": "red"}), "invalid_body"),
        (json!({"entities": ["Policy", "Nope"]}), "unknown_entity"),
    ] {
        let (s, b) = put(&e, &a, FAQ, body.clone()).await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some(code)),
            "{body}"
        );
    }
    let (s, b) = put(&e, &a, "retail:faq:nope", json!({"label": "x"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, b) = put(&e, &a, "Retail_FAQ", json!({"label": "x"})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_key"))
    );
    assert_eq!(
        faq(&e, &a, "").await.unwrap()["label"],
        "Veelgestelde vragen"
    );

    // revert drops the tenant row: the base serves again
    let (s, b) = refine(&e, "revert").await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let eff = faq(&e, &a, "").await.unwrap();
    assert_eq!(
        (
            eff["label"].as_str(),
            eff["origin"].as_str(),
            eff["overridden"].as_bool()
        ),
        (Some("General questions"), Some("base"), Some(false))
    );
    assert_eq!(eff["required"], json!(["question"]));
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM context_spaces WHERE origin = 'tenant'"
        )
        .await,
        0
    );

    // editing a hidden space overrides it and makes it visible again
    let (s, _) = refine(&e, "hide").await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(faq(&e, &a, "").await.is_none());
    let (s, b) = put(&e, &a, FAQ, json!({"label": "Vragen"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["label"].as_str(), b["hidden"].as_bool()),
        (Some("Vragen"), Some(false))
    );
}

#[tokio::test]
async fn a_tenant_user_cannot_override() {
    let e = crate::common::env("cs-override-user").await;
    let (s, _) = put(&e, &crate::common::user(), FAQ, json!({"label": "Vragen"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(
        n(
            crate::common::pool(&e),
            "SELECT count(*) FROM context_spaces WHERE origin = 'tenant'"
        )
        .await,
        0
    );
    // a super_admin acting in the tenant may, like a tenant_admin
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            put_req(
                &crate::common::super_admin(),
                FAQ,
                json!({"label": "Vragen"}),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Gives (tenant, job) the bootstrap release and a copy of the bootstrap job's base context spaces (every column,
/// re-scoped), through the owner pool: what migration 0009 seeded for the bootstrap job.
async fn copy_bootstrap_spaces(owner: &PgPool, tenant: uuid::Uuid, job: uuid::Uuid) {
    sqlx::query(
        "INSERT INTO job_releases (tenant_id, job_id, release_id, hash, is_current, applied_by)
         VALUES ($1, $2, 'bootstrap-0009', 'sha256:bootstrap-0009', true, 'test')",
    )
    .bind(tenant)
    .bind(job)
    .execute(owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO context_spaces
         SELECT (jsonb_populate_record(NULL::context_spaces,
                     to_jsonb(c) || jsonb_build_object('tenant_id', $1::uuid, 'job_id', $2::uuid))).*
           FROM context_spaces c
          WHERE c.origin = 'base' AND c.tenant_id = $3::uuid",
    )
    .bind(tenant)
    .bind(job)
    .bind(crate::common::BOOT_TENANT)
    .execute(owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn another_tenants_admin_cannot_see_or_touch_the_override() {
    let e = crate::common::env("cs-override-tenants").await;
    let owner = crate::common::pool(&e);
    // OTHER_TENANT runs the same job on a copy of the bootstrap base, with its own tenant_admin
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    let other_job = crate::common::insert_job(owner, other, crate::common::BOOT_JOB).await;
    copy_bootstrap_spaces(owner, other, other_job).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, updated_by) VALUES ('u-acme', $1, 'tenant_admin', 'test')",
    )
    .bind(other)
    .execute(owner)
    .await
    .unwrap();
    let acme = crate::common::token_for("u-acme", &[]);

    let (s, b) = put(&e, &crate::common::admin(), FAQ, json!({"label": "Vragen"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // acme sees its own base, not our override
    let theirs = faq(&e, &acme, "").await.unwrap();
    assert_eq!(
        (theirs["label"].as_str(), theirs["origin"].as_str()),
        (Some("General questions"), Some("base"))
    );
    assert!(
        crate::common::send(
            &e.app,
            crate::common::get(
                &crate::common::j("/context-spaces?layer=tenant"),
                Some(&acme)
            )
        )
        .await
        .1["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // acme's PUT and revert act on acme's rows only
    let (s, b) = put(&e, &acme, FAQ, json!({"label": "Acme FAQ"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/refinements/revert"),
            Some(&acme),
            json!({"object": "context_space", "key": {"key": FAQ}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(
        faq(&e, &crate::common::admin(), "").await.unwrap()["label"],
        "Vragen"
    );
    let ours: Vec<(String, String)> = sqlx::query_as(
        "SELECT tenant_id::text, label FROM context_spaces WHERE origin = 'tenant' ORDER BY tenant_id",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    assert_eq!(
        ours,
        vec![(crate::common::BOOT_TENANT.to_string(), "Vragen".to_string())]
    );
    // and the bootstrap admin cannot reach acme's job through acme's tenant header
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            put_req(&crate::common::admin(), FAQ, json!({"label": "x"})),
            crate::common::OTHER_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
