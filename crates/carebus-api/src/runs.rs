//! Re-sync runs: start one through strata_ingest, follow its jobs when the run is READ (no background worker),
//! and on completion replace earlier runs' documents in strata_rag. Deleting a source is here too, because
//! which strata documents a source owns is a property of its runs.
use std::collections::{BTreeSet, HashSet};

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit;
use crate::error::{ApiError, ApiResult};
use crate::ingest::{self, Created};
use crate::sources::{self, Credentials, SourceRecord, SourceType, WebsiteMode};
use crate::state::AppState;
use crate::strata::{self, Gone};
use crate::tenancy::JobCtx;
use crate::upstream::Payload;

pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_UPLOAD_FILES: usize = 20;
const PARALLEL_DELETES: usize = 8;

/// A running run with no recorded job this long after it started was orphaned by a restart mid-start.
fn orphan_after() -> chrono::Duration {
    chrono::Duration::seconds(60)
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SourceRun {
    pub id: Uuid,
    pub source_id: Uuid,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub started_by: String,
    pub ingest_job_ids: Vec<String>,
    pub document_ids: Vec<String>,
    pub error: Option<String>,
    pub replaced_run_id: Option<Uuid>,
    pub replace_report: Option<Value>,
    /// strata_ingest could not be asked during this read: `status` may be behind.
    #[sqlx(skip)]
    pub stale: bool,
    /// While a crawl or sitemap run is followed: its page jobs, `{total, by_status}` summed over the run's parent
    /// jobs. Absent otherwise (it is read live, not stored).
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct SourceDeleted {
    pub id: Uuid,
    pub purged: bool,
    pub documents_deleted: usize,
    pub documents_kept: usize,
}

/// One uploaded file of a file-source re-sync.
pub struct Upload {
    pub file_name: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

const RUN_COLUMNS: &str = "id, source_id, status, started_at, finished_at, started_by, ingest_job_ids, document_ids, error, replaced_run_id, replace_report";

// Every function acts in one job (`ctx`) and opens its own short transaction per database step: none is held
// across a strata call. Inserts take `tenant_id` / `job_id` from the transaction's GUC defaults (migration 0007).

pub async fn load(s: &AppState, ctx: &JobCtx, id: Uuid) -> ApiResult<SourceRun> {
    let mut tx = ctx.begin(&s.db).await?;
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM source_runs WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {id}")))
}

pub async fn running_of(s: &AppState, ctx: &JobCtx, source: Uuid) -> ApiResult<Option<Uuid>> {
    let mut tx = ctx.begin(&s.db).await?;
    Ok(
        sqlx::query_scalar(
            "SELECT id FROM source_runs WHERE source_id = $1 AND status = 'running'",
        )
        .bind(source)
        .fetch_optional(&mut *tx)
        .await?,
    )
}

/// Newest first; `before` is the previous page's last `started_at`.
pub async fn page(
    s: &AppState,
    ctx: &JobCtx,
    source: Uuid,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> ApiResult<Vec<SourceRun>> {
    let mut tx = ctx.begin(&s.db).await?;
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM source_runs WHERE source_id = $1 AND ($2::timestamptz IS NULL OR started_at < $2)
         ORDER BY started_at DESC, id DESC LIMIT $3"
    )))
    .bind(source)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?)
}

/// `running` → final, only if still running. True for the one caller that won.
async fn finish(
    s: &AppState,
    ctx: &JobCtx,
    id: Uuid,
    status: &str,
    docs: &[String],
    error: Option<String>,
) -> ApiResult<bool> {
    let mut tx = ctx.begin(&s.db).await?;
    let done = sqlx::query(
        "UPDATE source_runs SET status = $2, finished_at = now(), document_ids = $3, error = $4
         WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(status)
    .bind(docs)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(done.rows_affected() == 1)
}

/// strata_ingest's message with the secret replaced (before shortening, so no prefix of it survives), at most
/// 500 characters.
fn scrub(message: &str, secret: Option<&str>) -> String {
    let clean = match secret {
        Some(sec) => crate::secrets::scrub_secret(message, sec),
        None => message.to_string(),
    };
    clean.chars().take(500).collect()
}

fn options_of(config: &Value) -> Option<Value> {
    config.get("options").filter(|o| !o.is_null()).cloned()
}

/// A website source's strata_ingest body. `urls` mode is the plain `{urls}` request; `crawl` and `sitemap` are
/// `source: {kind, config}` with the start `url` and that mode's settings flattened into `config`.
fn website_body(config: &Value, store_id: Uuid, source_id: Uuid, options: Option<Value>) -> Value {
    let mode = WebsiteMode::of(config).unwrap_or(WebsiteMode::Urls);
    let mut body = match mode {
        WebsiteMode::Urls => {
            json!({"store_id": store_id, "urls": config["urls"], "source_id": source_id})
        }
        WebsiteMode::Crawl | WebsiteMode::Sitemap => {
            let kind = mode.as_str();
            let mut c = config
                .get(kind)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            c.insert("url".into(), config["url"].clone());
            json!({"store_id": store_id, "source_id": source_id,
                   "source": {"kind": kind, "config": c}})
        }
    };
    if let Some(o) = options {
        body["options"] = o;
    }
    body
}

/// The strata_ingest request for this source (see the table in the plan's Task 7).
fn ingest_payload(
    src: &SourceRecord,
    creds: Option<&Credentials>,
    files: Vec<Upload>,
) -> ApiResult<Payload> {
    let options = options_of(&src.config);
    match src.kind {
        SourceType::Website => Ok(Payload::Json(website_body(
            &src.config,
            src.target_store_id,
            src.id,
            options,
        ))),
        SourceType::Api | SourceType::Datatable => {
            let mut config = src.config.clone();
            if let Some(o) = config.as_object_mut() {
                o.remove("options");
            }
            let mut body = json!({
                "store_id": src.target_store_id,
                "source": {"kind": src.kind.as_str(), "config": config,
                           "credentials": creds.map(|c| json!({"secret": c.secret}))},
                "mapping": src.mapping,
                "source_id": src.id,
            });
            if let Some(o) = options {
                body["options"] = o;
            }
            Ok(Payload::Json(body))
        }
        SourceType::File => {
            if files.is_empty() {
                return Err(sources::bad(
                    "no_file",
                    "Attach one or more files to re-sync a file source.",
                ));
            }
            // strata_ingest takes `store_id`, `source_id` and `options` only before the first file part.
            let mut form = reqwest::multipart::Form::new()
                .text("store_id", src.target_store_id.to_string())
                .text("source_id", src.id.to_string());
            if let Some(o) = options {
                form = form.text("options", o.to_string());
            }
            for f in files {
                let mut part = reqwest::multipart::Part::bytes(f.bytes).file_name(f.file_name);
                if let Some(ct) = f.content_type {
                    part = part.mime_str(&ct).map_err(|_| {
                        sources::bad(
                            "invalid_body",
                            "An uploaded file has an invalid content type.",
                        )
                    })?;
                }
                form = form.part("file", part);
            }
            Ok(Payload::Multipart(form))
        }
    }
}

/// Starts a run. The request is built (and the upload read) BEFORE the run row exists, so a refused request
/// never leaves a run behind; a still-running previous run is refreshed first and then blocks with 409.
pub async fn start(
    s: &AppState,
    ctx: &JobCtx,
    src: &SourceRecord,
    actor: &str,
    files: Vec<Upload>,
) -> ApiResult<(StatusCode, SourceRun)> {
    if !src.enabled {
        return Err(ApiError::Conflict(
            "source_disabled",
            "This source is disabled. Enable it before a re-sync.".into(),
        ));
    }
    let creds = if src.kind.structured() {
        src.credentials(s.cfg.source_key.as_ref())?
    } else {
        None
    };
    let payload = ingest_payload(src, creds.as_ref(), files)?;
    if let Some(prev) = running_of(s, ctx, src.id).await? {
        refresh(s, ctx, prev).await?;
    }
    let run_id = Uuid::new_v4();
    {
        let mut tx = ctx.begin(&s.db).await?;
        let inserted = sqlx::query("INSERT INTO source_runs (id, source_id, status, started_by) VALUES ($1, $2, 'running', $3)")
            .bind(run_id)
            .bind(src.id)
            .bind(actor)
            .execute(&mut *tx)
            .await;
        match inserted {
            Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23505") => {
                return Err(busy());
            }
            r => {
                r?;
            }
        }
        tx.commit().await?;
    }
    match ingest::create(s, ctx.t.tenant, payload).await {
        Ok(Created::Jobs(jobs)) => {
            let job_ids: Vec<String> = jobs.iter().map(|j| j.id.clone()).collect();
            let mut tx = ctx.begin(&s.db).await?;
            sqlx::query("UPDATE source_runs SET ingest_job_ids = $2 WHERE id = $1")
                .bind(run_id)
                .bind(&job_ids)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            // A source that could not be fetched or parsed is a 202 with its job already `failed`
            // (source_auth, fetch_blocked, no_records, …): the run takes the job's outcome now.
            if jobs.iter().any(|j| j.is_terminal()) {
                let run = refresh(s, ctx, run_id).await?;
                if run.status != "running" {
                    return Ok((StatusCode::OK, run));
                }
            }
            Ok((StatusCode::ACCEPTED, load(s, ctx, run_id).await?))
        }
        Ok(Created::Refused {
            status,
            code,
            message,
        }) => {
            // Only a kind strata_ingest does not ingest is `unsupported`; a malformed request is a failure.
            let word = if status == StatusCode::UNPROCESSABLE_ENTITY
                && code.as_deref() == Some("unsupported_source_kind")
            {
                "unsupported"
            } else {
                "failed"
            };
            let msg = scrub(&message, creds.as_ref().map(|c| c.secret.as_str()));
            let error = format!(
                "strata_ingest refused the request (HTTP {}): {msg}",
                status.as_u16()
            );
            finish(s, ctx, run_id, word, &[], Some(error)).await?;
            Ok((StatusCode::OK, load(s, ctx, run_id).await?))
        }
        Err(e) => {
            let why = match &e {
                // A timeout or 5xx does not prove strata_ingest rejected the jobs; it may have accepted them.
                ApiError::Upstream(_) => {
                    "strata_ingest did not confirm the request; check Ingest jobs before re-syncing."
                }
                _ => "carebus-api failed while starting this run.",
            };
            finish(s, ctx, run_id, "failed", &[], Some(why.into())).await?;
            Err(e)
        }
    }
}

/// The source's decrypted secret, or `None` (no credentials, no key, unreadable): only used to scrub messages.
pub(crate) async fn secret_of(s: &AppState, ctx: &JobCtx, source: Uuid) -> Option<String> {
    let rec = sources::record(s, ctx, source).await.ok()?;
    rec.credentials(s.cfg.source_key.as_ref())
        .ok()
        .flatten()
        .map(|c| c.secret)
}

/// Brings a running run up to date from strata_ingest. Any other run is returned as stored.
pub async fn refresh(s: &AppState, ctx: &JobCtx, id: Uuid) -> ApiResult<SourceRun> {
    let mut run = load(s, ctx, id).await?;
    if run.status != "running" {
        return Ok(run);
    }
    if run.ingest_job_ids.is_empty() {
        if Utc::now() - run.started_at > orphan_after() {
            finish(
                s,
                ctx,
                id,
                "failed",
                &[],
                Some("No ingest job was recorded for this run; carebus-api stopped while starting it. Re-sync again.".into()),
            )
            .await?;
            return load(s, ctx, id).await;
        }
        return Ok(run);
    }
    let job_ids = run.ingest_job_ids.clone();
    let tenant = ctx.t.tenant;
    let lookups =
        futures_util::future::join_all(job_ids.iter().map(|j| ingest::get_job(s, tenant, j))).await;
    let mut jobs = Vec::new();
    let mut missing = Vec::new();
    for (jid, r) in job_ids.iter().zip(lookups) {
        match r {
            Ok(Some(j)) => jobs.push(j),
            Ok(None) => missing.push(jid.clone()),
            Err(ApiError::Upstream(_)) => {
                run.stale = true;
                return Ok(run);
            }
            Err(e) => return Err(e),
        }
    }
    run.children = children_summary(&jobs);
    if jobs
        .iter()
        .any(|j| !j.is_terminal() || j.children_pending())
    {
        return Ok(run);
    }
    // A crawl or sitemap job's documents come from its page jobs: they count as the run's jobs, so one failed
    // page fails the run (and, like any failed run, it replaces nothing).
    let child_ids: Vec<String> = jobs
        .iter()
        .flat_map(|j| j.child_job_ids.iter().cloned())
        .collect();
    let children: Vec<(String, ApiResult<Option<ingest::IngestJob>>)> = stream::iter(child_ids)
        .map(move |c| async move {
            let r = ingest::get_job(s, tenant, &c).await;
            (c, r)
        })
        .buffer_unordered(PARALLEL_DELETES)
        .collect()
        .await;
    for (cid, r) in children {
        match r {
            Ok(Some(j)) => jobs.push(j),
            Ok(None) => missing.push(cid),
            Err(ApiError::Upstream(_)) => {
                run.stale = true;
                return Ok(run);
            }
            Err(e) => return Err(e),
        }
    }
    if jobs.iter().any(|j| !j.is_terminal()) {
        return Ok(run);
    }
    let Outcome {
        docs,
        status,
        error,
    } = outcome(&jobs, &missing);
    let error = match error {
        Some(text) => {
            let secret = secret_of(s, ctx, run.source_id).await;
            Some(scrub(&text, secret.as_deref()))
        }
        None => None,
    };
    // Only a completed run replaces: a failed one (a failed job, an empty source's `no_records`, a crawl or
    // sitemap that found nothing) leaves every earlier run's documents in place.
    if finish(s, ctx, id, status, &docs, error).await? && status == "completed" {
        replace(s, ctx, id).await?;
    }
    let mut done = load(s, ctx, id).await?;
    done.children = run.children;
    Ok(done)
}

/// How a settled run ends: its documents (deduplicated, in order), `completed` or `failed`, and the error text
/// (not yet scrubbed of the source's secret).
#[derive(Debug, PartialEq)]
struct Outcome {
    docs: Vec<String>,
    status: &'static str,
    error: Option<String>,
}

/// A crawl or sitemap job: strata_ingest marks it with `children` / `child_job_ids`, and its source kind says so
/// even when it started no page at all.
fn is_parent(j: &ingest::IngestJob) -> bool {
    j.children.is_some()
        || !j.child_job_ids.is_empty()
        || matches!(j.source["kind"].as_str(), Some("crawl" | "sitemap"))
}

/// The outcome of a run whose `jobs` (parents and their page jobs) are all settled; `missing` are job ids
/// strata_ingest no longer knows. A crawl or sitemap run that discovered no page, or whose pages produced no
/// document, FAILS: completing it would let `replace` delete every document of the source's earlier runs, and an
/// empty source must leave those in place.
fn outcome(jobs: &[ingest::IngestJob], missing: &[String]) -> Outcome {
    let mut seen = BTreeSet::new();
    let docs: Vec<String> = jobs
        .iter()
        .filter(|j| j.status == "completed")
        .flat_map(|j| j.document_ids.iter().cloned())
        .filter(|d| seen.insert(d.clone()))
        .collect();
    let mut problems: Vec<String> = jobs
        .iter()
        .filter(|j| j.status != "completed")
        .map(|j| match j.error_text() {
            Some(e) => format!("{} {}: {e}", j.id, j.status),
            None => format!("{} {}", j.id, j.status),
        })
        .collect();
    problems.extend(
        missing
            .iter()
            .map(|m| format!("{m} is unknown to strata_ingest")),
    );
    if !problems.is_empty() {
        let text = format!(
            "{} of {} ingest jobs did not complete: {}",
            problems.len(),
            jobs.len() + missing.len(),
            problems.join("; ")
        );
        return Outcome {
            docs,
            status: "failed",
            error: Some(text),
        };
    }
    let parents: Vec<&ingest::IngestJob> = jobs.iter().filter(|j| is_parent(j)).collect();
    let pages = |p: &ingest::IngestJob| {
        p.children
            .as_ref()
            .and_then(|c| c["total"].as_u64())
            .unwrap_or(0)
            .max(p.child_job_ids.len() as u64)
    };
    let found: u64 = parents.iter().map(|&p| pages(p)).sum();
    let error = if parents.iter().any(|&p| pages(p) == 0) {
        Some(
            "no pages discovered: the crawl or sitemap found no page to ingest (an empty sitemap, or include/exclude \
             left none); the source's earlier documents are kept"
                .to_string(),
        )
    } else if !parents.is_empty() && docs.is_empty() {
        Some(format!(
            "no documents: the {found} pages discovered produced no document; the source's earlier documents are kept"
        ))
    } else {
        None
    };
    match error {
        Some(e) => Outcome {
            docs,
            status: "failed",
            error: Some(e),
        },
        None => Outcome {
            docs,
            status: "completed",
            error: None,
        },
    }
}

/// `{total, by_status}` summed over the jobs that are parents; `None` when none is.
fn children_summary(jobs: &[ingest::IngestJob]) -> Option<Value> {
    let parents: Vec<&Value> = jobs.iter().filter_map(|j| j.children.as_ref()).collect();
    if parents.is_empty() {
        return None;
    }
    let mut total = 0u64;
    let mut by_status = serde_json::Map::new();
    for c in parents {
        total += c["total"].as_u64().unwrap_or(0);
        if let Some(m) = c["by_status"].as_object() {
            for (k, v) in m {
                let n =
                    by_status.get(k).and_then(Value::as_u64).unwrap_or(0) + v.as_u64().unwrap_or(0);
                by_status.insert(k.clone(), json!(n));
            }
        }
    }
    Some(json!({"total": total, "by_status": by_status}))
}

/// Deletes from strata_rag the documents of earlier runs that this run no longer has.
async fn replace(s: &AppState, ctx: &JobCtx, id: Uuid) -> ApiResult<()> {
    let run = load(s, ctx, id).await?;
    let mut tx = ctx.begin(&s.db).await?;
    let prior: Vec<(Uuid, Vec<String>, String)> = sqlx::query_as(
        "SELECT id, document_ids, status FROM source_runs
         WHERE source_id = $1 AND id <> $2 AND status <> 'running' AND NOT documents_removed AND started_at <= $3
         ORDER BY started_at, id",
    )
    .bind(run.source_id)
    .bind(id)
    .bind(run.started_at)
    .fetch_all(&mut *tx)
    .await?;
    drop(tx);
    let keep: HashSet<&str> = run.document_ids.iter().map(String::as_str).collect();
    let candidates: Vec<String> = prior
        .iter()
        .flat_map(|(_, d, _)| d.iter().cloned())
        .collect();
    let elsewhere = owned_by_other_sources(s, ctx, run.source_id, &candidates).await?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut targets = Vec::new();
    let mut shared = 0usize;
    for (_, docs, _) in &prior {
        for d in docs {
            if !seen.insert(d.clone()) {
                continue;
            }
            if keep.contains(d.as_str()) || elsewhere.contains(d) {
                shared += 1;
            } else {
                targets.push(d.clone());
            }
        }
    }
    let tenant = ctx.t.tenant;
    let results: Vec<(String, ApiResult<Gone>)> = stream::iter(targets)
        .map(move |d| async move {
            let r = strata::delete_document(s, tenant, &d).await;
            (d, r)
        })
        .buffer_unordered(PARALLEL_DELETES)
        .collect()
        .await;
    let (mut deleted, mut gone) = (0usize, 0usize);
    let mut failed = Vec::new();
    for (d, r) in results {
        match r {
            Ok(Gone::Deleted) => deleted += 1,
            Ok(Gone::Missing) => gone += 1,
            Err(_) => failed.push(d),
        }
    }
    failed.sort();
    let failed_set: HashSet<&str> = failed.iter().map(String::as_str).collect();
    let mut tx = ctx.begin(&s.db).await?;
    for (pid, docs, _) in &prior {
        if docs.iter().all(|d| !failed_set.contains(d.as_str())) {
            sqlx::query("UPDATE source_runs SET documents_removed = true WHERE id = $1")
                .bind(pid)
                .execute(&mut *tx)
                .await?;
        }
    }
    let replaced = prior
        .iter()
        .rev()
        .find(|(_, _, st)| st == "completed")
        .map(|(p, _, _)| *p);
    let report =
        json!({"deleted": deleted, "already_gone": gone, "kept_shared": shared, "failed": failed});
    sqlx::query("UPDATE source_runs SET replaced_run_id = $2, replace_report = $3 WHERE id = $1")
        .bind(id)
        .bind(replaced)
        .bind(&report)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        "carebus-api",
        "system",
        "replace",
        "source",
        &run.source_id.to_string(),
        failed.is_empty(),
        json!({"run_id": id, "deleted": deleted, "already_gone": gone, "failed": failed.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The subset of `docs` that a run of ANOTHER source still owns. strata_rag dedups on `(store_id, content_hash)`,
/// so two sources feeding one leaf store with the same content share one document id; deleting it for one source
/// would silently empty the other. Read across every job of the tenant (job GUC empty): strata stores are
/// tenant-level, so a source of another job may feed the same store.
async fn owned_by_other_sources(
    s: &AppState,
    ctx: &JobCtx,
    source: Uuid,
    docs: &[String],
) -> ApiResult<HashSet<String>> {
    if docs.is_empty() {
        return Ok(HashSet::new());
    }
    let mut tx = ctx.t.begin(&s.db).await?;
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT d FROM source_runs, unnest(document_ids) AS d
         WHERE source_id <> $1 AND NOT documents_removed AND d = ANY($2)",
    )
    .bind(source)
    .bind(docs)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Deletes a source's config and runs.
/// - Without `purge` its strata_rag documents are KEPT, and the response counts them.
/// - With `purge` they are deleted first. strata_rag unavailable → 503 and the source stays; another failure →
///   409 `purge_incomplete` and the source stays.
/// - A purge keeps (and reports) documents another source's runs still own.
/// - Returns the kept ids for the audit row.
pub async fn delete_source(
    s: &AppState,
    ctx: &JobCtx,
    id: Uuid,
    purge: bool,
) -> ApiResult<(SourceDeleted, Vec<String>)> {
    sources::record(s, ctx, id).await?;
    if let Some(r) = running_of(s, ctx, id).await?
        && refresh(s, ctx, r).await?.status == "running"
    {
        return Err(busy());
    }
    let owned: Vec<String> = {
        let mut tx = ctx.begin(&s.db).await?;
        sqlx::query_scalar(
            "SELECT DISTINCT d FROM source_runs, unnest(document_ids) AS d
             WHERE source_id = $1 AND NOT documents_removed ORDER BY d",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?
    };
    let mut deleted = 0usize;
    let mut shared: Vec<String> = Vec::new();
    if purge {
        let elsewhere = owned_by_other_sources(s, ctx, id, &owned).await?;
        let (kept_shared, targets): (Vec<String>, Vec<String>) =
            owned.iter().cloned().partition(|d| elsewhere.contains(d));
        shared = kept_shared;
        let tenant = ctx.t.tenant;
        let results: Vec<ApiResult<Gone>> = stream::iter(targets)
            .map(move |d| async move { strata::delete_document(s, tenant, &d).await })
            .buffer_unordered(PARALLEL_DELETES)
            .collect()
            .await;
        let (mut unavailable, mut refused) = (false, 0usize);
        for r in results {
            match r {
                Ok(Gone::Deleted) => deleted += 1,
                Ok(Gone::Missing) => {}
                Err(ApiError::Upstream(_)) => unavailable = true,
                Err(_) => refused += 1,
            }
        }
        if unavailable {
            return Err(ApiError::Upstream(strata::NAME));
        }
        if refused > 0 {
            return Err(ApiError::Conflict(
                "purge_incomplete",
                format!(
                    "{refused} documents could not be deleted from strata_rag; the source was kept. Try again."
                ),
            ));
        }
    }
    let mut tx = ctx.begin(&s.db).await?;
    let gone = sqlx::query(
        "DELETE FROM sources WHERE id = $1
         AND NOT EXISTS (SELECT 1 FROM source_runs WHERE source_id = $1 AND status = 'running')",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if gone.rows_affected() == 0 {
        return Err(busy()); // a re-sync started meanwhile
    }
    // Without purge every owned document stays; with purge only those another source still owns.
    let kept_ids = if purge { shared } else { owned };
    Ok((
        SourceDeleted {
            id,
            purged: purge,
            documents_deleted: deleted,
            documents_kept: kept_ids.len(),
        },
        kept_ids,
    ))
}

fn busy() -> ApiError {
    ApiError::Conflict(
        "run_in_progress",
        "A re-sync of this source is still running. Wait for it to finish.".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (Uuid, Uuid) {
        (
            Uuid::parse_str("7f1c9a2e-0000-4000-8000-0000000000a1").unwrap(),
            Uuid::parse_str("7f1c9a2e-0000-4000-8000-0000000000e1").unwrap(),
        )
    }

    #[test]
    fn urls_mode_sends_the_plain_urls_request() {
        let (store, src) = ids();
        let cfg = json!({"urls": ["https://help.example.nl/a"], "options": {"captions": false}});
        let body = website_body(&cfg, store, src, options_of(&cfg));
        assert_eq!(
            body,
            json!({"store_id": store, "source_id": src, "urls": ["https://help.example.nl/a"],
                   "options": {"captions": false}})
        );
        let explicit = json!({"mode": "urls", "urls": ["https://help.example.nl/a"]});
        assert_eq!(
            website_body(&explicit, store, src, None),
            json!({"store_id": store, "source_id": src, "urls": ["https://help.example.nl/a"]})
        );
    }

    #[test]
    fn crawl_mode_sends_a_crawl_source_with_its_settings_flattened() {
        let (store, src) = ids();
        let cfg = json!({"mode": "crawl", "url": "https://help.example.nl/",
                         "crawl": {"max_depth": 3, "max_pages": 80, "allowed_domains": ["help.example.nl"],
                                   "link_query": "returns"},
                         "options": {"language": "nl"}});
        assert_eq!(
            website_body(&cfg, store, src, options_of(&cfg)),
            json!({"store_id": store, "source_id": src, "options": {"language": "nl"},
                   "source": {"kind": "crawl", "config": {
                       "url": "https://help.example.nl/", "max_depth": 3, "max_pages": 80,
                       "allowed_domains": ["help.example.nl"], "link_query": "returns"}}})
        );
        let bare = json!({"mode": "crawl", "url": "https://help.example.nl/"});
        assert_eq!(
            website_body(&bare, store, src, None),
            json!({"store_id": store, "source_id": src,
                   "source": {"kind": "crawl", "config": {"url": "https://help.example.nl/"}}})
        );
    }

    #[test]
    fn sitemap_mode_sends_a_sitemap_source() {
        let (store, src) = ids();
        let cfg = json!({"mode": "sitemap", "url": "https://help.example.nl/sitemap.xml",
                         "sitemap": {"max_urls": 300, "include": ["/help/"], "exclude": ["/archive/"]}});
        let body = website_body(&cfg, store, src, None);
        assert_eq!(
            body,
            json!({"store_id": store, "source_id": src,
                   "source": {"kind": "sitemap", "config": {
                       "url": "https://help.example.nl/sitemap.xml", "max_urls": 300,
                       "include": ["/help/"], "exclude": ["/archive/"]}}})
        );
        assert!(body.get("urls").is_none() && body.get("options").is_none());
    }

    fn job(children: Option<Value>) -> ingest::IngestJob {
        let mut j = json!({"id": "j", "store_id": "s", "source": {}, "options": {}, "meta": {},
                           "status": "running", "error": null, "document_ids": [],
                           "created_at": "2026-10-17T10:00:00Z", "updated_at": "2026-10-17T10:00:00Z"});
        if let Some(c) = children {
            j["children"] = c;
        }
        serde_json::from_value(j).unwrap()
    }

    #[test]
    fn children_are_summed_over_parent_jobs_and_absent_without_one() {
        assert_eq!(children_summary(&[job(None)]), None);
        let a = job(Some(
            json!({"total": 3, "by_status": {"completed": 2, "running": 1}}),
        ));
        let b = job(Some(
            json!({"total": 2, "by_status": {"completed": 1, "failed": 1}}),
        ));
        assert!(a.children_pending() && !b.children_pending());
        assert_eq!(
            children_summary(&[a, job(None), b]),
            Some(json!({"total": 5, "by_status": {"completed": 3, "running": 1, "failed": 1}}))
        );
    }

    #[test]
    fn an_ordinary_job_serialises_without_the_parent_fields() {
        let v = serde_json::to_value(job(None)).unwrap();
        assert!(v.get("children").is_none() && v.get("child_job_ids").is_none());
        let mut p = job(Some(json!({"total": 1, "by_status": {"queued": 1}})));
        p.child_job_ids = vec!["c1".into()];
        let v = serde_json::to_value(p).unwrap();
        assert_eq!(v["child_job_ids"], json!(["c1"]));
        assert_eq!(v["children"]["total"], 1);
    }

    /// A settled job: `id`, `status`, its documents, and (for a parent) its page jobs and summary.
    fn settled(id: &str, status: &str, docs: &[&str]) -> ingest::IngestJob {
        let mut j = job(None);
        j.id = id.into();
        j.status = status.into();
        j.document_ids = docs.iter().map(|d| d.to_string()).collect();
        if status == "failed" {
            j.error = Some(json!({"code": "fetch_failed", "message": "HTTP 404"}));
        }
        j
    }

    fn parent(kind: &str, pages: &[&str], by_status: Value) -> ingest::IngestJob {
        let mut p = settled("p", "completed", &[]);
        p.source = json!({"kind": kind, "config": {"url": "https://help.example.nl/sitemap.xml"}});
        p.children = Some(json!({"total": pages.len(), "by_status": by_status}));
        p.child_job_ids = pages.iter().map(|c| c.to_string()).collect();
        p
    }

    #[test]
    fn a_crawl_or_sitemap_that_discovered_no_page_fails_instead_of_completing_empty() {
        for kind in ["crawl", "sitemap"] {
            let o = outcome(&[parent(kind, &[], json!({}))], &[]);
            assert_eq!(
                o.status, "failed",
                "{kind}: completing would replace with nothing"
            );
            assert!(o.docs.is_empty());
            assert!(
                o.error
                    .as_deref()
                    .unwrap()
                    .starts_with("no pages discovered:"),
                "{o:?}"
            );
        }
        // strata_ingest leaving the summary off a parent that started nothing: its kind still marks it
        let mut bare = settled("p", "completed", &[]);
        bare.source = json!({"kind": "sitemap"});
        assert_eq!(outcome(&[bare], &[]).status, "failed");
    }

    #[test]
    fn a_crawl_whose_pages_produced_no_document_fails_too() {
        let jobs = [
            parent("crawl", &["c1", "c2"], json!({"completed": 2})),
            settled("c1", "completed", &[]),
            settled("c2", "completed", &[]),
        ];
        let o = outcome(&jobs, &[]);
        assert_eq!(o.status, "failed");
        assert_eq!(
            o.error.as_deref(),
            Some(
                "no documents: the 2 pages discovered produced no document; the source's earlier documents are kept"
            )
        );
    }

    #[test]
    fn a_crawl_whose_pages_all_completed_completes_with_their_documents() {
        let jobs = [
            parent("sitemap", &["c1", "c2"], json!({"completed": 2})),
            settled("c1", "completed", &["d1", "d2"]),
            settled("c2", "completed", &["d2", "d3"]),
        ];
        assert_eq!(
            outcome(&jobs, &[]),
            Outcome {
                docs: vec!["d1".into(), "d2".into(), "d3".into()],
                status: "completed",
                error: None
            }
        );
    }

    #[test]
    fn one_failed_page_fails_the_crawl_run() {
        let jobs = [
            parent("crawl", &["c1", "c2"], json!({"completed": 1, "failed": 1})),
            settled("c1", "completed", &["d1"]),
            settled("c2", "failed", &[]),
        ];
        let o = outcome(&jobs, &[]);
        assert_eq!(o.status, "failed");
        assert_eq!(
            o.error.as_deref(),
            Some("1 of 3 ingest jobs did not complete: c2 failed: fetch_failed: HTTP 404")
        );
        assert_eq!(o.docs, ["d1"], "partial documents are still recorded");
    }
}
