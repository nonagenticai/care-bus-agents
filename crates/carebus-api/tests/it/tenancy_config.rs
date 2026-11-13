//! Cross-tenant isolation of the job-scoped config and case routes (plan 10 Task 7): a tenant_admin of OTHER,
//! acting in OTHER's own `customer_care_agent` job, never reaches a bootstrap row by id, read or write.

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

async fn n(db: &PgPool, sql: &str, tenant: uuid::Uuid) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(tenant)
        .fetch_one(db)
        .await
        .unwrap()
}

/// OTHER with its own `customer_care_agent` job and `u-acme` as its tenant_admin, written as provision-down would.
async fn provision_other(owner: &PgPool) -> String {
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    crate::common::insert_job(owner, other, crate::common::BOOT_JOB).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-acme', $1, 'tenant_admin', 'Acme admin', 'test')",
    )
    .bind(other)
    .execute(owner)
    .await
    .unwrap();
    crate::common::token_for("u-acme", &[])
}

#[tokio::test]
async fn cross_tenant_ids_are_404() {
    let e = crate::common::env("ten-cfg-404").await;
    let owner = crate::common::pool(&e);
    let boot: uuid::Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    // Bootstrap rows the seed does not carry (the owner pool's session GUCs put them in the bootstrap job).
    sqlx::query(
        "INSERT INTO connectors (id, name, system, base_url, auth_type)
         VALUES ('crm-core', 'CRM', 'crm', 'https://crm.example/api', 'none')",
    )
    .execute(owner)
    .await
    .unwrap();
    let boot_source = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sources (id, name, type, target_store_id, updated_by)
         VALUES ($1, 'Help centre', 'website', $2, 'test')",
    )
    .bind(boot_source)
    .bind(uuid::Uuid::new_v4())
    .execute(owner)
    .await
    .unwrap();
    let boot_case = "CASE-BOOT-1";
    sqlx::query(
        "INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, started_at)
         VALUES ($1, 'C-1', 'return', 'order-support', 'retail:order-support:returns', now())",
    )
    .bind(boot_case)
    .execute(owner)
    .await
    .unwrap();
    let acme = provision_other(owner).await;

    let source_path = format!("/sources/{boot_source}");
    let case_path = format!("/cases/{boot_case}");
    for (m, path) in [
        ("GET", "/connectors/crm-core"),
        ("PUT", "/connectors/crm-core"),
        ("GET", "/procedures/return"),
        ("GET", "/procedures/return/versions/1.3.0"),
        ("POST", "/procedures/return/versions/1.3.0/approve"),
        ("GET", "/ontology/entities/Order"),
        ("PUT", "/ontology/entities/Order"),
        ("DELETE", "/ontology/relations/Customer/places/Order"),
        ("GET", source_path.as_str()),
        ("DELETE", source_path.as_str()),
        ("GET", case_path.as_str()),
    ] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(m, &crate::common::j(path), Some(&acme), json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {path}: {b}");
    }

    // A connector created by OTHER under the same id is legal in OTHER's own scope and leaves the bootstrap row alone.
    let body = json!({"id":"crm-core","name":"Acme CRM","system":"crm","base_url":"https://crm.acme.example/api",
                      "auth_type":"none","secret_ref":null,"timeout_ms":3000,"rate_limit":null});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/connectors"), Some(&acme), body),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let other: uuid::Uuid = crate::common::OTHER_TENANT.parse().unwrap();
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM connectors WHERE id='crm-core' AND name='Acme CRM' AND tenant_id=$1",
            other,
        )
        .await,
        1
    );
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM connectors WHERE id='crm-core' AND name='CRM' AND tenant_id=$1",
            boot,
        )
        .await,
        1
    );

    // The bootstrap rows are untouched.
    for sql in [
        "SELECT count(*) FROM ontology_relations WHERE subject='Customer' AND predicate='places' AND tenant_id=$1",
        "SELECT count(*) FROM ontology_entities WHERE name='Order' AND tenant_id=$1",
        "SELECT count(*) FROM connectors WHERE id='crm-core' AND tenant_id=$1",
        "SELECT count(*) FROM sources WHERE name='Help centre' AND tenant_id=$1",
        "SELECT count(*) FROM procedures WHERE name='return' AND version='1.3.0' AND tenant_id=$1",
    ] {
        assert_eq!(n(owner, sql, boot).await, 1, "{sql}");
    }
    let reviewers: Vec<String> = sqlx::query_scalar(
        "SELECT reviewers FROM procedures WHERE name='return' AND version='1.3.0' AND tenant_id=$1",
    )
    .bind(boot)
    .fetch_one(owner)
    .await
    .unwrap();
    assert!(!reviewers.contains(&"u-acme".to_string()));
    // No audit row of OTHER's refused writes landed in the bootstrap tenant.
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM audit_events WHERE actor_id='u-acme' AND tenant_id=$1",
            boot,
        )
        .await,
        0
    );
}
