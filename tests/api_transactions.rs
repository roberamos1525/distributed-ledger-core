//! HTTP integration tests for transactions, two-phase transfers, reversals,
//! idempotency, balances and history.

mod common;

use common::*;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

async fn funded() -> TestServer {
    let s = TestServer::start().await;
    s.open_account("cash", "asset", "USD").await;
    s.open_account("equity", "equity", "USD").await;
    s.open_account("revenue", "revenue", "USD").await;
    s.open_account("wallet", "liability", "USD").await;
    assert_eq!(
        s.transfer("cash", "equity", 100_000).await.status,
        StatusCode::CREATED
    );
    s
}

#[tokio::test]
async fn multi_leg_transaction_with_decimal_amounts() {
    let s = funded().await;
    s.open_account("fees", "revenue", "USD").await;
    let r = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({
                "description": "Card top-up with fee",
                "reference": "TOPUP-1",
                "metadata": {"channel": "card"},
                "postings": [
                    {"account_id": "cash", "direction": "debit", "amount": "102.50"},
                    {"account_id": "wallet", "direction": "credit", "amount": "100.00", "currency": "USD"},
                    {"account_id": "fees", "direction": "credit", "amount": 250}
                ]
            }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["status"], "posted");
    assert_eq!(r.body["postings"][0]["amount"], 10_250);
    assert_eq!(r.body["reference"], "TOPUP-1");
    assert_eq!(s.posted_balance("wallet").await, 10_000);
    assert_eq!(s.posted_balance("fees").await, 250);

    let id = r.body["id"].as_str().unwrap();
    let got = s.get(&format!("/v1/transactions/{id}"), READER_KEY).await;
    assert_eq!(got.body["description"], "Card top-up with fee");
}

#[tokio::test]
async fn transaction_rejections() {
    let s = funded().await;
    s.open_account("eur", "asset", "EUR").await;
    let unbalanced = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"postings": [
                {"account_id": "cash", "direction": "debit", "amount": 100},
                {"account_id": "revenue", "direction": "credit", "amount": 99}
            ]}),
        )
        .await;
    assert_eq!(unbalanced.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(unbalanced.body["error"]["code"], "unbalanced_transaction");
    assert_eq!(unbalanced.body["error"]["details"]["debits"], 100);

    let cross_currency = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"postings": [
                {"account_id": "eur", "direction": "debit", "amount": 100},
                {"account_id": "revenue", "direction": "credit", "amount": 100}
            ]}),
        )
        .await;
    assert_eq!(
        cross_currency.body["error"]["code"],
        "unbalanced_transaction"
    );

    let wrong_currency = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"postings": [
                {"account_id": "cash", "direction": "debit", "amount": 100, "currency": "EUR"},
                {"account_id": "revenue", "direction": "credit", "amount": 100}
            ]}),
        )
        .await;
    assert_eq!(wrong_currency.body["error"]["code"], "currency_mismatch");

    let too_precise = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"postings": [
                {"account_id": "cash", "direction": "debit", "amount": "1.001"},
                {"account_id": "revenue", "direction": "credit", "amount": "1.001"}
            ]}),
        )
        .await;
    assert_eq!(too_precise.status, StatusCode::BAD_REQUEST);

    let unknown = s.transfer("cash", "ghost", 1).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let single_leg = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"postings": [{"account_id": "cash", "direction": "debit", "amount": 1}]}),
        )
        .await;
    assert_eq!(single_leg.status, StatusCode::BAD_REQUEST);
    let negative = s.transfer("cash", "revenue", -5).await;
    assert_eq!(negative.status, StatusCode::BAD_REQUEST);
    let reader = s
        .send(
            Method::POST,
            "/v1/transactions",
            Some(READER_KEY),
            Some(json!({"postings": []})),
            &[],
        )
        .await;
    assert_eq!(reader.status, StatusCode::FORBIDDEN);
    // Nothing moved.
    assert_eq!(s.posted_balance("revenue").await, 0);
}

#[tokio::test]
async fn two_phase_transfer_post_and_void() {
    let s = funded().await;
    let pending = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"mode": "pending", "pending_ttl_secs": 3600, "postings": [
                {"account_id": "wallet", "direction": "credit", "amount": 40_000},
                {"account_id": "cash", "direction": "debit", "amount": 40_000}
            ]}),
        )
        .await;
    assert_eq!(pending.status, StatusCode::CREATED, "{}", pending.body);
    assert_eq!(pending.body["status"], "pending");
    assert!(pending.body["expires_at"].is_string());
    let id = pending.body["id"].as_str().unwrap().to_string();

    let bal = s.get("/v1/accounts/wallet/balance", READER_KEY).await;
    assert_eq!(bal.body["posted"], 0);
    assert_eq!(bal.body["pending_credits"], 40_000);

    let posted = s
        .send(
            Method::POST,
            &format!("/v1/transactions/{id}/post"),
            Some(WRITER_KEY),
            None,
            &[],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert_eq!(posted.body["status"], "posted");
    assert_eq!(s.posted_balance("wallet").await, 40_000);
    let again = s
        .send(
            Method::POST,
            &format!("/v1/transactions/{id}/post"),
            Some(WRITER_KEY),
            None,
            &[],
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.body["error"]["code"], "invalid_transaction_state");

    // Reserve, then void: available goes down and back up.
    let hold = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"mode": "pending", "postings": [
                {"account_id": "wallet", "direction": "debit", "amount": 30_000},
                {"account_id": "revenue", "direction": "credit", "amount": 30_000}
            ]}),
        )
        .await;
    assert_eq!(hold.status, StatusCode::CREATED, "{}", hold.body);
    let bal = s.get("/v1/accounts/wallet/balance", READER_KEY).await;
    assert_eq!(bal.body["available"], 10_000);
    // A second hold beyond the available balance fails.
    let too_much = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"mode": "pending", "postings": [
                {"account_id": "wallet", "direction": "debit", "amount": 10_001},
                {"account_id": "revenue", "direction": "credit", "amount": 10_001}
            ]}),
        )
        .await;
    assert_eq!(too_much.body["error"]["code"], "insufficient_funds");
    let hold_id = hold.body["id"].as_str().unwrap();
    let voided = s
        .post(
            &format!("/v1/transactions/{hold_id}/void"),
            WRITER_KEY,
            json!({"reason": "customer cancelled"}),
        )
        .await;
    assert_eq!(voided.status, StatusCode::OK, "{}", voided.body);
    assert_eq!(voided.body["status"], "voided");
    assert_eq!(voided.body["void_reason"], "customer cancelled");
    let bal = s.get("/v1/accounts/wallet/balance", READER_KEY).await;
    assert_eq!(bal.body["available"], 40_000);
    assert_eq!(bal.body["posted"], 40_000);

    let pending_list = s.get("/v1/transactions?status=voided", READER_KEY).await;
    assert_eq!(pending_list.body["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn reversals_full_partial_and_double() {
    let s = funded().await;
    let sale = s.transfer("cash", "revenue", 5_000).await;
    let id = sale.body["id"].as_str().unwrap().to_string();

    let partial = s
        .post(
            &format!("/v1/transactions/{id}/reversals"),
            WRITER_KEY,
            json!({"legs": [
                {"posting_index": 0, "amount": "10.00"},
                {"posting_index": 1, "amount": 1_000}
            ], "description": "partial refund"}),
        )
        .await;
    assert_eq!(partial.status, StatusCode::CREATED, "{}", partial.body);
    assert_eq!(partial.body["kind"]["type"], "reversal");
    assert_eq!(partial.body["kind"]["original"], id.as_str());
    assert_eq!(s.posted_balance("revenue").await, 4_000);

    let original = s.get(&format!("/v1/transactions/{id}"), READER_KEY).await;
    assert_eq!(original.body["reversed_amounts"], json!([1_000, 1_000]));
    assert_eq!(original.body["reversals"].as_array().unwrap().len(), 1);

    let over = s
        .post(
            &format!("/v1/transactions/{id}/reversals"),
            WRITER_KEY,
            json!({"legs": [
                {"posting_index": 0, "amount": 4_001},
                {"posting_index": 1, "amount": 4_001}
            ]}),
        )
        .await;
    assert_eq!(over.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(over.body["error"]["code"], "invalid_reversal");

    let rest = s
        .send(
            Method::POST,
            &format!("/v1/transactions/{id}/reversals"),
            Some(WRITER_KEY),
            None,
            &[],
        )
        .await;
    assert_eq!(rest.status, StatusCode::CREATED, "{}", rest.body);
    assert_eq!(rest.body["postings"][0]["amount"], 4_000);
    assert_eq!(s.posted_balance("revenue").await, 0);

    let double = s
        .send(
            Method::POST,
            &format!("/v1/transactions/{id}/reversals"),
            Some(WRITER_KEY),
            None,
            &[],
        )
        .await;
    assert_eq!(double.status, StatusCode::CONFLICT);
    assert_eq!(double.body["error"]["code"], "already_reversed");

    let reversal_id = rest.body["id"].as_str().unwrap();
    let of_reversal = s
        .send(
            Method::POST,
            &format!("/v1/transactions/{reversal_id}/reversals"),
            Some(WRITER_KEY),
            None,
            &[],
        )
        .await;
    assert_eq!(of_reversal.body["error"]["code"], "invalid_reversal");
}

#[tokio::test]
async fn idempotent_transaction_creation() {
    let s = funded().await;
    let body = json!({"reference": "PAY-1", "postings": [
        {"account_id": "cash", "direction": "debit", "amount": 700},
        {"account_id": "revenue", "direction": "credit", "amount": 700}
    ]});
    let first = s
        .post_idem("/v1/transactions", WRITER_KEY, body.clone(), "pay-1")
        .await;
    assert_eq!(first.status, StatusCode::CREATED);
    let second = s
        .post_idem("/v1/transactions", WRITER_KEY, body.clone(), "pay-1")
        .await;
    assert_eq!(second.status, StatusCode::CREATED);
    assert_eq!(second.headers["idempotent-replayed"], "true");
    assert_eq!(first.body["id"], second.body["id"]);
    assert_eq!(s.posted_balance("revenue").await, 700);

    // Same key, different payload: conflict.
    let mut changed = body.clone();
    changed["postings"][0]["amount"] = json!(701);
    changed["postings"][1]["amount"] = json!(701);
    let conflict = s
        .post_idem("/v1/transactions", WRITER_KEY, changed, "pay-1")
        .await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(conflict.body["error"]["code"], "idempotency_key_reused");

    // Keys are scoped per API key: the admin may reuse "pay-1".
    let other = s
        .post_idem("/v1/transactions", ADMIN_KEY, body.clone(), "pay-1")
        .await;
    assert_eq!(other.status, StatusCode::CREATED);
    assert_ne!(other.body["id"], first.body["id"]);

    // Business errors are replayed too (same answer for the same request).
    let failing = json!({"postings": [
        {"account_id": "wallet", "direction": "debit", "amount": 1},
        {"account_id": "cash", "direction": "credit", "amount": 1}
    ]});
    let e1 = s
        .post_idem("/v1/transactions", WRITER_KEY, failing.clone(), "bad-1")
        .await;
    let e2 = s
        .post_idem("/v1/transactions", WRITER_KEY, failing, "bad-1")
        .await;
    assert_eq!(e1.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e2.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e2.headers["idempotent-replayed"], "true");

    let invalid_key = s
        .post_idem("/v1/transactions", WRITER_KEY, body, "has space")
        .await;
    assert_eq!(invalid_key.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn idempotency_survives_restart() {
    let s = funded().await;
    let body = json!({"postings": [
        {"account_id": "cash", "direction": "debit", "amount": 123},
        {"account_id": "revenue", "direction": "credit", "amount": 123}
    ]});
    let first = s
        .post_idem("/v1/transactions", WRITER_KEY, body.clone(), "restart-1")
        .await;
    let dir = s.stop().await;
    let s = TestServer::start_in(dir, |_| {}).await;
    let replay = s
        .post_idem("/v1/transactions", WRITER_KEY, body, "restart-1")
        .await;
    assert_eq!(replay.headers["idempotent-replayed"], "true");
    assert_eq!(replay.body["id"], first.body["id"]);
    assert_eq!(s.posted_balance("revenue").await, 123);
}

#[tokio::test]
async fn optimistic_concurrency_on_transactions() {
    let s = funded().await;
    let v = s.get("/v1/accounts/revenue", READER_KEY).await.body["version"]
        .as_u64()
        .unwrap();
    let ok = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"expected_versions": {"revenue": v}, "postings": [
                {"account_id": "cash", "direction": "debit", "amount": 1},
                {"account_id": "revenue", "direction": "credit", "amount": 1}
            ]}),
        )
        .await;
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.body);
    let stale = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"expected_versions": {"revenue": v}, "postings": [
                {"account_id": "cash", "direction": "debit", "amount": 1},
                {"account_id": "revenue", "direction": "credit", "amount": 1}
            ]}),
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    assert_eq!(stale.body["error"]["details"]["expected_version"], v);
}

#[tokio::test]
async fn point_in_time_balances_and_history_pagination() {
    let s = funded().await;
    let mut sequences = Vec::new();
    for amount in [100, 200, 300, 400] {
        let r = s.transfer("cash", "revenue", amount).await;
        sequences.push(r.body["created_sequence"].as_u64().unwrap());
    }
    let at = |seq: u64| format!("/v1/accounts/revenue/balance?as_of_sequence={seq}");
    assert_eq!(
        s.get(&at(sequences[0]), READER_KEY).await.body["posted"],
        100
    );
    assert_eq!(
        s.get(&at(sequences[2]), READER_KEY).await.body["posted"],
        600
    );
    assert_eq!(
        s.get(&at(sequences[0] - 1), READER_KEY).await.body["posted"],
        0
    );
    let latest = s.get("/v1/accounts/revenue/balance", READER_KEY).await;
    assert_eq!(latest.body["posted"], 1_000);
    assert_eq!(latest.body["posted_decimal"], "10.00");
    let beyond = s.get(&at(10_000), READER_KEY).await;
    assert_eq!(beyond.status, StatusCode::BAD_REQUEST);
    let future = s
        .get(
            "/v1/accounts/revenue/balance?as_of_time=2999-01-01T00:00:00Z",
            READER_KEY,
        )
        .await;
    assert_eq!(future.body["posted"], 1_000);
    let past = s
        .get(
            "/v1/accounts/revenue/balance?as_of_time=2000-01-01T00:00:00Z",
            READER_KEY,
        )
        .await;
    assert_eq!(past.body["posted"], 0);
    let both = s
        .get(
            "/v1/accounts/revenue/balance?as_of_sequence=1&as_of_time=2000-01-01T00:00:00Z",
            READER_KEY,
        )
        .await;
    assert_eq!(both.status, StatusCode::BAD_REQUEST);

    // Ascending pages of 3 then 1.
    let page1 = s
        .get("/v1/accounts/revenue/history?limit=3", READER_KEY)
        .await;
    let items: Vec<i64> = page1.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["amount"].as_i64().unwrap())
        .collect();
    assert_eq!(items, vec![100, 200, 300]);
    let cursor = page1.body["next_cursor"].as_str().unwrap();
    let page2 = s
        .get(
            &format!("/v1/accounts/revenue/history?limit=3&cursor={cursor}"),
            READER_KEY,
        )
        .await;
    assert_eq!(page2.body["data"][0]["amount"], 400);
    assert_eq!(page2.body["data"][0]["posted_balance"], 1_000);
    assert!(page2.body["next_cursor"].is_null());
    let newest = s
        .get(
            "/v1/accounts/revenue/history?limit=1&order=desc",
            READER_KEY,
        )
        .await;
    assert_eq!(newest.body["data"][0]["amount"], 400);
    let bad_cursor = s
        .get("/v1/accounts/revenue/history?cursor=zzzz", READER_KEY)
        .await;
    assert_eq!(bad_cursor.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn transaction_listing_filters_and_cursor() {
    let s = funded().await;
    for i in 0..5 {
        let r = s
            .post(
                "/v1/transactions",
                WRITER_KEY,
                json!({"reference": format!("REF-{i}"), "postings": [
                    {"account_id": "cash", "direction": "debit", "amount": 10},
                    {"account_id": "revenue", "direction": "credit", "amount": 10}
                ]}),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED);
    }
    let mut seen = 0;
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/v1/transactions?account_id=revenue&limit=2&cursor={c}"),
            None => "/v1/transactions?account_id=revenue&limit=2".to_string(),
        };
        let page = s.get(&path, READER_KEY).await;
        seen += page.body["data"].as_array().unwrap().len();
        match page.body["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, 5);
    let by_ref = s.get("/v1/transactions?reference=REF-3", READER_KEY).await;
    let data: &Vec<Value> = by_ref.body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    let bad_limit = s.get("/v1/transactions?limit=0", READER_KEY).await;
    assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);
    let bad_status = s.get("/v1/transactions?status=done", READER_KEY).await;
    assert_eq!(bad_status.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pending_expiry_is_swept_in_background() {
    let s = TestServer::start_with(|c| c.ledger.pending_sweep_interval_secs = 1).await;
    s.open_account("cash", "asset", "USD").await;
    s.open_account("equity", "equity", "USD").await;
    let r = s
        .post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"mode": "pending", "pending_ttl_secs": 1, "postings": [
                {"account_id": "cash", "direction": "debit", "amount": 5},
                {"account_id": "equity", "direction": "credit", "amount": 5}
            ]}),
        )
        .await;
    let id = r.body["id"].as_str().unwrap().to_string();
    let mut status = String::new();
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let t = s.get(&format!("/v1/transactions/{id}"), READER_KEY).await;
        status = t.body["status"].as_str().unwrap().to_string();
        if status == "voided" {
            assert_eq!(t.body["void_reason"], "expired");
            break;
        }
    }
    assert_eq!(status, "voided");
    let late = s
        .send(
            Method::POST,
            &format!("/v1/transactions/{id}/post"),
            Some(WRITER_KEY),
            None,
            &[],
        )
        .await;
    assert_eq!(late.status, StatusCode::CONFLICT);
}
