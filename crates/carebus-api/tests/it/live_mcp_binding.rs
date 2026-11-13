//! The binding caller against a REAL nonagenticai_mcp_server (`bindings/ensure` with its Keycloak checks, then tasks):
//! carebus-api mints the tenant's mcp licence from licences, provisions the care-bus binding as the bundle minter,
//! keeps it sealed, and runs a long-running catalog tool to completion as that binding caller.
//!
//! Run (a local server with MCP_TASKS_ENABLED, the internal Keycloak configured and product care-bus mapped to a
//! toolkit holding LIVE_FR_TOOL):
//! MCP_URL=… LIVE_LICENCES_URL=<…/lic> LIVE_KEYCLOAK_TOKEN_URL=… LIVE_MINTER_SECRET=… LIVE_TENANT_ID=<uuid> \
//!   LIVE_FR_TOOL=… [LIVE_FR_ARGS='{…}'] cargo test -p carebus-api --test it live_mcp_binding -- --ignored --nocapture
use std::sync::Arc;
use std::time::{Duration, Instant};

use carebus::mcp_http::HttpMcp;
use carebus::mcp_tasks::{
    StaffActor, TaskOutcome, TaskStart, input_questions, outcome_of, validate_answer,
};
use carebus_api::crypto::SourceKey;
use carebus_api::mcp_binding::{Bindings, credentials, internal_base};
use carebus_api::signup::config::ClientCredentials;
use carebus_api::strata_tokens::BundleTokenConfig;
use serde_json::json;

#[tokio::test]
#[ignore]
async fn live_binding_is_provisioned_and_runs_an_fr_task() {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (Some(mcp_url), Some(licences), Some(token_url), Some(secret), Some(tenant), Some(tool)) = (
        var("MCP_URL"),
        var("LIVE_LICENCES_URL"),
        var("LIVE_KEYCLOAK_TOKEN_URL"),
        var("LIVE_MINTER_SECRET"),
        var("LIVE_TENANT_ID"),
        var("LIVE_FR_TOOL"),
    ) else {
        eprintln!(
            "MCP_URL / LIVE_LICENCES_URL / LIVE_KEYCLOAK_TOKEN_URL / LIVE_MINTER_SECRET / LIVE_TENANT_ID / LIVE_FR_TOOL not set; skipping"
        );
        return;
    };
    let args = var("LIVE_FR_ARGS")
        .map(|a| serde_json::from_str(&a).expect("LIVE_FR_ARGS is JSON"))
        .unwrap_or_else(|| json!({}));
    let e = crate::common::env_with("live-mcp-binding", |c| {
        c.strata_bundle = Some(BundleTokenConfig {
            url: licences.clone(),
            creds: ClientCredentials {
                token_url: token_url.clone(),
                client_id: var("LIVE_MINTER_CLIENT_ID")
                    .unwrap_or_else(|| "care-bus-bundle-minter".into()),
                client_secret: secret.clone(),
            },
            ttl_minutes: 15,
        });
    })
    .await;
    let tenant = crate::common::insert_tenant(&e.owner, &tenant).await;
    let key = {
        use base64::Engine;
        SourceKey::from_base64(&base64::engine::general_purpose::STANDARD.encode([5u8; 32]))
            .unwrap()
    };
    let bindings = Arc::new(Bindings::new(
        e.state.strata_tokens.clone(),
        reqwest::Client::new(),
        internal_base(&mcp_url, var("MCP_INTERNAL_URL").as_deref()),
        key,
        e.state.db.clone(),
    ));

    // Provisioned once, then served from the sealed row.
    let first = bindings
        .credentials(tenant, false)
        .await
        .expect("binding provisioned");
    let again = bindings
        .credentials(tenant, false)
        .await
        .expect("binding reused");
    assert_eq!(
        first.binding_token, again.binding_token,
        "kept, not re-provisioned"
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_bindings WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(stored, 1);

    // Run a task as the binding caller.
    let mcp = HttpMcp::new(mcp_url, credentials(bindings.clone()));
    let starter = StaffActor {
        tenant,
        staff_user: "staff-e2e-starter".into(),
    };
    let answerer = StaffActor {
        tenant,
        staff_user: "staff-e2e-answerer".into(),
    };
    let id = match mcp
        .start_task(&starter, &tool, args)
        .await
        .expect("tools/call")
    {
        TaskStart::Task(h) => h.task_id,
        TaskStart::Completed(v) => panic!("{tool} came back as a plain result, not a task: {v}"),
    };
    let start = Instant::now();
    let mut answered = false;
    let done = loop {
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "task {id} did not finish"
        );
        let s = mcp.get_task(tenant, &id).await.expect("tasks/get");
        eprintln!("tasks/get {id}: {} {:?}", s.status, s.status_message);
        if s.is_terminal() {
            break s;
        }
        if s.status == "input_required" && !answered {
            let q = &input_questions(&s.detail)[0];
            let a = validate_answer(
                &q.requested_schema,
                "accept",
                Some(&json!({"decision": "approved"})),
            )
            .unwrap();
            mcp.answer_task(&answerer, &id, &q.key, a)
                .await
                .expect("tasks/update");
            answered = true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert!(answered, "the task asked for an approval");
    let outcome = outcome_of(&done).expect("terminal");
    eprintln!("outcome {}", outcome.to_json());
    assert!(
        matches!(outcome, TaskOutcome::Completed(Ok(_))),
        "{}",
        done.detail
    );

    // Two refusals within the window rotate the binding: the new token works, the stored row follows it.
    let _ = bindings.credentials(tenant, true).await.expect("fresh 1");
    let rotated = bindings
        .credentials(tenant, true)
        .await
        .expect("fresh 2 rotates");
    assert_ne!(rotated.binding_token, first.binding_token, "rotated");
    let mcp = HttpMcp::new(var("MCP_URL").unwrap(), credentials(bindings));
    assert!(
        mcp.list_tools(tenant)
            .await
            .expect("tools/list with the rotated binding")
            .iter()
            .any(|t| t.name == tool),
        "the rotated binding still reaches {tool}"
    );
}
