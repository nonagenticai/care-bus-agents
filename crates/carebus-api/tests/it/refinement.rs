//! Refinement writes (W3 plan 11 Task 5): every tenant edit creates or changes `origin = 'tenant'` rows only; revert,
//! rebase, hide and unhide by natural key; the job settings overlay; base procedure versions are read-only.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgPool};

const OVERLAY_TABLES: [&str; 9] = [
    "context_spaces",
    "ontology_entities",
    "ontology_attributes",
    "ontology_relations",
    "procedures",
    "procedure_steps",
    "connectors",
    "connector_field_mappings",
    "knowledge_stores",
];

const STORE: &str = "7f1c9a2e-0000-4000-8000-0000000000aa";

/// Every base row of the 9 overlay tables as canonical JSON, plus the jobs' base settings.
async fn base_snapshot(owner: &PgPool) -> Vec<Value> {
    let mut out = Vec::new();
    for t in OVERLAY_TABLES {
        let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT coalesce(jsonb_agg(x ORDER BY x::text), '[]'::jsonb) FROM (SELECT to_jsonb(t) AS x FROM {t} t WHERE origin = 'base') s"
        )))
        .fetch_one(owner)
        .await
        .unwrap();
        out.push(v);
    }
    out.push(
        sqlx::query_scalar("SELECT jsonb_agg(settings ORDER BY id) FROM jobs")
            .fetch_one(owner)
            .await
            .unwrap(),
    );
    out
}

async fn n(owner: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(sql.to_string()))
        .fetch_one(owner)
        .await
        .unwrap()
}

fn connector_body(id: &str) -> Value {
    json!({"id": id, "name": "CRM", "system": "CRM", "base_url": "https://crm.example/api", "auth_type": "none",
           "secret_ref": null, "timeout_ms": 2000, "rate_limit": null, "mappings": []})
}

fn del(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::delete(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn refine(
    e: &crate::common::TestEnv,
    op: &str,
    object: &str,
    key: Value,
) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/refinements/{op}")),
            Some(&crate::common::admin()),
            json!({"object": object, "key": key}),
        ),
    )
    .await
}

#[tokio::test]
async fn tenant_edits_create_only_tenant_rows() {
    let e = crate::common::env("refine-tenant-only").await;
    let owner = crate::common::pool(&e);
    let before = base_snapshot(owner).await;
    let a = crate::common::admin();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Order"),
            Some(&a),
            json!({"label":"Bestelling","scope":"industry","industry":"retail","department":null,"synonyms":[]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (
            b["label"].as_str(),
            b["origin"].as_str(),
            b["overridden"].as_bool()
        ),
        (Some("Bestelling"), Some("tenant"), Some(true))
    );
    assert_eq!(
        b["attributes"].as_array().unwrap().len(),
        5,
        "absent attributes are unchanged"
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Communication"),
            Some(&a),
            json!({"hidden": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _) = crate::common::send(
        &e.app,
        del(
            &crate::common::j("/ontology/relations/Customer/places/Order"),
            &a,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/connectors"),
            Some(&a),
            connector_body("crm-core"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/knowledge-stores/faq-local"),
            Some(&a),
            json!({"label":"Local","kind":"leaf","parent":null,"areas":["faq"],"strata_store_id": STORE}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        (b["strata_store_id"].as_str(), b["origin"].as_str()),
        (Some(STORE), Some("tenant"))
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/settings"),
            Some(&a),
            json!({"stance":"formal"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // an attribute edit: one override, one tombstone per left-out base attribute
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Product"),
            Some(&a),
            json!({"label":"Product","scope":"industry","industry":"retail","department":null,"synonyms":[],
                   "attributes":[{"name":"sku","data_type":"text","pii":false},{"name":"name","data_type":"string","pii":false}]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let names: Vec<&str> = b["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["name", "sku"]);

    assert_eq!(base_snapshot(owner).await, before, "no base row changed");
    let t: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, CASE WHEN hidden THEN 'tombstone' WHEN base_hash IS NULL THEN 'extension' ELSE 'override' END
           FROM ontology_entities WHERE origin='tenant' ORDER BY name",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    assert_eq!(
        t,
        vec![
            ("Communication".into(), "tombstone".into()),
            ("Order".into(), "override".into()),
            ("Product".into(), "override".into()),
        ]
    );
    let h: String = sqlx::query_scalar(
        "SELECT base_hash FROM ontology_entities WHERE origin='tenant' AND name='Order'",
    )
    .fetch_one(owner)
    .await
    .unwrap();
    assert_eq!(h, "bootstrap", "the bootstrap base's content_hash");
    assert_eq!(
        n(owner, "SELECT count(*) FROM ontology_relations WHERE origin='tenant' AND hidden AND subject='Customer' AND object='Order'").await,
        1
    );
    assert_eq!(n(owner, "SELECT count(*) FROM connectors WHERE origin='tenant' AND id='crm-core' AND base_hash IS NULL").await, 1);
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM knowledge_stores WHERE origin='tenant' AND key='faq-local'"
        )
        .await,
        1
    );
    let over: Value = sqlx::query_scalar("SELECT settings_override FROM jobs WHERE key = $1")
        .bind(crate::common::BOOT_JOB)
        .fetch_one(owner)
        .await
        .unwrap();
    assert_eq!(over["stance"]["value"], "formal");
    // Product: sku overridden (data_type changed), name equal to base (no row), category tombstoned
    let attrs: Vec<(String, bool)> = sqlx::query_as(
        "SELECT name, hidden FROM ontology_attributes WHERE origin='tenant' AND entity='Product' ORDER BY name",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    assert_eq!(
        attrs,
        vec![("category".into(), true), ("sku".into(), false)]
    );
    // every write was audited with its refinement kind
    let kinds: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT details->>'refinement' FROM audit_events WHERE resource_type IN ('ontology_entity','ontology_relation','connector','knowledge_store') ORDER BY id",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    assert_eq!(
        kinds,
        [
            "override",
            "tombstone",
            "tombstone",
            "extension",
            "extension",
            "override"
        ]
        .map(|k| Some(k.to_string()))
        .to_vec()
    );
}

#[tokio::test]
async fn revert_restores_base_and_unhide_removes_pure_tombstone() {
    let e = crate::common::env("refine-revert").await;
    let owner = crate::common::pool(&e);
    let a = crate::common::admin();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Order"),
            Some(&a),
            json!({"label":"Bestelling","scope":"industry","industry":"retail","department":null,"synonyms":[],
                   "attributes":[{"name":"order_id","data_type":"string","pii":false}]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        n(
            owner,
            "SELECT count(*) FROM ontology_attributes WHERE origin='tenant' AND entity='Order'"
        )
        .await
            > 0
    );
    let (s, b) = refine(&e, "revert", "ontology_entity", json!({"name": "Order"})).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (_, o) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Order"), Some(&a)),
    )
    .await;
    assert_eq!(
        (o["label"].as_str(), o["origin"].as_str()),
        (Some("Order"), Some("base"))
    );
    assert_eq!(o["attributes"].as_array().unwrap().len(), 5);
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM ontology_entities WHERE origin='tenant'"
        )
        .await,
        0
    );
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM ontology_attributes WHERE origin='tenant'"
        )
        .await,
        0
    );

    let (s, _) = refine(
        &e,
        "hide",
        "ontology_entity",
        json!({"name": "Communication"}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM ontology_entities WHERE origin='tenant' AND hidden"
        )
        .await,
        1
    );
    for _ in 0..2 {
        // the second unhide finds nothing hidden: idempotent
        let (s, _) = refine(
            &e,
            "unhide",
            "ontology_entity",
            json!({"name": "Communication"}),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(
            n(
                owner,
                "SELECT count(*) FROM ontology_entities WHERE origin='tenant'"
            )
            .await,
            0
        );
    }
    // a hidden override keeps its values when un-hidden
    crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Carrier"),
            Some(&a),
            json!({"label":"Vervoerder","scope":"industry","industry":"retail","department":null,"synonyms":[]}),
        ),
    )
    .await;
    refine(&e, "hide", "ontology_entity", json!({"name": "Carrier"})).await;
    refine(&e, "unhide", "ontology_entity", json!({"name": "Carrier"})).await;
    let (_, c) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Carrier"), Some(&a)),
    )
    .await;
    assert_eq!(
        (c["label"].as_str(), c["hidden"].as_bool()),
        (Some("Vervoerder"), Some(false))
    );
    // rebase of an override without a tenant row is 404; of an existing one 204
    let (s, _) = refine(&e, "rebase", "ontology_entity", json!({"name": "Policy"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = refine(&e, "rebase", "ontology_entity", json!({"name": "Carrier"})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // a bad object or key is a 400
    let (s, b) = refine(&e, "hide", "nope", json!({"name": "x"})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_object"))
    );
    let (s, b) = refine(&e, "hide", "ontology_relation", json!({"subject": "Order"})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_key"))
    );
}

#[tokio::test]
async fn hide_needs_a_base_row() {
    let e = crate::common::env("refine-hide-missing").await;
    let (s, b) = refine(&e, "hide", "connector", json!({"id": "nope"})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "refinement_target_missing");
    // a base context space can be hidden, and its procedures leave the effective view with it
    let (s, _) = refine(
        &e,
        "hide",
        "context_space",
        json!({"key": "retail:faq:general"}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, l) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures"),
            Some(&crate::common::admin()),
        ),
    )
    .await;
    assert!(
        l["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["name"] != "faq_answer")
    );
}

#[tokio::test]
async fn base_procedure_versions_are_read_only() {
    let e = crate::common::env("refine-proc-ro").await;
    let a = crate::common::admin();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/return/versions/1.3.0/approve"),
            Some(&a),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "base_row_read_only");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/return/versions/1.4.0-beta/status"),
            Some(&a),
            json!({"to": "DISCARDED"}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("base_row_read_only"))
    );
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/return/versions/9.9.9/approve"),
            Some(&a),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // a tenant version with a base version's (name, version) is refused
    let body = json!({"name":"return","version":"1.3.0","context_space":"retail:order-support:returns",
        "plan":{"phases":[{"steps":[{"worker":"order","action":"get_order","as":"order","args":{}}],"gate":null}]},
        "steps":[{"step_number":1,"phase":0,"name":"Get order","action_type":"get-order","worker":"order","mcp_tool":"order_get_order","performed_by":"order worker"}]});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/procedures"), Some(&a), body),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("version_exists"))
    );
}

#[tokio::test]
async fn settings_overlay() {
    let e = crate::common::env("refine-settings").await;
    let owner = crate::common::pool(&e);
    sqlx::query("UPDATE jobs SET settings = '{\"actor\": \"the care desk\", \"stance\": \"friendly\"}' WHERE key = $1")
        .bind(crate::common::BOOT_JOB)
        .execute(owner)
        .await
        .unwrap();
    let a = crate::common::admin();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/settings"),
            Some(&a),
            json!({"stance":"formal","actor":null}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/settings"), Some(&crate::common::user())),
    )
    .await;
    assert_eq!(g["effective"], json!({"stance": "formal"}));
    assert_eq!(g["base"]["actor"], "the care desk");
    assert_eq!(
        g["override"]["stance"]["base_hash"].as_str(),
        Some(carebus_api::refine::setting_hash("friendly").as_str())
    );
    let (s, _) = crate::common::send(&e.app, del(&crate::common::j("/settings/actor"), &a)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/settings"), Some(&a)),
    )
    .await;
    assert_eq!(
        g["effective"],
        json!({"stance": "formal", "actor": "the care desk"})
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/settings"),
            Some(&a),
            json!({"colour":"red"}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_setting"))
    );
    // hiding a setting through the generic route works too; unhide restores the base value
    let (s, _) = refine(&e, "hide", "job_setting", json!({"key": "actor"})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/settings"), Some(&a)),
    )
    .await;
    assert!(g["effective"].get("actor").is_none());
    refine(&e, "unhide", "job_setting", json!({"key": "actor"})).await;
    let (_, g) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/settings"), Some(&a)),
    )
    .await;
    assert_eq!(g["effective"]["actor"], "the care desk");
    assert_eq!(
        base_snapshot(owner).await.last().unwrap()[0]["stance"],
        "friendly"
    );
}

#[tokio::test]
async fn connector_put_overrides_a_base_slot_and_test_writes_health_only() {
    let e = crate::common::env("refine-connector").await;
    let owner = crate::common::pool(&e);
    // a base slot without endpoint, as release 1 would bring it (owner pool: no RLS)
    sqlx::query(
        "INSERT INTO connectors (id, name, system, base_url, auth_type, origin, release_id, content_hash)
         VALUES ('crm-core', 'CRM', 'crm', NULL, 'none', 'base', 'bootstrap-0009', 'sha256:slot')",
    )
    .execute(owner)
    .await
    .unwrap();
    let before = base_snapshot(owner).await;
    let a = crate::common::admin();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/connectors"),
            Some(&a),
            connector_body("crm-core"),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("connector_exists"))
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/connectors/crm-core"),
            Some(&a),
            connector_body("crm-core"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["origin"].as_str(), b["overridden"].as_bool()),
        (Some("tenant"), Some(true))
    );
    assert_eq!(b["base_url"], "https://crm.example/api");
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/connectors/crm-core/test"),
            Some(&a),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(base_snapshot(owner).await, before);
    assert_eq!(
        n(
            owner,
            "SELECT count(*) FROM connector_health WHERE connector_id='crm-core'"
        )
        .await,
        1
    );
    let h: String = sqlx::query_scalar(
        "SELECT base_hash FROM connectors WHERE origin='tenant' AND id='crm-core'",
    )
    .fetch_one(owner)
    .await
    .unwrap();
    assert_eq!(h, "sha256:slot");
}

#[tokio::test]
async fn user_cannot_refine() {
    let e = crate::common::env("refine-authz").await;
    let u = crate::common::user();
    for (path, body) in [
        (
            "/refinements/hide",
            json!({"object": "ontology_entity", "key": {"name": "Order"}}),
        ),
        (
            "/refinements/revert",
            json!({"object": "ontology_entity", "key": {"name": "Order"}}),
        ),
        ("/settings", json!({"stance": "x"})),
    ] {
        let m = if path == "/settings" { "PUT" } else { "POST" };
        let (s, _) = crate::common::send(
            &e.app,
            crate::common::json(m, &crate::common::j(path), Some(&u), body),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
    }
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/knowledge-stores/x"),
            Some(&u),
            json!({"label": "x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
