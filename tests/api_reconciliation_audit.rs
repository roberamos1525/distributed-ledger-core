//! HTTP integration tests for reconciliation, reports and the audit trail.

mod common;

use common::*;
use reqwest::{Method, StatusCode};
use serde_json::json;

async fn bank_with_sales() -> TestServer {
    let s = TestServer::start().await;
    s.open_account("bank", "asset", "USD").await;
    s.open_account("sales", "revenue", "USD").await;
    for (amount, reference) in [(12_000, "INV-100"), (3_050, "INV-101"), (999, "INV-102")] {
        let r = s
            .post(
                "/v1/transactions",
                WRITER_KEY,
                json!({"reference": reference, "postings": [
                    {"account_id": "bank", "direction": "debit", "amount": amount},
                    {"account_id": "sales", "direction": "credit", "amount": amount}
                ]}),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED);
    }
    s
}

fn today() -> String {
    chrono::Utc::now().date_naive().to_string()
}

#[tokio::test]
async fn csv_statement_reconciles_cleanly() {
    let s = bank_with_sales().await;
    let d = today();
    let csv = format!(
        "date,description,amount,reference\n{d},Invoice 100,120.00,INV-100\n{d},Invoice 101,30.50,INV-101\n{d},Card,9.99,\n"
    );
    let r = s
        .post(
            "/v1/reconciliations",
            WRITER_KEY,
            json!({"account_id": "bank", "format": "csv", "data": csv}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "reconciled");
    assert_eq!(r.body["summary"]["matched"], 3);
    let methods: Vec<&str> = r.body["matched"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["method"].as_str().unwrap())
        .collect();
    assert_eq!(methods.iter().filter(|m| **m == "reference").count(), 2);
    assert_eq!(methods.iter().filter(|m| **m == "amount_date").count(), 1);
}

#[tokio::test]
async fn json_statement_with_discrepancies_and_tolerance() {
    let s = bank_with_sales().await;
    let d = today();
    let statement = json!({
        "currency": "USD",
        "closing_balance": "158.00",
        "lines": [
            {"date": d, "amount": "120.00", "reference": "INV-100"},
            {"date": d, "amount": "31.00", "reference": "INV-101"},
            {"date": d, "amount": "10.00"},
            {"date": d, "amount": "-2.50", "description": "bank fee"}
        ]
    })
    .to_string();
    let strict = s
        .post(
            "/v1/reconciliations",
            WRITER_KEY,
            json!({"account_id": "bank", "format": "json", "data": statement.clone()}),
        )
        .await;
    assert_eq!(strict.status, StatusCode::OK, "{}", strict.body);
    assert_eq!(strict.body["status"], "discrepancies");
    assert_eq!(strict.body["summary"]["discrepancies"], 1);
    assert_eq!(strict.body["discrepancies"][0]["kind"], "amount_mismatch");
    assert_eq!(strict.body["discrepancies"][0]["amount_difference"], 50);
    assert_eq!(strict.body["summary"]["unmatched_statement"], 2);
    assert_eq!(strict.body["summary"]["unmatched_ledger"], 1);
    assert_eq!(strict.body["balance_check"]["ledger_closing"], 16_049);
    assert_eq!(strict.body["balance_check"]["matches"], false);

    // With a 0.01 tolerance the card payment (9.99 vs 10.00) matches; the
    // INV-101 difference of 0.50 is still a discrepancy.
    let tolerant = s
        .post(
            "/v1/reconciliations",
            WRITER_KEY,
            json!({"account_id": "bank", "format": "json", "data": statement,
                   "amount_tolerance": "0.01", "date_tolerance_days": 0}),
        )
        .await;
    assert_eq!(tolerant.body["summary"]["matched"], 2);
    assert_eq!(tolerant.body["summary"]["unmatched_ledger"], 0);
}

#[tokio::test]
async fn reconciliation_validation() {
    let s = bank_with_sales().await;
    let bad_csv = s
        .post(
            "/v1/reconciliations",
            WRITER_KEY,
            json!({"account_id": "bank", "format": "csv", "data": "date,amount\nyesterday,1.00\n"}),
        )
        .await;
    assert_eq!(bad_csv.status, StatusCode::BAD_REQUEST);
    assert!(
        bad_csv.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("line 1")
    );
    let unknown_account = s
        .post(
            "/v1/reconciliations",
            WRITER_KEY,
            json!({"account_id": "nope", "format": "csv", "data": "date,amount\n"}),
        )
        .await;
    assert_eq!(unknown_account.status, StatusCode::NOT_FOUND);
    let bad_format = s
        .post(
            "/v1/reconciliations",
            WRITER_KEY,
            json!({"account_id": "bank", "format": "xml", "data": ""}),
        )
        .await;
    assert_eq!(bad_format.status, StatusCode::BAD_REQUEST);
    let wide_tolerance = s
        .post(
            "/v1/reconciliations",
            WRITER_KEY,
            json!({"account_id": "bank", "format": "csv", "data": "date,amount\n", "date_tolerance_days": 90}),
        )
        .await;
    assert_eq!(wide_tolerance.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn trial_balance_and_invariants() {
    let s = bank_with_sales().await;
    s.open_account("eur-bank", "asset", "EUR").await;
    s.open_account("eur-equity", "equity", "EUR").await;
    assert_eq!(
        s.transfer("eur-bank", "eur-equity", 5_000).await.status,
        StatusCode::CREATED
    );
    let tb = s.get("/v1/reports/trial-balance", READER_KEY).await;
    assert_eq!(tb.status, StatusCode::OK);
    assert_eq!(tb.body["balanced"], true);
    let currencies = tb.body["currencies"].as_array().unwrap();
    assert_eq!(currencies.len(), 2);
    let usd = currencies.iter().find(|c| c["currency"] == "USD").unwrap();
    assert_eq!(usd["total_debits"], 16_049);
    assert_eq!(usd["total_credits"], 16_049);

    let inv = s.get("/v1/reports/invariants", READER_KEY).await;
    assert_eq!(inv.status, StatusCode::OK, "{}", inv.body);
    assert_eq!(inv.body["ok"], true);
    assert_eq!(inv.body["posted_sums"]["USD"], 0);
    assert_eq!(inv.body["transactions_checked"], 4);
}

#[tokio::test]
async fn audit_trail_records_and_verifies() {
    let s = bank_with_sales().await;
    let failed = s.transfer("bank", "nowhere", 1).await;
    assert_eq!(failed.status, StatusCode::NOT_FOUND);

    let list = s.get("/v1/audit?limit=100", ADMIN_KEY).await;
    assert_eq!(list.status, StatusCode::OK);
    let records = list.body["data"].as_array().unwrap();
    // 2 accounts + 3 transactions + 1 rejected transaction.
    assert_eq!(records.len(), 6);
    assert_eq!(records[0]["action"], "account.open");
    assert_eq!(records[0]["actor"], "writer");
    assert_eq!(records[0]["prev_hash"], "0".repeat(64));
    assert_eq!(records[1]["prev_hash"], records[0]["hash"]);
    let last = &records[5];
    assert_eq!(last["action"], "transaction.create");
    assert_eq!(last["outcome"], "rejected:account_not_found");
    assert!(last["request_id"].is_string());

    let page = s.get("/v1/audit?after=4&limit=1", ADMIN_KEY).await;
    assert_eq!(page.body["data"][0]["sequence"], 5);

    let verify = s.get("/v1/audit/verify", ADMIN_KEY).await;
    assert_eq!(verify.status, StatusCode::OK);
    assert_eq!(verify.body["ok"], true);
    assert_eq!(verify.body["records"], 6);
}

#[tokio::test]
async fn audit_tampering_is_detected_via_api() {
    let s = bank_with_sales().await;
    let path = s.data_dir().join("audit.log");
    let mut bytes = std::fs::read(&path).unwrap();
    // Flip one byte inside the second record's payload. The frame CRC no
    // longer matches, so verification reports the break at record 2.
    let second_start = {
        let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        20 + len
    };
    bytes[second_start + 30] ^= 0x20;
    std::fs::write(&path, &bytes).unwrap();
    let verify = s.get("/v1/audit/verify", ADMIN_KEY).await;
    assert_eq!(verify.status, StatusCode::CONFLICT);
    assert_eq!(verify.body["ok"], false);
    assert_eq!(verify.body["first_invalid_sequence"], 2);
}

#[tokio::test]
async fn admin_compaction_endpoint() {
    let small_segments = |c: &mut ledger_core::config::Config| {
        c.storage.segment_max_bytes = 4096;
        c.storage.max_record_bytes = 4096;
    };
    let s = TestServer::start_with(small_segments).await;
    s.open_account("cash", "asset", "USD").await;
    s.open_account("equity", "equity", "USD").await;
    for _ in 0..30 {
        assert_eq!(
            s.transfer("cash", "equity", 10).await.status,
            StatusCode::CREATED
        );
    }
    let r = s
        .send(
            Method::POST,
            "/v1/admin/compactions",
            Some(ADMIN_KEY),
            None,
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body["removed_segments"].as_array().unwrap().is_empty());
    let dir = s.stop().await;
    let s = TestServer::start_in(dir, small_segments).await;
    assert_eq!(s.posted_balance("cash").await, 300);
    let info = s.get("/v1/info", READER_KEY).await;
    assert!(
        info.body["recovery"]["snapshot_sequence"]
            .as_u64()
            .is_some()
    );
}
