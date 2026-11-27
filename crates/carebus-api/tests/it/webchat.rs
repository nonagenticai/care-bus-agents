//! Web channel: registration (key + hashed secret), the shared-secret public door, one conversation
//! per session, and the events cursor. The turn runs in a spawned task; the tests assert the
//! customer event (written before the turn) and never depend on a model.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const HEADER: &str = "x-carebus-webchat-secret";

fn request(method: &str, uri: &str, secret: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(secret) = secret {
        b = b.header(HEADER, secret);
    }
    match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            b.body(Body::from(v.to_string())).unwrap()
        }
        None => b.body(Body::empty()).unwrap(),
    }
}

async fn register(e: &crate::common::TestEnv) -> (String, String) {
    let (status, body) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/channels/webchat"),
            Some(&crate::common::admin()),
            json!({"label": "test"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (
        body["key"].as_str().expect("key").to_string(),
        body["secret"].as_str().expect("secret").to_string(),
    )
}

#[tokio::test]
async fn registration_stores_only_a_hash_and_rotation_invalidates_the_old_secret() {
    let e = crate::common::env("webchat_register").await;
    let (key, secret) = register(&e).await;
    assert!(key.starts_with("wc_"), "{key}");
    assert_eq!(secret.len(), 64);

    let stored: String = sqlx::query_scalar("SELECT secret_hash FROM web_channels WHERE key = $1")
        .bind(&key)
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(stored, hex::encode(Sha256::digest(secret.as_bytes())));
    assert_ne!(stored, secret);

    let lookup: Option<(uuid::Uuid, String, String, bool)> = sqlx::query_as(
        "SELECT tenant_id, job_key, secret_hash, enabled FROM web_channel_lookup($1)",
    )
    .bind(&key)
    .fetch_optional(crate::common::app_pool(&e))
    .await
    .unwrap();
    assert!(
        lookup.is_some(),
        "the app role must see the registered channel"
    );

    let (same_key, new_secret) = register(&e).await;
    assert_eq!(same_key, key);
    assert_ne!(new_secret, secret);

    let uri = format!("/api/channel/webchat/{key}/messages");
    let (status, _) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            Some(&secret),
            Some(json!({"session_key": "s1", "text": "hi"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            Some(&new_secret),
            Some(json!({"session_key": "s1", "text": "hi"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

#[tokio::test]
async fn messages_create_one_conversation_per_session_and_events_replay_from_a_cursor() {
    let e = crate::common::env("webchat_messages").await;
    let (key, secret) = register(&e).await;
    let uri = format!("/api/channel/webchat/{key}/messages");

    let (status, _) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            None,
            Some(json!({"session_key": "s1", "text": "Hello"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            Some("wrong"),
            Some(json!({"session_key": "s1", "text": "Hello"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, first) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            Some(&secret),
            Some(json!({"session_key": "s1", "text": "Hello"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    let id = first["conversation_id"].as_str().expect("id").to_string();

    let (status, second) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            Some(&secret),
            Some(json!({"session_key": "s1", "text": "Again"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{second}");
    assert_eq!(second["conversation_id"], id);

    let events = format!("/api/channel/webchat/{key}/conversations/{id}/events?after=0");
    let (status, page) =
        crate::common::send(&e.app, request("GET", &events, Some(&secret), None)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let list = page["events"].as_array().expect("events");
    let hello = list
        .iter()
        .find(|ev| ev["role"] == "customer" && ev["text"] == "Hello")
        .expect("the customer event is replayed");
    assert_eq!(hello["seq"], 0);
    let cursor = page["cursor"].as_u64().expect("cursor");
    assert!(cursor >= 1);

    let after =
        format!("/api/channel/webchat/{key}/conversations/{id}/events?after={cursor}&wait_ms=0");
    let (status, page2) =
        crate::common::send(&e.app, request("GET", &after, Some(&secret), None)).await;
    assert_eq!(status, StatusCode::OK, "{page2}");
    for ev in page2["events"].as_array().expect("events") {
        assert!(ev["seq"].as_u64().unwrap() >= cursor);
    }

    let gone = format!(
        "/api/channel/webchat/{key}/conversations/{id}/events?after={}",
        cursor + 50
    );
    let (status, err) =
        crate::common::send(&e.app, request("GET", &gone, Some(&secret), None)).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(err["error"]["code"], "cursor_expired");

    let state = format!("/api/channel/webchat/{key}/conversations/{id}/state");
    let (status, st) =
        crate::common::send(&e.app, request("GET", &state, Some(&secret), None)).await;
    assert_eq!(status, StatusCode::OK, "{st}");
    assert_eq!(st["conversation_id"], id);
    assert!(st["customer_id"].as_str().unwrap().starts_with("web:"));
}

#[tokio::test]
async fn registration_needs_admin_and_delete_closes_the_door() {
    let e = crate::common::env("webchat_admin").await;
    let (status, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/channels/webchat"),
            Some(&crate::common::user()),
            json!({"label": "x"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (key, secret) = register(&e).await;
    let (status, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "DELETE",
            &crate::common::j("/channels/webchat"),
            Some(&crate::common::admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let uri = format!("/api/channel/webchat/{key}/messages");
    let (status, _) = crate::common::send(
        &e.app,
        request(
            "POST",
            &uri,
            Some(&secret),
            Some(json!({"session_key": "s", "text": "hi"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
