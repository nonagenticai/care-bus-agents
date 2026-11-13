//! `/api/providers/slots`: the tenant's external model providers (spec §4.1, §12 step 6a), provisioned down to the
//! service that uses them. strata_rag owns `embed`, `rerank` and `chat` (#16), strata_ingest `ocr`, `vision`,
//! `transcribe`. Both expose `GET/PUT /api/tenant/providers`, where the PUT REPLACES the
//! tenant's whole set: a capability left out is removed. So every write here reads the owner's current set and
//! sends it back in full: untouched capabilities as stored and without `api_key` (the service keeps the key
//! for an unchanged `base_url`), the edited one as given.
//!
//! `api_key` is write-only end to end: no read returns it, it is never logged, audited or echoed, and no tracing
//! layer records request bodies.
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::audit;
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::secrets;
use crate::state::AppState;
use crate::strata_tokens::{StrataApp, StrataCred};
use crate::tenancy::{Member, TenantAdmin, TenantCtx};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/providers/slots", get(list))
        .route(
            "/api/providers/slots/{capability}",
            axum::routing::put(save).delete(remove),
        )
}

/// A registration's live probes run concurrently upstream: 10 s each, a chat probe up to 30 s (strata_rag
/// `CHAT_PROBE_TIMEOUT_MAX`).
const WRITE_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Rag,
    Ingest,
}

impl Owner {
    fn name(self) -> &'static str {
        match self {
            Owner::Rag => crate::strata::NAME,
            Owner::Ingest => crate::ingest::NAME,
        }
    }

    /// The owner's base URL and our credentials acting for `tenant` (crate::strata_tokens); `None` when it is not
    /// configured, 409 `strata_not_provisioned_for_tenant` when its static token is not pinned to `tenant` (a
    /// minted token: the mint's refusal).
    async fn endpoint(self, s: &AppState, tenant: Uuid) -> ApiResult<Option<(&str, StrataCred)>> {
        let (base, app) = match self {
            Owner::Rag => (s.cfg.strata_rag_url.as_deref(), StrataApp::Rag),
            Owner::Ingest => (s.cfg.strata_ingest_url.as_deref(), StrataApp::Ingest),
        };
        let Some(base) = base else { return Ok(None) };
        Ok(Some((
            base,
            s.strata_tokens.cred(&s.cfg, app, tenant).await?,
        )))
    }

    /// What it is shown with while it cannot be asked (down or unconfigured).
    fn known(self) -> &'static [&'static str] {
        match self {
            Owner::Rag => &["embed", "rerank", "chat"],
            Owner::Ingest => &["ocr", "vision", "transcribe"],
        }
    }
}

/// Every capability care-bus routes, in display order, with its owner. Whether the owner offers it right now is
/// read from the owner's GET (a strata_rag before #16 does not list `chat`).
const CAPABILITIES: [(&str, Owner); 6] = [
    ("embed", Owner::Rag),
    ("rerank", Owner::Rag),
    ("chat", Owner::Rag),
    ("ocr", Owner::Ingest),
    ("vision", Owner::Ingest),
    ("transcribe", Owner::Ingest),
];

fn owner_of(capability: &str) -> Option<Owner> {
    CAPABILITIES
        .iter()
        .find(|(c, _)| *c == capability)
        .map(|(_, o)| *o)
}

/// The fields a PUT may carry for one capability (`dims` for embed only, `CHAT_FIELDS` for chat only).
const INPUT_FIELDS: [&str; 12] = [
    "base_url",
    "dialect",
    "model",
    "model_id",
    "dims",
    "regions",
    "zdr",
    "api_key",
    "timeout_ms",
    "concurrency",
    "thinking",
    "temperature",
];

/// strata_rag's chat-only settings (#16); `null` = not sent to the provider. strata_rag refuses them elsewhere.
const CHAT_FIELDS: [&str; 2] = ["thinking", "temperature"];

// ---- views ------------------------------------------------------------------------------------------------------

/// A tenant registration as carebus-api passes it on: named fields only, so nothing an upstream adds (least of
/// all a key) leaks through. `dims` is strata_rag's (embed); `thinking` / `temperature` its chat's, when set; `null`
/// elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderView {
    pub base_url: String,
    pub dialect: String,
    pub model: String,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub dims: Option<i64>,
    #[serde(default)]
    pub regions: Vec<String>,
    pub zdr: bool,
    pub timeout_ms: u64,
    pub concurrency: u64,
    pub has_api_key: bool,
    /// Chat only: `chat_template_kwargs.enable_thinking`; `null` = not sent.
    #[serde(default)]
    pub thinking: Option<bool>,
    /// Chat only; `null` = not sent (the provider's default).
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// A deployment default: what it is, never where it runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DefaultView {
    pub dialect: String,
    pub model: String,
    #[serde(default)]
    pub dims: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityView {
    pub capability: String,
    pub owner: &'static str,
    /// The owner answered; `false` = it is down or not configured, and `source` is `null`.
    pub available: bool,
    /// `tenant` | `default` | `unconfigured`, as the owner reports it.
    pub source: Option<String>,
    pub provider: Option<ProviderView>,
    pub default: Option<DefaultView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceView {
    pub name: &'static str,
    /// `ok` | `unavailable` | `unconfigured`
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProvidersView {
    pub services: Vec<ServiceView>,
    pub capabilities: Vec<CapabilityView>,
}

// ---- errors -----------------------------------------------------------------------------------------------------

/// Our errors, plus an owner's own 4xx (and its 503 `provider_key_unconfigured`) passed through with its code and
/// message, in our envelope.
#[derive(Debug)]
enum ProvError {
    Api(ApiError),
    Upstream {
        status: StatusCode,
        code: String,
        message: String,
    },
}

impl From<ApiError> for ProvError {
    fn from(e: ApiError) -> Self {
        ProvError::Api(e)
    }
}

impl ProvError {
    fn status_and_code(&self) -> (StatusCode, String) {
        match self {
            ProvError::Api(e) => {
                let (s, c) = e.status_and_code();
                (s, c.to_string())
            }
            ProvError::Upstream { status, code, .. } => (*status, code.clone()),
        }
    }
}

impl IntoResponse for ProvError {
    fn into_response(self) -> Response {
        match self {
            ProvError::Api(e) => e.into_response(),
            ProvError::Upstream {
                status,
                code,
                message,
            } => (
                status,
                Json(json!({"error": {"code": code, "message": message}})),
            )
                .into_response(),
        }
    }
}

type ProvResult<T> = Result<T, ProvError>;

/// An upstream error code we pass on: a plain snake_case word, else ours.
fn safe_code(code: Option<&str>) -> Option<String> {
    code.filter(|c| {
        !c.is_empty()
            && c.len() <= 64
            && c.starts_with(|ch: char| ch.is_ascii_lowercase())
            && c.chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    })
    .map(str::to_string)
}

// ---- upstream calls ---------------------------------------------------------------------------------------------

type OwnerView = Result<Map<String, Value>, &'static str>;

/// The owner's view per capability, or why it cannot be read (`unconfigured` | `unavailable`).
async fn fetch(s: &AppState, owner: Owner, tenant: Uuid) -> OwnerView {
    let (base, cred) = match owner.endpoint(s, tenant).await {
        Ok(Some(e)) => e,
        Ok(None) => return Err("unconfigured"),
        // handlers check `provisioned` first; never sent with a token that acts for another tenant
        Err(_) => return Err("unavailable"),
    };
    let req = cred.auth().apply(s.http.get(format!(
        "{}/api/tenant/providers",
        base.trim_end_matches('/')
    )));
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(upstream = owner.name(), error = %e, "providers: upstream unreachable");
            return Err("unavailable");
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(
            upstream = owner.name(),
            status = resp.status().as_u16(),
            "providers: upstream refused the read"
        );
        return Err("unavailable");
    }
    match resp.json::<Value>().await {
        Ok(Value::Object(m)) => Ok(m
            .into_iter()
            .filter(|(_, v)| v.get("source").is_some_and(Value::is_string))
            .collect()),
        _ => Err("unavailable"),
    }
}

/// `PUT /api/tenant/providers` with `body`. Success is the owner's new view; its 4xx (not 401/403: those are our
/// credentials) and its 503 `provider_key_unconfigured` pass through; anything else is 503 unavailable.
async fn replace(
    s: &AppState,
    owner: Owner,
    tenant: Uuid,
    body: &Value,
    sent_key: Option<&str>,
) -> ProvResult<()> {
    let Some((base, cred)) = owner.endpoint(s, tenant).await? else {
        return Err(ApiError::Upstream(owner.name()).into());
    };
    let req = cred.auth().apply(
        s.http
            .put(format!(
                "{}/api/tenant/providers",
                base.trim_end_matches('/')
            ))
            .timeout(WRITE_TIMEOUT)
            .json(body),
    );
    let resp = req.send().await.map_err(|e| {
        // reqwest's error text names the URL, never the body
        tracing::warn!(upstream = owner.name(), error = %e, "providers: upstream unreachable");
        ApiError::Upstream(owner.name())
    })?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let parsed: Value = resp.json().await.unwrap_or(Value::Null);
    let code = safe_code(parsed["error"]["code"].as_str());
    let passes = match status.as_u16() {
        401 | 403 | 404 | 405 => false,
        400..=499 => true,
        503 => code.as_deref() == Some("provider_key_unconfigured"),
        _ => false,
    };
    if !passes {
        tracing::warn!(
            upstream = owner.name(),
            status = status.as_u16(),
            "providers: upstream refused or failed the write"
        );
        return Err(ApiError::Upstream(owner.name()).into());
    }
    let generic = format!(
        "{} rejected the provider (HTTP {}).",
        owner.name(),
        status.as_u16()
    );
    let message = match parsed["error"]["message"].as_str() {
        Some(m) => scrub(m, sent_key, &generic),
        None => generic,
    };
    Err(ProvError::Upstream {
        status: StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        code: code.unwrap_or_else(|| "upstream_rejected".into()),
        message,
    })
}

/// The longest piece of a key an echoed message may still contain.
const KEY_WINDOW: usize = 8;
const MAX_MESSAGE_CHARS: usize = 300;

/// An upstream message as we echo it. The contract says it never names the key; make sure of it: the whole key
/// is redacted BEFORE truncating (so a cut cannot leave a prefix), and a message still holding any
/// `KEY_WINDOW`-char piece of the key (a partial or altered echo) is replaced by `generic` altogether.
fn scrub(message: &str, key: Option<&str>, generic: &str) -> String {
    let mut m = message.to_string();
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        m = m.replace(k, "***");
        let chars: Vec<char> = k.chars().collect();
        if chars.len() >= KEY_WINDOW
            && chars
                .windows(KEY_WINDOW)
                .any(|w| m.contains(&w.iter().collect::<String>()))
        {
            return generic.to_string();
        }
    }
    m.chars().take(MAX_MESSAGE_CHARS).collect()
}

// ---- merge ------------------------------------------------------------------------------------------------------

fn capability_view(capability: &str, owner: Owner, v: Option<&Value>) -> CapabilityView {
    let Some(v) = v else {
        return CapabilityView {
            capability: capability.into(),
            owner: owner.name(),
            available: false,
            source: None,
            provider: None,
            default: None,
        };
    };
    let source = v["source"].as_str().unwrap_or("unconfigured").to_string();
    let provider = (source == "tenant")
        .then(|| serde_json::from_value::<ProviderView>(v.clone()).ok())
        .flatten();
    let default = (source == "default")
        .then(|| serde_json::from_value::<DefaultView>(v.clone()).ok())
        .flatten();
    CapabilityView {
        capability: capability.into(),
        owner: owner.name(),
        available: true,
        source: Some(source),
        provider,
        default,
    }
}

/// Both owners' views, one entry per capability: known ones in display order, then any other an owner offers.
fn merge(views: &[(Owner, OwnerView)]) -> ProvidersView {
    let mut services = Vec::new();
    let mut capabilities = Vec::new();
    for (owner, view) in views {
        services.push(ServiceView {
            name: owner.name(),
            status: match view {
                Ok(_) => "ok",
                Err(why) => why,
            },
        });
        match view {
            Ok(m) => {
                let ordered = CAPABILITIES
                    .iter()
                    .filter(|(c, o)| o == owner && m.contains_key(*c))
                    .map(|(c, _)| c.to_string());
                let mut extra: Vec<String> = m
                    .keys()
                    .filter(|c| owner_of(c).is_none())
                    .cloned()
                    .collect();
                extra.sort();
                for c in ordered.chain(extra) {
                    capabilities.push(capability_view(&c, *owner, m.get(&c)));
                }
            }
            Err(_) => {
                for c in owner.known() {
                    capabilities.push(capability_view(c, *owner, None));
                }
            }
        }
    }
    ProvidersView {
        services,
        capabilities,
    }
}

async fn current(s: &AppState, tenant: Uuid) -> ProvidersView {
    let (rag, ingest) = tokio::join!(
        fetch(s, Owner::Rag, tenant),
        fetch(s, Owner::Ingest, tenant)
    );
    merge(&[(Owner::Rag, rag), (Owner::Ingest, ingest)])
}

/// A stored registration as a PUT input: its settings, never `api_key` (omitted = the owner keeps the key). The
/// chat-only settings go back only when set, so no other capability (nor strata_ingest) ever receives them.
fn resend(v: &Value, with_dims: bool) -> Option<Value> {
    let p: ProviderView = serde_json::from_value(v.clone()).ok()?;
    let mut out = json!({
        "base_url": p.base_url, "dialect": p.dialect, "model": p.model, "model_id": p.model_id,
        "regions": p.regions, "zdr": p.zdr, "timeout_ms": p.timeout_ms, "concurrency": p.concurrency,
    });
    if with_dims {
        out["dims"] = json!(p.dims);
    }
    if let Some(t) = p.thinking {
        out["thinking"] = json!(t);
    }
    if let Some(t) = p.temperature {
        out["temperature"] = json!(t);
    }
    Some(out)
}

/// The owner's full set with `capability` replaced by `edit` (`None` = removed).
fn full_set(
    owner_view: &Map<String, Value>,
    capability: &str,
    edit: Option<Value>,
) -> ProvResult<Value> {
    let mut set = Map::new();
    for (c, v) in owner_view {
        if c == capability || v["source"] != "tenant" {
            continue;
        }
        // strata_ingest refuses unknown fields: only strata_rag's views carry `dims`.
        let input = resend(v, v.get("dims").is_some()).ok_or_else(|| {
            tracing::warn!(capability = %c, "providers: a stored registration could not be read back");
            ApiError::Internal(anyhow::anyhow!(
                "the stored {c} provider could not be read back"
            ))
        })?;
        set.insert(c.clone(), input);
    }
    if let Some(e) = edit {
        set.insert(capability.to_string(), e);
    }
    Ok(Value::Object(set))
}

/// The edited capability as sent: an object of known fields, `dims` for embed only, `thinking` (bool) and
/// `temperature` (number) for chat only and dropped when `null` (= not sent), an empty `api_key` dropped (= keep the
/// stored one), and no secret anywhere but `api_key`.
fn checked_input(capability: &str, raw: Value) -> ProvResult<Value> {
    let Value::Object(mut o) = raw else {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "The body must be a provider object.".into(),
        )
        .into());
    };
    if let Some(k) = o.keys().find(|k| {
        !INPUT_FIELDS.contains(&k.as_str())
            || (*k == "dims" && capability != "embed")
            || (CHAT_FIELDS.contains(&k.as_str()) && capability != "chat")
    }) {
        let k: String = k.chars().take(40).collect();
        return Err(ApiError::BadRequest(
            "invalid_body",
            format!("{k} is not a {capability} provider setting."),
        )
        .into());
    }
    if !matches!(
        o.get("thinking"),
        None | Some(Value::Bool(_)) | Some(Value::Null)
    ) {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "thinking must be true, false or null.".into(),
        )
        .into());
    }
    if !matches!(
        o.get("temperature"),
        None | Some(Value::Number(_)) | Some(Value::Null)
    ) {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "temperature must be a number or null.".into(),
        )
        .into());
    }
    for k in CHAT_FIELDS {
        if o.get(k).is_some_and(Value::is_null) {
            o.remove(k);
        }
    }
    if !matches!(
        o.get("api_key"),
        None | Some(Value::String(_)) | Some(Value::Null)
    ) {
        return Err(
            ApiError::BadRequest("invalid_body", "api_key must be a string.".into()).into(),
        );
    }
    if o.get("api_key")
        .is_some_and(|k| k.is_null() || k.as_str().is_some_and(|t| t.trim().is_empty()))
    {
        o.remove("api_key");
    }
    let v = Value::Object(o);
    secrets::reject_secret_fields(&v, &["api_key"])?;
    Ok(v)
}

fn capability_of(capability: &str) -> ProvResult<Owner> {
    owner_of(capability).ok_or_else(|| {
        let c: String = capability.chars().take(40).collect();
        ApiError::NotFound(format!("provider capability {c}")).into()
    })
}

// ---- handlers ---------------------------------------------------------------------------------------------------

/// 409 `strata_not_provisioned_for_tenant` when a configured owner's static token is not pinned to `tenant`:
/// checked before any upstream call, read or write.
async fn provisioned(s: &AppState, tenant: Uuid) -> ApiResult<()> {
    Owner::Rag.endpoint(s, tenant).await?;
    Owner::Ingest.endpoint(s, tenant).await?;
    Ok(())
}

async fn list(Member(ctx): Member, State(s): State<AppState>) -> ApiResult<Json<ProvidersView>> {
    provisioned(&s, ctx.tenant).await?;
    Ok(Json(current(&s, ctx.tenant).await))
}

async fn save(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(capability): ApiPath<String>,
    ApiJson(raw): ApiJson<Value>,
) -> ProvResult<Json<ProvidersView>> {
    provisioned(&s, ctx.tenant).await?;
    let owner = capability_of(&capability)?;
    let input = checked_input(&capability, raw)?;
    let key = input["api_key"].as_str().map(str::to_string);
    let details = json!({
        "owner": owner.name(),
        "dialect": input["dialect"], "model": input["model"],
        "new_api_key": key.is_some(),
    });
    let res = write(
        &s,
        ctx.tenant,
        owner,
        &capability,
        Some(input),
        key.as_deref(),
    )
    .await;
    audit_write(&s, &ctx, "update", &capability, &res, details).await?;
    res.map(Json)
}

async fn remove(
    TenantAdmin(ctx): TenantAdmin,
    State(s): State<AppState>,
    ApiPath(capability): ApiPath<String>,
) -> ProvResult<Json<ProvidersView>> {
    provisioned(&s, ctx.tenant).await?;
    let owner = capability_of(&capability)?;
    let res = write(&s, ctx.tenant, owner, &capability, None, None).await;
    audit_write(
        &s,
        &ctx,
        "delete",
        &capability,
        &res,
        json!({"owner": owner.name()}),
    )
    .await?;
    res.map(Json)
}

/// Read-modify-write of the owner's whole set. carebus-api runs several replicas, so two saves to different
/// capabilities of one owner would each resend the set they read and the later PUT would revert the earlier one.
/// A transaction-scoped advisory lock per (tenant, owner) on carebus-api's own database serialises them across
/// replicas; it is released when the transaction ends (commit, or rollback when an error drops it). The lock
/// touches no table, so this transaction needs no tenant GUC.
async fn write(
    s: &AppState,
    tenant: Uuid,
    owner: Owner,
    capability: &str,
    edit: Option<Value>,
    key: Option<&str>,
) -> ProvResult<ProvidersView> {
    let mut tx = s.db.begin().await.map_err(ApiError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("providers:{tenant}:{}", owner.name()))
        .execute(&mut *tx)
        .await
        .map_err(ApiError::from)?;
    let res = write_locked(s, tenant, owner, capability, edit, key).await;
    tx.commit().await.map_err(ApiError::from)?;
    res
}

async fn write_locked(
    s: &AppState,
    tenant: Uuid,
    owner: Owner,
    capability: &str,
    edit: Option<Value>,
    key: Option<&str>,
) -> ProvResult<ProvidersView> {
    let view = fetch(s, owner, tenant)
        .await
        .map_err(|_| ApiError::Upstream(owner.name()))?;
    let Some(entry) = view.get(capability) else {
        return Err(ApiError::BadRequest(
            "capability_not_supported",
            format!("{} does not offer {capability} providers.", owner.name()),
        )
        .into());
    };
    if edit.is_none() && entry["source"] != "tenant" {
        return Ok(current(s, tenant).await); // nothing registered: nothing to remove
    }
    let body = full_set(&view, capability, edit)?;
    replace(s, owner, tenant, &body, key).await?;
    Ok(current(s, tenant).await)
}

/// Audit row in the request's tenant (providers are tenant-level: job GUC empty): the capability and settings we
/// chose to record, plus on failure the status and code. Never the key, never upstream text.
async fn audit_write<T>(
    s: &AppState,
    ctx: &TenantCtx,
    action: &str,
    capability: &str,
    res: &ProvResult<T>,
    mut details: Value,
) -> ProvResult<()> {
    if let Err(e) = res {
        let (status, code) = e.status_and_code();
        details["error_code"] = json!(code);
        details["http_status"] = json!(status.as_u16());
    }
    let mut tx = ctx.begin(&s.db).await?;
    audit::record(
        &mut tx,
        &ctx.caller.actor,
        ctx.caller.actor_type(),
        action,
        "provider",
        capability,
        res.is_ok(),
        details,
    )
    .await?;
    tx.commit().await.map_err(ApiError::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_capabilities_are_resent_without_their_key_and_defaults_are_left_out() {
        let view: Map<String, Value> = serde_json::from_value(json!({
            "embed": {"source": "tenant", "base_url": "https://e.example.eu/v1", "dialect": "openai",
                      "model": "m", "model_id": null, "dims": 1024, "regions": ["eu"], "zdr": true,
                      "timeout_ms": 30000, "concurrency": 8, "has_api_key": true, "updated_at": "t"},
            "rerank": {"source": "default", "dialect": "cohere", "model": "r", "dims": null},
        }))
        .unwrap();
        let set = full_set(&view, "rerank", Some(json!({"model": "new"}))).unwrap();
        assert_eq!(set["rerank"], json!({"model": "new"}));
        assert!(set["embed"].get("api_key").is_none());
        assert!(set["embed"].get("has_api_key").is_none());
        assert_eq!(set["embed"]["dims"], 1024);
        let set = full_set(&view, "embed", None).unwrap();
        assert_eq!(set, json!({}), "removed, and the default is not registered");
    }

    #[test]
    fn a_resent_ingest_registration_carries_no_dims() {
        let view: Map<String, Value> = serde_json::from_value(json!({
            "ocr": {"source": "tenant", "base_url": "https://o.example.eu/v1", "dialect": "mistral",
                    "model": "m", "model_id": null, "regions": ["eu"], "zdr": true, "timeout_ms": 1,
                    "concurrency": 1, "has_api_key": true, "updated_at": "t"},
        }))
        .unwrap();
        let set = full_set(&view, "vision", Some(json!({}))).unwrap();
        assert!(set["ocr"].get("dims").is_none(), "{set}");
    }

    #[test]
    fn an_empty_api_key_means_keep_and_dims_is_embed_only() {
        let v = checked_input(
            "embed",
            json!({"base_url": "https://x.example.eu", "api_key": "", "dims": 1024}),
        )
        .unwrap();
        assert!(v.get("api_key").is_none());
        assert!(checked_input("ocr", json!({"dims": 1024})).is_err());
        assert!(checked_input("ocr", json!({"api_key": 5})).is_err());
    }

    #[test]
    fn thinking_and_temperature_are_chat_only_typed_and_null_is_not_sent() {
        let v = checked_input("chat", json!({"thinking": true, "temperature": 0.7})).unwrap();
        assert_eq!(
            (&v["thinking"], &v["temperature"]),
            (&json!(true), &json!(0.7))
        );
        let v = checked_input("chat", json!({"thinking": null, "temperature": null})).unwrap();
        assert_eq!(v, json!({}), "null = not sent");
        assert!(checked_input("chat", json!({"thinking": "yes"})).is_err());
        assert!(checked_input("chat", json!({"temperature": "0.2"})).is_err());
        assert!(checked_input("chat", json!({"dims": 1024})).is_err());
        for c in ["embed", "rerank", "vision"] {
            assert!(checked_input(c, json!({"thinking": false})).is_err(), "{c}");
            assert!(
                checked_input(c, json!({"temperature": 0.1})).is_err(),
                "{c}"
            );
        }
    }

    #[test]
    fn a_resent_chat_keeps_its_settings_and_others_never_get_them() {
        let view: Map<String, Value> = serde_json::from_value(json!({
            "embed": {"source": "tenant", "base_url": "https://e.example.eu/v1", "dialect": "openai",
                      "model": "m", "model_id": null, "dims": 1024, "regions": ["eu"], "zdr": true,
                      "timeout_ms": 30000, "concurrency": 8, "has_api_key": true, "updated_at": "t"},
            "chat": {"source": "tenant", "base_url": "https://c.example.eu/v1", "dialect": "openai",
                     "model": "c", "model_id": null, "dims": null, "regions": ["eu"], "zdr": true,
                     "timeout_ms": 60000, "concurrency": 8, "has_api_key": true, "thinking": true,
                     "temperature": 0.3, "updated_at": "t"},
        }))
        .unwrap();
        let set = full_set(&view, "rerank", Some(json!({}))).unwrap();
        assert_eq!(
            (&set["chat"]["thinking"], &set["chat"]["temperature"]),
            (&json!(true), &json!(0.3))
        );
        assert!(set["chat"].get("api_key").is_none());
        assert!(
            set["embed"].get("thinking").is_none() && set["embed"].get("temperature").is_none()
        );
        let set = full_set(&view, "embed", Some(json!({}))).unwrap();
        assert_eq!(
            set["chat"]["timeout_ms"], 60000,
            "chat is not dropped: {set}"
        );
    }

    #[test]
    fn an_echoed_key_is_redacted_before_truncation_and_a_partial_echo_is_not_echoed() {
        let key = "k-0123456789abcdefghijklmnopqrstuv";
        for at in [0, 150, 280, 290, 299, 320] {
            let m = format!("{}{key} tail", "x".repeat(at));
            let out = scrub(&m, Some(key), "generic");
            assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
            for w in key.as_bytes().windows(KEY_WINDOW) {
                assert!(
                    !out.contains(std::str::from_utf8(w).unwrap()),
                    "at {at}: {out}"
                );
            }
        }
        assert_eq!(
            scrub("bad key k-0123456789ab…", Some(key), "generic"),
            "generic"
        );
        assert_eq!(
            scrub("regions must contain eu", Some(key), "g"),
            "regions must contain eu"
        );
        assert_eq!(scrub(&"y".repeat(400), None, "g").len(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn only_plain_upstream_codes_pass() {
        assert_eq!(
            safe_code(Some("region_not_allowed")).as_deref(),
            Some("region_not_allowed")
        );
        assert_eq!(safe_code(Some("Bad Code")), None);
        assert_eq!(safe_code(Some("")), None);
        assert_eq!(safe_code(None), None);
    }
}
