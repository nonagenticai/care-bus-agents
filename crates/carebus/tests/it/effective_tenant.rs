//! `X-Effective-Tenant-Id` on the wire: the platform `RigQualifier` (`RigQualifier::new`) sends the enclosing
//! `with_effective_tenant` tenant on every model call, and a tenant's own provider (`RigQualifier::with_client`)
//! never does. Against a local stub model that records the header of every request it gets.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use carebus::interaction::{
    EFFECTIVE_TENANT_HEADER, Qualifier, RigQualifier, conversation_start_data,
    with_effective_tenant,
};
use carebus::spaces::SpaceCatalog;

type Seen = Arc<Mutex<Vec<Option<String>>>>;

/// A model endpoint that records `X-Effective-Tenant-Id` (or its absence) of every request and answers 500, so the
/// extraction fails fast after its retries: only the requests matter here, not the answer.
async fn model_stub() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().fallback(move |h: HeaderMap| {
        let s = s.clone();
        async move {
            s.lock().unwrap().push(
                h.get(EFFECTIVE_TENANT_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "stub")
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

async fn qualify(q: &RigQualifier) {
    // The stub answers 500: the extraction fails; what was sent is in the stub's record.
    let _ = q
        .extract(
            "I want to return my headphones",
            None,
            &conversation_start_data(),
            &SpaceCatalog::bootstrap(),
        )
        .await;
}

fn take(seen: &Seen) -> Vec<Option<String>> {
    std::mem::take(&mut *seen.lock().unwrap())
}

/// Guards the mutation "remove `.with_middleware(EffectiveTenantHeader)` in `RigQualifier::new`": without it the
/// platform qualifier's requests reach the model with no `X-Effective-Tenant-Id` and this fails.
#[tokio::test]
async fn the_platform_qualifier_sends_the_effective_tenant_inside_its_scope_only() {
    let (url, seen) = model_stub().await;
    let q = RigQualifier::new("lic".into(), url, "local-llm".into()).unwrap();
    let tenant = uuid::Uuid::new_v4();

    with_effective_tenant(tenant, qualify(&q)).await;
    let inside = take(&seen);
    assert!(!inside.is_empty(), "the qualifier called the model");
    assert!(
        inside
            .iter()
            .all(|h| h.as_deref() == Some(tenant.to_string().as_str())),
        "every platform call inside the scope carries the tenant: {inside:?}"
    );

    // outside `with_effective_tenant`: the same client sends no header
    qualify(&q).await;
    let outside = take(&seen);
    assert!(!outside.is_empty(), "the qualifier called the model");
    assert!(
        outside.iter().all(Option::is_none),
        "no header outside the scope: {outside:?}"
    );
}

/// Guards the leak to third parties: a tenant's own provider (`RigQualifier::with_client`) must never get
/// `X-Effective-Tenant-Id`, even inside `with_effective_tenant` (e.g. the middleware added to `with_client` too).
#[tokio::test]
async fn a_tenant_provider_never_gets_the_effective_tenant() {
    let (url, seen) = model_stub().await;
    let http = rig_reqwest::reqwest::Client::builder()
        .timeout(carebus::interaction::LLM_TIMEOUT)
        .build()
        .unwrap();
    let q = RigQualifier::with_client("tenant-key".into(), url, "their-model".into(), http);

    with_effective_tenant(uuid::Uuid::new_v4(), qualify(&q)).await;
    let sent = take(&seen);
    assert!(!sent.is_empty(), "the qualifier called the model");
    assert!(
        sent.iter().all(Option::is_none),
        "a tenant's own provider gets no header: {sent:?}"
    );
}
