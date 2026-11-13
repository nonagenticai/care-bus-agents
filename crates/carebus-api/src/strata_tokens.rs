//! Where the strata bearer comes from (care-bus 17 C1).
//!
//! Off (`CAREBUS_STRATA_BUNDLE_TOKENS` not `true`, the default): the static `STRATA_RAG_TOKEN` /
//! `STRATA_INGEST_TOKEN`, pinned to `STRATA_TOKENS_TENANT_ID`; any other tenant is 409
//! `strata_not_provisioned_for_tenant` and nothing is sent. Exactly the behaviour before this module.
//!
//! On: a short-lived licence token per (tenant, app), minted by the licences service
//! (`POST {LICENSE_ADMIN_BASE_URL}/internal/tenants/{tenant}/bundle-token?app=strata-rag|strata-ingest&ttl_minutes=N`,
//! 201 `{token, expires_at, tenant_id, bundle, app, license_id, token_id}`) as the client-credentials SA
//! `care-bus-bundle-minter` (secure-apps, realm role `bundle-token-minter`; token URL `LICENSE_SA_TOKEN_URL`). The
//! token is on the tenant's dependency licence for that tool (licences 078: care-bus requires mcp, strata-rag,
//! strata-ingest), so strata introspects it, requires its app key and takes the tenant from the licence. We still
//! send `X-Tenant-Id` (always the same tenant: strata refuses a different one with 403 `tenant_mismatch`).
//!
//! A minted token is reused until shortly before its `expires_at` and never logged. Refusals:
//! - licences 404 (`no_bundle`, `app_not_in_bundle`, `no_app_licence`): 409 `strata_not_licensed_for_tenant`;
//! - licences 403 (missing role, `unknown_api_client`, `dependency_grant_scope`) or 409 (`ambiguous_*`): 503
//!   `strata_licence_refused` (an operator has to fix the licence set or the minter, not the user);
//! - anything else (unreachable, 5xx, unreadable, a token for another tenant or app): 503 `upstream_unavailable`.
//!
//! The same minter (only when on) also backs the MCP credentials (crate::mcp_credentials): the tenant-level
//! care-bus licence (`app=care-bus&bundle=care-bus`, refusals `not_licensed_for_tenant` / `licence_refused`) and the
//! strata-rag token, through [`StrataTokens::minted`].
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::signup::config::ClientCredentials;
use crate::signup::upstream::ServiceToken;
use crate::upstream::Auth;

/// The licences service as an upstream name (503 `upstream_unavailable` names it).
pub const LICENCES: &str = "licences";
/// `CAREBUS_STRATA_BUNDLE_TOKEN_TTL_MINUTES` when unset (the licences default); licences caps it at 60.
pub const DEFAULT_TTL_MINUTES: u32 = 15;
pub const MAX_TTL_MINUTES: u32 = 60;
pub const DEFAULT_MINTER_CLIENT_ID: &str = "care-bus-bundle-minter";
/// A cached token is dropped this long before it expires (at most half its remaining life).
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// The strata service a token is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StrataApp {
    Rag,
    Ingest,
}

impl StrataApp {
    /// The licences app key (licences 067/078).
    pub fn key(self) -> &'static str {
        match self {
            StrataApp::Rag => "strata-rag",
            StrataApp::Ingest => "strata-ingest",
        }
    }
}

/// `CAREBUS_STRATA_BUNDLE_TOKENS=true` and what minting needs.
#[derive(Clone, Debug)]
pub struct BundleTokenConfig {
    /// `LICENSE_ADMIN_BASE_URL`, no trailing slash.
    pub url: String,
    /// `LICENSE_SA_TOKEN_URL`, `LICENSE_BUNDLE_MINTER_CLIENT_ID` (default `care-bus-bundle-minter`),
    /// `LICENSE_BUNDLE_MINTER_CLIENT_SECRET`.
    pub creds: ClientCredentials,
    /// `CAREBUS_STRATA_BUNDLE_TOKEN_TTL_MINUTES` (1..=60, default 15).
    pub ttl_minutes: u32,
}

impl BundleTokenConfig {
    /// `None` while `CAREBUS_STRATA_BUNDLE_TOKENS` is not `true`. On, every part is required (startup fails naming
    /// the missing variables, never a value).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Option<Self>> {
        let trimmed = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        if trimmed("CAREBUS_STRATA_BUNDLE_TOKENS").as_deref() != Some("true") {
            return Ok(None);
        }
        let url = trimmed("LICENSE_ADMIN_BASE_URL");
        let token_url = trimmed("LICENSE_SA_TOKEN_URL");
        let secret = get("LICENSE_BUNDLE_MINTER_CLIENT_SECRET").filter(|v| !v.trim().is_empty());
        let missing: Vec<&str> = [
            ("LICENSE_ADMIN_BASE_URL", url.is_none()),
            ("LICENSE_SA_TOKEN_URL", token_url.is_none()),
            ("LICENSE_BUNDLE_MINTER_CLIENT_SECRET", secret.is_none()),
        ]
        .into_iter()
        .filter(|(_, m)| *m)
        .map(|(k, _)| k)
        .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "CAREBUS_STRATA_BUNDLE_TOKENS=true needs {} (no strata call could be authorised)",
                missing.join(", ")
            );
        }
        let ttl_minutes = match trimmed("CAREBUS_STRATA_BUNDLE_TOKEN_TTL_MINUTES") {
            None => DEFAULT_TTL_MINUTES,
            Some(v) => v
                .parse::<u32>()
                .ok()
                .filter(|m| (1..=MAX_TTL_MINUTES).contains(m))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "CAREBUS_STRATA_BUNDLE_TOKEN_TTL_MINUTES must be 1 to 60 minutes"
                    )
                })?,
        };
        let url = url.unwrap_or_default();
        let ok = reqwest::Url::parse(&url)
            .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some());
        if !ok {
            anyhow::bail!("LICENSE_ADMIN_BASE_URL must be an http(s) URL");
        }
        Ok(Some(Self {
            url: url.trim_end_matches('/').to_string(),
            creds: ClientCredentials {
                token_url: token_url.unwrap_or_default(),
                client_id: trimmed("LICENSE_BUNDLE_MINTER_CLIENT_ID")
                    .unwrap_or_else(|| DEFAULT_MINTER_CLIENT_ID.into()),
                client_secret: secret.unwrap_or_default(),
            },
            ttl_minutes,
        }))
    }
}

/// What one strata call is sent with: the bearer and the tenant it acts for.
pub struct StrataCred {
    token: Option<String>,
    tenant: Uuid,
    /// `X-App-Id` (strata_rag only: it relays it to inference-router as the calling app).
    app: Option<&'static str>,
    /// `X-Actor-Id` of a call that names no person (strata_rag only): `SERVICE_ACTOR_ID`.
    actor: Option<Uuid>,
}

impl StrataCred {
    pub fn auth(&self) -> Auth<'_> {
        Auth {
            token: self.token.as_deref(),
            tenant: Some(self.tenant),
            app: self.app,
            actor: self.actor,
        }
    }
}

impl std::fmt::Debug for StrataCred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrataCred")
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .field("tenant", &self.tenant)
            .field("app", &self.app)
            .field("actor", &self.actor)
            .finish()
    }
}

#[derive(Deserialize)]
struct Minted {
    token: String,
    expires_at: String,
    tenant_id: String,
    app: String,
    /// The licence the token was minted on (`bindings/ensure` names it, crate::mcp_binding).
    #[serde(default)]
    license_id: Option<String>,
}

/// One cached token: the token, the licence it was minted on (when licences named it), and when it is dropped.
type Cached = (String, Option<Uuid>, Instant);
/// (tenant, licences app key) -> cached token.
type TokenCache = HashMap<(Uuid, &'static str), Cached>;

/// The token source: static (off) or the per-tenant minter with its cache (on).
pub struct StrataTokens {
    minter: Option<(BundleTokenConfig, ServiceToken)>,
    http: reqwest::Client,
    /// (tenant, licences app key) -> (token, licence id, drop at).
    cache: Mutex<TokenCache>,
}

impl StrataTokens {
    pub fn new(cfg: Option<&BundleTokenConfig>, http: reqwest::Client) -> Self {
        Self {
            minter: cfg.map(|c| (c.clone(), ServiceToken::new(&c.creds, http.clone()))),
            http,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Whether tokens are minted per tenant (`CAREBUS_STRATA_BUNDLE_TOKENS=true`).
    pub fn minting(&self) -> bool {
        self.minter.is_some()
    }

    /// The credentials a call to `app` acting for `tenant` is sent with.
    pub async fn cred(&self, cfg: &Config, app: StrataApp, tenant: Uuid) -> ApiResult<StrataCred> {
        // care-bus' identity to strata_rag, which relays it to inference-router (strata_rag#30).
        let (app_id, actor) = match app {
            StrataApp::Rag => (Some(carebus::identity::APP_ID), cfg.service_actor),
            StrataApp::Ingest => (None, None),
        };
        if !self.minting() {
            let auth = match app {
                StrataApp::Rag => cfg.strata_rag_auth(tenant)?,
                StrataApp::Ingest => cfg.strata_ingest_auth(tenant)?,
            };
            return Ok(StrataCred {
                token: auth.token.map(str::to_string),
                tenant,
                app: app_id,
                actor,
            });
        }
        let (token, _) = self.minted(app.key(), None, tenant).await?;
        Ok(StrataCred {
            token: Some(token),
            tenant,
            app: app_id,
            actor,
        })
    }

    /// A short-lived licence token on `tenant`'s licence for licences app `key` (strata-rag, strata-ingest, or
    /// care-bus for MCP), minted under `bundle` when given, reused from the cache until shortly before it expires.
    /// Returned with `Some(until)`, the moment the cache drops it, or `None` when its expiry was unreadable (it is
    /// then used once and not cached). Minting off: 503 `mcp_credentials_unconfigured`.
    pub async fn minted(
        &self,
        key: &'static str,
        bundle: Option<&'static str>,
        tenant: Uuid,
    ) -> ApiResult<(String, Option<Instant>)> {
        let (token, _, until) = self.licence(key, bundle, tenant).await?;
        Ok((token, until))
    }

    /// [`Self::minted`], with the id of the licence the token was minted on (`None` when licences did not name it).
    pub async fn licence(
        &self,
        key: &'static str,
        bundle: Option<&'static str>,
        tenant: Uuid,
    ) -> ApiResult<(String, Option<Uuid>, Option<Instant>)> {
        let Some((m, sa)) = self.minter.as_ref() else {
            return Err(ApiError::Unavailable(
                "mcp_credentials_unconfigured",
                format!(
                    "No {key} licence token can be minted: CAREBUS_STRATA_BUNDLE_TOKENS is not on (carebus-api has \
                     no licences bundle minter)."
                ),
            ));
        };
        if let Some((t, id, until)) = self.cached(tenant, key) {
            return Ok((t, id, Some(until)));
        }
        let (token, id, until) = self.mint(m, sa, key, bundle, tenant).await?;
        if let Some(until) = until {
            lock(&self.cache).insert((tenant, key), (token.clone(), id, until));
        }
        Ok((token, id, until))
    }

    /// The minter's own service-account bearer (`care-bus-bundle-minter`, realm secure-apps): the identity
    /// nonagenticai_mcp_server's `bindings/ensure` accepts for product care-bus. `fresh` drops the cached one first
    /// (it was refused). Minting off: 503 `mcp_credentials_unconfigured`.
    pub async fn minter_bearer(&self, fresh: bool) -> ApiResult<String> {
        let Some((_, sa)) = self.minter.as_ref() else {
            return Err(ApiError::Unavailable(
                "mcp_credentials_unconfigured",
                "No licences bundle minter is configured (CAREBUS_STRATA_BUNDLE_TOKENS is not on)."
                    .into(),
            ));
        };
        if fresh {
            sa.forget().await;
        }
        sa.get().await.map_err(|e| {
            tracing::warn!(error = %e, "mcp_binding: minter SA token failed");
            ApiError::Upstream(LICENCES)
        })
    }

    /// Drop `tenant`'s cached token for `key`: the next use mints a new one (the old one was refused).
    pub fn forget(&self, tenant: Uuid, key: &'static str) {
        lock(&self.cache).remove(&(tenant, key));
    }

    fn cached(&self, tenant: Uuid, key: &'static str) -> Option<Cached> {
        let mut g = lock(&self.cache);
        let now = Instant::now();
        g.retain(|_, (_, _, until)| *until > now);
        g.get(&(tenant, key)).cloned()
    }

    /// Mint one token with the licence id licences names; `until` is when the cache drops it (`None`: use it once,
    /// its expiry is unreadable).
    async fn mint(
        &self,
        m: &BundleTokenConfig,
        sa: &ServiceToken,
        key: &'static str,
        bundle: Option<&'static str>,
        tenant: Uuid,
    ) -> ApiResult<(String, Option<Uuid>, Option<Instant>)> {
        let bearer = sa.get().await.map_err(|e| {
            tracing::warn!(error = %e, "strata_bundle_token: minter SA token failed");
            ApiError::Upstream(LICENCES)
        })?;
        let mut url = format!(
            "{}/internal/tenants/{tenant}/bundle-token?app={key}&ttl_minutes={}",
            m.url, m.ttl_minutes
        );
        if let Some(b) = bundle {
            url.push_str(&format!("&bundle={b}"));
        }
        let resp = self
            .http
            .post(url)
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(|_| {
                tracing::warn!(%tenant, app = key, "strata_bundle_token: licences unreachable");
                ApiError::Upstream(LICENCES)
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            return Err(refusal(status.as_u16(), &body, key, tenant));
        }
        let minted: Minted = resp.json().await.map_err(|_| {
            tracing::warn!(%tenant, app = key, "strata_bundle_token: unreadable mint reply");
            ApiError::Upstream(LICENCES)
        })?;
        let same_tenant = Uuid::parse_str(minted.tenant_id.trim()).ok() == Some(tenant);
        if !same_tenant || minted.app != key || minted.token.is_empty() {
            tracing::error!(
                %tenant,
                app = key,
                "strata_bundle_token: licences minted for another tenant or app; not used"
            );
            return Err(ApiError::Upstream(LICENCES));
        }
        let until = DateTime::parse_from_rfc3339(minted.expires_at.trim())
            .ok()
            .and_then(|e| (e.with_timezone(&Utc) - Utc::now()).to_std().ok())
            .map(|left| Instant::now() + left.saturating_sub(REFRESH_MARGIN.min(left / 2)));
        tracing::info!(%tenant, app = key, "strata_bundle_token_minted");
        let id = minted
            .license_id
            .as_deref()
            .and_then(|i| Uuid::parse_str(i.trim()).ok());
        Ok((minted.token, id, until))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// licences' `detail.error` (or `detail` / `error` as a plain code), when it is a short machine code.
fn licences_code(body: &serde_json::Value) -> Option<String> {
    body.pointer("/detail/error")
        .or_else(|| body.get("detail"))
        .or_else(|| body.get("error"))
        .and_then(|v| v.as_str())
        .filter(|c| c.len() <= 64 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .map(str::to_string)
}

/// A refused mint as the caller's error (module docs). A strata app keeps its `strata_*` codes, the platform laya
/// licence (gates, crate::gates) gets `laya_*` ones; the care-bus licence (MCP credentials) gets
/// `not_licensed_for_tenant` / `licence_refused`.
pub fn refusal(status: u16, body: &serde_json::Value, key: &str, tenant: Uuid) -> ApiError {
    let code = licences_code(body);
    let shown = code.as_deref().unwrap_or("refused");
    tracing::warn!(%tenant, app = key, status, code = shown, "strata_bundle_token_refused");
    let (not_licensed, refused) = if key.starts_with("strata-") {
        ("strata_not_licensed_for_tenant", "strata_licence_refused")
    } else if key == "laya" {
        ("laya_not_licensed_for_tenant", "laya_licence_refused")
    } else {
        ("not_licensed_for_tenant", "licence_refused")
    };
    match status {
        404 => ApiError::Conflict(
            not_licensed,
            format!(
                "This tenant holds no {key} licence yet (licences: {shown}), so what needs it is unavailable until \
                 its licence set is ensured."
            ),
        ),
        403 | 409 => ApiError::Unavailable(
            refused,
            format!(
                "The licences service refused to mint a {key} token for this tenant (HTTP {status}: {shown}); an \
                 operator has to fix the tenant's licence set or care-bus's bundle minter."
            ),
        ),
        _ => ApiError::Upstream(LICENCES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn off_unless_true_and_on_needs_every_part() {
        assert!(
            BundleTokenConfig::from_lookup(&lookup(&[]))
                .unwrap()
                .is_none()
        );
        assert!(
            BundleTokenConfig::from_lookup(&lookup(&[("CAREBUS_STRATA_BUNDLE_TOKENS", "1")]))
                .unwrap()
                .is_none()
        );
        let e = BundleTokenConfig::from_lookup(&lookup(&[
            ("CAREBUS_STRATA_BUNDLE_TOKENS", "true"),
            ("LICENSE_ADMIN_BASE_URL", "http://lic"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("LICENSE_SA_TOKEN_URL") && e.contains("LICENSE_BUNDLE_MINTER_CLIENT_SECRET"),
            "{e}"
        );
        let c = BundleTokenConfig::from_lookup(&lookup(&[
            ("CAREBUS_STRATA_BUNDLE_TOKENS", "true"),
            ("LICENSE_ADMIN_BASE_URL", "http://lic/"),
            ("LICENSE_SA_TOKEN_URL", "http://kc/token"),
            ("LICENSE_BUNDLE_MINTER_CLIENT_SECRET", "s3cr3t-minter"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.url, "http://lic");
        assert_eq!(c.creds.client_id, DEFAULT_MINTER_CLIENT_ID);
        assert_eq!(c.ttl_minutes, DEFAULT_TTL_MINUTES);
        assert!(
            !format!("{c:?}").contains("s3cr3t-minter"),
            "Debug never prints the secret"
        );
        for bad in ["0", "61", "x"] {
            let get = move |k: &str| match k {
                "CAREBUS_STRATA_BUNDLE_TOKENS" => Some("true".to_string()),
                "LICENSE_ADMIN_BASE_URL" => Some("http://lic".to_string()),
                "LICENSE_SA_TOKEN_URL" => Some("http://kc/token".to_string()),
                "LICENSE_BUNDLE_MINTER_CLIENT_SECRET" => Some("s".to_string()),
                "CAREBUS_STRATA_BUNDLE_TOKEN_TTL_MINUTES" => Some(bad.to_string()),
                _ => None,
            };
            assert!(BundleTokenConfig::from_lookup(&get).is_err(), "{bad}");
        }
    }

    #[test]
    fn refusals_map_to_clear_errors() {
        let t = Uuid::new_v4();
        let e = refusal(
            404,
            &json!({"detail": {"error": "no_app_licence"}}),
            StrataApp::Rag.key(),
            t,
        );
        assert_eq!(
            e.status_and_code(),
            (
                axum::http::StatusCode::CONFLICT,
                "strata_not_licensed_for_tenant"
            )
        );
        assert!(e.to_string().contains("no_app_licence") && e.to_string().contains("strata-rag"));
        let e = refusal(
            403,
            &json!({"detail": "Role required: bundle-token-minter"}),
            StrataApp::Ingest.key(),
            t,
        );
        assert_eq!(
            e.status_and_code(),
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "strata_licence_refused"
            )
        );
        let e = refusal(500, &json!({}), StrataApp::Rag.key(), t);
        assert_eq!(e.code(), "upstream_unavailable");
        let e = refusal(
            404,
            &json!({"detail": {"error": "no_app_licence"}}),
            "care-bus",
            t,
        );
        assert_eq!(
            e.status_and_code(),
            (axum::http::StatusCode::CONFLICT, "not_licensed_for_tenant")
        );
        let e = refusal(
            409,
            &json!({"detail": {"error": "ambiguous_app_licence"}}),
            "care-bus",
            t,
        );
        assert_eq!(
            e.status_and_code(),
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "licence_refused"
            )
        );
        let e = refusal(500, &json!({}), "care-bus", t);
        assert_eq!(e.code(), "upstream_unavailable");
        let e = refusal(
            404,
            &json!({"detail": {"error": "app_not_in_bundle"}}),
            "laya",
            t,
        );
        assert_eq!(
            e.status_and_code(),
            (
                axum::http::StatusCode::CONFLICT,
                "laya_not_licensed_for_tenant"
            )
        );
        let e = refusal(
            403,
            &json!({"detail": {"error": "unknown_api_client"}}),
            "laya",
            t,
        );
        assert_eq!(
            e.status_and_code(),
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "laya_licence_refused"
            )
        );
    }
}
