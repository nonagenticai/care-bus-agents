//! Live check of `RigQualifier` against the real model endpoint
//! (`CAREBUS_LLM_BASE_URL`, default the vLLM gateway serving `local-llm`).
//! Ignored by default: `cargo test -p carebus --test it live_vllm -- --ignored`.
//! Skips with a message when `VLLM_LICENSE_TOKEN` is unset.

use carebus::interaction::{Qualifier, RigQualifier, conversation_start_data};
use carebus::spaces::SpaceCatalog;

fn qualifier() -> Option<RigQualifier> {
    if std::env::var("VLLM_LICENSE_TOKEN").is_err() {
        eprintln!("SKIP: VLLM_LICENSE_TOKEN not set, live model test not run");
        return None;
    }
    Some(RigQualifier::from_env().expect("RigQualifier from env"))
}

#[tokio::test]
#[ignore = "calls the live model; needs VLLM_LICENSE_TOKEN"]
async fn live_return_extracts_type_order_and_reason() {
    let Some(q) = qualifier() else { return };
    let start = conversation_start_data();
    let ex = q
        .extract(
            "I want to return my headphones, they're too uncomfortable",
            None,
            &start,
            &SpaceCatalog::bootstrap(),
        )
        .await
        .expect("live extraction");
    println!("return -> {ex:?}");
    assert_eq!(ex.request_type.as_deref(), Some("return"));
    assert_eq!(ex.slots.get("order_id"), Some("ORD-58213"));
    let reason = ex.slots.get("reason").unwrap_or_default().to_lowercase();
    assert!(reason.contains("uncomfortable"), "reason: {reason:?}");
}

#[tokio::test]
#[ignore = "calls the live model; needs VLLM_LICENSE_TOKEN"]
async fn live_delivery_infers_order_from_start_data() {
    let Some(q) = qualifier() else { return };
    let start = conversation_start_data();
    let ex = q
        .extract(
            "Where is my airfryer?",
            None,
            &start,
            &SpaceCatalog::bootstrap(),
        )
        .await
        .expect("live extraction");
    println!("delivery -> {ex:?}");
    assert_eq!(ex.request_type.as_deref(), Some("delivery_status"));
    assert_eq!(ex.slots.get("order_id"), Some("ORD-58790"));
    assert_eq!(
        (
            ex.slots.get("reason"),
            ex.slots.get("defect"),
            ex.slots.get("error_type")
        ),
        (None, None, None),
        "blank fields must come back as None"
    );
}
