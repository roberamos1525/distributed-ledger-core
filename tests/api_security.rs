//! HTTP integration tests for authentication, authorization, request limits,
//! error envelopes, health/readiness and metrics.

mod common;

use common::*;
use reqwest::{Method, StatusCode};
use serde_json::json;

#[tokio::test]
async fn health_and_ready_are_public() {
    let s = TestServer::start().await;
    let h = s.send(Method::GET, "/health", None, None, &[]).await;
    assert_eq!(h.status, StatusCode::OK);
    assert_eq!(h.body["status"], "ok");
    let r = s.send(Method::GET, "/ready", None, None, &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["status"], "ready");
}

#[tokio::test]
async fn missing_and_invalid_keys_are_rejected() {
    let s = TestServer::start().await;
    let missing = s.send(Method::GET, "/v1/accounts", None, None, &[]).await;
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
    assert_eq!(missing.body["error"]["code"], "unauthorized");
    let invalid = s.get("/v1/accounts", "ldg_not_a_real_key").await;
    assert_eq!(invalid.status, StatusCode::UNAUTHORIZED);
    // X-API-Key works as an alternative header.
    let alt = s
        .send(
            Method::GET,
            "/v1/accounts",
            None,
            None,
            &[("x-api-key", READER_KEY)],
        )
        .await;
    assert_eq!(alt.status, StatusCode::OK);
    // Unknown routes are also behind auth, then return the 404 envelope.
    let unknown = s.get("/v1/nothing-here", READER_KEY).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.body["error"]["code"], "not_found");
}

#[tokio::test]
async fn role_matrix() {
    let s = TestServer::start().await;
    s.open_account("cash", "asset", "USD").await;
    // reader: read-only
    assert_eq!(
        s.get("/v1/accounts/cash", READER_KEY).await.status,
        StatusCode::OK
    );
    assert_eq!(
        s.get("/v1/reports/trial-balance", READER_KEY).await.status,
        StatusCode::OK
    );
    let r = s
        .post(
            "/v1/accounts",
            READER_KEY,
            json!({"name": "x", "type": "asset", "currency": "USD"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.body["error"]["code"], "forbidden");
    // writer: cannot read the audit log or snapshot
    assert_eq!(
        s.get("/v1/audit", WRITER_KEY).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        s.send(
            Method::POST,
            "/v1/admin/snapshots",
            Some(WRITER_KEY),
            None,
            &[]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // admin: everything
    assert_eq!(s.get("/v1/audit", ADMIN_KEY).await.status, StatusCode::OK);
    let snap = s
        .send(
            Method::POST,
            "/v1/admin/snapshots",
            Some(ADMIN_KEY),
            None,
            &[],
        )
        .await;
    assert_eq!(snap.status, StatusCode::CREATED, "{}", snap.body);
    assert!(snap.body["sequence"].as_u64().unwrap() >= 1);
    let info = s.get("/v1/info", ADMIN_KEY).await;
    assert_eq!(info.body["principal"]["role"], "admin");
    assert_eq!(info.body["stats"]["accounts"], 1);
}

#[tokio::test]
async fn request_body_limits_and_content_types() {
    let s = TestServer::start_with(|c| c.server.max_body_bytes = 2048).await;
    let big = json!({"name": "x".repeat(4096), "type": "asset", "currency": "USD"});
    let r = s.post("/v1/accounts", WRITER_KEY, big).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(r.body["error"]["code"], "payload_too_large");

    let resp = s
        .client
        .post(format!("{}/v1/accounts", s.base))
        .header("authorization", format!("Bearer {WRITER_KEY}"))
        .header("content-type", "text/plain")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let resp = s
        .client
        .post(format!("{}/v1/accounts", s.base))
        .header("authorization", format!("Bearer {WRITER_KEY}"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("malformed JSON")
    );
}

#[tokio::test]
async fn method_not_allowed_uses_envelope() {
    let s = TestServer::start().await;
    let r = s
        .send(Method::DELETE, "/v1/accounts", Some(ADMIN_KEY), None, &[])
        .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.body["error"]["code"], "method_not_allowed");
}

#[tokio::test]
async fn request_ids_are_echoed_or_generated() {
    let s = TestServer::start().await;
    let r = s
        .send(
            Method::GET,
            "/health",
            None,
            None,
            &[("x-request-id", "trace-123")],
        )
        .await;
    assert_eq!(r.headers["x-request-id"], "trace-123");
    let generated = s.send(Method::GET, "/health", None, None, &[]).await;
    assert_eq!(
        generated.headers["x-request-id"].to_str().unwrap().len(),
        36
    );
}

#[tokio::test]
async fn metrics_exposition() {
    let s = TestServer::start().await;
    s.open_account("cash", "asset", "USD").await;
    s.get("/v1/accounts/cash", READER_KEY).await;
    s.get("/v1/accounts", "bogus").await;
    let r = s.send(Method::GET, "/metrics", None, None, &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    let text = r.body.as_str().unwrap();
    assert!(text.contains("# TYPE ledger_http_requests_total counter"));
    assert!(text.contains("route=\"/v1/accounts/{id}\""));
    assert!(text.contains("ledger_http_request_duration_seconds_bucket"));
    assert!(text.contains("ledger_events_appended_total{type=\"account_opened\"} 1"));
    assert!(text.contains("ledger_accounts 1"));
    assert!(text.contains("ledger_auth_failures_total{reason=\"invalid\"} 1"));
    assert!(text.contains("ledger_commands_total{command=\"open_account\",outcome=\"ok\"} 1"));
}

#[tokio::test]
async fn private_metrics_require_a_key() {
    let s = TestServer::start_with(|c| c.server.metrics_public = false).await;
    let anon = s.send(Method::GET, "/metrics", None, None, &[]).await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
    let authed = s.get("/metrics", READER_KEY).await;
    assert_eq!(authed.status, StatusCode::OK);
}

#[tokio::test]
async fn no_keys_configured_rejects_everything_but_health() {
    let s = TestServer::start_with(|c| c.auth.api_keys.clear()).await;
    assert_eq!(
        s.send(Method::GET, "/health", None, None, &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(
        s.get("/v1/accounts", ADMIN_KEY).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn invalid_path_parameters() {
    let s = TestServer::start().await;
    let r = s.get("/v1/transactions/not-a-uuid", READER_KEY).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = s.get("/v1/accounts/bad%20id", READER_KEY).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = s
        .get(
            "/v1/transactions/0190f2a4-0000-7000-8000-000000000000",
            READER_KEY,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body["error"]["code"], "transaction_not_found");
}
