use serde_json::{Value, json};
use uuid::Uuid;

async fn exec(db: &sqlx::PgPool, sql: &str, a: Uuid, b: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(a)
        .bind(b)
        .execute(db)
        .await
        .map(|_| ())
}

#[tokio::test]
async fn sources_schema_enforces_its_invariants() {
    let e = crate::common::env("sources-schema").await;
    let db = crate::common::pool(&e);
    let (src, store) = (Uuid::new_v4(), Uuid::new_v4());
    exec(db, "INSERT INTO sources (id, name, type, target_store_id, updated_by) VALUES ($1,'Help centre','website',$2,'a.bakker')", src, store)
        .await
        .unwrap();
    let (config, mapping, enabled): (Value, Value, bool) =
        sqlx::query_as("SELECT config, mapping, enabled FROM sources WHERE id=$1")
            .bind(src)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!((config, mapping, enabled), (json!({}), json!([]), true));

    // type, name and config shape are checked by the database
    for bad in [
        "INSERT INTO sources (id, name, type, target_store_id, updated_by) VALUES ($1,'x','ftp',$2,'a')",
        "INSERT INTO sources (id, name, type, target_store_id, updated_by) VALUES ($1,'Help centre','api',$2,'a')",
        "INSERT INTO sources (id, name, type, target_store_id, updated_by) VALUES ($1,'  ','api',$2,'a')",
        "INSERT INTO sources (id, name, type, config, target_store_id, updated_by) VALUES ($1,'y','api','[]',$2,'a')",
    ] {
        assert!(exec(db, bad, Uuid::new_v4(), store).await.is_err(), "{bad}");
    }

    // ciphertext and nonce are stored together, and the nonce is 96 bits
    let set = |enc: &'static str, nonce: &'static str| {
        format!(
            "UPDATE sources SET credentials_enc = {enc}, credentials_nonce = {nonce} WHERE id = $1 AND $2 = $2"
        )
    };
    assert!(
        exec(db, &set("'\\x01'::bytea", "NULL"), src, src)
            .await
            .is_err()
    );
    assert!(
        exec(db, &set("'\\x01'::bytea", "decode('0102','hex')"), src, src)
            .await
            .is_err()
    );
    exec(
        db,
        &set("'\\x01'::bytea", "decode('000000000000000000000000','hex')"),
        src,
        src,
    )
    .await
    .unwrap();

    // at most one running run per source; running <=> no finished_at
    let run1 = Uuid::new_v4();
    exec(
        db,
        "INSERT INTO source_runs (id, source_id, status, started_by) VALUES ($1,$2,'running','a')",
        run1,
        src,
    )
    .await
    .unwrap();
    assert!(
        exec(db, "INSERT INTO source_runs (id, source_id, status, started_by) VALUES ($1,$2,'running','a')", Uuid::new_v4(), src)
            .await
            .is_err(),
        "a second running run must violate source_runs_one_running"
    );
    assert!(
        exec(db, "INSERT INTO source_runs (id, source_id, status, started_by, finished_at) VALUES ($1,$2,'running','a',now())", Uuid::new_v4(), src)
            .await
            .is_err()
    );
    assert!(
        exec(db, "INSERT INTO source_runs (id, source_id, status, started_by) VALUES ($1,$2,'completed','a')", Uuid::new_v4(), src)
            .await
            .is_err()
    );
    assert!(
        exec(db, "INSERT INTO source_runs (id, source_id, status, started_by, finished_at) VALUES ($1,$2,'paused','a',now())", Uuid::new_v4(), src)
            .await
            .is_err()
    );
    exec(
        db,
        "UPDATE source_runs SET status='completed', finished_at=now() WHERE id=$1 AND source_id=$2",
        run1,
        src,
    )
    .await
    .unwrap();
    exec(
        db,
        "INSERT INTO source_runs (id, source_id, status, started_by) VALUES ($1,$2,'running','a')",
        Uuid::new_v4(),
        src,
    )
    .await
    .unwrap();
    let (ids, docs, removed): (Vec<String>, Vec<String>, bool) = sqlx::query_as(
        "SELECT ingest_job_ids, document_ids, documents_removed FROM source_runs WHERE id=$1",
    )
    .bind(run1)
    .fetch_one(db)
    .await
    .unwrap();
    assert!(ids.is_empty() && docs.is_empty() && !removed);

    // deleting a source removes its runs (config only; Task 7 decides about strata documents first)
    exec(db, "DELETE FROM sources WHERE id=$1 AND $2 = $2", src, src)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM source_runs WHERE source_id=$1")
        .bind(src)
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}
