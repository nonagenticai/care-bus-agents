use sqlx::AssertSqlSafe;
use uuid::Uuid;

/// One entity row as 0009 left it: origin, hidden, release_id, content_hash, base_hash.
type OriginRow = (String, bool, Option<String>, Option<String>, Option<String>);

#[tokio::test]
async fn schema_and_seed_are_in_place() {
    let e = crate::common::env("migrations").await;
    let db = crate::common::pool(&e);
    let n = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(db)
            .await
            .unwrap()
    };
    assert_eq!(n("SELECT count(*) FROM context_spaces").await, 5);
    assert_eq!(n("SELECT count(*) FROM ontology_entities").await, 11);
    assert_eq!(n("SELECT count(*) FROM ontology_relations").await, 10);
    // 0015: model_routes and its enum are gone; the tenant-level LLM provider tables replace them, under RLS
    assert_eq!(
        n("SELECT count(*) FROM pg_class WHERE relname = 'model_routes'").await,
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM pg_type WHERE typname = 'model_use'").await,
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM pg_class WHERE relname IN ('llm_providers', 'llm_provider_uses') AND relkind = 'r' AND relrowsecurity").await,
        2
    );
    assert_eq!(n("SELECT count(*) FROM llm_providers").await, 0);
    assert_eq!(n("SELECT count(*) FROM llm_provider_uses").await, 0);
    // 0016: the retail demo's laya gate questions (5 request, 1 tool, 2 response), under RLS
    assert_eq!(
        n("SELECT count(*) FROM pg_class WHERE relname IN ('gate_questions', 'gate_verdicts') AND relkind = 'r' AND relrowsecurity").await,
        2
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE point = 'request'").await,
        5
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE point = 'tool'").await,
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE point = 'response'").await,
        2
    );
    // 0017: calibrated against laya — credentials_shared is a choice `in ["yes"]` with a 0.6 confidence floor,
    // emergency 0.75, bereavement annotates fail-open, and the response questions are off
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE key = 'credentials_shared' AND \"type\" = 'choice' AND fire_when = 'in' AND threshold = 0.6 AND fire_values = '[\"yes\"]'::jsonb AND action = 'block' AND fail = 'closed' AND enabled AND message LIKE 'For your security%'").await,
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE key = 'customer_consent' AND \"type\" = 'noul' AND fire_when = 'lt' AND threshold = 0.5 AND enabled AND instructions = 'Did the customer explicitly ask for a refund or a return?'").await,
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE key = 'emergency' AND threshold = 0.75").await,
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE key = 'bereavement' AND action = 'annotate' AND fail = 'open'").await,
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE point = 'response' AND enabled").await,
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE enabled").await,
        6
    );
    // 0018: what each question reads and a precheck; credentials_shared reads the customer's own words, is asked
    // only on digits or a code word, and its instructions name what is not a secret again
    assert_eq!(
        n("SELECT count(*) FROM information_schema.columns WHERE table_name = 'gate_questions' AND column_name IN ('input', 'precheck')").await,
        2
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE key = 'credentials_shared' AND input = 'message' AND precheck LIKE '(?i)%' AND instructions LIKE '%An order number, an email address, a phone number%are not secrets.' AND updated_by = 'migration:0018'").await,
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE input = 'request' AND precheck IS NULL").await,
        5
    );
    // 0027: emergency and bereavement read the customer's own words too (the request wrapper made a refund question
    // an emergency, verdict 31), with no precheck and 0017's thresholds, actions and fail policies kept
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE key IN ('emergency', 'bereavement') AND point = 'request' AND input = 'message' AND precheck IS NULL AND updated_by = 'migration:0027'").await,
        2
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE key = 'emergency' AND threshold = 0.75 AND action = 'handoff' AND fail = 'closed' AND enabled").await,
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE key = 'bereavement' AND threshold = 0.7 AND action = 'annotate' AND fail = 'open' AND enabled").await,
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM gate_questions WHERE point = 'request' AND input = 'request'")
            .await,
        2
    );
    assert_eq!(
        n("SELECT count(*) FROM procedures WHERE version_status = 'PUBLISHED'").await,
        5
    );
    assert_eq!(n("SELECT count(*) FROM procedure_steps ps JOIN procedures p ON p.id = ps.procedure_id WHERE p.name = 'return' AND p.version = '1.3.0'").await, 5);
    // 18 seeded rows + the 5 that 0004 backfills for return@1.4.0-beta (copied from 1.3.0, whose plan has the same shape)
    assert_eq!(n("SELECT count(*) FROM procedure_steps").await, 23);
    // every PUBLISHED plan is a real carebus Plan with work in it (an empty plan "completes" a case doing nothing)
    let plans: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT name, plan FROM procedures WHERE version_status = 'PUBLISHED'")
            .fetch_all(db)
            .await
            .unwrap();
    for (name, plan) in plans {
        let p: carebus::domain::Plan =
            serde_json::from_value(plan).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!p.phases.is_empty(), "{name} has no phases");
    }
    // only one PUBLISHED version per name and origin, enforced by the database (0009: the seed is the base layer;
    // a tenant row may publish its own version over it)
    let dup = sqlx::query("INSERT INTO procedures (tenant_id, job_id, origin, release_id, content_hash, name, version, version_status, context_space, domain)
                           SELECT tenant_id, job_id, 'base', release_id, 'x', 'return', '9.9.9', 'PUBLISHED', context_space, domain
                             FROM procedures WHERE name = 'return' AND version_status = 'PUBLISHED' AND origin = 'base'")
        .execute(db).await;
    assert!(
        dup.unwrap_err()
            .to_string()
            .contains("procedures_one_published"),
        "a second PUBLISHED base return must violate the partial unique index"
    );
    // numeric version ordering: 1.10.0 > 1.9.0 (text order says the opposite); pre-release suffix ignored
    let order: Vec<String> = sqlx::query_scalar("SELECT version FROM (VALUES ('1.9.0'), ('1.10.0'), ('1.4.0-beta')) v(version) ORDER BY string_to_array(regexp_replace(version,'-.*$',''),'.')::int[] DESC")
        .fetch_all(db).await.unwrap();
    assert_eq!(order, ["1.10.0", "1.9.0", "1.4.0-beta"]);
    let vs: Vec<i32> = sqlx::query_scalar("SELECT unnest(version_sort) FROM procedures WHERE name = 'return' AND version = '1.4.0-beta'")
        .fetch_all(db).await.unwrap();
    assert_eq!(vs, [1, 4, 0]);
    // one execute audit row per request_id; other actions may repeat
    let ins = |action: &'static str| async move {
        sqlx::query("INSERT INTO audit_events (actor_id, actor_type, action_type, resource_type, resource_id, status, request_id) VALUES ('a','system',$1,'tool','t','success','req-1')")
            .bind(action).execute(db).await
    };
    ins("execute").await.unwrap();
    assert!(
        ins("execute").await.is_err(),
        "duplicate execute request_id must violate the partial unique index"
    );
    ins("other").await.unwrap();
}

/// 0006: every pre-W3 row is adopted into the bootstrap tenant/job, and the old global keys are gone.
#[tokio::test]
async fn existing_rows_belong_to_the_bootstrap_job() {
    let e = crate::common::env("migrations-0006").await;
    let db = crate::common::pool(&e);
    let boot: (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.id, j.id FROM tenants t JOIN jobs j ON j.tenant_id = t.id WHERE j.key = 'customer_care_agent'",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(boot.0, crate::common::BOOT_TENANT.parse::<Uuid>().unwrap());
    for t in crate::common::TENANT_TABLES {
        let stray: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT count(*) FROM {t} WHERE tenant_id IS DISTINCT FROM $1"
        )))
        .bind(boot.0)
        .fetch_one(db)
        .await
        .unwrap();
        assert_eq!(stray, 0, "{t}: rows outside the bootstrap tenant");
        if t != "audit_events" {
            let off: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT count(*) FROM {t} WHERE job_id IS DISTINCT FROM $1"
            )))
            .bind(boot.1)
            .fetch_one(db)
            .await
            .unwrap();
            assert_eq!(off, 0, "{t}: rows outside the bootstrap job");
        }
    }
    // the seeded rows survived the adoption
    let ctx: i64 = sqlx::query_scalar("SELECT count(*) FROM context_spaces WHERE job_id = $1")
        .bind(boot.1)
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(ctx, 5);
    // the same context space (key, request_type) in another job is now legal: the global keys are gone
    let second = crate::common::insert_job(db, boot.0, "second").await;
    sqlx::query(
        "INSERT INTO context_spaces (tenant_id, job_id, key, area, label, request_type, required, entities, procedure)
         SELECT tenant_id, $1, key, area, label, request_type, required, entities, procedure
           FROM context_spaces WHERE key = 'retail:faq:general' AND job_id = $2",
    )
    .bind(second)
    .bind(boot.1)
    .execute(db)
    .await
    .unwrap();
    // and a base procedure may not reference a context space of another job (composite FK; 0009 applies it to base
    // rows only, a tenant row's context space may live in either layer)
    sqlx::query(
        "INSERT INTO job_releases (tenant_id, job_id, release_id, hash, is_current, applied_by)
         VALUES ($1, $2, 'bootstrap-0009', 'bootstrap', true, 'test')",
    )
    .bind(boot.0)
    .bind(second)
    .execute(db)
    .await
    .unwrap();
    let cross = sqlx::query(
        "INSERT INTO procedures (tenant_id, job_id, origin, release_id, content_hash, name, version, context_space)
         VALUES ($1, $2, 'base', 'bootstrap-0009', 'x', 'x', '1.0.0', 'retail:order-support:returns')",
    )
    .bind(boot.0)
    .bind(second)
    .execute(db)
    .await;
    assert!(
        cross
            .unwrap_err()
            .to_string()
            .contains("procedures_context_space_fkey"),
        "a procedure must not point at another job's context space"
    );
}

/// 0005: the procedure columns exist, start NULL, and are indexed for the per-version window.
#[tokio::test]
async fn case_history_has_nullable_procedure_columns() {
    let e = crate::common::env("migrations-0005").await;
    let db = crate::common::pool(&e);
    let cols: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name::text, is_nullable::text FROM information_schema.columns
          WHERE table_name = 'case_history' AND column_name LIKE 'procedure_%' ORDER BY column_name",
    )
    .fetch_all(db)
    .await
    .unwrap();
    assert_eq!(
        cols,
        [
            ("procedure_name".to_string(), "YES".to_string()),
            ("procedure_version".to_string(), "YES".to_string())
        ]
    );
    let idx: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE tablename = 'case_history' AND indexname = 'case_history_procedure'",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(idx, 1);
}

#[tokio::test]
async fn all_scopes_lists_jobs_of_active_tenants_for_the_app_role() {
    let e = crate::common::env("mig-scopes").await;
    let owner = crate::common::pool(&e);
    let other = crate::common::insert_tenant(owner, crate::common::OTHER_TENANT).await;
    crate::common::insert_job(owner, other, "care").await;
    let q = "SELECT tenant_id, job_key FROM carebus_all_scopes()";
    // the runtime role, no tenant GUC: a definer function, so every tenant's jobs are listed
    let rows: Vec<(Uuid, String)> = sqlx::query_as(q)
        .fetch_all(crate::common::app_pool(&e))
        .await
        .unwrap();
    let boot: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    assert_eq!(
        rows,
        vec![
            (boot, crate::common::BOOT_JOB.to_string()),
            (other, "care".to_string())
        ]
    );
    sqlx::query("UPDATE tenants SET status = 'suspended' WHERE id = $1")
        .bind(other)
        .execute(owner)
        .await
        .unwrap();
    let rows: Vec<(Uuid, String)> = sqlx::query_as(q)
        .fetch_all(crate::common::app_pool(&e))
        .await
        .unwrap();
    assert_eq!(rows, vec![(boot, crate::common::BOOT_JOB.to_string())]);
}

// ---- W3 plan 11 Task 1 (0009): origin base|tenant, release tables, bootstrap split ----

async fn count_rows(db: &sqlx::PgPool, sql: String) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(sql))
        .fetch_one(db)
        .await
        .unwrap()
}

/// 0009: the seeded rows of the bootstrap job are base rows of the synthetic release `bootstrap-0009`; a fresh
/// database has no tenant rows; a default INSERT is a tenant row; base and tenant rows of one key coexist.
#[tokio::test]
async fn bootstrap_rows_are_split_into_base_and_tenant() {
    let e = crate::common::env("mig-0009-split").await;
    let db = crate::common::pool(&e);
    let boot_job: Uuid =
        sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
            .bind(crate::common::BOOT_TENANT.parse::<Uuid>().unwrap())
            .bind(crate::common::BOOT_JOB)
            .fetch_one(db)
            .await
            .unwrap();
    let cur: (String, bool) =
        sqlx::query_as("SELECT release_id, is_current FROM job_releases WHERE job_id = $1")
            .bind(boot_job)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(cur, ("bootstrap-0009".to_string(), true));
    for (t, base) in [
        ("context_spaces", 5i64),
        ("ontology_entities", 11),
        ("ontology_attributes", 37),
        ("ontology_relations", 10),
        ("procedures", 6),
        ("procedure_steps", 23),
    ] {
        assert_eq!(
            count_rows(
                db,
                format!("SELECT count(*) FROM {t} WHERE origin = 'base' AND release_id = 'bootstrap-0009'")
            )
            .await,
            base,
            "{t}"
        );
        assert_eq!(
            count_rows(
                db,
                format!("SELECT count(*) FROM {t} WHERE origin = 'tenant'")
            )
            .await,
            0,
            "{t}: a fresh database has no admin rows"
        );
    }
    for t in ["sources", "connectors", "connector_field_mappings"] {
        assert_eq!(
            count_rows(
                db,
                format!("SELECT count(*) FROM {t} WHERE origin <> 'tenant'")
            )
            .await,
            0,
            "{t}"
        );
    }
    // a default INSERT is a tenant row; a base row needs release_id and content_hash
    sqlx::query("INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure) VALUES ('x:y','faq','X','x_req','{}','{}','faq_answer')")
        .execute(db).await.unwrap();
    assert_eq!(
        count_rows(
            db,
            "SELECT count(*) FROM context_spaces WHERE key = 'x:y' AND origin = 'tenant'".into()
        )
        .await,
        1
    );
    assert!(
        sqlx::query("UPDATE context_spaces SET origin = 'base' WHERE key = 'x:y'")
            .execute(db)
            .await
            .is_err()
    );
    // base and tenant rows of one key coexist; two tenant rows do not
    sqlx::query("INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure) SELECT key, area, 'Mine', request_type, required, entities, procedure FROM context_spaces WHERE key = 'retail:faq:general'")
        .execute(db).await.unwrap();
    assert!(sqlx::query("INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure) SELECT key, area, 'Again', 'faq2', required, entities, procedure FROM context_spaces WHERE key = 'retail:faq:general' AND origin = 'tenant'")
        .execute(db).await.is_err());
    // a base connector cannot hold a secret
    let secret = sqlx::query("INSERT INTO connectors (id, name, system, base_url, auth_type, secret_ref, origin, release_id, content_hash) VALUES ('c','c','s',NULL,'none','k/v','base','bootstrap-0009','h')")
        .execute(db).await;
    assert!(
        secret
            .unwrap_err()
            .to_string()
            .contains("connectors_secret_tenant_only"),
        "a base connector must not carry a secret_ref"
    );
    // the new tables exist under RLS and connector health moved out of connectors
    let health_cols = count_rows(
        db,
        "SELECT count(*) FROM information_schema.columns WHERE table_name = 'connectors' AND column_name LIKE 'health%'".into(),
    )
    .await;
    assert_eq!(health_cols, 0);
    for t in [
        "job_releases",
        "knowledge_stores",
        "connector_health",
        "release_conflicts",
    ] {
        let rls: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class WHERE relname = $1 AND relnamespace = 'public'::regnamespace",
        )
        .bind(t)
        .fetch_one(db)
        .await
        .unwrap();
        assert!(rls, "{t}: RLS must be enabled");
    }
}

/// 0009 on a database where a seeded entity was hidden before it ran: a visible base row plus a tenant tombstone.
#[tokio::test]
async fn hidden_seeded_entity_becomes_a_tombstone() {
    let cfg = crate::common::config_with("mig-0009-tombstone", |_| {}).await;
    let db = sqlx::PgPool::connect(&cfg.database_url).await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    migrator.run_to(8, &db).await.unwrap();
    sqlx::query("UPDATE ontology_entities SET hidden = true WHERE name = 'Communication'")
        .execute(&db)
        .await
        .unwrap();
    migrator.run(&db).await.unwrap();
    let rows: Vec<OriginRow> = sqlx::query_as(
        "SELECT origin, hidden, release_id, content_hash, base_hash FROM ontology_entities
          WHERE name = 'Communication' ORDER BY origin",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "base".to_string(),
                false,
                Some("bootstrap-0009".to_string()),
                Some("bootstrap".to_string()),
                None
            ),
            (
                "tenant".to_string(),
                true,
                None,
                None,
                Some("bootstrap".to_string())
            ),
        ]
    );
    // every other seeded entity stays a single base row
    let tenant: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ontology_entities WHERE origin = 'tenant'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(tenant, 1);
    db.close().await;
}

/// The SHA-384 of every migration that production's `carebus._sqlx_migrations` records, so an edit
/// to an applied migration fails here rather than at boot.
///
/// sqlx validates these when the service starts and refuses to come up on a mismatch (`migration N
/// was previously applied but has been modified`). Nothing else in this suite can see that: a fresh
/// database has no applied rows to disagree with, so the mismatch only ever appears in production,
/// on every pod, while the previous revision keeps serving.
///
/// This is not hypothetical. `0002_seed` was rewritten during the 2026-11-13 re-foundation of this
/// repo and `carebus-api` could not deploy an image for two days.
///
/// Version 2 needed one further reconciliation, recorded here because a checksum alone does not
/// explain it. The pre-re-foundation bytes named an internal host in a seed row, and
/// `scripts/ci/no-private-refs.sh` — correctly — forbids that in a public repo, so those bytes
/// cannot be restored. The row seeds `model_routes`, which migration 0015 DROPS, so the two
/// versions cannot leave a different surviving schema; production's recorded checksum for version 2
/// was moved to this file's value on 2026-11-15 (previously `59096b39…`). That is the only
/// circumstance in which the recorded checksum may be edited, and it is not a pattern to repeat.
///
/// Never edit an applied migration. If this test fails, you have edited one: put your change in a
/// new migration instead.
const APPLIED: &[(i64, &str)] = &[
    (
        1,
        "bb98a0dcfe5c1d2cef66a3802cd63b1e9e0ed9e1be78ee84c072d6734b17cb0abfb2637773affbad0691540059bce8f1",
    ),
    (
        2,
        "fe8da91e3060bc651af51f36054e700bf37e151370a2d943ca34942f11c8fc015489a4522b76b2f1d5d29711d3f49831",
    ),
    (
        3,
        "e36eb17e8eff69024d596a4a6c0387a33c9434cfdada4a87b52b87b2d98994f48bcbd04c2e8563a8bd6210fc7074c6e4",
    ),
    (
        4,
        "263005a48b41e0a0e9c34b67bd5ac113ebcb14199534fc467385e9d8c11ae2b7176e9df6b6828b8e5ee809ac0b86a3d4",
    ),
    (
        5,
        "a5377b33ec2ff4644ed927b81ac829883df08f892bb1ea4e0146009411aec7066a43059c3a8625b07f5babc1240ef8ec",
    ),
    (
        6,
        "23470deccec72e431f87139234538537f45a0b51aff5f86e087b14f79dd86bd5f48e518ff362c7d8eeb678ba9ccbf07e",
    ),
    (
        7,
        "3eebb19e79921996b72073bb84af0a8a82ace4bd19446ffbf86c39a8c0f6224323f11ebbfcfd2ca2841533cf5fe2e817",
    ),
    (
        8,
        "835de21c69f57d824b3fd4d1e1b548e1ce9511fedc451f699e9e845c78bf0e06e854e593f0a4ee77377461c8cc5b27bb",
    ),
    (
        9,
        "e2a9e3ce227be95f34103e3d96dd45759298469b71d7b77c440c9d537bfb46a213eeefd2fdc58ad13736be3e4a12e30a",
    ),
    (
        10,
        "23a4a5a907934a4e31c9767a12d0e7e699ec49f6f4b855a95aafe4e5891bbc49e48f1f3d631035f36e5ecc7a4f5dec78",
    ),
    (
        11,
        "b92cefc485b656f7ff100699c45d74f69fd09079e96082d0125bb90f48931c345e70618869670048e92044f39f4d149b",
    ),
    (
        13,
        "cb888f700c2096c1a70cb31900ac6652e81d2cd17c1745889ca3bc6122db86d7c20a841c5072f3d039f4935b4d8f1b5f",
    ),
    (
        14,
        "4791567668d4f318dcbcbdd425b1643aa2dff2827ac69c007887586f380fd33ffe81da3d812b7159e10502253498b392",
    ),
    (
        15,
        "9f76f80b3fa0f5034595a2cf2dae404afc5140896727f3c3d1c4f8f91edbd6dffe2355489a7324280bba278784f81d5d",
    ),
    (
        16,
        "9620a83e9eeb82cd05345a46984cade0cb7787ad2a72bc93b12facb540736df168396f0dc9b8732b1b675c7c2765b79f",
    ),
    (
        17,
        "e0d8163a8dfd41cf2b1ef3fac170a959df9dc4002b9bf0a97a72945caf0a447a725dae70a1f4def635242be3b944d0bd",
    ),
    (
        18,
        "ca8801f37109598ac3f7f84c06e8fc003d3524fdc98036ec480c94fd6e6414527ed9c6888b90d6d09a40a7210198aae4",
    ),
    (
        19,
        "c9151f257cc3f24fa3da4917f325a34688014a8ecc188e6f0722452156bcc8a2b38e4c0c426950cb3eb0213183e29740",
    ),
    (
        20,
        "8bc6630f81d350a1229ccc507f229cf3cec8fb7c18b0ce0f16cb65a92f51dbb341fa4b29479110f28d99113f49c8cebc",
    ),
    (
        21,
        "4815637c0c77b44f0d730498bb31cbdc9a10b5abf46d99b7a0c7cae74b71b884bf964e4b1397af61222eb4b2d6e27103",
    ),
    (
        22,
        "29b73bb151a0d17ba834a64871ff43f4655b7e6a10be37c112632b0d61a1ddc667f31b03996e8d7fce6a19d3425b0ee9",
    ),
    (
        23,
        "43604fc8731ca5880af3027421ce386ac634ab150ca4775bd33b1d6ab827948252fdcb214d6ca744e5fcf551e248970b",
    ),
    (
        24,
        "1003073bc557b79c249f4316b22cce2c49aada682b3142cc36822f07413c9d9ac56c7e382b862870a200b55334ea9d75",
    ),
    (
        25,
        "3db72113a3853f930a8963ce57abeedeb7e3c5b727bcdffd0d9f23a80ba64fd6b24155d58e9cf99f43085d7023aae243",
    ),
    (
        26,
        "77b5d5f0435973cac1b26c75fd63c6c6129a7a29f06c72b0a75807167edb568c68ffe7e8269a3d0c3f6dd1287ee90004",
    ),
    (
        27,
        "e370d99f6e121c2579469d4fffa32bbd08ca3163e514cd55397d7ab6a51fc47f3c81ee8c4ef4eda4e9555e117256c443",
    ),
    (
        28,
        "43bc7b3e7634400f88231874a29d979e1ff0710a059e9320da1cd2d0917a9ac32692584f75657c7c6180c633653465b1",
    ),
    (
        29,
        "5f4e4d81b670ab88ba4df6dc58e5786fc2dd91b3dfcf659e8d39b88315e885aeed410c32161a7f00fb536b01ebb6815f",
    ),
    (
        30,
        "e5522f5d57a9ead6a91dfb98c6d47c69398fc70b51e457aaa6b188dbb170a4c08fa9615ceca18875c5346385ffb8043b",
    ),
];

#[test]
fn applied_migrations_are_never_edited() {
    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
    for (version, expected) in APPLIED {
        let migration = MIGRATOR.migrations.iter().find(|m| m.version == *version);
        let Some(migration) = migration else {
            panic!(
                "migration {version} was applied in production but is absent from ./migrations; \
                 deleting an applied migration is as fatal as editing one"
            );
        };
        let actual: String = migration
            .checksum
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            actual.as_str(),
            *expected,
            "migration {version} ({}) was edited after it was applied, so sqlx will refuse to boot \
             against production. Restore its original bytes and put the change in a new migration.",
            migration.description
        );
    }
}
