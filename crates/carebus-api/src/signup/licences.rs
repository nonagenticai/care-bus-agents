//! Licence issuing for the approve and for licence on login (agent `licenses_admin.rs`), as the client-credentials
//! SA `care-bus-licenses-admin` (secure-apps; owner `care-bus`, licences 068). Against licences origin/main:
//! - `PUT /tenants/{id}` `{name}`: register the care-bus tenant under the caller's owner (idempotent; 200/201). The
//!   answer is the tenant row with its `owner_key`: an id already registered to another app that granted care-bus
//!   access answers 200 with THAT owner (unchanged), one that did not answers 409 `tenant_owned_by_other_app`.
//! - `POST /internal/tenants/{id}/bundles/{bundle}/ensure` `{plans}`: the tenant licence set (bundle `care-bus`; its
//!   `app_requires` deps — mcp, strata-rag, strata-ingest — are licences' business, migration 059).
//! - `POST /licenses/user/{sub}` (`X-Tenant-Id`) `{app_id, plan_id, subject_realm}`: the member licence; licences
//!   returns the active one when it exists (idempotent per tenant, app, subject).
//! - `GET /licenses/active?tenant_id&app_key&subject_id=svc:{tenant}:{app}`: the tenant's bundle licence (held by the
//!   service subject; `subject_match` true), for offboarding.
//! - `POST /license/revoke` (`X-Tenant-Id`) `{license_id}`: reject repair / offboarding. Its
//!   `released_dependency_ids` are the dependency licences (mcp, strata-rag, strata-ingest) licences released because
//!   no active care-bus licence on the tenant requires them any more (licences #292 cascade, pinned by #303).
//!
//! - `GET /licenses` (`X-Tenant-Id`): every licence of the tenant, for removing an approved signup.
//! - `PATCH /tenants/{id}/archive` `{archived: true}`: retire the tenant in licences (removal, last licences call).
//!
//! Bundle key, app key, plan and subject realm come from configuration (crate::signup::config).
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::signup::config::LicencesIssueConfig;
use crate::signup::upstream::{ServiceToken, UpstreamError, encode};

#[derive(Deserialize)]
struct WireId {
    id: Option<String>,
}

#[derive(Deserialize)]
struct WireTenant {
    owner_key: Option<String>,
}

/// Who owns a tenant in licences, as `PUT /tenants/{id}` answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantOwner {
    /// Care-bus (`LICENSE_OWNER_KEY`) owns it: care-bus ensures its licence set.
    Caller,
    /// Another app owns it and granted care-bus access (200, the row unchanged). Only the owner can ensure its
    /// licence set (`tenant_owner_only`).
    Granted(String),
    /// Another app owns it and granted care-bus nothing (409 `tenant_owned_by_other_app`).
    Other,
}

#[derive(Deserialize)]
struct WirePlan {
    id: Option<String>,
    code: Option<String>,
}

#[derive(Deserialize)]
struct WireLicense {
    license_id: Option<String>,
    id: Option<String>,
}

/// The licences registry name of a tenant (`^[a-z0-9][a-z0-9-]*$`, unique per owner): the name as a slug (at most 40
/// characters) plus the last 8 hex digits of its id.
pub fn registry_name(name: &str, id: Uuid) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    let hex = id.simple().to_string();
    let tail = &hex[hex.len() - 8..];
    if slug.is_empty() {
        format!("t-{tail}")
    } else {
        format!("{slug}-{tail}")
    }
}

pub struct LicencesAdmin {
    pub cfg: LicencesIssueConfig,
    token: ServiceToken,
    http: reqwest::Client,
    /// (app id, plan id), resolved once.
    ids: Mutex<Option<(String, String)>>,
}

impl LicencesAdmin {
    pub fn new(cfg: &LicencesIssueConfig, http: reqwest::Client) -> Self {
        Self {
            cfg: cfg.clone(),
            token: ServiceToken::new(&cfg.creds, http.clone()),
            http,
            ids: Mutex::new(None),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.cfg.url.trim_end_matches('/'))
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        tenant: Option<Uuid>,
        what: &str,
    ) -> Result<reqwest::Response, UpstreamError> {
        let token = self.token.get().await?;
        let mut req = req.bearer_auth(token);
        if let Some(t) = tenant {
            req = req.header("x-tenant-id", t.to_string());
        }
        req.send()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))
    }

    /// A non-2xx answer as an error carrying licences' own `detail.error` code when it gives one.
    async fn refused(resp: reqwest::Response, what: &str) -> UpstreamError {
        let status = resp.status();
        let code = resp.json::<serde_json::Value>().await.ok().and_then(|b| {
            b.pointer("/detail/error")
                .or_else(|| b.get("error"))
                .and_then(|v| v.as_str())
                .filter(|c| {
                    c.len() <= 64 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
                .map(str::to_string)
        });
        match code {
            Some(c) => UpstreamError::new(
                Some(status.as_u16()),
                format!("{what} failed: HTTP {status}: {c}"),
            ),
            None => UpstreamError::status(what, status),
        }
    }

    async fn app_and_plan(&self) -> Result<(String, String), UpstreamError> {
        let mut ids = self.ids.lock().await;
        if let Some(hit) = ids.as_ref() {
            return Ok(hit.clone());
        }
        let key = encode(&self.cfg.app_key);
        let what = "licences resolve app";
        let resp = self
            .send(self.http.get(self.url(&format!("apps/{key}"))), None, what)
            .await?;
        if !resp.status().is_success() {
            return Err(Self::refused(resp, what).await);
        }
        let app: WireId = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let app_id = app
            .id
            .ok_or_else(|| UpstreamError::new(None, "licences resolve app: no id"))?;
        let what = "licences resolve plans";
        let resp = self
            .send(
                self.http.get(self.url(&format!("apps/{key}/plans"))),
                None,
                what,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(Self::refused(resp, what).await);
        }
        let plans: Vec<WirePlan> = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let plan_id = plans
            .into_iter()
            .find(|p| p.code.as_deref() == Some(self.cfg.plan_code.as_str()))
            .and_then(|p| p.id)
            .ok_or_else(|| {
                UpstreamError::new(
                    None,
                    format!(
                        "plan {:?} not found for app {:?}",
                        self.cfg.plan_code, self.cfg.app_key
                    ),
                )
            })?;
        *ids = Some((app_id.clone(), plan_id.clone()));
        Ok((app_id, plan_id))
    }

    /// Register `tenant` under its registry name (idempotent PUT); who owns it. A 201 is a tenant created under the
    /// caller; a 200 names its `owner_key` (an answer without one is read as the caller's, the ensure then decides).
    pub async fn register_tenant(
        &self,
        tenant: Uuid,
        name: &str,
    ) -> Result<TenantOwner, UpstreamError> {
        let what = "licences register tenant";
        let resp = self
            .send(
                self.http
                    .put(self.url(&format!("tenants/{tenant}")))
                    .json(&json!({"name": registry_name(name, tenant)})),
                None,
                what,
            )
            .await?;
        if !resp.status().is_success() {
            let err = Self::refused(resp, what).await;
            if err.status == Some(409) && err.message.ends_with(": tenant_owned_by_other_app") {
                return Ok(TenantOwner::Other);
            }
            return Err(err);
        }
        if resp.status() == reqwest::StatusCode::CREATED {
            return Ok(TenantOwner::Caller);
        }
        let owner = resp
            .json::<WireTenant>()
            .await
            .ok()
            .and_then(|t| t.owner_key)
            .filter(|o| !o.is_empty());
        Ok(match owner {
            Some(o) if o != self.cfg.owner_key => {
                TenantOwner::Granted(o.chars().take(64).collect())
            }
            _ => TenantOwner::Caller,
        })
    }

    /// Ensure the tenant licence set of `LICENSE_BUNDLE_KEY` (idempotent: exists / adopt / insert per app).
    pub async fn ensure_bundle(&self, tenant: Uuid) -> Result<(), UpstreamError> {
        let what = "licences ensure bundle";
        let resp = self
            .send(
                self.http
                    .post(self.url(&format!(
                        "internal/tenants/{tenant}/bundles/{}/ensure",
                        encode(&self.cfg.bundle_key)
                    )))
                    .json(&json!({"plans": self.cfg.bundle_plans})),
                None,
                what,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(Self::refused(resp, what).await);
        }
        // `{tenant_id, bundle, apps: {key: {decision, license_id, ...}}, refused: [...]}`: an app licences refused to
        // ensure (e.g. a dependency it could not license) leaves the set incomplete.
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        if body["refused"].as_array().is_some_and(|r| !r.is_empty()) {
            return Err(UpstreamError::new(
                Some(409),
                format!(
                    "{what}: licences refused part of the bundle {}",
                    self.cfg.bundle_key
                ),
            ));
        }
        Ok(())
    }

    /// The member licence of `sub` on `tenant` (licences returns the active one when it exists). Its id.
    pub async fn member_licence(&self, tenant: Uuid, sub: &str) -> Result<String, UpstreamError> {
        let (app_id, plan_id) = self.app_and_plan().await?;
        let what = "licences create member licence";
        let resp = self
            .send(
                self.http
                    .post(self.url(&format!("licenses/user/{}", encode(sub))))
                    .json(&json!({
                        "app_id": app_id,
                        "plan_id": plan_id,
                        "subject_realm": self.cfg.subject_realm,
                    })),
                Some(tenant),
                what,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(Self::refused(resp, what).await);
        }
        let l: WireLicense = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        l.license_id
            .or(l.id)
            .filter(|i| !i.is_empty() && i.len() <= 64)
            .ok_or_else(|| {
                UpstreamError::new(None, "licences create member licence: no license_id")
            })
    }

    /// The tenant's bundle licence (the `app_key` licence its service subject `svc:{tenant}:{app_key}` holds), or
    /// `None` when it holds none (404 `no_active_license`, or the answer is not that subject's licence).
    pub async fn bundle_licence(&self, tenant: Uuid) -> Result<Option<String>, UpstreamError> {
        let what = "licences read bundle licence";
        let subject = format!("svc:{tenant}:{}", self.cfg.app_key);
        let url = format!(
            "{}?tenant_id={tenant}&app_key={}&subject_id={}",
            self.url("licenses/active"),
            encode(&self.cfg.app_key),
            encode(&subject)
        );
        let resp = self.send(self.http.get(url), None, what).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            let code = body
                .pointer("/detail/error")
                .or_else(|| body.get("error"))
                .and_then(|v| v.as_str());
            if code == Some("no_active_license") {
                return Ok(None);
            }
            return Err(UpstreamError::new(
                Some(404),
                format!("{what} failed: HTTP 404"),
            ));
        }
        if !resp.status().is_success() {
            return Err(Self::refused(resp, what).await);
        }
        let l: crate::licence::ActiveLicence = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        let ours = l.active
            && l.subject_match == Some(true)
            && l.app.as_ref().is_some_and(|a| a.key == self.cfg.app_key)
            && l.tenant_id
                .as_deref()
                .and_then(|t| Uuid::parse_str(t.trim()).ok())
                == Some(tenant);
        Ok(l.license_id
            .filter(|i| ours && !i.is_empty() && i.len() <= 64))
    }

    /// Revoke `license_id` on `tenant`. `Some(released)` only when licences says it revoked it (200 `{revoked: true}`,
    /// with the dependency licences it released, `released_dependency_ids`); `None` when it did not: 200
    /// `{revoked: false, message: "License not found"}` (unknown, another tenant's, or already revoked) or a 404.
    /// Callers that only need "no longer live" treat `None` as done; the removal confirms by a fresh listing.
    pub async fn revoke(
        &self,
        tenant: Uuid,
        license_id: &str,
    ) -> Result<Option<Vec<String>>, UpstreamError> {
        let what = "licences revoke";
        let resp = self
            .send(
                self.http
                    .post(self.url("license/revoke"))
                    .json(&json!({"license_id": license_id})),
                Some(tenant),
                what,
            )
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(Self::refused(resp, what).await);
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))?;
        if body["revoked"].as_bool() != Some(true) {
            return Ok(None);
        }
        Ok(Some(
            body["released_dependency_ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .filter(|i| !i.is_empty() && i.len() <= 64)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        ))
    }

    /// Every licence of `tenant` (`GET /licenses`, `X-Tenant-Id`; all apps, all holders, revoked ones included).
    /// Needs `license-admin` or `license-reader`. Removal (crate::signup::removal).
    pub async fn tenant_licences(&self, tenant: Uuid) -> Result<Vec<TenantLicence>, UpstreamError> {
        let what = "licences list tenant licences";
        let resp = self
            .send(self.http.get(self.url("licenses")), Some(tenant), what)
            .await?;
        if !resp.status().is_success() {
            return Err(Self::refused(resp, what).await);
        }
        resp.json()
            .await
            .map_err(|e| UpstreamError::transport(what, &e))
    }

    /// Retire `tenant` in licences (`PATCH /tenants/{id}/archive` `{"archived": true}`; idempotent: re-archiving
    /// keeps the first timestamp). An archived tenant's licences stop authorizing and no new one can be minted.
    /// `false` when licences does not know the tenant (404: it was never registered). Owner only (a grantee gets 403
    /// `tenant_owner_only`).
    pub async fn archive_tenant(&self, tenant: Uuid) -> Result<bool, UpstreamError> {
        let what = "licences archive tenant";
        let resp = self
            .send(
                self.http
                    .patch(self.url(&format!("tenants/{tenant}/archive")))
                    .json(&json!({"archived": true})),
                None,
                what,
            )
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !resp.status().is_success() {
            return Err(Self::refused(resp, what).await);
        }
        Ok(true)
    }
}

/// One `GET /licenses` row: only what the removal needs.
#[derive(Debug, Clone, Deserialize)]
pub struct TenantLicence {
    pub id: String,
    pub tenant_id: Option<String>,
    pub status: Option<String>,
    pub revoked_at: Option<String>,
    /// licences migration 059: a shared DEPENDENCY licence (mcp, strata-rag, strata-ingest), released by licences
    /// when the last licence requiring it goes; not care-bus's to revoke while one does.
    #[serde(default)]
    pub is_dependency: bool,
    /// A licence a bundle ADOPTED carries its bundle id and is the owner's to revoke.
    pub bundle_id: Option<String>,
}

impl TenantLicence {
    pub fn live(&self) -> bool {
        self.revoked_at.is_none() && self.status.as_deref() != Some("revoked")
    }

    pub fn dependency(&self) -> bool {
        self.bundle_id.is_none() && self.is_dependency
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_slugs_with_the_id_tail() {
        let id = Uuid::parse_str("01a00000-0000-7000-8000-0000deadbeef").unwrap();
        assert_eq!(
            registry_name("Globex Clinics B.V.", id),
            "globex-clinics-b-v-deadbeef"
        );
        assert_eq!(registry_name("  ", id), "t-deadbeef");
        assert!(registry_name(&"x".repeat(90), id).len() <= 49);
    }
}
