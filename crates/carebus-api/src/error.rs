use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{1}")]
    BadRequest(&'static str, String),
    #[error("missing or invalid token")]
    Unauthorized,
    #[error("this needs the {0} role")]
    Forbidden(&'static str),
    /// A valid token whose subject has no `users` row (and is not a super_admin).
    #[error("this account is not provisioned in any tenant")]
    NotProvisioned,
    #[error("this tenant is suspended")]
    TenantSuspended,
    /// Licence gate 2 (crate::licence): `license_required` or `license_unavailable`, both 402.
    #[error("{1}")]
    PaymentRequired(&'static str, String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{1}")]
    Conflict(&'static str, String),
    /// A 409 whose envelope carries more than code and message: these object's keys, next to them in `error`.
    #[error("{1}")]
    ConflictWith(&'static str, String, serde_json::Value),
    /// Well-formed but refused by a rule (422 with its own code).
    #[error("{1}")]
    Unprocessable(&'static str, String),
    #[error("field {0} looks like a secret; store a secret_ref instead")]
    SecretRejected(String),
    #[error("{0} is unavailable")]
    Upstream(&'static str),
    #[error("{0}")]
    NotConfigured(&'static str),
    /// An upstream refused the request (a 4xx other than auth): its status, our envelope, its safe message.
    #[error("{1}")]
    Rejected(StatusCode, String),
    /// A secret found in a source body outside its `credentials` field.
    #[error("{0} looks like a secret; send it in the credentials field instead")]
    SecretUseCredentials(String),
    #[error("{0}")]
    PayloadTooLarge(&'static str),
    #[error("{0}")]
    Unsupported(String),
    #[error("this route does not accept that method")]
    MethodNotAllowed,
    /// 403 with its own machine code (the early-access service-account doors).
    #[error("{1}")]
    Denied(&'static str, String),
    /// 503 with its own machine code (`signup_not_configured`, `signup_queue_full`).
    #[error("{1}")]
    Unavailable(&'static str, String),
    /// Any status with its own code and extra envelope keys (a signup approve that did not land: 409/502 + `steps`).
    #[error("{2}")]
    Detailed(StatusCode, &'static str, String, serde_json::Value),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::RowNotFound => ApiError::NotFound("row".into()),
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => {
                ApiError::Upstream("postgres")
            }
            sqlx::Error::Database(d) if d.is_unique_violation() => {
                ApiError::Conflict("conflict", "that already exists".into())
            }
            _ => ApiError::Internal(e.into()),
        }
    }
}

impl From<redis::RedisError> for ApiError {
    fn from(e: redis::RedisError) -> Self {
        if e.is_io_error()
            || e.is_connection_dropped()
            || e.is_connection_refusal()
            || e.is_timeout()
        {
            ApiError::Upstream("redis")
        } else {
            ApiError::Internal(e.into())
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// HTTP status and machine code only; never carries message text (safe for audit details).
    pub fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest(c, _) => (StatusCode::BAD_REQUEST, *c),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotProvisioned => (StatusCode::FORBIDDEN, "not_provisioned"),
            ApiError::TenantSuspended => (StatusCode::FORBIDDEN, "tenant_suspended"),
            ApiError::PaymentRequired(c, _) => (StatusCode::PAYMENT_REQUIRED, *c),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Conflict(c, _) | ApiError::ConflictWith(c, _, _) => {
                (StatusCode::CONFLICT, *c)
            }
            ApiError::Unprocessable(c, _) => (StatusCode::UNPROCESSABLE_ENTITY, *c),
            ApiError::SecretRejected(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "secret_value_rejected")
            }
            ApiError::Upstream(_) => (StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable"),
            ApiError::NotConfigured(_) => (StatusCode::SERVICE_UNAVAILABLE, "not_configured"),
            ApiError::Rejected(st, _) => (*st, "upstream_rejected"),
            ApiError::SecretUseCredentials(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "secret_value_rejected")
            }
            ApiError::PayloadTooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
            ApiError::Unsupported(_) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_format"),
            ApiError::MethodNotAllowed => (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"),
            ApiError::Denied(c, _) => (StatusCode::FORBIDDEN, *c),
            ApiError::Unavailable(c, _) => (StatusCode::SERVICE_UNAVAILABLE, *c),
            ApiError::Detailed(st, c, _, _) => (*st, *c),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }

    /// The machine code alone.
    pub fn code(&self) -> &'static str {
        self.status_and_code().1
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(e) = &self {
            tracing::error!(error = %format!("{e:#}"), "internal error");
        }
        let (status, code) = self.status_and_code();
        let message = match &self {
            ApiError::Internal(_) => "Something went wrong on our side.".to_string(),
            o => o.to_string(),
        };
        let mut error = json!({"code": code, "message": message});
        if let ApiError::ConflictWith(_, _, serde_json::Value::Object(extra))
        | ApiError::Detailed(_, _, _, serde_json::Value::Object(extra)) = &self
        {
            for (k, v) in extra {
                if k != "code" && k != "message" {
                    error[k.as_str()] = v.clone();
                }
            }
        }
        (status, Json(json!({ "error": error }))).into_response()
    }
}

/// Unknown route: the same error envelope as everything else.
pub async fn fallback() -> ApiError {
    ApiError::NotFound("route".into())
}

/// Known route, wrong method: the same error envelope.
pub async fn method_not_allowed() -> ApiError {
    ApiError::MethodNotAllowed
}

/// `Json<T>` whose rejection is a 400 `invalid_body` envelope. Routes never use bare `Json` input.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(r) => Err(reject_json(r)),
        }
    }
}

fn reject_json(r: JsonRejection) -> ApiError {
    ApiError::BadRequest(
        "invalid_body",
        format!("The request body is not valid: {}", r.body_text()),
    )
}

/// `Query<T>` whose rejection is a 400 `invalid_query` envelope.
pub struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Self(v)),
            Err(r) => Err(reject_query(r)),
        }
    }
}

fn reject_query(r: QueryRejection) -> ApiError {
    ApiError::BadRequest(
        "invalid_query",
        format!("The query string is not valid: {}", r.body_text()),
    )
}

/// `Path<T>` whose rejection is a 400 `invalid_path` envelope.
pub struct ApiPath<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(v)) => Ok(Self(v)),
            Err(r) => Err(reject_path(r)),
        }
    }
}

fn reject_path(r: PathRejection) -> ApiError {
    ApiError::BadRequest(
        "invalid_path",
        format!("The path is not valid: {}", r.body_text()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use http_body_util::BodyExt;
    use serde::Deserialize;
    use tower::ServiceExt;

    #[derive(Deserialize)]
    struct Body1 {
        #[allow(dead_code)]
        n: i32,
    }
    #[derive(Deserialize)]
    struct Q {
        #[allow(dead_code)]
        limit: i64,
    }

    fn app() -> Router {
        Router::new()
            .route("/body", post(|ApiJson(_): ApiJson<Body1>| async { "ok" }))
            .route("/q", get(|ApiQuery(_): ApiQuery<Q>| async { "ok" }))
            .route("/p/{id}", get(|ApiPath(_): ApiPath<i64>| async { "ok" }))
    }

    async fn code(req: Request<Body>) -> (StatusCode, String) {
        let resp = app().oneshot(req).await.unwrap();
        let st = resp.status();
        let b = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap_or_default();
        (st, v["error"]["code"].as_str().unwrap_or("").to_string())
    }

    #[tokio::test]
    async fn extractor_rejections_use_the_envelope() {
        let bad_json = Request::post("/body")
            .header("content-type", "application/json")
            .body(Body::from("{\"n\":\"x\"}"))
            .unwrap();
        assert_eq!(
            code(bad_json).await,
            (StatusCode::BAD_REQUEST, "invalid_body".into())
        );
        let no_ct = Request::post("/body")
            .body(Body::from("{\"n\":1}"))
            .unwrap();
        assert_eq!(
            code(no_ct).await,
            (StatusCode::BAD_REQUEST, "invalid_body".into())
        );
        let bad_q = Request::get("/q?limit=abc").body(Body::empty()).unwrap();
        assert_eq!(
            code(bad_q).await,
            (StatusCode::BAD_REQUEST, "invalid_query".into())
        );
        let bad_p = Request::get("/p/abc").body(Body::empty()).unwrap();
        assert_eq!(
            code(bad_p).await,
            (StatusCode::BAD_REQUEST, "invalid_path".into())
        );
        let ok = Request::get("/p/7").body(Body::empty()).unwrap();
        assert_eq!(code(ok).await.0, StatusCode::OK);
    }

    #[test]
    fn dependency_errors_map_to_503() {
        let e: ApiError = sqlx::Error::PoolTimedOut.into();
        assert!(matches!(e, ApiError::Upstream("postgres")));
        let e: ApiError = sqlx::Error::RowNotFound.into();
        assert!(matches!(e, ApiError::NotFound(_)));
        let io =
            redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        let e: ApiError = io.into();
        assert!(matches!(e, ApiError::Upstream("redis")));
        let (status, _) = (ApiError::Upstream("redis").into_response().status(), ());
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
