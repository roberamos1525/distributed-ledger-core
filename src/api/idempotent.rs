//! Glue between handlers and the idempotency store.

use std::future::Future;

use axum::Json;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::api::AppState;
use crate::api::auth::Principal;
use crate::api::error::ApiError;
use crate::domain::transaction::IdempotencyTag;
use crate::idempotency::{self, Begin, StoredResponse};

/// Header carrying the client's idempotency key.
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Response header set when a stored response is replayed.
pub const REPLAYED_HEADER: &str = "idempotent-replayed";

/// Whether an error response should be remembered for the key. Transient or
/// caller-state-dependent failures are not stored so the client can retry.
fn cacheable_error(status: StatusCode) -> bool {
    status.is_client_error()
        && !matches!(
            status,
            StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::CONFLICT
                | StatusCode::REQUEST_TIMEOUT
                | StatusCode::TOO_MANY_REQUESTS
        )
}

fn respond(status: u16, body: Value, replayed: bool) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut response = (status, Json(body)).into_response();
    if replayed {
        response
            .headers_mut()
            .insert(REPLAYED_HEADER, HeaderValue::from_static("true"));
    }
    response
}

/// Runs `operation` at most once per (principal, idempotency key).
///
/// Without an `Idempotency-Key` header the operation simply runs. With one,
/// the request is fingerprinted (method, path, canonical body) and either
/// executed, replayed, or rejected as a conflicting reuse. The operation
/// receives the scoped key and fingerprint so that the engine can persist
/// them with the transaction.
pub async fn run<F, Fut>(
    state: &AppState,
    principal: &Principal,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: &Value,
    operation: F,
) -> Response
where
    F: FnOnce(Option<IdempotencyTag>) -> Fut,
    Fut: Future<Output = Result<(StatusCode, Value), ApiError>>,
{
    let key = match headers.get(IDEMPOTENCY_HEADER) {
        None => {
            return match operation(None).await {
                Ok((status, body)) => respond(status.as_u16(), body, false),
                Err(e) => e.into_response(),
            };
        }
        Some(raw) => match raw.to_str() {
            Ok(k) => k.to_string(),
            Err(_) => {
                return ApiError::bad_request("idempotency key must be ASCII").into_response();
            }
        },
    };
    if let Err(message) = idempotency::validate_key(&key) {
        return ApiError::bad_request(message).into_response();
    }
    let scoped = format!("{}:{}", principal.id, key);
    let fingerprint = idempotency::fingerprint(method, path, body);
    let metrics = state.metrics();
    let guard = match state.idempotency().begin(&scoped, &fingerprint) {
        Begin::Replay(stored) => {
            metrics.idempotency.with(&["replayed"]).inc();
            return respond(stored.status, stored.body, true);
        }
        Begin::Mismatch => {
            metrics.idempotency.with(&["conflict"]).inc();
            return ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_key_reused",
                "idempotency key was already used with a different request",
            )
            .into_response();
        }
        Begin::InProgress => {
            metrics.idempotency.with(&["in_progress"]).inc();
            return ApiError::new(
                StatusCode::CONFLICT,
                "request_in_progress",
                "a request with this idempotency key is still being processed",
            )
            .into_response();
        }
        Begin::Proceed(guard) => guard,
    };
    metrics.idempotency.with(&["new"]).inc();
    let tag = IdempotencyTag {
        key: scoped,
        fingerprint,
    };
    let (status, body, store) = match operation(Some(tag)).await {
        Ok((status, body)) => (status, body, true),
        Err(e) => {
            let store = cacheable_error(e.status);
            (e.status, e.body(), store)
        }
    };
    if store {
        if let Err(e) = guard.complete(StoredResponse {
            status: status.as_u16(),
            body: body.clone(),
        }) {
            tracing::error!(error = %e, "failed to persist idempotency record");
        }
    } else {
        guard.abandon();
    }
    respond(status.as_u16(), body, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_caching_policy() {
        assert!(cacheable_error(StatusCode::BAD_REQUEST));
        assert!(cacheable_error(StatusCode::UNPROCESSABLE_ENTITY));
        assert!(cacheable_error(StatusCode::NOT_FOUND));
        assert!(!cacheable_error(StatusCode::CONFLICT));
        assert!(!cacheable_error(StatusCode::UNAUTHORIZED));
        assert!(!cacheable_error(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!cacheable_error(StatusCode::SERVICE_UNAVAILABLE));
    }
}
