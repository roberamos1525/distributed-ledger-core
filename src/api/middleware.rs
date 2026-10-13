//! Cross-cutting HTTP middleware: request ids, access logging and metrics.

use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

use crate::api::AppState;
use crate::api::auth::Principal;

/// Header carrying the request id in both directions.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Request id stored in request extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(pub String);

/// Accepts a client-supplied id if it is short and printable, otherwise
/// generates a new one.
fn sanitize_request_id(raw: Option<&HeaderValue>) -> String {
    raw.and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Outermost middleware: assigns the request id and echoes it back.
pub async fn request_id(mut req: Request, next: Next) -> Response {
    let id = sanitize_request_id(req.headers().get(REQUEST_ID_HEADER));
    req.extensions_mut().insert(RequestId(id.clone()));
    let mut response = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// Route-level middleware: latency histogram, request counter, in-flight
/// gauge and a structured access log line.
pub async fn observe(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = req.method().as_str().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_default();
    let principal = req
        .extensions()
        .get::<Principal>()
        .map(|p| p.id.clone())
        .unwrap_or_else(|| "-".into());
    let metrics = state.metrics().clone();
    metrics.http_in_flight.inc();
    let response = next.run(req).await;
    metrics.http_in_flight.dec();
    let elapsed = started.elapsed();
    let status = response.status().as_u16().to_string();
    metrics
        .http_requests
        .with(&[&method, &route, &status])
        .inc();
    metrics
        .http_latency
        .with(&[&method, &route])
        .observe(elapsed.as_secs_f64());
    tracing::info!(
        target: "ledger_core::access",
        %method,
        %route,
        status = response.status().as_u16(),
        latency_ms = elapsed.as_secs_f64() * 1000.0,
        %request_id,
        %principal,
        "request"
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_sanitized() {
        let ok = HeaderValue::from_static("abc-123");
        assert_eq!(sanitize_request_id(Some(&ok)), "abc-123");
        let spaced = HeaderValue::from_static("has space");
        assert_ne!(sanitize_request_id(Some(&spaced)), "has space");
        let long = HeaderValue::from_str(&"a".repeat(200)).unwrap();
        assert_eq!(sanitize_request_id(Some(&long)).len(), 36);
        assert_eq!(sanitize_request_id(None).len(), 36);
    }
}
