//! Shared helpers for integration tests: an in-process HTTP server bound to
//! an ephemeral port, API keys for every role, and engine fixtures.

#![allow(dead_code)]

use std::sync::Arc;

use ledger_core::api::auth::hash_key;
use ledger_core::app::App;
use ledger_core::config::{ApiKeyConfig, Config, FsyncMode, Role};
use ledger_core::domain::account::AccountType;
use ledger_core::engine::{CreateTransaction, Ledger, LedgerOptions, OpenAccount, PostingSpec};
use ledger_core::metrics::LedgerMetrics;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub const ADMIN_KEY: &str = "ldg_test_admin_key_0000000000000000";
pub const WRITER_KEY: &str = "ldg_test_writer_key_000000000000000";
pub const READER_KEY: &str = "ldg_test_reader_key_000000000000000";

/// Test configuration rooted at `dir`.
pub fn test_config(dir: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.storage.data_dir = dir.to_path_buf();
    config.storage.fsync = FsyncMode::Never;
    config.storage.audit_fsync = false;
    config.storage.snapshot_every_events = 0;
    config.server.bind = "127.0.0.1:0".parse().unwrap();
    config.auth.api_keys = vec![
        ApiKeyConfig {
            id: "admin".into(),
            role: Role::Admin,
            key_sha256: hash_key(ADMIN_KEY),
        },
        ApiKeyConfig {
            id: "writer".into(),
            role: Role::Writer,
            key_sha256: hash_key(WRITER_KEY),
        },
        ApiKeyConfig {
            id: "reader".into(),
            role: Role::Reader,
            key_sha256: hash_key(READER_KEY),
        },
    ];
    config
}

/// A running in-process server.
pub struct TestServer {
    pub base: String,
    pub client: reqwest::Client,
    dir: Option<TempDir>,
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

/// Response pieces.
pub struct Reply {
    pub status: StatusCode,
    pub body: Value,
    pub headers: HeaderMap,
}

impl TestServer {
    pub async fn start() -> TestServer {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(customize: impl FnOnce(&mut Config)) -> TestServer {
        Self::start_in(tempfile::tempdir().unwrap(), customize).await
    }

    /// Starts a server on an existing data directory (restart scenarios).
    pub async fn start_in(dir: TempDir, customize: impl FnOnce(&mut Config)) -> TestServer {
        let mut config = test_config(dir.path());
        customize(&mut config);
        config.validate().expect("test config must be valid");
        let app = App::open(config).expect("app opens");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(app.serve(listener, async move {
            let _ = rx.await;
        }));
        TestServer {
            base: format!("http://{addr}"),
            client: reqwest::Client::new(),
            dir: Some(dir),
            shutdown: Some(tx),
            handle: Some(handle),
        }
    }

    /// Gracefully stops the server and hands back its data directory.
    pub async fn stop(mut self) -> TempDir {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            h.await.unwrap().unwrap();
        }
        self.dir.take().unwrap()
    }

    pub fn data_dir(&self) -> &std::path::Path {
        self.dir.as_ref().unwrap().path()
    }

    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        key: Option<&str>,
        body: Option<Value>,
        extra_headers: &[(&str, &str)],
    ) -> Reply {
        let mut req = self
            .client
            .request(method, format!("{}{}", self.base, path));
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        for (name, value) in extra_headers {
            req = req.header(*name, *value);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.expect("request sent");
        let status = resp.status();
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Reply {
            status,
            body,
            headers,
        }
    }

    pub async fn get(&self, path: &str, key: &str) -> Reply {
        self.send(reqwest::Method::GET, path, Some(key), None, &[])
            .await
    }

    pub async fn post(&self, path: &str, key: &str, body: Value) -> Reply {
        self.send(reqwest::Method::POST, path, Some(key), Some(body), &[])
            .await
    }

    pub async fn post_idem(&self, path: &str, key: &str, body: Value, idem: &str) -> Reply {
        self.send(
            reqwest::Method::POST,
            path,
            Some(key),
            Some(body),
            &[("idempotency-key", idem)],
        )
        .await
    }

    /// Creates an account and asserts success.
    pub async fn open_account(&self, id: &str, account_type: &str, currency: &str) -> Value {
        let r = self
            .post(
                "/v1/accounts",
                WRITER_KEY,
                json!({"id": id, "name": id, "type": account_type, "currency": currency}),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        r.body
    }

    /// Posts a simple two-leg transfer and returns the response.
    pub async fn transfer(&self, debit: &str, credit: &str, amount: i64) -> Reply {
        self.post(
            "/v1/transactions",
            WRITER_KEY,
            json!({"postings": [
                {"account_id": debit, "direction": "debit", "amount": amount},
                {"account_id": credit, "direction": "credit", "amount": amount}
            ]}),
        )
        .await
    }

    pub async fn posted_balance(&self, account: &str) -> i64 {
        let r = self
            .get(&format!("/v1/accounts/{account}/balance"), READER_KEY)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        r.body["posted"].as_i64().unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Opens an engine on a fresh temp dir (no fsync).
pub fn ledger() -> (TempDir, Arc<Ledger>) {
    let dir = tempfile::tempdir().unwrap();
    let ledger = open_ledger(dir.path());
    (dir, ledger)
}

pub fn open_ledger(path: &std::path::Path) -> Arc<Ledger> {
    Arc::new(
        Ledger::open(
            LedgerOptions::ephemeral(path),
            Arc::new(LedgerMetrics::new()),
        )
        .unwrap(),
    )
}

/// Opens accounts with the given type in USD and unrestricted policy.
pub fn open_accounts(ledger: &Ledger, ids: &[&str], account_type: AccountType) {
    for id in ids {
        ledger
            .open_account(
                OpenAccount::new(id, account_type, "USD")
                    .unwrap()
                    .with_policy(ledger_core::domain::account::BalancePolicy::Unrestricted),
            )
            .unwrap();
    }
}

/// Posts a two-leg transfer through the engine.
pub fn transfer(
    ledger: &Ledger,
    debit: &str,
    credit: &str,
    amount: i64,
) -> ledger_core::error::LedgerResult<ledger_core::domain::transaction::TransactionRecord> {
    ledger
        .create_transaction(CreateTransaction::posted(vec![
            PostingSpec::debit(debit, amount).unwrap(),
            PostingSpec::credit(credit, amount).unwrap(),
        ]))
        .map(|o| o.transaction)
}
