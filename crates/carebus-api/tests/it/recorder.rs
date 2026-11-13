//! The recorder runs per registered scope and writes under that scope's RLS GUCs (plan 13 Task 4).

use std::time::Duration;

use carebus::domain::{QualifiedRequest, REQUEST_QUALIFIED, RequestType, Slots};
use carebus::{Envelope, Scope};
use uuid::Uuid;

fn qualified_env(case: &str) -> Envelope {
    let q = QualifiedRequest {
        case_id: case.into(),
        customer_id: "C-10482".into(),
        request_type: RequestType::r#return(),
        area: "order-support".into(),
        context_space: "retail:order-support:returns".into(),
        slots: Slots::of(&[("order_id", "ORD-58213")]),
        utterance: "return my headphones".into(),
        gates: None,
    };
    Envelope::new(&q.topic(REQUEST_QUALIFIED), "interaction", case, q).unwrap()
}

/// (tenant, job) of the case's row, read through the owner pool (RLS does not apply), polled for up to 10 s.
async fn row_scope(e: &crate::common::TestEnv, case: &str) -> Option<(Uuid, Uuid)> {
    for _ in 0..100 {
        let row: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT tenant_id, job_id FROM case_history WHERE case_id = $1")
                .bind(case)
                .fetch_optional(crate::common::pool(e))
                .await
                .unwrap();
        if row.is_some() {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

#[tokio::test]
async fn recorder_writes_in_the_envelope_scope() {
    let e = crate::common::env("recorder-scope").await;
    let owner = crate::common::pool(&e);
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    let other_job = crate::common::insert_job(owner, other, crate::common::BOOT_JOB).await;
    // The recorder listens to the areas of the job's context spaces: OTHER's job gets one in order-support.
    sqlx::query(
        "INSERT INTO context_spaces (tenant_id, job_id, key, area, label, request_type, required, entities, procedure)
         VALUES ($1, $2, 'retail:order-support:returns', 'order-support', 'Returns', 'return', '{order_id,reason}',
                 '{Order}', 'return')",
    )
    .bind(other)
    .bind(other_job)
    .execute(owner)
    .await
    .unwrap();
    let other_scope = Scope {
        tenant: other,
        job: crate::common::BOOT_JOB.into(),
    };
    carebus::scopes::add(&e.state.bus, &other_scope)
        .await
        .unwrap();
    let recorder = carebus_api::recorder::spawn(e.state.clone(), "recorder-scope-test".into());

    let bus = e.state.bus.scoped(&other_scope);
    let area = "order-support";
    bus.publish(&bus.streams.customer(area), &qualified_env("CASE-OTHER"))
        .await
        .unwrap();
    assert_eq!(
        row_scope(&e, "CASE-OTHER").await,
        Some((other, other_job)),
        "recorded in OTHER's tenant and job"
    );

    // the bootstrap scope (registered at startup) records into the bootstrap tenant and job
    let boot_scope = Scope::parse(&format!(
        "{}/{}",
        crate::common::BOOT_TENANT,
        crate::common::BOOT_JOB
    ))
    .unwrap();
    let boot_job: Uuid =
        sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1::uuid AND key = $2")
            .bind(crate::common::BOOT_TENANT)
            .bind(crate::common::BOOT_JOB)
            .fetch_one(owner)
            .await
            .unwrap();
    let boot_bus = e.state.bus.scoped(&boot_scope);
    boot_bus
        .publish(
            &boot_bus.streams.customer(area),
            &qualified_env("CASE-BOOT"),
        )
        .await
        .unwrap();
    assert_eq!(
        row_scope(&e, "CASE-BOOT").await,
        Some((boot_scope.tenant, boot_job))
    );
    let in_boot: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM case_history WHERE case_id = 'CASE-OTHER' AND tenant_id = $1::uuid",
    )
    .bind(crate::common::BOOT_TENANT)
    .fetch_one(owner)
    .await
    .unwrap();
    assert_eq!(in_boot, 0);

    // the runtime role sees OTHER's case only inside OTHER's scope
    let ids = carebus_api::recorder::ScopeIds::resolve(&e.state, bus.clone(), &other_scope)
        .await
        .unwrap();
    assert_eq!((ids.tenant, ids.job_id), (other, other_job));
    let mut tx = ids.begin(crate::common::app_pool(&e)).await.unwrap();
    let seen: Vec<String> = sqlx::query_scalar("SELECT case_id FROM case_history ORDER BY case_id")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, ["CASE-OTHER"]);
    drop(tx);
    recorder.abort();
}

#[tokio::test]
async fn unknown_job_scope_does_not_resolve() {
    let e = crate::common::env("recorder-nojob").await;
    let ghost = Scope::parse(&format!("{}/ghost", crate::common::BOOT_TENANT)).unwrap();
    let bus = e.state.bus.scoped(&ghost);
    assert!(
        carebus_api::recorder::ScopeIds::resolve(&e.state, bus, &ghost)
            .await
            .is_err()
    );
}
