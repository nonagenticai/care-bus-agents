//! Licence gate 2 (W3 plan 15): the caller's tenant must hold an active `care-bus` licence. Port of commonpractice
//! `backend/src/middleware/license-gate.ts`. care-bus stores no per-tenant licence token, so only CP's tenant-level
//! half applies: `lib/license-status.ts` `tenantLevelLicense`, `services/tenant-level-licence.ts`
//! `isValidTenantLevelLicence`, and `services/licenses-admin.ts` `getActiveLicense`. The licence SA (client
//! credentials) asks the licences service for the licence held by `svc:<tenant>:<app>`.
//!
//! The gate fails closed. No valid licence gives 402 `license_required`. A licences service or token endpoint that
//! is unreachable, answers non-2xx or sends an unreadable body gives 402 `license_unavailable`: an outage denies and
//! never admits. Only positive verdicts are cached, for `LICENSE_CACHE_TTL_SEC` (300 s), and they are validated again
//! on every read, so an expiry inside the window still denies. A negative verdict is not cached, so a tenant that has
//! just bought a licence is admitted on its next request.
//!
//! Which requests are gated follows from the guards, as in CP: `tenancy.rs` calls `check` where a `TenantCtx` is
//! resolved, once per request. A super_admin is judged against the tenant it switched into.
//! Routes that resolve no tenant (provisioning, `/api/me`, `/health`) are never
//! gated. When `LICENSE_GATE_2_ENABLED` is not `true`, `check` returns immediately and no request is ever built.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

pub const DEFAULT_APP_KEY: &str = "care-bus";
pub const DEFAULT_CACHE_TTL_SECS: u64 = 300;

#[derive(Clone)]
pub struct LicenceConfig {
    /// `LICENSE_GATE_2_ENABLED=true`.
    pub enabled: bool,
    /// `LICENSE_ADMIN_BASE_URL`: the licences admin API base (`…/licenses/active` is appended).
    pub admin_base_url: String,
    /// `LICENSE_SA_TOKEN_URL`, `LICENSE_SA_CLIENT_ID`, `LICENSE_SA_CLIENT_SECRET`: the licence SA (client credentials).
    pub sa_token_url: String,
    pub sa_client_id: String,
    pub sa_client_secret: String,
    /// `LICENSE_APP_KEY` (default `care-bus`): the app key a licence must carry.
    pub app_key: String,
    /// `LICENSE_CACHE_TTL_SEC` (default 300): how long a positive verdict is reused.
    pub cache_ttl: Duration,
}

impl Default for LicenceConfig {
    /// Off.
    fn default() -> Self {
        Self {
            enabled: false,
            admin_base_url: String::new(),
            sa_token_url: String::new(),
            sa_client_id: String::new(),
            sa_client_secret: String::new(),
            app_key: DEFAULT_APP_KEY.into(),
            cache_ttl: Duration::from_secs(DEFAULT_CACHE_TTL_SECS),
        }
    }
}

impl std::fmt::Debug for LicenceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LicenceConfig")
            .field("enabled", &self.enabled)
            .field("admin_base_url", &self.admin_base_url)
            .field("sa_token_url", &self.sa_token_url)
            .field("sa_client_id", &self.sa_client_id)
            .field("sa_client_secret", &"<redacted>")
            .field("app_key", &self.app_key)
            .field("cache_ttl", &self.cache_ttl)
            .finish()
    }
}

impl LicenceConfig {
    /// With the gate on, every endpoint and credential is required, so startup fails instead of answering 402 to
    /// every tenant (CP `validateConfigInvariants`).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let opt = |k: &str| get(k).map(|v| v.trim().to_string()).unwrap_or_default();
        let enabled = get("LICENSE_GATE_2_ENABLED").as_deref() == Some("true");
        let cache_ttl = match get("LICENSE_CACHE_TTL_SEC").filter(|v| !v.trim().is_empty()) {
            None => DEFAULT_CACHE_TTL_SECS,
            Some(v) => v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|s| *s > 0)
                .context("LICENSE_CACHE_TTL_SEC must be a positive number of seconds")?,
        };
        let c = Self {
            enabled,
            admin_base_url: opt("LICENSE_ADMIN_BASE_URL"),
            sa_token_url: opt("LICENSE_SA_TOKEN_URL"),
            sa_client_id: opt("LICENSE_SA_CLIENT_ID"),
            sa_client_secret: get("LICENSE_SA_CLIENT_SECRET").unwrap_or_default(),
            app_key: Some(opt("LICENSE_APP_KEY"))
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| DEFAULT_APP_KEY.into()),
            cache_ttl: Duration::from_secs(cache_ttl),
        };
        if c.enabled {
            let missing: Vec<&str> = [
                ("LICENSE_ADMIN_BASE_URL", c.admin_base_url.is_empty()),
                ("LICENSE_SA_TOKEN_URL", c.sa_token_url.is_empty()),
                ("LICENSE_SA_CLIENT_ID", c.sa_client_id.is_empty()),
                ("LICENSE_SA_CLIENT_SECRET", c.sa_client_secret.is_empty()),
            ]
            .into_iter()
            .filter(|(_, m)| *m)
            .map(|(k, _)| k)
            .collect();
            if !missing.is_empty() {
                anyhow::bail!(
                    "LICENSE_GATE_2_ENABLED=true needs {} (the gate would refuse every tenant)",
                    missing.join(", ")
                );
            }
        }
        Ok(c)
    }
}

/// The `app` block of an active-licence reply.
#[derive(Clone, Debug, Deserialize)]
pub struct LicenceApp {
    pub key: String,
}

/// `GET /licenses/active` (the introspect response model). Fields other than `active` may be absent; a missing
/// field fails validation.
#[derive(Clone, Debug, Deserialize)]
pub struct ActiveLicence {
    pub active: bool,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub license_id: Option<String>,
    #[serde(default)]
    pub app: Option<LicenceApp>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub subject_match: Option<bool>,
}

/// CP `isValidTenantLevelLicence`: active, the selector matched the svc holder (not the service's fallback pick),
/// not expired (an unparseable expiry is not a denial, as in CP), this app, this tenant (compared as uuids, so
/// case and hyphenation do not matter).
pub fn is_valid(l: &ActiveLicence, tenant: Uuid, app_key: &str, now: DateTime<Utc>) -> bool {
    let unexpired = l.expires_at.as_deref().is_none_or(|e| {
        DateTime::parse_from_rfc3339(e.trim())
            .ok()
            .is_none_or(|t| t.with_timezone(&Utc) > now)
    });
    let same_tenant = l
        .tenant_id
        .as_deref()
        .and_then(|t| Uuid::parse_str(t.trim()).ok())
        == Some(tenant);
    l.active
        && l.subject_match == Some(true)
        && unexpired
        && l.app.as_ref().is_some_and(|a| a.key == app_key)
        && same_tenant
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub struct LicenceGate {
    cfg: LicenceConfig,
    http: reqwest::Client,
    sa_token: Mutex<Option<(String, Instant)>>,
    cache: Mutex<HashMap<Uuid, (ActiveLicence, Instant)>>,
}

impl LicenceGate {
    pub fn new(cfg: LicenceConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            sa_token: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Gate 2 for `tenant`. `Ok` when the gate is off or the tenant holds a valid licence.
    pub async fn check(&self, tenant: Uuid) -> ApiResult<()> {
        if !self.cfg.enabled {
            return Ok(());
        }
        match self.tenant_licence(tenant).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(ApiError::PaymentRequired(
                "license_required",
                "This tenant has no active care-bus license.".into(),
            )),
            Err(e) => {
                tracing::warn!(%tenant, error = %format!("{e:#}"), "licence lookup failed; denying (fail-closed)");
                Err(ApiError::PaymentRequired(
                    "license_unavailable",
                    "Could not verify the tenant license; access denied.".into(),
                ))
            }
        }
    }

    /// The cached, still-valid licence of `tenant`. Never calls out (metering attribution reads it).
    pub fn cached(&self, tenant: Uuid) -> Option<ActiveLicence> {
        let mut cache = lock(&self.cache);
        let hit = cache.get(&tenant).and_then(|(l, until)| {
            (*until > Instant::now() && is_valid(l, tenant, &self.cfg.app_key, Utc::now()))
                .then(|| l.clone())
        });
        if hit.is_none() {
            cache.remove(&tenant);
        }
        hit
    }

    /// The tenant's valid licence, `None` when it holds none; an error when the licences service cannot be asked.
    pub async fn tenant_licence(&self, tenant: Uuid) -> anyhow::Result<Option<ActiveLicence>> {
        if let Some(l) = self.cached(tenant) {
            return Ok(Some(l));
        }
        match self.fetch(tenant).await? {
            Some(l) if is_valid(&l, tenant, &self.cfg.app_key, Utc::now()) => {
                lock(&self.cache).insert(tenant, (l.clone(), Instant::now() + self.cfg.cache_ttl));
                Ok(Some(l))
            }
            _ => Ok(None),
        }
    }

    /// CP `getActiveLicense`: 404 with `no_active_license` is "none"; any other non-2xx is an error.
    async fn fetch(&self, tenant: Uuid) -> anyhow::Result<Option<ActiveLicence>> {
        let token = self.sa_token().await?;
        let base = self.cfg.admin_base_url.trim_end_matches('/');
        let subject = format!("svc:{tenant}:{}", self.cfg.app_key);
        let url = reqwest::Url::parse_with_params(
            &format!("{base}/licenses/active"),
            [
                ("tenant_id", tenant.to_string()),
                ("app_key", self.cfg.app_key.clone()),
                ("subject_id", subject),
            ],
        )
        .context("LICENSE_ADMIN_BASE_URL is not a valid URL")?;
        let res = self
            .http
            .get(url)
            .bearer_auth(&token)
            .send()
            .await
            .context("licences service unreachable")?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
            let literal = body
                .pointer("/detail/error")
                .or_else(|| body.get("error"))
                .and_then(|v| v.as_str());
            if literal == Some("no_active_license") {
                return Ok(None);
            }
            anyhow::bail!("licences service answered 404");
        }
        if !status.is_success() {
            anyhow::bail!("licences service answered {status}");
        }
        let l: ActiveLicence = res
            .json()
            .await
            .context("licences service sent an unreadable licence")?;
        Ok(Some(l))
    }

    /// The licence SA's bearer (client credentials), reused until 30 s before it expires.
    async fn sa_token(&self) -> anyhow::Result<String> {
        {
            let g = lock(&self.sa_token);
            if let Some((t, until)) = g.as_ref()
                && *until > Instant::now()
            {
                return Ok(t.clone());
            }
        }
        let res = self
            .http
            .post(&self.cfg.sa_token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.cfg.sa_client_id.as_str()),
                ("client_secret", self.cfg.sa_client_secret.as_str()),
            ])
            .send()
            .await
            .context("licence SA token endpoint unreachable")?;
        if !res.status().is_success() {
            anyhow::bail!("licence SA token endpoint answered {}", res.status());
        }
        let t: TokenReply = res
            .json()
            .await
            .context("licence SA token reply unreadable")?;
        let life = Duration::from_secs(t.expires_in.unwrap_or(60).saturating_sub(30).max(1));
        *lock(&self.sa_token) = Some((t.access_token.clone(), Instant::now() + life));
        Ok(t.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0195f48b-d000-7000-8000-000000000099";

    fn valid() -> ActiveLicence {
        ActiveLicence {
            active: true,
            tenant_id: Some(T.into()),
            subject_id: Some("5b0c1e7a-0000-4000-8000-000000000001".into()),
            license_id: Some("5b0c1e7a-0000-4000-8000-000000000002".into()),
            app: Some(LicenceApp {
                key: "care-bus".into(),
            }),
            expires_at: Some("2999-01-01T00:00:00Z".into()),
            subject_match: Some(true),
        }
    }

    #[test]
    fn validity_matrix() {
        let t: Uuid = T.parse().unwrap();
        let now = Utc::now();
        let ok = |l: &ActiveLicence| is_valid(l, t, "care-bus", now);
        assert!(ok(&valid()));
        type Edit = fn(&mut ActiveLicence);
        let cases: [(&str, Edit); 8] = [
            ("inactive", |l| l.active = false),
            ("fallback pick", |l| l.subject_match = Some(false)),
            ("no selector answer", |l| l.subject_match = None),
            ("expired", |l| {
                l.expires_at = Some("2020-01-01T00:00:00+00:00".into())
            }),
            ("other app", |l| {
                l.app = Some(LicenceApp {
                    key: "commonpractice".into(),
                })
            }),
            ("no app", |l| l.app = None),
            ("other tenant", |l| {
                l.tenant_id = Some("7f1c9a2e-0000-4000-8000-00000000fe12".into())
            }),
            ("no tenant", |l| l.tenant_id = None),
        ];
        for (name, f) in cases {
            let mut l = valid();
            f(&mut l);
            assert!(!ok(&l), "{name} must not admit");
        }
        // same tenant in another spelling, no expiry, and an unparseable expiry (CP does not deny on it)
        let admitted: [Edit; 4] = [
            |l| l.tenant_id = Some(T.to_uppercase()),
            |l| l.tenant_id = Some(T.replace('-', "")),
            |l| l.expires_at = None,
            |l| l.expires_at = Some("someday".into()),
        ];
        for f in admitted {
            let mut l = valid();
            f(&mut l);
            assert!(ok(&l), "{l:?}");
        }
    }

    #[test]
    fn enabled_without_endpoints_fails_naming_them() {
        let get = |k: &str| match k {
            "LICENSE_GATE_2_ENABLED" => Some("true".to_string()),
            "LICENSE_SA_CLIENT_SECRET" => Some("s3cr3t-value".to_string()),
            _ => None,
        };
        let err = LicenceConfig::from_lookup(&get).unwrap_err().to_string();
        for k in [
            "LICENSE_ADMIN_BASE_URL",
            "LICENSE_SA_TOKEN_URL",
            "LICENSE_SA_CLIENT_ID",
        ] {
            assert!(err.contains(k), "{err}");
        }
        assert!(!err.contains("s3cr3t-value"), "{err}");
        // off: nothing required, defaults applied
        let off = LicenceConfig::from_lookup(&|_: &str| None).unwrap();
        assert!(!off.enabled);
        assert_eq!(off.app_key, "care-bus");
        assert_eq!(off.cache_ttl, Duration::from_secs(300));
        let full = |k: &str| match k {
            "LICENSE_GATE_2_ENABLED" => Some("true".to_string()),
            "LICENSE_ADMIN_BASE_URL" => Some("http://lic.example".to_string()),
            "LICENSE_SA_TOKEN_URL" => Some("http://kc/token".to_string()),
            "LICENSE_SA_CLIENT_ID" => Some("care-bus-licence".to_string()),
            "LICENSE_SA_CLIENT_SECRET" => Some("s3cr3t-value".to_string()),
            _ => None,
        };
        let on = LicenceConfig::from_lookup(&full).unwrap();
        assert!(on.enabled);
        assert!(!format!("{on:?}").contains("s3cr3t-value"));
    }
}
