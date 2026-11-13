//! Per-tenant MCP credentials (nonagenticai_mcp_server `src/middleware/auth.py`, `src/tool_scope.py`).
//!
//! Every MCP call care-bus makes for bus tenant T carries T's own credentials, never a shared identity:
//! - `X-Tenant-Id: Bearer <licence>`: T's TENANT-LEVEL care-bus licence (subject `svc:T:care-bus`), minted by licences
//!   `bundle-token?app=care-bus&bundle=care-bus`. A person's per-login care-bus licence carries no bundle, so it is
//!   never the one minted on (and the MCP server gives a per-login licence no tools at all);
//! - `X-Strata-Token: <token>`: T's strata-rag licence token, which the MCP server's `faq_ask` searches strata with.
//!
//! Both are minted by the licences bundle minter only carebus-api holds (crate::strata_tokens, on with
//! `CAREBUS_STRATA_BUNDLE_TOKENS=true`). The agents (tool-executor, researcher) fetch them from
//! `POST /api/internal/mcp-credentials` (routes::mcp_credentials, service token only); carebus-api's own MCP client
//! uses [`credentials`] in-process. Anything that cannot be minted is an error: no MCP call is made for T.
use std::sync::Arc;
use std::time::{Duration, Instant};

use carebus::mcp_http::{FnSource, McpCredentials, TenantCredentials};
use uuid::Uuid;

use crate::error::ApiResult;
use crate::strata_tokens::{StrataApp, StrataTokens};

/// The licences app and bundle the MCP licence is minted on.
pub const CARE_BUS_APP: &str = "care-bus";
pub const CARE_BUS_BUNDLE: &str = "care-bus";

/// One tenant's minted MCP credentials. Never logged.
pub struct Minted {
    pub licence_token: String,
    pub strata_token: String,
    /// When the earlier of the two leaves carebus-api's cache (shortly before it expires); now when unknown.
    pub until: Instant,
}

impl Minted {
    /// How long the pair may still be used.
    pub fn expires_in(&self) -> Duration {
        self.until.saturating_duration_since(Instant::now())
    }
}

/// `tenant`'s MCP credentials. `fresh`: the previous ones were refused, so both are minted anew rather than taken
/// from the cache. Minting off: 503 `mcp_credentials_unconfigured`; a tenant without the licence: 409
/// `not_licensed_for_tenant` / `strata_not_licensed_for_tenant`; licences refusing the minter: 503.
pub async fn mint(tokens: &StrataTokens, tenant: Uuid, fresh: bool) -> ApiResult<Minted> {
    let rag = StrataApp::Rag.key();
    if fresh {
        tokens.forget(tenant, CARE_BUS_APP);
        tokens.forget(tenant, rag);
    }
    let (licence_token, licence_until) = tokens
        .minted(CARE_BUS_APP, Some(CARE_BUS_BUNDLE), tenant)
        .await?;
    let (strata_token, strata_until) = tokens.minted(rag, None, tenant).await?;
    let now = Instant::now();
    Ok(Minted {
        licence_token,
        strata_token,
        until: licence_until
            .unwrap_or(now)
            .min(strata_until.unwrap_or(now)),
    })
}

/// carebus-api's own per-tenant MCP credentials (`GET /api/jobs/{job}/tools`, the overview probe), minted in-process.
pub fn credentials(tokens: Arc<StrataTokens>) -> TenantCredentials {
    TenantCredentials::new(FnSource(move |tenant: Uuid, fresh: bool| {
        let tokens = tokens.clone();
        async move {
            match mint(&tokens, tenant, fresh).await {
                Ok(m) => McpCredentials::new(&m.licence_token, &m.strata_token, m.until),
                Err(e) => Err(anyhow::anyhow!("{}: {e}", e.code())),
            }
        }
    }))
}
