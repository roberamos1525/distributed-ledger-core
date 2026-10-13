//! Replay speed: rebuilding state from the WAL alone versus from a snapshot
//! plus the WAL tail.

use std::sync::Arc;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use ledger_core::domain::account::{AccountType, BalancePolicy};
use ledger_core::engine::{
    CreateTransaction, Ledger, LedgerOptions, LedgerState, OpenAccount, PostingSpec,
};
use ledger_core::metrics::LedgerMetrics;
use ledger_core::store::fs_util::DataLayout;
use ledger_core::store::recovery::{RecoveryOptions, recover};

const EVENTS: usize = 5_000;

/// Writes `EVENTS` transfers and optionally a snapshot at the half-way point.
fn build(snapshot_halfway: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let ledger = Ledger::open(
        LedgerOptions::ephemeral(dir.path()),
        Arc::new(LedgerMetrics::new()),
    )
    .expect("ledger opens");
    for i in 0..16 {
        ledger
            .open_account(
                OpenAccount::new(&format!("acct-{i}"), AccountType::Asset, "USD")
                    .expect("valid id")
                    .with_policy(BalancePolicy::Unrestricted),
            )
            .expect("account opens");
    }
    for i in 0..EVENTS {
        let cmd = CreateTransaction::posted(vec![
            PostingSpec::debit(&format!("acct-{}", i % 16), 10).expect("id"),
            PostingSpec::credit(&format!("acct-{}", (i + 5) % 16), 10).expect("id"),
        ]);
        ledger.create_transaction(cmd).expect("posts");
        if snapshot_halfway && i == EVENTS / 2 {
            ledger.snapshot().expect("snapshot");
        }
    }
    dir
}

fn replay(c: &mut Criterion) {
    let full = build(false);
    let with_snapshot = build(true);
    let mut group = c.benchmark_group("replay");
    group.throughput(Throughput::Elements(EVENTS as u64));
    group.sample_size(10);
    group.bench_function("wal_only_5k_events", |b| {
        let layout = DataLayout::new(full.path());
        b.iter_batched(
            RecoveryOptions::default,
            |opts| recover::<LedgerState>(&layout, &opts).expect("recovers"),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("snapshot_plus_tail_5k_events", |b| {
        let layout = DataLayout::new(with_snapshot.path());
        b.iter_batched(
            RecoveryOptions::default,
            |opts| recover::<LedgerState>(&layout, &opts).expect("recovers"),
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group!(benches, replay);
criterion_main!(benches);
