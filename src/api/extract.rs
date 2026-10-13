//! Extractors that turn malformed input into the standard error envelope.

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, OptionalFromRequest, Query, Request};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::api::error::ApiError;

/// A JSON request body, parsed both as `T` and as a raw [`Value`] (the raw
/// form is used to fingerprint idempotent requests).
pub struct JsonBody<T> {
    pub value: T,
    pub raw: Value,
}

fn is_json(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|ct| ct.split(';').next())
        .map(|mime| {
            let mime = mime.trim().to_ascii_lowercase();
            mime == "application/json"
                || (mime.starts_with("application/") && mime.ends_with("+json"))
        })
        .unwrap_or(false)
}

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let content_type = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if !is_json(content_type.as_deref()) {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "expected Content-Type: application/json",
            ));
        }
        let bytes = Bytes::from_request(req, state).await.map_err(|rejection| {
            if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "request body exceeds the configured limit",
                )
            } else {
                ApiError::bad_request(format!(
                    "could not read request body: {}",
                    rejection.body_text()
                ))
            }
        })?;
        let raw: Value = serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::bad_request(format!("malformed JSON: {e}")))?;
        let value = serde_json::from_value(raw.clone())
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
        Ok(JsonBody { value, raw })
    }
}

/// Optional JSON body: a request without `Content-Type` and with an empty
/// body yields `None`; anything else must be valid JSON.
impl<S, T> OptionalFromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        if req.headers().contains_key(CONTENT_TYPE) {
            return <Self as FromRequest<S>>::from_request(req, state)
                .await
                .map(Some);
        }
        let bytes = Bytes::from_request(req, state).await.map_err(|r| {
            ApiError::bad_request(format!("could not read request body: {}", r.body_text()))
        })?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            Ok(None)
        } else {
            Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "expected Content-Type: application/json",
            ))
        }
    }
}

/// Query string parameters with enveloped errors.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| ApiQuery(v))
            .map_err(|e| ApiError::bad_request(format!("invalid query string: {}", e.body_text())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_content_types() {
        assert!(is_json(Some("application/json")));
        assert!(is_json(Some("application/json; charset=utf-8")));
        assert!(is_json(Some("Application/JSON")));
        assert!(is_json(Some("application/merge-patch+json")));
        assert!(!is_json(Some("text/plain")));
        assert!(!is_json(None));
    }
}
