//! care-bus' identity to strata_rag on the wire: every strata_rag request carries `X-App-Id: care-bus` (strata_rag
//! relays it to inference-router), and one no person is behind carries `X-Actor-Id` = `SERVICE_ACTOR_ID`, or none
//! when that is unset. Through a knowledge write proxy (`strata::call`, crate::strata_tokens credentials).
use crate::common::fakes;
use axum::http::{HeaderMap, StatusCode};
use axum::{Json, Router};
use serde_json::json;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// `(X-App-Id, X-Actor-Id count, X-Actor-Id)` of one request.
type Sent = (Option<String>, usize, Option<String>);

/// A strata_rag that records the identity headers of every request and answers each with a new store.
async fn recording_rag() -> (String, Arc<Mutex<Vec<Sent>>>) {
    let seen: Arc<Mutex<Vec<Sent>>> = Arc::default();
    let log = seen.clone();
    let app = Router::new().fallback(move |h: HeaderMap| {
        let log = log.clone();
        async move {
            let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
            log.lock().unwrap().push((
                hv("x-app-id"),
                h.get_all("x-actor-id").iter().count(),
                hv("x-actor-id"),
            ));
            (
                StatusCode::CREATED,
                Json(json!({"id": fakes::NEW_STORE, "parent_id": null, "kind": "leaf", "name": "warranty",
                            "description": "", "children": []})),
            )
        }
    });
    (fakes::serve(app).await, seen)
}

/// One store write by the bootstrap tenant's admin (`u-admin`, not a uuid subject: no person to name).
async fn write_store(e: &crate::common::TestEnv) {
    let body = json!({"kind": "leaf", "name": "warranty"});
    let _ = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            "/api/knowledge/stores",
            Some(&crate::common::admin()),
            body,
        ),
    )
    .await;
}

#[tokio::test]
async fn strata_rag_calls_carry_the_app_id_and_the_service_actor() {
    let service = Uuid::parse_str("7f1c9a2e-0000-4000-8000-000000005e5a").unwrap();
    let (rag, seen) = recording_rag().await;
    let e = crate::common::env_with("strata-identity-sa", |c| {
        c.strata_rag_url = Some(rag.clone());
        c.strata_rag_token = Some(fakes::RAG_TOKEN.into());
        c.service_actor = Some(service);
    })
    .await;
    write_store(&e).await;
    let got = std::mem::take(&mut *seen.lock().unwrap());
    assert!(!got.is_empty(), "the write reached strata_rag");
    for (app, actors, actor) in &got {
        assert_eq!(app.as_deref(), Some("care-bus"), "{got:?}");
        assert_eq!(*actors, 1, "one X-Actor-Id: {got:?}");
        assert_eq!(
            actor.as_deref(),
            Some(service.to_string().as_str()),
            "{got:?}"
        );
    }
}

#[tokio::test]
async fn without_a_service_actor_strata_rag_gets_the_app_id_and_no_actor() {
    let (rag, seen) = recording_rag().await;
    let e = crate::common::env_with("strata-identity-no-sa", |c| {
        c.strata_rag_url = Some(rag.clone());
        c.strata_rag_token = Some(fakes::RAG_TOKEN.into());
    })
    .await;
    write_store(&e).await;
    let got = std::mem::take(&mut *seen.lock().unwrap());
    assert!(!got.is_empty(), "the write reached strata_rag");
    for (app, actors, _) in &got {
        assert_eq!(app.as_deref(), Some("care-bus"), "{got:?}");
        assert_eq!(*actors, 0, "no made-up actor: {got:?}");
    }
}
