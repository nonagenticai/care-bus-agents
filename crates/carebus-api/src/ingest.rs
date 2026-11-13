//! strata_ingest client and the job view carebus-api passes on. Credentials never leave through it: strata_ingest
//! may echo a job's `source` (which, for the extended contract, carried them), so every job is redacted on parse.
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ApiError, ApiResult};
use crate::secrets;
use crate::state::AppState;
use crate::tenancy::{JobCtx, TenantCtx};
use crate::upstream::{self, Payload, Reply, segment};
use uuid::Uuid;

pub const NAME: &str = "strata_ingest";
/// strata_ingest's job states (PR #11): `cancelling` is a running job asked to stop, waiting for its step in flight;
/// `deleting` an ended job whose `DELETE` has not finished (a 502 leaves it there; `DELETE` again resumes).
pub const JOB_STATUSES: [&str; 8] = [
    "queued",
    "running",
    "cancelling",
    "deleting",
    "completed",
    "failed",
    "rejected",
    "cancelled",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestStep {
    pub kind: String,
    pub key: String,
    pub status: String,
    pub attempt: i32,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub usage: Option<Value>,
    pub cost_micros: Option<i64>,
    pub error: Option<Value>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestJob {
    pub id: String,
    pub store_id: String,
    pub source: Value,
    pub options: Value,
    pub meta: Value,
    pub status: String,
    pub error: Option<Value>,
    pub document_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub steps: Vec<IngestStep>,
    /// A crawl or sitemap job is a parent: strata_ingest's `{total, by_status}` of the page jobs it started.
    /// Passed through as given; absent for an ordinary job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Value>,
    /// The page jobs of a parent job; absent (empty) for an ordinary job.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_job_ids: Vec<String>,
}

impl IngestJob {
    /// Its work has ended. `deleting` counts: only an ended job can be deleted, and it never runs again.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "failed" | "rejected" | "cancelled" | "deleting"
        )
    }

    /// A parent job whose summary still counts queued, running or cancelling children.
    pub fn children_pending(&self) -> bool {
        self.children.as_ref().is_some_and(|c| {
            ["queued", "running", "cancelling"]
                .iter()
                .any(|st| c["by_status"][*st].as_u64().unwrap_or(0) > 0)
        })
    }

    /// `error.code: error.message` (strata_ingest's `{class, code, message, provider}`), the message alone when
    /// there is no code, else the error as JSON text.
    pub fn error_text(&self) -> Option<String> {
        self.error
            .as_ref()
            .map(|e| match (e["code"].as_str(), e["message"].as_str()) {
                (Some(code), Some(msg)) => format!("{code}: {msg}"),
                (None, Some(msg)) => msg.to_string(),
                _ => e.to_string(),
            })
    }

    /// The structured-source kinds (`api` | `datatable`): strata_ingest keeps no credentials for them, so a failed
    /// job of one cannot be retried there (409); it is re-submitted instead.
    pub fn is_structured_source(&self) -> bool {
        matches!(self.source["kind"].as_str(), Some("api" | "datatable"))
    }

    /// The care-bus source this job was submitted for (`meta.source_id`), when it is one of ours.
    pub fn source_id(&self) -> Option<Uuid> {
        self.meta["source_id"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
    }

    pub fn redacted(mut self) -> Self {
        redact(&mut self.source);
        redact(&mut self.options);
        redact(&mut self.meta);
        if let Some(e) = &mut self.error {
            redact(e);
        }
        for st in &mut self.steps {
            if let Some(e) = &mut st.error {
                redact(e);
            }
            if let Some(u) = &mut st.usage {
                redact(u);
            }
        }
        self
    }
}

/// Replaces every form of the secret (raw, base64, percent-encoded: [`secrets::secret_forms`]) in every string
/// at any depth.
fn scrub_strings(v: &mut Value, forms: &[String]) {
    match v {
        Value::String(t) => {
            for f in forms {
                if t.contains(f.as_str()) {
                    *t = t.replace(f.as_str(), "[redacted]");
                }
            }
        }
        Value::Object(m) => m.values_mut().for_each(|x| scrub_strings(x, forms)),
        Value::Array(a) => a.iter_mut().for_each(|x| scrub_strings(x, forms)),
        _ => {}
    }
}

/// Every job's own source secret, wherever an upstream echoed it (an error message that quotes the source's
/// reply, say), is replaced. Jobs of the same source share one lookup.
/// Sources are looked up in `t`'s tenant (any job); a job naming a source the tenant does not have is left as
/// redacted on parse.
pub async fn scrub_jobs(s: &AppState, t: &TenantCtx, jobs: &mut [IngestJob]) {
    let mut secrets: std::collections::HashMap<Uuid, Option<String>> = Default::default();
    for j in jobs.iter_mut() {
        let Some(src) = j.source_id() else { continue };
        if let std::collections::hash_map::Entry::Vacant(v) = secrets.entry(src) {
            let secret = match source_job(s, t, src).await {
                Ok(Some(job)) => crate::runs::secret_of(s, &job, src).await,
                _ => None,
            };
            v.insert(secret);
        }
        let Some(Some(sec)) = secrets.get(&src) else {
            continue;
        };
        let sec = secrets::secret_forms(sec);
        if sec.is_empty() {
            continue;
        }
        for v in [&mut j.source, &mut j.options, &mut j.meta] {
            scrub_strings(v, &sec);
        }
        if let Some(e) = &mut j.error {
            scrub_strings(e, &sec);
        }
        for st in &mut j.steps {
            for v in [&mut st.error, &mut st.usage].into_iter().flatten() {
                scrub_strings(v, &sec);
            }
        }
    }
}

/// The job of the tenant's care-bus source `source` (`None` when the tenant has no such source), as the `JobCtx`
/// the source and run functions act in. Read in the tenant with the job GUC empty, so every job's sources count.
pub async fn source_job(s: &AppState, t: &TenantCtx, source: Uuid) -> ApiResult<Option<JobCtx>> {
    let mut tx = t.begin(&s.db).await?;
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT j.id, j.key FROM sources s JOIN jobs j ON j.id = s.job_id WHERE s.id = $1",
    )
    .bind(source)
    .fetch_optional(&mut *tx)
    .await?;
    drop(tx);
    Ok(row.map(|(job_id, job_key)| JobCtx {
        t: t.clone(),
        job_id,
        job_key,
    }))
}

/// Drops every `credentials` key (any case) and blanks every secret-named value, at any depth.
pub fn redact(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, _| !k.eq_ignore_ascii_case("credentials"));
            for (k, x) in m.iter_mut() {
                if secrets::is_secret_key(k) && !x.is_null() {
                    *x = Value::String("[redacted]".into());
                } else {
                    redact(x);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact),
        _ => {}
    }
}

fn parse_job(body: Value) -> ApiResult<IngestJob> {
    serde_json::from_value::<IngestJob>(body)
        .map(IngestJob::redacted)
        .map_err(|_| ApiError::Upstream(NAME))
}

/// One strata_ingest request acting for `tenant`. Unconfigured is 503; a tenant the static token is not pinned
/// to is 409 `strata_not_provisioned_for_tenant`, and nothing is sent.
async fn call(
    s: &AppState,
    tenant: Uuid,
    method: Method,
    path: &str,
    payload: Payload,
) -> ApiResult<Reply> {
    let base = s
        .cfg
        .strata_ingest_url
        .as_deref()
        .ok_or(ApiError::Upstream(NAME))?;
    let cred = s
        .strata_tokens
        .cred(&s.cfg, crate::strata_tokens::StrataApp::Ingest, tenant)
        .await?;
    upstream::call_with(
        &s.http,
        NAME,
        Some(base),
        cred.auth(),
        method,
        path,
        payload,
    )
    .await
}

/// 409 `strata_not_provisioned_for_tenant` when strata_ingest is configured and its static token is not pinned to
/// `tenant` (minted tokens: the mint's refusal); writes check it first, so the refusal is neither sent nor audited.
/// Unconfigured passes (the call is then the 503).
pub async fn provisioned_for(s: &AppState, tenant: Uuid) -> ApiResult<()> {
    if s.cfg.strata_ingest_url.is_some() {
        s.strata_tokens
            .cred(&s.cfg, crate::strata_tokens::StrataApp::Ingest, tenant)
            .await?;
    }
    Ok(())
}

fn refusal(status: StatusCode, body: &Value, what: &str) -> ApiError {
    upstream::refusal(NAME, status, body, what, "ingest_conflict")
}

/// `None` when strata_ingest does not know the job (another tenant's job is also `None`).
pub async fn get_job(s: &AppState, tenant: Uuid, id: &str) -> ApiResult<Option<IngestJob>> {
    let id = segment(id, false)?;
    let Reply { status, body } = call(
        s,
        tenant,
        Method::GET,
        &format!("/api/ingest/jobs/{id}"),
        Payload::Empty,
    )
    .await?;
    if status.is_success() {
        return parse_job(body).map(Some);
    }
    match refusal(status, &body, &format!("ingest job {id}")) {
        ApiError::NotFound(_) => Ok(None),
        e => Err(e),
    }
}

/// `path_and_query` is built by the route from validated parts. Returns the page and strata_ingest's `next`
/// cursor (a job id to pass as `before`), checked to be a job id.
pub async fn list_jobs(
    s: &AppState,
    tenant: Uuid,
    path_and_query: &str,
) -> ApiResult<(Vec<IngestJob>, Option<String>)> {
    let Reply { status, mut body } =
        call(s, tenant, Method::GET, path_and_query, Payload::Empty).await?;
    if !status.is_success() {
        return Err(refusal(status, &body, "ingest jobs"));
    }
    let next = match body["next"].take() {
        Value::Null => None,
        Value::String(n) if Uuid::parse_str(&n).is_ok() => Some(n),
        _ => return Err(ApiError::Upstream(NAME)),
    };
    match body["jobs"].take() {
        Value::Array(a) => Ok((
            a.into_iter().map(parse_job).collect::<ApiResult<_>>()?,
            next,
        )),
        _ => Err(ApiError::Upstream(NAME)),
    }
}

/// `POST …/{id}/retry` or `POST …/{id}/cancel`: the job view after the action.
pub async fn job_action(
    s: &AppState,
    tenant: Uuid,
    method: Method,
    id: &str,
    suffix: &str,
) -> ApiResult<IngestJob> {
    let id = segment(id, false)?;
    let Reply { status, body } = call(
        s,
        tenant,
        method,
        &format!("/api/ingest/jobs/{id}{suffix}"),
        Payload::Empty,
    )
    .await?;
    if status.is_success() {
        return parse_job(body);
    }
    Err(refusal(status, &body, &format!("ingest job {id}")))
}

/// strata_ingest's answer to `DELETE /api/ingest/jobs/{id}` (PR #11): the ended job, its staged blobs and (for a
/// crawl / sitemap parent) its child jobs are gone; the documents it delivered stay in strata_rag (`document_ids`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedJob {
    pub job_id: String,
    /// The status the job had.
    pub status: String,
    pub deleted_jobs: u64,
    pub deleted_objects: u64,
    pub document_ids: Vec<String>,
    /// Always `retained`: delete them in strata_rag if wanted.
    pub documents: String,
}

/// What a `DELETE` of a job came to.
#[derive(Debug, Clone)]
pub enum JobDelete {
    /// 200: the job is gone.
    Done(DeletedJob),
    /// 202 `{job_id, status: "deleting"}`: strata_ingest finishes the delete in the background (its sweeper); the
    /// job reads `deleting` until it is gone (404).
    Continuing,
}

/// `DELETE …/{id}`: only an ended job; 409 `ingest_conflict` while it (or a child) is queued, running or
/// cancelling; 409 `child_of_parent_job` (with `parent_job_id`) for a crawl / sitemap page job, which goes only with
/// its parent. A blob strata_ingest cannot delete is its 502, our 503: some blobs may already be gone, the job stays
/// `deleting` (it can no longer run or be retried) and deleting it again finishes it.
pub async fn delete_job(s: &AppState, tenant: Uuid, id: &str) -> ApiResult<JobDelete> {
    let id = segment(id, false)?;
    let Reply { status, body } = call(
        s,
        tenant,
        Method::DELETE,
        &format!("/api/ingest/jobs/{id}"),
        Payload::Empty,
    )
    .await?;
    if status == StatusCode::ACCEPTED {
        if body["job_id"].as_str() != Some(id) || body["status"].as_str() != Some("deleting") {
            return Err(ApiError::Upstream(NAME));
        }
        return Ok(JobDelete::Continuing);
    }
    if status.is_success() {
        let d: DeletedJob = serde_json::from_value(body).map_err(|_| ApiError::Upstream(NAME))?;
        if d.job_id != id || !JOB_STATUSES.contains(&d.status.as_str()) {
            return Err(ApiError::Upstream(NAME));
        }
        return Ok(JobDelete::Done(d));
    }
    if let Some(e) = child_of_parent(status, &body) {
        return Err(e);
    }
    Err(refusal(status, &body, &format!("ingest job {id}")))
}

/// strata_ingest's 409 `child_of_parent_job` kept as itself (not collapsed to `ingest_conflict`), with the parent's
/// id checked to be a uuid: the UI offers to delete that parent instead.
fn child_of_parent(status: StatusCode, body: &Value) -> Option<ApiError> {
    if status != StatusCode::CONFLICT
        || body["error"]["code"].as_str() != Some("child_of_parent_job")
    {
        return None;
    }
    let parent = body["error"]["parent_job_id"]
        .as_str()
        .and_then(|p| Uuid::parse_str(p).ok())?;
    Some(ApiError::ConflictWith(
        "child_of_parent_job",
        format!(
            "This is a page job of crawl/sitemap job {parent}: it is deleted only with that job. Delete the parent \
             job, which removes its page jobs with it."
        ),
        serde_json::json!({"parent_job_id": parent}),
    ))
}

/// One job strata_ingest created: its id and its first status (`queued`; `failed` when a structured source could
/// not be fetched or parsed, `rejected` for an infected upload: those are final already).
pub struct CreatedJob {
    pub id: String,
    pub status: String,
}

impl CreatedJob {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "failed" | "rejected" | "cancelled"
        )
    }
}

pub enum Created {
    Jobs(Vec<CreatedJob>),
    /// strata_ingest answered 4xx (a malformed request, or 422 `unsupported_source_kind`). `message` is its full
    /// text; the caller scrubs and shortens it.
    Refused {
        status: StatusCode,
        code: Option<String>,
        message: String,
    },
}

pub async fn create(s: &AppState, tenant: Uuid, payload: Payload) -> ApiResult<Created> {
    let Reply { status, body } = call(s, tenant, Method::POST, "/api/ingest/jobs", payload).await?;
    if status.is_success() {
        // A crawl or sitemap request answers with its one parent job, `{job_id}`; every other kind with `{jobs}`.
        let single;
        let listed: &[Value] = match body["jobs"].as_array() {
            Some(a) => a,
            None if body["job_id"].is_string() => {
                single = [body.clone()];
                &single
            }
            None => return Err(ApiError::Upstream(NAME)),
        };
        let ids = listed
            .iter()
            .map(|j| {
                let id = j["job_id"]
                    .as_str()
                    .filter(|i| segment(i, false).is_ok())
                    .ok_or(ApiError::Upstream(NAME))?;
                let status = j["status"].as_str().unwrap_or("queued");
                if !JOB_STATUSES.contains(&status) {
                    return Err(ApiError::Upstream(NAME));
                }
                Ok(CreatedJob {
                    id: id.to_string(),
                    status: status.to_string(),
                })
            })
            .collect::<ApiResult<Vec<_>>>()?;
        if ids.is_empty() {
            return Err(ApiError::Upstream(NAME));
        }
        return Ok(Created::Jobs(ids));
    }
    let missing_route = matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) && body["error"]["code"].as_str().is_none();
    if status.is_client_error() && !missing_route {
        return Ok(Created::Refused {
            status,
            code: body["error"]["code"].as_str().map(str::to_string),
            message: upstream::message(NAME, status, &body),
        });
    }
    Err(ApiError::Upstream(NAME))
}
