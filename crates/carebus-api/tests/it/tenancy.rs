//! Tenant and job resolution (plan 10 Task 5): X-Active-Tenant-Id, the audited super_admin switch, and the guards.

use crate::common::{get, send, with_tenant};
use axum::http::StatusCode;
use carebus::Scope;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn super_admin_without_active_tenant_is_400() {
    let e = crate::common::env("ten-sa400").await;
    let sa = crate::common::super_admin();
    let (s, b) = send(&e.app, get(&crate::common::j("/procedures"), Some(&sa))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "active_tenant_required");
    let (s, b) = send(
        &e.app,
        with_tenant(
            get(&crate::common::j("/procedures"), Some(&sa)),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // a tenant care-bus was never provisioned with
    let (s, _) = send(
        &e.app,
        with_tenant(
            get(&crate::common::j("/procedures"), Some(&sa)),
            crate::common::OTHER_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // not a bare uuid
    let (s, b) = send(
        &e.app,
        with_tenant(
            get(&crate::common::j("/procedures"), Some(&sa)),
            "not-a-uuid",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_active_tenant");
}

#[tokio::test]
async fn switch_is_audited_once_per_change() {
    let e = crate::common::env("ten-switch").await;
    let owner = crate::common::pool(&e);
    let sa = crate::common::super_admin();
    for _ in 0..2 {
        let (s, b) = send(
            &e.app,
            with_tenant(
                get(&crate::common::j("/cases"), Some(&sa)),
                crate::common::BOOT_TENANT,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    crate::common::insert_job(owner, other, crate::common::BOOT_JOB).await;
    let (s, b) = send(
        &e.app,
        with_tenant(
            get(&crate::common::j("/cases"), Some(&sa)),
            crate::common::OTHER_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(
        "SELECT tenant_id, details FROM audit_events WHERE action_type = 'tenant_switch' ORDER BY id",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows[0].0,
        crate::common::BOOT_TENANT.parse::<Uuid>().unwrap()
    );
    assert!(rows[0].1["from"].is_null());
    assert_eq!(rows[1].0, other);
    assert_eq!(rows[1].1["from"], crate::common::BOOT_TENANT);
}

#[tokio::test]
async fn member_cannot_name_another_tenant() {
    let e = crate::common::env("ten-member").await;
    let (s, b) = send(
        &e.app,
        with_tenant(
            get(
                &crate::common::j("/procedures"),
                Some(&crate::common::admin()),
            ),
            crate::common::OTHER_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "forbidden");
    // naming the home tenant is the same as naming none
    let (s, _) = send(
        &e.app,
        with_tenant(
            get(
                &crate::common::j("/procedures"),
                Some(&crate::common::admin()),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn unknown_job_is_404() {
    let e = crate::common::env("ten-job404").await;
    let (s, _) = send(
        &e.app,
        get("/api/jobs/nope/procedures", Some(&crate::common::admin())),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // another tenant's job is not ours either
    let owner = crate::common::pool(&e);
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    crate::common::insert_job(owner, other, "acme_only").await;
    let (s, _) = send(
        &e.app,
        get(
            "/api/jobs/acme_only/procedures",
            Some(&crate::common::admin()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// care-bus-agents is standalone (spec 2026-10-18 §1): the configurator's `release-writer` client role grants
/// nothing. Such a token is an ordinary, unprovisioned person: it may not provision and is no tenant member.
#[tokio::test]
async fn release_writer_role_grants_nothing() {
    let e = crate::common::env("ten-rw").await;
    let rw = crate::common::token_for("sa-release", &["release-writer"]);
    let (s, b) = send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/tenants/{}", crate::common::OTHER_TENANT),
            Some(&rw),
            json!({"name": "Acme", "status": "active"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "forbidden");
    let (s, b) = send(
        &e.app,
        crate::common::json(
            "PUT",
            &format!("/api/tenants/{}/jobs/care", crate::common::BOOT_TENANT),
            Some(&rw),
            json!({"label": "Care"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) = send(
        &e.app,
        with_tenant(
            get(&crate::common::j("/procedures"), Some(&rw)),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "not_provisioned");
    let (s, me) = send(&e.app, get("/api/me", Some(&rw))).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["super_admin"], false);
    assert!(me["tenant"].is_null(), "{me}");
    assert!(me.get("release_writer").is_none(), "{me}");
    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(tenants, 1, "nothing was provisioned");
}

#[tokio::test]
async fn service_token_reads_procedures_only() {
    let e = crate::common::env("ten-svc").await;
    let svc = "svc-test-token";
    let (s, b) = send(
        &e.app,
        with_tenant(
            get(&crate::common::j("/procedures"), Some(svc)),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _) = send(
        &e.app,
        with_tenant(
            get(&crate::common::j("/cases"), Some(svc)),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = send(
        &e.app,
        with_tenant(
            crate::common::json(
                "POST",
                &crate::common::j("/procedures"),
                Some(svc),
                json!({}),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = send(&e.app, get(&crate::common::j("/procedures"), Some(svc))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "active_tenant_required");
}

// ---------------------------------------------------------------------------------------------------------------
// Provision-down, /api/me, /api/jobs (plan 10 Task 6) and the scope registry (plan 13 Task 3)
// ---------------------------------------------------------------------------------------------------------------

fn tenant_url(tenant: &str) -> String {
    format!("/api/tenants/{tenant}")
}

async fn put(
    e: &crate::common::TestEnv,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(&e.app, crate::common::json("PUT", path, Some(token), body)).await
}

#[tokio::test]
async fn provisioning_round_trip() {
    let e = crate::common::env("ten-prov").await;
    let sa = crate::common::super_admin();
    let t = tenant_url(crate::common::OTHER_TENANT);
    let acme = json!({"name": "Acme", "status": "active"});
    assert_eq!(put(&e, &t, &sa, acme.clone()).await.0, StatusCode::CREATED);
    let (s, b) = put(&e, &t, &sa, acme).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"id": crate::common::OTHER_TENANT, "name": "Acme", "status": "active"})
    );
    let (s, b) = put(
        &e,
        &format!("{t}/jobs/customer_care_agent"),
        &sa,
        json!({"label": "Care", "settings": {"stance": "formal"}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["settings"], json!({"stance": "formal"}));
    let (s, b) = put(
        &e,
        &format!("{t}/users/u-acme"),
        &sa,
        json!({"tenant_role": "tenant_admin"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let acme = crate::common::token_for("u-acme", &[]);
    let (s, me) = send(&e.app, get("/api/me", Some(&acme))).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["tenant"]["id"], crate::common::OTHER_TENANT);
    assert_eq!(me["tenant"]["role"], "tenant_admin");
    assert_eq!(me["tenant"]["name"], "Acme");
    assert_eq!(me["active_tenant"], crate::common::OTHER_TENANT);
    assert_eq!(me["super_admin"], false);
    let (s, b) = send(&e.app, get(&crate::common::j("/connectors"), Some(&acme))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 0, "{b}");
    let (s, jobs) = send(&e.app, get("/api/jobs", Some(&acme))).await;
    assert_eq!(s, StatusCode::OK, "{jobs}");
    assert_eq!(
        jobs,
        json!({"items": [{"key": "customer_care_agent", "label": "Care", "settings": {"stance": "formal"}, "on_bus": true}], "next": null})
    );
    // every provisioning write is audited in the tenant it provisions
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action_type FROM audit_events WHERE tenant_id = $1::uuid ORDER BY id",
    )
    .bind(crate::common::OTHER_TENANT)
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        actions,
        [
            "tenant.upsert",
            "tenant.upsert",
            "job.upsert",
            "user.upsert"
        ]
    );
}

/// An existing job's settings are its base: a re-PUT may repeat or omit them, never change them
/// (tenant edits go to `PUT /api/jobs/{job}/settings`).
#[tokio::test]
async fn reprovisioning_keeps_base_settings() {
    let e = crate::common::env("ten-prov-base").await;
    let sa = crate::common::super_admin();
    let t = tenant_url(crate::common::OTHER_TENANT);
    put(&e, &t, &sa, json!({"name": "Acme", "status": "active"})).await;
    let job = format!("{t}/jobs/customer_care_agent");
    let (s, b) = put(
        &e,
        &job,
        &sa,
        json!({"label": "Care", "settings": {"stance": "formal"}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = put(
        &e,
        &job,
        &sa,
        json!({"label": "Care 2", "settings": {"stance": "formal"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["label"], "Care 2");
    let (s, b) = put(&e, &job, &sa, json!({"label": "Care 3"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["settings"], json!({"stance": "formal"}));
    let (s, b) = put(
        &e,
        &job,
        &sa,
        json!({"label": "Care 4", "settings": {"stance": "casual"}}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("job_settings_read_only")),
        "{b}"
    );
    let (base, label): (Value, String) = sqlx::query_as(
        "SELECT settings, label FROM jobs WHERE tenant_id = $1::uuid AND key = 'customer_care_agent'",
    )
    .bind(crate::common::OTHER_TENANT)
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        (base, label.as_str()),
        (json!({"stance": "formal"}), "Care 3")
    );
}

#[tokio::test]
async fn provisioning_validates_input() {
    let e = crate::common::env("ten-prov-bad").await;
    let sa = crate::common::super_admin();
    let (s, b) = put(
        &e,
        "/api/tenants/not-a-uuid",
        &sa,
        json!({"name": "x", "status": "active"}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_tenant_id"))
    );
    let nil = tenant_url("00000000-0000-0000-0000-000000000000");
    let (s, b) = put(&e, &nil, &sa, json!({"name": "x", "status": "active"})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_tenant_id"))
    );
    let boot = tenant_url(crate::common::BOOT_TENANT);
    let (s, _) = put(&e, &boot, &sa, json!({"name": "x", "status": "gone"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, b) = put(
        &e,
        &format!("{boot}/jobs/Bad-Key"),
        &sa,
        json!({"label": "x"}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_job_key"))
    );
    let (s, b) = put(
        &e,
        &format!("{boot}/jobs/ok"),
        &sa,
        json!({"label": "x", "settings": {"tone": "x"}}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_body"))
    );
    // a tenant care-bus was never given
    let other = tenant_url(crate::common::OTHER_TENANT);
    let (s, _) = put(
        &e,
        &format!("{other}/jobs/care"),
        &sa,
        json!({"label": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = put(
        &e,
        &format!("{other}/users/u-x"),
        &sa,
        json!({"tenant_role": "tenant_user"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn one_tenant_per_user() {
    let e = crate::common::env("ten-one").await;
    let owner = crate::common::pool(&e);
    crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    let (s, b) = put(
        &e,
        &format!("{}/users/u-admin", tenant_url(crate::common::OTHER_TENANT)),
        &crate::common::super_admin(),
        json!({"tenant_role": "tenant_admin"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_in_other_tenant");
    let home: String =
        sqlx::query_scalar("SELECT tenant_id::text FROM users WHERE sub = 'u-admin'")
            .fetch_one(owner)
            .await
            .unwrap();
    assert_eq!(home, crate::common::BOOT_TENANT);
}

#[tokio::test]
async fn provisioning_guards() {
    let e = crate::common::env("ten-guards").await;
    crate::common::insert_tenant(crate::common::pool(&e), crate::common::OTHER_TENANT).await;
    let admin = crate::common::admin();
    let boot = tenant_url(crate::common::BOOT_TENANT);
    let other = tenant_url(crate::common::OTHER_TENANT);
    let (s, _) = put(
        &e,
        &other,
        &admin,
        json!({"name": "Acme", "status": "active"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = put(&e, &format!("{boot}/jobs/x"), &admin, json!({"label": "X"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = put(
        &e,
        &format!("{boot}/users/u-new"),
        &admin,
        json!({"tenant_role": "tenant_user", "display_name": "N. Ew"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, _) = put(
        &e,
        &format!("{boot}/users/u-new"),
        &admin,
        json!({"tenant_role": "tenant_admin"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = put(
        &e,
        &format!("{other}/users/u-new2"),
        &admin,
        json!({"tenant_role": "tenant_user"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    // a tenant_user manages nobody
    let (s, _) = put(
        &e,
        &format!("{boot}/users/u-new3"),
        &crate::common::user(),
        json!({"tenant_role": "tenant_user"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = send(
        &e.app,
        crate::common::json(
            "DELETE",
            &format!("{boot}/users/u-new"),
            Some(&crate::common::user()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn last_admin_cannot_be_removed_by_a_tenant_admin() {
    let e = crate::common::env("ten-lastadmin").await;
    let users = format!("{}/users", tenant_url(crate::common::BOOT_TENANT));
    let del = |token: String, sub: &str| {
        crate::common::json("DELETE", &format!("{users}/{sub}"), Some(&token), json!({}))
    };
    let (s, b) = send(&e.app, del(crate::common::admin(), "u-admin")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "last_tenant_admin");
    // demoting the last admin is the same removal
    let (s, b) = put(
        &e,
        &format!("{users}/u-admin"),
        &crate::common::admin(),
        json!({"tenant_role": "tenant_user"}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("last_tenant_admin"))
    );
    let (s, _) = send(&e.app, del(crate::common::admin(), "nobody")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // the tenant_admin may remove a tenant_user
    let (s, _) = send(&e.app, del(crate::common::admin(), "u-user")).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // a super_admin may remove the last admin
    let (s, b) = send(
        &e.app,
        with_tenant(
            del(crate::common::super_admin(), "u-admin"),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(left, 0);
    let deletes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action_type = 'user.delete'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(deletes, 2);
}

#[tokio::test]
async fn me_reports_identity_and_active_tenant() {
    let e = crate::common::env("ten-me").await;
    let (s, me) = send(&e.app, get("/api/me", Some(&crate::common::user()))).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["sub"], "u-user");
    assert_eq!(me["tenant"]["role"], "tenant_user");
    assert_eq!(me["tenant"]["status"], "active");
    // a super_admin without a users row: no home tenant, the named one is active
    let sa = crate::common::super_admin();
    let (_, me) = send(&e.app, get("/api/me", Some(&sa))).await;
    assert!(
        me["tenant"].is_null() && me["active_tenant"].is_null(),
        "{me}"
    );
    assert_eq!(me["super_admin"], true);
    let (_, me) = send(
        &e.app,
        with_tenant(get("/api/me", Some(&sa)), crate::common::BOOT_TENANT),
    )
    .await;
    assert_eq!(me["active_tenant"], crate::common::BOOT_TENANT);
    // reading /api/me never records a switch
    let switches: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action_type = 'tenant_switch'")
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert_eq!(switches, 0);
    assert_eq!(
        send(&e.app, get("/api/me", None)).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn job_provision_registers_scope() {
    let e = crate::common::env("ten-scope").await;
    let sa = crate::common::super_admin();
    let t = tenant_url(crate::common::OTHER_TENANT);
    let care = Scope::parse(&format!("{}/care", crate::common::OTHER_TENANT)).unwrap();
    put(&e, &t, &sa, json!({"name": "Acme", "status": "active"})).await;
    let (s, b) = put(
        &e,
        &format!("{t}/jobs/care"),
        &sa,
        json!({"label": "Care", "settings": {}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let all = carebus::scopes::list(&e.state.bus).await.unwrap();
    assert!(all.contains(&care), "{all:?}");
    put(&e, &t, &sa, json!({"name": "Acme", "status": "suspended"})).await;
    let all = carebus::scopes::list(&e.state.bus).await.unwrap();
    assert!(!all.iter().any(|s| s.job == "care"), "{all:?}");
    // the bootstrap scope is untouched by another tenant's suspension
    assert!(all.iter().any(|s| s.job == crate::common::BOOT_JOB));
    // a job provisioned while the tenant is suspended is not served either
    put(
        &e,
        &format!("{t}/jobs/later"),
        &sa,
        json!({"label": "Later"}),
    )
    .await;
    let all = carebus::scopes::list(&e.state.bus).await.unwrap();
    assert!(!all.iter().any(|s| s.tenant == care.tenant), "{all:?}");
    // reactivation serves every job of the tenant again
    put(&e, &t, &sa, json!({"name": "Acme", "status": "active"})).await;
    let all = carebus::scopes::list(&e.state.bus).await.unwrap();
    let jobs: Vec<&str> = all
        .iter()
        .filter(|s| s.tenant == care.tenant)
        .map(|s| s.job.as_str())
        .collect();
    assert_eq!(jobs, ["care", "later"]);
}

#[tokio::test]
async fn registry_reconciles_at_startup() {
    let cfg = crate::common::config_with("ten-reconcile", |_| {}).await;
    let root = carebus::Bus::connect(&cfg.redis_url, &cfg.prefix)
        .await
        .unwrap();
    let stale = Scope::parse(&format!("{}/stale_job", crate::common::OTHER_TENANT)).unwrap();
    carebus::scopes::add(&root, &stale).await.unwrap();
    let st = carebus_api::state::build_for_tests(cfg)
        .await
        .expect("state");
    let all = carebus::scopes::list(&st.bus).await.unwrap();
    let boot = Scope::parse(&format!(
        "{}/{}",
        crate::common::BOOT_TENANT,
        crate::common::BOOT_JOB
    ))
    .unwrap();
    assert_eq!(all, vec![boot]);
}

// ---- plan 14: reads for the admin UI ----

#[tokio::test]
async fn super_admin_lists_tenants() {
    let e = crate::common::env("ten-list").await;
    let owner = crate::common::pool(&e);
    crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    sqlx::query("UPDATE tenants SET name = 'Acme' WHERE id = $1::uuid")
        .bind(crate::common::OTHER_TENANT)
        .execute(owner)
        .await
        .unwrap();
    // no X-Active-Tenant-Id needed: the picker runs before a tenant is chosen
    let (s, b) = send(
        &e.app,
        get("/api/tenants", Some(&crate::common::super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    let ids: Vec<&str> = items.iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&crate::common::BOOT_TENANT), "{b}");
    assert!(ids.contains(&crate::common::OTHER_TENANT), "{b}");
    let acme = items
        .iter()
        .find(|i| i["id"] == crate::common::OTHER_TENANT)
        .unwrap();
    assert_eq!(acme["name"], "Acme");
    assert_eq!(acme["status"], "active");
    assert!(b["next"].is_null());
    for t in [crate::common::admin(), crate::common::user()] {
        let (s, _) = send(&e.app, get("/api/tenants", Some(&t))).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn tenant_users_are_listed_to_members() {
    let e = crate::common::env("ten-users").await;
    let owner = crate::common::pool(&e);
    crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    let boot = format!("/api/tenants/{}/users", crate::common::BOOT_TENANT);
    for t in [crate::common::admin(), crate::common::user()] {
        let (s, b) = send(&e.app, get(&boot, Some(&t))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let rows: Vec<(&str, &str)> = b["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["sub"].as_str().unwrap(),
                    i["tenant_role"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![("u-admin", "tenant_admin"), ("u-user", "tenant_user")]
        );
        assert_eq!(b["items"][0]["display_name"], "A. Bakker");
    }
    let other = format!("/api/tenants/{}/users", crate::common::OTHER_TENANT);
    let (s, _) = send(&e.app, get(&other, Some(&crate::common::admin()))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = send(&e.app, get(&other, Some(&crate::common::super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"], json!([]));
    let unknown = "/api/tenants/3d2b1c0a-0000-4000-8000-000000000abc/users";
    let (s, _) = send(&e.app, get(unknown, Some(&crate::common::super_admin()))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
