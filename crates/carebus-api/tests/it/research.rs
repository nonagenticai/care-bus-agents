//! Plan 18: `/api/jobs/{job}/research-runs` (create, list, read, cancel) and migration 0020's claim function.
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{admin, j, user};

async fn post(
    e: &crate::common::TestEnv,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json("POST", &j(path), Some(token), body),
    )
    .await
}

async fn get(e: &crate::common::TestEnv, path: &str, token: &str) -> (StatusCode, Value) {
    crate::common::send(&e.app, crate::common::get(&j(path), Some(token))).await
}

async fn create(e: &crate::common::TestEnv, body: Value) -> (StatusCode, Value) {
    post(e, "/research-runs", &user(), body).await
}

/// A leaf knowledge store of the bootstrap job bound to a fresh strata store id; returns that id.
async fn bound_store(e: &crate::common::TestEnv, key: &str) -> String {
    let sid = Uuid::new_v4().to_string();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &j(&format!("/knowledge-stores/{key}")),
            Some(&admin()),
            json!({"label": "Firm docs", "kind": "leaf", "areas": ["general"], "strata_store_id": sid}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    sid
}

/// A legal matter led by `u-user`, with `sid` attached and `u-admin` added as a member (migration 0026: a legal run
/// needs a matter, and only members see its runs); returns the matter id.
async fn matter_with_store(e: &crate::common::TestEnv, sid: &str) -> String {
    let (s, m) = post(e, "/legal-matters", &user(), json!({"name": "Umowa najmu"})).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    let id = m["id"].as_str().unwrap().to_string();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &j(&format!("/legal-matters/{id}/stores/{sid}")),
            Some(&user()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "PUT",
            &j(&format!("/legal-matters/{id}/members/u-admin")),
            Some(&user()),
            json!({"role": "member"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    id
}

#[tokio::test]
async fn create_list_get_cancel_round_trip() {
    let e = crate::common::env("research-crud").await;
    let sid = bound_store(&e, "firm-docs").await;
    let matter = matter_with_store(&e, &sid).await;
    let (s, run) = create(
        &e,
        json!({"profile": "legal", "question": "  Czy umowa wymaga formy pisemnej?  ", "matter_id": matter,
               "store_ids": [sid.to_uppercase(), sid], "budget": {"max_steps": 20, "max_wall_s": 600}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{run}");
    assert_eq!(run["status"], "queued");
    assert_eq!(run["profile"], "legal");
    assert_eq!(run["matter_id"], json!(matter));
    assert_eq!(run["question"], "Czy umowa wymaga formy pisemnej?");
    assert_eq!(run["store_ids"], json!([sid]));
    assert_eq!(run["budget"], json!({"max_steps": 20, "max_wall_s": 600}));
    assert_eq!(run["created_by"], "u-user");
    assert_eq!(run["cancel_requested"], false);
    assert_eq!(run["attempts"], 0);
    let id = run["id"].as_str().unwrap().to_string();

    let long = "x".repeat(300);
    let (s, second) = create(&e, json!({"profile": "research", "question": long})).await;
    assert_eq!(s, StatusCode::CREATED, "{second}");
    assert_eq!(second["budget"], json!({}));

    // newest first, questions cut to 200 characters, paged by cursor
    let (s, page) = get(&e, "/research-runs?limit=1", &admin()).await;
    assert_eq!(s, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["id"], second["id"]);
    assert_eq!(
        page["items"][0]["question"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        200
    );
    let next = page["next"].as_str().expect("a full page has a cursor");
    let (s, rest) = get(
        &e,
        &format!("/research-runs?limit=1&before={}", urlencode(next)),
        &admin(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{rest}");
    assert_eq!(rest["items"][0]["id"], json!(id));
    assert_eq!(rest["items"][0]["steps_used"], 0);
    let (_, queued) = get(&e, "/research-runs?status=cancelled", &user()).await;
    assert_eq!(queued["items"], json!([]));

    // steps, claims and citations as the runner and verifier write them
    let owner = crate::common::pool(&e);
    let rid: Uuid = id.parse().unwrap();
    sqlx::query(
        "INSERT INTO research_steps (run_id, ordinal, kind, request, response, status, duration_ms) VALUES
           ($1, 0, 'model', '{\"content\": \"plan\"}', '{\"finish_reason\": \"tool_calls\"}', 'ok', 900),
           ($1, 1, 'tool', '{\"tool_call_id\": \"c1\", \"name\": \"legal_eli_get_act\", \"arguments\": {}}',
                           '{\"ok\": true, \"text\": \"Art. 74 KC\", \"truncated\": false}', 'ok', 120)",
    )
    .bind(rid)
    .execute(owner)
    .await
    .unwrap();
    let cid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO research_citations (run_id, id, source_kind, locator, quote, step_ordinal, verify_status)
         VALUES ($1, $2, 'eli', 'DU/1964/93', 'Art. 74 KC', 1, 'verified')",
    )
    .bind(rid)
    .bind(cid)
    .execute(owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO research_claims (run_id, id, ordinal, text, citation_ids, status)
         VALUES ($1, $2, 0, 'Forma pisemna ad probationem.', ARRAY[$3]::uuid[], 'supported')",
    )
    .bind(rid)
    .bind(Uuid::new_v4())
    .bind(cid)
    .execute(owner)
    .await
    .unwrap();

    let (s, d) = get(&e, &format!("/research-runs/{id}"), &user()).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["run"]["id"], json!(id));
    assert_eq!(d["steps"].as_array().unwrap().len(), 2);
    assert_eq!(d["steps"][0]["tool"], Value::Null);
    assert_eq!(d["steps"][1]["tool"], "legal_eli_get_act");
    assert_eq!(d["steps"][1]["duration_ms"], 120);
    assert!(d["steps"][1].get("request").is_none(), "{d}");
    assert!(d["steps"][1].get("response").is_none(), "{d}");
    assert_eq!(d["claims"][0]["citation_ids"], json!([cid]));
    assert_eq!(d["claims"][0]["status"], "supported");
    assert_eq!(d["citations"][0]["locator"], "DU/1964/93");
    assert_eq!(d["citations"][0]["verify_status"], "verified");
    let (s, full) = get(
        &e,
        &format!("/research-runs/{id}?include=steps_full"),
        &user(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{full}");
    assert_eq!(full["steps"][1]["response"]["text"], "Art. 74 KC");
    assert_eq!(full["steps"][1]["request"]["name"], "legal_eli_get_act");
    let (s, b) = get(&e, &format!("/research-runs/{id}?include=all"), &user()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");

    // a queued run is cancelled at once; a finished one is 409
    let (s, c) = post(
        &e,
        &format!("/research-runs/{id}/cancel"),
        &user(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["status"], "cancelled");
    assert_eq!(c["cancel_requested"], true);
    assert!(c["finished_at"].is_string());
    let (s, b) = post(
        &e,
        &format!("/research-runs/{id}/cancel"),
        &user(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "run_finished");

    // a running run only gets the flag; the runner stops it
    let sid2 = second["id"].as_str().unwrap();
    sqlx::query("UPDATE research_runs SET status = 'running' WHERE id = $1::uuid")
        .bind(sid2)
        .execute(owner)
        .await
        .unwrap();
    let (s, c) = post(
        &e,
        &format!("/research-runs/{sid2}/cancel"),
        &admin(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["status"], "running");
    assert_eq!(c["cancel_requested"], true);
    assert_eq!(c["finished_at"], Value::Null);

    let audit = crate::common::audit_text(&e).await;
    assert!(audit.contains("research_run"), "{audit}");

    let (s, _) = get(&e, &format!("/research-runs/{}", Uuid::new_v4()), &user()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = get(&e, "/research-runs/not-a-uuid", &user()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = crate::common::send(&e.app, crate::common::get(&j("/research-runs"), None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

fn urlencode(s: &str) -> String {
    s.replace(':', "%3A")
        .replace('|', "%7C")
        .replace('+', "%2B")
}

#[tokio::test]
async fn invalid_input_is_refused() {
    let e = crate::common::env("research-validation").await;
    let cases = [
        (
            json!({"profile": "medical", "question": "q"}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "   "}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "x".repeat(8001)}),
            "invalid_body",
        ),
        (json!({"profile": "research"}), "invalid_body"),
        (
            json!({"profile": "research", "question": "q", "extra": 1}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "q", "store_ids": ["nope"]}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "q",
                   "store_ids": (0..21).map(|_| Uuid::new_v4().to_string()).collect::<Vec<_>>()}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "q", "budget": {"max_steps": 0}}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "q", "budget": {"max_wall_s": 21601}}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "q", "budget": {"max_tokens": 999}}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "q", "budget": {"max_tool_calls": 401}}),
            "invalid_body",
        ),
        (
            json!({"profile": "research", "question": "q", "budget": {"max_loops": 3}}),
            "invalid_body",
        ),
    ];
    for (body, code) in cases {
        let (s, b) = create(&e, body.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
        assert_eq!(b["error"]["code"], code, "{body} -> {b}");
    }
    // a well-formed store id that is not bound to this job's knowledge stores
    let (s, b) = create(
        &e,
        json!({"profile": "research", "question": "q", "store_ids": [Uuid::new_v4()]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_store");
    // the bounds themselves are accepted
    let (s, b) = create(
        &e,
        json!({"profile": "research", "question": "x".repeat(8000),
               "budget": {"max_steps": 200, "max_tool_calls": 400, "max_wall_s": 21600, "max_tokens": 5000000}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = get(&e, "/research-runs?status=bogus", &user()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = get(&e, "/research-runs?before=garbage", &user()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "invalid_cursor");
    let (_, list) = get(&e, "/research-runs", &user()).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1, "{list}");
}

#[tokio::test]
async fn another_tenant_cannot_see_or_cancel_a_run() {
    let e = crate::common::env("research-isolation").await;
    let owner = crate::common::pool(&e);
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    crate::common::insert_job(owner, other, crate::common::BOOT_JOB).await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-other', $1, 'tenant_admin', 'O. Ther', 'test')",
    )
    .bind(other)
    .execute(owner)
    .await
    .unwrap();
    let theirs = crate::common::token_for("u-other", &[]);

    let (s, run) = create(&e, json!({"profile": "research", "question": "q"})).await;
    assert_eq!(s, StatusCode::CREATED, "{run}");
    let id = run["id"].as_str().unwrap();

    let (s, b) = get(&e, &format!("/research-runs/{id}"), &theirs).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, b) = post(
        &e,
        &format!("/research-runs/{id}/cancel"),
        &theirs,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, list) = get(&e, "/research-runs", &theirs).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list["items"], json!([]));
    // and their own run is invisible to the bootstrap tenant
    let (s, mine) = post(
        &e,
        "/research-runs",
        &theirs,
        json!({"profile": "research", "question": "q"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{mine}");
    let (_, list) = get(&e, "/research-runs", &user()).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["id"], json!(id));
    // still queued: the failed cancel changed nothing
    let (_, d) = get(&e, &format!("/research-runs/{id}"), &user()).await;
    assert_eq!(d["run"]["status"], "queued");
    assert_eq!(d["run"]["cancel_requested"], false);
}

#[tokio::test]
async fn the_runner_claims_a_queued_run_once() {
    let e = crate::common::env("research-claim").await;
    let app = crate::common::app_pool(&e);
    let (s, run) = create(&e, json!({"profile": "research", "question": "q"})).await;
    assert_eq!(s, StatusCode::CREATED, "{run}");
    let id: Uuid = run["id"].as_str().unwrap().parse().unwrap();

    // the runtime role, no tenant GUCs: the definer function is its one cross-tenant read+write
    let claimed: Vec<(Uuid, Uuid, Uuid)> =
        sqlx::query_as("SELECT tenant_id, job_id, id FROM carebus_research_claim('t', 60)")
            .fetch_all(app)
            .await
            .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].0,
        crate::common::BOOT_TENANT.parse::<Uuid>().unwrap()
    );
    assert_eq!(claimed[0].2, id);
    let (status, owner, started): (String, Option<String>, bool) = sqlx::query_as(
        "SELECT status, lease_owner, started_at IS NOT NULL FROM research_runs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), owner.as_deref(), started),
        ("running", Some("t"), true)
    );

    // leased: nothing left to claim
    let again: Vec<(Uuid, Uuid, Uuid)> =
        sqlx::query_as("SELECT tenant_id, job_id, id FROM carebus_research_claim('t2', 60)")
            .fetch_all(app)
            .await
            .unwrap();
    assert!(again.is_empty(), "{again:?}");

    // an expired lease (a crashed runner) is claimable again, without going back to queued
    sqlx::query("UPDATE research_runs SET lease_until = now() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    let back: Vec<(Uuid, Uuid, Uuid)> =
        sqlx::query_as("SELECT tenant_id, job_id, id FROM carebus_research_claim('t3', 60)")
            .fetch_all(app)
            .await
            .unwrap();
    assert_eq!(back.len(), 1);

    // the lease bounds are enforced
    let err = sqlx::query("SELECT * FROM carebus_research_claim('t', 5)")
        .execute(app)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("lease"), "{err}");

    // the runtime role still sees no run without a tenant scope (RLS)
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM research_runs")
        .fetch_one(app)
        .await
        .unwrap();
    assert_eq!(n, 0);
}
