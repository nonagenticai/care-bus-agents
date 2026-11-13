//! Row-level security (migration 0007), checked in raw SQL as the runtime role `carebus_app`, so a predicate an
//! application query forgot cannot hide behind application code.
//!
//! The tests switch to `carebus_app` with `SET LOCAL ROLE` from the superuser test pool: RLS applies to the current
//! role, and the role needs no LOGIN for that. Every scoped transaction sets both GUCs explicitly (empty = unset), so
//! a session default on the pool's connections cannot leak into what a test observes.

use sqlx::{AssertSqlSafe, PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

/// A transaction acting as `carebus_app` with the given tenant/job GUCs (transaction-local).
async fn app_tx(
    db: &PgPool,
    tenant: Option<Uuid>,
    job: Option<Uuid>,
) -> Transaction<'static, Postgres> {
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SET LOCAL ROLE carebus_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.current_tenant', $1, true), set_config('app.current_job', $2, true)")
        .bind(tenant.map(|t| t.to_string()).unwrap_or_default())
        .bind(job.map(|j| j.to_string()).unwrap_or_default())
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

async fn boot_job(db: &PgPool) -> Uuid {
    sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(crate::common::BOOT_TENANT.parse::<Uuid>().unwrap())
        .bind(crate::common::BOOT_JOB)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Anti-drift: a new table with a `tenant_id` column cannot ship without RLS and a policy.
#[tokio::test]
async fn every_tenant_table_has_rls_and_policy() {
    let e = crate::common::env("rls-drift").await;
    let db = crate::common::pool(&e);
    let rows: Vec<(String, bool, i64)> = sqlx::query_as(
        "SELECT c.relname::text, c.relrowsecurity, (SELECT count(*) FROM pg_policy p WHERE p.polrelid = c.oid)
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
          WHERE c.relkind = 'r'
            AND (c.relname = 'tenants' OR EXISTS (
                 SELECT 1 FROM information_schema.columns k
                  WHERE k.table_schema = 'public' AND k.table_name = c.relname AND k.column_name = 'tenant_id'))
          ORDER BY 1",
    )
    .fetch_all(db)
    .await
    .unwrap();
    // the 14 adopted tables + tenants, users, jobs, super_admin_active
    assert!(rows.len() >= 18, "{rows:?}");
    for t in crate::common::TENANT_TABLES {
        assert!(rows.iter().any(|r| r.0 == t), "{t} has no tenant_id");
    }
    for (t, rls, n) in rows {
        assert!(rls, "{t}: RLS must be enabled");
        // super_admin_active is definer-only: RLS on, no policy, no grant
        if t != "super_admin_active" {
            assert!(n >= 1, "{t}: needs a policy");
        }
    }
}

#[tokio::test]
async fn app_role_is_not_owner_super_or_bypass() {
    let e = crate::common::env("rls-role").await;
    let db = crate::common::pool(&e);
    let (sup, byp): (bool, bool) =
        sqlx::query_as("SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = 'carebus_app'")
            .fetch_one(db)
            .await
            .unwrap();
    assert!(
        !sup && !byp,
        "carebus_app must not be superuser or BYPASSRLS"
    );
    let owned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_tables WHERE schemaname = 'public' AND tableowner = 'carebus_app'",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(owned, 0, "a table owner bypasses its own RLS");
    // the definer lookups are callable by carebus_app only, not by every role on the server
    let (pub_auth, pub_switch, app_auth, app_switch): (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT has_function_privilege('public', 'carebus_auth_user(text)', 'execute'),
                has_function_privilege('public', 'carebus_switch_active_tenant(text, uuid)', 'execute'),
                has_function_privilege('carebus_app', 'carebus_auth_user(text)', 'execute'),
                has_function_privilege('carebus_app', 'carebus_switch_active_tenant(text, uuid)', 'execute')",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(
        (pub_auth, pub_switch, app_auth, app_switch),
        (false, false, true, true)
    );
}

#[tokio::test]
async fn app_role_sees_only_current_tenant() {
    let e = crate::common::env("rls-tenant").await;
    let owner = crate::common::pool(&e);
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    let other_job = crate::common::insert_job(owner, other, crate::common::BOOT_JOB).await;
    // migrations seed no connector: one per tenant, so each side has exactly one row of its own
    crate::common::insert_connector(owner, boot, boot_job(owner).await, "boot-crm").await;
    crate::common::insert_connector(owner, other, other_job, "acme-crm").await;

    // no tenant GUC: nothing at all, not even the tenant rows
    let mut tx = app_tx(owner, None, None).await;
    for t in [
        "connectors",
        "context_spaces",
        "procedures",
        "tenants",
        "jobs",
    ] {
        assert_eq!(count(&mut tx, t).await, 0, "{t} without a tenant");
    }
    tx.rollback().await.unwrap();

    let mut tx = app_tx(owner, Some(boot), None).await;
    let seen: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT tenant_id FROM connectors")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, vec![boot]);
    assert_eq!(count(&mut tx, "tenants").await, 1);
    let upd = sqlx::query("UPDATE connectors SET name = 'x' WHERE tenant_id = $1")
        .bind(other)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(upd.rows_affected(), 0);
    let del = sqlx::query("DELETE FROM connectors WHERE tenant_id = $1")
        .bind(other)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(del.rows_affected(), 0);
    // a row written without naming its scope lands in the GUC's tenant
    let landed: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (key, label) VALUES ('defaulted', 'x') RETURNING tenant_id",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(landed, boot);
    // last statement: the error aborts the transaction
    let ins = sqlx::query("INSERT INTO jobs (tenant_id, key, label) VALUES ($1, 'evil', 'x')")
        .bind(other)
        .execute(&mut *tx)
        .await;
    assert!(
        ins.unwrap_err().to_string().contains("row-level security"),
        "writing another tenant's row must violate the policy"
    );
    tx.rollback().await.unwrap();

    // the other tenant sees its own connector and none of the bootstrap rows
    let mut tx = app_tx(owner, Some(other), None).await;
    assert_eq!(count(&mut tx, "connectors").await, 1);
    assert_eq!(count(&mut tx, "context_spaces").await, 0);
    tx.rollback().await.unwrap();

    // super_admin_active has no grant: only the definer function touches it
    let mut tx = app_tx(owner, Some(boot), None).await;
    let denied = sqlx::query("SELECT * FROM super_admin_active")
        .execute(&mut *tx)
        .await;
    assert!(
        denied
            .unwrap_err()
            .to_string()
            .contains("permission denied"),
        "carebus_app must not read super_admin_active"
    );
    tx.rollback().await.unwrap();
}

/// The definer functions are the only reads before a tenant is known; they work with no GUC set.
#[tokio::test]
async fn definer_lookups_work_without_a_tenant() {
    let e = crate::common::env("rls-definer").await;
    let owner = crate::common::pool(&e);
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    // the harness provisions u-admin in the bootstrap tenant already; keep the row explicit for this test
    sqlx::query("INSERT INTO users (sub, tenant_id, tenant_role, updated_by) VALUES ('u-admin', $1, 'tenant_admin', 't') ON CONFLICT (sub) DO NOTHING")
        .bind(boot)
        .execute(owner)
        .await
        .unwrap();

    let mut tx = app_tx(owner, None, None).await;
    let home: Option<(Uuid, String, String)> =
        sqlx::query_as("SELECT tenant_id, tenant_role, tenant_status FROM carebus_auth_user($1)")
            .bind("u-admin")
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
    assert_eq!(
        home,
        Some((boot, "tenant_admin".to_string(), "active".to_string()))
    );
    let none: Option<(Uuid, String, String)> =
        sqlx::query_as("SELECT tenant_id, tenant_role, tenant_status FROM carebus_auth_user($1)")
            .bind("nobody")
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
    assert_eq!(none, None);

    // first switch: no previous tenant; same tenant again: returns itself (unchanged); change: returns the previous
    let switch = |t: Uuid| {
        sqlx::query_scalar::<_, Option<Uuid>>("SELECT carebus_switch_active_tenant('u-super', $1)")
            .bind(t)
    };
    assert_eq!(switch(boot).fetch_one(&mut *tx).await.unwrap(), None);
    assert_eq!(switch(boot).fetch_one(&mut *tx).await.unwrap(), Some(boot));
    assert_eq!(switch(other).fetch_one(&mut *tx).await.unwrap(), Some(boot));
    tx.commit().await.unwrap();
    let active: Uuid =
        sqlx::query_scalar("SELECT tenant_id FROM super_admin_active WHERE sub = 'u-super'")
            .fetch_one(owner)
            .await
            .unwrap();
    assert_eq!(active, other);
}

/// pgbouncer runs the database in session mode: a tenant set for one transaction must be gone after COMMIT, on the
/// same server connection, or it would leak to the next client.
#[tokio::test]
async fn tenant_guc_does_not_survive_commit() {
    let e = crate::common::env("rls-guc").await;
    // the runtime pool (no session-level GUC options), one connection held across both transactions
    let mut conn = e.state.db.acquire().await.unwrap();
    let mut tx = sqlx::Connection::begin(&mut *conn).await.unwrap();
    sqlx::query("SELECT set_config('app.current_tenant', $1, true)")
        .bind(crate::common::BOOT_TENANT)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let after: Option<String> =
        sqlx::query_scalar("SELECT nullif(current_setting('app.current_tenant', true), '')")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(after, None);
}

/// Job scope is a restrictive filter inside the tenant: unset = every job of the tenant.
#[tokio::test]
async fn job_scope_filters_within_a_tenant() {
    let e = crate::common::env("rls-job").await;
    let owner = crate::common::pool(&e);
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job = boot_job(owner).await;
    let second = crate::common::insert_job(owner, boot, "second").await;
    crate::common::insert_connector(owner, boot, job, "boot-crm").await;
    crate::common::insert_connector(owner, boot, second, "second-crm").await;

    let mut tx = app_tx(owner, Some(boot), Some(job)).await;
    assert_eq!(count(&mut tx, "connectors").await, 1);
    // writing into another job of the same tenant under a job scope is refused
    let ins = sqlx::query(
        "INSERT INTO connectors (job_id, id, name, system, base_url, auth_type)
         VALUES ($1, 'x', 'x', 'test', 'https://example.test', 'none')",
    )
    .bind(second)
    .execute(&mut *tx)
    .await;
    assert!(ins.unwrap_err().to_string().contains("row-level security"));
    tx.rollback().await.unwrap();

    let mut tx = app_tx(owner, Some(boot), None).await;
    assert_eq!(count(&mut tx, "connectors").await, 2);
    tx.rollback().await.unwrap();
}

/// The runtime pool must not connect as a role RLS does not apply to: startup refuses to serve.
#[tokio::test]
async fn startup_refuses_bypass_role() {
    let cfg = crate::common::config_with("rls-bypass", |c| {
        c.database_app_url = c.database_url.clone()
    })
    .await;
    let err = match carebus_api::state::build_for_tests(cfg).await {
        Ok(_) => panic!("a superuser runtime pool must stop startup"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("bypasses RLS"), "{err}");
}

/// The test harness connects the API as `carebus_app` itself, so every route test runs under RLS.
#[tokio::test]
async fn the_api_pool_is_the_runtime_role() {
    let e = crate::common::env("rls-app-pool").await;
    let who: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(crate::common::app_pool(&e))
        .await
        .unwrap();
    assert_eq!(who, "carebus_app");
}

// ---- W3 plan 11 Task 2 (0010): effective views and the base write guard ----

/// Anti-drift: a plain view runs as its owner, which bypasses RLS on its tables, so every view must be
/// `security_invoker`; and the 9 effective views exist, readable (not writable) by the runtime role.
#[tokio::test]
async fn every_view_is_security_invoker() {
    let e = crate::common::env("rls-views").await;
    let owner = crate::common::pool(&e);
    let bad: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
          WHERE c.relkind = 'v' AND NOT coalesce(c.reloptions @> ARRAY['security_invoker=true'], false)",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    assert!(bad.is_empty(), "views that bypass RLS: {bad:?}");
    let views: Vec<String> = sqlx::query_scalar(
        "SELECT viewname::text FROM pg_views WHERE schemaname = 'public' AND viewname LIKE 'eff\\_%' ORDER BY 1",
    )
    .fetch_all(owner)
    .await
    .unwrap();
    assert_eq!(views.len(), 9, "{views:?}");
    for v in &views {
        let (sel, ins): (bool, bool) = sqlx::query_as(
            "SELECT has_table_privilege('carebus_app', $1, 'select'), has_table_privilege('carebus_app', $1, 'insert')",
        )
        .bind(v)
        .fetch_one(owner)
        .await
        .unwrap();
        assert_eq!((sel, ins), (true, false), "{v}");
    }
}

/// Base rows are written only by the release apply (transaction-local `app.release_apply`); the runtime role cannot
/// update, delete or insert them otherwise, nor write the release ledger.
#[tokio::test]
async fn base_rows_are_read_only_without_release_flag() {
    let e = crate::common::env("rls-base-guard").await;
    let owner = crate::common::pool(&e);
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job = boot_job(owner).await;

    let mut tx = app_tx(owner, Some(boot), Some(job)).await;
    let upd = sqlx::query("UPDATE context_spaces SET label = 'x' WHERE origin = 'base'")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(upd.rows_affected(), 0, "USING hides base rows from UPDATE");
    let del = sqlx::query("DELETE FROM ontology_entities WHERE origin = 'base'")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(del.rows_affected(), 0);
    let del = sqlx::query("DELETE FROM procedure_steps WHERE origin = 'base'")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(del.rows_affected(), 0);
    let ins = sqlx::query(
        "INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure, origin, release_id, content_hash)
         VALUES ('b:c','faq','B','b_req','{}','{}','faq_answer','base','bootstrap-0009','h')",
    )
    .execute(&mut *tx)
    .await;
    assert!(ins.unwrap_err().to_string().contains("row-level security"));
    tx.rollback().await.unwrap();

    // promoting a tenant row to base is refused too
    let mut tx = app_tx(owner, Some(boot), Some(job)).await;
    sqlx::query(
        "INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure)
         VALUES ('t:c','faq','T','t_req','{}','{}','faq_answer')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let promote = sqlx::query(
        "UPDATE context_spaces SET origin = 'base', release_id = 'bootstrap-0009', content_hash = 'h' WHERE key = 't:c'",
    )
    .execute(&mut *tx)
    .await;
    assert!(
        promote
            .unwrap_err()
            .to_string()
            .contains("row-level security")
    );
    tx.rollback().await.unwrap();

    // the release ledger is apply-only
    let mut tx = app_tx(owner, Some(boot), Some(job)).await;
    let rel = sqlx::query(
        "INSERT INTO job_releases (release_id, hash, bundle, is_current, applied_by) VALUES ('r2','h','{}',false,'x')",
    )
    .execute(&mut *tx)
    .await;
    assert!(rel.unwrap_err().to_string().contains("row-level security"));
    tx.rollback().await.unwrap();

    // under the release flag the same statements reach the base rows
    let mut tx = app_tx(owner, Some(boot), Some(job)).await;
    sqlx::query("SELECT set_config('app.release_apply', 'on', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let upd = sqlx::query("UPDATE context_spaces SET label = label WHERE origin = 'base'")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(upd.rows_affected(), 5);
    tx.rollback().await.unwrap();
}
