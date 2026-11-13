//! Input extractors whose rejections are `DemoError` JSON bodies instead of axum's plain text.
use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, OptionalFromRequest, Path, Query, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::DemoError;

pub struct JsonIn<T>(pub T);
pub struct PathIn<T>(pub T);
pub struct QueryIn<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonIn<T> {
    type Rejection = Response;
    async fn from_request(req: Request, state: &S) -> Result<Self, Response> {
        match <Json<T> as FromRequest<S>>::from_request(req, state).await {
            Ok(Json(v)) => Ok(JsonIn(v)),
            Err(e) => Err(json_rejection(e)),
        }
    }
}

/// `Option<JsonIn<T>>`: no body / no JSON content-type is `None`; a malformed JSON body is still the 422.
impl<S: Send + Sync, T: DeserializeOwned> OptionalFromRequest<S> for JsonIn<T> {
    type Rejection = Response;
    async fn from_request(req: Request, state: &S) -> Result<Option<Self>, Response> {
        match <Json<T> as OptionalFromRequest<S>>::from_request(req, state).await {
            Ok(v) => Ok(v.map(|Json(v)| JsonIn(v))),
            Err(e) => Err(json_rejection(e)),
        }
    }
}

fn json_rejection(e: JsonRejection) -> Response {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": {"code": "payload_too_large", "message": e.body_text()}})),
        )
            .into_response();
    }
    DemoError::Invalid(e.body_text()).into_response()
}

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for PathIn<T> {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Response> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(v)| PathIn(v))
            .map_err(|e: PathRejection| {
                DemoError::Invalid(format!("invalid path: {}", e.body_text())).into_response()
            })
    }
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for QueryIn<T> {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Response> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| QueryIn(v))
            .map_err(|e: QueryRejection| {
                DemoError::Invalid(format!("invalid query: {}", e.body_text())).into_response()
            })
    }
}
