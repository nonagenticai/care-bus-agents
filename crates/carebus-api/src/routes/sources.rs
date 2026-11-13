//! `/api/jobs/{job}/sources`: one job's customer data sources. Credentials are write-only (see `crate::sources`).
//! A source writing into a store walled for a matter the caller is not a member of (crate::walls) is left out of
//! the list and 404 everywhere else; a create or update naming such a store is 404 (`sources::parse`).
use axum::body::Bytes;
use axum::extract::multipart::{Field, Multipart, MultipartError, MultipartRejection};
use axum::extract::{DefaultBodyLimit, FromRequest, Request, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::netguard::{self, ApiSample, Auth};
use crate::page::{Page, Paged, ts_cursor};
use crate::runs::{self, SourceDeleted, SourceRun, Upload};
use crate::sources::{self, CredentialsChange, SealedChange, SourceType, SourceView};
use crate::state::AppState;
use crate::tabular::{self, ColumnPreview, PreviewError, TableFormat};
use crate::tenancy::{JobAdmin, JobCtx, JobMember};
use crate::walls;

/// The source `id`, or 404 when it writes into a store walled for a matter the caller is not a member of
/// (crate::walls): such a source, its config and its runs do not exist for them, as the store does not.
async fn visible(s: &AppState, ctx: &JobCtx, id: Uuid) -> ApiResult<sources::SourceRecord> {
    let rec = sources::record(s, ctx, id).await?;
    let (hidden, _) = walls::hidden(s, &ctx.t).await?;
    if hidden.contains(&rec.target_store_id.to_string()) {
        return Err(ApiError::NotFound(format!("source {id}")));
    }
    Ok(rec)
}

/// `{id}` under the nested `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
struct IdPath {
    id: Uuid,
}

#[derive(Deserialize)]
struct RunPath {
    id: Uuid,
    run_id: Uuid,
}

pub const MAX_PREVIEW_BYTES: usize = 10 * 1024 * 1024;
/// Longest a column preview may parse before the request gives up with `table_too_complex`.
const PREVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sources", get(list).post(create))
        .route(
            "/sources/preview-columns",
            post(preview).layer(DefaultBodyLimit::max(MAX_PREVIEW_BYTES + 64 * 1024)),
        )
        .route("/sources/{id}", get(one).put(update).delete(remove))
        // static segments win over `{id}` in axum's router, so `test` never reaches the `{id}` routes
        .route("/sources/test", post(test_unsaved))
        .route("/sources/{id}/test", post(test))
        .route(
            "/sources/{id}/resync",
            post(resync).layer(DefaultBodyLimit::max(runs::MAX_UPLOAD_BYTES + 1024 * 1024)),
        )
        .route("/sources/{id}/runs", get(runs_list))
        .route("/sources/{id}/runs/{run_id}", get(run_one))
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    before: Option<String>,
}

async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Paged<SourceView>>> {
    let page = Page::new(q.limit, q.before);
    let limit = page.limit();
    let hidden: Vec<Uuid> = walls::hidden(&s, &ctx.t)
        .await?
        .0
        .iter()
        .filter_map(|h| h.parse().ok())
        .collect();
    let mut items = sources::page(&s, &ctx, page.before.as_deref(), limit, &hidden).await?;
    let running: Vec<Uuid> = items
        .iter()
        .filter_map(|v| {
            v.last_run
                .as_ref()
                .filter(|r| r.status == "running")
                .map(|r| r.id)
        })
        .collect();
    if !running.is_empty() {
        for r in futures_util::future::join_all(running.iter().map(|r| runs::refresh(&s, &ctx, *r)))
            .await
        {
            if let Err(e) = r {
                // our error code only, never upstream text
                tracing::warn!(
                    code = e.status_and_code().1,
                    "run refresh during the source list failed"
                );
            }
        }
        items = sources::page(&s, &ctx, page.before.as_deref(), limit, &hidden).await?;
    }
    Ok(Json(Paged::new(items, limit, |v| v.name.clone())))
}

async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<SourceView>> {
    visible(&s, &ctx, id).await?;
    if let Some(r) = runs::running_of(&s, &ctx, id).await? {
        runs::refresh(&s, &ctx, r).await?;
    }
    Ok(Json(sources::view(&s, &ctx, id).await?))
}

async fn create(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<SourceView>)> {
    let p = sources::parse(&s, &ctx, raw).await?;
    let id = Uuid::new_v4();
    let sealed = match &p.credentials {
        CredentialsChange::Set(cr) => Some(sources::seal(&s, id, cr)?),
        CredentialsChange::Keep | CredentialsChange::Clear => None,
    };
    sources::insert(&s, &ctx, id, &p.input, sealed.as_ref(), &ctx.t.caller.actor).await?;
    sources::audit_source(
        &s,
        &ctx,
        "create",
        &id.to_string(),
        true,
        json!({"type": p.input.kind.as_str(), "has_credentials": sealed.is_some()}),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(sources::view(&s, &ctx, id).await?),
    ))
}

async fn update(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<SourceView>> {
    let existing = visible(&s, &ctx, id).await?; // 404 before body validation
    let p = sources::parse(&s, &ctx, raw).await?;
    let has_after = match &p.credentials {
        CredentialsChange::Keep => existing.has_credentials(),
        CredentialsChange::Clear => false,
        CredentialsChange::Set(_) => true,
    };
    sources::check_credentials_allowed(p.input.kind, &p.input.config, has_after)?;
    let (change, word) = match &p.credentials {
        CredentialsChange::Keep => (SealedChange::Keep, "unchanged"),
        CredentialsChange::Clear => (SealedChange::Clear, "cleared"),
        CredentialsChange::Set(cr) => (SealedChange::Set(sources::seal(&s, id, cr)?), "replaced"),
    };
    sources::update(&s, &ctx, id, &p.input, &change, &ctx.t.caller.actor).await?;
    sources::audit_source(
        &s,
        &ctx,
        "update",
        &id.to_string(),
        true,
        json!({"type": p.input.kind.as_str(), "credentials": word}),
    )
    .await?;
    Ok(Json(sources::view(&s, &ctx, id).await?))
}

/// A test body as a JSON object; an empty body is `{}` (the no-override test of a saved source).
fn test_body(body: &[u8]) -> ApiResult<serde_json::Map<String, Value>> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(o)) => Ok(o),
        _ => Err(sources::bad(
            "invalid_body",
            "The request body must be a JSON object.",
        )),
    }
}

fn only_keys(o: &serde_json::Map<String, Value>, allowed: &[&str]) -> ApiResult<()> {
    match o.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(sources::bad(
            "invalid_body",
            format!(
                "A test takes only {}; {} is not one of them.",
                allowed.join(", "),
                k.chars().take(40).collect::<String>()
            ),
        )),
        None => Ok(()),
    }
}

/// True when `a` and `b` are the same scheme, host and port: a stored secret may follow a path or query edit,
/// never a different host.
fn same_origin(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

/// ONE guarded GET to `config.url` with `secret` presented per `config.auth`, audited without the secret or URL.
/// The audit transaction opens after the fetch.
async fn run_test(
    s: &AppState,
    ctx: &JobCtx,
    entity: &str,
    config: &Value,
    secret: Option<&str>,
    mut details: Value,
) -> ApiResult<Json<ApiSample>> {
    let url = config["url"].as_str().unwrap_or_default();
    let header = config["auth"]["header_name"].as_str().unwrap_or_default();
    let auth = match (sources::auth_type(config), secret) {
        ("bearer", Some(t)) => Auth::Bearer(t),
        ("header", Some(t)) => Auth::Header(header, t),
        _ => Auth::None,
    };
    let res = netguard::fetch_sample(url, auth, s.cfg.source_fetch_allow_loopback).await;
    match &res {
        Ok(x) => {
            details["ok"] = json!(x.ok);
            details["status"] = json!(x.status);
        }
        Err(r) => details["refused"] = json!(r.0),
    }
    sources::audit_source(s, ctx, "test", entity, res.is_ok(), details).await?;
    res.map(Json)
        .map_err(|r| ApiError::BadRequest("address_refused", r.0.to_string()))
}

fn test_not_supported() -> ApiError {
    sources::bad(
        "test_not_supported",
        "Test connection is for api sources. Use the column preview for a datatable.",
    )
}

/// api sources only. With no body (or `{}`) the stored `config.url`/`config.auth` and the decrypted credential are
/// used. A body `{config?: {url, auth}, credentials?}` tests edits before they are saved: `config` replaces the
/// stored connection settings, `credentials` `{secret}` replaces the stored secret for this request only, `null`
/// sends none, and when absent the stored secret is used, but only against the stored URL's origin.
async fn test(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    body: Bytes,
) -> ApiResult<Json<ApiSample>> {
    let rec = visible(&s, &ctx, id).await?;
    if rec.kind != SourceType::Api {
        return Err(test_not_supported());
    }
    let mut o = test_body(&body)?;
    only_keys(&o, &["config", "credentials"])?;
    let edited = o.get("config").cloned();
    let change = sources::credentials_change(o.remove("credentials"))?;
    let overridden = edited.is_some() || !matches!(change, CredentialsChange::Keep);
    if let Some(cfg) = &edited {
        sources::validate_test_config(cfg)?;
    }
    let config = edited.as_ref().unwrap_or(&rec.config);
    // Rows that bypassed save must not send an arbitrary header name.
    sources::validate_auth(config.get("auth"))?;
    let creds = match change {
        CredentialsChange::Set(cr) => {
            sources::check_credentials_allowed(SourceType::Api, config, true)?;
            Some(cr)
        }
        CredentialsChange::Clear => None,
        CredentialsChange::Keep => {
            let stored = rec.credentials(s.cfg.source_key.as_ref())?;
            let moved = !same_origin(
                config["url"].as_str().unwrap_or_default(),
                rec.config["url"].as_str().unwrap_or_default(),
            );
            if stored.is_some() && moved && sources::auth_type(config) != "none" {
                return Err(sources::bad(
                    "credentials_required",
                    "The stored secret is only sent to the saved host. Enter the secret to test a different host.",
                ));
            }
            stored
        }
    };
    run_test(
        &s,
        &ctx,
        &id.to_string(),
        config,
        creds.as_ref().map(|c| c.secret.as_str()),
        json!({"override": overridden}),
    )
    .await
}

/// An unsaved api source: `{type: "api", config: {url, auth}, credentials?: {secret}}`. Nothing is stored; the
/// secret is used for this one request and is not logged, audited or returned.
async fn test_unsaved(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    body: Bytes,
) -> ApiResult<Json<ApiSample>> {
    let mut o = test_body(&body)?;
    only_keys(&o, &["type", "config", "credentials"])?;
    match o.get("type").and_then(Value::as_str) {
        Some("api") => {}
        Some(t) if SourceType::parse(t).is_some() => return Err(test_not_supported()),
        _ => {
            return Err(sources::bad(
                "invalid_type",
                "type must be api (website, file and datatable sources are not tested this way).",
            ));
        }
    }
    let change = sources::credentials_change(o.remove("credentials"))?;
    let config = o.remove("config").unwrap_or(Value::Null);
    sources::validate_test_config(&config)?;
    let creds = match change {
        CredentialsChange::Set(cr) => {
            sources::check_credentials_allowed(SourceType::Api, &config, true)?;
            Some(cr)
        }
        CredentialsChange::Keep | CredentialsChange::Clear => None,
    };
    run_test(
        &s,
        &ctx,
        "-",
        &config,
        creds.as_ref().map(|c| c.secret.as_str()),
        json!({"unsaved": true, "has_credentials": creds.is_some()}),
    )
    .await
}

pub(crate) fn multipart_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::PayloadTooLarge("The upload is larger than this route accepts.")
    } else {
        sources::bad(
            "invalid_body",
            format!("The upload is not valid multipart: {}", e.body_text()),
        )
    }
}

/// A field's bytes, refusing more than `cap` (413 with `too_large`).
pub(crate) async fn read_capped(
    field: &mut Field<'_>,
    cap: usize,
    too_large: &'static str,
) -> ApiResult<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        if out.len() + chunk.len() > cap {
            return Err(ApiError::PayloadTooLarge(too_large));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The `file` parts of a file-source re-sync (at most 20 files, 50 MiB in all). An empty upload is refused
/// by `runs::start` (`no_file`), the single place that check lives.
async fn read_uploads(req: Request) -> ApiResult<Vec<Upload>> {
    let mut mp = Multipart::from_request(req, &()).await.map_err(|e| {
        sources::bad(
            "invalid_body",
            format!(
                "A file source re-sync takes a multipart upload with file fields: {}",
                e.body_text()
            ),
        )
    })?;
    let mut out: Vec<Upload> = Vec::new();
    let mut total = 0usize;
    while let Some(mut field) = mp.next_field().await.map_err(multipart_error)? {
        if field.name() != Some("file") {
            return Err(sources::bad(
                "invalid_body",
                "Only file fields are accepted.",
            ));
        }
        let Some(file_name) = field
            .file_name()
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string)
        else {
            return Err(sources::bad(
                "invalid_body",
                "Every file field needs a file name.",
            ));
        };
        if out.len() == runs::MAX_UPLOAD_FILES {
            return Err(sources::bad(
                "too_many_files",
                "A re-sync takes at most 20 files.",
            ));
        }
        let content_type = field.content_type().map(str::to_string);
        let bytes = read_capped(
            &mut field,
            runs::MAX_UPLOAD_BYTES - total,
            "A re-sync takes up to 50 MB of files.",
        )
        .await?;
        total += bytes.len();
        out.push(Upload {
            file_name,
            content_type,
            bytes,
        });
    }
    Ok(out)
}

async fn resync(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    req: Request,
) -> ApiResult<(StatusCode, Json<SourceRun>)> {
    let src = visible(&s, &ctx, id).await?;
    let files = if src.kind == SourceType::File {
        read_uploads(req).await?
    } else {
        Vec::new()
    };
    let res = runs::start(&s, &ctx, &src, &ctx.t.caller.actor, files).await;
    // Audit details hold our own status/code only, never upstream text.
    let (ok, details) = match &res {
        Ok((_, r)) => (
            matches!(r.status.as_str(), "running" | "completed"),
            json!({"run_id": r.id, "status": r.status, "jobs": r.ingest_job_ids.len()}),
        ),
        Err(e) => {
            let (st, code) = e.status_and_code();
            (false, json!({"status": st.as_u16(), "code": code}))
        }
    };
    let reached_ingest = !matches!(&res, Err(ApiError::BadRequest(..) | ApiError::Conflict(..)));
    if res.is_ok() || reached_ingest {
        sources::audit_source(&s, &ctx, "resync", &id.to_string(), ok, details).await?;
    }
    res.map(|(st, r)| (st, Json(r)))
}

#[derive(Deserialize)]
struct RunsQuery {
    limit: Option<i64>,
    before: Option<String>,
}

async fn runs_list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiQuery(q): ApiQuery<RunsQuery>,
) -> ApiResult<Json<Paged<SourceRun>>> {
    visible(&s, &ctx, id).await?; // 404 for an unknown (or walled) source, never an empty page
    let page = Page::new(q.limit, q.before);
    let limit = page.limit();
    let before = page.before_ts()?;
    let mut live = None;
    if let Some(r) = runs::running_of(&s, &ctx, id).await? {
        live = Some(runs::refresh(&s, &ctx, r).await?);
    }
    let mut items = runs::page(&s, &ctx, id, before, limit).await?;
    if let Some(l) = live {
        // The refreshed run's live-only fields: staleness, and a crawl's or sitemap's page-job summary.
        for r in items.iter_mut().filter(|r| r.id == l.id) {
            r.stale = l.stale && r.status == "running";
            r.children = l.children.clone();
        }
    }
    Ok(Json(Paged::new(items, limit, |r| ts_cursor(r.started_at))))
}

async fn run_one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(RunPath { id, run_id }): ApiPath<RunPath>,
) -> ApiResult<Json<SourceRun>> {
    visible(&s, &ctx, id).await?;
    // Look the run up first: refreshing a run that belongs to another source would act on it before the 404.
    if runs::load(&s, &ctx, run_id).await?.source_id != id {
        return Err(ApiError::NotFound(format!("run {run_id}")));
    }
    Ok(Json(runs::refresh(&s, &ctx, run_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    purge: Option<bool>,
}

/// The audit row lists at most this many kept ids; `documents_kept` carries the full count.
const KEPT_IDS_IN_AUDIT: usize = 500;

/// Config and run history only, unless `purge=true` (see `runs::delete_source`).
async fn remove(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiQuery(q): ApiQuery<DeleteQuery>,
) -> ApiResult<Json<SourceDeleted>> {
    let purge = q.purge.unwrap_or(false);
    visible(&s, &ctx, id).await?;
    let (deleted, kept_ids) = match runs::delete_source(&s, &ctx, id, purge).await {
        Ok(x) => x,
        Err(e) => {
            // A purge that reached strata_rag and failed leaves a trace; the source is kept.
            let (st, code) = e.status_and_code();
            if purge && matches!(code, "upstream_unavailable" | "purge_incomplete") {
                sources::audit_source(
                    &s,
                    &ctx,
                    "delete",
                    &id.to_string(),
                    false,
                    json!({"purge": true, "status": st.as_u16(), "code": code}),
                )
                .await?;
            }
            return Err(e);
        }
    };
    let kept: Vec<&String> = kept_ids.iter().take(KEPT_IDS_IN_AUDIT).collect();
    let truncated = kept_ids.len() > KEPT_IDS_IN_AUDIT;
    sources::audit_source(
        &s,
        &ctx,
        "delete",
        &id.to_string(),
        true,
        json!({"purge": purge, "documents_deleted": deleted.documents_deleted,
               "documents_kept": deleted.documents_kept, "kept_document_ids": kept,
               "kept_document_ids_truncated": truncated}),
    )
    .await?;
    Ok(Json(deleted))
}

/// Runs `parse` on the blocking pool. Past `limit` the request answers 400 `table_too_complex` (the parse
/// itself is bounded by the tabular caps and finishes on its own).
async fn off_thread<T: Send + 'static>(
    limit: std::time::Duration,
    parse: impl FnOnce() -> T + Send + 'static,
) -> ApiResult<T> {
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(parse)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(ApiError::Internal(anyhow::anyhow!("preview task: {e}"))),
        Err(_) => Err(sources::bad(
            "table_too_complex",
            format!(
                "The table took longer than {} s to read; send a smaller or simpler file.",
                limit.as_secs()
            ),
        )),
    }
}

/// Columns, five sample rows and the row count of an uploaded CSV or XLSX (optional `sheet` field; the first
/// sheet by default; optional `header_row` field, `false` when the first row is data), for the mapping editor. Nothing is stored.
async fn preview(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    mp: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<ColumnPreview>> {
    let mut mp = mp.map_err(|e| {
        sources::bad(
            "invalid_body",
            format!(
                "Send a multipart upload with a file field: {}",
                e.body_text()
            ),
        )
    })?;
    let mut file: Option<(String, bool, Vec<u8>)> = None;
    let mut sheet: Option<String> = None;
    let mut header_row: Option<bool> = None;
    while let Some(mut field) = mp.next_field().await.map_err(multipart_error)? {
        match field.name().unwrap_or_default() {
            "file" if file.is_none() => {
                let name = field.file_name().unwrap_or_default().to_string();
                let xlsx_type = field
                    .content_type()
                    .is_some_and(|t| t.contains("spreadsheetml"));
                let bytes = read_capped(
                    &mut field,
                    MAX_PREVIEW_BYTES,
                    "Previews take files up to 10 MB.",
                )
                .await?;
                file = Some((name, xlsx_type, bytes));
            }
            "sheet" if sheet.is_none() => {
                // used for XLSX; a CSV has no sheets and ignores it
                let text = field.text().await.map_err(multipart_error)?;
                sheet = Some(text.trim().to_string()).filter(|t| !t.is_empty());
            }
            "header_row" if header_row.is_none() => {
                // false: the first row is data, not column names (default true)
                let text = field.text().await.map_err(multipart_error)?;
                header_row = Some(match text.trim() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(sources::bad(
                            "invalid_body",
                            "The header_row field must be true or false.",
                        ));
                    }
                });
            }
            _ => {
                return Err(sources::bad(
                    "invalid_body",
                    "The upload takes one file field and optional sheet and header_row fields.",
                ));
            }
        }
    }
    let header_row = header_row.unwrap_or(true);
    let (name, xlsx_type, bytes) = file
        .ok_or_else(|| sources::bad("no_file", "Attach a CSV or XLSX file in the file field."))?;
    let format = if xlsx_type {
        Some(TableFormat::Xlsx)
    } else {
        TableFormat::from_file_name(&name)
    };
    // CPU-bound parsing (up to 10 MB of CSV, 50 MiB of XML): off the async workers, and bounded in time.
    let result = match format {
        Some(TableFormat::Csv) => {
            off_thread(PREVIEW_TIMEOUT, move || {
                tabular::preview_csv(&bytes, header_row)
            })
            .await?
        }
        Some(TableFormat::Xlsx) => {
            off_thread(PREVIEW_TIMEOUT, move || {
                tabular::preview_xlsx(&bytes, sheet.as_deref(), header_row)
            })
            .await?
        }
        None => {
            return Err(ApiError::Unsupported(
                "Only .csv and .xlsx files can be previewed.".into(),
            ));
        }
    };
    let preview = result.map_err(|e| match e {
        PreviewError::SheetNotFound(_) => sources::bad("sheet_not_found", format!("{e}.")),
        PreviewError::TooLarge(_) => sources::bad("table_too_large", format!("{e}.")),
        PreviewError::TooManyColumns => sources::bad("too_many_columns", format!("{e}.")),
        PreviewError::Empty | PreviewError::Unreadable(_) => {
            sources::bad("unreadable_table", format!("{e}."))
        }
    })?;
    sources::audit_source(
        &s,
        &ctx,
        "preview",
        "-",
        true,
        json!({"format": preview.format, "columns": preview.columns.len(), "row_count": preview.row_count}),
    )
    .await?;
    Ok(Json(preview))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_parse_that_outruns_its_limit_is_table_too_complex() {
        let slow = off_thread(std::time::Duration::from_millis(50), || {
            std::thread::sleep(std::time::Duration::from_millis(400));
        })
        .await;
        assert!(
            matches!(slow, Err(ApiError::BadRequest("table_too_complex", _))),
            "{slow:?}"
        );
        let fast = off_thread(std::time::Duration::from_secs(5), || 7).await;
        assert!(matches!(fast, Ok(7)));
    }
}
