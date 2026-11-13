//! Plan 18 compliance (migration 0026): legal matters as ethical walls. Route rules through the API, and the same
//! wall in raw SQL as the runtime role `carebus_app` (member vs non-member vs tenant_admin vs another tenant vs the
//! worker's subject-less scope), so a predicate a route forgot cannot hide behind application code.
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::common::{admin, j, user};

async fn send(
    e: &crate::common::TestEnv,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    crate::common::send(
        &e.app,
        crate::common::json(method, &j(path), Some(token), body),
    )
    .await
}

async fn get(e: &crate::common::TestEnv, path: &str, token: &str) -> (StatusCode, Value) {
    crate::common::send(&e.app, crate::common::get(&j(path), Some(token))).await
}

async fn attach(
    e: &crate::common::TestEnv,
    id: &str,
    token: &str,
    store: &str,
) -> (StatusCode, Value) {
    send(
        e,
        "PUT",
        &format!("/legal-matters/{id}/stores/{store}"),
        token,
        json!({}),
    )
    .await
}

async fn run(e: &crate::common::TestEnv, token: &str, body: Value) -> (StatusCode, Value) {
    send(e, "POST", "/research-runs", token, body).await
}

/// `u-third`: a second tenant_user of the bootstrap tenant.
fn third() -> String {
    crate::common::token_for("u-third", &[])
}

async fn provision_third(owner: &PgPool) {
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-third', $1::uuid, 'tenant_user', 'T. Kowalska', 'test')",
    )
    .bind(crate::common::BOOT_TENANT)
    .execute(owner)
    .await
    .unwrap();
}

/// A leaf knowledge store of the bootstrap job bound to a fresh strata store id; returns that id.
async fn bound_store(e: &crate::common::TestEnv, key: &str) -> String {
    let sid = Uuid::new_v4().to_string();
    let (s, b) = send(
        e,
        "PUT",
        &format!("/knowledge-stores/{key}"),
        &admin(),
        json!({"label": "Firm docs", "kind": "leaf", "areas": ["general"], "strata_store_id": sid}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    sid
}

#[tokio::test]
async fn matter_rules_through_the_api() {
    let e = crate::common::env("legal-matters-api").await;
    provision_third(crate::common::pool(&e)).await;
    let sid = bound_store(&e, "client-files").await;
    let open_sid = bound_store(&e, "public-forms").await;

    // any member creates a matter and becomes its lead
    let (s, m) = send(
        &e,
        "POST",
        "/legal-matters",
        &user(),
        json!({"name": "  Kowalski v. Nowak ", "reference": " I C 12/26 "}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    assert_eq!(m["name"], "Kowalski v. Nowak");
    assert_eq!(m["reference"], "I C 12/26");
    assert_eq!(m["my_role"], "lead");
    assert_eq!(m["member_count"], 1);
    let id = m["id"].as_str().unwrap().to_string();
    let (s, b) = send(
        &e,
        "POST",
        "/legal-matters",
        &user(),
        json!({"name": "x", "reference": "I C 12/26"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "reference_taken");
    let (s, b) = send(&e, "POST", "/legal-matters", &user(), json!({"name": "  "})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");

    // the admin sees the matter list (not a member), u-third does not see it at all
    let (_, l) = get(&e, "/legal-matters", &admin()).await;
    assert_eq!(l["items"][0]["id"], json!(id), "{l}");
    assert_eq!(l["items"][0]["my_role"], Value::Null);
    let (_, l) = get(&e, "/legal-matters?mine=true", &admin()).await;
    assert_eq!(l["items"], json!([]));
    let (_, l) = get(&e, "/legal-matters", &third()).await;
    assert_eq!(l["items"], json!([]));
    let (s, _) = get(&e, &format!("/legal-matters/{id}"), &third()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // stores: lead only, a job knowledge store only
    let (s, b) = attach(&e, &id, &admin(), &sid).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) = attach(&e, &id, &user(), &Uuid::new_v4().to_string()).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_store");
    let (s, b) = attach(&e, &id, &user(), &sid.to_uppercase()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["store_id"], json!(sid));

    // run rules
    let (s, b) = run(
        &e,
        &user(),
        json!({"profile": "legal", "question": "q", "matter_id": id, "store_ids": [open_sid]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "store_not_in_matter");
    // a tenant_admin who is not a member cannot file a run under the matter
    let (s, b) = run(
        &e,
        &admin(),
        json!({"profile": "legal", "question": "q", "matter_id": id}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_matter");
    // a walled store cannot be searched without its matter
    let (s, b) = run(
        &e,
        &user(),
        json!({"profile": "research", "question": "q", "store_ids": [sid]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "store_walled");
    let (s, b) = run(
        &e,
        &user(),
        json!({"profile": "research", "question": "q", "store_ids": [open_sid]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let open_run = b["id"].as_str().unwrap().to_string();
    let (s, r) = run(
        &e,
        &user(),
        json!({"profile": "legal", "question": "Czy?", "matter_id": id, "store_ids": [sid]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    assert_eq!(r["matter_id"], json!(id));
    let rid = r["id"].as_str().unwrap().to_string();

    // the run is the members' only: the admin and u-third get 404 and do not list it
    for tok in [admin(), third()] {
        let (s, _) = get(&e, &format!("/research-runs/{rid}"), &tok).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = send(
            &e,
            "POST",
            &format!("/research-runs/{rid}/cancel"),
            &tok,
            json!({}),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (_, l) = get(&e, "/research-runs", &tok).await;
        let ids: Vec<&str> = l["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, vec![open_run.as_str()], "{l}");
    }
    let (_, l) = get(&e, &format!("/research-runs?matter_id={id}"), &user()).await;
    assert_eq!(l["items"].as_array().unwrap().len(), 1, "{l}");
    assert_eq!(l["items"][0]["matter_id"], json!(id));

    // membership: the admin may not add themselves, but may add u-third, who then sees the run
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{id}/members/u-admin"),
        &admin(),
        json!({"role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{id}/members/u-nobody"),
        &admin(),
        json!({"role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_user");
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{id}/members/u-third"),
        &admin(),
        json!({"role": "boss"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{id}/members/u-third"),
        &admin(),
        json!({"role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, d) = get(&e, &format!("/research-runs/{rid}"), &third()).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    // a plain member manages nothing
    let (s, _) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{id}/stores/{open_sid}"),
        &third(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = send(
        &e,
        "PATCH",
        &format!("/legal-matters/{id}"),
        &third(),
        json!({"name": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, d) = get(&e, &format!("/legal-matters/{id}"), &third()).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["matter"]["my_role"], "member");
    assert_eq!(d["members"][0]["user_sub"], "u-user");
    assert_eq!(d["members"][0]["role"], "lead");
    assert_eq!(d["members"][1]["display_name"], "T. Kowalska");
    assert_eq!(d["stores"][0]["store_id"], json!(sid));

    // the last lead can be neither removed nor demoted
    let (s, b) = send(
        &e,
        "DELETE",
        &format!("/legal-matters/{id}/members/u-user"),
        &admin(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "last_lead");
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{id}/members/u-user"),
        &user(),
        json!({"role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    // removing a member takes the run away from them
    let (s, _) = send(
        &e,
        "DELETE",
        &format!("/legal-matters/{id}/members/u-third"),
        &user(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = get(&e, &format!("/research-runs/{rid}"), &third()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // closed: no new run; a matter with runs cannot be deleted
    let (s, m) = send(
        &e,
        "PATCH",
        &format!("/legal-matters/{id}"),
        &admin(),
        json!({"closed": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{m}");
    assert!(m["closed_at"].is_string());
    let (s, b) = run(
        &e,
        &user(),
        json!({"profile": "legal", "question": "q", "matter_id": id}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "matter_closed");
    let (_, l) = get(&e, "/legal-matters?open=true", &user()).await;
    assert_eq!(l["items"], json!([]));
    let (s, b) = send(
        &e,
        "DELETE",
        &format!("/legal-matters/{id}"),
        &user(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "matter_has_runs");

    // an empty matter is deleted; detaching a store
    let (_, m2) = send(
        &e,
        "POST",
        "/legal-matters",
        &third(),
        json!({"name": "Spadek"}),
    )
    .await;
    let id2 = m2["id"].as_str().unwrap();
    let (s, _) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{id2}/stores/{open_sid}"),
        &third(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(
        &e,
        "DELETE",
        &format!("/legal-matters/{id2}/stores/{open_sid}"),
        &third(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = send(
        &e,
        "DELETE",
        &format!("/legal-matters/{id2}/stores/{open_sid}"),
        &third(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(
        &e,
        "DELETE",
        &format!("/legal-matters/{id2}"),
        &third(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // audited, without the client's name
    let audit = crate::common::audit_text(&e).await;
    for action in [
        "\"create\"",
        "member_set",
        "member_remove",
        "store_attach",
        "store_detach",
        "\"delete\"",
    ] {
        assert!(audit.contains(action), "{action}: {audit}");
    }
    assert!(audit.contains("legal_matter"), "{audit}");
    assert!(!audit.contains("Kowalski"), "{audit}");
}

/// Owner 2026-10-25: a legal-profile run no longer needs a matter; a matter the caller is not a member of is still
/// refused, and a walled store still needs its matter.
#[tokio::test]
async fn a_legal_run_needs_no_matter() {
    let e = crate::common::env("legal-matters-optional").await;
    let (s, b) = run(&e, &user(), json!({"profile": "legal", "question": "q"})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(b["matter_id"].is_null(), "{b}");
    let (s, m) = send(&e, "POST", "/legal-matters", &user(), json!({"name": "M"})).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    let (s, b) = run(
        &e,
        &admin(),
        json!({"profile": "legal", "question": "q", "matter_id": m["id"]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_matter");
}

#[tokio::test]
async fn another_tenant_never_sees_a_matter() {
    let e = crate::common::env("legal-matters-tenant").await;
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
    let (_, m) = send(
        &e,
        "POST",
        "/legal-matters",
        &user(),
        json!({"name": "Ours"}),
    )
    .await;
    let id = m["id"].as_str().unwrap();
    let (s, _) = get(&e, &format!("/legal-matters/{id}"), &theirs).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, l) = get(&e, "/legal-matters", &theirs).await;
    assert_eq!(l["items"], json!([]));
    let (s, _) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{id}/members/u-other"),
        &theirs,
        json!({"role": "lead"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // nor can a member of ours be added to their matter
    let (_, t) = send(
        &e,
        "POST",
        "/legal-matters",
        &theirs,
        json!({"name": "Theirs"}),
    )
    .await;
    let tid = t["id"].as_str().unwrap();
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{tid}/members/u-user"),
        &theirs,
        json!({"role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_user");
}

/// A knowledge store of the bootstrap job bound to the strata store `sid` (one of the fake strata_rag's tree).
async fn bind(e: &crate::common::TestEnv, key: &str, sid: &str) {
    let (s, b) = send(
        e,
        "PUT",
        &format!("/knowledge-stores/{key}"),
        &admin(),
        json!({"label": key, "kind": "leaf", "areas": ["general"], "strata_store_id": sid}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

async fn create_matter(e: &crate::common::TestEnv, token: &str, name: &str) -> String {
    let (s, m) = send(e, "POST", "/legal-matters", token, json!({"name": name})).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    m["id"].as_str().unwrap().to_string()
}

/// A tenant-level request (`/api/...`, not under a job).
async fn api(
    e: &crate::common::TestEnv,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = match body {
        Some(b) => crate::common::json(method, path, Some(token), b),
        None => crate::common::get(path, Some(token)),
    };
    crate::common::send(&e.app, req).await
}

fn ids(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["id"].as_str().map(str::to_string))
        .collect()
}

/// The audit finding: Mallory (`u-third`, a tenant_user and NOT a member of matter A) opened a matter of her own,
/// attached A's store to it and filed a run there, so the researcher searched A's documents for her. Now a store
/// belongs to one matter (and walls never nest across matters), and every tenant-level knowledge route hides a
/// walled store from whoever is not a member of its matter, a tenant_admin included.
#[tokio::test]
async fn a_store_belongs_to_one_matter_and_is_hidden_from_everyone_else() {
    use crate::common::fakes::{self, LEAF, OTHER_LEAF, ROUTER};
    let (e, fake) = fakes::sources_env("legal-walls").await;
    provision_third(crate::common::pool(&e)).await;
    bind(&e, "client-a", LEAF).await;
    bind(&e, "public-forms", OTHER_LEAF).await;
    bind(&e, "firm-root", ROUTER).await;
    let mallory = third();

    let a = create_matter(&e, &user(), "Kowalski v. Nowak").await;
    let (s, b) = attach(&e, &a, &user(), LEAF).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // Mallory's own matter cannot take A's store, nor the router above it (a run routed from it reaches A's leaf)
    let b_id = create_matter(&e, &mallory, "Spadek").await;
    for st in [LEAF.to_string(), LEAF.to_uppercase(), ROUTER.to_string()] {
        let (s, b) = attach(&e, &b_id, &mallory, &st).await;
        assert_eq!(s, StatusCode::CONFLICT, "{st}: {b}");
        assert_eq!(b["error"]["code"], "store_in_other_matter", "{st}");
    }
    // so a run on her matter cannot name it, nor can a run without a matter reach it through the router
    let (s, b) = run(
        &e,
        &mallory,
        json!({"profile": "legal", "question": "q", "matter_id": b_id, "store_ids": [LEAF]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "store_not_in_matter");
    for st in [LEAF, ROUTER] {
        let (s, b) = run(
            &e,
            &mallory,
            json!({"profile": "research", "question": "q", "store_ids": [st]}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{st}: {b}");
        assert_eq!(b["error"]["code"], "store_walled", "{st}");
    }
    let (s, b) = run(
        &e,
        &mallory,
        json!({"profile": "legal", "question": "q", "matter_id": a, "store_ids": [LEAF]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_matter");
    // a store outside every wall is still hers to search, and to wall in her matter
    let (s, b) = run(
        &e,
        &mallory,
        json!({"profile": "research", "question": "q", "store_ids": [OTHER_LEAF]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    // a tenant_admin through a matter of their own: refused the same way
    let c = create_matter(&e, &admin(), "Admin's own").await;
    for st in [LEAF, ROUTER] {
        let (s, b) = attach(&e, &c, &admin(), st).await;
        assert_eq!(s, StatusCode::CONFLICT, "{st}: {b}");
        assert_eq!(b["error"]["code"], "store_in_other_matter", "{st}");
    }

    // knowledge routes: A's store does not exist for Mallory or the admin; the member still reads it
    let docs = format!("/api/knowledge/stores/{LEAF}/documents");
    for tok in [mallory.clone(), admin()] {
        let (s, b) = api(&e, "GET", &docs, &tok, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
        let (s, _) = api(
            &e,
            "GET",
            &format!("/api/knowledge/stores/{}/documents", LEAF.to_uppercase()),
            &tok,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        for body in [
            json!({"query": "pozew", "root_ids": [LEAF]}),
            json!({"query": "pozew", "root_id": LEAF.to_uppercase()}),
            json!({"query": "pozew", "leaf_ids": [LEAF, OTHER_LEAF]}),
        ] {
            let (s, b) = api(&e, "POST", "/api/knowledge/query", &tok, Some(body.clone())).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{body}: {b}");
        }
        // the default query (the only root) searches the visible leaves only
        let (s, b) = api(
            &e,
            "POST",
            "/api/knowledge/query",
            &tok,
            Some(json!({"query": "pozew", "route_mode": "hybrid"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let sent = fake.lock().unwrap().queries.last().cloned().unwrap();
        assert_eq!(sent["leaf_ids"], json!([OTHER_LEAF]), "{sent}");
        assert!(
            sent.get("root_id").is_none() && sent.get("root_ids").is_none(),
            "{sent}"
        );
        assert!(!b.to_string().contains(LEAF), "{b}");
        // the store list leaves it out, also as a child of the router
        let (s, l) = api(&e, "GET", "/api/knowledge/stores", &tok, None).await;
        assert_eq!(s, StatusCode::OK, "{l}");
        assert!(!ids(&l).contains(&LEAF.to_string()), "{l}");
        assert!(!l.to_string().contains(LEAF), "{l}");
    }
    let (s, b) = api(&e, "GET", &docs, &user(), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"][0]["store_id"], LEAF);
    let (s, b) = api(
        &e,
        "POST",
        "/api/knowledge/query",
        &user(),
        Some(json!({"query": "pozew", "root_ids": [LEAF]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let sent = fake.lock().unwrap().queries.last().cloned().unwrap();
    assert_eq!(
        sent,
        json!({"query": "pozew", "root_ids": [LEAF]}),
        "a member's query is untouched"
    );
    let (_, l) = api(&e, "GET", "/api/knowledge/stores", &user(), None).await;
    assert!(ids(&l).contains(&LEAF.to_string()), "{l}");

    // an eval that would search A's store is refused for the admin who is not a member
    let (s, b) = api(
        &e,
        "POST",
        "/api/knowledge/eval/runs",
        &admin(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "store_walled");
    let (s, b) = api(
        &e,
        "POST",
        "/api/knowledge/eval/runs",
        &admin(),
        Some(json!({"root_id": LEAF})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");

    // ingest jobs into A's store are not listed for Mallory, and 404 one by one
    let walled_job = fakes::add_job(&fake, LEAF, json!({"kind": "upload"}));
    let open_job = fakes::add_job(&fake, OTHER_LEAF, json!({"kind": "upload"}));
    let (s, l) = api(&e, "GET", "/api/ingest/jobs", &mallory, None).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    assert_eq!(ids(&l), vec![open_job.clone()], "{l}");
    let (s, _) = api(
        &e,
        "GET",
        &format!("/api/ingest/jobs/{walled_job}"),
        &mallory,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, l) = api(&e, "GET", "/api/ingest/jobs", &user(), None).await;
    assert_eq!(ids(&l).len(), 2, "{l}");

    // a matter with stores cannot be deleted (that would unwall them), by its lead or an admin
    for tok in [user(), admin()] {
        let (s, b) = send(
            &e,
            "DELETE",
            &format!("/legal-matters/{a}"),
            &tok,
            json!({}),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(b["error"]["code"], "matter_has_stores");
    }
    // detached, the store is unwalled, and another matter's lead may wall it
    let (s, _) = send(
        &e,
        "DELETE",
        &format!("/legal-matters/{a}/stores/{LEAF}"),
        &user(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = attach(&e, &b_id, &mallory, LEAF).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _) = send(
        &e,
        "DELETE",
        &format!("/legal-matters/{a}"),
        &user(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = api(&e, "GET", &docs, &user(), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "now Mallory's matter's store");
}

/// The rest of the wall: the customer chat's store scope (the bus `faq` worker asks it), sources writing into a
/// walled store, ingest job actions, a document delete by id and the eval run history are out of reach for whoever
/// is not a member of the matter, a tenant_admin included.
#[tokio::test]
async fn chat_sources_jobs_documents_and_eval_history_respect_the_wall() {
    use crate::common::fakes::{self, LEAF, OTHER_LEAF};
    const SVC: &str = "svc-test-token";
    let (e, fake) = fakes::sources_env("legal-walls-rest").await;
    bind(&e, "client-a", LEAF).await;
    bind(&e, "public-forms", OTHER_LEAF).await;
    let scope_body = json!({"tenant_id": crate::common::BOOT_TENANT});
    let faq_scope = "/api/internal/faq/scope";

    // no wall yet: chat keeps strata_rag's default routing
    let (s, b) = api(&e, "POST", faq_scope, SVC, Some(scope_body.clone())).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"restricted": false}));

    // two sources, written before the wall goes up
    let mut src = vec![];
    for (name, store) in [("Case files", LEAF), ("Forms", OTHER_LEAF)] {
        let (s, b) = send(
            &e,
            "POST",
            "/sources",
            &admin(),
            json!({"name": name, "type": "file", "target_store_id": store,
                   "config": {"options": {"ocr": "auto"}}}),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        src.push(b["id"].as_str().unwrap().to_string());
    }
    let (walled_src, open_src) = (src[0].clone(), src[1].clone());

    let a = create_matter(&e, &user(), "Kowalski v. Nowak").await;
    let (s, b) = attach(&e, &a, &user(), LEAF).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // chat: only the leaves outside every wall, whoever's matter it is; a person's token is refused
    let (s, b) = api(&e, "POST", faq_scope, SVC, Some(scope_body.clone())).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"restricted": true, "leaf_ids": [OTHER_LEAF]}));
    for tok in [admin(), user()] {
        let (s, _) = api(&e, "POST", faq_scope, &tok, Some(scope_body.clone())).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
    let (s, _) = api(
        &e,
        "POST",
        faq_scope,
        SVC,
        Some(json!({"tenant_id": crate::common::BOOT_TENANT, "store": LEAF})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // sources into the walled store: absent from the list and 404 everywhere for the admin (not a member)
    let (s, l) = get(&e, "/sources", &admin()).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    assert_eq!(ids(&l), vec![open_src.clone()], "{l}");
    let (_, l) = get(&e, "/sources", &user()).await;
    assert_eq!(ids(&l).len(), 2, "the member sees both: {l}");
    for (method, path) in [
        ("GET", format!("/sources/{walled_src}")),
        ("GET", format!("/sources/{walled_src}/runs")),
        ("DELETE", format!("/sources/{walled_src}")),
        ("POST", format!("/sources/{walled_src}/resync")),
    ] {
        let (s, b) = send(&e, method, &path, &admin(), json!({})).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{method} {path}: {b}");
    }
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/sources/{walled_src}"),
        &admin(),
        json!({"name": "Case files", "type": "file", "target_store_id": OTHER_LEAF, "config": {"options": {"ocr": "auto"}}}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    // nor can a source be pointed at it, new or existing
    let (s, b) = send(
        &e,
        "POST",
        "/sources",
        &admin(),
        json!({"name": "Sneaky", "type": "file", "target_store_id": LEAF, "config": {"options": {"ocr": "auto"}}}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/sources/{open_src}"),
        &admin(),
        json!({"name": "Forms", "type": "file", "target_store_id": LEAF, "config": {"options": {"ocr": "auto"}}}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, b) = get(&e, &format!("/sources/{walled_src}"), &user()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = get(&e, &format!("/sources/{open_src}"), &admin()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["target_store_id"], OTHER_LEAF);

    // ingest jobs into the walled store: cancel / retry / delete are 404, and nothing reaches strata_ingest
    let walled_job = fakes::add_job(&fake, LEAF, json!({"kind": "upload"}));
    let open_job = fakes::add_job(&fake, OTHER_LEAF, json!({"kind": "upload"}));
    for (method, path) in [
        ("POST", format!("/api/ingest/jobs/{walled_job}/cancel")),
        ("POST", format!("/api/ingest/jobs/{walled_job}/retry")),
        ("DELETE", format!("/api/ingest/jobs/{walled_job}")),
    ] {
        let (s, b) = api(&e, method, &path, &admin(), Some(json!({}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{method} {path}: {b}");
    }
    {
        let g = fake.lock().unwrap();
        assert!(g.retried.is_empty() && g.deleted_jobs.is_empty());
        assert_ne!(g.jobs[&walled_job]["status"], "cancelled");
    }
    let (s, b) = api(
        &e,
        "POST",
        &format!("/api/ingest/jobs/{open_job}/cancel"),
        &admin(),
        Some(json!({})),
    )
    .await;
    assert_ne!(s, StatusCode::NOT_FOUND, "{b}");

    // eval history: a run over the walled store, or over the whole tree (which holds it), is not the admin's
    let (r_walled, r_open, r_tree) = (fakes::eval_id(50), fakes::eval_id(51), fakes::eval_id(52));
    for (id, root) in [
        (&r_walled, json!(LEAF)),
        (&r_open, json!(OTHER_LEAF)),
        (&r_tree, Value::Null),
    ] {
        fake.lock().unwrap().eval_runs.push(json!({
            "id": id, "source": "tenant", "root_id": root, "modes": ["hybrid"], "status": "completed",
            "error": null, "progress": {"done": 1, "total": 1}, "questions": [{"id": "q"}],
            "results": [{"question_id": "q", "mode": "hybrid", "rank": 1, "leaf_hit": true}]}));
    }
    let (s, l) = api(&e, "GET", "/api/knowledge/eval/runs", &admin(), None).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    assert_eq!(ids(&l), vec![r_open.clone()], "{l}");
    for id in [&r_walled, &r_tree] {
        let (s, _) = api(
            &e,
            "GET",
            &format!("/api/knowledge/eval/runs/{id}"),
            &admin(),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{id}");
    }
    let (s, _) = api(
        &e,
        "GET",
        &format!("/api/knowledge/eval/runs/{r_open}"),
        &admin(),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, l) = api(&e, "GET", "/api/knowledge/eval/runs", &user(), None).await;
    assert_eq!(ids(&l).len(), 3, "the member sees every run: {l}");

    // a document of the walled store cannot be deleted by id; one of an open store can
    let walled_doc = fakes::doc_id(7);
    fake.lock()
        .unwrap()
        .store_docs
        .insert(LEAF.to_string(), vec![fakes::doc_id(6), walled_doc.clone()]);
    let (s, b) = api(
        &e,
        "DELETE",
        &format!("/api/knowledge/documents/{}", walled_doc.to_uppercase()),
        &admin(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert!(fake.lock().unwrap().deleted_docs.is_empty());
    let (s, b) = api(
        &e,
        "DELETE",
        &format!("/api/knowledge/documents/{}", fakes::doc_id(1)),
        &admin(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    // made a member by the lead, the admin may
    let (s, b) = send(
        &e,
        "PUT",
        &format!("/legal-matters/{a}/members/u-admin"),
        &user(),
        json!({"role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = api(
        &e,
        "DELETE",
        &format!("/api/knowledge/documents/{walled_doc}"),
        &admin(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = get(&e, &format!("/sources/{walled_src}"), &admin()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

// ---- the wall in raw SQL -----------------------------------------------------------------------------------------

/// Who a raw-SQL transaction acts as.
struct As<'a> {
    tenant: Uuid,
    sub: Option<&'a str>,
    admin: bool,
}

/// A transaction as `carebus_app` with the tenant, job and subject GUCs set transaction-locally (empty = unset).
async fn app_tx(db: &PgPool, job: Uuid, who: &As<'_>) -> Transaction<'static, Postgres> {
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SET LOCAL ROLE carebus_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "SELECT set_config('app.current_tenant', $1, true), set_config('app.current_job', $2, true),
                set_config('app.current_sub', $3, true), set_config('app.current_sub_admin', $4, true)",
    )
    .bind(who.tenant.to_string())
    .bind(job.to_string())
    .bind(who.sub.unwrap_or_default())
    .bind(if who.admin { "1" } else { "" })
    .execute(&mut *tx)
    .await
    .unwrap();
    tx
}

async fn count(conn: &mut PgConnection, table: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(conn)
        .await
        .unwrap()
}

const WALLED: [&str; 5] = [
    "legal_matters",
    "research_runs WHERE matter_id IS NOT NULL",
    "research_steps",
    "research_claims",
    "research_citations",
];

#[tokio::test]
async fn the_wall_holds_in_sql_for_every_kind_of_caller() {
    let e = crate::common::env("legal-matters-rls").await;
    let owner = crate::common::pool(&e);
    provision_third(owner).await;
    let tenant: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(crate::common::BOOT_JOB)
        .fetch_one(owner)
        .await
        .unwrap();
    // a matter led by u-user, one run under it with a step, a citation and a claim (the owner pool's session GUCs
    // put them in the bootstrap tenant and job)
    let matter = Uuid::new_v4();
    let run = Uuid::new_v4();
    let cid = Uuid::new_v4();
    for (sql, binds) in [
        (
            "INSERT INTO legal_matters (id, name, created_by) VALUES ($1, 'Kowalski', 'u-user')",
            vec![matter],
        ),
        (
            "INSERT INTO legal_matter_members (matter_id, user_sub, role, added_by) VALUES ($1, 'u-user', 'lead', 'u-user')",
            vec![matter],
        ),
        (
            "INSERT INTO research_runs (id, profile, question, created_by, matter_id) VALUES ($1, 'legal', 'q', 'u-user', $2)",
            vec![run, matter],
        ),
        (
            "INSERT INTO research_steps (run_id, ordinal, kind, request, status) VALUES ($1, 0, 'model', '{}', 'ok')",
            vec![run],
        ),
        (
            "INSERT INTO research_citations (run_id, id, source_kind, locator, quote) VALUES ($1, $2, 'eli', 'DU/1964/93', 'Art. 74')",
            vec![run, cid],
        ),
        (
            "INSERT INTO research_claims (run_id, id, ordinal, text) VALUES ($1, $2, 0, 'claim')",
            vec![run, Uuid::new_v4()],
        ),
    ] {
        let mut q = sqlx::query(sql);
        for b in binds {
            q = q.bind(b);
        }
        q.execute(owner).await.unwrap();
    }
    // and a run without a matter, which everyone in the job keeps seeing
    sqlx::query("INSERT INTO research_runs (id, profile, question, created_by) VALUES ($1, 'research', 'q', 'u-third')")
        .bind(Uuid::new_v4())
        .execute(owner)
        .await
        .unwrap();
    let app = crate::common::app_pool(&e);

    // the member sees everything
    let mut tx = app_tx(
        app,
        job,
        &As {
            tenant,
            sub: Some("u-user"),
            admin: false,
        },
    )
    .await;
    for t in WALLED {
        assert_eq!(count(&mut tx, t).await, 1, "member: {t}");
    }
    assert_eq!(count(&mut tx, "research_runs").await, 2);
    drop(tx);

    // a non-member sees none of it, only the run without a matter
    let mut tx = app_tx(
        app,
        job,
        &As {
            tenant,
            sub: Some("u-third"),
            admin: false,
        },
    )
    .await;
    for t in WALLED {
        assert_eq!(count(&mut tx, t).await, 0, "non-member: {t}");
    }
    assert_eq!(count(&mut tx, "legal_matter_members").await, 0);
    assert_eq!(count(&mut tx, "research_runs").await, 1);
    // nor can they write into it
    let err = sqlx::query("INSERT INTO research_runs (id, profile, question, created_by, matter_id) VALUES ($1, 'legal', 'q', 'u-third', $2)")
        .bind(Uuid::new_v4())
        .bind(matter)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("row-level security"), "{err}");
    drop(tx);
    let mut tx = app_tx(
        app,
        job,
        &As {
            tenant,
            sub: Some("u-third"),
            admin: false,
        },
    )
    .await;
    let n = sqlx::query("UPDATE research_runs SET cancel_requested = true WHERE id = $1")
        .bind(run)
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(n, 0, "a non-member cannot cancel a matter's run");
    let err = sqlx::query("INSERT INTO legal_matter_members (matter_id, user_sub, role, added_by) VALUES ($1, 'u-third', 'lead', 'u-third')")
        .bind(matter)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("row-level security"), "{err}");
    drop(tx);

    // a tenant_admin sees the matter and its members, but not its runs, steps, claims or citations
    let mut tx = app_tx(
        app,
        job,
        &As {
            tenant,
            sub: Some("u-admin"),
            admin: true,
        },
    )
    .await;
    assert_eq!(count(&mut tx, "legal_matters").await, 1);
    assert_eq!(count(&mut tx, "legal_matter_members").await, 1);
    for t in &WALLED[1..] {
        assert_eq!(count(&mut tx, t).await, 0, "admin: {t}");
    }
    // the admin may add someone else, but not themselves
    sqlx::query("INSERT INTO legal_matter_members (matter_id, user_sub, role, added_by) VALUES ($1, 'u-third', 'member', 'u-admin')")
        .bind(matter)
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("INSERT INTO legal_matter_members (matter_id, user_sub, role, added_by) VALUES ($1, 'u-admin', 'member', 'u-admin')")
        .bind(matter)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("row-level security"), "{err}");
    drop(tx); // rolled back: u-third is still not a member

    // another tenant, even naming the member's subject, sees nothing
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    let other_job = crate::common::insert_job(owner, other, crate::common::BOOT_JOB).await;
    let mut tx = app_tx(
        app,
        other_job,
        &As {
            tenant: other,
            sub: Some("u-user"),
            admin: true,
        },
    )
    .await;
    for t in WALLED {
        assert_eq!(count(&mut tx, t).await, 0, "other tenant: {t}");
    }
    drop(tx);

    // the worker's scope (no subject) reads and writes the run it claimed, as before 0026
    let mut tx = app_tx(
        app,
        job,
        &As {
            tenant,
            sub: None,
            admin: false,
        },
    )
    .await;
    for t in WALLED {
        assert_eq!(count(&mut tx, t).await, 1, "worker: {t}");
    }
    sqlx::query("INSERT INTO research_steps (run_id, ordinal, kind, request, status) VALUES ($1, 1, 'tool', '{}', 'ok')")
        .bind(run)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE research_runs SET steps_used = 2 WHERE id = $1")
        .bind(run)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // a matter is created only through the definer function, which makes the subject its lead
    let mut tx = app_tx(
        app,
        job,
        &As {
            tenant,
            sub: Some("u-third"),
            admin: false,
        },
    )
    .await;
    let err =
        sqlx::query("INSERT INTO legal_matters (id, name, created_by) VALUES ($1, 'x', 'u-third')")
            .bind(Uuid::new_v4())
            .execute(&mut *tx)
            .await
            .unwrap_err();
    assert!(err.to_string().contains("row-level security"), "{err}");
    drop(tx);
    let mut tx = app_tx(
        app,
        job,
        &As {
            tenant,
            sub: Some("u-third"),
            admin: false,
        },
    )
    .await;
    let mine = Uuid::new_v4();
    sqlx::query("SELECT carebus_legal_matter_create($1, 'Spadek', NULL)")
        .bind(mine)
        .execute(&mut *tx)
        .await
        .unwrap();
    let role: String =
        sqlx::query_scalar("SELECT role FROM legal_matter_members WHERE matter_id = $1")
            .bind(mine)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(role, "lead");
    assert_eq!(
        count(&mut tx, "legal_matters").await,
        1,
        "u-third sees only their own matter"
    );
    drop(tx);
    // without a subject the function refuses
    let mut tx = app_tx(
        app,
        job,
        &As {
            tenant,
            sub: None,
            admin: false,
        },
    )
    .await;
    let err = sqlx::query("SELECT carebus_legal_matter_create($1, 'x', NULL)")
        .bind(Uuid::new_v4())
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("subject"), "{err}");
    drop(tx);

    // a store belongs to ONE matter of the tenant: u-third, lead of a matter of their own, cannot attach u-user's
    let store = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO legal_matter_stores (matter_id, store_id, added_by) VALUES ($1, $2, 'u-user')",
    )
    .bind(matter)
    .bind(&store)
    .execute(owner)
    .await
    .unwrap();
    let as_third = As {
        tenant,
        sub: Some("u-third"),
        admin: false,
    };
    let mut tx = app_tx(app, job, &as_third).await;
    let mine = Uuid::new_v4();
    sqlx::query("SELECT carebus_legal_matter_create($1, 'Spadek', NULL)")
        .bind(mine)
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query(
        "INSERT INTO legal_matter_stores (matter_id, store_id, added_by) VALUES ($1, $2, 'u-third')",
    )
    .bind(mine)
    .bind(&store)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("legal_matter_stores_tenant_store_key"),
        "{err}"
    );
    drop(tx);
    // nor file a run, under her matter or none, that names it (RLS `run_stores`)
    for m in [Some(mine), None] {
        let mut tx = app_tx(app, job, &as_third).await;
        sqlx::query("SELECT carebus_legal_matter_create($1, 'Spadek', NULL)")
            .bind(mine)
            .execute(&mut *tx)
            .await
            .unwrap();
        let err = sqlx::query(
            "INSERT INTO research_runs (id, profile, question, store_ids, created_by, matter_id)
             VALUES ($1, 'research', 'q', ARRAY[$2::text], 'u-third', $3)",
        )
        .bind(Uuid::new_v4())
        .bind(&store)
        .bind(m)
        .execute(&mut *tx)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("run_stores"), "{m:?}: {err}");
    }
    // who sees the walled store: the member and the worker, not a non-member nor a tenant_admin
    for (who, visible) in [
        (Some("u-user"), true),
        (Some("u-third"), false),
        (Some("u-admin"), false),
        (None, true),
    ] {
        let mut tx = app_tx(
            app,
            job,
            &As {
                tenant,
                sub: who,
                admin: who == Some("u-admin"),
            },
        )
        .await;
        let v: bool =
            sqlx::query_scalar("SELECT visible FROM carebus_store_walls() WHERE store_id = $1")
                .bind(&store)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(v, visible, "{who:?}");
    }
    // a matter that still has stores cannot be deleted (RESTRICT: a delete never silently unwalls a store)
    let mut tx = app_tx(app, job, &as_third).await;
    sqlx::query("SELECT carebus_legal_matter_create($1, 'Spadek', NULL)")
        .bind(mine)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO legal_matter_stores (matter_id, store_id, added_by) VALUES ($1, $2, 'u-third')",
    )
    .bind(mine)
    .bind(Uuid::new_v4().to_string())
    .execute(&mut *tx)
    .await
    .unwrap();
    let err = sqlx::query("DELETE FROM legal_matters WHERE id = $1")
        .bind(mine)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("legal_matter_stores_matter_fkey"),
        "{err}"
    );
}
