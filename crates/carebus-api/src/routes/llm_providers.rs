//! `/api/providers`: the tenant's LLM providers and the uses bound to them (spec 2026-10-18 §2, D38).
//! `qualify` (carebus-api's chat qualifier, an OpenAI-compatible endpoint), `gates` (Laya System-1), and plan 18's
//! `research` / `research_write` (the researcher's loop and memo writer, OpenAI-compatible). Tenant-level:
//! read = any tenant member, write = tenant_admin. The strata_rag / strata_ingest slots live under
//! `/api/providers/slots` (crate::routes::providers).
//!
//! `PUT /api/providers/settings` (tenant_admin) sets the tenant's LLM settings (migration 0023): today only
//! `external_llm_allowed` (default false), which gates research runs to the platform's own model unless set. It
//! applies ONLY to research runs for now; `qualify` and `gates` ignore it. `GET /api/providers` returns it.
//!
//! `token` is write-only end to end: sealed with `CAREBUS_SOURCE_KEY` (crate::crypto, bound to the provider id) into
//! `secret_enc`, never returned (only `has_token`), never logged, audited or echoed.
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit;
use crate::crypto::{self, SourceKey, Unreadable};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::netguard;
use crate::state::AppState;
use crate::tenancy::{Member, TenantAdmin, TenantCtx};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/providers", get(list).post(create))
        .route("/api/providers/{id}", put(update).delete(remove))
        // Static, so it never reaches `/api/providers/{id}` (axum matches static segments first; `{id}` has no POST).
        .route("/api/providers/test", post(test_unsaved))
        .route("/api/providers/{id}/test", post(test))
        .route("/api/providers/uses/{use_name}", put(bind))
        // Static, so it never reaches `/api/providers/{id}`.
        .route("/api/providers/settings", put(set_settings))
}

/// The provider test's whole budget (connect + answer).
const TEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A `/models` answer larger than this is not parsed (the probe still reports its status).
const MAX_MODELS_BODY: usize = 1 << 20;
/// At most this many model names come back from a test.
const MAX_MODELS: usize = 200;
const MAX_TOKEN_CHARS: usize = 4096;
const MAX_URL_CHARS: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// OpenAI-compatible Chat Completions (vLLM, …): models at `{base_url}/models`.
    Openai,
    /// Laya System-1: models at `{base_url}/v1/models`.
    Laya,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Openai => "openai",
            Kind::Laya => "laya",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Kind::Openai),
            "laya" => Some(Kind::Laya),
            _ => None,
        }
    }
    fn models_url(self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        match self {
            Kind::Openai => format!("{base}/models"),
            Kind::Laya => format!("{base}/v1/models"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    Qualify,
    Gates,
    /// The researcher's tool loop (plan, read, act; plan 18).
    Research,
    /// The researcher's final memo writer (plan 18). Citation verification is code, not a use.
    ResearchWrite,
}

impl Use {
    pub const ALL: [Use; 4] = [Use::Qualify, Use::Gates, Use::Research, Use::ResearchWrite];
    pub fn as_str(self) -> &'static str {
        match self {
            Use::Qualify => "qualify",
            Use::Gates => "gates",
            Use::Research => "research",
            Use::ResearchWrite => "research_write",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "qualify" => Some(Use::Qualify),
            "gates" => Some(Use::Gates),
            "research" => Some(Use::Research),
            "research_write" => Some(Use::ResearchWrite),
            _ => None,
        }
    }
    /// The only provider kind this use may be bound to.
    pub fn kind(self) -> Kind {
        match self {
            Use::Qualify | Use::Research | Use::ResearchWrite => Kind::Openai,
            Use::Gates => Kind::Laya,
        }
    }
}

/// A provider as every read returns it: no token, only whether one is stored.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct ProviderView {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub has_token: bool,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

/// A use's binding; `provider_id: null` = unbound.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct UseView {
    #[serde(rename = "use")]
    pub use_name: String,
    pub provider_id: Option<Uuid>,
    pub model: Option<String>,
}

/// The deployment's own qualifier LLM from env (`VLLM_LICENSE_TOKEN`, `CAREBUS_LLM_BASE_URL`, `CAREBUS_MODEL`).
#[derive(Clone, Debug, Serialize)]
pub struct PlatformDefaultView {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub configured: bool,
}

#[derive(Serialize)]
pub struct ProvidersView {
    pub providers: Vec<ProviderView>,
    pub uses: Vec<UseView>,
    pub platform_default: PlatformDefaultView,
    /// May a research run use a model other than the platform's own (migration 0023)? Research runs only.
    pub external_llm_allowed: bool,
}

/// `PUT /api/providers/settings`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsInput {
    external_llm_allowed: bool,
}

#[derive(Serialize)]
pub struct SettingsView {
    pub external_llm_allowed: bool,
}

/// `tenant_llm_settings.external_llm_allowed` of the tenant in the transaction's GUC; no row = false (the default).
pub async fn external_llm_allowed(conn: &mut PgConnection) -> sqlx::Result<bool> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT external_llm_allowed FROM tenant_llm_settings")
            .fetch_optional(conn)
            .await?
            .unwrap_or(false),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInput {
    name: String,
    kind: String,
    base_url: String,
    /// Absent = keep the stored token, `""` = clear it.
    #[serde(default)]
    token: Option<String>,
}

/// The add/edit form's Test button: a provider as typed, not stored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestInput {
    kind: String,
    base_url: String,
    /// The token as typed; absent or blank = none typed.
    #[serde(default)]
    token: Option<String>,
    /// Editing a stored provider: its stored token is used when no token is typed and `base_url` is unchanged.
    #[serde(default)]
    provider_id: Option<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UseInput {
    #[serde(default)]
    provider_id: Option<Uuid>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub models: Vec<String>,
}

fn bad(code: &'static str, msg: &str) -> ApiError {
    ApiError::BadRequest(code, msg.into())
}

/// http(s), a host, no userinfo, query or fragment: a credential in the URL would be stored and returned in clear.
pub fn valid_base_url(raw: &str) -> bool {
    raw.chars().count() <= MAX_URL_CHARS
        && reqwest::Url::parse(raw).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host().is_some()
                && u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none()
        })
}

/// Where a provider may live: only a public host. Loopback, private, link-local, CGNAT (the tailnet) and unspecified
/// literals, IPv6 ULA/link-local, and `localhost`, dotless, `*.local`, `*.internal`, `*.svc`, `*.cluster.local` names
/// are refused (crate::netguard, the rules every source URL follows): the provider test and the gates call send the
/// tenant's token there from inside the cluster. `allow_loopback` is the test-only allowance
/// (`source_fetch_allow_loopback`, never settable from env).
pub fn allowed_base_url(raw: &str, allow_loopback: bool) -> bool {
    netguard::check_url(raw, allow_loopback).is_ok()
}

fn guard_base_url(s: &AppState, base_url: &str) -> ApiResult<()> {
    if allowed_base_url(base_url, s.cfg.source_fetch_allow_loopback) {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(
            "base_url_not_allowed",
            "base_url must be a public host, not a private, loopback, link-local, tailnet or cluster address."
                .into(),
        ))
    }
}

/// The validated input: (name, kind, base_url without trailing `/`, token change).
struct Checked {
    name: String,
    kind: Kind,
    base_url: String,
    /// `None` keep, `Some(None)` clear, `Some(Some(t))` replace.
    token: Option<Option<String>>,
}

fn check(i: ProviderInput) -> ApiResult<Checked> {
    let name = i.name.trim().to_string();
    if !(1..=120).contains(&name.chars().count()) {
        return Err(bad("invalid_name", "name must be 1 to 120 characters."));
    }
    let kind =
        Kind::parse(&i.kind).ok_or_else(|| bad("invalid_kind", "kind must be openai or laya."))?;
    let base_url = i.base_url.trim().trim_end_matches('/').to_string();
    if !valid_base_url(&base_url) {
        return Err(bad(
            "invalid_base_url",
            "base_url must be an http(s) URL without credentials, query or fragment.",
        ));
    }
    let token = match i.token {
        None => None,
        Some(t) => {
            let t = t.trim();
            if t.chars().count() > MAX_TOKEN_CHARS {
                return Err(bad("invalid_token", "token is too long."));
            }
            Some((!t.is_empty()).then(|| t.to_string()))
        }
    };
    Ok(Checked {
        name,
        kind,
        base_url,
        token,
    })
}

fn aad(id: Uuid) -> Vec<u8> {
    format!("llm_provider:{id}").into_bytes()
}

/// `secret_enc`: standard base64 of nonce (12 bytes) followed by the AES-256-GCM ciphertext, bound to `id`.
pub fn seal_token(key: &SourceKey, id: Uuid, token: &str) -> String {
    let s = key.seal(&aad(id), token.as_bytes());
    let mut out = s.nonce;
    out.extend_from_slice(&s.ciphertext);
    STANDARD.encode(out)
}

/// The token sealed by [`seal_token`] for the same provider id.
pub fn open_token(key: &SourceKey, id: Uuid, secret_enc: &str) -> Result<String, Unreadable> {
    let raw = STANDARD.decode(secret_enc).map_err(|_| Unreadable)?;
    if raw.len() < 12 {
        return Err(Unreadable);
    }
    let (nonce, ciphertext) = raw.split_at(12);
    let plain = key.open(&aad(id), nonce, ciphertext)?;
    String::from_utf8(plain).map_err(|_| Unreadable)
}

/// The sealed token for a change, or 503 `not_configured` when a token is given without `CAREBUS_SOURCE_KEY`.
fn sealed(s: &AppState, id: Uuid, token: Option<&str>) -> ApiResult<Option<String>> {
    match token {
        None => Ok(None),
        Some(t) => Ok(Some(seal_token(
            crypto::require(s.cfg.source_key.as_ref())?,
            id,
            t,
        ))),
    }
}

const SELECT_ONE: &str =
    "SELECT id, name, kind, base_url, secret_enc IS NOT NULL AS has_token, updated_at, updated_by
                            FROM llm_providers WHERE id = $1";

async fn one(conn: &mut PgConnection, id: Uuid) -> ApiResult<ProviderView> {
    sqlx::query_as(SELECT_ONE)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| ApiError::NotFound("provider".into()))
}

/// The uses bound to `id`.
async fn bound_uses(conn: &mut PgConnection, id: Uuid) -> ApiResult<Vec<Use>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT \"use\" FROM llm_provider_uses WHERE provider_id = $1 ORDER BY \"use\"",
    )
    .bind(id)
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().filter_map(|u| Use::parse(u)).collect())
}

fn in_use(uses: &[Use]) -> ApiError {
    let names: Vec<&str> = uses.iter().map(|u| u.as_str()).collect();
    ApiError::Conflict(
        "provider_in_use",
        format!(
            "This provider is bound to {}; unbind it first.",
            names.join(", ")
        ),
    )
}

async fn audit_provider(
    conn: &mut PgConnection,
    ctx: &TenantCtx,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    ok: bool,
    details: Value,
) -> ApiResult<()> {
    audit::record(
        conn,
        &ctx.caller.actor,
        ctx.caller.actor_type(),
        action,
        resource_type,
        resource_id,
        ok,
        details,
    )
    .await
}

async fn list(Member(ctx): Member, State(s): State<AppState>) -> ApiResult<Json<ProvidersView>> {
    let mut tx = ctx.begin(&s.db).await?;
    let providers: Vec<ProviderView> = sqlx::query_as(
        "SELECT id, name, kind, base_url, secret_enc IS NOT NULL AS has_token, updated_at, updated_by
           FROM llm_providers ORDER BY name, id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let bound: Vec<UseView> = sqlx::query_as(
        "SELECT \"use\" AS use_name, provider_id, model FROM llm_provider_uses ORDER BY \"use\"",
    )
    .fetch_all(&mut *tx)
    .await?;
    let external = external_llm_allowed(&mut tx).await?;
    drop(tx);
    // Every use always listed; a use without a row reads as unbound.
    let uses = Use::ALL
        .iter()
        .map(|u| {
            bound
                .iter()
                .find(|b| b.use_name == u.as_str())
                .cloned()
                .unwrap_or(UseView {
                    use_name: u.as_str().to_string(),
                    provider_id: None,
                    model: None,
                })
        })
        .collect();
    Ok(Json(ProvidersView {
        providers,
        uses,
        platform_default: s.platform_llm.view(),
        external_llm_allowed: external,
    }))
}

/// Sets the tenant's LLM settings (tenant_admin). `external_llm_allowed: false` makes the researcher fail a run whose
/// model is not the platform's own; it does not touch chat or gates.
async fn set_settings(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiJson(input): ApiJson<SettingsInput>,
) -> ApiResult<Json<SettingsView>> {
    let mut tx = ctx.begin(&s.db).await?;
    let allowed: bool = sqlx::query_scalar(
        "INSERT INTO tenant_llm_settings (external_llm_allowed, updated_at, updated_by)
         VALUES ($1, now(), $2)
         ON CONFLICT (tenant_id) DO UPDATE
            SET external_llm_allowed = EXCLUDED.external_llm_allowed,
                updated_at = now(), updated_by = EXCLUDED.updated_by
         RETURNING external_llm_allowed",
    )
    .bind(input.external_llm_allowed)
    .bind(&ctx.caller.actor)
    .fetch_one(&mut *tx)
    .await?;
    audit_provider(
        &mut tx,
        &ctx,
        "update",
        "tenant_llm_settings",
        &ctx.tenant.to_string(),
        true,
        json!({"external_llm_allowed": allowed}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(SettingsView {
        external_llm_allowed: allowed,
    }))
}

async fn create(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiJson(input): ApiJson<ProviderInput>,
) -> ApiResult<(StatusCode, Json<ProviderView>)> {
    let c = check(input)?;
    guard_base_url(&s, &c.base_url)?;
    let id = Uuid::new_v4();
    let secret_enc = sealed(&s, id, c.token.flatten().as_deref())?;
    let mut tx = ctx.begin(&s.db).await?;
    sqlx::query(
        "INSERT INTO llm_providers (id, name, kind, base_url, secret_enc, updated_at, updated_by)
         VALUES ($1, $2, $3, $4, $5, now(), $6)",
    )
    .bind(id)
    .bind(&c.name)
    .bind(c.kind.as_str())
    .bind(&c.base_url)
    .bind(&secret_enc)
    .bind(&ctx.caller.actor)
    .execute(&mut *tx)
    .await?;
    let view = one(&mut tx, id).await?;
    audit_provider(
        &mut tx,
        &ctx,
        "create",
        "llm_provider",
        &id.to_string(),
        true,
        json!({"name": view.name, "kind": view.kind, "base_url": view.base_url, "has_token": view.has_token}),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(view)))
}

async fn update(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<ProviderInput>,
) -> ApiResult<Json<ProviderView>> {
    let c = check(input)?;
    guard_base_url(&s, &c.base_url)?;
    let (replace, secret_enc) = match &c.token {
        None => (false, None),
        Some(t) => (true, sealed(&s, id, t.as_deref())?),
    };
    let mut tx = ctx.begin(&s.db).await?;
    let current = one(&mut tx, id).await?;
    // Moving a stored token to another host would hand it to whoever runs that host: a new base_url needs the
    // token again (or none stored, or `""` to clear it).
    if c.token.is_none() && current.has_token && current.base_url != c.base_url {
        return Err(ApiError::Unprocessable(
            "token_required_for_new_base_url",
            "Changing base_url needs the token again; the stored token is never sent to a new host.".into(),
        ));
    }
    // A bound use keeps the kind it needs: rebinding comes first.
    let uses = bound_uses(&mut tx, id).await?;
    if uses.iter().any(|u| u.kind() != c.kind) {
        return Err(in_use(&uses));
    }
    sqlx::query(
        "UPDATE llm_providers
            SET name = $2, kind = $3, base_url = $4,
                secret_enc = CASE WHEN $5 THEN $6 ELSE secret_enc END,
                updated_at = now(), updated_by = $7
          WHERE id = $1",
    )
    .bind(id)
    .bind(&c.name)
    .bind(c.kind.as_str())
    .bind(&c.base_url)
    .bind(replace)
    .bind(&secret_enc)
    .bind(&ctx.caller.actor)
    .execute(&mut *tx)
    .await?;
    let view = one(&mut tx, id).await?;
    audit_provider(
        &mut tx,
        &ctx,
        "update",
        "llm_provider",
        &id.to_string(),
        true,
        json!({"name": view.name, "kind": view.kind, "base_url": view.base_url, "has_token": view.has_token,
               "token_changed": replace}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(view))
}

async fn remove(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = ctx.begin(&s.db).await?;
    let view = one(&mut tx, id).await?;
    let uses = bound_uses(&mut tx, id).await?;
    if !uses.is_empty() {
        return Err(in_use(&uses));
    }
    match sqlx::query("DELETE FROM llm_providers WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
    {
        Ok(_) => {}
        // Bound by a concurrent request between the check and the delete.
        Err(sqlx::Error::Database(d)) if d.is_foreign_key_violation() => {
            return Err(ApiError::Conflict(
                "provider_in_use",
                "This provider is bound to a use; unbind it first.".into(),
            ));
        }
        Err(e) => return Err(e.into()),
    }
    audit_provider(
        &mut tx,
        &ctx,
        "delete",
        "llm_provider",
        &id.to_string(),
        true,
        json!({"name": view.name, "kind": view.kind}),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Model names from an OpenAI-style `{"data":[{"id":…}]}` or a `{"models":[…]}` list of names or objects.
fn model_names(v: &Value) -> Vec<String> {
    let items = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(Value::as_array)
        .or_else(|| v.as_array());
    let Some(items) = items else { return vec![] };
    items
        .iter()
        .filter_map(|m| match m {
            Value::String(s) => Some(s.clone()),
            o => o
                .get("id")
                .or_else(|| o.get("name"))
                .and_then(Value::as_str)
                .map(String::from),
        })
        .filter(|n| !n.is_empty() && n.chars().count() <= 200)
        .take(MAX_MODELS)
        .collect()
}

/// `GET {base_url}/models` (openai) or `GET {base_url}/v1/models` (laya) with the stored token, 5 s, no redirects.
/// Reports status, latency and model names only: never the URL, the token, an upstream body or error text. No
/// transaction is held across the probe.
async fn test(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<TestResult>> {
    let (kind, base_url, secret_enc): (String, String, Option<String>) = {
        let mut tx = ctx.begin(&s.db).await?;
        sqlx::query_as("SELECT kind, base_url, secret_enc FROM llm_providers WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::NotFound("provider".into()))?
    };
    let kind = Kind::parse(&kind).ok_or_else(|| {
        ApiError::Internal(anyhow::anyhow!("llm_providers.kind has an unknown value"))
    })?;
    let token = match secret_enc {
        None => None,
        Some(enc) => Some(
            open_token(crypto::require(s.cfg.source_key.as_ref())?, id, &enc).map_err(|_| {
                ApiError::Internal(anyhow::anyhow!(
                    "a stored llm provider token cannot be decrypted"
                ))
            })?,
        ),
    };
    let result = probe(&s, kind, &base_url, token.as_deref()).await?;
    // One row per test: the admin action succeeded even when the provider is down.
    let mut tx = ctx.begin(&s.db).await?;
    audit_provider(
        &mut tx,
        &ctx,
        "test",
        "llm_provider",
        &id.to_string(),
        true,
        json!({"ok": result.ok, "status": result.status, "models": result.models.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

/// The probe both test routes run: `GET {base_url}/models` (openai) or `GET {base_url}/v1/models` (laya) with `token`,
/// 5 s, no redirects. Its host is resolved now, every answer checked and the connection pinned (crate::netguard), so
/// a name that resolves to a private address is never called. Reports status, latency and model names only: never
/// the URL, the token, an upstream body or error text.
async fn probe(
    s: &AppState,
    kind: Kind,
    base_url: &str,
    token: Option<&str>,
) -> ApiResult<TestResult> {
    let started = Instant::now();
    let pinned = match netguard::check_and_pin(
        &kind.models_url(base_url),
        s.cfg.source_fetch_allow_loopback,
    )
    .await
    {
        Ok((url, builder)) => Some((
            url,
            builder.build().map_err(|e| {
                ApiError::Internal(anyhow::anyhow!("building the probe client: {e}"))
            })?,
        )),
        Err(netguard::UNRESOLVED) => None,
        Err(r) => return Err(ApiError::Unprocessable("base_url_not_allowed", r.0.into())),
    };
    let sent = match pinned {
        // The host does not resolve: unreachable, like a refused connection.
        None => None,
        Some((url, client)) => {
            let mut req = client.get(url).timeout(TEST_TIMEOUT);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            req.send().await.ok()
        }
    };
    let (ok, status, models) = match sent {
        Some(resp) => {
            let status = resp.status();
            let models = if status.is_success() {
                match resp.bytes().await {
                    Ok(b) if b.len() <= MAX_MODELS_BODY => serde_json::from_slice::<Value>(&b)
                        .map(|v| model_names(&v))
                        .unwrap_or_default(),
                    _ => vec![],
                }
            } else {
                vec![]
            };
            (status.is_success(), Some(status.as_u16()), models)
        }
        None => (false, None, vec![]),
    };
    Ok(TestResult {
        ok,
        status,
        latency_ms: started.elapsed().as_millis() as u64,
        models,
    })
}

/// The add/edit form's Test: probes a provider as typed, stores nothing. The token is the typed one; else, with
/// `provider_id`, that provider's stored token, but only while `base_url` is its stored one (the update route's rule:
/// a stored token is never sent to a new host, 422 `token_required_for_new_base_url`); else none. Same base_url
/// validation, guard and DNS pinning as a save and the stored test; audited without the token.
async fn test_unsaved(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiJson(input): ApiJson<TestInput>,
) -> ApiResult<Json<TestResult>> {
    let kind = Kind::parse(&input.kind)
        .ok_or_else(|| bad("invalid_kind", "kind must be openai or laya."))?;
    let base_url = input.base_url.trim().trim_end_matches('/').to_string();
    if !valid_base_url(&base_url) {
        return Err(bad(
            "invalid_base_url",
            "base_url must be an http(s) URL without credentials, query or fragment.",
        ));
    }
    guard_base_url(&s, &base_url)?;
    let typed = match input.token.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(t) if t.chars().count() > MAX_TOKEN_CHARS => {
            return Err(bad("invalid_token", "token is too long."));
        }
        Some(t) => Some(t.to_string()),
    };
    let token = match (typed, input.provider_id) {
        (Some(t), _) => Some(t),
        (None, None) => None,
        (None, Some(id)) => {
            let (stored_url, secret_enc): (String, Option<String>) = {
                let mut tx = ctx.begin(&s.db).await?;
                sqlx::query_as("SELECT base_url, secret_enc FROM llm_providers WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| ApiError::NotFound("provider".into()))?
            };
            match secret_enc {
                None => None,
                Some(_) if stored_url != base_url => {
                    return Err(ApiError::Unprocessable(
                        "token_required_for_new_base_url",
                        "Testing another base_url needs the token again; the stored token is never sent to a new host."
                            .into(),
                    ));
                }
                Some(enc) => Some(
                    open_token(crypto::require(s.cfg.source_key.as_ref())?, id, &enc).map_err(
                        |_| {
                            ApiError::Internal(anyhow::anyhow!(
                                "a stored llm provider token cannot be decrypted"
                            ))
                        },
                    )?,
                ),
            }
        }
    };
    let result = probe(&s, kind, &base_url, token.as_deref()).await?;
    let resource = input
        .provider_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "unsaved".into());
    let mut tx = ctx.begin(&s.db).await?;
    audit_provider(
        &mut tx,
        &ctx,
        "test",
        "llm_provider",
        &resource,
        true,
        json!({"unsaved": true, "kind": kind.as_str(), "base_url": base_url, "ok": result.ok,
               "status": result.status, "models": result.models.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

/// Binds (or, with `provider_id: null`, unbinds) a use. `qualify` takes an `openai` provider, `gates` a `laya` one.
async fn bind(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(use_name): ApiPath<String>,
    ApiJson(input): ApiJson<UseInput>,
) -> ApiResult<Json<UseView>> {
    let u = Use::parse(&use_name).ok_or_else(|| {
        bad(
            "unknown_use",
            "use must be qualify, gates, research or research_write.",
        )
    })?;
    let model = match input.model.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(m) if m.chars().count() <= 200 => Some(m.to_string()),
        Some(_) => {
            return Err(bad(
                "invalid_model",
                "model must be at most 200 characters.",
            ));
        }
    };
    let mut tx = ctx.begin(&s.db).await?;
    if let Some(pid) = input.provider_id {
        let kind: Option<String> =
            sqlx::query_scalar("SELECT kind FROM llm_providers WHERE id = $1")
                .bind(pid)
                .fetch_optional(&mut *tx)
                .await?;
        let kind = kind.ok_or_else(|| ApiError::NotFound("provider".into()))?;
        if Kind::parse(&kind) != Some(u.kind()) {
            return Err(ApiError::BadRequest(
                "provider_kind_mismatch",
                format!("{} needs a {} provider.", u.as_str(), u.kind().as_str()),
            ));
        }
    }
    let view: UseView = match sqlx::query_as(
        "INSERT INTO llm_provider_uses (\"use\", provider_id, model, updated_at, updated_by)
         VALUES ($1, $2, $3, now(), $4)
         ON CONFLICT (tenant_id, \"use\") DO UPDATE
            SET provider_id = EXCLUDED.provider_id, model = EXCLUDED.model,
                updated_at = now(), updated_by = EXCLUDED.updated_by
         RETURNING \"use\" AS use_name, provider_id, model",
    )
    .bind(u.as_str())
    .bind(input.provider_id)
    .bind(&model)
    .bind(&ctx.caller.actor)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(v) => v,
        // The provider was deleted by a concurrent request.
        Err(sqlx::Error::Database(d)) if d.is_foreign_key_violation() => {
            return Err(ApiError::NotFound("provider".into()));
        }
        Err(e) => return Err(e.into()),
    };
    audit_provider(
        &mut tx,
        &ctx,
        "bind",
        "llm_provider_use",
        u.as_str(),
        true,
        json!({"provider_id": view.provider_id, "model": view.model}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(view))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SourceKey {
        SourceKey::from_base64(&STANDARD.encode((0u8..32).collect::<Vec<u8>>())).unwrap()
    }

    #[test]
    fn a_sealed_token_opens_only_for_its_provider() {
        let k = key();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let enc = seal_token(&k, a, "tok-123");
        assert!(!enc.contains("tok-123"));
        assert_eq!(open_token(&k, a, &enc).unwrap(), "tok-123");
        assert_eq!(open_token(&k, b, &enc), Err(Unreadable));
        assert_eq!(open_token(&k, a, "not base64!"), Err(Unreadable));
        assert_eq!(
            open_token(&k, a, &STANDARD.encode([1u8; 5])),
            Err(Unreadable)
        );
    }

    #[test]
    fn base_url_is_http_without_credentials_query_or_fragment() {
        for ok in ["https://api.example.com/v1", "http://vllm.example:8001/v1"] {
            assert!(valid_base_url(ok), "{ok}");
        }
        for bad in [
            "ftp://x.example.com",
            "https://user:pw@x.example.com",
            "https://user@x.example.com",
            "https://x.example.com/v1?key=1",
            "https://x.example.com/v1#frag",
            "not a url",
            "",
        ] {
            assert!(!valid_base_url(bad), "{bad}");
        }
    }

    #[test]
    fn base_url_guard_allows_public_hosts_only() {
        for ok in [
            "https://api.nonagentic.ai/laya",
            "https://api.example.com/v1",
            "http://8.8.8.8/v1",
        ] {
            assert!(allowed_base_url(ok, false), "{ok}");
        }
        for bad in [
            "http://127.0.0.1:8001/v1",
            "http://[::1]/v1",
            "http://10.0.0.5/v1",
            "http://172.16.1.1/v1",
            "http://192.168.1.10/v1",
            "http://169.254.169.254/latest",
            "http://100.64.0.7/v1",
            "http://0.0.0.0/v1",
            "http://[fd00::1]/v1",
            "http://[fe80::1]/v1",
            "http://localhost:8001/v1",
            "http://vllm/v1",
            "http://printer.local/v1",
            "http://vllm.internal:8001/v1",
            "http://llm.default.svc/v1",
            "http://llm.ns.svc.cluster.local/v1",
        ] {
            assert!(!allowed_base_url(bad, false), "{bad}");
        }
        // the test-only allowance opens loopback and nothing else
        assert!(allowed_base_url("http://127.0.0.1:9/v1", true));
        assert!(!allowed_base_url("http://10.0.0.5/v1", true));
        assert!(!allowed_base_url("http://localhost:9/v1", true));
    }

    #[test]
    fn models_url_per_kind_and_names_from_both_shapes() {
        assert_eq!(
            Kind::Openai.models_url("https://h/v1/"),
            "https://h/v1/models"
        );
        assert_eq!(
            Kind::Laya.models_url("https://h/laya"),
            "https://h/laya/v1/models"
        );
        assert_eq!(
            model_names(&json!({"object": "list", "data": [{"id": "local-llm"}, {"id": "other"}]})),
            vec!["local-llm", "other"]
        );
        assert_eq!(
            model_names(&json!({"models": ["jev-latest", {"name": "jev-1"}]})),
            vec!["jev-latest", "jev-1"]
        );
        assert!(model_names(&json!({"error": "nope"})).is_empty());
    }

    #[test]
    fn token_absent_keeps_empty_clears() {
        let base = |token: Option<&str>| ProviderInput {
            name: " Local ".into(),
            kind: "openai".into(),
            base_url: "https://h/v1/".into(),
            token: token.map(String::from),
        };
        let c = check(base(None)).unwrap();
        assert_eq!(c.name, "Local");
        assert_eq!(c.base_url, "https://h/v1");
        assert_eq!(c.token, None);
        assert_eq!(check(base(Some(""))).unwrap().token, Some(None));
        assert_eq!(
            check(base(Some(" t "))).unwrap().token,
            Some(Some("t".into()))
        );
        let mut wrong = base(None);
        wrong.kind = "anthropic".into();
        assert!(matches!(
            check(wrong),
            Err(ApiError::BadRequest("invalid_kind", _))
        ));
    }
}
