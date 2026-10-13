//! Crash-recovery tests: torn writes, corruption, snapshot fallback and
//! replay determinism.

mod common;

use std::fs::{self, OpenOptions};

use common::*;
use ledger_core::domain::account::AccountType;
use ledger_core::engine::LedgerOptions;
use ledger_core::error::{LedgerError, StorageError};
use ledger_core::metrics::LedgerMetrics;
use ledger_core::store::frame;
use ledger_core::store::fs_util::DataLayout;
use ledger_core::store::snapshot::list_snapshots;
use ledger_core::store::wal::list_segments;
use std::sync::Arc;

fn last_segment(dir: &std::path::Path) -> ledger_core::store::wal::SegmentInfo {
    list_segments(&DataLayout::new(dir).wal_dir())
        .unwrap()
        .pop()
        .unwrap()
}

fn populate(path: &std::path::Path, transfers: usize) -> ledger_core::engine::LedgerState {
    let ledger = open_ledger(path);
    open_accounts(&ledger, &["a", "b", "c"], AccountType::Asset);
    for i in 0..transfers {
        let (from, to) = if i % 2 == 0 { ("a", "b") } else { ("b", "c") };
        transfer(&ledger, from, to, (i as i64 + 1) * 10).unwrap();
    }
    ledger.capture_state()
}

#[test]
fn truncating_the_wal_mid_record_loses_only_that_record() {
    let dir = tempfile::tempdir().unwrap();
    let before = populate(dir.path(), 10);
    let seg = last_segment(dir.path());
    // Cut 7 bytes off the final frame, as if the process died mid-write.
    OpenOptions::new()
        .write(true)
        .open(&seg.path)
        .unwrap()
        .set_len(seg.len - 7)
        .unwrap();

    let ledger = open_ledger(dir.path());
    let report = ledger.recovery_report().clone();
    let damage = report.tail_damage.expect("torn tail reported");
    assert_eq!(damage.kind, "torn");
    assert!(damage.repaired);
    assert_eq!(report.last_sequence, before.last_sequence - 1);
    assert!(ledger.check_invariants().ok);

    // The ledger keeps working and the next event reuses the lost sequence.
    let tx = transfer(&ledger, "a", "c", 1).unwrap();
    assert_eq!(tx.created_sequence, before.last_sequence);
    drop(ledger);
    let reopened = open_ledger(dir.path());
    assert!(reopened.recovery_report().tail_damage.is_none());
    assert_eq!(reopened.last_sequence(), before.last_sequence);
}

#[test]
fn every_truncation_point_recovers_a_consistent_prefix() {
    let dir = tempfile::tempdir().unwrap();
    populate(dir.path(), 3);
    let seg = last_segment(dir.path());
    let original = fs::read(&seg.path).unwrap();
    let mut last_seen = 0;
    // Try a cut at many offsets across the whole file.
    for cut in (0..original.len()).step_by(37) {
        let work = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(work.path());
        layout.ensure().unwrap();
        fs::write(
            layout.wal_dir().join(seg.path.file_name().unwrap()),
            &original[..cut],
        )
        .unwrap();
        let ledger = open_ledger(work.path());
        let seq = ledger.last_sequence();
        assert!(seq >= last_seen, "recovered prefix must grow with the cut");
        last_seen = seq;
        let report = ledger.check_invariants();
        assert!(report.ok, "cut {cut}: {:?}", report.violations);
    }
}

#[test]
fn garbage_after_last_record_is_corruption_unless_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let before = populate(dir.path(), 4);
    let seg = last_segment(dir.path());
    let mut bytes = fs::read(&seg.path).unwrap();
    bytes.extend_from_slice(b"\xde\xad\xbe\xefnot a frame at all but long enough to be a header");
    fs::write(&seg.path, &bytes).unwrap();

    let err = ledger_core::engine::Ledger::open(
        LedgerOptions::ephemeral(dir.path()),
        Arc::new(LedgerMetrics::new()),
    )
    .err()
    .expect("bad magic must be reported as corruption");
    assert!(
        matches!(err, LedgerError::Storage(StorageError::Corrupt { .. })),
        "{err}"
    );

    let mut opts = LedgerOptions::ephemeral(dir.path());
    opts.recovery.truncate_corruption = true;
    let ledger = ledger_core::engine::Ledger::open(opts, Arc::new(LedgerMetrics::new())).unwrap();
    assert_eq!(ledger.last_sequence(), before.last_sequence);
    assert_eq!(
        ledger.recovery_report().tail_damage.as_ref().unwrap().kind,
        "corrupt"
    );
}

#[test]
fn flipped_bit_in_middle_record_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    populate(dir.path(), 6);
    let seg = last_segment(dir.path());
    let mut bytes = fs::read(&seg.path).unwrap();
    // Flip a bit inside the payload of a record in the middle of the log.
    let scan = frame::scan(&bytes, frame::WAL_MAGIC, 1 << 20);
    let victim = &scan.frames[scan.frames.len() / 2];
    let mid = (victim.payload.start + victim.payload.end) / 2;
    bytes[mid] ^= 0x01;
    fs::write(&seg.path, &bytes).unwrap();
    let result = ledger_core::engine::Ledger::open(
        LedgerOptions::ephemeral(dir.path()),
        Arc::new(LedgerMetrics::new()),
    );
    assert!(matches!(
        result,
        Err(LedgerError::Storage(StorageError::Corrupt { .. }))
    ));
}

#[test]
fn replay_rebuilds_identical_state_with_and_without_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = open_ledger(dir.path());
    open_accounts(&ledger, &["a", "b"], AccountType::Asset);
    for i in 0..20 {
        transfer(&ledger, "a", "b", i + 1).unwrap();
        if i == 9 {
            ledger.snapshot().unwrap();
        }
    }
    let live = ledger.capture_state();
    drop(ledger);

    let with_snapshot = open_ledger(dir.path());
    assert_eq!(with_snapshot.recovery_report().events_replayed, 10);
    assert_eq!(with_snapshot.capture_state(), live);
    drop(with_snapshot);

    for snap in list_snapshots(&DataLayout::new(dir.path()).snapshot_dir()).unwrap() {
        fs::remove_file(snap.path).unwrap();
    }
    let from_log = open_ledger(dir.path());
    assert_eq!(from_log.recovery_report().snapshot_sequence, None);
    assert_eq!(from_log.capture_state(), live);
}

#[test]
fn corrupt_newest_snapshot_falls_back_to_older_one() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = open_ledger(dir.path());
    open_accounts(&ledger, &["a", "b"], AccountType::Asset);
    transfer(&ledger, "a", "b", 5).unwrap();
    ledger.snapshot().unwrap();
    transfer(&ledger, "a", "b", 7).unwrap();
    let newest = ledger.snapshot().unwrap();
    let live = ledger.capture_state();
    drop(ledger);

    let mut bytes = fs::read(&newest.path).unwrap();
    let n = bytes.len();
    bytes[n - 3] ^= 0xFF;
    fs::write(&newest.path, bytes).unwrap();

    let ledger = open_ledger(dir.path());
    let report = ledger.recovery_report();
    assert_eq!(report.snapshots_skipped.len(), 1);
    assert!(report.snapshot_sequence.unwrap() < newest.sequence);
    assert_eq!(ledger.capture_state(), live);
}

#[test]
fn compaction_then_crash_still_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = LedgerOptions::ephemeral(dir.path());
    opts.segment_max_bytes = 3_000;
    let ledger =
        ledger_core::engine::Ledger::open(opts.clone(), Arc::new(LedgerMetrics::new())).unwrap();
    open_accounts(&ledger, &["a", "b"], AccountType::Asset);
    for _ in 0..25 {
        transfer(&ledger, "a", "b", 3).unwrap();
    }
    let report = ledger.compact().unwrap();
    assert!(!report.removed_segments.is_empty());
    for _ in 0..3 {
        transfer(&ledger, "b", "a", 1).unwrap();
    }
    let live = ledger.capture_state();
    drop(ledger);
    // Tear the newest record after compaction.
    let seg = last_segment(dir.path());
    OpenOptions::new()
        .write(true)
        .open(&seg.path)
        .unwrap()
        .set_len(seg.len - 1)
        .unwrap();
    let ledger = ledger_core::engine::Ledger::open(opts, Arc::new(LedgerMetrics::new())).unwrap();
    assert_eq!(ledger.last_sequence(), live.last_sequence - 1);
    assert!(ledger.check_invariants().ok);
    let b = ledger.balance(
        &ledger_core::domain::ids::AccountId::new("b").unwrap(),
        ledger_core::engine::AsOf::Latest,
    );
    // b was credited 75 then debited 2 (the third debit was torn away).
    assert_eq!(b.unwrap().posted, -(25 * 3) + 2);
}
