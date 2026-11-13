//! Per-tenant MCP catalog bindings: care-bus as a BINDING caller of nonagenticai_mcp_server
//! (docs/AUTHENTICATION_BINDINGS.md there; migration 0030 here).
//!
//! The care-bus licence (crate::mcp_credentials) reaches the server's builtin tools only. Catalog tools, and with them
//! MCP tasks (fr projects, n8n workflows; crate::mcp_tasks), answer only a caller that sends
//! - `X-Tenant-Id: Bearer <licence>`: the tenant's TENANT-LEVEL `mcp` licence, minted by licences
//!   `bundle-token?app=mcp&bundle=care-bus` (care-bus requires mcp, licences 078), and
//! - `X-MCP-Binding: <token>`: the tenant's binding for product care-bus.
//!
//! The binding is provisioned with `POST {MCP_INTERNAL_URL}/mcp/internal/tenants/{tenant}/bindings/ensure`
//! `{product, licence_id, licence_token, rotate?}` as the licences minter's own service account
//! (`care-bus-bundle-minter`, realm secure-apps, audience `nonagenticai-mcp-internal`; listed in the server's
//! MCP_INTERNAL_KEYCLOAK_ALLOWED_CLIENTS and MCP_BINDING_ENSURE_CLIENTS). The server returns the token once: it is kept
//! sealed in `mcp_bindings`. A re-ensure answers `token_already_issued` without a token; a token care-bus no longer
//! holds (or one pinned to a replaced licence, or one refused twice within ten minutes) is rotated. Provisioning is
//! serialised per tenant with an advisory lock, so two replicas never rotate each other's fresh token.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use carebus::mcp_http::{FnSource, McpCredentials, TenantCredentials};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::crypto::{SourceKey, Unreadable};
use crate::error::{ApiError, ApiResult};
use crate::mcp_credentials::CARE_BUS_BUNDLE;
use crate::strata_tokens::{StrataApp, StrataTokens};
use crate::tenancy::begin_scoped;

/// The product care-bus provisions bindings for (the server maps it to toolkits and to licences bundle care-bus).
pub const PRODUCT: &str = "care-bus";
/// The licences app of the gateway licence a binding caller sends.
pub const MCP_APP: &str = "mcp";
/// Refusals of a tenant's binding credentials within this window...
const REFUSAL_WINDOW: Duration = Duration::from_secs(600);
/// ...that rotate its binding (a fresh licence alone did not help: the binding itself is gone).
const REFUSALS_BEFORE_ROTATE: usize = 2;
const UPSTREAM: &str = "mcp";

/// One tenant's binding-caller credentials. Never logged.
pub struct BindingCredentials {
    pub licence_token: String,
    pub binding_token: String,
    pub strata_token: String,
    /// When the earlier of the licence and the strata token leaves the cache.
    pub until: Instant,
}

impl BindingCredentials {
    pub fn expires_in(&self) -> Duration {
        self.until.saturating_duration_since(Instant::now())
    }
}

/// `{MCP_INTERNAL_URL}` when set, else the origin of `MCP_URL` (its `/mcp` path removed).
pub fn internal_base(mcp_url: &str, internal: Option<&str>) -> String {
    let base = internal.unwrap_or(mcp_url).trim().trim_end_matches('/');
    base.strip_suffix("/mcp").unwrap_or(base).to_string()
}

pub struct Bindings {
    tokens: Arc<StrataTokens>,
    http: reqwest::Client,
    base: String,
    key: SourceKey,
    db: PgPool,
    refusals: Mutex<HashMap<Uuid, Vec<Instant>>>,
}

/// What `bindings/ensure` answered.
#[derive(Deserialize)]
struct Ensured {
    binding_id: Uuid,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    token_already_issued: bool,
}

impl Bindings {
    pub fn new(
        tokens: Arc<StrataTokens>,
        http: reqwest::Client,
        base: String,
        key: SourceKey,
        db: PgPool,
    ) -> Self {
        Self {
            tokens,
            http,
            base,
            key,
            db,
            refusals: Mutex::new(HashMap::new()),
        }
    }

    /// `tenant`'s binding-caller credentials. `fresh`: the previous ones were refused, so the licence and the strata
    /// token are minted anew; a second refusal within ten minutes also rotates the binding.
    pub async fn credentials(&self, tenant: Uuid, fresh: bool) -> ApiResult<BindingCredentials> {
        let rag = StrataApp::Rag.key();
        let rotate = fresh && self.refused(tenant);
        if fresh {
            self.tokens.forget(tenant, MCP_APP);
            self.tokens.forget(tenant, rag);
        }
        let (licence_token, licence_id, licence_until) = self
            .tokens
            .licence(MCP_APP, Some(CARE_BUS_BUNDLE), tenant)
            .await?;
        let Some(licence_id) = licence_id else {
            tracing::error!(%tenant, "mcp_binding: licences minted an mcp licence without a license_id");
            return Err(ApiError::Upstream("licences"));
        };
        let (strata_token, strata_until) = self.tokens.minted(rag, None, tenant).await?;
        let binding_token = self
            .binding(tenant, licence_id, &licence_token, rotate)
            .await?;
        let now = Instant::now();
        Ok(BindingCredentials {
            licence_token,
            binding_token,
            strata_token,
            until: licence_until
                .unwrap_or(now)
                .min(strata_until.unwrap_or(now)),
        })
    }

    /// Records a refusal; true when this one makes `REFUSALS_BEFORE_ROTATE` within the window (and resets the count).
    fn refused(&self, tenant: Uuid) -> bool {
        let mut g = self.refusals.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let seen = g.entry(tenant).or_default();
        seen.retain(|t| now.duration_since(*t) < REFUSAL_WINDOW);
        seen.push(now);
        if seen.len() >= REFUSALS_BEFORE_ROTATE {
            seen.clear();
            true
        } else {
            false
        }
    }

    /// The stored token, or a newly provisioned one. Serialised per tenant (advisory lock held while ensuring).
    async fn binding(
        &self,
        tenant: Uuid,
        licence_id: Uuid,
        licence: &str,
        rotate: bool,
    ) -> ApiResult<String> {
        let mut tx = begin_scoped(&self.db, tenant, None).await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("mcp_binding:{tenant}:{PRODUCT}"))
            .execute(&mut *tx)
            .await?;
        let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
            "SELECT binding_id, licence_id, token_enc FROM mcp_bindings WHERE product = $1",
        )
        .bind(PRODUCT)
        .fetch_optional(&mut *tx)
        .await?;
        let mut rotate = rotate;
        if let Some((binding_id, stored_licence, enc)) = &row
            && !rotate
        {
            if *stored_licence == licence_id {
                match open(&self.key, tenant, *binding_id, enc) {
                    Ok(token) => {
                        tx.commit().await?;
                        return Ok(token);
                    }
                    Err(Unreadable) => tracing::warn!(
                        %tenant,
                        "mcp_binding: stored token unreadable (CAREBUS_SOURCE_KEY changed?); rotating"
                    ),
                }
            }
            // A token pinned to another licence, or one this key cannot open: it is replaced.
            rotate = true;
        }
        let (binding_id, token) = match self.ensure(tenant, licence_id, licence, rotate).await? {
            Some(t) => t,
            // Provisioned before, but care-bus holds no token for it: rotate to get one.
            None => self
                .ensure(tenant, licence_id, licence, true)
                .await?
                .ok_or_else(|| {
                    tracing::error!(%tenant, "mcp_binding: a rotation returned no token");
                    ApiError::Upstream(UPSTREAM)
                })?,
        };
        sqlx::query(
            "INSERT INTO mcp_bindings (tenant_id, product, binding_id, licence_id, token_enc)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (tenant_id, product) DO UPDATE
                SET binding_id = EXCLUDED.binding_id, licence_id = EXCLUDED.licence_id,
                    token_enc = EXCLUDED.token_enc, updated_at = now()",
        )
        .bind(tenant)
        .bind(PRODUCT)
        .bind(binding_id)
        .bind(licence_id)
        .bind(seal(&self.key, tenant, binding_id, &token))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(%tenant, %binding_id, rotated = rotate, "mcp_binding_provisioned");
        Ok(token)
    }

    /// One `bindings/ensure`. `Some((binding, token))` when the server issued a token, `None` when it says one was
    /// already issued. A 401 is retried once with a fresh service-account token.
    async fn ensure(
        &self,
        tenant: Uuid,
        licence_id: Uuid,
        licence: &str,
        rotate: bool,
    ) -> ApiResult<Option<(Uuid, String)>> {
        let url = format!(
            "{}/mcp/internal/tenants/{tenant}/bindings/ensure",
            self.base
        );
        let body = json!({"product": PRODUCT, "licence_id": licence_id, "licence_token": licence, "rotate": rotate});
        let mut fresh = false;
        loop {
            let bearer = self.tokens.minter_bearer(fresh).await?;
            let resp = self
                .http
                .post(&url)
                .bearer_auth(bearer)
                .json(&body)
                .send()
                .await
                .map_err(|_| {
                    tracing::warn!(%tenant, "mcp_binding: MCP server unreachable");
                    ApiError::Upstream(UPSTREAM)
                })?;
            let status = resp.status().as_u16();
            if status == 401 && !fresh {
                fresh = true;
                continue;
            }
            if !(200..300).contains(&status) {
                let detail: serde_json::Value =
                    resp.json().await.unwrap_or(serde_json::Value::Null);
                return Err(refusal(status, &detail, tenant));
            }
            let e: Ensured = resp.json().await.map_err(|_| {
                tracing::warn!(%tenant, "mcp_binding: unreadable bindings/ensure reply");
                ApiError::Upstream(UPSTREAM)
            })?;
            return Ok(match e.token.filter(|t| !t.is_empty()) {
                Some(t) => Some((e.binding_id, t)),
                None if e.token_already_issued => None,
                None => {
                    tracing::error!(%tenant, "mcp_binding: bindings/ensure returned neither a token nor token_already_issued");
                    return Err(ApiError::Upstream(UPSTREAM));
                }
            });
        }
    }
}

/// A refused `bindings/ensure` as an API error: 403 = the server does not accept the minter or the licence, 404 = it
/// does not know product care-bus (both configuration, 503); anything else is the upstream failing.
fn refusal(status: u16, detail: &serde_json::Value, tenant: Uuid) -> ApiError {
    tracing::warn!(%tenant, status, detail = %detail, "mcp_binding: bindings/ensure refused");
    match status {
        401 | 403 => ApiError::Unavailable(
            "mcp_binding_refused",
            format!(
                "The MCP server refused to provision the tenant's binding (HTTP {status}): it must accept \
                 care-bus-bundle-minter (MCP_BINDING_ENSURE_CLIENTS) and the tenant needs an active tenant-level \
                 mcp licence."
            ),
        ),
        404 => ApiError::Unavailable(
            "mcp_binding_product_unknown",
            "The MCP server does not provision product care-bus (MCP_BINDING_PRODUCTS).".into(),
        ),
        _ => ApiError::Upstream(UPSTREAM),
    }
}

fn aad(tenant: Uuid, binding: Uuid) -> String {
    format!("mcp_binding:{tenant}:{PRODUCT}:{binding}")
}

/// base64(nonce || ciphertext) of `token`, bound to the tenant, product and binding.
pub fn seal(key: &SourceKey, tenant: Uuid, binding: Uuid, token: &str) -> String {
    let s = key.seal(aad(tenant, binding).as_bytes(), token.as_bytes());
    let mut out = s.nonce;
    out.extend_from_slice(&s.ciphertext);
    STANDARD.encode(out)
}

/// The token [`seal`] sealed for the same tenant and binding.
pub fn open(key: &SourceKey, tenant: Uuid, binding: Uuid, enc: &str) -> Result<String, Unreadable> {
    let raw = STANDARD.decode(enc).map_err(|_| Unreadable)?;
    if raw.len() < 12 {
        return Err(Unreadable);
    }
    let (nonce, ciphertext) = raw.split_at(12);
    let plain = key.open(aad(tenant, binding).as_bytes(), nonce, ciphertext)?;
    String::from_utf8(plain).map_err(|_| Unreadable)
}

/// carebus-api's own binding-caller credentials (the MCP task routes), provisioned in-process.
pub fn credentials(bindings: Arc<Bindings>) -> TenantCredentials {
    TenantCredentials::new(FnSource(move |tenant: Uuid, fresh: bool| {
        let bindings = bindings.clone();
        async move {
            match bindings.credentials(tenant, fresh).await {
                Ok(c) => McpCredentials::new(&c.licence_token, &c.strata_token, c.until)
                    .and_then(|m| m.with_binding(&c.binding_token)),
                Err(e) => Err(anyhow::anyhow!("{}: {e}", e.code())),
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_internal_base_is_the_mcp_origin_unless_set() {
        assert_eq!(
            internal_base("http://mcp.example:8033/mcp", None),
            "http://mcp.example:8033"
        );
        assert_eq!(internal_base("http://m:8033/mcp/", None), "http://m:8033");
        assert_eq!(
            internal_base("http://m:8033/mcp", Some("http://override.example:9/")),
            "http://override.example:9"
        );
    }

    #[test]
    fn a_sealed_token_opens_only_for_its_own_binding() {
        let key = SourceKey::from_base64(&STANDARD.encode([7u8; 32])).unwrap();
        let (t, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let enc = seal(&key, t, b, "secret-binding");
        assert_eq!(open(&key, t, b, &enc).unwrap(), "secret-binding");
        assert!(open(&key, t, Uuid::from_u128(3), &enc).is_err());
        assert!(open(&key, Uuid::from_u128(9), b, &enc).is_err());
        assert!(!enc.contains("secret"));
    }
}
