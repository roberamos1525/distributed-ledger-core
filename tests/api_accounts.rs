//! HTTP integration tests for account management.

mod common;

use common::*;
use reqwest::{Method, StatusCode};
use serde_json::json;

#[tokio::test]
async fn create_get_and_list_accounts() {
    let s = TestServer::start().await;
    let created = s.open_account("assets:cash", "asset", "USD").await;
    assert_eq!(created["id"], "assets:cash");
    assert_eq!(created["status"], "open");
    assert_eq!(created["version"], 1);
    assert_eq!(created["policy"]["kind"], "no_overdraft");
    assert_eq!(created["balance"]["posted"], 0);

    s.open_account("equity:capital", "equity", "USD").await;
    s.open_account("eur:cash", "asset", "EUR").await;

    let got = s.get("/v1/accounts/assets:cash", READER_KEY).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.body["currency"], "USD");

    let all = s.get("/v1/accounts?limit=2", READER_KEY).await;
    assert_eq!(all.body["data"].as_array().unwrap().len(), 2);
    let cursor = all.body["next_cursor"].as_str().unwrap().to_string();
    let rest = s
        .get(&format!("/v1/accounts?limit=2&cursor={cursor}"), READER_KEY)
        .await;
    assert_eq!(rest.body["data"].as_array().unwrap().len(), 1);
    assert!(rest.body["next_cursor"].is_null());

    let eur = s.get("/v1/accounts?currency=EUR", READER_KEY).await;
    assert_eq!(eur.body["data"].as_array().unwrap().len(), 1);
    let equity = s
        .get("/v1/accounts?type=equity&status=open", READER_KEY)
        .await;
    assert_eq!(equity.body["data"][0]["id"], "equity:capital");
}

#[tokio::test]
async fn generated_ids_policies_and_metadata() {
    let s = TestServer::start().await;
    let r = s
        .post(
            "/v1/accounts",
            WRITER_KEY,
            json!({
                "name": "Customer wallet",
                "type": "liability",
                "currency": "JPY",
                "policy": {"kind": "overdraft", "limit": 500},
                "metadata": {"customer": "c-42"}
            }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let id = r.body["id"].as_str().unwrap();
    assert!(id.starts_with("acc_"));
    assert_eq!(r.body["policy"]["limit"], 500);
    assert_eq!(r.body["metadata"]["customer"], "c-42");
}

#[tokio::test]
async fn account_validation_errors() {
    let s = TestServer::start().await;
    let cases = [
        json!({"name": "x", "type": "asset", "currency": "usd"}),
        json!({"name": "x", "type": "asset", "currency": "XXX"}),
        json!({"name": "x", "type": "piggybank", "currency": "USD"}),
        json!({"name": " ", "type": "asset", "currency": "USD"}),
        json!({"id": "has space", "name": "x", "type": "asset", "currency": "USD"}),
        json!({"name": "x", "type": "asset", "currency": "USD", "policy": {"kind": "overdraft", "limit": -1}}),
        json!({"name": "x", "type": "asset", "currency": "USD", "extra": true}),
        json!({"name": "x", "type": "asset", "currency": "USD", "metadata": {"bad key": "v"}}),
    ];
    for body in cases {
        let r = s.post("/v1/accounts", WRITER_KEY, body.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body} -> {}", r.body);
        assert_eq!(r.body["error"]["code"], "validation_error");
    }
    s.open_account("dup", "asset", "USD").await;
    let r = s
        .post(
            "/v1/accounts",
            WRITER_KEY,
            json!({"id": "dup", "name": "again", "type": "asset", "currency": "USD"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.body["error"]["code"], "account_exists");
    let missing = s.get("/v1/accounts/nope", READER_KEY).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.body["error"]["code"], "account_not_found");
}

#[tokio::test]
async fn status_lifecycle_with_versions() {
    let s = TestServer::start().await;
    s.open_account("cash", "asset", "USD").await;
    s.open_account("equity", "equity", "USD").await;

    // Writers may not change status; admins may.
    let forbidden = s
        .post(
            "/v1/accounts/cash/status",
            WRITER_KEY,
            json!({"status": "frozen"}),
        )
        .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);

    let stale = s
        .post(
            "/v1/accounts/cash/status",
            ADMIN_KEY,
            json!({"status": "frozen", "expected_version": 7}),
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    assert_eq!(stale.body["error"]["code"], "version_conflict");
    assert_eq!(stale.body["error"]["details"]["actual_version"], 1);

    let frozen = s
        .post(
            "/v1/accounts/cash/status",
            ADMIN_KEY,
            json!({"status": "frozen", "expected_version": 1, "reason": "kyc review"}),
        )
        .await;
    assert_eq!(frozen.status, StatusCode::OK, "{}", frozen.body);
    assert_eq!(frozen.body["status"], "frozen");
    assert_eq!(frozen.body["version"], 2);

    let blocked = s.transfer("cash", "equity", 100).await;
    assert_eq!(blocked.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(blocked.body["error"]["code"], "account_not_active");

    let reopened = s
        .post(
            "/v1/accounts/cash/status",
            ADMIN_KEY,
            json!({"status": "open"}),
        )
        .await;
    assert_eq!(reopened.status, StatusCode::OK);
    assert_eq!(
        s.transfer("cash", "equity", 100).await.status,
        StatusCode::CREATED
    );

    // Closing requires a zero balance.
    let not_empty = s
        .post(
            "/v1/accounts/cash/status",
            ADMIN_KEY,
            json!({"status": "closed"}),
        )
        .await;
    assert_eq!(not_empty.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(not_empty.body["error"]["code"], "account_not_empty");
    assert_eq!(
        s.transfer("equity", "cash", 100).await.status,
        StatusCode::CREATED
    );
    let closed = s
        .post(
            "/v1/accounts/cash/status",
            ADMIN_KEY,
            json!({"status": "closed"}),
        )
        .await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.body);
    let terminal = s
        .post(
            "/v1/accounts/cash/status",
            ADMIN_KEY,
            json!({"status": "open"}),
        )
        .await;
    assert_eq!(terminal.body["error"]["code"], "invalid_status_transition");
}

#[tokio::test]
async fn metadata_and_policy_updates() {
    let s = TestServer::start().await;
    s.open_account("wallet", "liability", "USD").await;
    let r = s
        .send(
            Method::PATCH,
            "/v1/accounts/wallet/metadata",
            Some(WRITER_KEY),
            Some(json!({"set": {"tier": "gold", "region": "eu"}, "expected_version": 1})),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["metadata"]["tier"], "gold");
    let r = s
        .send(
            Method::PATCH,
            "/v1/accounts/wallet/metadata",
            Some(WRITER_KEY),
            Some(json!({"remove": ["region"]})),
            &[],
        )
        .await;
    assert!(r.body["metadata"].get("region").is_none());
    assert_eq!(r.body["version"], 3);

    let as_writer = s
        .send(
            Method::PUT,
            "/v1/accounts/wallet/policy",
            Some(WRITER_KEY),
            Some(json!({"policy": {"kind": "unrestricted"}})),
            &[],
        )
        .await;
    assert_eq!(as_writer.status, StatusCode::FORBIDDEN);
    let r = s
        .send(
            Method::PUT,
            "/v1/accounts/wallet/policy",
            Some(ADMIN_KEY),
            Some(json!({"policy": {"kind": "overdraft", "limit": 1000}, "expected_version": 3})),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["policy"]["kind"], "overdraft");
}

#[tokio::test]
async fn overdraft_policy_is_enforced_per_account() {
    let s = TestServer::start().await;
    s.open_account("bank", "asset", "USD").await;
    s.post(
        "/v1/accounts",
        WRITER_KEY,
        json!({"id": "wallet", "name": "w", "type": "liability", "currency": "USD",
               "policy": {"kind": "overdraft", "limit": 300}}),
    )
    .await;
    // Debiting a liability reduces it; the overdraft allows -300.
    assert_eq!(
        s.transfer("wallet", "bank", 300).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    s.open_account("equity", "equity", "USD").await;
    assert_eq!(
        s.transfer("bank", "equity", 1_000).await.status,
        StatusCode::CREATED
    );
    let ok = s.transfer("wallet", "bank", 300).await;
    // bank (asset) is credited 300 from 1000: fine; wallet goes to -300: allowed.
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.body);
    let over = s.transfer("wallet", "bank", 1).await;
    assert_eq!(over.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(over.body["error"]["code"], "insufficient_funds");
    assert_eq!(over.body["error"]["details"]["account_id"], "wallet");
    assert_eq!(s.posted_balance("wallet").await, -300);
}
