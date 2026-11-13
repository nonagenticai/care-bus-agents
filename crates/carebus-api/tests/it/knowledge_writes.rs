use crate::common::fakes;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::{Request, StatusCode};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

fn delete(uri: &str, token: &str) -> Request<Body> {
    Request::delete(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn create_store_validates_forwards_and_audits() {
    let (e, fake) = fakes::sources_env("kw-create").await;
    let admin = crate::common::admin();
    let viewer = crate::common::user();
    let body = json!({"parent_id": fakes::ROUTER, "kind": "leaf", "name": "warranty", "description": "Warranty terms"});
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", "/api/knowledge/stores", Some(&viewer), body.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", "/api/knowledge/stores", Some(&admin), body.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["id"], fakes::NEW_STORE);
    assert_eq!(b["children"], json!([]));
    assert_eq!(fake.lock().unwrap().created_stores, [body]);
    for (bad, code) in [
        (json!({"kind": "folder", "name": "x"}), "invalid_kind"),
        (json!({"kind": "leaf", "name": " "}), "invalid_name"),
        (
            json!({"kind": "leaf", "name": "x", "colour": "red"}),
            "invalid_body",
        ),
    ] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json("POST", "/api/knowledge/stores", Some(&admin), bad),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::BAD_REQUEST, json!(code))
        );
    }
    // Built at runtime so no high-entropy literal sits in the repo.
    let secret = json!({"kind": "leaf", "name": "x", "description": format!("Zx81Qm0aPv7L{}", "hK2cYt5NwR9eB3uJd6Fg0SxT4oHq")});
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", "/api/knowledge/stores", Some(&admin), secret),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/knowledge/stores",
            Some(&admin),
            json!({"kind": "leaf", "name": "taken"}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("store_conflict"))
    );
    let audit: Vec<(String, String)> = sqlx::query_as(
        "SELECT resource_id, status FROM audit_events WHERE resource_type='knowledge_store' AND action_type='create' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        audit,
        [
            (fakes::NEW_STORE.to_string(), "success".to_string()),
            ("taken".into(), "failure".into())
        ]
    );
    // PF5: a failure row holds our code and status only, never upstream text.
    let failure: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE resource_type='knowledge_store' AND status='failure'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(failure["error_code"], "store_conflict");
    assert_eq!(failure["http_status"], 409);
    assert!(!failure.to_string().contains("sibling"), "{failure}");
}

#[tokio::test]
async fn a_store_a_source_still_targets_cannot_be_deleted() {
    let (e, fake) = fakes::sources_env("kw-delete").await;
    let admin = crate::common::admin();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/sources"),
            Some(&admin),
            json!({
            "name": "Help centre", "type": "website", "config": {"urls": ["https://help.example.nl/"]},
            "target_store_id": fakes::LEAF}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, b) = crate::common::send(
        &e.app,
        delete(&format!("/api/knowledge/stores/{}", fakes::LEAF), &admin),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("store_in_use"))
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Help centre")
    );
    let (s, b) = crate::common::send(
        &e.app,
        delete(
            &format!("/api/knowledge/stores/{}?recursive=true", fakes::ROUTER),
            &admin,
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("store_in_use")),
        "a target below counts"
    );
    let (s, b) = crate::common::send(
        &e.app,
        delete(&format!("/api/knowledge/stores/{}", fakes::ROUTER), &admin),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("store_conflict")),
        "strata_rag: has_children"
    );
    assert!(fake.lock().unwrap().deleted_stores.is_empty());

    let (s, b) = crate::common::send(
        &e.app,
        delete(
            &format!("/api/knowledge/stores/{}", fakes::OTHER_LEAF),
            &admin,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(
        fake.lock().unwrap().deleted_stores,
        [(fakes::OTHER_LEAF.to_string(), false)]
    );
    let ghost = uuid::Uuid::new_v4();
    let (s, _) = crate::common::send(
        &e.app,
        delete(&format!("/api/knowledge/stores/{ghost}"), &admin),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_document_is_204_404_or_503() {
    let (e, fake) = fakes::sources_env("kw-doc").await;
    let admin = crate::common::admin();
    let viewer = crate::common::user();
    let d = fakes::doc_id(1);
    let uri = format!("/api/knowledge/documents/{d}");
    assert_eq!(
        crate::common::send(&e.app, delete(&uri, &viewer)).await.0,
        StatusCode::FORBIDDEN
    );
    let (s, b) = crate::common::send(&e.app, delete(&uri, &admin)).await;
    assert_eq!((s, b), (StatusCode::NO_CONTENT, Value::Null));
    assert_eq!(
        crate::common::send(&e.app, delete(&uri, &admin)).await.0,
        StatusCode::NOT_FOUND,
        "already deleted"
    );
    fake.lock().unwrap().rag_down = true;
    let other = format!("/api/knowledge/documents/{}", fakes::doc_id(2));
    assert_eq!(
        crate::common::send(&e.app, delete(&other, &admin)).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM audit_events WHERE resource_type='knowledge_document' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(rows, ["success", "failure", "failure"]);
}

/// A strata_rag that records the `X-Tenant-Id` of every request it gets and answers each with a new store.
async fn recording_rag() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let log = seen.clone();
    let app = Router::new().fallback(move |h: HeaderMap| {
        let log = log.clone();
        async move {
            let tenant = h
                .get("x-tenant-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            log.lock().unwrap().push(tenant);
            (
                StatusCode::CREATED,
                Json(json!({"id": fakes::NEW_STORE, "parent_id": null, "kind": "leaf", "name": "warranty",
                            "description": "", "children": []})),
            )
        }
    });
    (fakes::serve(app).await, seen)
}

#[tokio::test]
async fn strata_call_for_other_tenant_is_409() {
    let (rag, seen) = recording_rag().await;
    let e = crate::common::env_with("kw-other-tenant", |c| {
        c.strata_rag_url = Some(rag.clone());
        c.strata_rag_token = Some(fakes::RAG_TOKEN.into());
    })
    .await;
    // OTHER_TENANT with its own tenant_admin; the static strata tokens are pinned to BOOT_TENANT.
    let other =
        crate::common::insert_tenant(crate::common::pool(&e), crate::common::OTHER_TENANT).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, updated_by) VALUES ('u-acme', $1, 'tenant_admin', 'test')",
    )
    .bind(other)
    .execute(crate::common::pool(&e))
    .await
    .unwrap();
    let acme = crate::common::token_for("u-acme", &[]);
    let body = json!({"kind": "leaf", "name": "warranty"});

    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", "/api/knowledge/stores", Some(&acme), body.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "strata_not_provisioned_for_tenant");
    for uri in [
        "/api/knowledge/stores",
        "/api/providers/slots",
        "/api/ingest/jobs",
    ] {
        let (s, b) = crate::common::send(&e.app, crate::common::get(uri, Some(&acme))).await;
        // ingest is not configured in this env: unconfigured stays 503, never a call
        let want = if uri == "/api/ingest/jobs" {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::CONFLICT
        };
        assert_eq!(s, want, "{uri}: {b}");
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reaches strata_rag for a tenant its token is not pinned to"
    );

    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/knowledge/stores",
            Some(&crate::common::admin()),
            body,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        *seen.lock().unwrap(),
        [Some(crate::common::BOOT_TENANT.to_string())],
        "the request's tenant is forwarded"
    );
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM audit_events WHERE resource_type='knowledge_store' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        rows,
        [crate::common::BOOT_TENANT.parse::<Uuid>().unwrap()],
        "the refused call is not audited; the write is audited in its tenant"
    );
}
