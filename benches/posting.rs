//! Posting throughput: two-leg transfers and multi-leg transactions through
//! the full engine path (validation, locking, WAL append, apply).
//!
//! fsync is disabled so the numbers reflect engine overhead rather than the
//! disk; run with `SyncMode::Always` to measure durable throughput.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ledger_core::domain::account::{AccountType, BalancePolicy};
use ledger_core::engine::{CreateTransaction, Ledger, LedgerOptions, OpenAccount, PostingSpec};
use ledger_core::metrics::LedgerMetrics;

fn setup(accounts: usize) -> (tempfile::TempDir, Arc<Ledger>) {
    let dir = tempfile::tempdir().expect("temp dir");
    let ledger = Arc::new(
        Ledger::open(
            LedgerOptions::ephemeral(dir.path()),
            Arc::new(LedgerMetrics::new()),
        )
        .expect("ledger opens"),
    );
    for i in 0..accounts {
        ledger
            .open_account(
                OpenAccount::new(&format!("acct-{i}"), AccountType::Asset, "USD")
                    .expect("valid id")
                    .with_policy(BalancePolicy::Unrestricted),
            )
            .expect("account opens");
    }
    (dir, ledger)
}

fn two_leg(c: &mut Criterion) {
    let (_dir, ledger) = setup(64);
    let mut i = 0usize;
    let mut group = c.benchmark_group("posting");
    group.throughput(Throughput::Elements(1));
    group.bench_function("two_leg_transfer", |b| {
        b.iter(|| {
            i += 1;
            let from = format!("acct-{}", i % 64);
            let to = format!("acct-{}", (i * 7 + 1) % 64);
            let cmd = CreateTransaction::posted(vec![
                PostingSpec::debit(&from, 100).expect("id"),
                PostingSpec::credit(&to, 100).expect("id"),
            ]);
            black_box(ledger.create_transaction(cmd).expect("posts"));
        })
    });
    group.finish();
}

fn multi_leg(c: &mut Criterion) {
    let (_dir, ledger) = setup(64);
    let mut i = 0usize;
    let mut group = c.benchmark_group("posting");
    group.throughput(Throughput::Elements(1));
    group.bench_function("eight_leg_transaction", |b| {
        b.iter(|| {
            i += 1;
            let mut postings = Vec::with_capacity(8);
            for leg in 0..4 {
                postings
                    .push(PostingSpec::debit(&format!("acct-{}", (i + leg) % 64), 25).expect("id"));
                postings.push(
                    PostingSpec::credit(&format!("acct-{}", (i + leg + 32) % 64), 25).expect("id"),
                );
            }
            black_box(
                ledger
                    .create_transaction(CreateTransaction::posted(postings))
                    .expect("posts"),
            );
        })
    });
    group.finish();
}

fn contended(c: &mut Criterion) {
    let (_dir, ledger) = setup(8);
    let mut group = c.benchmark_group("posting");
    group.throughput(Throughput::Elements(400));
    group.bench_function("4_threads_x_100_transfers", |b| {
        b.iter(|| {
            std::thread::scope(|s| {
                for t in 0..4usize {
                    let ledger = ledger.clone();
                    s.spawn(move || {
                        for n in 0..100usize {
                            let cmd = CreateTransaction::posted(vec![
                                PostingSpec::debit(&format!("acct-{}", (t + n) % 8), 1)
                                    .expect("id"),
                                PostingSpec::credit(&format!("acct-{}", (t + n + 3) % 8), 1)
                                    .expect("id"),
                            ]);
                            ledger.create_transaction(cmd).expect("posts");
                        }
                    });
                }
            });
        })
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = two_leg, multi_leg, contended
}
criterion_main!(benches);
