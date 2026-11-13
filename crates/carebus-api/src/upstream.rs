//! Shared plumbing for the proxy routes (strata_rag, demo-backends).
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::Value;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

/// What an upstream answered, once it has been judged reachable and willing.
pub struct Reply {
    pub status: StatusCode,
    pub body: Value,
}

/// A file upload to strata_ingest outlasts the 8 s client default. It must stay well under the 60 s `orphan_after`
/// in `runs.rs`: a run whose create is still in flight when that elapses would be failed as orphaned.
const UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The strata services' tenant header: the bare tenant uuid this request acts for.
pub const TENANT_HEADER: &str = "x-tenant-id";

/// Who we are to an upstream: its bearer and, for strata_rag / strata_ingest, the tenant we act for.
#[derive(Clone, Copy, Default)]
pub struct Auth<'a> {
    pub token: Option<&'a str>,
    /// Sent as `X-Tenant-Id` when set. strata_rag requires it to match a tenant-pinned token (403 otherwise) and
    /// requires it outright for a `*` token; strata_ingest takes the tenant from the token and ignores it.
    pub tenant: Option<Uuid>,
    /// Sent as `X-App-Id` when set: the app we call as (strata_rag relays it to inference-router).
    pub app: Option<&'static str>,
    /// Sent as `X-Actor-Id` when set and the call names no actor of its own ([`Extra::actor`] wins): care-bus'
    /// service account for strata_rag (crate::strata_tokens).
    pub actor: Option<Uuid>,
}

impl Auth<'_> {
    /// Bearer, tenant, app and actor headers onto `req`.
    pub fn apply(self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(t) = self.token {
            req = req.bearer_auth(t);
        }
        if let Some(t) = self.tenant {
            req = req.header(TENANT_HEADER, t.to_string());
        }
        if let Some(a) = self.app {
            req = req.header(carebus::identity::APP_ID_HEADER, a);
        }
        if let Some(a) = self.actor.filter(|a| !a.is_nil()) {
            req = req.header(ACTOR_HEADER, a.to_string());
        }
        req
    }
}

/// What a request carries.
pub enum Payload {
    Empty,
    Json(Value),
    Multipart(reqwest::multipart::Form),
    /// A body forwarded as-is under its own content type (a CSV import).
    Raw {
        content_type: String,
        body: Vec<u8>,
    },
}

/// Per-call additions to [`call_with`]: the person the call is made for and a longer timeout.
#[derive(Clone, Copy, Default)]
pub struct Extra {
    /// Sent as `X-Actor-Id` (bare uuid): who the upstream records as having made the change.
    pub actor: Option<Uuid>,
    /// Overrides the client's 8 s default (a whole eval run takes far longer).
    pub timeout: Option<std::time::Duration>,
}

/// The strata services' acting-user header.
pub const ACTOR_HEADER: &str = "x-actor-id";

/// `call_with` for a JSON body or none.
pub async fn call(
    http: &reqwest::Client,
    name: &'static str,
    base: Option<&str>,
    auth: Auth<'_>,
    method: Method,
    path_and_query: &str,
    body: Option<Value>,
) -> ApiResult<Reply> {
    let payload = body.map_or(Payload::Empty, Payload::Json);
    call_with(http, name, base, auth, method, path_and_query, payload).await
}

/// One upstream request. `base` unset, a connection failure, a 5xx, a 401/403 (our credentials are
/// wrong, which the console user cannot fix) and an unreadable 2xx body are all
/// 503 `upstream_unavailable` naming `name`. `204 No Content` is a success with a `Null` body.
/// Any other status is returned for the caller to judge.
pub async fn call_with(
    http: &reqwest::Client,
    name: &'static str,
    base: Option<&str>,
    auth: Auth<'_>,
    method: Method,
    path_and_query: &str,
    payload: Payload,
) -> ApiResult<Reply> {
    call_extra(
        http,
        name,
        base,
        auth,
        method,
        path_and_query,
        payload,
        Extra::default(),
    )
    .await
}

/// [`call_with`] plus an acting user and/or a timeout of its own.
#[allow(clippy::too_many_arguments)]
pub async fn call_extra(
    http: &reqwest::Client,
    name: &'static str,
    base: Option<&str>,
    auth: Auth<'_>,
    method: Method,
    path_and_query: &str,
    payload: Payload,
    extra: Extra,
) -> ApiResult<Reply> {
    let base = base.ok_or(ApiError::Upstream(name))?;
    // One X-Actor-Id: the call's own actor, else the credentials' (reqwest appends a repeated header).
    let auth = Auth {
        actor: extra.actor.or(auth.actor),
        ..auth
    };
    let req = auth.apply(http.request(
        method,
        format!("{}{path_and_query}", base.trim_end_matches('/')),
    ));
    let req = match payload {
        Payload::Empty => req,
        Payload::Json(b) => req.json(&b),
        Payload::Multipart(f) => req.multipart(f).timeout(UPLOAD_TIMEOUT),
        Payload::Raw { content_type, body } => req
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body),
    };
    let req = match extra.timeout {
        Some(t) => req.timeout(t),
        None => req,
    };
    let resp = req.send().await.map_err(|e| {
        tracing::warn!(upstream = name, error = %e, "upstream unreachable");
        ApiError::Upstream(name)
    })?;
    let raw = resp.status();
    if raw.is_server_error()
        || raw == reqwest::StatusCode::UNAUTHORIZED
        || raw == reqwest::StatusCode::FORBIDDEN
    {
        tracing::warn!(
            upstream = name,
            status = raw.as_u16(),
            "upstream refused or failed"
        );
        return Err(ApiError::Upstream(name));
    }
    let status = StatusCode::from_u16(raw.as_u16()).map_err(|_| ApiError::Upstream(name))?;
    if status == StatusCode::NO_CONTENT {
        return Ok(Reply {
            status,
            body: Value::Null,
        });
    }
    let bytes = resp.bytes().await.map_err(|_| ApiError::Upstream(name))?;
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => v,
        Err(_) if status.is_success() => return Err(ApiError::Upstream(name)),
        Err(_) => Value::Null,
    };
    Ok(Reply { status, body })
}

/// An upstream 4xx's own message, else a generic one naming `name`.
pub fn message(name: &str, status: StatusCode, body: &Value) -> String {
    body["error"]["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{name} rejected the request (HTTP {}).", status.as_u16()))
}

/// An upstream's 4xx as our error (shared by strata_rag and strata_ingest):
/// - `not_found` is our 404 naming `what`
/// - a 404/405 without its envelope (route missing) is 503
/// - 409 is `conflict_code` with the upstream message
/// - anything else is `upstream_rejected` with the upstream message (first 300 chars)
pub fn refusal(
    name: &'static str,
    status: StatusCode,
    body: &Value,
    what: &str,
    conflict_code: &'static str,
) -> ApiError {
    let msg: String = message(name, status, body).chars().take(300).collect();
    match (status, body["error"]["code"].as_str()) {
        (StatusCode::NOT_FOUND, Some("not_found")) => ApiError::NotFound(what.to_string()),
        (StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED, _) => ApiError::Upstream(name),
        (StatusCode::CONFLICT, _) => ApiError::Conflict(conflict_code, msg),
        _ => ApiError::Rejected(status, msg),
    }
}

/// A path segment that can never leave its route: letters, digits, `_`, `-` (no `/`, no `..`).
/// `upper` also admits uppercase (record ids like `ORD-58790`); table and scenario names are lowercase.
pub fn segment(v: &str, upper: bool) -> ApiResult<&str> {
    let ok = |c: char| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || c == '_'
            || c == '-'
            || (upper && c.is_ascii_uppercase())
    };
    if !v.is_empty() && v.chars().all(ok) {
        Ok(v)
    } else {
        Err(ApiError::BadRequest(
            "invalid_path",
            "A path segment may only contain letters, digits, _ and -.".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::delete;

    #[tokio::test]
    async fn no_content_is_a_null_body_not_unavailable() {
        let app = Router::new().route("/gone", delete(|| async { StatusCode::NO_CONTENT }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let http = reqwest::Client::new();
        let r = call_with(
            &http,
            "x",
            Some(&base),
            Auth::default(),
            Method::DELETE,
            "/gone",
            Payload::Empty,
        )
        .await
        .unwrap_or_else(|_| panic!("204 must not be 503"));
        assert_eq!((r.status, r.body), (StatusCode::NO_CONTENT, Value::Null));
    }

    /// Echoes the bearer and `X-Tenant-Id` it received (`null` when absent).
    async fn echo() -> String {
        let app = Router::new().route(
            "/echo",
            axum::routing::get(|h: axum::http::HeaderMap| async move {
                let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
                axum::Json(serde_json::json!({
                    "authorization": get("authorization"),
                    "tenant": get(TENANT_HEADER),
                    "tenants": h.get_all(TENANT_HEADER).iter().count(),
                    "actor": get(ACTOR_HEADER),
                    "actors": h.get_all(ACTOR_HEADER).iter().count(),
                    "app": get(carebus::identity::APP_ID_HEADER),
                }))
            })
            .post(|h: axum::http::HeaderMap, body: String| async move {
                let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
                axum::Json(serde_json::json!({
                    "actor": get(ACTOR_HEADER),
                    "content_type": get("content-type"),
                    "body": body,
                }))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        base
    }

    #[tokio::test]
    async fn the_tenant_is_sent_as_one_bare_uuid_header_next_to_the_bearer() {
        let base = echo().await;
        let http = reqwest::Client::new();
        let tenant = Uuid::parse_str("7f1c9a2e-0000-4000-8000-00000000fe11").unwrap();
        let auth = Auth {
            token: Some("t-1"),
            tenant: Some(tenant),
            ..Auth::default()
        };
        let r = call(&http, "x", Some(&base), auth, Method::GET, "/echo", None)
            .await
            .unwrap_or_else(|_| panic!("echo is reachable"));
        assert_eq!(r.body["authorization"], "Bearer t-1");
        assert_eq!(
            r.body["tenant"],
            tenant.to_string(),
            "bare uuid, no `Bearer`"
        );
        assert_eq!(r.body["tenants"], 1);

        let r = call(
            &http,
            "x",
            Some(&base),
            Auth::default(),
            Method::GET,
            "/echo",
            None,
        )
        .await
        .unwrap_or_else(|_| panic!("echo is reachable"));
        assert_eq!(r.body["tenant"], Value::Null, "no tenant, no header");
        assert_eq!(r.body["authorization"], Value::Null);
    }

    #[tokio::test]
    async fn extra_sends_the_actor_and_a_raw_body_under_its_own_content_type() {
        let base = echo().await;
        let http = reqwest::Client::new();
        let actor = Uuid::parse_str("7f1c9a2e-0000-4000-8000-00000000ac70").unwrap();
        let r = call_extra(
            &http,
            "x",
            Some(&base),
            Auth::default(),
            Method::POST,
            "/echo",
            Payload::Raw {
                content_type: "text/csv".into(),
                body: b"query,expected_text\nq,t\n".to_vec(),
            },
            Extra {
                actor: Some(actor),
                timeout: Some(std::time::Duration::from_secs(30)),
            },
        )
        .await
        .unwrap_or_else(|_| panic!("echo is reachable"));
        assert_eq!(r.body["actor"], actor.to_string());
        assert_eq!(r.body["content_type"], "text/csv");
        assert_eq!(r.body["body"], "query,expected_text\nq,t\n");

        // the plain call never sends an actor
        let r = call(
            &http,
            "x",
            Some(&base),
            Auth::default(),
            Method::GET,
            "/echo",
            None,
        )
        .await
        .unwrap_or_else(|_| panic!("echo is reachable"));
        assert_eq!(r.body["actor"], Value::Null);
    }

    /// strata_rag credentials: `X-App-Id` on every call, `X-Actor-Id` = the service account when the call names no
    /// actor, the call's own actor when it does (exactly one header), and nothing of either when unset.
    #[tokio::test]
    async fn the_app_id_and_the_fallback_actor_ride_on_the_credentials() {
        let base = echo().await;
        let http = reqwest::Client::new();
        let service = Uuid::parse_str("7f1c9a2e-0000-4000-8000-000000005e5a").unwrap();
        let person = Uuid::parse_str("7f1c9a2e-0000-4000-8000-00000000ac70").unwrap();
        let auth = Auth {
            token: Some("t-1"),
            tenant: Some(Uuid::new_v4()),
            app: Some(carebus::identity::APP_ID),
            actor: Some(service),
        };
        let get = |extra: Extra| {
            let http = http.clone();
            let base = base.clone();
            async move {
                call_extra(
                    &http,
                    "x",
                    Some(&base),
                    auth,
                    Method::GET,
                    "/echo",
                    Payload::Empty,
                    extra,
                )
                .await
                .unwrap_or_else(|_| panic!("echo is reachable"))
                .body
            }
        };
        let b = get(Extra::default()).await;
        assert_eq!(b["app"], "care-bus");
        assert_eq!(
            b["actor"],
            service.to_string(),
            "no actor: the service account"
        );
        assert_eq!(b["actors"], 1);
        let b = get(Extra {
            actor: Some(person),
            timeout: None,
        })
        .await;
        assert_eq!(b["app"], "care-bus");
        assert_eq!(b["actor"], person.to_string(), "the person wins");
        assert_eq!(b["actors"], 1, "one X-Actor-Id, never both");

        // credentials without app or actor (demo-backends, strata_ingest) send neither
        let r = call(
            &http,
            "x",
            Some(&base),
            Auth::default(),
            Method::GET,
            "/echo",
            None,
        )
        .await
        .unwrap_or_else(|_| panic!("echo is reachable"));
        assert_eq!(
            (&r.body["app"], &r.body["actor"]),
            (&Value::Null, &Value::Null)
        );
    }
}
