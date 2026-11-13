//! Tenant refinements of a job's base configuration (W3 plan 11).
//!
//! A base row (`origin = 'base'`) belongs to the job's current release and is written only by migrations. A
//! tenant row (`origin = 'tenant'`) refines it by natural key: an **override** (base row exists, `base_hash` = the
//! base row's `content_hash` when the refinement was written), an **extension** (no base row, `base_hash` NULL) or a
//! **tombstone** (`hidden = true`). Every write route goes through this module and writes `origin = 'tenant'` rows
//! only; the restrictive `base_guard_*` policies (migration 0010) refuse anything else.
//!
//! Job settings are an overlay on columns: `jobs.settings` is the base, `jobs.settings_override` holds
//! `{"<setting>": {"value": string|null, "base_hash": "sha256:…"|null}}` (`value: null` hides the base setting); a
//! setting's base hash is [`setting_hash`] of its base value.
//!
//! Every function runs on a job-scoped transaction (`JobCtx::begin`): the current release is read under the job GUC.
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

/// The job settings a base and an override may carry.
pub const JOB_SETTINGS: [&str; 3] = ["preamble", "actor", "stance"];

/// The objects a tenant can refine; `as_str` is the `object` of a refinement request and of a release conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    ContextSpace,
    OntologyEntity,
    OntologyAttribute,
    OntologyRelation,
    Procedure,
    Connector,
    ConnectorFieldMapping,
    KnowledgeStore,
    JobSetting,
}

/// What a tenant row is to the base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowKind {
    Override,
    Extension,
    Tombstone,
}

impl RowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RowKind::Override => "override",
            RowKind::Extension => "extension",
            RowKind::Tombstone => "tombstone",
        }
    }
    /// From a tenant row's `hidden` and `base_hash`.
    pub fn of(hidden: bool, base_hash: Option<&str>) -> Self {
        if hidden {
            RowKind::Tombstone
        } else if base_hash.is_some() {
            RowKind::Override
        } else {
            RowKind::Extension
        }
    }
}

/// A natural key: exactly the object's `key_columns`, each a string.
pub type NaturalKey = Map<String, Value>;

impl ObjectKind {
    pub const ALL: [ObjectKind; 9] = [
        ObjectKind::ContextSpace,
        ObjectKind::OntologyEntity,
        ObjectKind::OntologyAttribute,
        ObjectKind::OntologyRelation,
        ObjectKind::Procedure,
        ObjectKind::Connector,
        ObjectKind::ConnectorFieldMapping,
        ObjectKind::KnowledgeStore,
        ObjectKind::JobSetting,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::ContextSpace => "context_space",
            ObjectKind::OntologyEntity => "ontology_entity",
            ObjectKind::OntologyAttribute => "ontology_attribute",
            ObjectKind::OntologyRelation => "ontology_relation",
            ObjectKind::Procedure => "procedure",
            ObjectKind::Connector => "connector",
            ObjectKind::ConnectorFieldMapping => "connector_field_mapping",
            ObjectKind::KnowledgeStore => "knowledge_store",
            ObjectKind::JobSetting => "job_setting",
        }
    }

    /// The overlay table (`jobs` for a job setting, whose overlay is `settings` / `settings_override`).
    pub fn table(self) -> &'static str {
        match self {
            ObjectKind::ContextSpace => "context_spaces",
            ObjectKind::OntologyEntity => "ontology_entities",
            ObjectKind::OntologyAttribute => "ontology_attributes",
            ObjectKind::OntologyRelation => "ontology_relations",
            ObjectKind::Procedure => "procedures",
            ObjectKind::Connector => "connectors",
            ObjectKind::ConnectorFieldMapping => "connector_field_mappings",
            ObjectKind::KnowledgeStore => "knowledge_stores",
            ObjectKind::JobSetting => "jobs",
        }
    }

    /// The natural key: column names of the table, and the field names of a conflict's / a request's `key`.
    pub fn key_columns(self) -> &'static [&'static str] {
        match self {
            ObjectKind::ContextSpace | ObjectKind::KnowledgeStore | ObjectKind::JobSetting => {
                &["key"]
            }
            ObjectKind::OntologyEntity => &["name"],
            ObjectKind::OntologyAttribute => &["entity", "name"],
            ObjectKind::OntologyRelation => &["subject", "predicate", "object"],
            ObjectKind::Procedure => &["name", "version"],
            ObjectKind::Connector => &["id"],
            ObjectKind::ConnectorFieldMapping => &["connector_id", "backend_field"],
        }
    }

    /// The content columns a tenant copy (override or tombstone) takes from the base row: every column of the
    /// bundle item, key included; never the overlay columns, ids, timestamps or tenant wiring.
    fn content_columns(self) -> &'static [&'static str] {
        match self {
            ObjectKind::ContextSpace => &[
                "key",
                "area",
                "label",
                "request_type",
                "required",
                "entities",
                "procedure",
            ],
            ObjectKind::OntologyEntity => &[
                "name",
                "scope",
                "industry",
                "department",
                "label",
                "synonyms",
            ],
            ObjectKind::OntologyAttribute => &["entity", "name", "data_type", "pii", "synonyms"],
            ObjectKind::OntologyRelation => &["subject", "predicate", "object", "cardinality"],
            ObjectKind::Procedure => &[
                "name",
                "version",
                "version_status",
                "context_space",
                "domain",
                "description",
                "pddl_domain",
                "tool_requirements",
                "success_criteria",
                "reviewers",
                "plan",
            ],
            ObjectKind::Connector => &[
                "id",
                "name",
                "system",
                "base_url",
                "auth_type",
                "timeout_ms",
                "rate_limit",
            ],
            ObjectKind::ConnectorFieldMapping => &["connector_id", "backend_field", "attribute"],
            ObjectKind::KnowledgeStore => {
                &["key", "label", "parent_key", "kind", "areas", "description"]
            }
            ObjectKind::JobSetting => &[],
        }
    }

    /// 400 `invalid_object` for anything but the names of [`ObjectKind::as_str`].
    pub fn parse(s: &str) -> ApiResult<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| {
                ApiError::BadRequest(
                    "invalid_object",
                    format!(
                        "object must be one of {}.",
                        Self::ALL.map(|k| k.as_str()).join(", ")
                    ),
                )
            })
    }

    /// The key as an object with exactly `key_columns`, each a non-empty string; 400 `invalid_key` otherwise.
    pub fn key(self, v: &Value) -> ApiResult<NaturalKey> {
        let bad = || {
            ApiError::BadRequest(
                "invalid_key",
                format!(
                    "key of a {} must be an object with exactly the string fields {}.",
                    self.as_str(),
                    self.key_columns().join(", ")
                ),
            )
        };
        let m = v.as_object().ok_or_else(bad)?;
        if m.len() != self.key_columns().len()
            || !self.key_columns().iter().all(|c| {
                m.get(*c)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
        {
            return Err(bad());
        }
        if self == ObjectKind::JobSetting
            && !JOB_SETTINGS.contains(&m["key"].as_str().unwrap_or(""))
        {
            return Err(ApiError::BadRequest(
                "invalid_key",
                format!("a job setting is one of {}.", JOB_SETTINGS.join(", ")),
            ));
        }
        Ok(m.clone())
    }

    /// A key from its values, in `key_columns` order (callers that already hold validated values).
    pub fn key_of(self, values: &[&str]) -> NaturalKey {
        self.key_columns()
            .iter()
            .zip(values)
            .map(|(c, v)| ((*c).to_string(), Value::String((*v).to_string())))
            .collect()
    }
}

/// The values of `key`, in `key_columns` order.
fn key_values(kind: ObjectKind, key: &NaturalKey) -> Vec<String> {
    kind.key_columns()
        .iter()
        .map(|c| {
            key.get(*c)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// `a.c1 = $n AND a.c2 = $n+1 …` over the key columns.
fn key_pred(kind: ObjectKind, alias: &str, first: usize) -> String {
    kind.key_columns()
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{alias}.{c} = ${}", first + i))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// `a.c1 = b.c1 AND …` over the key columns, plus the scope.
fn key_join(kind: ObjectKind, a: &str, b: &str) -> String {
    let mut parts = vec![
        format!("{a}.tenant_id = {b}.tenant_id"),
        format!("{a}.job_id = {b}.job_id"),
    ];
    parts.extend(
        kind.key_columns()
            .iter()
            .map(|c| format!("{a}.{c} = {b}.{c}")),
    );
    parts.join(" AND ")
}

/// The current release of the row aliased `alias` (correlated on its tenant and job).
pub fn current_release(alias: &str) -> String {
    format!(
        "(SELECT r.release_id FROM job_releases r WHERE r.tenant_id = {alias}.tenant_id AND r.job_id = {alias}.job_id AND r.is_current)"
    )
}

/// `"sha256:…"` of a base job setting's value (the `base_hash` of a setting override).
pub fn setting_hash(value: &str) -> String {
    // A string always canonicalises; the fallback is unreachable but keeps this infallible.
    crate::release::row_hash(&Value::String(value.to_string()), &[])
        .unwrap_or_else(|_| "sha256:".into())
}

/// `SELECT 1 FROM jobs WHERE id = $1 FOR SHARE`: a refinement never records a `base_hash` from a base that a release
/// apply (which locks the job `FOR UPDATE`) is swapping.
pub async fn lock_job_shared(tx: &mut PgConnection, job_id: Uuid) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM jobs WHERE id = $1 FOR SHARE")
        .bind(job_id)
        .fetch_optional(&mut *tx)
        .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::NotFound("job".into()))
}

/// The base value of a job setting (`None` when the base does not set it).
async fn base_setting(tx: &mut PgConnection, key: &str) -> ApiResult<Option<String>> {
    let v: Option<Option<String>> =
        sqlx::query_scalar("SELECT settings ->> $1 FROM jobs WHERE id = app_current_job()")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    Ok(v.flatten())
}

/// The tenant override of a job setting: `Some(value)` (value `None` = hidden), or `None` when not overridden.
async fn setting_override(tx: &mut PgConnection, key: &str) -> ApiResult<Option<Option<String>>> {
    let v: Option<Option<Value>> =
        sqlx::query_scalar("SELECT settings_override -> $1 FROM jobs WHERE id = app_current_job()")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    Ok(v.flatten()
        .map(|o| o.get("value").and_then(Value::as_str).map(str::to_string)))
}

/// Sets the tenant override of a job setting (`None` hides the base value), with the current base hash.
pub async fn set_setting(tx: &mut PgConnection, key: &str, value: Option<&str>) -> ApiResult<()> {
    let base_hash = base_setting(tx, key).await?.map(|b| setting_hash(&b));
    sqlx::query(
        "UPDATE jobs SET settings_override = settings_override || jsonb_build_object($1::text, $2::jsonb), updated_at = now()
          WHERE id = app_current_job()",
    )
    .bind(key)
    .bind(json!({"value": value, "base_hash": base_hash}))
    .execute(&mut *tx)
    .await?;
    Ok(())
}

async fn remove_setting(tx: &mut PgConnection, key: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE jobs SET settings_override = settings_override - $1::text, updated_at = now() WHERE id = app_current_job()",
    )
    .bind(key)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The current release's base `content_hash` of `key` (for a job setting, [`setting_hash`] of its base value).
pub async fn base_hash_of(
    tx: &mut PgConnection,
    kind: ObjectKind,
    key: &NaturalKey,
) -> ApiResult<Option<String>> {
    let vals = key_values(kind, key);
    if kind == ObjectKind::JobSetting {
        return Ok(base_setting(tx, &vals[0]).await?.map(|b| setting_hash(&b)));
    }
    let sql = format!(
        "SELECT b.content_hash FROM {t} b WHERE b.origin = 'base' AND b.release_id = {cur} AND {pred}",
        t = kind.table(),
        cur = current_release("b"),
        pred = key_pred(kind, "b", 1),
    );
    let mut q = sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(sql));
    for v in vals {
        q = q.bind(v);
    }
    Ok(q.fetch_optional(&mut *tx).await?.flatten())
}

/// The tenant row of `key`: `(hidden, base_hash)`, or `None`.
pub async fn tenant_row(
    tx: &mut PgConnection,
    kind: ObjectKind,
    key: &NaturalKey,
) -> ApiResult<Option<(bool, Option<String>)>> {
    let vals = key_values(kind, key);
    if kind == ObjectKind::JobSetting {
        let v: Option<Option<Value>> = sqlx::query_scalar(
            "SELECT settings_override -> $1 FROM jobs WHERE id = app_current_job()",
        )
        .bind(&vals[0])
        .fetch_optional(&mut *tx)
        .await?;
        return Ok(v.flatten().map(|o| {
            (
                o.get("value").is_none_or(Value::is_null),
                o.get("base_hash")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )
        }));
    }
    let sql = format!(
        "SELECT t.hidden, t.base_hash FROM {tab} t WHERE t.origin = 'tenant' AND {pred}",
        tab = kind.table(),
        pred = key_pred(kind, "t", 1),
    );
    let mut q = sqlx::query_as::<_, (bool, Option<String>)>(AssertSqlSafe(sql));
    for v in vals {
        q = q.bind(v);
    }
    Ok(q.fetch_optional(&mut *tx).await?)
}

/// Inserts a tenant copy of the current base row of `key` (`hidden` = a tombstone). A procedure tombstone is stored
/// ARCHIVED so it never occupies the tenant's one-PUBLISHED-per-name slot.
async fn copy_base(
    tx: &mut PgConnection,
    kind: ObjectKind,
    key: &NaturalKey,
    hidden: bool,
    base_hash: &str,
) -> ApiResult<()> {
    let cols = kind.content_columns();
    let select: Vec<String> = cols
        .iter()
        .map(|c| {
            if kind == ObjectKind::Procedure && hidden && *c == "version_status" {
                "'ARCHIVED'::version_status".to_string()
            } else {
                format!("b.{c}")
            }
        })
        .collect();
    let k = kind.key_columns().len();
    let sql = format!(
        "INSERT INTO {t} ({cols}, hidden, base_hash)
         SELECT {select}, ${h}::boolean, ${bh}::text FROM {t} b
          WHERE b.origin = 'base' AND b.release_id = {cur} AND {pred}",
        t = kind.table(),
        cols = cols.join(", "),
        select = select.join(", "),
        h = k + 1,
        bh = k + 2,
        cur = current_release("b"),
        pred = key_pred(kind, "b", 1),
    );
    let mut q = sqlx::query(AssertSqlSafe(sql));
    for v in key_values(kind, key) {
        q = q.bind(v);
    }
    q.bind(hidden)
        .bind(base_hash.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(())
}

fn target_missing(kind: ObjectKind) -> ApiError {
    ApiError::Conflict(
        "refinement_target_missing",
        format!(
            "There is no base {} with that key to refine.",
            kind.as_str().replace('_', " ")
        ),
    )
}

/// Copy-on-write: makes sure a tenant row of `key` exists and says what it is. An existing tenant row is classified;
/// otherwise a base row is copied as an override (`base_hash` = its content_hash); with neither, `Extension` is
/// returned and nothing is written (the caller inserts). Callers then UPDATE/INSERT `WHERE origin = 'tenant'` only.
/// A procedure version is never overridden (409 `base_row_read_only`); a connector slot without `base_url` cannot
/// be copied (the tenant row needs one), so the connector routes upsert instead.
pub async fn ensure_tenant_row(
    tx: &mut PgConnection,
    kind: ObjectKind,
    key: &NaturalKey,
) -> ApiResult<RowKind> {
    if kind == ObjectKind::JobSetting {
        let vals = key_values(kind, key);
        return Ok(if base_setting(tx, &vals[0]).await?.is_some() {
            RowKind::Override
        } else {
            RowKind::Extension
        });
    }
    if let Some((hidden, bh)) = tenant_row(tx, kind, key).await? {
        return Ok(RowKind::of(hidden, bh.as_deref()));
    }
    match base_hash_of(tx, kind, key).await? {
        Some(_) if kind == ObjectKind::Procedure => Err(base_row_read_only()),
        Some(h) => {
            copy_base(tx, kind, key, false, &h).await?;
            Ok(RowKind::Override)
        }
        None => Ok(RowKind::Extension),
    }
}

/// 409 `base_row_read_only`: a base procedure version changes only with a release.
pub fn base_row_read_only() -> ApiError {
    ApiError::Conflict(
        "base_row_read_only",
        "This version belongs to the job's base release and is read-only; add a version of your own instead."
            .into(),
    )
}

/// Hides `key`: an existing tenant row becomes hidden, otherwise a tombstone copy of the base row is written.
/// 409 `refinement_target_missing` when there is neither.
pub async fn hide(tx: &mut PgConnection, kind: ObjectKind, key: &NaturalKey) -> ApiResult<()> {
    let base = base_hash_of(tx, kind, key).await?;
    if kind == ObjectKind::JobSetting {
        if base.is_none() {
            return Err(target_missing(kind));
        }
        return set_setting(tx, &key_values(kind, key)[0], None).await;
    }
    let tenant = tenant_row(tx, kind, key).await?;
    match tenant {
        Some((true, _)) => Ok(()),
        Some((false, _)) => {
            let k = kind.key_columns().len();
            let sql = format!(
                "UPDATE {t} t SET hidden = true, base_hash = ${bh}::text WHERE t.origin = 'tenant' AND {pred}",
                t = kind.table(),
                bh = k + 1,
                pred = key_pred(kind, "t", 1),
            );
            let mut q = sqlx::query(AssertSqlSafe(sql));
            for v in key_values(kind, key) {
                q = q.bind(v);
            }
            q.bind(base).execute(&mut *tx).await?;
            Ok(())
        }
        None => match base {
            Some(h) => copy_base(tx, kind, key, true, &h).await,
            None => Err(target_missing(kind)),
        },
    }
}

/// Un-hides `key`: a pure tombstone (its content equals the base row's) is deleted, a hidden override or extension
/// keeps its values and becomes visible. Idempotent: nothing hidden, nothing done.
pub async fn unhide(tx: &mut PgConnection, kind: ObjectKind, key: &NaturalKey) -> ApiResult<()> {
    let vals = key_values(kind, key);
    if kind == ObjectKind::JobSetting {
        if setting_override(tx, &vals[0]).await? == Some(None) {
            remove_setting(tx, &vals[0]).await?;
        }
        return Ok(());
    }
    if !matches!(tenant_row(tx, kind, key).await?, Some((true, _))) {
        return Ok(());
    }
    let pure = if base_hash_of(tx, kind, key).await?.is_none() {
        false
    } else if kind == ObjectKind::Procedure {
        // a tenant row with a base version's (name, version) can only be a tombstone
        true
    } else {
        let cols = kind.content_columns();
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {t} x JOIN {t} b ON {join}
               WHERE x.origin = 'tenant' AND {pred} AND b.origin = 'base' AND b.release_id = {cur}
                 AND ({xc}) IS NOT DISTINCT FROM ({bc}))",
            t = kind.table(),
            join = key_join(kind, "x", "b"),
            pred = key_pred(kind, "x", 1),
            cur = current_release("b"),
            xc = cols
                .iter()
                .map(|c| format!("x.{c}"))
                .collect::<Vec<_>>()
                .join(", "),
            bc = cols
                .iter()
                .map(|c| format!("b.{c}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let mut q = sqlx::query_scalar::<_, bool>(AssertSqlSafe(sql));
        for v in &vals {
            q = q.bind(v.clone());
        }
        q.fetch_one(&mut *tx).await?
    };
    let sql = if pure {
        format!(
            "DELETE FROM {t} t WHERE t.origin = 'tenant' AND {pred}",
            t = kind.table(),
            pred = key_pred(kind, "t", 1)
        )
    } else {
        format!(
            "UPDATE {t} t SET hidden = false WHERE t.origin = 'tenant' AND {pred}",
            t = kind.table(),
            pred = key_pred(kind, "t", 1)
        )
    };
    let mut q = sqlx::query(AssertSqlSafe(sql));
    for v in vals {
        q = q.bind(v);
    }
    q.execute(&mut *tx).await?;
    Ok(())
}

/// Reverts `key` to the base: deletes the tenant row and its tenant children (an entity's attributes, a connector's
/// field mappings; an extension entity's relations too, since nothing is left for them to point at; a procedure's
/// steps cascade), then resolves the key's open conflicts as `reverted`. Idempotent.
pub async fn revert(
    tx: &mut PgConnection,
    kind: ObjectKind,
    key: &NaturalKey,
    actor: &str,
) -> ApiResult<()> {
    let vals = key_values(kind, key);
    match kind {
        ObjectKind::JobSetting => {
            remove_setting(tx, &vals[0]).await?;
            resolve_conflicts(tx, kind, key, "reverted", actor).await?;
            return Ok(());
        }
        ObjectKind::OntologyEntity => {
            sqlx::query("DELETE FROM ontology_attributes WHERE origin = 'tenant' AND entity = $1")
                .bind(&vals[0])
                .execute(&mut *tx)
                .await?;
            if base_hash_of(tx, kind, key).await?.is_none() {
                sqlx::query(
                    "DELETE FROM ontology_relations WHERE origin = 'tenant' AND (subject = $1 OR object = $1)",
                )
                .bind(&vals[0])
                .execute(&mut *tx)
                .await?;
            }
        }
        ObjectKind::Connector => {
            sqlx::query(
                "DELETE FROM connector_field_mappings WHERE origin = 'tenant' AND connector_id = $1",
            )
            .bind(&vals[0])
            .execute(&mut *tx)
            .await?;
        }
        _ => {}
    }
    let sql = format!(
        "DELETE FROM {t} t WHERE t.origin = 'tenant' AND {pred}",
        t = kind.table(),
        pred = key_pred(kind, "t", 1)
    );
    let mut q = sqlx::query(AssertSqlSafe(sql));
    for v in vals {
        q = q.bind(v);
    }
    q.execute(&mut *tx).await?;
    resolve_conflicts(tx, kind, key, "reverted", actor).await?;
    Ok(())
}

/// Accepts the current base under a refinement: `base_hash` := the current base content_hash (NULL when the base row
/// is gone: the refinement becomes an extension), and the key's open conflicts are resolved as `rebased`.
/// 404 when the key has no tenant row.
pub async fn rebase(
    tx: &mut PgConnection,
    kind: ObjectKind,
    key: &NaturalKey,
    actor: &str,
) -> ApiResult<()> {
    let vals = key_values(kind, key);
    if kind == ObjectKind::JobSetting {
        let Some(value) = setting_override(tx, &vals[0]).await? else {
            return Err(ApiError::NotFound("refinement".into()));
        };
        set_setting(tx, &vals[0], value.as_deref()).await?;
    } else {
        let current = base_hash_of(tx, kind, key).await?;
        refresh_base_hash(tx, kind, key, current.as_deref()).await?;
        if tenant_row(tx, kind, key).await?.is_none() {
            return Err(ApiError::NotFound("refinement".into()));
        }
    }
    resolve_conflicts(tx, kind, key, "rebased", actor).await?;
    Ok(())
}

/// `base_hash` of the tenant row of `key` := `base_hash` (the hash the admin wrote against).
pub async fn refresh_base_hash(
    tx: &mut PgConnection,
    kind: ObjectKind,
    key: &NaturalKey,
    base_hash: Option<&str>,
) -> ApiResult<()> {
    let k = kind.key_columns().len();
    let sql = format!(
        "UPDATE {t} t SET base_hash = ${bh}::text WHERE t.origin = 'tenant' AND {pred}",
        t = kind.table(),
        bh = k + 1,
        pred = key_pred(kind, "t", 1),
    );
    let mut q = sqlx::query(AssertSqlSafe(sql));
    for v in key_values(kind, key) {
        q = q.bind(v);
    }
    q.bind(base_hash.map(str::to_string))
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Resolves the open `release_conflicts` of (`kind`, `key`) with `resolution` (`rebased` | `reverted`); returns how
/// many. The stored `key` is the natural key object, so jsonb equality matches whatever its field order.
pub async fn resolve_conflicts(
    tx: &mut PgConnection,
    kind: ObjectKind,
    key: &NaturalKey,
    resolution: &str,
    actor: &str,
) -> ApiResult<u64> {
    let r = sqlx::query(
        "UPDATE release_conflicts SET resolved_at = now(), resolution = $1, resolved_by = $2
          WHERE resolved_at IS NULL AND object = $3 AND key = $4",
    )
    .bind(resolution)
    .bind(actor)
    .bind(kind.as_str())
    .bind(Value::Object(key.clone()))
    .execute(&mut *tx)
    .await?;
    Ok(r.rows_affected())
}

/// Which rows a list route serves (`?layer=`): the effective view (default), the current release's base rows, or the
/// tenant rows including tombstones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    #[default]
    Effective,
    Base,
    Tenant,
}

/// A FROM-able relation for `kind` in `layer`: the table's columns plus `overridden` (and `shadowed` for
/// procedures). `overridden` = a tenant row whose key also has a current base row.
pub fn layer_source(kind: ObjectKind, layer: Layer) -> String {
    let t = kind.table();
    let shadowed = if kind == ObjectKind::Procedure {
        ", false AS shadowed"
    } else {
        ""
    };
    match layer {
        Layer::Effective => format!("eff_{t}"),
        Layer::Base => format!(
            "(SELECT t.*, false AS overridden{shadowed} FROM {t} t WHERE t.origin = 'base' AND t.release_id = {cur})",
            cur = current_release("t"),
        ),
        Layer::Tenant => format!(
            "(SELECT t.*, EXISTS (SELECT 1 FROM {t} b WHERE b.origin = 'base' AND b.release_id = {cur} AND {join}) AS overridden{shadowed}
                FROM {t} t WHERE t.origin = 'tenant')",
            cur = current_release("b"),
            join = key_join(kind, "b", "t"),
        ),
    }
}

/// `?layer=` of the list routes; an unknown value is 400 `invalid_query` (ApiQuery).
#[derive(Debug, Default, serde::Deserialize)]
pub struct LayerQ {
    pub layer: Option<Layer>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_and_keys_validate() {
        for k in ObjectKind::ALL {
            assert_eq!(ObjectKind::parse(k.as_str()).unwrap(), k);
        }
        assert_eq!(
            ObjectKind::parse("nope").unwrap_err().code(),
            "invalid_object"
        );
        let k = ObjectKind::OntologyRelation;
        assert!(
            k.key(&json!({"subject": "A", "predicate": "p", "object": "B"}))
                .is_ok()
        );
        for bad in [
            json!({"subject": "A", "predicate": "p"}),
            json!({"subject": "A", "predicate": "p", "object": "B", "x": "y"}),
            json!({"subject": "A", "predicate": "p", "object": 1}),
            json!({"subject": "", "predicate": "p", "object": "B"}),
            json!(["A"]),
        ] {
            assert_eq!(k.key(&bad).unwrap_err().code(), "invalid_key", "{bad}");
        }
        assert!(
            ObjectKind::JobSetting
                .key(&json!({"key": "stance"}))
                .is_ok()
        );
        assert!(
            ObjectKind::JobSetting
                .key(&json!({"key": "colour"}))
                .is_err()
        );
        assert_eq!(
            Value::Object(ObjectKind::Procedure.key_of(&["return", "1.3.0"])),
            json!({"name": "return", "version": "1.3.0"})
        );
    }

    #[test]
    fn row_kind_classifies_tenant_rows() {
        assert_eq!(RowKind::of(true, Some("h")), RowKind::Tombstone);
        assert_eq!(RowKind::of(false, Some("h")), RowKind::Override);
        assert_eq!(RowKind::of(false, None), RowKind::Extension);
    }

    #[test]
    fn predicates_number_their_parameters() {
        assert_eq!(
            key_pred(ObjectKind::ConnectorFieldMapping, "t", 1),
            "t.connector_id = $1 AND t.backend_field = $2"
        );
        assert!(layer_source(ObjectKind::Procedure, Layer::Base).contains("false AS shadowed"));
        assert_eq!(
            layer_source(ObjectKind::ContextSpace, Layer::Effective),
            "eff_context_spaces"
        );
        assert!(setting_hash("formal").starts_with("sha256:"));
    }
}
