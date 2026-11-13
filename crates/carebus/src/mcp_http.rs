//! MCP over streamable HTTP to nonagenticai_mcp_server (`/mcp`), always as ONE bus tenant.
//!
//! Every request carries that tenant's own credentials (nonagenticai_mcp_server `src/middleware/auth.py`,
//! `src/tool_scope.py`): `X-Tenant-Id: Bearer <licence>`, the tenant-level care-bus licence (subject
//! `svc:<tenant>:care-bus`, minted by licences `bundle-token?app=care-bus&bundle=care-bus`), and
//! `X-Strata-Token: <token>`, the tenant's strata-rag licence, which the MCP server's `faq_ask` searches strata with.
//! There is no `Authorization` header and no shared identity: the credentials come per tenant from a
//! [`CredentialSource`] (carebus-api `POST /api/internal/mcp-credentials` for the agents, carebus-api's own minter
//! in-process), are cached per tenant until shortly before they expire, and a tenant whose credentials cannot be
//! fetched gets no call at all (fail closed). Each tenant has its own MCP session, so one tenant's session never
//! carries another tenant's licence.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::model::{ClientConfig, ClientJsonRpcMessage};
use rmcp::service::ClientServiceExt;
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::common::client_side_sse::{BoxedSseResponse, ExponentialBackoff};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::mcp::McpClient;
use crate::tools::ToolError;

fn retryable(message: impl Into<String>) -> ToolError {
    ToolError {
        message: message.into(),
        retryable: true,
    }
}

/// Error code of a tool error caused by the MCP server refusing freshly fetched credentials.
pub const AUTH_REJECTED: &str = "auth_rejected";

/// The server refused credentials fetched just before the request (HTTP `status`): not expiry but a licence or
/// configuration fault. Permanent, so the call fails now instead of being retried until its deadline.
fn auth_rejected(status: u16) -> ToolError {
    ToolError {
        message: format!(
            "{AUTH_REJECTED}: MCP server refused the tenant's freshly fetched licence (HTTP {status}): the tenant \
             needs an active tenant-level care-bus licence (svc:<tenant>:care-bus) and the MCP server must \
             introspect licences (LICENSE_INTROSPECT_URL)"
        ),
        retryable: false,
    }
}

/// Request header carrying the tenant's care-bus licence, as `Bearer <licence>`.
pub const TENANT_HEADER: &str = "x-tenant-id";
/// Request header carrying the tenant's strata-rag licence token.
pub const STRATA_TOKEN_HEADER: &str = "x-strata-token";
/// Request header carrying a catalog binding token (nonagenticai_mcp_server docs/AUTHENTICATION_BINDINGS.md). Catalog
/// tools, and with them MCP tasks, are reachable only by a binding caller: the tenant's `mcp` licence in
/// `X-Tenant-Id` plus this token. The care-bus licence alone reaches the builtin tools only.
pub const BINDING_HEADER: &str = "x-mcp-binding";
/// carebus-api's service-token route the agents fetch credentials from.
pub const MCP_CREDENTIALS_PATH: &str = "/api/internal/mcp-credentials";
/// Longest lifetime a credentials answer is believed for (licences caps bundle tokens at 60 minutes).
const MAX_LIFETIME: Duration = Duration::from_secs(3600);

/// A token that can travel in a header as is: non-empty visible ASCII.
fn header_safe(t: &str) -> bool {
    !t.is_empty() && t.len() <= 8192 && t.bytes().all(|b| b.is_ascii_graphic())
}

/// One tenant's MCP credentials: its care-bus licence and its strata-rag licence token, and when the earlier of the
/// two expires. Never printed.
#[derive(Clone)]
pub struct McpCredentials {
    licence_token: String,
    strata_token: String,
    licence: HeaderValue,
    strata: HeaderValue,
    /// `X-MCP-Binding`, for a binding caller (catalog tools and tasks); `None` for the builtin care-bus caller.
    binding: Option<HeaderValue>,
    /// When the earlier of the two tokens expires.
    pub expires: Instant,
}

impl McpCredentials {
    /// Both tokens must be non-empty visible ASCII (an error otherwise, naming which, never the value).
    pub fn new(licence_token: &str, strata_token: &str, expires: Instant) -> Result<Self> {
        anyhow::ensure!(
            header_safe(licence_token),
            "the care-bus licence token is empty or not header-safe"
        );
        anyhow::ensure!(
            header_safe(strata_token),
            "the strata-rag token is empty or not header-safe"
        );
        let mut licence = HeaderValue::from_str(&format!("Bearer {licence_token}"))
            .context("the care-bus licence token is not header-safe")?;
        licence.set_sensitive(true);
        let mut strata = HeaderValue::from_str(strata_token)
            .context("the strata-rag token is not header-safe")?;
        strata.set_sensitive(true);
        Ok(Self {
            licence_token: licence_token.to_string(),
            strata_token: strata_token.to_string(),
            licence,
            strata,
            binding: None,
            expires,
        })
    }

    /// The same credentials as a binding caller: every request also carries `X-MCP-Binding: <token>`.
    pub fn with_binding(mut self, binding_token: &str) -> Result<Self> {
        anyhow::ensure!(
            header_safe(binding_token),
            "the binding token is empty or not header-safe"
        );
        let mut binding =
            HeaderValue::from_str(binding_token).context("the binding token is not header-safe")?;
        binding.set_sensitive(true);
        self.binding = Some(binding);
        Ok(self)
    }

    pub fn licence_token(&self) -> &str {
        &self.licence_token
    }

    /// The tenant's strata-rag licence token, for callers that search strata themselves as the tenant (the
    /// researcher's `strata_search`). Never log it.
    pub fn strata_token(&self) -> &str {
        &self.strata_token
    }

    /// `X-Tenant-Id: Bearer <licence>` and `X-Strata-Token: <token>`, ready to send.
    pub fn headers(&self) -> [(HeaderName, HeaderValue); 2] {
        [
            (HeaderName::from_static(TENANT_HEADER), self.licence.clone()),
            (
                HeaderName::from_static(STRATA_TOKEN_HEADER),
                self.strata.clone(),
            ),
        ]
    }

    /// `headers` with this tenant's two headers set, and no `Authorization`: these calls carry no shared identity.
    fn apply(
        &self,
        mut headers: HashMap<HeaderName, HeaderValue>,
    ) -> HashMap<HeaderName, HeaderValue> {
        headers.remove(&reqwest::header::AUTHORIZATION);
        headers.extend(self.headers());
        if let Some(b) = &self.binding {
            headers.insert(HeaderName::from_static(BINDING_HEADER), b.clone());
        }
        headers
    }
}

impl std::fmt::Debug for McpCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCredentials")
            .field("licence_token", &"<redacted>")
            .field("strata_token", &"<redacted>")
            .field("binding", &self.binding.as_ref().map(|_| "<redacted>"))
            .field("expires", &self.expires)
            .finish()
    }
}

/// Where a tenant's credentials come from.
pub trait CredentialSource: Send + Sync + 'static {
    /// The credentials of `tenant`. `fresh`: the previous ones were refused, so a cached copy must not be handed
    /// back. An error means no MCP call is made for the tenant.
    fn fetch(&self, tenant: Uuid, fresh: bool) -> BoxFuture<'_, Result<McpCredentials>>;
}

/// A closure `(tenant, fresh) -> future of credentials` as a [`CredentialSource`] (carebus-api's in-process minter,
/// tests).
pub struct FnSource<F>(pub F);

impl<F, Fut> CredentialSource for FnSource<F>
where
    F: Fn(Uuid, bool) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<McpCredentials>> + Send + 'static,
{
    fn fetch(&self, tenant: Uuid, fresh: bool) -> BoxFuture<'_, Result<McpCredentials>> {
        Box::pin((self.0)(tenant, fresh))
    }
}

/// carebus-api's `POST /api/internal/mcp-credentials {tenant_id, fresh}` with the service token (`CAREBUS_API_TOKEN`),
/// answered `{tenant_id, licence_token, strata_token, expires_at, expires_in}`. Only carebus-api holds the licences
/// minter credential; the tool-executor and the researcher ask it.
pub struct ApiCredentialSource {
    http: reqwest::Client,
    url: String,
    token: String,
    /// Ask for the BINDING caller's credentials (`kind: binding`: the tenant's mcp licence plus its binding token,
    /// for catalog tools and MCP tasks) instead of the care-bus licence.
    binding: bool,
}

/// Never prints the service token.
impl std::fmt::Debug for ApiCredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiCredentialSource")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct WireCredentials {
    tenant_id: Uuid,
    licence_token: String,
    strata_token: String,
    /// Only with `kind: binding`.
    #[serde(default)]
    binding_token: Option<String>,
    /// Seconds the credentials stay valid.
    expires_in: u64,
}

impl ApiCredentialSource {
    pub fn new(base_url: &str, token: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client"),
            url: format!("{}{MCP_CREDENTIALS_PATH}", base_url.trim_end_matches('/')),
            token,
            binding: false,
        }
    }

    /// The same source for the binding caller (catalog tools, MCP tasks).
    pub fn binding(mut self) -> Self {
        self.binding = true;
        self
    }

    async fn ask(&self, tenant: Uuid, fresh: bool) -> Result<McpCredentials> {
        let w: WireCredentials = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&if self.binding {
                serde_json::json!({"tenant_id": tenant, "fresh": fresh, "kind": "binding"})
            } else {
                serde_json::json!({"tenant_id": tenant, "fresh": fresh})
            })
            .send()
            .await
            .context("carebus-api unreachable")?
            .error_for_status()
            .context("carebus-api refused the MCP credentials")?
            .json()
            .await
            .context("carebus-api returned unreadable MCP credentials")?;
        anyhow::ensure!(
            w.tenant_id == tenant,
            "carebus-api answered with credentials for another tenant; not used"
        );
        let left = Duration::from_secs(w.expires_in).min(MAX_LIFETIME);
        let c = McpCredentials::new(&w.licence_token, &w.strata_token, Instant::now() + left)?;
        if !self.binding {
            return Ok(c);
        }
        let b = w
            .binding_token
            .context("carebus-api answered binding credentials without a binding token")?;
        c.with_binding(&b)
    }
}

impl CredentialSource for ApiCredentialSource {
    fn fetch(&self, tenant: Uuid, fresh: bool) -> BoxFuture<'_, Result<McpCredentials>> {
        Box::pin(self.ask(tenant, fresh))
    }
}

/// tenant -> (credentials, refetch at).
type CredentialCache = HashMap<Uuid, (McpCredentials, Instant)>;

/// Per-tenant credentials, cached until shortly before they expire. Fetches are single-flight per tenant: concurrent
/// cold starts (or renewals) of one tenant wait for one fetch; other tenants are never held up.
pub struct TenantCredentials {
    source: Box<dyn CredentialSource>,
    cached: std::sync::Mutex<CredentialCache>,
    /// One fetch at a time per tenant.
    flights: std::sync::Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
    /// Refetch this long before the credentials expire (at most half their remaining life).
    pub margin: Duration,
}

impl TenantCredentials {
    pub fn new(source: impl CredentialSource) -> Self {
        Self {
            source: Box::new(source),
            cached: std::sync::Mutex::new(HashMap::new()),
            flights: std::sync::Mutex::new(HashMap::new()),
            margin: Duration::from_secs(60),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CredentialCache> {
        self.cached.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `tenant`'s fetch slot.
    fn flight(&self, tenant: Uuid) -> Arc<Mutex<()>> {
        self.flights
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(tenant)
            .or_default()
            .clone()
    }

    fn cached(&self, tenant: Uuid) -> Option<McpCredentials> {
        let now = Instant::now();
        self.lock()
            .get(&tenant)
            .filter(|(_, refetch_at)| now < *refetch_at)
            .map(|(c, _)| c.clone())
    }

    async fn fetch(&self, tenant: Uuid, fresh: bool) -> Result<McpCredentials> {
        let c = self
            .source
            .fetch(tenant, fresh)
            .await
            .with_context(|| format!("MCP credentials for tenant {tenant}"))?;
        let now = Instant::now();
        let left = c.expires.saturating_duration_since(now);
        let refetch_at = now + left.saturating_sub(self.margin.min(left / 2));
        self.lock().insert(tenant, (c.clone(), refetch_at));
        Ok(c)
    }

    /// `tenant`'s credentials: the cached ones while they are fresh, else fetched (once for concurrent callers).
    pub async fn get(&self, tenant: Uuid) -> Result<McpCredentials> {
        if let Some(c) = self.cached(tenant) {
            return Ok(c);
        }
        let flight = self.flight(tenant);
        let _one = flight.lock().await;
        // A concurrent caller may have fetched while this one waited.
        if let Some(c) = self.cached(tenant) {
            return Ok(c);
        }
        self.fetch(tenant, false).await
    }

    /// After the licence `stale` was refused: fresh credentials, or the cached ones when another request already
    /// replaced `stale`.
    pub async fn renew(&self, tenant: Uuid, stale: &str) -> Result<McpCredentials> {
        let flight = self.flight(tenant);
        let _one = flight.lock().await;
        if let Some(c) = self.cached(tenant)
            && c.licence_token() != stale
        {
            return Ok(c);
        }
        self.fetch(tenant, true).await
    }

    /// Forget `tenant`'s credentials; the next `get` fetches.
    pub fn invalidate(&self, tenant: Uuid) {
        self.lock().remove(&tenant);
    }
}

/// Longest wait between two SSE reconnects. Unbounded doubling reached 512 s live.
const SSE_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

/// rmcp's own default (`DEFAULT_MAX_SSE_EVENT_SIZE` is crate-private); only used by the trait's
/// size-less entry points, which the transport never calls.
const MAX_SSE_EVENT_SIZE: usize = 16 * 1024 * 1024;

/// The credentials one session's requests carry. Shared by the session and its transport's client, so a renewal
/// after a 401 is seen by every later request: POSTs and the `GET /mcp` reconnects. Bound to ONE tenant.
#[derive(Clone)]
pub(crate) struct SessionAuth {
    tenant: Uuid,
    current: Arc<std::sync::Mutex<McpCredentials>>,
    creds: Arc<TenantCredentials>,
    /// HTTP status (401/403) of the last request refused even with fresh credentials; 0 = none, or a later request
    /// succeeded. Read when a call fails, to report `auth_rejected`.
    rejected: Arc<AtomicU16>,
}

impl SessionAuth {
    fn new(tenant: Uuid, current: McpCredentials, creds: Arc<TenantCredentials>) -> Self {
        Self {
            tenant,
            current: Arc::new(std::sync::Mutex::new(current)),
            creds,
            rejected: Arc::new(AtomicU16::new(0)),
        }
    }

    /// The status of a refusal that survived the fresh-credentials retry, if no request succeeded since.
    fn rejection(&self) -> Option<u16> {
        match self.rejected.load(Ordering::SeqCst) {
            0 => None,
            status => Some(status),
        }
    }

    /// Record the outcome of a request that was not refused, or of the retry with fresh credentials: a success
    /// clears the rejection, a refusal of the retry records it.
    fn settle<T>(&self, outcome: Result<T, HttpError>) -> Result<T, HttpError> {
        match &outcome {
            Ok(_) => self.rejected.store(0, Ordering::SeqCst),
            Err(e) => {
                if let Some(status) = auth_refusal(e) {
                    tracing::error!(
                        status,
                        tenant = %self.tenant,
                        "mcp: freshly fetched licence credentials were refused too"
                    );
                    self.rejected.store(status, Ordering::SeqCst);
                }
            }
        }
        outcome
    }

    fn current(&self) -> McpCredentials {
        self.current.lock().expect("session auth lock").clone()
    }

    /// After `refused` got a 401/403: fetch fresh credentials for this session's tenant and use them from now on.
    /// `None` when they cannot be fetched (the caller returns the original error, retryable).
    async fn refresh(&self, refused: &McpCredentials) -> Option<McpCredentials> {
        match self.creds.renew(self.tenant, refused.licence_token()).await {
            Ok(fresh) => {
                *self.current.lock().expect("session auth lock") = fresh.clone();
                tracing::warn!(tenant = %self.tenant, "mcp: licence refused (401/403), retrying once with fresh credentials");
                Some(fresh)
            }
            Err(e) => {
                tracing::warn!(tenant = %self.tenant, error = %format!("{e:#}"), "mcp: licence refused (401/403) and no fresh credentials");
                None
            }
        }
    }
}

type HttpError = StreamableHttpError<reqwest::Error>;

/// The status of a 401 or 403 in any of the shapes rmcp's reqwest client reports it:
/// `AuthRequired` / `InsufficientScope` (with a `WWW-Authenticate` challenge), the GET's
/// `error_for_status`, or a POST's `HTTP 401 …` / `HTTP 403 …` text.
fn auth_refusal(e: &HttpError) -> Option<u16> {
    match e {
        StreamableHttpError::AuthRequired(_) => Some(401),
        StreamableHttpError::InsufficientScope(_) => Some(403),
        StreamableHttpError::Client(e) => e
            .status()
            .filter(|s| {
                *s == reqwest::StatusCode::UNAUTHORIZED || *s == reqwest::StatusCode::FORBIDDEN
            })
            .map(|s| s.as_u16()),
        StreamableHttpError::UnexpectedServerResponse(m) if m.starts_with("HTTP 401") => Some(401),
        StreamableHttpError::UnexpectedServerResponse(m) if m.starts_with("HTTP 403") => Some(403),
        _ => None,
    }
}

/// rmcp's reqwest client whose requests always carry the session's current tenant credentials (and never an
/// `Authorization` header), and that on a 401/403 fetches fresh credentials for that tenant and retries the request
/// once. A refusal of the retry is recorded on the session (`SessionAuth::rejection`) and the call reports
/// `auth_rejected`. The transport's static `auth_header` is ignored: it is what made a restarted server's
/// `GET /mcp` loop on 401.
#[derive(Clone)]
struct RefreshingClient {
    http: reqwest::Client,
    auth: SessionAuth,
}

impl StreamableHttpClient for RefreshingClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            None,
            custom_headers,
            MAX_SSE_EVENT_SIZE,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let creds = self.auth.current();
        let first = self
            .http
            .post_message_with_max_sse_event_size(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                None,
                creds.apply(custom_headers.clone()),
                max_sse_event_size,
            )
            .await;
        match first {
            Err(e) if auth_refusal(&e).is_some() => match self.auth.refresh(&creds).await {
                Some(fresh) => self.auth.settle(
                    self.http
                        .post_message_with_max_sse_event_size(
                            uri,
                            message,
                            session_id,
                            None,
                            fresh.apply(custom_headers),
                            max_sse_event_size,
                        )
                        .await,
                ),
                None => Err(e),
            },
            other => self.auth.settle(other),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        let creds = self.auth.current();
        let first = self
            .http
            .delete_session(
                uri.clone(),
                session_id.clone(),
                None,
                creds.apply(custom_headers.clone()),
            )
            .await;
        match first {
            Err(e) if auth_refusal(&e).is_some() => match self.auth.refresh(&creds).await {
                Some(fresh) => self.auth.settle(
                    self.http
                        .delete_session(uri, session_id, None, fresh.apply(custom_headers))
                        .await,
                ),
                None => Err(e),
            },
            other => self.auth.settle(other),
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, HttpError> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            None,
            custom_headers,
            MAX_SSE_EVENT_SIZE,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, HttpError> {
        let creds = self.auth.current();
        let first = self
            .http
            .get_stream_with_max_sse_event_size(
                uri.clone(),
                session_id.clone(),
                last_event_id.clone(),
                None,
                creds.apply(custom_headers.clone()),
                max_sse_event_size,
            )
            .await;
        match first {
            Err(e) if auth_refusal(&e).is_some() => match self.auth.refresh(&creds).await {
                Some(fresh) => self.auth.settle(
                    self.http
                        .get_stream_with_max_sse_event_size(
                            uri,
                            session_id,
                            last_event_id,
                            None,
                            fresh.apply(custom_headers),
                            max_sse_event_size,
                        )
                        .await,
                ),
                None => Err(e),
            },
            other => self.auth.settle(other),
        }
    }
}

/// One tenant's MCP session: the credentials it was opened with, a generation number, and the running service.
/// Calls hold the `Arc`, so a session replaced on a licence rotation stays open until its last in-flight call
/// finishes.
struct Session {
    auth: SessionAuth,
    generation: u64,
    service: Arc<RunningService<RoleClient, ClientConfig>>,
    /// The server advertised the tasks extension (crate::mcp_tasks); without it a task-capable call is a plain call.
    tasks: bool,
}

/// A tenant's open session: generation (for `drop_session`), service, auth (for `classify`), tasks offered.
pub(crate) type OpenSession = (
    u64,
    Arc<RunningService<RoleClient, ClientConfig>>,
    SessionAuth,
    bool,
);

/// One tenant's session slot. Its own lock: opening (or dropping) one tenant's session never waits on another's.
type SessionSlot = Arc<Mutex<Option<Session>>>;

pub struct HttpMcp {
    url: String,
    creds: Arc<TenantCredentials>,
    /// One session slot per tenant; the map lock is only held to find or add a slot, never across I/O.
    sessions: std::sync::Mutex<HashMap<Uuid, SessionSlot>>,
    generations: AtomicU64,
    /// Upper bound for opening a session (`initialize`), so a hung MCP server fails the call instead of holding the
    /// tenant's slot.
    pub connect_timeout: Duration,
    /// Upper bound for `list_tools` (every page), so callers report "unavailable" instead of hanging.
    pub list_timeout: Duration,
    /// First delay before the SSE stream (`GET /mcp`) reconnects; doubles per attempt.
    pub sse_retry_base: Duration,
}

impl HttpMcp {
    pub fn new(url: String, creds: TenantCredentials) -> Self {
        Self {
            url,
            creds: Arc::new(creds),
            sessions: std::sync::Mutex::new(HashMap::new()),
            generations: AtomicU64::new(0),
            connect_timeout: Duration::from_secs(10),
            list_timeout: Duration::from_secs(10),
            sse_retry_base: Duration::from_secs(1),
        }
    }

    pub fn from_env() -> Result<Self> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// `MCP_URL`, and `CAREBUS_API_URL` / `CAREBUS_API_TOKEN` for the per-tenant credentials
    /// ([`ApiCredentialSource`]); unset and empty values are both errors.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let var = |name: &str| crate::required_var(name, get(name));
        let url = var("MCP_URL")?;
        let api = var("CAREBUS_API_URL").context("MCP credentials come from carebus-api")?;
        let token = var("CAREBUS_API_TOKEN").context("MCP credentials come from carebus-api")?;
        Ok(Self::new(
            url,
            TenantCredentials::new(ApiCredentialSource::new(&api, token)),
        ))
    }

    /// [`Self::from_vars`] as the BINDING caller: the tenant's mcp licence plus its binding, which catalog tools and
    /// MCP tasks require (carebus-api provisions the binding).
    pub fn catalog_from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let var = |name: &str| crate::required_var(name, get(name));
        let url = var("MCP_URL")?;
        let api = var("CAREBUS_API_URL").context("MCP credentials come from carebus-api")?;
        let token = var("CAREBUS_API_TOKEN").context("MCP credentials come from carebus-api")?;
        Ok(Self::new(
            url,
            TenantCredentials::new(ApiCredentialSource::new(&api, token).binding()),
        ))
    }

    /// An [`McpClient`] that calls as `tenant`.
    pub fn for_tenant(self: &Arc<Self>, tenant: Uuid) -> TenantMcp {
        TenantMcp {
            mcp: self.clone(),
            tenant,
        }
    }

    /// `tenant`'s current credentials, for callers that hit the server's HTTP routes directly (carebus-api's
    /// `/api/health` probe).
    pub async fn credentials(&self, tenant: Uuid) -> Result<McpCredentials> {
        self.creds.get(tenant).await
    }

    /// Fresh credentials for `tenant` after the server refused `stale`.
    pub async fn renew_credentials(
        &self,
        tenant: Uuid,
        stale: &McpCredentials,
    ) -> Result<McpCredentials> {
        self.creds.renew(tenant, stale.licence_token()).await
    }

    /// `tenant`'s session for its current credentials; opens a new one when they changed. Returns the session's
    /// generation for `drop_session`, and its auth for `classify`. No credentials: an error, and no request is sent.
    async fn session(
        &self,
        tenant: Uuid,
    ) -> Result<
        (
            u64,
            Arc<RunningService<RoleClient, ClientConfig>>,
            SessionAuth,
        ),
        ToolError,
    > {
        let (generation, service, auth, _) = self.session_for_tasks(tenant).await?;
        Ok((generation, service, auth))
    }

    /// [`Self::session`], plus whether the server offers MCP tasks.
    pub(crate) async fn session_for_tasks(&self, tenant: Uuid) -> Result<OpenSession, ToolError> {
        let creds = self
            .creds
            .get(tenant)
            .await
            .map_err(|e| retryable(format!("mcp credentials: {e:#}")))?;
        let slot = self.slot(tenant);
        let mut guard = slot.lock().await;
        // `auth.current()` follows a 401 renewal inside the transport, so a session whose client already moved to
        // the fresh credentials is kept rather than re-initialized.
        if let Some(s) = guard.as_ref()
            && s.auth.current().licence_token() == creds.licence_token()
        {
            return Ok((s.generation, s.service.clone(), s.auth.clone(), s.tasks));
        }
        let auth = SessionAuth::new(tenant, creds, self.creds.clone());
        // The headers are supplied per request by `RefreshingClient`; rmcp's static `auth_header` is left unset so
        // no copy of a credential can go stale inside the transport.
        let mut cfg = StreamableHttpClientTransportConfig::with_uri(self.url.clone());
        let mut backoff = ExponentialBackoff::default();
        backoff.base_duration = self.sse_retry_base;
        backoff.max_delay = Some(SSE_RETRY_MAX_DELAY);
        cfg.retry_config = Arc::new(backoff);
        let client = RefreshingClient {
            http: reqwest::Client::new(),
            auth: auth.clone(),
        };
        let transport = StreamableHttpClientTransport::with_client(client, cfg);
        // Every session declares the tasks extension (C1); the server only returns tasks to clients that do.
        let service = match tokio::time::timeout(
            self.connect_timeout,
            crate::mcp_tasks::client_config()
                .serve_with_lifecycle(transport, crate::mcp_tasks::lifecycle()),
        )
        .await
        {
            Ok(Ok(service)) => Arc::new(service),
            // `initialize` refused even with fresh credentials: a licence or configuration fault, not an outage.
            Ok(Err(e)) => {
                return Err(match auth.rejection() {
                    Some(status) => auth_rejected(status),
                    None => retryable(format!("mcp connect: {e}")),
                });
            }
            Err(_) => {
                return Err(retryable(format!(
                    "mcp connect: no answer within {} ms",
                    self.connect_timeout.as_millis()
                )));
            }
        };
        let tasks = service
            .peer()
            .peer_info()
            .is_some_and(|i| i.capabilities.supports_tasks());
        if !tasks {
            tracing::info!(%tenant, "MCP server does not offer tasks; long-running tools run as plain calls");
        }
        let generation = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
        *guard = Some(Session {
            auth: auth.clone(),
            generation,
            service: service.clone(),
            tasks,
        });
        Ok((generation, service, auth, tasks))
    }

    /// `tenant`'s session slot, created on first use.
    fn slot(&self, tenant: Uuid) -> SessionSlot {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(tenant)
            .or_default()
            .clone()
    }

    /// Forget `tenant`'s session `generation` and its cached credentials: reconnect with fetched credentials on the
    /// next call. A no-op when a newer session already replaced it (a late failure on a rotated-out session must not
    /// drop the fresh one).
    async fn drop_session(&self, tenant: Uuid, generation: u64) {
        let slot = self.slot(tenant);
        let mut guard = slot.lock().await;
        if guard.as_ref().is_some_and(|s| s.generation == generation) {
            *guard = None;
            drop(guard);
            self.creds.invalidate(tenant);
        }
    }

    /// The tools the server offers `tenant` (`tools/list`, every page), bounded by `list_timeout`.
    /// Used by carebus-api `GET /api/jobs/{job}/tools` and the researcher.
    pub async fn list_tools(&self, tenant: Uuid) -> Result<Vec<McpTool>> {
        let (generation, svc, auth) = self
            .session(tenant)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        let tools = match tokio::time::timeout(self.list_timeout, svc.peer().list_all_tools()).await
        {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                let (te, reset) = classify(&e, &auth);
                if reset {
                    self.drop_session(tenant, generation).await;
                }
                anyhow::bail!("mcp tools/list: {}", te.message);
            }
            Err(_) => {
                self.drop_session(tenant, generation).await;
                anyhow::bail!(
                    "mcp tools/list: no answer within {} ms",
                    self.list_timeout.as_millis()
                );
            }
        };
        // Map through JSON so field names follow the MCP wire format, whatever rmcp's Rust types are.
        let v = serde_json::json!({ "tools": serde_json::to_value(&tools)? });
        Ok(mcp_tools_from_list_result(&v))
    }

    /// One tool call as `tenant`.
    pub async fn call_tool_for(
        &self,
        tenant: Uuid,
        name: &str,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        // Holding the Arc keeps this call's session open even if a licence rotation replaces it meanwhile.
        let (generation, svc, auth) = self.session(tenant).await?;
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Value::Object(m) = arguments {
            params = params.with_arguments(m);
        }
        match svc.peer().call_tool(params).await {
            Ok(res) => tool_result_to_value(res),
            Err(e) => Err(self
                .fail(tenant, generation, &auth, &e, &format!("call {name}"))
                .await),
        }
    }

    /// A failed request on session `generation` as a tool error (`mcp <what>: …`); drops the session when the failure
    /// calls for a reconnect.
    pub(crate) async fn fail(
        &self,
        tenant: Uuid,
        generation: u64,
        auth: &SessionAuth,
        e: &ServiceError,
        what: &str,
    ) -> ToolError {
        let (mut te, reset) = classify(e, auth);
        if reset {
            self.drop_session(tenant, generation).await; // reconnect on the next call
        }
        te.message = format!("mcp {what}: {}", te.message);
        te
    }
}

/// [`HttpMcp`] bound to one tenant: the tool-executor builds one per scope.
#[derive(Clone)]
pub struct TenantMcp {
    mcp: Arc<HttpMcp>,
    tenant: Uuid,
}

impl TenantMcp {
    pub fn tenant(&self) -> Uuid {
        self.tenant
    }
}

impl McpClient for TenantMcp {
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, ToolError> {
        self.mcp.call_tool_for(self.tenant, name, arguments).await
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Parse an MCP `tools/list` result: `{"tools":[{"name","description","inputSchema"}]}`. Entries without a name are skipped.
pub fn mcp_tools_from_list_result(result: &Value) -> Vec<McpTool> {
    result["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    Some(McpTool {
                        name: t["name"].as_str()?.to_string(),
                        description: t["description"].as_str().map(String::from),
                        input_schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `service_error_to_tool_error`, except that a transport failure on a session whose fresh credentials were refused
/// (401/403 after the retry) is the permanent `auth_rejected`; the session is dropped.
fn classify(e: &ServiceError, auth: &SessionAuth) -> (ToolError, bool) {
    match (e, auth.rejection()) {
        (ServiceError::McpError(_), _) | (_, None) => service_error_to_tool_error(e),
        (_, Some(status)) => (auth_rejected(status), true),
    }
}

/// Classify an rmcp failure. A JSON-RPC protocol error (unknown tool, invalid params) will not
/// succeed on retry: permanent, session kept. Transport / timeout / cancelled / anything else:
/// retryable, and the second value says the session and credentials should be dropped.
pub fn service_error_to_tool_error(e: &ServiceError) -> (ToolError, bool) {
    match e {
        ServiceError::McpError(_) => (
            ToolError {
                message: e.to_string(),
                retryable: false,
            },
            false,
        ),
        _ => (retryable(e.to_string()), true),
    }
}

/// Plan 03 error contract: `[retryable] …` / `[permanent] …`; no prefix means permanent.
pub fn tool_error_from_text(text: &str) -> ToolError {
    let text = text.trim();
    let (retryable, message) = if let Some(m) = text.strip_prefix("[retryable] ") {
        (true, m)
    } else if let Some(m) = text.strip_prefix("[permanent] ") {
        (false, m)
    } else {
        (false, text)
    };
    let message = if message.is_empty() {
        "tool error"
    } else {
        message
    };
    ToolError {
        message: message.to_string(),
        retryable,
    }
}

/// `isError` → tool error classified by its prefix; otherwise structured content, or the text parsed as JSON.
pub(crate) fn tool_result_to_value(res: CallToolResult) -> Result<Value, ToolError> {
    let text: String = res
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    if res.is_error == Some(true) {
        return Err(tool_error_from_text(&text));
    }
    if let Some(v) = res.structured_content {
        return Ok(v);
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}
