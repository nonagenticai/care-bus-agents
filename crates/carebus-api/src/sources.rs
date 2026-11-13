//! Source configuration: types, validation, the write-only credentials and the row loaders.
//!
//! Credentials arrive only under the top-level `credentials` key of a POST/PUT body. `parse` splits that key off
//! BEFORE the secret rejector walks the rest of the body, which makes it the one and only exemption: a
//! `credentials` object nested in `config` is still rejected.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::types::Json;
use uuid::Uuid;

use crate::audit;
use crate::crypto::{self, Sealed, SourceKey};
use crate::error::{ApiError, ApiResult};
use crate::netguard;
use crate::secrets;
use crate::state::AppState;
use crate::strata;
use crate::tenancy::JobCtx;

pub fn bad(code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest(code, msg.into())
}

/// One `source` audit row as the request's caller, in its own transaction scoped to the job.
pub async fn audit_source(
    s: &AppState,
    ctx: &JobCtx,
    action: &str,
    resource_id: &str,
    ok: bool,
    details: Value,
) -> ApiResult<()> {
    let mut tx = ctx.begin(&s.db).await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        action,
        "source",
        resource_id,
        ok,
        details,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    Website,
    Api,
    File,
    Datatable,
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Website => "website",
            Self::Api => "api",
            Self::File => "file",
            Self::Datatable => "datatable",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "website" => Some(Self::Website),
            "api" => Some(Self::Api),
            "file" => Some(Self::File),
            "datatable" => Some(Self::Datatable),
            _ => None,
        }
    }
    /// api and datatable go through strata_ingest's extended contract: only they carry a mapping and credentials.
    pub fn structured(self) -> bool {
        matches!(self, Self::Api | Self::Datatable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRow {
    pub source_field: String,
    pub entity: String,
    pub attribute: String,
}

/// The secret of an api or datatable source. Deserialised from requests and serialised only to be sealed or sent
/// to strata_ingest; no response type contains it, and `Debug` is redacted.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(..)")
    }
}

/// `credentials` absent = Keep, `null` = Clear, `{"secret"}` = Set.
pub enum CredentialsChange {
    Keep,
    Clear,
    Set(Credentials),
}

pub enum SealedChange {
    Keep,
    Clear,
    Set(Sealed),
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}
fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: SourceType,
    #[serde(default = "empty_object")]
    pub config: Value,
    #[serde(default)]
    pub mapping: Vec<MappingRow>,
    pub target_store_id: Uuid,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

pub struct Parsed {
    pub input: SourceInput,
    pub credentials: CredentialsChange,
}

#[derive(Debug, Serialize)]
pub struct RunSummary {
    pub id: Uuid,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub document_count: i32,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SourceView {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: SourceType,
    pub config: Value,
    pub mapping: Vec<MappingRow>,
    pub target_store_id: Uuid,
    pub schedule: Option<String>,
    pub enabled: bool,
    pub has_credentials: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
    pub last_run: Option<RunSummary>,
}

#[derive(sqlx::FromRow)]
struct ViewRow {
    id: Uuid,
    name: String,
    #[sqlx(rename = "type")]
    kind: String,
    config: Value,
    mapping: Json<Vec<MappingRow>>,
    target_store_id: Uuid,
    schedule: Option<String>,
    enabled: bool,
    has_credentials: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    updated_by: String,
    run_id: Option<Uuid>,
    run_status: Option<String>,
    run_started_at: Option<DateTime<Utc>>,
    run_finished_at: Option<DateTime<Utc>>,
    run_document_count: Option<i32>,
    run_error: Option<String>,
}

fn kind_of(raw: &str) -> ApiResult<SourceType> {
    SourceType::parse(raw)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("unknown source type {raw}")))
}

impl ViewRow {
    fn into_view(self) -> ApiResult<SourceView> {
        let last_run = match (self.run_id, self.run_status, self.run_started_at) {
            (Some(id), Some(status), Some(started_at)) => Some(RunSummary {
                id,
                status,
                started_at,
                finished_at: self.run_finished_at,
                document_count: self.run_document_count.unwrap_or(0),
                error: self.run_error,
            }),
            _ => None,
        };
        Ok(SourceView {
            id: self.id,
            name: self.name,
            kind: kind_of(&self.kind)?,
            config: self.config,
            mapping: self.mapping.0,
            target_store_id: self.target_store_id,
            schedule: self.schedule,
            enabled: self.enabled,
            has_credentials: self.has_credentials,
            created_at: self.created_at,
            updated_at: self.updated_at,
            updated_by: self.updated_by,
            last_run,
        })
    }
}

/// The credential columns are never selected here, only whether they are set.
const VIEW_SELECT: &str = "SELECT s.id, s.name, s.type, s.config, s.mapping, s.target_store_id, s.schedule, s.enabled,
        (s.credentials_enc IS NOT NULL) AS has_credentials, s.created_at, s.updated_at, s.updated_by,
        r.id AS run_id, r.status AS run_status, r.started_at AS run_started_at, r.finished_at AS run_finished_at,
        cardinality(r.document_ids) AS run_document_count, r.error AS run_error
    FROM sources s
    LEFT JOIN LATERAL (SELECT id, status, started_at, finished_at, document_ids, error FROM source_runs
                       WHERE source_id = s.id ORDER BY started_at DESC, id DESC LIMIT 1) r ON true";

pub async fn view(s: &AppState, ctx: &JobCtx, id: Uuid) -> ApiResult<SourceView> {
    let mut tx = ctx.begin(&s.db).await?;
    let row: Option<ViewRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{VIEW_SELECT} WHERE s.id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    row.ok_or_else(|| ApiError::NotFound(format!("source {id}")))?
        .into_view()
}

/// Sources ordered by name; `after` is the previous page's last name (the `before` cursor of D1).
/// `hidden`: target stores the caller may not see (crate::walls); their sources are left out.
pub async fn page(
    s: &AppState,
    ctx: &JobCtx,
    after: Option<&str>,
    limit: i64,
    hidden: &[Uuid],
) -> ApiResult<Vec<SourceView>> {
    let mut tx = ctx.begin(&s.db).await?;
    let rows: Vec<ViewRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{VIEW_SELECT} WHERE ($1::text IS NULL OR s.name > $1) AND NOT (s.target_store_id = ANY($3))
          ORDER BY s.name LIMIT $2"
    )))
    .bind(after)
    .bind(limit)
    .bind(hidden)
    .fetch_all(&mut *tx)
    .await?;
    rows.into_iter().map(ViewRow::into_view).collect()
}

#[derive(sqlx::FromRow)]
struct RecordRow {
    id: Uuid,
    name: String,
    #[sqlx(rename = "type")]
    kind: String,
    config: Value,
    mapping: Json<Vec<MappingRow>>,
    target_store_id: Uuid,
    enabled: bool,
    credentials_enc: Option<Vec<u8>>,
    credentials_nonce: Option<Vec<u8>>,
}

/// A source as re-sync and test-connection need it: credentials still sealed.
pub struct SourceRecord {
    pub id: Uuid,
    pub name: String,
    pub kind: SourceType,
    pub config: Value,
    pub mapping: Vec<MappingRow>,
    pub target_store_id: Uuid,
    pub enabled: bool,
    sealed: Option<(Vec<u8>, Vec<u8>)>,
}

impl SourceRecord {
    pub fn has_credentials(&self) -> bool {
        self.sealed.is_some()
    }

    /// Decrypts the credentials:
    /// - 503 `not_configured` without a key
    /// - 409 `credentials_unreadable` when the stored bytes do not open with this key and this source id
    pub fn credentials(&self, key: Option<&SourceKey>) -> ApiResult<Option<Credentials>> {
        let Some((ciphertext, nonce)) = &self.sealed else {
            return Ok(None);
        };
        let key = crypto::require(key)?;
        let unreadable = || {
            ApiError::Conflict(
                "credentials_unreadable",
                "The stored credentials cannot be decrypted with the current CAREBUS_SOURCE_KEY. Enter them again."
                    .into(),
            )
        };
        let plain = key
            .open(self.id.as_bytes(), nonce, ciphertext)
            .map_err(|_| unreadable())?;
        serde_json::from_slice(&plain)
            .map(Some)
            .map_err(|_| unreadable())
    }
}

pub async fn record(s: &AppState, ctx: &JobCtx, id: Uuid) -> ApiResult<SourceRecord> {
    let mut tx = ctx.begin(&s.db).await?;
    let r: RecordRow = sqlx::query_as(
        "SELECT id, name, type, config, mapping, target_store_id, enabled, credentials_enc, credentials_nonce
         FROM sources WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("source {id}")))?;
    Ok(SourceRecord {
        id: r.id,
        name: r.name,
        kind: kind_of(&r.kind)?,
        config: r.config,
        mapping: r.mapping.0,
        target_store_id: r.target_store_id,
        enabled: r.enabled,
        sealed: r.credentials_enc.zip(r.credentials_nonce),
    })
}

/// Seals `c` for source `id` (the id is the associated data).
pub fn seal(s: &AppState, id: Uuid, c: &Credentials) -> ApiResult<Sealed> {
    let key = crypto::require(s.cfg.source_key.as_ref())?;
    let plain = serde_json::to_vec(c).map_err(|e| ApiError::Internal(e.into()))?;
    Ok(key.seal(id.as_bytes(), &plain))
}

/// `config.auth.type`, `none` when absent.
pub fn auth_type(config: &Value) -> &str {
    config["auth"]["type"].as_str().unwrap_or("none")
}

/// Credentials are allowed only where something will use them.
pub fn check_credentials_allowed(kind: SourceType, config: &Value, has: bool) -> ApiResult<()> {
    if has && !(kind.structured() && auth_type(config) != "none") {
        return Err(bad(
            "credentials_not_supported",
            "Only api and datatable sources with config.auth.type bearer or header use credentials. Send \"credentials\": null to remove them.",
        ));
    }
    Ok(())
}

/// Splits off `credentials`, runs the secret rejector over everything else, parses and validates.
pub async fn parse(s: &AppState, ctx: &JobCtx, mut raw: Value) -> ApiResult<Parsed> {
    let Some(obj) = raw.as_object_mut() else {
        return Err(bad(
            "invalid_body",
            "The request body must be a JSON object.",
        ));
    };
    let credentials = credentials_change(obj.remove("credentials"))?;
    secrets::reject_secret_fields(&raw, &[]).map_err(|e| match e {
        ApiError::SecretRejected(name) => ApiError::SecretUseCredentials(name),
        other => other,
    })?;
    let input: SourceInput = secrets::parse_body(raw, "source")?;
    validate(s, ctx, &input).await?;
    if let CredentialsChange::Set(_) = credentials {
        // a create; update re-checks with the stored state
        check_credentials_allowed(input.kind, &input.config, true)?;
    }
    Ok(Parsed { input, credentials })
}

/// The top-level `credentials` key of a body: absent = Keep, `null` = Clear, `{"secret"}` = Set.
pub fn credentials_change(v: Option<Value>) -> ApiResult<CredentialsChange> {
    Ok(match v {
        None => CredentialsChange::Keep,
        Some(Value::Null) => CredentialsChange::Clear,
        Some(v) => CredentialsChange::Set(parse_credentials(v)?),
    })
}

/// The connection settings a test connection takes from a request body (an unsaved api source, or edits to a
/// saved one): `url` and `auth`, plus `options`, which a test ignores. The address itself is checked by
/// `netguard::fetch_sample`, so a refused target answers 400 `address_refused` like a stored one.
pub fn validate_test_config(cfg: &Value) -> ApiResult<()> {
    let o = cfg
        .as_object()
        .ok_or_else(|| bad("invalid_config", "config must be a JSON object."))?;
    if let Some(k) = o
        .keys()
        .find(|k| !["url", "auth", "options"].contains(&k.as_str()))
    {
        return Err(bad(
            "invalid_config",
            format!("config.{} is not an api connection setting.", short(k)),
        ));
    }
    if !o
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|u| !u.trim().is_empty() && u.len() <= 2048)
    {
        return Err(bad(
            "invalid_config",
            "config.url is required (up to 2048 characters).",
        ));
    }
    validate_auth(o.get("auth"))
}

fn parse_credentials(v: Value) -> ApiResult<Credentials> {
    let c: Credentials = serde_json::from_value(v).map_err(|_| {
        bad(
            "invalid_credentials",
            "credentials must be {\"secret\": \"...\"} or null.",
        )
    })?;
    if c.secret.trim().is_empty() || c.secret.len() > 4096 || c.secret.chars().any(char::is_control)
    {
        return Err(bad(
            "invalid_credentials",
            "The secret must be 1-4096 characters without line breaks or control characters.",
        ));
    }
    Ok(c)
}

/// The mapping is checked against the job's ontology (its own transaction); the store against strata_rag for the
/// job's tenant, with no transaction open.
async fn validate(s: &AppState, ctx: &JobCtx, i: &SourceInput) -> ApiResult<()> {
    let name = i.name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(bad("invalid_name", "name must be 1-120 characters."));
    }
    validate_config(i.kind, &i.config, s.cfg.source_fetch_allow_loopback)?;
    if !i.kind.structured() && !i.mapping.is_empty() {
        return Err(bad(
            "mapping_not_supported",
            format!("A {} source has no fields to map.", i.kind.as_str()),
        ));
    }
    validate_mapping(s, ctx, &i.mapping).await?;
    if let Some(c) = &i.schedule
        && !valid_cron(c)
    {
        return Err(bad(
            "invalid_schedule",
            "schedule must be a five-field cron expression such as 0 3 * * *.",
        ));
    }
    // A store walled for a matter the caller is not a member of does not exist for them (crate::walls).
    let (hidden, _) = crate::walls::hidden(s, &ctx.t).await?;
    if hidden.contains(&i.target_store_id.to_string()) {
        return Err(ApiError::NotFound(format!("store {}", i.target_store_id)));
    }
    match strata::store_kind(s, ctx.t.tenant, i.target_store_id).await? {
        None => Err(bad(
            "unknown_store",
            "target_store_id is not a strata_rag store.",
        )),
        Some(k) if k != "leaf" => Err(bad(
            "store_not_leaf",
            "target_store_id must be a leaf store; router stores hold no documents.",
        )),
        Some(_) => Ok(()),
    }
}

/// Five whitespace-separated cron fields of digits and `* / , -`. Stored and shown; nothing runs it yet.
pub fn valid_cron(c: &str) -> bool {
    let fields: Vec<&str> = c.split_whitespace().collect();
    c.len() <= 100
        && fields.len() == 5
        && fields.iter().all(|f| {
            f.bytes()
                .all(|b| b.is_ascii_digit() || b"*/,-".contains(&b))
        })
}

/// Query parameter names that carry a secret in common APIs; such a URL would store the secret in clear.
const URL_SECRET_PARAMS: [&str; 9] = [
    "key",
    "api_key",
    "apikey",
    "token",
    "access_token",
    "secret",
    "password",
    "signature",
    "sig",
];

fn url_ok(raw: &str, allow_loopback: bool) -> ApiResult<()> {
    let url = netguard::check_url(raw, allow_loopback)
        .map_err(|r| bad("invalid_url", format!("{r}.")))?;
    for (k, v) in url.query_pairs() {
        if URL_SECRET_PARAMS.iter().any(|p| p.eq_ignore_ascii_case(&k))
            || secrets::looks_like_secret(&v)
        {
            let name: String = k.chars().take(40).collect();
            return Err(ApiError::SecretUseCredentials(format!(
                "config URL query parameter {name}"
            )));
        }
    }
    Ok(())
}

fn short(k: &str) -> String {
    k.chars().take(64).collect()
}

pub fn validate_config(kind: SourceType, cfg: &Value, allow_loopback: bool) -> ApiResult<()> {
    let o = cfg
        .as_object()
        .ok_or_else(|| bad("invalid_config", "config must be a JSON object."))?;
    let allowed: &[&str] = match kind {
        SourceType::Website => &["mode", "urls", "url", "crawl", "sitemap", "options"],
        SourceType::Api => &["url", "auth", "options"],
        SourceType::File => &["options"],
        SourceType::Datatable => &["url", "format", "sheet", "header_row", "auth", "options"],
    };
    if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(bad(
            "invalid_config",
            format!("config.{} is not a {} setting.", short(k), kind.as_str()),
        ));
    }
    if let Some(opts) = o.get("options") {
        validate_options(opts)?;
    }
    match kind {
        SourceType::Website => validate_website(o, allow_loopback)?,
        SourceType::Api | SourceType::Datatable => {
            let u = o
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("invalid_config", "config.url is required."))?;
            url_ok(u, allow_loopback)?;
            validate_auth(o.get("auth"))?;
            if kind == SourceType::Datatable {
                if !matches!(
                    o.get("format").and_then(Value::as_str),
                    Some("csv" | "xlsx")
                ) {
                    return Err(bad("invalid_config", "config.format must be csv or xlsx."));
                }
                if let Some(sheet) = o.get("sheet")
                    && !sheet
                        .as_str()
                        .is_some_and(|t| !t.trim().is_empty() && t.len() <= 100)
                {
                    return Err(bad(
                        "invalid_config",
                        "config.sheet must be a sheet name (1-100 characters).",
                    ));
                }
                if o.get("header_row").is_some_and(|v| !v.is_boolean()) {
                    return Err(bad(
                        "invalid_config",
                        "config.header_row must be true or false.",
                    ));
                }
            }
        }
        SourceType::File => {}
    }
    Ok(())
}

/// How a website source finds its pages (`config.mode`, default `urls`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebsiteMode {
    /// `config.urls`: 1-50 pages, fetched as listed.
    Urls,
    /// `config.url` is the start page; strata_ingest follows links (`config.crawl`).
    Crawl,
    /// `config.url` is a sitemap; strata_ingest reads the pages it lists (`config.sitemap`).
    Sitemap,
}

impl WebsiteMode {
    /// The mode of a website config; `None` for a value that is not one.
    pub fn of(cfg: &Value) -> Option<Self> {
        Self::parse(cfg.get("mode"))
    }
    fn parse(mode: Option<&Value>) -> Option<Self> {
        match mode {
            None => Some(Self::Urls),
            Some(m) => match m.as_str()? {
                "urls" => Some(Self::Urls),
                "crawl" => Some(Self::Crawl),
                "sitemap" => Some(Self::Sitemap),
                _ => None,
            },
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Urls => "urls",
            Self::Crawl => "crawl",
            Self::Sitemap => "sitemap",
        }
    }
}

/// `urls` mode keeps the 1-50 `urls` list. `crawl` and `sitemap` take ONE start URL under `url` (not `urls[0]`: a
/// one-element list would read as "these pages"), plus that mode's own settings object; the other mode's keys are
/// refused so a stored config never carries settings that are not sent.
fn validate_website(o: &serde_json::Map<String, Value>, allow_loopback: bool) -> ApiResult<()> {
    let mode = WebsiteMode::parse(o.get("mode")).ok_or_else(|| {
        bad(
            "invalid_config",
            "config.mode must be urls, crawl or sitemap.",
        )
    })?;
    let own: &[&str] = match mode {
        WebsiteMode::Urls => &["urls"],
        WebsiteMode::Crawl => &["url", "crawl"],
        WebsiteMode::Sitemap => &["url", "sitemap"],
    };
    if let Some(k) = ["urls", "url", "crawl", "sitemap"]
        .into_iter()
        .find(|k| o.contains_key(*k) && !own.contains(k))
    {
        return Err(bad(
            "invalid_config",
            format!(
                "config.{k} is not a setting of a website source in {} mode.",
                mode.as_str()
            ),
        ));
    }
    match mode {
        WebsiteMode::Urls => {
            let urls = o
                .get("urls")
                .and_then(Value::as_array)
                .filter(|a| (1..=50).contains(&a.len()))
                .ok_or_else(|| bad("invalid_config", "config.urls must list 1 to 50 URLs."))?;
            for u in urls {
                let u = u.as_str().ok_or_else(|| {
                    bad(
                        "invalid_config",
                        "every entry of config.urls must be a URL string.",
                    )
                })?;
                url_ok(u, allow_loopback)?;
            }
        }
        WebsiteMode::Crawl | WebsiteMode::Sitemap => {
            let u = o.get("url").and_then(Value::as_str).ok_or_else(|| {
                bad(
                    "invalid_config",
                    format!(
                        "config.url (the {}) is required in {} mode.",
                        if mode == WebsiteMode::Crawl {
                            "start page"
                        } else {
                            "sitemap URL"
                        },
                        mode.as_str()
                    ),
                )
            })?;
            url_ok(u, allow_loopback)?;
            if mode == WebsiteMode::Crawl {
                if let Some(c) = o.get("crawl") {
                    validate_crawl(c)?;
                }
            } else if let Some(c) = o.get("sitemap") {
                validate_sitemap(c)?;
            }
        }
    }
    Ok(())
}

fn int_in(x: &Value, lo: u64, hi: u64) -> bool {
    x.as_u64().is_some_and(|n| (lo..=hi).contains(&n))
}

/// A list of 1-50 non-empty strings of at most `max_len` characters, each passing `each`.
fn str_list(x: &Value, max_len: usize, each: impl Fn(&str) -> bool) -> bool {
    x.as_array().is_some_and(|a| {
        (1..=50).contains(&a.len())
            && a.iter().all(|v| {
                v.as_str().is_some_and(|t| {
                    !t.trim().is_empty()
                        && t.chars().count() <= max_len
                        && !t.chars().any(char::is_control)
                        && each(t)
                })
            })
    })
}

/// A bare host name such as `help.example.nl`: letters, digits, `-` and `.`, no scheme, port or path.
fn host_ok(h: &str) -> bool {
    h.len() <= 253
        && h.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// strata_ingest's crawl settings: `max_depth` 1-5, `max_pages` 1-200, `allowed_domains` host names,
/// `link_query` text. Each is optional (strata_ingest defaults depth 2, 50 pages).
fn validate_crawl(v: &Value) -> ApiResult<()> {
    let o = v
        .as_object()
        .ok_or_else(|| bad("invalid_config", "config.crawl must be an object."))?;
    for (k, x) in o {
        let (ok, rule) = match k.as_str() {
            "max_depth" => (int_in(x, 1, 5), "a whole number from 1 to 5"),
            "max_pages" => (int_in(x, 1, 200), "a whole number from 1 to 200"),
            "allowed_domains" => (
                str_list(x, 253, host_ok),
                "a list of 1-50 host names such as help.example.nl",
            ),
            "link_query" => (
                x.as_str().is_some_and(|t| {
                    !t.trim().is_empty()
                        && t.chars().count() <= 500
                        && !t.chars().any(char::is_control)
                }),
                "text of 1-500 characters",
            ),
            _ => {
                return Err(bad(
                    "invalid_config",
                    format!("config.crawl.{} is not a crawl setting.", short(k)),
                ));
            }
        };
        if !ok {
            return Err(bad(
                "invalid_config",
                format!("config.crawl.{k} must be {rule}."),
            ));
        }
    }
    Ok(())
}

/// strata_ingest's sitemap settings: `max_urls` 1-1000, `include` / `exclude` URL substrings. Each is optional
/// (strata_ingest defaults to 200 URLs).
fn validate_sitemap(v: &Value) -> ApiResult<()> {
    let o = v
        .as_object()
        .ok_or_else(|| bad("invalid_config", "config.sitemap must be an object."))?;
    for (k, x) in o {
        let (ok, rule) = match k.as_str() {
            "max_urls" => (int_in(x, 1, 1000), "a whole number from 1 to 1000"),
            "include" | "exclude" => (
                str_list(x, 200, |_| true),
                "a list of 1-50 URL fragments of at most 200 characters",
            ),
            _ => {
                return Err(bad(
                    "invalid_config",
                    format!("config.sitemap.{} is not a sitemap setting.", short(k)),
                ));
            }
        };
        if !ok {
            return Err(bad(
                "invalid_config",
                format!("config.sitemap.{k} must be {rule}."),
            ));
        }
    }
    Ok(())
}

fn validate_options(v: &Value) -> ApiResult<()> {
    let o = v
        .as_object()
        .ok_or_else(|| bad("invalid_config", "config.options must be an object."))?;
    for (k, x) in o {
        let ok = match k.as_str() {
            "ocr" => x.as_str().is_some_and(|t| {
                (1..=20).contains(&t.len())
                    && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            }),
            "captions" => x.is_boolean(),
            "language" => {
                x.is_null()
                    || x.as_str().is_some_and(|t| {
                        (1..=35).contains(&t.len())
                            && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    })
            }
            _ => false,
        };
        if !ok {
            return Err(bad(
                "invalid_config",
                format!(
                    "config.options.{} is not a valid strata_ingest option.",
                    short(k)
                ),
            ));
        }
    }
    Ok(())
}

const FORBIDDEN_HEADERS: [&str; 8] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "cookie",
];

fn header_name_ok(h: &str) -> bool {
    (1..=64).contains(&h.len())
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && !FORBIDDEN_HEADERS.contains(&h.to_ascii_lowercase().as_str())
}

pub fn validate_auth(a: Option<&Value>) -> ApiResult<()> {
    let Some(a) = a else { return Ok(()) };
    let o = a
        .as_object()
        .ok_or_else(|| bad("invalid_config", "config.auth must be an object."))?;
    if o.keys().any(|k| k != "type" && k != "header_name") {
        return Err(bad(
            "invalid_config",
            "config.auth takes only type and header_name.",
        ));
    }
    match (o.get("type").and_then(Value::as_str), o.get("header_name")) {
        (Some("none" | "bearer"), None) => Ok(()),
        (Some("header"), Some(Value::String(h))) if header_name_ok(h) => Ok(()),
        (Some("header"), _) => Err(bad(
            "invalid_config",
            "config.auth.header_name must be an HTTP header name such as X-Api-Key (not Host, Cookie or a hop-by-hop header).",
        )),
        _ => Err(bad(
            "invalid_config",
            "config.auth.type must be none, bearer or header; header_name goes only with header.",
        )),
    }
}

async fn validate_mapping(s: &AppState, ctx: &JobCtx, rows: &[MappingRow]) -> ApiResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = ctx.begin(&s.db).await?;
    for (n, m) in rows.iter().enumerate() {
        let f = m.source_field.trim();
        if f.is_empty() || f.len() > 200 || rows[..n].iter().any(|p| p.source_field.trim() == f) {
            return Err(bad(
                "invalid_mapping",
                "each mapping row needs a distinct, non-empty source_field (at most 200 characters).",
            ));
        }
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM eff_ontology_attributes WHERE entity = $1 AND name = $2)",
        )
        .bind(&m.entity)
        .bind(&m.attribute)
        .fetch_one(&mut *tx)
        .await?;
        if !known {
            return Err(bad(
                "unknown_attribute",
                format!(
                    "The ontology has no attribute {}.{}.",
                    short(&m.entity),
                    short(&m.attribute)
                ),
            ));
        }
    }
    Ok(())
}

fn conflict_on_name(
    r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
    name: &str,
) -> ApiResult<u64> {
    match r {
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23505") => {
            Err(ApiError::Conflict(
                "source_exists",
                format!("A source named {} already exists.", short(name)),
            ))
        }
        r => Ok(r?.rows_affected()),
    }
}

/// `tenant_id` / `job_id` come from the scoped transaction's GUC defaults (migration 0007).
pub async fn insert(
    s: &AppState,
    ctx: &JobCtx,
    id: Uuid,
    i: &SourceInput,
    sealed: Option<&Sealed>,
    actor: &str,
) -> ApiResult<()> {
    let mut tx = ctx.begin(&s.db).await?;
    let r = sqlx::query(
        "INSERT INTO sources (id, name, type, config, mapping, target_store_id, schedule, enabled,
                              credentials_enc, credentials_nonce, updated_by)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
    )
    .bind(id)
    .bind(i.name.trim())
    .bind(i.kind.as_str())
    .bind(&i.config)
    .bind(Json(&i.mapping))
    .bind(i.target_store_id)
    .bind(&i.schedule)
    .bind(i.enabled)
    .bind(sealed.map(|x| x.ciphertext.clone()))
    .bind(sealed.map(|x| x.nonce.clone()))
    .bind(actor)
    .execute(&mut *tx)
    .await;
    conflict_on_name(r, &i.name)?;
    tx.commit().await?;
    Ok(())
}

pub async fn update(
    s: &AppState,
    ctx: &JobCtx,
    id: Uuid,
    i: &SourceInput,
    change: &SealedChange,
    actor: &str,
) -> ApiResult<()> {
    let (mode, enc, nonce) = match change {
        SealedChange::Keep => ("keep", None, None),
        SealedChange::Clear => ("clear", None, None),
        SealedChange::Set(x) => ("set", Some(x.ciphertext.clone()), Some(x.nonce.clone())),
    };
    let mut tx = ctx.begin(&s.db).await?;
    let r = sqlx::query(
        "UPDATE sources SET name=$2, type=$3, config=$4, mapping=$5, target_store_id=$6, schedule=$7, enabled=$8,
                credentials_enc   = CASE $9::text WHEN 'keep' THEN credentials_enc   WHEN 'clear' THEN NULL ELSE $10::bytea END,
                credentials_nonce = CASE $9::text WHEN 'keep' THEN credentials_nonce WHEN 'clear' THEN NULL ELSE $11::bytea END,
                updated_at = now(), updated_by = $12
         WHERE id = $1",
    )
    .bind(id)
    .bind(i.name.trim())
    .bind(i.kind.as_str())
    .bind(&i.config)
    .bind(Json(&i.mapping))
    .bind(i.target_store_id)
    .bind(&i.schedule)
    .bind(i.enabled)
    .bind(mode)
    .bind(enc)
    .bind(nonce)
    .bind(actor)
    .execute(&mut *tx)
    .await;
    if conflict_on_name(r, &i.name)? == 0 {
        return Err(ApiError::NotFound(format!("source {id}")));
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn web(cfg: Value) -> ApiResult<()> {
        validate_config(SourceType::Website, &cfg, false)
    }

    /// The `invalid_config` message of a refused config.
    fn refused(cfg: Value) -> String {
        match web(cfg.clone()) {
            Err(ApiError::BadRequest("invalid_config", m)) => m,
            other => panic!("{cfg} was not refused as invalid_config: {other:?}"),
        }
    }

    #[test]
    fn urls_mode_is_the_default_and_unchanged() {
        assert!(web(json!({"urls": ["https://help.example.nl/a"]})).is_ok());
        assert!(web(json!({"mode": "urls", "urls": ["https://help.example.nl/a"]})).is_ok());
        assert!(refused(json!({"urls": []})).contains("1 to 50"));
        assert!(refused(json!({})).contains("config.urls"));
        // a crawl setting or a single start url does not belong to urls mode
        assert!(
            refused(
                json!({"urls": ["https://help.example.nl/a"], "url": "https://help.example.nl/"})
            )
            .contains("config.url is not")
        );
        assert!(
            refused(json!({"urls": ["https://help.example.nl/a"], "crawl": {"max_depth": 2}}))
                .contains("config.crawl")
        );
    }

    #[test]
    fn crawl_mode_takes_one_start_url_and_bounded_settings() {
        assert!(web(json!({"mode": "crawl", "url": "https://help.example.nl/"})).is_ok());
        assert!(
            web(json!({"mode": "crawl", "url": "https://help.example.nl/",
                        "crawl": {"max_depth": 5, "max_pages": 200, "allowed_domains": ["help.example.nl"],
                                  "link_query": "returns and warranty"},
                        "options": {"captions": false}}))
            .is_ok()
        );
        assert!(refused(json!({"mode": "crawl"})).contains("config.url"));
        assert!(
            refused(json!({"mode": "crawl", "url": "https://help.example.nl/",
                           "urls": ["https://help.example.nl/a"]}))
            .contains("config.urls")
        );
        assert!(
            refused(json!({"mode": "crawl", "url": "https://help.example.nl/", "sitemap": {}}))
                .contains("config.sitemap")
        );
        for (crawl, key) in [
            (json!({"max_depth": 0}), "max_depth"),
            (json!({"max_depth": 6}), "max_depth"),
            (json!({"max_depth": 2.5}), "max_depth"),
            (json!({"max_pages": 0}), "max_pages"),
            (json!({"max_pages": 201}), "max_pages"),
            (json!({"max_pages": "50"}), "max_pages"),
            (json!({"allowed_domains": []}), "allowed_domains"),
            (
                json!({"allowed_domains": ["https://help.example.nl"]}),
                "allowed_domains",
            ),
            (
                json!({"allowed_domains": ["help.example.nl/path"]}),
                "allowed_domains",
            ),
            (json!({"link_query": ""}), "link_query"),
            (json!({"link_query": "x".repeat(501)}), "link_query"),
            (json!({"follow_external": true}), "follow_external"),
        ] {
            let m = refused(
                json!({"mode": "crawl", "url": "https://help.example.nl/", "crawl": crawl}),
            );
            assert!(m.contains(&format!("config.crawl.{key}")), "{m}");
        }
        assert!(
            refused(json!({"mode": "crawl", "url": "https://help.example.nl/", "crawl": [1]}))
                .contains("must be an object")
        );
    }

    #[test]
    fn crawl_and_sitemap_urls_pass_the_same_url_guard() {
        for mode in ["crawl", "sitemap"] {
            let r = web(json!({"mode": mode, "url": "http://10.0.0.1/sitemap.xml"}));
            assert!(
                matches!(r, Err(ApiError::BadRequest("invalid_url", _))),
                "{mode}: {r:?}"
            );
        }
    }

    #[test]
    fn sitemap_mode_takes_one_url_and_bounded_filters() {
        assert!(
            web(json!({"mode": "sitemap", "url": "https://help.example.nl/sitemap.xml"})).is_ok()
        );
        assert!(
            web(json!({"mode": "sitemap", "url": "https://help.example.nl/sitemap.xml",
                        "sitemap": {"max_urls": 1000, "include": ["/help/"], "exclude": ["/archive/", "?print"]}}))
            .is_ok()
        );
        assert!(
            refused(
                json!({"mode": "sitemap", "url": "https://help.example.nl/sitemap.xml",
                           "crawl": {"max_depth": 2}})
            )
            .contains("config.crawl")
        );
        for (sitemap, key) in [
            (json!({"max_urls": 0}), "max_urls"),
            (json!({"max_urls": 1001}), "max_urls"),
            (json!({"include": []}), "include"),
            (json!({"include": [""]}), "include"),
            (json!({"exclude": [7]}), "exclude"),
            (json!({"exclude": ["x".repeat(201)]}), "exclude"),
            (json!({"max_pages": 10}), "max_pages"),
        ] {
            let m = refused(
                json!({"mode": "sitemap", "url": "https://help.example.nl/sitemap.xml", "sitemap": sitemap}),
            );
            assert!(m.contains(&format!("config.sitemap.{key}")), "{m}");
        }
    }

    #[test]
    fn an_unknown_mode_or_key_is_refused() {
        assert!(
            refused(json!({"mode": "spider", "url": "https://help.example.nl/"}))
                .contains("config.mode")
        );
        assert!(
            refused(json!({"mode": null, "urls": ["https://help.example.nl/a"]}))
                .contains("config.mode")
        );
        assert!(
            refused(json!({"mode": "crawl", "url": "https://help.example.nl/", "depth": 2}))
                .contains("config.depth is not a website setting")
        );
    }

    #[test]
    fn website_mode_of_a_stored_config() {
        assert_eq!(
            WebsiteMode::of(&json!({"urls": []})),
            Some(WebsiteMode::Urls)
        );
        assert_eq!(
            WebsiteMode::of(&json!({"mode": "crawl"})),
            Some(WebsiteMode::Crawl)
        );
        assert_eq!(
            WebsiteMode::of(&json!({"mode": "sitemap"})),
            Some(WebsiteMode::Sitemap)
        );
        assert_eq!(WebsiteMode::of(&json!({"mode": "x"})), None);
    }
}
