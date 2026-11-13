use std::sync::Arc;

use anyhow::{Context, Result};
use carebus::mcp_http::HttpMcp;
use carebus::{Bus, Scope};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::auth::Verifier;
use crate::chat_q::{AnyQualifier, PlatformLlm, TenantQualifiers};
use crate::config::Config;
use crate::licence::LicenceGate;

#[derive(Clone)]
pub struct AppState {
    pub bus: Bus,
    /// The runtime pool, connected as `carebus_app` (`DATABASE_APP_URL`): every query is under row-level security.
    /// Handlers reach tenant rows only through `TenantCtx::begin` / `JobCtx::begin` (crate::tenancy).
    pub db: PgPool,
    pub http: reqwest::Client,
    /// Admin reachability probes (LLM provider and connector `/test`, overview): never follows a
    /// redirect, so a 3xx is reported as such instead of whatever the redirect target answers.
    pub probe: reqwest::Client,
    pub cfg: Arc<Config>,
    pub auth: Arc<Verifier>,
    /// Built once at startup, calling as each request's tenant (crate::mcp_credentials); `None` when `MCP_URL` is unset
    /// or no licences minter is configured (`/api/tools` is then 503).
    pub mcp: Option<Arc<HttpMcp>>,
    /// The same server as a BINDING caller (crate::mcp_binding): catalog tools and MCP tasks (answer, cancel). `None`
    /// without `MCP_URL`, the licences minter or `CAREBUS_SOURCE_KEY` (the binding token is stored sealed).
    pub mcp_catalog: Option<Arc<HttpMcp>>,
    /// Binding provisioning behind `mcp_catalog` and `POST /api/internal/mcp-credentials {kind: binding}`.
    pub bindings: Option<Arc<crate::mcp_binding::Bindings>>,
    /// The platform default chat qualifier (a tenant without a `qualify` binding): the rig extractor when
    /// `VLLM_LICENSE_TOKEN` is set, keyword rules otherwise. Resolved per turn by `chat_q::qualifier_for`.
    pub qualifier: Arc<AnyQualifier>,
    /// What the platform default runs on (`GET /api/providers` `platform_default`).
    pub platform_llm: Arc<PlatformLlm>,
    /// Qualifiers built from tenants' `qualify` bindings.
    pub qualifiers: Arc<TenantQualifiers>,
    /// Licence gate 2; a no-op unless `LICENSE_GATE_2_ENABLED=true`.
    pub licence: Arc<LicenceGate>,
    /// Early-access signup and licence on login (crate::signup); its routes answer 503 while it is off.
    pub signup: Arc<crate::signup::Signup>,
    /// The strata bearer per call (crate::strata_tokens): static and pinned, or minted per tenant.
    pub strata_tokens: Arc<crate::strata_tokens::StrataTokens>,
}

pub async fn build(cfg: Config) -> Result<AppState> {
    // Migrations run as the database owner over one connection, which is closed before anything is served.
    let owner = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&cfg.database_url)
        .await
        .context("connect DATABASE_URL (migration owner)")?;
    sqlx::migrate!("./migrations").run(&owner).await?;
    owner.close().await;
    let db = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&cfg.database_app_url)
        .await
        .context("connect DATABASE_APP_URL (runtime role)")?;
    refuse_rls_bypass(&db).await?;
    let bus = Bus::connect(&cfg.redis_url, &cfg.prefix).await?;
    reconcile_scopes(&db, &bus).await?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let probe = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let auth = Arc::new(Verifier::jwks_many(
        &cfg.keycloak_trusted_issuers,
        &cfg.keycloak_service_only_realms,
        &cfg.keycloak_audience,
        http.clone(),
    ));
    // The signup's Keycloak / licences / Telegram calls get a longer timeout than the shared client: Keycloak sends
    // the set-password email synchronously, and an answer that never comes is "outcome unknown", never re-sent.
    let signup_http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let signup = Arc::new(crate::signup::Signup::new(cfg.signup.clone(), signup_http));
    let licence = Arc::new(LicenceGate::new(cfg.licence.clone(), http.clone()));
    let strata_tokens = Arc::new(crate::strata_tokens::StrataTokens::new(
        cfg.strata_bundle.as_ref(),
        http.clone(),
    ));
    // MCP is called as each tenant with its own minted licences (crate::mcp_credentials): no minter, no MCP.
    let mcp = match cfg.mcp_url.clone() {
        None => None,
        Some(url) if strata_tokens.minting() => Some(Arc::new(HttpMcp::new(
            url,
            crate::mcp_credentials::credentials(strata_tokens.clone()),
        ))),
        Some(_) => {
            tracing::warn!(
                "MCP_URL is set but CAREBUS_STRATA_BUNDLE_TOKENS is off: MCP calls need per-tenant licences minted by \
                 carebus-api; /api/tools and the MCP credentials route are unavailable"
            );
            None
        }
    };
    // The binding caller: same server, the tenant's mcp licence plus its provisioned binding (crate::mcp_binding).
    let bindings = match (cfg.mcp_url.as_deref(), cfg.source_key.clone()) {
        (Some(url), Some(key)) if strata_tokens.minting() => {
            Some(Arc::new(crate::mcp_binding::Bindings::new(
                strata_tokens.clone(),
                http.clone(),
                crate::mcp_binding::internal_base(url, cfg.mcp_internal_url.as_deref()),
                key,
                db.clone(),
            )))
        }
        (Some(_), None) if strata_tokens.minting() => {
            tracing::warn!(
                "CAREBUS_SOURCE_KEY is not set: MCP binding tokens cannot be stored, so MCP tasks (catalog tools) are \
                 unavailable"
            );
            None
        }
        _ => None,
    };
    let mcp_catalog = match (cfg.mcp_url.clone(), bindings.clone()) {
        (Some(url), Some(b)) => Some(Arc::new(HttpMcp::new(
            url,
            crate::mcp_binding::credentials(b),
        ))),
        _ => None,
    };
    Ok(AppState {
        bus,
        db,
        http,
        probe,
        auth,
        mcp,
        mcp_catalog,
        bindings,
        qualifier: Arc::new(AnyQualifier::from_env()?),
        platform_llm: Arc::new(PlatformLlm::from_env()),
        qualifiers: Arc::new(TenantQualifiers::default()),
        licence,
        signup,
        strata_tokens,
        cfg: Arc::new(cfg),
    })
}

/// RLS is silently off for a superuser, a BYPASSRLS role and the owner of a table (no FORCE ROW LEVEL SECURITY),
/// so the runtime pool must connect as none of them.
async fn refuse_rls_bypass(db: &PgPool) -> Result<()> {
    let bypass: bool = sqlx::query_scalar(
        "SELECT r.rolsuper OR r.rolbypassrls OR EXISTS (
                    SELECT 1 FROM pg_tables t WHERE t.schemaname = 'public' AND t.tableowner = current_user)
           FROM pg_roles r WHERE r.rolname = current_user",
    )
    .fetch_one(db)
    .await
    .context("read the runtime role's attributes")?;
    if bypass {
        anyhow::bail!(
            "DATABASE_APP_URL connects as a role that bypasses RLS (superuser, BYPASSRLS or table owner); \
             it must be carebus_app"
        );
    }
    Ok(())
}

/// Rebuilds the Redis scope registry from the database (every job of every active tenant), so the agents serve
/// exactly the provisioned scopes even after Redis was flushed or a registry write after a provisioning commit
/// failed. Provisioning routes keep it current between starts (`scopes::add` / `remove` after COMMIT).
async fn reconcile_scopes(db: &PgPool, bus: &Bus) -> Result<()> {
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT tenant_id, job_key FROM carebus_all_scopes()")
            .fetch_all(db)
            .await
            .context("read the provisioned scopes (carebus_all_scopes)")?;
    let all: Vec<Scope> = rows
        .into_iter()
        .map(|(tenant, job)| Scope { tenant, job })
        .collect();
    carebus::scopes::replace_all(bus, &all)
        .await
        .context("rebuild the scope registry")?;
    tracing::info!(
        scopes = all.len(),
        "scope registry rebuilt from the jobs table"
    );
    Ok(())
}

/// Same as `build`, but tokens are verified against a static HS256 key instead of Keycloak's JWKS.
/// `main` never calls this.
pub async fn build_for_tests(cfg: Config) -> Result<AppState> {
    let mut st = build(cfg).await?;
    st.qualifier = Arc::new(AnyQualifier::Rules(carebus::interaction::RuleQualifier)); // never the live LLM
    st.platform_llm = Arc::new(PlatformLlm::default());
    st.mcp = None; // tests inject a stub explicitly, never the developer's env
    st.mcp_catalog = None;
    st.bindings = None;
    st.auth = Arc::new(Verifier::test_key_many(
        &st.cfg.keycloak_trusted_issuers,
        &st.cfg.keycloak_service_only_realms,
        &st.cfg.keycloak_audience,
        b"carebus-api-test-secret",
    ));
    Ok(st)
}
