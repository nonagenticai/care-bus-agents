//! DemoError → HTTP. Body `{"error":{"code","message"}}`; 500s never leak internals.
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum DemoError {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{message}")]
    Conflict { code: &'static str, message: String },
    #[error("{0}")]
    ReadOnly(String),
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("database unavailable")]
    DbUnavailable,
    #[error("missing or wrong bearer token")]
    Unauthorized,
    #[error("injected fault on {0}")]
    FaultInjected(String),
    #[error("config: {0}")]
    Config(String),
    #[error("database: {0}")]
    Db(sqlx::Error),
}

/// Pool exhaustion / unreachable server is "unavailable" (503), not an internal bug.
impl From<sqlx::Error> for DemoError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => {
                DemoError::DbUnavailable
            }
            other => DemoError::Db(other),
        }
    }
}

pub type Result<T> = std::result::Result<T, DemoError>;

impl DemoError {
    pub fn code(&self) -> &'static str {
        match self {
            DemoError::NotFound(_) => "not_found",
            DemoError::Invalid(_) => "invalid_input",
            DemoError::Conflict { code, .. } => code,
            DemoError::ReadOnly(_) => "read_only_table",
            DemoError::MethodNotAllowed => "method_not_allowed",
            DemoError::DbUnavailable => "db_unavailable",
            DemoError::Unauthorized => "unauthorized",
            DemoError::FaultInjected(_) => "fault_injected",
            DemoError::Config(_) | DemoError::Db(_) => "internal",
        }
    }
    pub fn status(&self) -> StatusCode {
        match self {
            DemoError::NotFound(_) => StatusCode::NOT_FOUND,
            DemoError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            DemoError::Conflict { .. } => StatusCode::CONFLICT,
            DemoError::ReadOnly(_) | DemoError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            DemoError::DbUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            DemoError::Unauthorized => StatusCode::UNAUTHORIZED,
            DemoError::FaultInjected(_) => StatusCode::SERVICE_UNAVAILABLE,
            DemoError::Config(_) | DemoError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    /// Postgres class 22 (data exception) and 23502/23514 (not-null / check) are caller mistakes → 422.
    pub fn from_db(e: sqlx::Error) -> DemoError {
        if let sqlx::Error::Database(d) = &e {
            let code = d.code().map(|c| c.to_string()).unwrap_or_default();
            if code.starts_with("22") || code == "23502" || code == "23514" {
                return DemoError::Invalid(d.message().to_string());
            }
        }
        e.into()
    }
}

impl IntoResponse for DemoError {
    fn into_response(self) -> Response {
        let status = self.status();
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "request failed");
            "internal server error".to_string()
        } else {
            self.to_string()
        };
        (
            status,
            Json(json!({"error": {"code": self.code(), "message": message}})),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::DemoError;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::Value;

    async fn parts(e: DemoError) -> (StatusCode, Value) {
        let r = e.into_response();
        let s = r.status();
        let b = to_bytes(r.into_body(), usize::MAX).await.unwrap();
        (s, serde_json::from_slice(&b).unwrap())
    }

    #[tokio::test]
    async fn statuses_and_codes() {
        let cases = [
            (DemoError::NotFound("order ORD-1".into()), 404, "not_found"),
            (DemoError::Invalid("bad".into()), 422, "invalid_input"),
            (
                DemoError::Conflict {
                    code: "refund_exceeds_capture",
                    message: "x".into(),
                },
                409,
                "refund_exceeds_capture",
            ),
            (DemoError::Unauthorized, 401, "unauthorized"),
            (
                DemoError::FaultInjected("delivery_track_shipment".into()),
                503,
                "fault_injected",
            ),
        ];
        for (e, status, code) in cases {
            let (s, v) = parts(e).await;
            assert_eq!(s.as_u16(), status);
            assert_eq!(v["error"]["code"], code);
            assert!(
                v["error"]["message"]
                    .as_str()
                    .is_some_and(|m| !m.is_empty())
            );
        }
    }

    #[tokio::test]
    async fn db_errors_hide_details() {
        let (s, v) = parts(DemoError::Db(sqlx::Error::RowNotFound)).await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(v["error"]["message"], "internal server error");
    }

    #[tokio::test]
    async fn unreachable_db_is_503_db_unavailable() {
        let io = || sqlx::Error::Io(std::io::Error::other("secret host 10.0.0.1 refused"));
        for e in [sqlx::Error::PoolTimedOut, sqlx::Error::PoolClosed, io()] {
            let (s, v) = parts(DemoError::from(e)).await;
            assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(v["error"]["code"], "db_unavailable");
            assert_eq!(v["error"]["message"], "database unavailable");
        }
        let (s, _) = parts(DemoError::from(sqlx::Error::RowNotFound)).await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn method_not_allowed_is_405_json() {
        let (s, v) = parts(DemoError::MethodNotAllowed).await;
        assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(v["error"]["code"], "method_not_allowed");
    }
}
