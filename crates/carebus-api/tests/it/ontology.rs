use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn context_spaces_carry_derived_streams_and_topic_prefix() {
    let e = crate::common::env("ctx").await;
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/context-spaces"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let ret = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "retail:order-support:returns")
        .unwrap();
    assert_eq!(ret["streams"]["customer"], "bus:customer:order-support");
    assert_eq!(
        ret["streams"]["worker_results"],
        "bus:worker:order-support:results"
    );
    assert_eq!(ret["topic_prefix"], "order-support.returns");
    // derived from the published `return` procedure's steps, as the static table used to list them
    assert_eq!(
        ret["streams"]["worker"],
        json!([
            "bus:worker:order-support:crm",
            "bus:worker:order-support:order",
            "bus:worker:order-support:business-rules",
            "bus:worker:order-support:procedures"
        ])
    );
    let faq = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "retail:faq:general")
        .unwrap();
    assert_eq!(faq["streams"]["worker"], json!(["bus:worker:faq:faq"]));
}

/// The agents read the spaces with the service token, naming the tenant (`HttpSpaces::for_scope`).
#[tokio::test]
async fn service_token_reads_context_spaces() {
    let e = crate::common::env("ctx-svc").await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::get(&crate::common::j("/context-spaces"), Some("svc-test-token")),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 5);
}

/// A tenant extension space in a new area is listed with that area's streams; with no PUBLISHED procedure of its
/// own it names no worker streams (and the route does not fail).
#[tokio::test]
async fn extension_space_adds_an_area() {
    let e = crate::common::env("ctx-ext").await;
    sqlx::query(
        "INSERT INTO context_spaces (tenant_id, job_id, key, area, label, request_type, required, entities, procedure)
         SELECT tenant_id, job_id, 'retail:returns-desk:returns-b', 'returns-desk', 'Returns desk', 'returns_b',
                '{order_id}', '{Order}', 'return'
           FROM context_spaces WHERE key = 'retail:order-support:returns' AND origin = 'base'",
    )
    .execute(crate::common::pool(&e))
    .await
    .unwrap();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/context-spaces"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    let x = items
        .iter()
        .find(|c| c["key"] == "retail:returns-desk:returns-b")
        .unwrap();
    assert_eq!(x["area"], "returns-desk");
    assert_eq!(x["origin"], "tenant");
    assert_eq!(x["topic_prefix"], "returns-desk.returns-b");
    assert_eq!(x["streams"]["customer"], "bus:customer:returns-desk");
    assert_eq!(
        x["streams"]["worker_results"],
        "bus:worker:returns-desk:results"
    );
    assert_eq!(x["streams"]["worker"], json!([]));
}

#[tokio::test]
async fn entity_crud_with_attributes_and_tombstone() {
    let e = crate::common::env("onto").await;
    let admin = crate::common::admin();
    let viewer = crate::common::user();
    let voucher = json!({
        "name":"Voucher","scope":"industry_dept","industry":"retail","department":"payments","label":"Voucher","synonyms":["gift card"],
        "attributes":[{"name":"code","data_type":"string","pii":false,"synonyms":[]}]});
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/entities"),
            Some(&viewer),
            voucher.clone(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/entities"),
            Some(&admin),
            voucher,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = crate::common::send(&e.app, crate::common::json("POST", &crate::common::j("/ontology/entities"), Some(&admin), json!({
        "name":"Voucher","scope":"industry_dept","industry":"retail","department":"payments","label":"Voucher","synonyms":[],"attributes":[]}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Voucher"),
            Some(&admin),
            json!({"hidden": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // hidden entities are absent from the effective list and listed with include_hidden (the admin's unhide list)
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities"), Some(&admin)),
    )
    .await;
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["name"] != "Voucher")
    );
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/ontology/entities?include_hidden=true"),
            Some(&admin),
        ),
    )
    .await;
    let v = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "Voucher")
        .unwrap();
    assert_eq!(v["hidden"], true);
    assert_eq!(
        (v["origin"].as_str(), v["overridden"].as_bool()),
        (Some("tenant"), Some(false))
    );
    assert_eq!(v["attributes"][0]["name"], "code");
    let (_, rel) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/relations"), Some(&admin)),
    )
    .await;
    assert_eq!(rel["items"].as_array().unwrap().len(), 10);
}

#[tokio::test]
async fn single_entity_get_and_404() {
    let e = crate::common::env("onto-one").await;
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Order"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "Order");
    assert!(
        b["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "order_id")
    );
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Nope"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn put_rejects_non_objects_and_duplicate_attributes() {
    let e = crate::common::env("onto-put").await;
    let admin = crate::common::admin();
    for body in [json!([1]), json!("x"), json!(7), json!(null)] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(
                "PUT",
                &crate::common::j("/ontology/entities/Order"),
                Some(&admin),
                body,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b["error"]["code"], "invalid_body");
    }
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Nope"),
            Some(&admin),
            json!({"hidden": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let dup = json!({"name":"Order","scope":"industry","industry":"retail","department":null,"label":"Order","synonyms":[],
        "attributes":[{"name":"a","data_type":"string","pii":false},{"name":"a","data_type":"text","pii":true}]});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Order"),
            Some(&admin),
            dup.clone(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "duplicate_attribute");
    // the transaction rolled back: Order keeps its seeded attributes
    let (_, o) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Order"), Some(&admin)),
    )
    .await;
    assert!(o["attributes"].as_array().unwrap().len() > 2);
    // duplicate on create leaves no half-created entity
    let mut c = dup;
    c["name"] = json!("Dup");
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/entities"),
            Some(&admin),
            c,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Dup"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_entity_body_gets_a_fixed_message() {
    let e = crate::common::env("onto-fixed").await;
    let admin = crate::common::admin();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/entities"),
            Some(&admin),
            json!({"name":"X","scope":"generic","label":"ECHOMARKER","synonyms":"ECHOMARKER"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!b.to_string().contains("ECHOMARKER"), "{b}");
}

#[tokio::test]
async fn secret_shaped_entity_values_are_rejected_and_not_stored() {
    let e = crate::common::env("onto-secret").await;
    let admin = crate::common::admin();
    let body = json!({"name":"Leaky","scope":"generic","label":"Leaky","synonyms":["sk-live-LEAKLEAKLEAK"],"attributes":[]});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/entities"),
            Some(&admin),
            body,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "secret_value_rejected");
    assert!(!b.to_string().contains("LEAKLEAK"));
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Leaky"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let put = json!({"name":"Order","scope":"industry","industry":"retail","department":null,"label":"sk-live-LEAKLEAKLEAK","synonyms":[],"attributes":[]});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Order"),
            Some(&admin),
            put,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!b.to_string().contains("LEAKLEAK"));
    let (_, o) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Order"), Some(&admin)),
    )
    .await;
    assert_eq!(o["label"], "Order");
}

#[tokio::test]
async fn entity_put_cannot_orphan_a_connector_mapping() {
    let e = crate::common::env("onto-orphan").await;
    let admin = crate::common::admin();
    let conn = json!({"id":"oms","name":"OMS","system":"OMS","base_url":"http://127.0.0.1:9/api","auth_type":"none","secret_ref":null,"timeout_ms":2000,"rate_limit":null,
                      "mappings":[{"backend_field":"ord_no","attribute":"Order.order_id"}]});
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/connectors"), Some(&admin), conn),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, before) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Order"), Some(&admin)),
    )
    .await;
    let drop_it = json!({"name":"Order","scope":"industry","industry":"retail","department":null,"label":"Order","synonyms":[],
                         "attributes":[{"name":"placed","data_type":"date","pii":false}]});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Order"),
            Some(&admin),
            drop_it,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "attribute_in_use");
    let (_, after) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Order"), Some(&admin)),
    )
    .await;
    assert_eq!(before, after, "rolled back");
    // keeping the mapped attribute is fine
    let keep = json!({"name":"Order","scope":"industry","industry":"retail","department":null,"label":"Order","synonyms":[],
                      "attributes":[{"name":"order_id","data_type":"string","pii":false}]});
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/ontology/entities/Order"),
            Some(&admin),
            keep,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

fn del(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::delete(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

fn rel(subject: &str, predicate: &str, object: &str, cardinality: &str) -> serde_json::Value {
    json!({"subject": subject, "predicate": predicate, "object": object, "cardinality": cardinality})
}

#[tokio::test]
async fn relation_create_duplicate_and_delete() {
    let e = crate::common::env("onto-rel").await;
    let admin = crate::common::admin();
    let body = rel("Customer", "files", "WarrantyClaim", "1:n");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/relations"),
            Some(&admin),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b, body);
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/relations"), Some(&admin)),
    )
    .await;
    let listed = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["subject"] == "Customer" && r["predicate"] == "files")
        .expect("listed");
    assert_eq!(listed["cardinality"], "1:n");
    assert_eq!(
        (listed["origin"].as_str(), listed["overridden"].as_bool()),
        (Some("tenant"), Some(false))
    );
    assert_eq!(list["items"].as_array().unwrap().len(), 11);

    // same (subject, predicate, object) with another cardinality is still a duplicate
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/relations"),
            Some(&admin),
            rel("Customer", "files", "WarrantyClaim", "n:m"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "relation_exists");

    let audit = crate::common::audit_text(&e).await;
    assert!(
        audit.contains("Customer/files/WarrantyClaim") && audit.contains("ontology_relation"),
        "{audit}"
    );

    let uri = &crate::common::j("/ontology/relations/Customer/files/WarrantyClaim");
    let (s, _) = crate::common::send(&e.app, del(uri, &admin)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = crate::common::send(&e.app, del(uri, &admin)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "not_found");
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/relations"), Some(&admin)),
    )
    .await;
    assert_eq!(list["items"].as_array().unwrap().len(), 10);
    let deletes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE resource_type='ontology_relation' AND action_type='delete'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(deletes, 1, "only the delete that removed a row is audited");

    // a seeded (base) relation can be deleted too: a tenant tombstone hides it, the base row stays
    let (s, _) = crate::common::send(
        &e.app,
        del(
            &crate::common::j("/ontology/relations/Order/paidBy/Payment"),
            &admin,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT origin, hidden FROM ontology_relations WHERE subject='Order' AND predicate='paidBy' AND object='Payment' ORDER BY origin",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(rows, vec![("base".into(), false), ("tenant".into(), true)]);
    let (s, _) = crate::common::send(
        &e.app,
        del(
            &crate::common::j("/ontology/relations/Order/paidBy/Payment"),
            &admin,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a hidden relation is gone");
    // re-creating it as it was removes the tombstone: the base row serves again
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/relations"),
            Some(&admin),
            rel("Order", "paidBy", "Payment", "1:n"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let tenant: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ontology_relations WHERE origin='tenant' AND subject='Order' AND predicate='paidBy'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(tenant, 0);
}

#[tokio::test]
async fn relation_validation() {
    let e = crate::common::env("onto-rel-bad").await;
    let admin = crate::common::admin();
    let post = |body| {
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/relations"),
            Some(&admin),
            body,
        )
    };
    let (s, b) = crate::common::send(&e.app, post(rel("Nope", "has", "Order", "1:n"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "unknown_entity");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("subject"),
        "{b}"
    );
    let (s, b) = crate::common::send(&e.app, post(rel("Order", "has", "Nope", "1:n"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "unknown_entity");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("object"),
        "{b}"
    );
    for card in ["1:many", "", "m:n", "N:M"] {
        let (s, b) = crate::common::send(&e.app, post(rel("Order", "has", "Carrier", card))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{card}");
        assert_eq!(b["error"]["code"], "invalid_body");
    }
    for pred in ["", "Has", "paid by", "paid_by", "has/x", "1st"] {
        let (s, b) = crate::common::send(&e.app, post(rel("Order", pred, "Carrier", "1:n"))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{pred:?}");
        assert_eq!(b["error"]["code"], "invalid_body");
    }
    let (s, b) = crate::common::send(&e.app, post(json!({"subject": "Order"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_body");
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/relations"), Some(&admin)),
    )
    .await;
    assert_eq!(
        list["items"].as_array().unwrap().len(),
        10,
        "nothing stored"
    );
}

#[tokio::test]
async fn relation_writes_need_admin() {
    let e = crate::common::env("onto-rel-authz").await;
    let viewer = crate::common::user();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/ontology/relations"),
            Some(&viewer),
            rel("Order", "has", "Carrier", "1:n"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = crate::common::send(
        &e.app,
        del(
            &crate::common::j("/ontology/relations/Order/paidBy/Payment"),
            &viewer,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/relations"), Some(&viewer)),
    )
    .await;
    assert_eq!(list["items"].as_array().unwrap().len(), 10);
}

#[tokio::test]
async fn entity_scope_needs_its_industry_and_department() {
    let e = crate::common::env("onto-scope").await;
    let admin = crate::common::admin();
    let cases = [
        json!({"scope":"industry","industry":null,"department":null}),
        json!({"scope":"industry","industry":"  ","department":null}),
        json!({"scope":"industry_dept","industry":"retail","department":null}),
        json!({"scope":"industry_dept","industry":null,"department":"payments"}),
    ];
    for c in cases {
        let mut body = json!({"name":"Scoped","label":"Scoped","synonyms":[],"attributes":[]});
        for (k, v) in c.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                &crate::common::j("/ontology/entities"),
                Some(&admin),
                body.clone(),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{c}");
        assert_eq!(b["error"]["code"], "invalid_scope");
        let mut put = body;
        put["name"] = json!("Order");
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(
                "PUT",
                &crate::common::j("/ontology/entities/Order"),
                Some(&admin),
                put,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{c}");
        assert_eq!(b["error"]["code"], "invalid_scope");
    }
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/ontology/entities/Scoped"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // generic needs neither; industry with an industry is fine
    for (name, scope) in [
        ("G", json!({"scope":"generic"})),
        ("I", json!({"scope":"industry","industry":"retail"})),
    ] {
        let mut body = json!({"name":name,"label":name,"synonyms":[],"attributes":[]});
        for (k, v) in scope.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                &crate::common::j("/ontology/entities"),
                Some(&admin),
                body,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }
}
