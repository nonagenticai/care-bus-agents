//! Conflicts between a job's tenant refinements and the base of a release, as stored in `release_conflicts`.
//! Nothing pushes a release any more (the base is seeded by migrations), so no new conflict is detected; the stored
//! rows stay readable.
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;

use crate::error::ApiResult;

/// One conflict as the API returns it (`id` is the `release_conflicts` row).
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Conflict {
    pub id: i64,
    pub kind: String,
    pub object: String,
    pub key: Value,
    pub tenant_row: String,
    pub base_hash_before: Option<String>,
    pub base_hash_after: Option<String>,
}

/// The open conflicts of `release_id`, oldest first.
pub async fn open(conn: &mut PgConnection, release_id: &str) -> ApiResult<Vec<Conflict>> {
    Ok(sqlx::query_as(
        "SELECT id, kind, object, key, tenant_row, base_hash_before, base_hash_after
           FROM release_conflicts
          WHERE release_id = $1 AND resolved_at IS NULL
          ORDER BY id",
    )
    .bind(release_id)
    .fetch_all(&mut *conn)
    .await?)
}
