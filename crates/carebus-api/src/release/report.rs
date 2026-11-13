//! The report of a job's current release: its row in `job_releases`, the live base counts and its open conflicts.
//! Releases are read-only here; the base is seeded by migrations (0009 bootstrap release).
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;

use super::conflicts::{self, Conflict};
use crate::error::{ApiError, ApiResult};

/// Base rows of the current release, per table.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Counts {
    pub context_spaces: i64,
    pub ontology_entities: i64,
    pub ontology_attributes: i64,
    pub ontology_relations: i64,
    pub procedures: i64,
    pub procedure_steps: i64,
    pub connectors: i64,
    pub connector_field_mappings: i64,
    pub knowledge_stores: i64,
}

/// `GET …/releases/current`.
#[derive(Clone, Debug, Serialize)]
pub struct ReleaseReport {
    pub release_id: String,
    pub hash: String,
    pub applied: bool,
    pub previous_release_id: Option<String>,
    pub applied_at: DateTime<Utc>,
    pub applied_by: String,
    pub counts: Counts,
    pub conflicts: Vec<Conflict>,
}

#[derive(sqlx::FromRow)]
struct ReleaseRow {
    hash: String,
    last_applied_at: DateTime<Utc>,
    applied_by: String,
    previous_release_id: Option<String>,
}

/// The report of `release_id` (the job's current release): live base counts and its open conflicts.
pub async fn report(
    conn: &mut PgConnection,
    release_id: &str,
    applied: bool,
) -> ApiResult<ReleaseReport> {
    let r: ReleaseRow = sqlx::query_as(
        "SELECT r.hash, r.last_applied_at, r.applied_by,
                (SELECT p.release_id FROM job_releases p
                  WHERE p.release_id <> r.release_id AND p.last_applied_at <= r.last_applied_at
                  ORDER BY p.last_applied_at DESC LIMIT 1) AS previous_release_id
           FROM job_releases r
          WHERE r.release_id = $1",
    )
    .bind(release_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::NotFound("release".into()))?;
    let counts: Counts = sqlx::query_as(
        "SELECT (SELECT count(*) FROM context_spaces WHERE origin = 'base' AND release_id = $1) AS context_spaces,
                (SELECT count(*) FROM ontology_entities WHERE origin = 'base' AND release_id = $1) AS ontology_entities,
                (SELECT count(*) FROM ontology_attributes WHERE origin = 'base' AND release_id = $1) AS ontology_attributes,
                (SELECT count(*) FROM ontology_relations WHERE origin = 'base' AND release_id = $1) AS ontology_relations,
                (SELECT count(*) FROM procedures WHERE origin = 'base' AND release_id = $1) AS procedures,
                (SELECT count(*) FROM procedure_steps WHERE origin = 'base' AND release_id = $1) AS procedure_steps,
                (SELECT count(*) FROM connectors WHERE origin = 'base' AND release_id = $1) AS connectors,
                (SELECT count(*) FROM connector_field_mappings WHERE origin = 'base' AND release_id = $1)
                    AS connector_field_mappings,
                (SELECT count(*) FROM knowledge_stores WHERE origin = 'base' AND release_id = $1) AS knowledge_stores",
    )
    .bind(release_id)
    .fetch_one(&mut *conn)
    .await?;
    let conflicts = conflicts::open(conn, release_id).await?;
    Ok(ReleaseReport {
        release_id: release_id.to_string(),
        hash: r.hash,
        applied,
        previous_release_id: r.previous_release_id,
        applied_at: r.last_applied_at,
        applied_by: r.applied_by,
        counts,
        conflicts,
    })
}

/// The report of the job's current release, `None` when the job never had one.
pub async fn current_report(conn: &mut PgConnection) -> ApiResult<Option<ReleaseReport>> {
    let current: Option<String> =
        sqlx::query_scalar("SELECT release_id FROM job_releases WHERE is_current")
            .fetch_optional(&mut *conn)
            .await?;
    match current {
        Some(id) => Ok(Some(report(conn, &id, true).await?)),
        None => Ok(None),
    }
}
