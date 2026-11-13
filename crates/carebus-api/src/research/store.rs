//! Every read and write of a run, each in its own `begin_scoped(tenant, job)` transaction (RLS). Writes that
//! advance a run check the lease (`lease_owner = owner`) in the same transaction, so a runner that lost its lease
//! (expired, re-claimed elsewhere) cannot write a step.
use anyhow::Result;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::verify::{CitationIn, ToolStepView, Verdict, VerifyStatus};
use super::{SourceKind, tools::SUBMIT_REPORT};
use crate::tenancy::begin_scoped;

/// The scope a claimed run continues under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunScope {
    pub tenant: Uuid,
    pub job: Uuid,
    pub run: Uuid,
}

/// `carebus_research_claim`: lease the oldest claimable run (cross-tenant, definer function).
pub async fn claim(db: &PgPool, owner: &str, lease_s: i32) -> Result<Option<RunScope>> {
    let row: Option<(Uuid, Uuid, Uuid)> =
        sqlx::query_as("SELECT tenant_id, job_id, id FROM carebus_research_claim($1, $2)")
            .bind(owner)
            .bind(lease_s)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(tenant, job, run)| RunScope { tenant, job, run }))
}

#[derive(Debug, sqlx::FromRow)]
pub struct RunRow {
    pub profile: String,
    pub question: String,
    pub store_ids: Vec<String>,
    pub status: String,
    pub budget: Value,
    pub tokens_used: i64,
    pub cancel_requested: bool,
    pub lease_owner: Option<String>,
    /// Claims of the run so far (`carebus_research_claim` counts every one).
    pub attempts: i32,
    /// Seconds since the run first started.
    pub elapsed_s: f64,
    /// The run's job key (metering attribution).
    pub job_key: String,
    /// The legal matter the run is filed under (migration 0026), and who filed it.
    pub matter_id: Option<Uuid>,
    pub created_by: String,
}

pub async fn load_run(db: &PgPool, s: RunScope) -> Result<Option<RunRow>> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    Ok(sqlx::query_as(
        "SELECT profile, question, store_ids, status, budget, tokens_used, cancel_requested, lease_owner, attempts,
                extract(epoch FROM now() - coalesce(started_at, now()))::float8 AS elapsed_s,
                coalesce((SELECT j.key FROM jobs j WHERE j.id = research_runs.job_id), '') AS job_key,
                matter_id, created_by
           FROM research_runs WHERE id = $1",
    )
    .bind(s.run)
    .fetch_optional(&mut *tx)
    .await?)
}

/// The walled stores of the tenant (every job, migration 0026) a run filed under `matter` by `creator` may not
/// search: every store of another matter (of any, for a run without a matter), and the stores of its own matter once
/// `creator` is no longer a member of it. Canonical ids, as `legal_matter_stores` holds them.
pub async fn forbidden_walls(
    db: &PgPool,
    tenant: Uuid,
    matter: Option<Uuid>,
    creator: &str,
) -> Result<Vec<String>> {
    let mut tx = begin_scoped(db, tenant, None).await?;
    Ok(sqlx::query_scalar(
        "SELECT w.store_id FROM carebus_store_walls() w
          WHERE w.matter_id IS DISTINCT FROM $1
             OR NOT EXISTS (SELECT 1 FROM legal_matter_members m
                             WHERE m.matter_id = w.matter_id AND m.user_sub = $2)
          ORDER BY w.store_id",
    )
    .bind(matter)
    .bind(creator)
    .fetch_all(&mut *tx)
    .await?)
}

/// The tenant's `external_llm_allowed` (migration 0023; no row = false), read under its RLS scope.
pub async fn external_llm_allowed(db: &PgPool, tenant: Uuid) -> Result<bool> {
    let mut tx = begin_scoped(db, tenant, None).await?;
    let allowed = crate::routes::llm_providers::external_llm_allowed(&mut tx).await?;
    Ok(allowed)
}

/// What a lease renewal reads back.
#[derive(Debug, sqlx::FromRow)]
pub struct Heartbeat {
    pub status: String,
    pub cancel_requested: bool,
    pub tokens_used: i64,
    pub elapsed_s: f64,
}

/// Extends the lease; `None` when this runner no longer holds it.
pub async fn renew(
    db: &PgPool,
    s: RunScope,
    owner: &str,
    lease_s: i32,
) -> Result<Option<Heartbeat>> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    let hb = sqlx::query_as(
        "UPDATE research_runs
            SET lease_until = now() + make_interval(secs => $3), updated_at = now()
          WHERE id = $1 AND lease_owner = $2
         RETURNING status, cancel_requested, tokens_used,
                   extract(epoch FROM now() - coalesce(started_at, now()))::float8 AS elapsed_s",
    )
    .bind(s.run)
    .bind(owner)
    .bind(lease_s)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(hb)
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct StepRow {
    pub ordinal: i32,
    pub kind: String,
    pub request: Value,
    pub response: Option<Value>,
    pub status: String,
}

pub async fn load_steps(db: &PgPool, s: RunScope) -> Result<Vec<StepRow>> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    Ok(sqlx::query_as(
        "SELECT ordinal, kind, request, response, status FROM research_steps WHERE run_id = $1 ORDER BY ordinal",
    )
    .bind(s.run)
    .fetch_all(&mut *tx)
    .await?)
}

pub struct NewStep {
    pub ordinal: i32,
    pub kind: &'static str,
    pub request: Value,
    pub response: Value,
    pub status: &'static str,
    pub duration_ms: i32,
    /// Model turns this step adds to `steps_used` (1 for a model step).
    pub turns: i32,
    pub tokens: i64,
}

async fn insert_step(
    tx: &mut sqlx::PgConnection,
    s: RunScope,
    owner: &str,
    lease_s: i32,
    st: &NewStep,
) -> Result<bool> {
    let held: Option<Uuid> = sqlx::query_scalar(
        "UPDATE research_runs
            SET steps_used = steps_used + $3, tokens_used = tokens_used + $4,
                lease_until = now() + make_interval(secs => $5), updated_at = now()
          WHERE id = $1 AND lease_owner = $2 AND status IN ('running', 'verifying')
         RETURNING id",
    )
    .bind(s.run)
    .bind(owner)
    .bind(st.turns)
    .bind(st.tokens)
    .bind(lease_s)
    .fetch_optional(&mut *tx)
    .await?;
    if held.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO research_steps (run_id, ordinal, kind, request, response, status, duration_ms)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(s.run)
    .bind(st.ordinal)
    .bind(st.kind)
    .bind(&st.request)
    .bind(&st.response)
    .bind(st.status)
    .bind(st.duration_ms)
    .execute(&mut *tx)
    .await?;
    Ok(true)
}

/// Writes one step and advances the counters; `false` (nothing written) when the lease is lost.
pub async fn append_step(
    db: &PgPool,
    s: RunScope,
    owner: &str,
    lease_s: i32,
    st: &NewStep,
) -> Result<bool> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    if !insert_step(&mut tx, s, owner, lease_s, st).await? {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// How [`park_on_task`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parked {
    /// The run is `waiting` now (no lease): the task's end requeues it.
    Waiting,
    /// The task had already ended: its result is the step's, and the run goes on under this lease.
    Settled,
    LeaseLost,
}

/// The tool step of a call that started MCP task `task_id`, and the run parked as `waiting`, in one transaction (no
/// lease, one claim less: waiting must not use up `max_attempts`). The task row is locked first, like
/// `mcp_tasks::finish` does: a task that ended before this commit is seen as ended and its result is written as the
/// step instead of parking the run (nobody would requeue it).
pub async fn park_on_task(
    db: &PgPool,
    s: RunScope,
    owner: &str,
    lease_s: i32,
    st: &NewStep,
    task_id: &str,
    tool: &str,
) -> Result<Parked> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    let task: Option<(Option<Value>,)> = sqlx::query_as(
        "SELECT CASE WHEN terminal_at IS NULL THEN NULL ELSE result END FROM mcp_tasks
          WHERE task_id = $1 AND run_id = $2 FOR UPDATE",
    )
    .bind(task_id)
    .bind(s.run)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((result,)) = task else {
        anyhow::bail!("MCP task {task_id} of run {} is not recorded", s.run);
    };
    if let Some(result) = result {
        let (ok, response) = crate::mcp_tasks::step_response(tool, task_id, &result);
        let settled = NewStep {
            ordinal: st.ordinal,
            kind: st.kind,
            request: st.request.clone(),
            response,
            status: if ok { "ok" } else { "error" },
            duration_ms: st.duration_ms,
            turns: st.turns,
            tokens: st.tokens,
        };
        if !insert_step(&mut tx, s, owner, lease_s, &settled).await? {
            return Ok(Parked::LeaseLost);
        }
        tx.commit().await?;
        return Ok(Parked::Settled);
    }
    if !insert_step(&mut tx, s, owner, lease_s, st).await? {
        return Ok(Parked::LeaseLost);
    }
    sqlx::query(
        "UPDATE research_runs
            SET status = 'waiting', lease_owner = NULL, lease_until = NULL,
                attempts = GREATEST(attempts - 1, 0), updated_at = now()
          WHERE id = $1",
    )
    .bind(s.run)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Parked::Waiting)
}

/// A validated `submit_report` (its free-text `summary_markdown` stays in the step row only).
#[derive(Clone, Debug)]
pub struct ReportIn {
    pub claims: Vec<(String, Vec<CitationIn>)>,
}

/// The `submit_report` step, its claims and citations, and status `verifying`, in one transaction.
pub async fn accept_report(
    db: &PgPool,
    s: RunScope,
    owner: &str,
    lease_s: i32,
    st: &NewStep,
    report: &ReportIn,
) -> Result<bool> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    if !insert_step(&mut tx, s, owner, lease_s, st).await? {
        return Ok(false);
    }
    for (i, (text, cits)) in report.claims.iter().enumerate() {
        let mut ids = vec![];
        for c in cits {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO research_citations (run_id, id, source_kind, locator, title, quote, step_ordinal)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(s.run)
            .bind(id)
            .bind(c.source_kind.as_str())
            .bind(&c.locator)
            .bind(&c.title)
            .bind(&c.quote)
            .bind(c.step_ordinal)
            .execute(&mut *tx)
            .await?;
            ids.push(id);
        }
        sqlx::query(
            "INSERT INTO research_claims (run_id, id, ordinal, text, citation_ids) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(s.run)
        .bind(Uuid::new_v4())
        .bind(i32::try_from(i).unwrap_or(i32::MAX))
        .bind(text)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE research_runs SET status = 'verifying', updated_at = now() WHERE id = $1")
        .bind(s.run)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// A terminal status (`done`, `failed`, `cancelled`); the lease ends now. `false` when the lease is lost.
pub async fn finish(
    db: &PgPool,
    s: RunScope,
    owner: &str,
    status: &str,
    error: Option<&str>,
    report: Option<&str>,
) -> Result<bool> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    let done: Option<Uuid> = sqlx::query_scalar(
        "UPDATE research_runs
            SET status = $3, error = $4, report = coalesce($5, report), finished_at = now(),
                lease_until = now(), updated_at = now()
          WHERE id = $1 AND lease_owner = $2
         RETURNING id",
    )
    .bind(s.run)
    .bind(owner)
    .bind(status)
    .bind(error)
    .bind(report)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(done.is_some())
}

/// Gives the lease up (shutdown): the run is re-claimable at once, and the claim does not count as an attempt.
pub async fn release(db: &PgPool, s: RunScope, owner: &str) -> Result<()> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    sqlx::query(
        "UPDATE research_runs
            SET lease_until = now(), attempts = GREATEST(attempts - 1, 0), updated_at = now()
          WHERE id = $1 AND lease_owner = $2",
    )
    .bind(s.run)
    .bind(owner)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub struct StoredCitation {
    pub id: Uuid,
    pub citation: CitationIn,
}

pub struct StoredClaim {
    pub id: Uuid,
    pub text: String,
    pub citation_ids: Vec<Uuid>,
}

/// What the verifier reads: the claims, the citations and the successful tool steps (not `submit_report`).
pub struct VerifyInput {
    pub claims: Vec<StoredClaim>,
    pub citations: Vec<StoredCitation>,
    pub steps: Vec<ToolStepView>,
}

pub async fn load_verify_input(db: &PgPool, s: RunScope) -> Result<VerifyInput> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    let claims: Vec<(Uuid, String, Vec<Uuid>)> = sqlx::query_as(
        "SELECT id, text, citation_ids FROM research_claims WHERE run_id = $1 ORDER BY ordinal",
    )
    .bind(s.run)
    .fetch_all(&mut *tx)
    .await?;
    type Cit = (Uuid, String, String, Option<String>, String, Option<i32>);
    let cits: Vec<Cit> = sqlx::query_as(
        "SELECT id, source_kind, locator, title, quote, step_ordinal FROM research_citations WHERE run_id = $1
          ORDER BY created_at, id",
    )
    .bind(s.run)
    .fetch_all(&mut *tx)
    .await?;
    let rows: Vec<(i32, Value, Option<Value>, String)> = sqlx::query_as(
        "SELECT ordinal, request, response, status FROM research_steps
          WHERE run_id = $1 AND kind = 'tool' ORDER BY ordinal",
    )
    .bind(s.run)
    .fetch_all(&mut *tx)
    .await?;
    let mut steps = vec![];
    for (ordinal, req, resp, status) in rows {
        let name = req["name"].as_str().unwrap_or("").to_string();
        if status != "ok" || name == SUBMIT_REPORT {
            continue;
        }
        steps.push(ToolStepView {
            ordinal,
            name,
            arguments: req["arguments"].clone(),
            text: resp
                .as_ref()
                .and_then(|r| r["text"].as_str())
                .unwrap_or("")
                .to_string(),
            truncated: resp
                .as_ref()
                .and_then(|r| r["truncated"].as_bool())
                .unwrap_or(false),
        });
    }
    Ok(VerifyInput {
        claims: claims
            .into_iter()
            .map(|(id, text, citation_ids)| StoredClaim {
                id,
                text,
                citation_ids,
            })
            .collect(),
        citations: cits
            .into_iter()
            .map(
                |(id, kind, locator, title, quote, step_ordinal)| StoredCitation {
                    id,
                    citation: CitationIn {
                        source_kind: SourceKind::parse(&kind).unwrap_or(SourceKind::Other),
                        locator,
                        title,
                        quote,
                        step_ordinal,
                    },
                },
            )
            .collect(),
        steps,
    })
}

/// How [`save_verification`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
    Done,
    /// A cancel was requested meanwhile: the verdicts are kept, the run is `cancelled` without a report.
    Cancelled,
    LeaseLost,
}

/// Verdicts, claim statuses, the memo and status `done`, in one transaction; `cancelled` (no memo) when a cancel
/// was requested before this write.
pub async fn save_verification(
    db: &PgPool,
    s: RunScope,
    owner: &str,
    verdicts: &[(Uuid, Verdict)],
    claims: &[(Uuid, bool)],
    report: &str,
) -> Result<Saved> {
    let mut tx = begin_scoped(db, s.tenant, Some(s.job)).await?;
    let held: Option<bool> = sqlx::query_scalar(
        "SELECT cancel_requested FROM research_runs
          WHERE id = $1 AND lease_owner = $2 AND status = 'verifying' FOR UPDATE",
    )
    .bind(s.run)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(cancel) = held else {
        return Ok(Saved::LeaseLost);
    };
    for (id, v) in verdicts {
        sqlx::query(
            "UPDATE research_citations SET verify_status = $2, verify_detail = $3, verified_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(v.status.as_str())
        .bind(&v.detail)
        .execute(&mut *tx)
        .await?;
    }
    for (id, supported) in claims {
        sqlx::query("UPDATE research_claims SET status = $2 WHERE id = $1")
            .bind(id)
            .bind(if *supported {
                "supported"
            } else {
                "unsupported"
            })
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE research_runs
            SET status = CASE WHEN $3 THEN 'cancelled' ELSE 'done' END,
                report = CASE WHEN $3 THEN NULL ELSE $2 END,
                error = NULL, finished_at = now(), lease_until = now(), updated_at = now()
          WHERE id = $1",
    )
    .bind(s.run)
    .bind(report)
    .bind(cancel)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(if cancel {
        Saved::Cancelled
    } else {
        Saved::Done
    })
}

/// `{"status": "unverifiable"…}` for a citation id a claim names but no row has (never expected).
pub fn missing_citation() -> Verdict {
    Verdict {
        status: VerifyStatus::Unverifiable,
        detail: json!({"reason": "citation_missing"}),
    }
}
