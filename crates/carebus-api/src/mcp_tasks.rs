//! Durable MCP tasks (migration 0029; MCP-tasks client spec, 2026-10-30, C3-C6).
//!
//! A long-running MCP tool (one named in `MCP_TASK_TOOLS`) comes back from the server as a task. [`record_started`]
//! keeps the handle in `mcp_tasks`; [`TaskPoller`] (run by the `researcher` bin) claims due rows with
//! `carebus_mcp_task_claim`, polls `tasks/get`, retries cancels and unsent staff answers, and stores the terminal
//! outcome on the row before the server's `ttlMs` drops it. No request and no tokio task is held per task.
//!
//! Answers come only from a human: [`answer`] takes the route's authenticated caller. Nothing here is reachable from
//! an agent tool set (`research::Profile::mcp_allow_list` never names a task method).
use std::sync::Arc;
use std::time::Duration;

use carebus::mcp_http::HttpMcp;
use carebus::mcp_tasks::{
    StaffActor, TaskError, TaskHandle, TaskOutcome, TaskSnapshot, input_questions, outcome_of,
    poll_delay, validate_answer,
};
use carebus::tools::ToolError;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::tenancy::begin_scoped;

/// After a transport failure, the next try.
const RETRY_AFTER: Duration = Duration::from_secs(15);

/// Where a task came from.
pub struct Origin<'a> {
    pub tenant: Uuid,
    pub job: Uuid,
    pub run: Option<Uuid>,
    pub case: Option<Uuid>,
    pub created_by: &'a str,
}

/// Keeps a freshly started task (C3). Its first poll is after the server's `pollIntervalMs`.
pub async fn record_started(
    db: &PgPool,
    o: &Origin<'_>,
    tool: &str,
    h: &TaskHandle,
) -> ApiResult<()> {
    let mut tx = begin_scoped(db, o.tenant, Some(o.job)).await?;
    let status = if h.status == "input_required" {
        "input_required"
    } else {
        "working"
    };
    sqlx::query(
        "INSERT INTO mcp_tasks (tenant_id, job_id, task_id, case_id, run_id, tool, status, status_message,
                                poll_after, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now() + make_interval(secs => $9), $10)",
    )
    .bind(o.tenant)
    .bind(o.job)
    .bind(&h.task_id)
    .bind(o.case)
    .bind(o.run)
    .bind(tool)
    .bind(status)
    .bind(&h.status_message)
    .bind(poll_delay(h.poll_interval_ms).as_secs_f64())
    .bind(o.created_by)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The research step a finished task leaves behind (C6): `(ok, response)` for the step that was waiting on it. An ok
/// result is the text the research tool path gives an MCP result; an error, a cancel or a lost task is the message.
pub fn step_response(tool: &str, task_id: &str, result: &Value) -> (bool, Value) {
    let ok = result["ok"].as_bool().unwrap_or(false);
    let text = if ok {
        crate::research::tools::value_text(&result["result"])
    } else {
        match result["outcome"].as_str() {
            Some("cancelled") => "task cancelled".to_string(),
            Some("lost") => format!(
                "task lost: {}",
                result["error"]
                    .as_str()
                    .unwrap_or("the server no longer knows it")
            ),
            _ => result["error"]
                .as_str()
                .unwrap_or("the task failed")
                .to_string(),
        }
    };
    let (text, truncated) = crate::research::cap_tool_text(tool, &text);
    (
        ok,
        json!({"ok": ok, "text": text, "truncated": truncated, "task_id": task_id}),
    )
}

/// Ends a row with `outcome` (C6): the result is stored in the same statement that makes the row terminal. In the
/// same transaction the research step that waits on the task gets the result, and only then is the run, if it is
/// still `waiting`, requeued for the researcher (a cancelled run stays cancelled). The time spent waiting is not
/// counted against the run's wall-clock budget.
async fn finish(
    conn: &mut PgConnection,
    task_id: &str,
    outcome: &TaskOutcome,
    detail: Option<&Value>,
) -> ApiResult<()> {
    let result = outcome.to_json();
    let ended: Option<(Option<Uuid>, String)> = sqlx::query_as(
        "UPDATE mcp_tasks
            SET status = $2, result = $3, last_detail = coalesce($4, last_detail), terminal_at = now(),
                status_message = coalesce($4 ->> 'statusMessage', status_message), updated_at = now()
          WHERE task_id = $1 AND terminal_at IS NULL
         RETURNING run_id, tool",
    )
    .bind(task_id)
    .bind(outcome.status())
    .bind(&result)
    .bind(detail)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((Some(run_id), tool)) = ended else {
        return Ok(());
    };
    let (ok, response) = step_response(&tool, task_id, &result);
    sqlx::query(
        "UPDATE research_steps SET response = $3, status = $4
          WHERE run_id = $1 AND kind = 'tool' AND response ->> 'task_id' = $2 AND response ->> 'waiting' = 'true'",
    )
    .bind(run_id)
    .bind(task_id)
    .bind(&response)
    .bind(if ok { "ok" } else { "error" })
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE research_runs
            SET status = 'queued', started_at = started_at + (now() - updated_at), updated_at = now()
          WHERE id = $1 AND status = 'waiting'",
    )
    .bind(run_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Stores one `tasks/get` answer: terminal → [`finish`]; otherwise the status, its detail and the next poll time.
pub async fn apply_snapshot(
    conn: &mut PgConnection,
    task_id: &str,
    s: &TaskSnapshot,
) -> ApiResult<()> {
    if let Some(outcome) = outcome_of(s) {
        return finish(conn, task_id, &outcome, Some(&s.detail)).await;
    }
    let status = if s.status == "input_required" {
        "input_required"
    } else {
        "working"
    };
    sqlx::query(
        "UPDATE mcp_tasks
            SET status = $2, status_message = $3, last_detail = $4,
                poll_after = now() + make_interval(secs => $5), updated_at = now()
          WHERE task_id = $1 AND terminal_at IS NULL",
    )
    .bind(task_id)
    .bind(status)
    .bind(&s.status_message)
    .bind(&s.detail)
    .bind(poll_delay(s.poll_interval_ms).as_secs_f64())
    .execute(conn)
    .await?;
    Ok(())
}

async fn mark_lost(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
    message: &str,
) -> ApiResult<()> {
    let mut tx = begin_scoped(db, tenant, Some(job)).await?;
    finish(
        &mut tx,
        task_id,
        &TaskOutcome::Lost(ToolError::fatal(message)),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The poller (C3). Each claim leases due rows for `lease_s`; a crashed poller's rows come back after that.
pub struct TaskPoller {
    pub db: PgPool,
    pub mcp: Arc<HttpMcp>,
    pub lease_s: i32,
    pub batch: i32,
}

#[derive(sqlx::FromRow)]
struct Due {
    tenant_id: Uuid,
    job_id: Uuid,
    task_id: String,
}

#[derive(sqlx::FromRow)]
struct Unsent {
    input_key: String,
    answered_by: String,
    response: Value,
}

impl TaskPoller {
    /// Polls until `shutdown`; sleeps `idle` when nothing is due.
    pub async fn work(self: Arc<Self>, mut shutdown: watch::Receiver<bool>, idle: Duration) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            let n = match self.tick().await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "mcp tasks: poll tick failed");
                    0
                }
            };
            if n == 0 {
                tokio::select! {
                    _ = tokio::time::sleep(idle) => {}
                    _ = shutdown.changed() => {}
                }
            }
        }
    }

    /// One claim and the work on every claimed row; returns how many were claimed.
    pub async fn tick(&self) -> ApiResult<usize> {
        let due: Vec<Due> =
            sqlx::query_as("SELECT tenant_id, job_id, task_id FROM carebus_mcp_task_claim($1, $2)")
                .bind(self.lease_s)
                .bind(self.batch)
                .fetch_all(&self.db)
                .await?;
        for d in &due {
            if let Err(e) = self.poll_one(d).await {
                tracing::warn!(task_id = %d.task_id, error = %e, "mcp tasks: poll failed");
            }
        }
        Ok(due.len())
    }

    async fn poll_one(&self, d: &Due) -> ApiResult<()> {
        let (tenant, job, id) = (d.tenant_id, d.job_id, d.task_id.as_str());
        // Read what is owed to the server, then talk to it outside any transaction.
        let mut tx = begin_scoped(&self.db, tenant, Some(job)).await?;
        let (cancel_owed,): (bool,) = sqlx::query_as(
            "SELECT cancel_requested AND NOT cancel_sent FROM mcp_tasks WHERE task_id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        let unsent: Vec<Unsent> = sqlx::query_as(
            "SELECT input_key, answered_by, response FROM mcp_task_answers WHERE task_id = $1 AND sent_at IS NULL",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        if cancel_owed {
            match self.mcp.cancel_task(tenant, id).await {
                Ok(()) => set_cancel_sent(&self.db, tenant, job, id).await?,
                Err(TaskError::Lost(m)) => return mark_lost(&self.db, tenant, job, id, &m).await,
                Err(TaskError::Tool(e)) => {
                    tracing::warn!(task_id = id, error = %e.message, "mcp tasks: cancel not sent; retried")
                }
            }
        }
        for a in unsent {
            let actor = StaffActor {
                tenant,
                staff_user: a.answered_by,
            };
            match self
                .mcp
                .answer_task(&actor, id, &a.input_key, a.response)
                .await
            {
                Ok(()) => set_answer_sent(&self.db, tenant, job, id, &a.input_key).await?,
                Err(TaskError::Lost(m)) => return mark_lost(&self.db, tenant, job, id, &m).await,
                Err(TaskError::Tool(e)) => {
                    tracing::warn!(task_id = id, error = %e.message, "mcp tasks: answer not sent; retried")
                }
            }
        }

        match self.mcp.get_task(tenant, id).await {
            Ok(s) => {
                let mut tx = begin_scoped(&self.db, tenant, Some(job)).await?;
                apply_snapshot(&mut tx, id, &s).await?;
                tx.commit().await?;
                if let Some(o) = outcome_of(&s) {
                    tracing::info!(
                        task_id = id,
                        outcome = o.status(),
                        "mcp tasks: task finished"
                    );
                }
                Ok(())
            }
            Err(TaskError::Lost(m)) => mark_lost(&self.db, tenant, job, id, &m).await,
            Err(TaskError::Tool(e)) => {
                tracing::warn!(task_id = id, error = %e.message, "mcp tasks: tasks/get failed; retried");
                let mut tx = begin_scoped(&self.db, tenant, Some(job)).await?;
                sqlx::query(
                    "UPDATE mcp_tasks SET poll_after = now() + make_interval(secs => $2), updated_at = now()
                      WHERE task_id = $1 AND terminal_at IS NULL",
                )
                .bind(id)
                .bind(RETRY_AFTER.as_secs_f64())
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }
}

async fn set_cancel_sent(db: &PgPool, tenant: Uuid, job: Uuid, id: &str) -> ApiResult<()> {
    let mut tx = begin_scoped(db, tenant, Some(job)).await?;
    sqlx::query("UPDATE mcp_tasks SET cancel_sent = true, poll_after = now(), updated_at = now() WHERE task_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn set_answer_sent(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    id: &str,
    key: &str,
) -> ApiResult<()> {
    let mut tx = begin_scoped(db, tenant, Some(job)).await?;
    sqlx::query(
        "UPDATE mcp_task_answers SET sent_at = now() WHERE task_id = $1 AND input_key = $2",
    )
    .bind(id)
    .bind(key)
    .execute(&mut *tx)
    .await?;
    // The decision moves the task on: poll it soon.
    sqlx::query("UPDATE mcp_tasks SET poll_after = now(), updated_at = now() WHERE task_id = $1 AND terminal_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The questions of a stored detail that nobody here answered yet.
pub fn open_questions(detail: Option<&Value>, answered: &[String]) -> Vec<Value> {
    detail
        .map(input_questions)
        .unwrap_or_default()
        .into_iter()
        .filter(|q| !answered.contains(&q.key))
        .map(
            |q| json!({"key": q.key, "message": q.message, "requested_schema": q.requested_schema}),
        )
        .collect()
}

/// A staff answer as posted: the question's key, `accept` / `decline` / `cancel`, and the content for `accept`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerInput {
    pub key: String,
    pub action: String,
    #[serde(default)]
    pub content: Option<Value>,
}

/// What happened to a staff answer.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct AnswerResult {
    /// The server acknowledged it now; `false` = kept and retried by the poller.
    pub sent: bool,
    /// This key was already answered here: nothing was sent again.
    pub already_answered: bool,
}

/// C4: one human answer. The task must be waiting on `key` (else 409 `not_waiting` / `answered_elsewhere`), the
/// answer must meet the question's schema (else 422 `invalid_answer`). The answer is stored first (a double submit is
/// a no-op), then sent with the deciding human; a failed send is retried by the poller.
pub async fn answer(
    db: &PgPool,
    mcp: &HttpMcp,
    actor: &StaffActor,
    job: Uuid,
    task_id: &str,
    input: &AnswerInput,
) -> ApiResult<AnswerResult> {
    let (key, action, content) = (
        input.key.as_str(),
        input.action.as_str(),
        input.content.as_ref(),
    );
    let mut tx = begin_scoped(db, actor.tenant, Some(job)).await?;
    let row: Option<(String, Option<Value>)> =
        sqlx::query_as("SELECT status, last_detail FROM mcp_tasks WHERE task_id = $1 FOR UPDATE")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((status, detail)) = row else {
        return Err(ApiError::NotFound(format!("task {task_id}")));
    };
    let done: Option<(String,)> = sqlx::query_as(
        "SELECT answered_by FROM mcp_task_answers WHERE task_id = $1 AND input_key = $2",
    )
    .bind(task_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if done.is_some() {
        return Ok(AnswerResult {
            sent: false,
            already_answered: true,
        });
    }
    if status != "input_required" {
        return Err(ApiError::Conflict(
            "not_waiting",
            format!("This task is {status}, not waiting for input."),
        ));
    }
    let questions = detail.as_ref().map(input_questions).unwrap_or_default();
    let Some(q) = questions.iter().find(|q| q.key == key) else {
        return Err(ApiError::Conflict(
            "answered_elsewhere",
            "This question is no longer open: it was answered on another channel.".into(),
        ));
    };
    let response = validate_answer(&q.requested_schema, action, content)
        .map_err(|e| ApiError::Unprocessable("invalid_answer", e))?;
    sqlx::query(
        "INSERT INTO mcp_task_answers (tenant_id, job_id, task_id, input_key, answered_by, response)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(actor.tenant)
    .bind(job)
    .bind(task_id)
    .bind(key)
    .bind(&actor.staff_user)
    .bind(&response)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    match mcp.answer_task(actor, task_id, key, response).await {
        Ok(()) => {
            set_answer_sent(db, actor.tenant, job, task_id, key).await?;
            Ok(AnswerResult {
                sent: true,
                already_answered: false,
            })
        }
        Err(TaskError::Lost(m)) => {
            mark_lost(db, actor.tenant, job, task_id, &m).await?;
            Err(ApiError::Conflict(
                "task_lost",
                "The server no longer knows this task.".into(),
            ))
        }
        Err(TaskError::Tool(e)) => {
            tracing::warn!(task_id, error = %e.message, "mcp tasks: answer kept, send retried by the poller");
            Ok(AnswerResult {
                sent: false,
                already_answered: false,
            })
        }
    }
}

/// C5: asks the server to cancel. The row stays non-terminal until `tasks/get` says otherwise; a failed send is
/// retried by the poller. A finished task is 409 `task_finished`. Returns whether the server acknowledged now.
pub async fn cancel(
    db: &PgPool,
    mcp: &HttpMcp,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
) -> ApiResult<bool> {
    let mut tx = begin_scoped(db, tenant, Some(job)).await?;
    let status: String =
        sqlx::query_scalar("SELECT status FROM mcp_tasks WHERE task_id = $1 FOR UPDATE")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("task {task_id}")))?;
    if matches!(
        status.as_str(),
        "completed" | "cancelled" | "failed" | "lost"
    ) {
        return Err(ApiError::Conflict(
            "task_finished",
            format!("This task is already {status}."),
        ));
    }
    sqlx::query(
        "UPDATE mcp_tasks SET cancel_requested = true, updated_at = now() WHERE task_id = $1",
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    match mcp.cancel_task(tenant, task_id).await {
        Ok(()) => {
            set_cancel_sent(db, tenant, job, task_id).await?;
            Ok(true)
        }
        Err(TaskError::Lost(m)) => {
            mark_lost(db, tenant, job, task_id, &m).await?;
            Ok(false)
        }
        Err(TaskError::Tool(e)) => {
            tracing::warn!(task_id, error = %e.message, "mcp tasks: cancel kept, send retried by the poller");
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_questions_skip_answered_keys() {
        let d = json!({"inputRequests": {
            "t:1": {"method": "elicitation/create", "params": {"message": "A?", "requestedSchema": {}}},
            "t:2": {"method": "elicitation/create", "params": {"message": "B?", "requestedSchema": {}}}}});
        let q = open_questions(Some(&d), &["t:1".to_string()]);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0]["key"], "t:2");
        assert!(open_questions(None, &[]).is_empty());
    }
}
