//! RAG eval proxy (`/api/knowledge/eval/...`) over the strata_rag fake (strata_rag PR #21 shapes): who may do what,
//! what reaches strata_rag (tenant, actor, body, content type), which strata_rag codes reach the client, and the
//! web contract fixtures under web/src/lib/api/fixtures/eval/.
use crate::common::fakes::{self, Shared};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};

/// A Keycloak-style subject (a uuid): the one strata_rag records as `created_by` / `started_by`.
const ACTOR: &str = "7f1c9a2e-0000-4000-8000-00000000ac70";

/// `ACTOR` provisioned as a tenant_admin of the bootstrap tenant; its token.
async fn uuid_admin(e: &crate::common::TestEnv) -> String {
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ($1, $2::uuid, 'tenant_admin', 'E. Valuator', 'test')",
    )
    .bind(ACTOR)
    .bind(crate::common::BOOT_TENANT)
    .execute(crate::common::pool(e))
    .await
    .expect("provision the uuid admin");
    crate::common::token_for(ACTOR, &[])
}

fn raw(
    method: &str,
    uri: &str,
    token: &str,
    content_type: Option<&str>,
    body: &str,
) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if let Some(ct) = content_type {
        b = b.header("content-type", ct);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn seen(f: &Shared) -> Vec<fakes::EvalSeen> {
    f.lock().unwrap().eval_seen.clone()
}

const Q: &str = "/api/knowledge/eval/questions";
const RUNS: &str = "/api/knowledge/eval/runs";

#[tokio::test]
async fn questions_crud_forwards_the_actor_and_keeps_strata_codes() {
    let (e, fake) = fakes::sources_env("eval-crud").await;
    let admin = uuid_admin(&e).await;
    let viewer = crate::common::user();
    let body = json!({"query": "How long does a refund take?", "expected_document": "Returns policy",
                      "expected_leaf": "help-centre"});

    // a tenant_user reads but never writes; the refusal never reaches strata_rag
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", Q, Some(&viewer), body.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(seen(&fake).is_empty());

    let (s, created) = crate::common::send(
        &e.app,
        crate::common::json("POST", Q, Some(&admin), body.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    assert_eq!(created["query"], "How long does a refund take?");
    assert_eq!(
        created["created_by"], ACTOR,
        "X-Actor-Id is the caller: {created}"
    );
    let id = created["id"].as_str().unwrap().to_string();
    let first = &seen(&fake)[0];
    assert_eq!(
        (first.method.as_str(), first.path_and_query.as_str()),
        ("POST", "/api/eval/questions")
    );
    assert_eq!(first.actor.as_deref(), Some(ACTOR));
    assert_eq!(serde_json::from_str::<Value>(&first.body).unwrap(), body);

    // a subject that is not a uuid sends no actor (never a made-up one)
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            Q,
            Some(&crate::common::admin()),
            json!({"query": "Can I return a gift?", "expected_text": "gift receipt"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["created_by"], Value::Null);
    assert_eq!(seen(&fake).last().unwrap().actor, None);

    let (s, list) = crate::common::send(&e.app, crate::common::get(Q, Some(&viewer))).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list["next"], Value::Null);
    let queries: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["query"].as_str().unwrap())
        .collect();
    assert_eq!(
        queries,
        ["How long does a refund take?", "Can I return a gift?"]
    );

    // strata_rag's own codes reach the client
    for (b, status, code) in [
        (
            json!({"query": "how long does a REFUND take?", "expected_text": "x"}),
            StatusCode::CONFLICT,
            "question_exists",
        ),
        (
            json!({"query": "No expectation"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
        ),
    ] {
        let (s, r) =
            crate::common::send(&e.app, crate::common::json("POST", Q, Some(&admin), b)).await;
        assert_eq!(
            (s, r["error"]["code"].as_str()),
            (status, Some(code)),
            "{r}"
        );
    }
    // not an object: ours, never sent, not a write attempt
    let n = seen(&fake).len();
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json("POST", Q, Some(&admin), json!(["not", "an", "object"])),
    )
    .await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_body"))
    );
    assert_eq!(seen(&fake).len(), n);

    // PATCH: omitted keeps, null clears
    let uri = format!("{Q}/{id}");
    let (s, upd) = crate::common::send(
        &e.app,
        crate::common::json(
            "PATCH",
            &uri,
            Some(&admin),
            json!({"expected_leaf": null, "expected_text": "5 working days"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{upd}");
    assert_eq!(upd["expected_leaf"], Value::Null);
    assert_eq!(upd["expected_document"], "Returns policy");
    assert_eq!(upd["expected_text"], "5 working days");
    let last = seen(&fake).last().unwrap().clone();
    assert_eq!(
        (last.method.as_str(), last.path_and_query.as_str()),
        ("PATCH", format!("/api/eval/questions/{id}").as_str())
    );

    let del = |t: &str| raw("DELETE", &uri, t, None, "");
    let (s, _) = crate::common::send(&e.app, del(&viewer)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = crate::common::send(&e.app, del(&admin)).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = crate::common::send(&e.app, del(&admin)).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    assert_eq!(fake.lock().unwrap().eval_questions.len(), 1);

    // the cap is strata_rag's: too_many_questions as itself
    fake.lock().unwrap().eval_cap = Some(1);
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            Q,
            Some(&admin),
            json!({"query": "One more", "expected_text": "x"}),
        ),
    )
    .await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("too_many_questions"))
    );

    // every write is audited, failures with our code only
    let audit: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT action_type, resource_id, status FROM audit_events WHERE resource_type='knowledge_eval_question' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    let actions: Vec<(&str, &str)> = audit
        .iter()
        .map(|(a, _, st)| (a.as_str(), st.as_str()))
        .collect();
    assert_eq!(
        actions,
        [
            ("create", "success"),
            ("create", "success"),
            ("create", "failure"),
            ("create", "failure"),
            ("update", "success"),
            ("delete", "success"),
            ("delete", "failure"),
            ("create", "failure"),
        ]
    );
    assert_eq!(audit[0].1, id);
    let codes: Vec<Value> = sqlx::query_scalar(
        "SELECT details->'error_code' FROM audit_events WHERE resource_type='knowledge_eval_question' AND status='failure' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        codes,
        [
            json!("question_exists"),
            json!("invalid_input"),
            json!("not_found"),
            json!("too_many_questions")
        ]
    );
}

#[tokio::test]
async fn import_forwards_csv_or_json_as_sent_all_or_nothing() {
    let (e, fake) = fakes::sources_env("eval-import").await;
    let admin = uuid_admin(&e).await;
    let uri = format!("{Q}/import");
    let csv = "query,expected_document,expected_document_ids\nWhere is my parcel?,Delivery FAQ,\nCan I pay later?,,7f1c9a2e-0000-4000-8000-0000000000d1;7f1c9a2e-0000-4000-8000-0000000000d2\n";

    let (s, _) = crate::common::send(
        &e.app,
        raw("POST", &uri, &crate::common::user(), Some("text/csv"), csv),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, b) = crate::common::send(
        &e.app,
        raw("POST", &uri, &admin, Some("text/csv; charset=utf-8"), csv),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b, json!({"added": 2, "total": 2}));
    let sent = seen(&fake).last().unwrap().clone();
    assert_eq!(sent.path_and_query, "/api/eval/questions/import");
    assert_eq!(sent.content_type.as_deref(), Some("text/csv"));
    assert_eq!(sent.body, csv, "the CSV reaches strata_rag byte for byte");
    assert_eq!(sent.actor.as_deref(), Some(ACTOR));
    let stored = fake.lock().unwrap().eval_questions.clone();
    assert_eq!(
        stored[1]["expected_document_ids"].as_array().unwrap().len(),
        2
    );

    // a bad row imports nothing, and strata_rag's line list reaches the client
    let bad = "query,expected_text\nGood one,found\n,orphan expectation\n";
    let (s, r) =
        crate::common::send(&e.app, raw("POST", &uri, &admin, Some("text/csv"), bad)).await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_input"))
    );
    assert!(
        r["error"]["message"].as_str().unwrap().contains("line 3"),
        "{r}"
    );
    assert_eq!(fake.lock().unwrap().eval_questions.len(), 2);

    // a query already stored: question_exists, nothing added
    let dup = "query,expected_text\nwhere is my PARCEL?,x\n";
    let (s, r) = crate::common::send(&e.app, raw("POST", &uri, &admin, None, dup)).await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("question_exists")),
        "{r}"
    );
    assert_eq!(
        seen(&fake).last().unwrap().content_type.as_deref(),
        Some("text/csv"),
        "no content type is CSV"
    );

    // JSON {questions}
    let j =
        json!({"questions": [{"query": "Do you ship abroad?", "expected_text": "EU countries"}]})
            .to_string();
    let (s, b) = crate::common::send(
        &e.app,
        raw("POST", &uri, &admin, Some("application/json"), &j),
    )
    .await;
    assert_eq!(
        (s, b.clone()),
        (StatusCode::CREATED, json!({"added": 1, "total": 3})),
        "{b}"
    );
    assert_eq!(
        seen(&fake).last().unwrap().content_type.as_deref(),
        Some("application/json")
    );

    // refused before strata_rag: another media type, an empty body
    let before = seen(&fake).len();
    let (s, r) = crate::common::send(
        &e.app,
        raw("POST", &uri, &admin, Some("application/xml"), "<q/>"),
    )
    .await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Some("unsupported_format")
        )
    );
    let (s, r) = crate::common::send(&e.app, raw("POST", &uri, &admin, Some("text/csv"), "")).await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_body"))
    );
    assert_eq!(seen(&fake).len(), before);

    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE resource_type='knowledge_eval_question' AND action_type='import' AND status='success' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        details,
        [
            json!({"format": "text/csv", "added": 2, "total": 2}),
            json!({"format": "application/json", "added": 1, "total": 3})
        ]
    );
}

#[tokio::test]
async fn a_run_scores_the_tenant_questions_and_is_listed_and_read_back() {
    let (e, fake) = fakes::sources_env("eval-run").await;
    let admin = uuid_admin(&e).await;
    let viewer = crate::common::user();
    let run = |t: &str, b: Value| crate::common::json("POST", RUNS, Some(t), b);

    // no questions yet: strata_rag's eval_not_ready as itself
    let (s, r) = crate::common::send(&e.app, run(&admin, json!({}))).await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("eval_not_ready")),
        "{r}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&seen(&fake)[0].body).unwrap(),
        json!({"source": "tenant"})
    );

    for (q, leaf) in [
        ("How long does a refund take?", "help-centre"),
        ("Where is my parcel?", "customers"),
    ] {
        let (s, b) = crate::common::send(
            &e.app,
            crate::common::json(
                "POST",
                Q,
                Some(&admin),
                json!({"query": q, "expected_text": "x", "expected_leaf": leaf}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }

    let (s, _) = crate::common::send(&e.app, run(&viewer, json!({}))).await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "running an eval costs laya calls: tenant_admin only"
    );

    // bad options are ours, and nothing reaches strata_rag
    let before = seen(&fake).len();
    for (b, code) in [
        (json!({"modes": ["embed", "bm25"]}), "invalid_modes"),
        (json!({"modes": []}), "invalid_modes"),
        (json!({"beam": 0}), "invalid_beam"),
        (json!({"source": "builtin"}), "invalid_body"),
        (json!({"questions": [{"query": "inline"}]}), "invalid_body"),
        (json!({"store_id": "not-a-uuid"}), "invalid_body"),
    ] {
        let (s, r) = crate::common::send(&e.app, run(&admin, b.clone())).await;
        assert_eq!(
            (s, r["error"]["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some(code)),
            "{b}: {r}"
        );
    }
    assert_eq!(seen(&fake).len(), before);

    let (s, started) = crate::common::send(
        &e.app,
        run(
            &admin,
            json!({"store_id": fakes::ROUTER, "modes": ["embed", "flat"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{started}");
    let sent = seen(&fake).last().unwrap().clone();
    assert_eq!(sent.path_and_query, "/api/eval");
    assert_eq!(sent.actor.as_deref(), Some(ACTOR));
    assert_eq!(
        serde_json::from_str::<Value>(&sent.body).unwrap(),
        json!({"source": "tenant", "root_id": fakes::ROUTER, "modes": ["embed", "flat"]})
    );
    // async: the run summary, running, nothing scored yet
    assert_eq!(started["status"], "running");
    assert_eq!(started["progress"], json!({"done": 0, "total": 4}));
    assert_eq!(started["question_count"], 2);
    assert_eq!(
        (started["reports"].clone(), started["verdict"].clone()),
        (Value::Null, Value::Null)
    );
    assert!(started.get("results").is_none());
    assert_eq!(started["started_by"], ACTOR);
    assert_eq!(started["root_id"], fakes::ROUTER);
    let run_id = started["id"].as_str().unwrap().to_string();
    let poll = |id: &str| crate::common::get(&format!("{RUNS}/{id}"), Some(&viewer));
    let (s, started) = crate::common::send(&e.app, poll(&run_id)).await;
    assert_eq!(s, StatusCode::OK, "{started}");
    assert_eq!(started["status"], "completed");
    assert_eq!(started["error"], Value::Null);
    assert_eq!(started["progress"], json!({"done": 4, "total": 4}));
    // strata_rag's defaults where none were given
    assert_eq!(
        (started["beam"].clone(), started["min_score"].clone()),
        (json!(3), json!(0.35))
    );
    let recall: Vec<(String, f64, f64)> = started["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["mode"].as_str().unwrap().to_string(),
                r["recall_at_1"].as_f64().unwrap(),
                r["recall_at_10"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        recall,
        [
            ("embed".to_string(), 0.5, 1.0),
            ("flat".to_string(), 0.0, 0.0)
        ]
    );
    assert_eq!(
        started["results"].as_array().unwrap().len(),
        4,
        "2 questions x 2 modes"
    );

    // only these questions, in this order
    let qid = fake.lock().unwrap().eval_questions[1]["id"].clone();
    let (s, b) = crate::common::send(
        &e.app,
        run(
            &admin,
            json!({"question_ids": [qid], "modes": ["hybrid"], "beam": 4, "min_score": 0.2}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["question_count"], 1);
    assert_eq!(b["beam"], 4);
    let second = b["id"].as_str().unwrap().to_string();

    // one eval per tenant at a time: strata_rag's eval_running while the second is still running
    let (s, r) = crate::common::send(&e.app, run(&admin, json!({}))).await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("eval_running")),
        "{r}"
    );
    let (_, b) = crate::common::send(&e.app, poll(&second)).await;
    assert_eq!(b["status"], "completed", "{b}");
    assert_eq!(b["questions"].as_array().unwrap().len(), 1);

    // strata_rag's refusals as themselves
    let (s, r) = crate::common::send(
        &e.app,
        run(
            &admin,
            json!({"root_id": "7f1c9a2e-0000-4000-8000-0000000000ff"}),
        ),
    )
    .await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found")),
        "{r}"
    );
    // a generic strata_rag conflict is not "an eval is running"
    fake.lock().unwrap().eval_conflict = true;
    let (s, r) = crate::common::send(&e.app, run(&admin, json!({}))).await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("conflict")),
        "{r}"
    );
    fake.lock().unwrap().eval_conflict = false;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            Q,
            Some(&admin),
            json!({"query": "Unknown leaf", "expected_text": "x", "expected_leaf": "warranty"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, r) = crate::common::send(&e.app, run(&admin, json!({}))).await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("eval_not_ready"))
    );
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown leaf"),
        "names the question: {r}"
    );

    // a member lists runs (newest first, no results) and reads one with results
    let (s, list) = crate::common::send(
        &e.app,
        crate::common::get(&format!("{RUNS}?limit=1"), Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["modes"], json!(["hybrid"]), "newest first");
    assert!(list["items"][0].get("results").is_none());
    assert!(
        list["items"][0].get("questions").is_none(),
        "a listed run is a summary"
    );
    assert_eq!(list["items"][0]["question_count"], 1);
    let next = list["next"].as_str().expect("a second page").to_string();
    assert_eq!(
        seen(&fake).last().unwrap().path_and_query,
        "/api/eval/runs?limit=1"
    );
    let (s, page2) = crate::common::send(
        &e.app,
        crate::common::get(&format!("{RUNS}?limit=1&before={next}"), Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page2["items"][0]["id"], run_id.as_str());
    assert_eq!(page2["next"], Value::Null);
    assert_eq!(
        seen(&fake).last().unwrap().path_and_query,
        format!("/api/eval/runs?limit=1&cursor={next}")
    );
    // limit is clamped to strata_rag's 100
    crate::common::send(
        &e.app,
        crate::common::get(&format!("{RUNS}?limit=5000"), Some(&viewer)),
    )
    .await;
    assert_eq!(
        seen(&fake).last().unwrap().path_and_query,
        "/api/eval/runs?limit=100"
    );

    let (s, one) = crate::common::send(
        &e.app,
        crate::common::get(&format!("{RUNS}/{run_id}"), Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{one}");
    let ranks: Vec<(String, Value)> = one["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["mode"].as_str().unwrap().to_string(), r["rank"].clone()))
        .collect();
    assert_eq!(
        ranks,
        [
            ("embed".into(), json!(1)),
            ("embed".into(), json!(2)),
            ("flat".into(), Value::Null),
            ("flat".into(), Value::Null)
        ]
    );
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::get(
            &format!("{RUNS}/7f1c9a2e-0000-4000-8000-0000000000ff"),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );

    let runs: Vec<(String, String)> = sqlx::query_as(
        "SELECT resource_id, status FROM audit_events WHERE resource_type='knowledge_eval_run' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    let statuses: Vec<&str> = runs.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(
        statuses,
        [
            "failure", "success", "success", "failure", "failure", "failure", "failure"
        ]
    );
    assert_eq!(runs[1].0, run_id);
}

#[tokio::test]
async fn strata_rag_down_or_unprovisioned_is_never_an_empty_list() {
    let (e, fake) = fakes::sources_env("eval-down").await;
    fake.lock().unwrap().rag_down = true;
    for uri in [Q, RUNS] {
        let (s, r) = crate::common::send(
            &e.app,
            crate::common::get(uri, Some(&crate::common::user())),
        )
        .await;
        assert_eq!(
            (s, r["error"]["code"].as_str()),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Some("upstream_unavailable")
            ),
            "{uri}"
        );
    }
    fake.lock().unwrap().rag_down = false;

    // a tenant the strata token is not pinned to: refused before anything is sent
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
    let before = seen(&fake).len();
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            Q,
            Some(&acme),
            json!({"query": "x", "expected_text": "y"}),
        ),
    )
    .await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (
            StatusCode::CONFLICT,
            Some("strata_not_provisioned_for_tenant")
        ),
        "{r}"
    );
    for uri in [Q, RUNS] {
        let (s, r) = crate::common::send(&e.app, crate::common::get(uri, Some(&acme))).await;
        assert_eq!(
            (s, r["error"]["code"].as_str()),
            (
                StatusCode::CONFLICT,
                Some("strata_not_provisioned_for_tenant")
            ),
            "{uri}"
        );
    }
    assert_eq!(
        seen(&fake).len(),
        before,
        "nothing reaches strata_rag for a tenant its token is not pinned to"
    );

    let (s, _) = crate::common::send(&e.app, crate::common::get(Q, None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

/// The web contract: one real response per eval route, shape-checked against web/src/lib/api/fixtures/eval/*.json
/// (`UPDATE_FIXTURES=1` rewrites them). shapes.ts `EVAL_SHAPES` holds the web's key maps of the same files.
#[tokio::test]
async fn eval_contract_fixtures() {
    let (e, _fake) = fakes::sources_env("eval-fixtures").await;
    let admin = uuid_admin(&e).await;
    let (s, question) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            Q,
            Some(&admin),
            json!({"query": "How long does a refund take?", "expected_document": "Returns policy",
                   "expected_leaf": "help-centre", "expected_document_ids": ["7f1c9a2e-0000-4000-8000-0000000000d1"],
                   "expected_chunk_ids": ["7f1c9a2e-0000-4000-8000-0000000000c1"], "expected_text": "within 5 working days"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{question}");
    crate::common::fixture("eval/question", &question);
    let (s, import) = crate::common::send(
        &e.app,
        raw(
            "POST",
            &format!("{Q}/import"),
            &admin,
            Some("text/csv"),
            "query,expected_text\nWhere is my parcel?,tracking link\n",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{import}");
    crate::common::fixture("eval/import", &import);
    let (s, questions) = crate::common::send(&e.app, crate::common::get(Q, Some(&admin))).await;
    assert_eq!(s, StatusCode::OK);
    crate::common::fixture("eval/questions", &questions);
    let (s, started) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            RUNS,
            Some(&admin),
            json!({"store_id": fakes::ROUTER, "modes": ["embed", "hybrid"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{started}");
    crate::common::fixture("eval/run_started", &started);
    let id = started["id"].as_str().unwrap();
    let (s, run) = crate::common::send(
        &e.app,
        crate::common::get(&format!("{RUNS}/{id}"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(run["status"], "completed");
    crate::common::fixture("eval/run", &run);
    let (s, runs) = crate::common::send(&e.app, crate::common::get(RUNS, Some(&admin))).await;
    assert_eq!(s, StatusCode::OK);
    crate::common::fixture("eval/runs", &runs);
}

/// A run strata_rag could not finish is `failed` with its `error`, read on the poll; a running run is listed.
#[tokio::test]
async fn a_failed_run_carries_its_error_and_a_running_run_is_listed() {
    let (e, fake) = fakes::sources_env("eval-run-failed").await;
    let admin = uuid_admin(&e).await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            Q,
            Some(&admin),
            json!({"query": "Where is my parcel?", "expected_text": "tracking"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, started) = crate::common::send(
        &e.app,
        crate::common::json("POST", RUNS, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{started}");
    let id = started["id"].as_str().unwrap().to_string();
    let (_, list) = crate::common::send(&e.app, crate::common::get(RUNS, Some(&admin))).await;
    assert_eq!(
        (
            list["items"][0]["id"].as_str(),
            list["items"][0]["status"].as_str()
        ),
        (Some(id.as_str()), Some("running")),
        "{list}"
    );
    assert_eq!(
        list["items"][0]["modes"],
        json!(["embed", "hybrid", "flat"])
    );
    fake.lock().unwrap().eval_fail = Some("laya is unavailable".into());
    let (s, run) = crate::common::send(
        &e.app,
        crate::common::get(&format!("{RUNS}/{id}"), Some(&admin)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{run}");
    assert_eq!(
        (
            run["status"].as_str(),
            run["error"].as_str(),
            run["verdict"].clone()
        ),
        (Some("failed"), Some("laya is unavailable"), Value::Null)
    );
}

/// strata_rag refuses an empty PATCH (422 `invalid_input`); it reaches the client as itself.
#[tokio::test]
async fn an_empty_patch_is_strata_rags_422() {
    let (e, _fake) = fakes::sources_env("eval-empty-patch").await;
    let admin = uuid_admin(&e).await;
    let (s, q) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            Q,
            Some(&admin),
            json!({"query": "Where is my parcel?", "expected_text": "tracking"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{q}");
    let id = q["id"].as_str().unwrap();
    let (s, r) = crate::common::send(
        &e.app,
        crate::common::json("PATCH", &format!("{Q}/{id}"), Some(&admin), json!({})),
    )
    .await;
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_input")),
        "{r}"
    );
}
