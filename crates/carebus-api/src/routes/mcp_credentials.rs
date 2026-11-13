//! `POST /api/internal/mcp-credentials {tenant_id, fresh?}` -> `{tenant_id, licence_token, strata_token, expires_at,
//! expires_in}`: one bus tenant's MCP credentials (crate::mcp_credentials), for the tool-executor and the researcher,
//! which hold no licences minter. Service token (`CAREBUS_API_SERVICE_TOKENS`) only, as the faq scope route, and only
//! for an active care-bus tenant (one with a scope in `carebus_all_scopes()`). `fresh: true` (after the MCP server
//! refused the previous pair) mints both anew. The answer is `no-store`; nothing here logs a token.
//!
//! `kind: "binding"` answers the BINDING caller's credentials instead (crate::mcp_binding): `licence_token` is then the
//! tenant's `mcp` licence and `binding_token` its provisioned care-bus binding, for catalog tools and MCP tasks.
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, routing::post};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{Caller, CallerKind};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::mcp_credentials;
use crate::state::AppState;

/// Absolute path; merged by `routes::all`.
pub fn internal_routes() -> Router<AppState> {
    Router::new().route(
        carebus::mcp_http::MCP_CREDENTIALS_PATH,
        post(mcp_credentials_for),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    tenant_id: Uuid,
    #[serde(default)]
    fresh: bool,
    #[serde(default)]
    kind: Kind,
}

/// Which caller the credentials are for.
#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    /// The care-bus licence: the builtin tools.
    #[default]
    Builtin,
    /// The mcp licence plus the tenant's binding: catalog tools and tasks.
    Binding,
}

async fn mcp_credentials_for(
    caller: Caller,
    State(s): State<AppState>,
    ApiJson(i): ApiJson<Input>,
) -> ApiResult<Response> {
    if !matches!(caller.kind, CallerKind::Service) {
        return Err(ApiError::Forbidden("service token"));
    }
    if i.tenant_id.is_nil() {
        return Err(ApiError::BadRequest(
            "invalid_tenant",
            "tenant_id must be a non-nil uuid.".into(),
        ));
    }
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM carebus_all_scopes() WHERE tenant_id = $1)",
    )
    .bind(i.tenant_id)
    .fetch_one(&s.db)
    .await?;
    if !active {
        return Err(ApiError::NotFound("tenant".into()));
    }
    let (body, left) = match i.kind {
        Kind::Builtin => {
            let m = mcp_credentials::mint(&s.strata_tokens, i.tenant_id, i.fresh).await?;
            let left = m.expires_in();
            (
                json!({"tenant_id": i.tenant_id, "licence_token": m.licence_token, "strata_token": m.strata_token}),
                left,
            )
        }
        Kind::Binding => {
            let b = s.bindings.as_ref().ok_or(ApiError::Unavailable(
                "mcp_binding_unconfigured",
                "MCP bindings need MCP_URL, the licences bundle minter and CAREBUS_SOURCE_KEY."
                    .into(),
            ))?;
            let c = b.credentials(i.tenant_id, i.fresh).await?;
            let left = c.expires_in();
            (
                json!({"tenant_id": i.tenant_id, "licence_token": c.licence_token, "strata_token": c.strata_token,
                       "binding_token": c.binding_token}),
                left,
            )
        }
    };
    let expires_at = Utc::now() + chrono::Duration::from_std(left).unwrap_or_default();
    tracing::info!(tenant = %i.tenant_id, fresh = i.fresh, binding = (i.kind == Kind::Binding),
                   expires_in = left.as_secs(), "mcp_credentials_issued");
    let mut body = body;
    body["expires_at"] = json!(expires_at.to_rfc3339());
    body["expires_in"] = json!(left.as_secs());
    let mut resp = Json(body).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}
