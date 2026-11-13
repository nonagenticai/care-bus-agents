use axum::http::StatusCode;

fn boot_scope() -> carebus::Scope {
    carebus::Scope {
        tenant: crate::common::BOOT_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    }
}

/// The bootstrap job's bus: the routes under `crate::common::j(..)` read exactly these keys.
fn boot_bus(e: &crate::common::TestEnv) -> carebus::Bus {
    e.state.bus.scoped(&boot_scope())
}

#[tokio::test]
async fn agents_aggregate_their_consumer_groups() {
    let e = crate::common::env("agents").await;
    let bus = &boot_bus(&e);
    let k = bus.streams.key("bus:tool:delivery");
    bus.ensure_group(&k, "tool-executor").await.unwrap();
    for i in 0..25 {
        bus.publish(
            &k,
            &carebus::Envelope::new(
                "logistics.delivery-status.tool.call",
                "worker:delivery",
                &format!("C{i}"),
                serde_json::json!({}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    }
    let t = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/agents"), Some(&t)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "interaction",
            "orchestrator",
            "worker-crm",
            "worker-order",
            "worker-delivery",
            "worker-faq",
            "worker-business-rules",
            "worker-procedures",
            "tool-executor"
        ]
    );
    let (_, te) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/agents/tool-executor"), Some(&t)),
    )
    .await;
    assert_eq!(te["lag"], 25);
    assert_eq!(te["status"], "idle"); // no consumer registered yet: idle beats warn
    assert_eq!(te["streams"], serde_json::json!(["bus:tool:delivery"]));
    assert_eq!(te["kind"], "bridge");
    // `model_routes` lists the agent's LLM uses that are bound to a provider: none yet
    let interaction = || crate::common::get(&crate::common::j("/agents/interaction"), Some(&t));
    let (_, ia) = crate::common::send(&e.app, interaction()).await;
    assert_eq!(ia["model_routes"], serde_json::json!([]));
    crate::common::bind_llm_use(crate::common::pool(&e), "qualify").await;
    let (_, ia) = crate::common::send(&e.app, interaction()).await;
    assert_eq!(ia["model_routes"], serde_json::json!(["qualify"]));
    let (_, orch) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/agents/orchestrator"), Some(&t)),
    )
    .await;
    assert_eq!(orch["model_routes"], serde_json::json!([]));
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j("/agents/nope"), Some(&t))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(&crate::common::j("/agents"), None)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}
