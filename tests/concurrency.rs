//! Concurrency stress tests: deterministic lock ordering (no deadlocks),
//! no lost updates, overdraft safety under contention, optimistic version
//! checks, idempotency races and snapshots taken during writes.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use ledger_core::domain::account::AccountType;
use ledger_core::domain::ids::AccountId;
use ledger_core::domain::transaction::IdempotencyTag;
use ledger_core::engine::{AsOf, CreateTransaction, OpenAccount, PostingSpec, UpdateMetadata};
use ledger_core::error::LedgerError;

/// Small deterministic PRNG so failures are reproducible.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const ACCOUNTS: [&str; 6] = ["a0", "a1", "a2", "a3", "a4", "a5"];

#[test]
fn random_multi_leg_transfers_keep_invariants_and_replay_identically() {
    let (dir, ledger) = ledger();
    open_accounts(&ledger, &ACCOUNTS, AccountType::Asset);
    let committed = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..8u64 {
            let ledger = ledger.clone();
            let committed = &committed;
            s.spawn(move || {
                let mut rng = XorShift(0x9E37_79B9_7F4A_7C15 ^ (t + 1));
                for _ in 0..150 {
                    // Three legs over random (possibly repeated) accounts in
                    // random order: exercises lock ordering heavily.
                    let a = ACCOUNTS[rng.below(6) as usize];
                    let b = ACCOUNTS[rng.below(6) as usize];
                    let c = ACCOUNTS[rng.below(6) as usize];
                    let x = rng.below(500) as i64 + 1;
                    let y = rng.below(500) as i64 + 1;
                    let cmd = CreateTransaction::posted(vec![
                        PostingSpec::debit(a, x + y).unwrap(),
                        PostingSpec::credit(b, x).unwrap(),
                        PostingSpec::credit(c, y).unwrap(),
                    ]);
                    ledger.create_transaction(cmd).unwrap();
                    committed.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert_eq!(committed.load(Ordering::Relaxed), 1200);
    let report = ledger.check_invariants();
    assert!(report.ok, "{:?}", report.violations);
    assert_eq!(report.transactions_checked, 1200);
    let live = ledger.capture_state();
    drop(ledger);
    let replayed = open_ledger(dir.path());
    assert_eq!(replayed.capture_state(), live);
}

#[test]
fn overdraft_floor_holds_under_contention() {
    let (_dir, ledger) = ledger();
    open_accounts(&ledger, &["source"], AccountType::Equity);
    ledger
        .open_account(OpenAccount::new("wallet", AccountType::Asset, "USD").unwrap())
        .unwrap();
    open_accounts(&ledger, &["sink"], AccountType::Expense);
    transfer(&ledger, "wallet", "source", 1_000).unwrap();

    let ok = AtomicUsize::new(0);
    let rejected = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..16 {
            s.spawn(|| {
                for _ in 0..20 {
                    match transfer(&ledger, "sink", "wallet", 10) {
                        Ok(_) => ok.fetch_add(1, Ordering::Relaxed),
                        Err(LedgerError::InsufficientFunds { .. }) => {
                            rejected.fetch_add(1, Ordering::Relaxed)
                        }
                        Err(e) => panic!("unexpected error {e}"),
                    };
                }
            });
        }
    });
    assert_eq!(ok.load(Ordering::Relaxed), 100);
    assert_eq!(rejected.load(Ordering::Relaxed), 16 * 20 - 100);
    let wallet = ledger
        .balance(&AccountId::new("wallet").unwrap(), AsOf::Latest)
        .unwrap();
    assert_eq!(wallet.posted, 0);
    assert!(ledger.check_invariants().ok);
}

#[test]
fn optimistic_version_checks_never_lose_updates() {
    let (_dir, ledger) = ledger();
    open_accounts(&ledger, &["acct"], AccountType::Asset);
    let id = AccountId::new("acct").unwrap();
    let conflicts = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..6 {
            let ledger = ledger.clone();
            let id = id.clone();
            let conflicts = &conflicts;
            s.spawn(move || {
                for i in 0..25 {
                    loop {
                        let current = ledger.account(&id).unwrap().account;
                        let count: u64 = current
                            .metadata
                            .get("counter")
                            .map(|v| v.parse().unwrap())
                            .unwrap_or(0);
                        let mut set = BTreeMap::new();
                        set.insert("counter".to_string(), (count + 1).to_string());
                        set.insert(format!("t{t}"), i.to_string());
                        match ledger.update_metadata(UpdateMetadata {
                            account_id: id.clone(),
                            set,
                            remove: vec![],
                            expected_version: Some(current.version),
                        }) {
                            Ok(_) => break,
                            Err(LedgerError::VersionConflict { .. }) => {
                                conflicts.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => panic!("unexpected {e}"),
                        }
                    }
                }
            });
        }
    });
    let account = ledger.account(&id).unwrap().account;
    // Read-modify-write with version checks: no increment is lost.
    assert_eq!(account.metadata["counter"], "150");
    assert_eq!(account.version, 1 + 150);
}

#[test]
fn concurrent_requests_with_same_idempotency_key_create_one_transaction() {
    let (_dir, ledger) = ledger();
    open_accounts(&ledger, &["a", "b"], AccountType::Asset);
    let ids = Mutex::new(HashSet::new());
    std::thread::scope(|s| {
        for _ in 0..12 {
            s.spawn(|| {
                let mut cmd = CreateTransaction::posted(vec![
                    PostingSpec::debit("a", 42).unwrap(),
                    PostingSpec::credit("b", 42).unwrap(),
                ]);
                cmd.idempotency = Some(IdempotencyTag {
                    key: "client:same".into(),
                    fingerprint: "fp".into(),
                });
                loop {
                    match ledger.create_transaction(cmd.clone()) {
                        Ok(outcome) => {
                            ids.lock().unwrap().insert(outcome.transaction.id);
                            break;
                        }
                        Err(LedgerError::RequestInProgress) => std::thread::yield_now(),
                        Err(e) => panic!("unexpected {e}"),
                    }
                }
            });
        }
    });
    assert_eq!(ids.lock().unwrap().len(), 1);
    let a = ledger
        .balance(&AccountId::new("a").unwrap(), AsOf::Latest)
        .unwrap();
    assert_eq!(a.posted, 42);
}

#[test]
fn post_and_void_race_has_exactly_one_winner() {
    let (_dir, ledger) = ledger();
    open_accounts(&ledger, &["a", "b"], AccountType::Asset);
    for _ in 0..20 {
        let pending = ledger
            .create_transaction(CreateTransaction::pending(
                vec![
                    PostingSpec::debit("a", 5).unwrap(),
                    PostingSpec::credit("b", 5).unwrap(),
                ],
                None,
            ))
            .unwrap()
            .transaction;
        let wins = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for t in 0..4 {
                let ledger = &ledger;
                let wins = &wins;
                s.spawn(move || {
                    let result = if t % 2 == 0 {
                        ledger.post_pending(pending.id)
                    } else {
                        ledger.void_pending(pending.id, None)
                    };
                    match result {
                        Ok(_) => {
                            wins.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(LedgerError::InvalidTransactionState { .. }) => {}
                        Err(e) => panic!("unexpected {e}"),
                    }
                });
            }
        });
        assert_eq!(wins.load(Ordering::Relaxed), 1);
    }
    assert!(ledger.check_invariants().ok);
    assert_eq!(ledger.stats().pending_transactions, 0);
}

#[test]
fn snapshots_taken_during_writes_are_consistent() {
    let (dir, ledger) = ledger();
    open_accounts(&ledger, &ACCOUNTS, AccountType::Asset);
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let ledger = ledger.clone();
            s.spawn(move || {
                let mut rng = XorShift(t + 7);
                for _ in 0..200 {
                    let a = ACCOUNTS[rng.below(6) as usize];
                    let b = ACCOUNTS[rng.below(6) as usize];
                    transfer(&ledger, a, b, rng.below(100) as i64 + 1).unwrap();
                }
            });
        }
        let ledger = ledger.clone();
        s.spawn(move || {
            for _ in 0..10 {
                let state = ledger.capture_state();
                let report = ledger_core::engine::reports::check_invariants(&state);
                assert!(report.ok, "inconsistent capture: {:?}", report.violations);
                ledger.snapshot().unwrap();
            }
        });
    });
    let live = ledger.capture_state();
    drop(ledger);
    let recovered = open_ledger(dir.path());
    assert!(recovered.recovery_report().snapshot_sequence.is_some());
    assert_eq!(recovered.capture_state(), live);
}
