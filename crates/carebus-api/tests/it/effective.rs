//! The effective configuration (migration 0010): per natural key the tenant row over the base row of the current
//! release, tombstones hide, children follow their effective parents, and the views respect RLS. Read as the runtime
//! role `carebus_app`; fixtures are written through the owner pool (bootstrap tenant/job session GUCs).

use sqlx::{AssertSqlSafe, PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

/// A transaction acting as `carebus_app` with the given tenant/job GUCs (transaction-local).
async fn app_tx(db: &PgPool, tenant: &str, job: Option<Uuid>) -> Transaction<'static, Postgres> {
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SET LOCAL ROLE carebus_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.current_tenant', $1, true), set_config('app.current_job', $2, true)")
        .bind(tenant)
        .bind(job.map(|j| j.to_string()).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

async fn n(conn: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(conn)
        .await
        .unwrap()
}

async fn boot_job(db: &PgPool) -> Uuid {
    sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(crate::common::BOOT_TENANT.parse::<Uuid>().unwrap())
        .bind(crate::common::BOOT_JOB)
        .fetch_one(db)
        .await
        .unwrap()
}

/// An override: a tenant copy of a base context space with another label, `base_hash` = the base content_hash.
async fn override_space(owner: &PgPool, key: &str, label: &str) {
    sqlx::query(
        "INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure, base_hash)
         SELECT key, area, $2, request_type, required, entities, procedure, content_hash
           FROM context_spaces WHERE key = $1 AND origin = 'base'",
    )
    .bind(key)
    .bind(label)
    .execute(owner)
    .await
    .unwrap();
}

/// A tombstone: a hidden tenant copy of a base entity.
async fn tombstone_entity(owner: &PgPool, name: &str) {
    sqlx::query(
        "INSERT INTO ontology_entities (name, scope, industry, department, label, synonyms, hidden, base_hash)
         SELECT name, scope, industry, department, label, synonyms, true, content_hash
           FROM ontology_entities WHERE name = $1 AND origin = 'base'",
    )
    .bind(name)
    .execute(owner)
    .await
    .unwrap();
}

/// An extension: a tenant context space with no base row (label `Shipping`).
async fn extend_space(owner: &PgPool, key: &str, request_type: &str) {
    sqlx::query(
        "INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure)
         VALUES ($1, 'faq', 'Shipping', $2, '{question}', '{Policy}', $2)",
    )
    .bind(key)
    .bind(request_type)
    .execute(owner)
    .await
    .unwrap();
}

/// A tenant attribute on an entity (the parent may be a base row).
async fn add_tenant_attribute(owner: &PgPool, entity: &str, name: &str) {
    sqlx::query(
        "INSERT INTO ontology_attributes (entity, name, data_type) VALUES ($1, $2, 'string')",
    )
    .bind(entity)
    .bind(name)
    .execute(owner)
    .await
    .unwrap();
}

/// A tenant procedure version on the context space of the base procedure of the same name.
async fn add_tenant_procedure(owner: &PgPool, name: &str, version: &str, status: &str) {
    sqlx::query(
        "INSERT INTO procedures (name, version, version_status, context_space)
         SELECT $1, $2, $3::version_status, context_space
           FROM procedures WHERE name = $1 AND origin = 'base' LIMIT 1",
    )
    .bind(name)
    .bind(version)
    .bind(status)
    .execute(owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn override_wins_tombstone_hides_extension_adds() {
    let e = crate::common::env("eff-overlay").await;
    let owner = crate::common::pool(&e);
    let job = boot_job(owner).await;
    override_space(owner, "retail:faq:general", "FAQ (ours)").await;
    tombstone_entity(owner, "Communication").await;
    extend_space(owner, "retail:faq:shipping", "shipping_faq").await;

    let mut tx = app_tx(owner, crate::common::BOOT_TENANT, Some(job)).await;
    let rows: Vec<(String, String, String, bool)> = sqlx::query_as(
        "SELECT key, label, origin, overridden FROM eff_context_spaces ORDER BY key",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(rows.len(), 6, "{rows:?}");
    assert!(rows.contains(&(
        "retail:faq:general".into(),
        "FAQ (ours)".into(),
        "tenant".into(),
        true
    )));
    assert!(rows.contains(&(
        "retail:faq:shipping".into(),
        "Shipping".into(),
        "tenant".into(),
        false
    )));
    assert!(rows.contains(&(
        "retail:order-support:returns".into(),
        "Returns".into(),
        "base".into(),
        false
    )));
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM eff_ontology_entities")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(names.len(), 10);
    assert!(!names.contains(&"Communication".to_string()));
}

#[tokio::test]
async fn children_follow_effective_parents() {
    let e = crate::common::env("eff-children").await;
    let owner = crate::common::pool(&e);
    let job = boot_job(owner).await;
    // base relation Customer-receives-Communication and Communication's attributes vanish with it
    tombstone_entity(owner, "Communication").await;
    let mut tx = app_tx(owner, crate::common::BOOT_TENANT, Some(job)).await;
    assert_eq!(
        n(
            &mut tx,
            "SELECT count(*) FROM eff_ontology_relations WHERE object = 'Communication'"
        )
        .await,
        0
    );
    assert_eq!(
        n(
            &mut tx,
            "SELECT count(*) FROM eff_ontology_attributes WHERE entity = 'Communication'"
        )
        .await,
        0
    );
    assert_eq!(
        n(&mut tx, "SELECT count(*) FROM eff_ontology_relations").await,
        9
    );
    tx.rollback().await.unwrap();

    // a tenant child on a BASE parent is allowed and effective
    add_tenant_attribute(owner, "Order", "channel_ref").await;
    let mut tx = app_tx(owner, crate::common::BOOT_TENANT, Some(job)).await;
    assert_eq!(
        n(
            &mut tx,
            "SELECT count(*) FROM eff_ontology_attributes WHERE entity = 'Order'"
        )
        .await,
        6
    );
    // an orphan tenant child (no parent in any origin) is not
    tx.rollback().await.unwrap();
    add_tenant_attribute(owner, "Nowhere", "x").await;
    let mut tx = app_tx(owner, crate::common::BOOT_TENANT, Some(job)).await;
    assert_eq!(
        n(
            &mut tx,
            "SELECT count(*) FROM eff_ontology_attributes WHERE entity = 'Nowhere'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn tenant_published_shadows_base_published() {
    let e = crate::common::env("eff-shadow").await;
    let owner = crate::common::pool(&e);
    let job = boot_job(owner).await;
    add_tenant_procedure(owner, "return", "1.3.0-acme", "PUBLISHED").await;
    let mut tx = app_tx(owner, crate::common::BOOT_TENANT, Some(job)).await;
    let v: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT version, origin, shadowed FROM eff_procedures
          WHERE name = 'return' AND version_status = 'PUBLISHED' ORDER BY origin",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        v,
        vec![
            ("1.3.0".into(), "base".into(), true),
            ("1.3.0-acme".into(), "tenant".into(), false)
        ]
    );
    // no other base version is shadowed
    assert_eq!(
        n(
            &mut tx,
            "SELECT count(*) FROM eff_procedures WHERE shadowed"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn views_see_only_current_tenant() {
    let e = crate::common::env("eff-tenant").await;
    let owner = crate::common::pool(&e);
    let b = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    crate::common::insert_job(owner, b, crate::common::BOOT_JOB).await;
    let mut tx = app_tx(owner, crate::common::OTHER_TENANT, None).await;
    for v in [
        "eff_context_spaces",
        "eff_ontology_entities",
        "eff_ontology_attributes",
        "eff_ontology_relations",
        "eff_procedures",
        "eff_procedure_steps",
        "eff_connectors",
        "eff_connector_field_mappings",
        "eff_knowledge_stores",
    ] {
        assert_eq!(
            n(&mut tx, &format!("SELECT count(*) FROM {v}")).await,
            0,
            "{v}"
        );
    }
    tx.rollback().await.unwrap();
    // and the bootstrap tenant, job unscoped, sees its own effective rows
    let mut tx = app_tx(owner, crate::common::BOOT_TENANT, None).await;
    assert_eq!(
        n(&mut tx, "SELECT count(*) FROM eff_context_spaces").await,
        5
    );
    assert_eq!(
        n(&mut tx, "SELECT count(*) FROM eff_procedure_steps").await,
        23
    );
}

// ---- Task 4: the read routes serve the effective view ----

fn item<'a>(b: &'a serde_json::Value, field: &str, value: &str) -> Option<&'a serde_json::Value> {
    b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i[field] == value)
}

#[tokio::test]
async fn routes_return_the_effective_view() {
    let e = crate::common::env("eff-routes").await;
    let owner = crate::common::pool(&e);
    override_space(owner, "retail:faq:general", "FAQ (ours)").await;
    tombstone_entity(owner, "Communication").await;

    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/context-spaces"),
            Some(&crate::common::admin()),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{b}");
    let faq = item(&b, "key", "retail:faq:general").unwrap();
    assert_eq!(
        (
            faq["label"].as_str(),
            faq["origin"].as_str(),
            faq["overridden"].as_bool()
        ),
        (Some("FAQ (ours)"), Some("tenant"), Some(true))
    );
    assert_eq!(b["items"].as_array().unwrap().len(), 5);

    let (_, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/context-spaces?layer=base"),
            Some(&crate::common::admin()),
        ),
    )
    .await;
    let faq = item(&b, "key", "retail:faq:general").unwrap();
    assert_eq!(
        (faq["label"].as_str(), faq["origin"].as_str()),
        (Some("General questions"), Some("base"))
    );
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/context-spaces?layer=tenant"),
            Some(&crate::common::admin()),
        ),
    )
    .await;
    assert_eq!(b["items"].as_array().unwrap().len(), 1);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/context-spaces?layer=nope"),
            Some(&crate::common::admin()),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_query");

    let (_, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/ontology/entities"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert!(item(&b, "name", "Communication").is_none());
    assert_eq!(b["items"].as_array().unwrap().len(), 10);
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/ontology/entities?include_hidden=true"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    let c = item(&b, "name", "Communication").unwrap();
    assert_eq!(c["hidden"], true);
    assert!(
        !c["attributes"].as_array().unwrap().is_empty(),
        "a hidden entity still lists its attributes"
    );
    // the relation to the hidden entity is not effective
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/ontology/relations"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(b["items"].as_array().unwrap().len(), 9);
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["origin"] == "base" && r["overridden"] == false)
    );
    // knowledge stores: none in the bootstrap base
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/knowledge-stores"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK);
    assert_eq!(b, serde_json::json!({"items": [], "next": null}));
}

#[tokio::test]
async fn shadowed_base_version_is_not_served() {
    let e = crate::common::env("eff-shadow-route").await;
    let owner = crate::common::pool(&e);
    add_tenant_procedure(owner, "return", "1.3.0-acme", "PUBLISHED").await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::get(
                &crate::common::j("/procedures/return"),
                Some("svc-test-token"),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "{b}");
    assert_eq!(b["version"], "1.3.0-acme");
    assert_eq!(
        (b["origin"].as_str(), b["shadowed"].as_bool()),
        (Some("tenant"), Some(false))
    );
    // the orchestrator's lookup by context space gets the tenant version only
    let (_, l) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::get(
                &crate::common::j(
                    "/procedures?context_space=retail%3Aorder-support%3Areturns&status=PUBLISHED",
                ),
                Some("svc-test-token"),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    let versions: Vec<&str> = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions, vec!["1.3.0-acme"]);
    // the full list still shows the base version, flagged
    let (_, l) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures"),
            Some(&crate::common::admin()),
        ),
    )
    .await;
    let base = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "return" && i["version"] == "1.3.0")
        .unwrap();
    assert_eq!(
        (base["origin"].as_str(), base["shadowed"].as_bool()),
        (Some("base"), Some(true))
    );
}
