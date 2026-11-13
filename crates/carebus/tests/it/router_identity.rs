//! care-bus' caller identity on the wire: inside `with_effective_tenant` the platform `RigQualifier` sends
//! `X-App-Id: care-bus`, `X-Project-Id` (the tenant's tenant-level project) and `X-Actor-Id` (its service account)
//! next to `X-Effective-Tenant-Id`; a tenant's own provider (`RigQualifier::with_client`) never gets any of them.
//! Against a local stub model that records those headers of every request it gets.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use carebus::identity::{
    ACTOR_ID_HEADER, APP_ID_HEADER, PROJECT_ID_HEADER, tenant_level_project_id,
};
use carebus::interaction::{
    EFFECTIVE_TENANT_HEADER, Qualifier, RigQualifier, conversation_start_data,
    with_effective_tenant,
};
use carebus::spaces::SpaceCatalog;

const NAMES: [&str; 4] = [
    EFFECTIVE_TENANT_HEADER,
    APP_ID_HEADER,
    PROJECT_ID_HEADER,
    ACTOR_ID_HEADER,
];

/// What one request carried under [`NAMES`] (`None` = absent).
type Sent = [Option<String>; 4];
type Seen = Arc<Mutex<Vec<Sent>>>;

/// A model endpoint that records [`NAMES`] of every request and answers 500, so the extraction fails fast after its
/// retries: only the requests matter here, not the answer.
async fn model_stub() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().fallback(move |h: HeaderMap| {
        let s = s.clone();
        async move {
            let got = NAMES.map(|k| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string));
            s.lock().unwrap().push(got);
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

fn take(seen: &Seen) -> Vec<Sent> {
    std::mem::take(&mut *seen.lock().unwrap())
}

/// Guards `crate::identity::router_headers` in `EffectiveTenantHeader` and the `service_actor` it is built with in
/// `RigQualifier::with_service_actor`: dropping either header, or the actor, fails this.
#[tokio::test]
async fn the_platform_qualifier_sends_care_bus_identity_inside_the_tenant_scope() {
    let (url, seen) = model_stub().await;
    let actor = uuid::Uuid::parse_str("7f1c9a2e-0000-4000-8000-000000005e5a").unwrap();
    let q = RigQualifier::with_service_actor("lic".into(), url, "local-llm".into(), Some(actor))
        .unwrap();
    let tenant = uuid::Uuid::new_v4();
    let want: Sent = [
        Some(tenant.to_string()),
        Some("care-bus".to_string()),
        Some(tenant_level_project_id(tenant).to_string()),
        Some(actor.to_string()),
    ];

    with_effective_tenant(tenant, qualify(&q)).await;
    let inside = take(&seen);
    assert!(!inside.is_empty(), "the qualifier called the model");
    assert!(inside.iter().all(|h| *h == want), "{inside:?}");

    // outside `with_effective_tenant`: none of them
    qualify(&q).await;
    let outside = take(&seen);
    assert!(!outside.is_empty(), "the qualifier called the model");
    assert!(
        outside.iter().all(|h| h.iter().all(Option::is_none)),
        "nothing outside the scope: {outside:?}"
    );
}

/// No service actor configured (`RigQualifier::new`): the identity without `X-Actor-Id`, never an empty or nil one.
#[tokio::test]
async fn without_a_service_actor_no_actor_header_is_sent() {
    let (url, seen) = model_stub().await;
    let q = RigQualifier::new("lic".into(), url, "local-llm".into()).unwrap();
    let tenant = uuid::Uuid::new_v4();
    with_effective_tenant(tenant, qualify(&q)).await;
    let sent = take(&seen);
    assert!(!sent.is_empty(), "the qualifier called the model");
    for [eff, app, project, actor] in &sent {
        assert_eq!(eff.as_deref(), Some(tenant.to_string().as_str()));
        assert_eq!(app.as_deref(), Some("care-bus"));
        assert_eq!(
            project.as_deref(),
            Some(tenant_level_project_id(tenant).to_string().as_str())
        );
        assert_eq!(actor, &None);
    }
}

/// Guards the leak to third parties: a tenant's own provider (`RigQualifier::with_client`) gets none of care-bus'
/// identity headers, even inside `with_effective_tenant`.
#[tokio::test]
async fn a_tenant_provider_gets_none_of_the_identity_headers() {
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
        sent.iter().all(|h| h.iter().all(Option::is_none)),
        "a tenant's own provider gets none of them: {sent:?}"
    );
}
