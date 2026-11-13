use axum::http::StatusCode;
use carebus::domain::{ProcedureSource, static_plan};
use carebus::spaces::SpaceCatalog;
use serde_json::{Value, json};

/// A tenant_admin token acting as `user`: the provisioned `u-admin` subject with `user` as its
/// `preferred_username`, which is the actor the audit trail and `reviewers` record. Distinct users make distinct
/// reviewers; `admin_as("a.bakker")` is the reviewer the seeded canary already has.
fn admin_as(user: &str) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let exp = (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp();
    let claims = json!({
        "sub": "u-admin", "preferred_username": user, "exp": exp,
        "iss": crate::common::TEST_ISSUER, "aud": crate::common::TEST_AUDIENCE,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(crate::common::TEST_SECRET),
    )
    .unwrap()
}

async fn reviewers_of(e: &crate::common::TestEnv, name: &str, version: &str) -> Vec<String> {
    sqlx::query_scalar::<_, Vec<String>>(
        "SELECT reviewers FROM procedures WHERE name=$1 AND version=$2",
    )
    .bind(name)
    .bind(version)
    .fetch_one(crate::common::pool(e))
    .await
    .unwrap()
}

/// Turns the seeded base versions of `name` into tenant versions with the same content, steps and reviewers (owner
/// pool). Base versions are read-only since plan 11 (a tenant changes a procedure by adding versions), so the
/// lifecycle tests below run on tenant versions; `base_procedure_versions_are_read_only` (tests/refinement.rs)
/// covers the base side.
async fn adopt_as_tenant(e: &crate::common::TestEnv, name: &str) {
    let db = crate::common::pool(e);
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM procedures WHERE name=$1 AND origin='base' ORDER BY id")
            .bind(name)
            .fetch_all(db)
            .await
            .unwrap();
    assert!(!ids.is_empty(), "{name} has base versions");
    for id in ids {
        let copy: i64 = sqlx::query_scalar(
            "INSERT INTO procedures (name, version, version_status, context_space, domain, description, pddl_domain,
                                     tool_requirements, success_criteria, reviewers, plan, created_at, updated_at)
             SELECT name, version, version_status, context_space, domain, description, pddl_domain,
                    tool_requirements, success_criteria, reviewers, plan, created_at, updated_at
               FROM procedures WHERE id = $1 RETURNING id",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO procedure_steps (procedure_id, step_number, phase, name, action_type, worker, mcp_tool,
                                          performed_by, preconditions, postconditions, parameters, gate)
             SELECT $2, step_number, phase, name, action_type, worker, mcp_tool,
                    performed_by, preconditions, postconditions, parameters, gate
               FROM procedure_steps WHERE procedure_id = $1",
        )
        .bind(id)
        .bind(copy)
        .execute(db)
        .await
        .unwrap();
        sqlx::query("DELETE FROM procedures WHERE id = $1")
            .bind(id)
            .execute(db)
            .await
            .unwrap();
    }
}

/// Approve and publish are separate calls: approving records a reviewer (200 with the detail), publishing with
/// fewer than two distinct reviewers is refused and writes nothing, and the publish after a second approval archives
/// the old version.
#[tokio::test]
async fn approve_then_publish_needs_two_distinct_reviewers_and_archives_the_old_version() {
    let e = crate::common::env("proc").await;
    adopt_as_tenant(&e, "return").await;
    let admin = admin_as("a.bakker");
    let status = &crate::common::j("/procedures/return/versions/1.4.0-beta/status");
    let approve = &crate::common::j("/procedures/return/versions/1.4.0-beta/approve");
    // CANARY seed has one reviewer (a.bakker): publishing now is refused and changes nothing
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", status, Some(&admin), json!({"to":"PUBLISHED"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "needs_two_reviewers");
    assert_eq!(
        reviewers_of(&e, "return", "1.4.0-beta").await,
        vec!["a.bakker".to_string()],
        "a refused publish does not touch the reviewers"
    );
    // the caller already approved: a second approval is refused
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", approve, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("already_approved"))
    );
    // a distinct reviewer approves: 200 with the updated detail, still CANARY
    let visser = admin_as("r.visser");
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::json("POST", approve, Some(&visser), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["version_status"], "CANARY");
    assert_eq!(p["reviewers"], json!(["a.bakker", "r.visser"]));
    let last = p["history"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        (
            last["actor"].clone(),
            last["action"].clone(),
            last["ok"].clone()
        ),
        (json!("r.visser"), json!("approve"), json!(true))
    );
    // now the publish goes through
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::json("POST", status, Some(&admin), json!({"to":"PUBLISHED"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["version_status"], "PUBLISHED");
    assert_eq!(p["reviewers"], json!(["a.bakker", "r.visser"]));
    let (_, old) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures/return/versions/1.3.0"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(old["version_status"], "ARCHIVED");
    // a PUBLISHED version is no longer under review
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", approve, Some(&admin_as("j.smit")), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("not_in_review"))
    );
    // ARCHIVED -> CANARY / DRAFT stay refused (roll-back is ARCHIVED -> PUBLISHED only)
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/return/versions/1.3.0/status"),
            Some(&admin),
            json!({"to":"CANARY"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "invalid_transition");
    // one audit row per successful mutation: the approval + the publish (refusals write nothing)
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events WHERE resource_type='procedure' AND resource_id='return@1.4.0-beta'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(n, 2);
}

/// Only a CANARY version can be approved; a DRAFT is refused with nothing written. The service token cannot approve.
#[tokio::test]
async fn approve_is_refused_outside_review() {
    let e = crate::common::env("proc-approve").await;
    let admin = admin_as("a.bakker");
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            new_procedure("approve_me", "1.0.0", "retail:faq:general"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let approve = &crate::common::j("/procedures/approve_me/versions/1.0.0/approve");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", approve, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("not_in_review"))
    );
    assert!(reviewers_of(&e, "approve_me", "1.0.0").await.is_empty());
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/nope/versions/1.0.0/approve"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // promoted to CANARY it can be approved, once per reviewer
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/approve_me/versions/1.0.0/status"),
            Some(&admin),
            json!({"to":"CANARY"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::json("POST", approve, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["reviewers"], json!(["a.bakker"]));
    // one reviewer is not enough to publish, and the refusal leaves the one approval in place
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/approve_me/versions/1.0.0/status"),
            Some(&admin),
            json!({"to":"PUBLISHED"}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("needs_two_reviewers"))
    );
    assert_eq!(
        reviewers_of(&e, "approve_me", "1.0.0").await,
        vec!["a.bakker".to_string()]
    );
    let svc = "svc-test-token";
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::json("POST", approve, Some(svc), json!({})),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn transitions_validate_the_plan_and_reject_unknown_targets() {
    let e = crate::common::env("proc-tr").await;
    let admin = admin_as("a.bakker");
    let body = json!({"name":"broken","version":"0.1.0","context_space":"retail:faq:general",
        "plan":{"phases":[{"steps":[{"worker":"order","action":"get_order","as":"o","args":{}}],"gate":null}]},"steps":[get_order_step()]});
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), body),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    // make the stored plan unrunnable behind the API's back, then try to promote it
    sqlx::query("UPDATE procedures SET plan='{\"phases\":[]}' WHERE name='broken'")
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/broken/versions/0.1.0/status"),
            Some(&admin),
            json!({"to":"CANARY"}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_plan"))
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/broken/versions/0.1.0/status"),
            Some(&admin),
            json!({"to":"BOGUS"}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_status"))
    );
    // DRAFT -> DISCARDED is allowed
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/broken/versions/0.1.0/status"),
            Some(&admin),
            json!({"to":"DISCARDED"}),
        ),
    )
    .await;
    assert_eq!(
        (s, p["version_status"].clone()),
        (StatusCode::OK, json!("DISCARDED"))
    );
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/nope/versions/1.0.0/status"),
            Some(&admin),
            json!({"to":"CANARY"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_draft_with_pddl_steps_and_read_it_back() {
    let e = crate::common::env("proc-new").await;
    let admin = admin_as("a.bakker");
    let body = json!({"name":"delivery_status","version":"2.1.0","context_space":"retail:logistics:delivery-status","pddl_domain":"(define (domain d))",
        "plan":{"phases":[{"steps":[{"worker":"order","action":"get_order","as":"order","args":{"order_id":"$request.order_id"}}],"gate":null}]},
        "steps":[{"step_number":1,"phase":0,"name":"Get order","action_type":"get-order","worker":"order","mcp_tool":"order_get_order","performed_by":"order worker",
                  "preconditions":["(order ?o)"],"postconditions":["(order-known ?o)"],"parameters":{"?order_id":"order_id"},"gate":false}]});
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                &crate::common::j("/procedures"),
                Some(&admin),
                body.clone()
            )
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, p) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures/delivery_status/versions/2.1.0"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(p["version_status"], "DRAFT");
    assert_eq!(p["steps"][0]["postconditions"][0], "(order-known ?o)");
    let (_, l) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/procedures?status=DRAFT"), Some(&admin)),
    )
    .await;
    assert_eq!(l["items"].as_array().unwrap().len(), 1); // every seeded procedure is PUBLISHED or CANARY
    // audit row for the create
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events WHERE action_type='create' AND resource_id='delivery_status@2.1.0'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(n, 1);
    // viewer cannot create
    let viewer = crate::common::user();
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                &crate::common::j("/procedures"),
                Some(&viewer),
                body.clone()
            )
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // bad inputs
    let mut bad = body.clone();
    bad["version"] = json!("1.2");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), bad),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_version"))
    );
    for v in ["1.2.3-", "1234567890.0.0", "1.2.3-a_b", "v1.2.3", "1.2.3 "] {
        let mut bad = body.clone();
        bad["version"] = json!(v);
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), bad),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::BAD_REQUEST, json!("invalid_version")),
            "{v}"
        );
    }
    let mut bad = body.clone();
    bad["version"] = json!("3.0.0");
    bad["plan"] = json!({"phases":[{"steps":[{"worker":"ghost","action":"x","as":"a","args":{}}],"gate":null}]});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), bad),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_plan"))
    );
    let mut bad = body.clone();
    bad["version"] = json!("3.0.0");
    bad["plan"] = json!({"phases":"nope"});
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), bad),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_plan"))
    );
    let mut bad = body.clone();
    bad["version"] = json!("3.0.0");
    bad["context_space"] = json!("retail:nope:nope");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), bad),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("unknown_context_space"))
    );
    // nothing was left behind by the refused creates
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM procedures WHERE name='delivery_status'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(n, 2); // seeded 2.0.0 + the new 2.1.0
}

#[tokio::test]
async fn versions_sort_numerically_not_lexically() {
    let e = crate::common::env("proc-sort").await;
    let admin = admin_as("a.bakker");
    for v in ["10.0.0", "9.0.0", "2.0.1-rc.1"] {
        let body = json!({"name":"delivery_status","version":v,"context_space":"retail:logistics:delivery-status",
            "plan":{"phases":[{"steps":[{"worker":"order","action":"get_order","as":"order","args":{}}],"gate":null}]},"steps":[get_order_step()]});
        let (s, _) = crate::common::send(
            &e.app,
            crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), body),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let (_, l) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures?context_space=retail%3Alogistics%3Adelivery-status"),
            Some(&admin),
        ),
    )
    .await;
    let versions: Vec<&str> = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions, vec!["10.0.0", "9.0.0", "2.0.1-rc.1", "2.0.0"]);
}

#[tokio::test]
async fn service_lookups_by_context_space_and_by_name() {
    let e = crate::common::env("proc-svc").await;
    let svc = "svc-test-token";
    // plan 01 HttpProcedures: by context space, PUBLISHED, plan deserializes as carebus::domain::Plan
    let (s, l) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::get(
                &crate::common::j(
                    "/procedures?context_space=retail%3Aorder-support%3Areturns&status=PUBLISHED",
                ),
                Some(svc),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let items = l["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["version"], "1.3.0");
    let plan: carebus::domain::Plan =
        serde_json::from_value(items[0]["plan"].clone()).expect("plan is a carebus Plan");
    assert_eq!(plan.phases.len(), 3);
    assert_eq!(plan.phases[1].gate.as_deref(), Some("rules"));
    // plan 03 procedures_get_procedure: by name
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::get(
                &crate::common::j("/procedures/return?status=PUBLISHED"),
                Some(svc),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(p["version"], "1.3.0");
    assert_eq!(p["steps"].as_array().unwrap().len(), 5);
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::get(&crate::common::j("/procedures/return"), Some(svc)),
            crate::common::BOOT_TENANT,
        ),
    )
    .await; // default PUBLISHED
    assert_eq!((s, p["version"].clone()), (StatusCode::OK, json!("1.3.0")));
    // every context space has a PUBLISHED plan with work in it (an empty plan would "complete" a case doing nothing)
    for cs in [
        "retail:order-support:warranty",
        "retail:logistics:delivery-status",
        "retail:payments:payment-errors",
        "retail:faq:general",
    ] {
        let (_, l) = crate::common::send(
            &e.app,
            crate::common::with_tenant(
                crate::common::get(
                    &crate::common::j(&format!(
                        "/procedures?context_space={}&status=PUBLISHED",
                        cs.replace(':', "%3A")
                    )),
                    Some(svc),
                ),
                crate::common::BOOT_TENANT,
            ),
        )
        .await;
        let plan: carebus::domain::Plan =
            serde_json::from_value(l["items"][0]["plan"].clone()).expect(cs);
        assert!(!plan.phases.is_empty(), "{cs}");
    }
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::with_tenant(
                crate::common::get(
                    &crate::common::j("/procedures/no_such_procedure?status=PUBLISHED"),
                    Some(svc)
                ),
                crate::common::BOOT_TENANT
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::get(
                &crate::common::j("/procedures/return?status=DRAFT"),
                Some(svc),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("unsupported_status"))
    );
    // the service token cannot change procedures
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::with_tenant(
                crate::common::json(
                    "POST",
                    &crate::common::j("/procedures/return/versions/1.4.0-beta/status"),
                    Some(svc),
                    json!({"to":"DISCARDED"})
                ),
                crate::common::BOOT_TENANT
            )
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

/// The bootstrap context space that runs procedure `name`.
fn space_running(name: &str) -> carebus::spaces::ContextSpace {
    SpaceCatalog::bootstrap()
        .spaces()
        .iter()
        .find(|s| s.procedure == name)
        .unwrap_or_else(|| panic!("unmapped seeded procedure {name}"))
        .clone()
}

/// D7: the seeded PUBLISHED plans are exactly the in-code static plans.
#[tokio::test]
async fn seeded_published_plans_equal_static_plan() {
    let e = crate::common::env("proc-d7").await;
    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT name, context_space, plan FROM procedures WHERE version_status='PUBLISHED' ORDER BY name",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(rows.len(), 5);
    for (name, cs, plan) in rows {
        assert_eq!(cs, space_running(&name).key, "{name}");
        assert_eq!(
            plan,
            serde_json::to_value(static_plan(&name).expect("built-in plan")).unwrap(),
            "{name}"
        );
    }
}

/// Plan 01's real `HttpProcedures` against this router on an ephemeral port.
#[tokio::test]
async fn http_procedures_reads_the_published_plan_from_this_api() {
    let e = crate::common::env("proc-http").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = e.app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let src = carebus::procedures::HttpProcedures::new(
        format!("http://{addr}"),
        Some("svc-test-token".into()),
    )
    .for_scope(&carebus::Scope {
        tenant: crate::common::BOOT_TENANT.parse().unwrap(),
        job: crate::common::BOOT_JOB.into(),
    });
    for (name, version) in [
        ("return", "return@1.3.0"),
        ("duplicate_charge_refund", "duplicate_charge_refund@1.0.2"),
    ] {
        let mut plan = src
            .plan(&space_running(name))
            .await
            .expect("HttpProcedures loads the plan");
        // stamped with the PUBLISHED version it chose; the rest is the stored plan
        assert_eq!(plan.procedure.take().as_deref(), Some(version));
        assert_eq!(
            serde_json::to_value(plan).unwrap(),
            serde_json::to_value(static_plan(name).expect("built-in plan")).unwrap()
        );
    }
    // without a token the api refuses and HttpProcedures reports it
    let anon = carebus::procedures::HttpProcedures::new(format!("http://{addr}"), None);
    assert!(anon.plan(&space_running("return")).await.is_err());
    server.abort();
}

fn new_procedure(name: &str, version: &str, space: &str) -> Value {
    json!({"name":name,"version":version,"context_space":space,"description":"plain words",
        "plan":{"phases":[{"steps":[{"worker":"order","action":"get_order","as":"order","args":{"order_id":"$request.order_id"}}],"gate":null}]},
        "steps":[{"step_number":1,"phase":0,"name":"Get order","action_type":"get-order","worker":"order","mcp_tool":"order_get_order","performed_by":"order worker"}]})
}

#[tokio::test]
async fn create_rejects_secret_shaped_values_and_never_echoes_them() {
    let e = crate::common::env("proc-secret").await;
    let admin = admin_as("a.bakker");
    let count = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM procedures WHERE name='leaky'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap()
    };
    let mut in_description = new_procedure("leaky", "1.0.0", "retail:faq:general");
    in_description["description"] = json!("sk-live-ECHOMARKER0123456789");
    let mut in_plan_args = new_procedure("leaky", "1.0.0", "retail:faq:general");
    in_plan_args["plan"]["phases"][0]["steps"][0]["args"]["api_key"] = json!("ECHOMARKER");
    let mut in_step_params = new_procedure("leaky", "1.0.0", "retail:faq:general");
    in_step_params["steps"][0]["parameters"] =
        json!({"aws": "wJalrXUtnFEMI/K7MDENG/bPxRfiECHOMARKERKEY"});
    for (what, body) in [
        ("description", in_description),
        ("plan args", in_plan_args),
        ("step parameters", in_step_params),
    ] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), body),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {b}");
        assert_eq!(b["error"]["code"], "secret_value_rejected", "{what}");
        assert!(!b.to_string().contains("ECHOMARKER"), "{what}: {b}");
    }
    assert_eq!(count().await, 0, "nothing stored");
    // a type error gets a fixed message: serde's text would echo the value
    let mut wrong_type = new_procedure("leaky", "1.0.0", "retail:faq:general");
    wrong_type["steps"][0]["step_number"] = json!("ECHOMARKER-not-a-number");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            wrong_type,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_body");
    assert!(!b.to_string().contains("ECHOMARKER"), "{b}");
    assert_eq!(count().await, 0);
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE resource_type='procedure'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(n, 0, "a rejected create leaves no audit row");
}

#[tokio::test]
async fn a_second_procedure_name_cannot_be_published_into_an_occupied_space() {
    let e = crate::common::env("proc-space").await;
    let admin = admin_as("a.bakker");
    // one PUBLISHED per space is per origin: a tenant version in a space the base serves shadows the base one
    // (tests/effective.rs), so the tenant's own faq_answer holds the space here
    adopt_as_tenant(&e, "faq_answer").await;
    // seeds hold exactly one PUBLISHED procedure per context space
    let dup: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT context_space FROM procedures WHERE version_status='PUBLISHED' GROUP BY context_space HAVING count(*) > 1) d",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(dup, 0);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            new_procedure("faq_other", "1.0.0", "retail:faq:general"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let url = &crate::common::j("/procedures/faq_other/versions/1.0.0/status");
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", url, Some(&admin), json!({"to":"CANARY"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    sqlx::query("UPDATE procedures SET reviewers='{a.bakker,r.visser}' WHERE name='faq_other'")
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", url, Some(&admin), json!({"to":"PUBLISHED"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "space_already_published");
    let (st, pub_faq): (String, i64) = (
        sqlx::query_scalar("SELECT version_status::text FROM procedures WHERE name='faq_other'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap(),
        sqlx::query_scalar(
            "SELECT count(*) FROM procedures WHERE name='faq_answer' AND version_status='PUBLISHED'",
        )
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap(),
    );
    assert_eq!((st.as_str(), pub_faq), ("CANARY", 1), "nothing changed");
    // the same name still archives-then-publishes within its space
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            new_procedure("faq_answer", "9.0.0", "retail:faq:general"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let url = &crate::common::j("/procedures/faq_answer/versions/9.0.0/status");
    crate::common::send(
        &e.app,
        crate::common::json("POST", url, Some(&admin), json!({"to":"CANARY"})),
    )
    .await;
    sqlx::query("UPDATE procedures SET reviewers='{a.bakker,r.visser}' WHERE name='faq_answer' AND version='9.0.0'")
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", url, Some(&admin), json!({"to":"PUBLISHED"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // and the index itself refuses a second PUBLISHED row in one space
    let raw =
        sqlx::query("UPDATE procedures SET version_status='PUBLISHED' WHERE name='faq_other'")
            .execute(crate::common::pool(&e))
            .await;
    assert!(raw.is_err(), "the partial unique index enforces it");
}

/// Flattened plan steps as (phase, worker, action), the order the step rows must follow.
fn plan_shape(plan: &Value) -> Vec<(i64, String, String)> {
    plan["phases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .flat_map(|(i, ph)| {
            ph["steps"].as_array().unwrap().iter().map(move |st| {
                (
                    i64::try_from(i).unwrap(),
                    st["worker"].as_str().unwrap().to_string(),
                    st["action"].as_str().unwrap().to_string(),
                )
            })
        })
        .collect()
}

fn get_order_step() -> Value {
    json!({"step_number":1,"phase":0,"name":"Get order","action_type":"get-order","worker":"order",
           "mcp_tool":"order_get_order","performed_by":"order worker"})
}

/// The list's `steps` is the step count of THAT version's plan, not the number of PDDL annotation rows. The seeded
/// return@1.4.0-beta canary had a 5-step plan and no procedure_steps rows and listed as `steps: 0`; here a version's
/// rows are removed behind the API's back to prove the count no longer depends on them.
#[tokio::test]
async fn list_counts_the_plan_steps_of_non_published_versions() {
    let e = crate::common::env("proc-list").await;
    let admin = admin_as("a.bakker");
    sqlx::query("DELETE FROM procedure_steps s USING procedures p WHERE p.id=s.procedure_id AND p.name='return' AND p.version='1.4.0-beta'")
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/procedures?status=CANARY"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let beta = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "return" && i["version"] == "1.4.0-beta")
        .expect("return@1.4.0-beta listed");
    assert_eq!(plan_shape(&beta["plan"]).len(), 5);
    assert_eq!(beta["steps"], 5);
    assert_eq!(beta["version_status"], "CANARY");
    assert_eq!(beta["domain"], "customer_care");
}

/// The detail of the seeded canary carries one PDDL step row per plan step (migration 0004 backfilled them), in
/// the plan's phase/worker order -- the Steps tab and the Flow tab describe the same 5 steps.
#[tokio::test]
async fn canary_detail_has_a_step_row_for_every_plan_step() {
    let e = crate::common::env("proc-beta").await;
    let admin = admin_as("a.bakker");
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures/return/versions/1.4.0-beta"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(p["version_status"], "CANARY");
    let rows: Vec<(i64, String)> = p["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|st| {
            (
                st["phase"].as_i64().unwrap(),
                st["worker"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let plan: Vec<(i64, String)> = plan_shape(&p["plan"])
        .into_iter()
        .map(|(ph, w, _)| (ph, w))
        .collect();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows, plan);
    assert_eq!(p["steps"][4]["mcp_tool"], "order_create_rma");
}

/// The write path cannot store a plan without matching step rows any more.
#[tokio::test]
async fn create_refuses_steps_that_do_not_mirror_the_plan() {
    let e = crate::common::env("proc-mirror").await;
    let admin = admin_as("a.bakker");
    let mut no_steps = new_procedure("mirror", "1.0.0", "retail:faq:general");
    no_steps["steps"] = json!([]);
    let mut wrong_worker = new_procedure("mirror", "1.0.0", "retail:faq:general");
    wrong_worker["steps"][0]["worker"] = json!("crm");
    let mut wrong_phase = new_procedure("mirror", "1.0.0", "retail:faq:general");
    wrong_phase["steps"][0]["phase"] = json!(1);
    let mut extra = new_procedure("mirror", "1.0.0", "retail:faq:general");
    let mut second = get_order_step();
    second["step_number"] = json!(2);
    extra["steps"].as_array_mut().unwrap().push(second);
    for (what, body) in [
        ("no steps", no_steps),
        ("wrong worker", wrong_worker),
        ("wrong phase", wrong_phase),
        ("extra step", extra),
    ] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), body),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
        assert_eq!(b["error"]["code"], "steps_do_not_match_plan", "{what}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM procedures WHERE name='mirror'")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing stored");
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            new_procedure("mirror", "1.0.0", "retail:faq:general"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
}

fn history_of(p: &Value) -> Vec<(String, String, bool, String, String)> {
    p["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["actor"].as_str().unwrap().to_string(),
                h["action"].as_str().unwrap().to_string(),
                h["ok"].as_bool().unwrap(),
                h["from"].as_str().unwrap_or("").to_string(),
                h["to"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// Roll-back = ARCHIVED -> PUBLISHED through the ordinary publish path: the current PUBLISHED version is archived in
/// the same transaction (one PUBLISHED per name), the two-reviewer rule still applies, DISCARDED never comes back,
/// and both versions' history records who did it.
#[tokio::test]
async fn rollback_republishes_an_archived_version_and_archives_the_current_one() {
    let e = crate::common::env("proc-rollback").await;
    adopt_as_tenant(&e, "return").await;
    let admin = admin_as("a.bakker");
    let db = crate::common::pool(&e);
    let status = |v: &'static str| async move {
        sqlx::query_scalar::<_, String>(
            "SELECT version_status::text FROM procedures WHERE name='return' AND version=$1",
        )
        .bind(v)
        .fetch_one(db)
        .await
        .unwrap()
    };
    // publish the canary (seed reviewers + one more), which archives 1.3.0
    sqlx::query("UPDATE procedures SET reviewers='{a.bakker,r.visser}' WHERE name='return' AND version='1.4.0-beta'")
        .execute(db)
        .await
        .unwrap();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/return/versions/1.4.0-beta/status"),
            Some(&admin),
            json!({"to":"PUBLISHED"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(status("1.3.0").await, "ARCHIVED");
    // an archived version with fewer than two reviewers is not re-published
    sqlx::query("UPDATE procedures SET reviewers='{}' WHERE name='return' AND version='1.3.0'")
        .execute(db)
        .await
        .unwrap();
    let url = &crate::common::j("/procedures/return/versions/1.3.0/status");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", url, Some(&admin), json!({"to":"PUBLISHED"})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("needs_two_reviewers"))
    );
    assert_eq!(status("1.3.0").await, "ARCHIVED");
    assert_eq!(status("1.4.0-beta").await, "PUBLISHED", "nothing moved");
    // with its two reviewers back, the roll-back goes through
    sqlx::query("UPDATE procedures SET reviewers='{a.bakker,r.visser}' WHERE name='return' AND version='1.3.0'")
        .execute(db)
        .await
        .unwrap();
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::json("POST", url, Some(&admin), json!({"to":"PUBLISHED"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["version_status"], "PUBLISHED");
    assert_eq!(status("1.4.0-beta").await, "ARCHIVED");
    let published: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM procedures WHERE name='return' AND version_status='PUBLISHED'",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(published, 1);
    // the rolled-back-to version's history: archived by the canary's publish, then the re-publish (the refused
    // attempt wrote nothing)
    assert_eq!(
        history_of(&p),
        vec![
            (
                "a.bakker".into(),
                "transition".into(),
                true,
                "PUBLISHED".into(),
                "ARCHIVED".into()
            ),
            (
                "a.bakker".into(),
                "transition".into(),
                true,
                "ARCHIVED".into(),
                "PUBLISHED".into()
            ),
        ]
    );
    assert_eq!(p["history"][0]["replaced_by"], "1.4.0-beta");
    assert!(p["history"][1]["at"].is_string());
    // the displaced version says it was replaced by the roll-back
    let (_, beta) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures/return/versions/1.4.0-beta"),
            Some(&admin),
        ),
    )
    .await;
    let last = beta["history"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        (
            last["from"].clone(),
            last["to"].clone(),
            last["replaced_by"].clone()
        ),
        (json!("PUBLISHED"), json!("ARCHIVED"), json!("1.3.0"))
    );
    // DISCARDED is final: no roll-back to it
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            new_procedure("return", "0.9.0", "retail:order-support:returns"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    for to in ["DISCARDED", "PUBLISHED"] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                &crate::common::j("/procedures/return/versions/0.9.0/status"),
                Some(&admin),
                json!({"to": to}),
            ),
        )
        .await;
        if to == "DISCARDED" {
            assert_eq!(s, StatusCode::OK, "{b}");
        } else {
            assert_eq!(
                (s, b["error"]["code"].clone()),
                (StatusCode::CONFLICT, json!("invalid_transition"))
            );
        }
    }
}

/// A new version's detail starts its history with the create row; seeded versions have none.
#[tokio::test]
async fn detail_history_records_create_and_transitions() {
    let e = crate::common::env("proc-hist").await;
    let admin = admin_as("a.bakker");
    let (s, created) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            new_procedure("hist", "1.0.0", "retail:faq:general"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        history_of(&created),
        vec![(
            "a.bakker".into(),
            "create".into(),
            true,
            String::new(),
            String::new()
        )]
    );
    let (_, p) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures/hist/versions/1.0.0/status"),
            Some(&admin),
            json!({"to":"CANARY"}),
        ),
    )
    .await;
    assert_eq!(
        history_of(&p)[1],
        (
            "a.bakker".into(),
            "transition".into(),
            true,
            "DRAFT".into(),
            "CANARY".into()
        )
    );
    let (_, seeded) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/procedures/faq_answer/versions/2.0.0"),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(seeded["history"], json!([]));
}

/// Insert one recorded case. `procedure` None = a row from before attribution existed.
async fn case(
    db: &sqlx::PgPool,
    id: &str,
    procedure: Option<(&str, &str)>,
    outcome: Option<&str>,
    days_ago: i32,
) {
    sqlx::query(
        "INSERT INTO case_history (case_id, customer_id, request_type, area, context_space, outcome, started_at,
                                   procedure_name, procedure_version)
         VALUES ($1,'C-1','return','order-support','retail:order-support:returns',$2,
                 now() - make_interval(days => $3) - interval '1 minute',$4,$5)",
    )
    .bind(id)
    .bind(outcome)
    .bind(days_ago)
    .bind(procedure.map(|p| p.0))
    .bind(procedure.map(|p| p.1))
    .execute(db)
    .await
    .unwrap();
}

/// runs_7d / success_rate per version: resolved cases attributed to THAT version in the last 7 days; success is
/// completed or not_eligible, failure is failed. Older rows, open cases and unattributed (NULL) rows do not count.
#[tokio::test]
async fn list_reports_runs_and_success_per_version_over_7_days() {
    let e = crate::common::env("proc-runs").await;
    let db = crate::common::pool(&e);
    let stable = Some(("return", "1.3.0"));
    let canary = Some(("return", "1.4.0-beta"));
    case(db, "R1", stable, Some("completed"), 0).await;
    case(db, "R2", stable, Some("completed"), 3).await;
    case(db, "R3", stable, Some("not_eligible"), 6).await;
    case(db, "R4", stable, Some("failed"), 1).await;
    case(db, "R5", stable, Some("failed"), 8).await; // older than 7 days
    case(db, "R6", stable, None, 0).await; // still open
    case(db, "R7", None, Some("failed"), 0).await; // recorded before attribution
    case(db, "R8", canary, Some("failed"), 2).await;
    let admin = admin_as("a.bakker");
    let (s, l) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/procedures"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let by = |name: &str, version: &str| {
        l["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name && p["version"] == version)
            .unwrap_or_else(|| panic!("{name}@{version} listed"))
            .clone()
    };
    let p = by("return", "1.3.0");
    assert_eq!(p["runs_7d"], 4);
    assert_eq!(p["success_rate"], 0.75);
    let c = by("return", "1.4.0-beta");
    assert_eq!(c["runs_7d"], 1);
    assert_eq!(c["success_rate"].as_f64(), Some(0.0), "{c}");
    let w = by("warranty_claim", "1.1.0");
    assert_eq!(w["runs_7d"], 0);
    assert_eq!(
        w["success_rate"],
        Value::Null,
        "no runs is no rate, not 0 %"
    );
}

/// Plan 12 Task 6: `POST /api/procedures` takes an optional `side_effect` per step; it is served on the detail and on
/// the matching plan step of the list. A flag on a step whose tool is not in its plan phase is 400 `invalid_steps`.
#[tokio::test]
async fn create_takes_an_optional_step_side_effect() {
    let e = crate::common::env("proc-side-effect").await;
    let admin = admin_as("a.bakker");
    let mut step = get_order_step();
    step["side_effect"] = json!(true);
    let body = json!({"name":"delivery_status","version":"2.2.0","context_space":"retail:logistics:delivery-status",
        "plan":{"phases":[{"steps":[{"worker":"order","action":"get_order","as":"order","args":{}}],"gate":null}]},
        "steps":[step]});
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!(p["steps"][0]["side_effect"], true);
    let (s, l) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(
                "/procedures?context_space=retail%3Alogistics%3Adelivery-status&status=DRAFT",
            ),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let item = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["version"] == "2.2.0")
        .unwrap();
    assert_eq!(item["plan"]["phases"][0]["steps"][0]["side_effect"], true);
    assert!(item.get("side_effect_steps").is_none());
    // without the key: stored NULL, no key on the detail
    let mut plain = body.clone();
    plain["version"] = json!("2.2.1");
    plain["steps"] = json!([get_order_step()]);
    let (s, p) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/procedures"),
            Some(&admin),
            plain,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert!(p["steps"][0].get("side_effect").is_none());
    // a flag on a tool the plan phase does not call
    let mut bad = body;
    bad["version"] = json!("2.2.2");
    bad["steps"][0]["mcp_tool"] = json!("order_issue_refund");
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/procedures"), Some(&admin), bad),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    assert_eq!(r["error"]["code"], "invalid_steps");
}
