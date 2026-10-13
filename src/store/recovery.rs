//! Crash recovery: newest valid snapshot + WAL replay, with torn-write repair.

use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::domain::events::EventEnvelope;
use crate::error::StorageError;
use crate::store::frame::{self, Tail, WAL_MAGIC};
use crate::store::fs_util::{DataLayout, read_file, truncate_file};
use crate::store::snapshot;
use crate::store::wal::{SegmentInfo, decode_envelope, list_segments};

/// State that can be rebuilt by replaying events on top of a snapshot.
pub trait ReplayTarget: Serialize + DeserializeOwned + Default {
    /// Sequence of the last event folded into this state.
    fn last_sequence(&self) -> u64;
    /// Timestamp of the last event folded into this state.
    fn last_timestamp(&self) -> Option<DateTime<Utc>>;
    /// Applies one event. Errors abort recovery.
    fn apply_event(&mut self, envelope: &EventEnvelope) -> Result<(), String>;
}

/// Knobs for recovery behaviour.
#[derive(Debug, Clone)]
pub struct RecoveryOptions {
    /// Truncate an incomplete record at the end of the newest segment.
    pub repair_torn_tail: bool,
    /// Also truncate at a corrupt record in the newest segment (data after it
    /// is discarded). Off by default: corruption is surfaced as an error.
    pub truncate_corruption: bool,
    /// Maximum accepted record payload.
    pub max_record_bytes: usize,
    /// Stop replay after this sequence (point-in-time rebuild, read-only).
    pub up_to_sequence: Option<u64>,
    /// Ignore snapshots and replay the full log (verification mode).
    pub ignore_snapshots: bool,
}

impl Default for RecoveryOptions {
    fn default() -> Self {
        RecoveryOptions {
            repair_torn_tail: true,
            truncate_corruption: false,
            max_record_bytes: 16 << 20,
            up_to_sequence: None,
            ignore_snapshots: false,
        }
    }
}

/// Description of a damaged WAL tail found during recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TailDamage {
    pub segment: PathBuf,
    pub offset: u64,
    pub discarded_bytes: u64,
    pub kind: &'static str,
    pub reason: String,
    pub repaired: bool,
}

/// What recovery did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecoveryReport {
    pub snapshot_sequence: Option<u64>,
    pub snapshots_skipped: Vec<String>,
    pub segments_scanned: usize,
    pub events_replayed: u64,
    pub events_skipped: u64,
    pub last_sequence: u64,
    pub tail_damage: Option<TailDamage>,
    pub duration_ms: u64,
}

/// Recovered state plus what the WAL writer needs to resume.
#[derive(Debug)]
pub struct Recovered<T> {
    pub state: T,
    pub report: RecoveryReport,
    /// Newest segment and its valid length, to continue appending.
    pub active_segment: Option<(PathBuf, u64)>,
}

/// Rebuilds state from the newest valid snapshot plus the WAL.
pub fn recover<T: ReplayTarget>(
    layout: &DataLayout,
    opts: &RecoveryOptions,
) -> Result<Recovered<T>, StorageError> {
    let started = Instant::now();
    let mut report = RecoveryReport::default();

    let mut state = T::default();
    if !opts.ignore_snapshots {
        let latest = snapshot::load_latest::<T>(&layout.snapshot_dir(), opts.up_to_sequence)?;
        report.snapshots_skipped = latest
            .skipped
            .iter()
            .map(|(meta, reason)| format!("{}: {reason}", meta.path.display()))
            .collect();
        if let Some((loaded, meta)) = latest.loaded {
            if loaded.last_sequence() != meta.sequence {
                return Err(StorageError::Corrupt {
                    file: meta.path.display().to_string(),
                    offset: 0,
                    reason: "snapshot state sequence does not match header".into(),
                });
            }
            report.snapshot_sequence = Some(meta.sequence);
            state = loaded;
        }
    }

    let segments = list_segments(&layout.wal_dir())?;
    let mut expected = state.last_sequence() + 1;
    let mut active_segment = None;
    let read_only = opts.up_to_sequence.is_some();

    'segments: for (index, segment) in segments.iter().enumerate() {
        let is_last = index + 1 == segments.len();
        report.segments_scanned += 1;
        let buf = read_file(&segment.path)?;
        let scan = frame::scan(&buf, WAL_MAGIC, opts.max_record_bytes);
        if let Some(first) = scan.frames.first()
            && first.sequence != segment.first_sequence
        {
            return Err(StorageError::Corrupt {
                file: segment.path.display().to_string(),
                offset: 0,
                reason: format!(
                    "segment named for sequence {} starts with {}",
                    segment.first_sequence, first.sequence
                ),
            });
        }
        for f in &scan.frames {
            let envelope = decode_envelope(&buf, f, &segment.path)?;
            if envelope.sequence < expected {
                report.events_skipped += 1;
                continue;
            }
            if opts
                .up_to_sequence
                .is_some_and(|max| envelope.sequence > max)
            {
                break 'segments;
            }
            if envelope.sequence != expected {
                return Err(StorageError::SequenceGap {
                    expected,
                    found: envelope.sequence,
                    file: segment.path.display().to_string(),
                });
            }
            state
                .apply_event(&envelope)
                .map_err(|reason| StorageError::Replay {
                    sequence: envelope.sequence,
                    reason,
                })?;
            report.events_replayed += 1;
            expected += 1;
        }
        let valid_len = handle_tail(segment, &scan, is_last, read_only, opts, &mut report)?;
        if is_last {
            active_segment = Some((segment.path.clone(), valid_len));
        }
    }

    report.last_sequence = expected - 1;
    report.duration_ms = started.elapsed().as_millis() as u64;
    Ok(Recovered {
        state,
        report,
        active_segment,
    })
}

fn handle_tail(
    segment: &SegmentInfo,
    scan: &frame::Scan,
    is_last: bool,
    read_only: bool,
    opts: &RecoveryOptions,
    report: &mut RecoveryReport,
) -> Result<u64, StorageError> {
    let (offset, reason, kind) = match &scan.tail {
        Tail::Clean => return Ok(scan.valid_len),
        Tail::Torn { offset, reason } => (*offset, reason.clone(), "torn"),
        Tail::Corrupt { offset, reason } => (*offset, reason.clone(), "corrupt"),
    };
    let repairable = is_last
        && match kind {
            "torn" => opts.repair_torn_tail,
            _ => opts.truncate_corruption,
        };
    if !is_last || (kind == "corrupt" && !opts.truncate_corruption) {
        return Err(StorageError::Corrupt {
            file: segment.path.display().to_string(),
            offset,
            reason,
        });
    }
    let repaired = repairable && !read_only;
    if repaired {
        truncate_file(&segment.path, scan.valid_len)?;
        tracing::warn!(
            segment = %segment.path.display(),
            offset,
            discarded = segment.len - scan.valid_len,
            %reason,
            "truncated damaged WAL tail"
        );
    }
    report.tail_damage = Some(TailDamage {
        segment: segment.path.clone(),
        offset,
        discarded_bytes: segment.len - scan.valid_len,
        kind,
        reason,
        repaired,
    });
    Ok(scan.valid_len)
}

/// Per-segment integrity report produced by [`inspect_wal`].
#[derive(Debug, Clone, Serialize)]
pub struct SegmentReport {
    pub path: PathBuf,
    pub first_sequence: u64,
    pub records: usize,
    pub bytes: u64,
    pub valid_bytes: u64,
    pub tail: String,
}

/// Verifies every WAL segment without modifying anything.
pub fn inspect_wal(
    layout: &DataLayout,
    max_record_bytes: usize,
) -> Result<Vec<SegmentReport>, StorageError> {
    let mut out = Vec::new();
    for segment in list_segments(&layout.wal_dir())? {
        let buf = read_file(&segment.path)?;
        let scan = frame::scan(&buf, WAL_MAGIC, max_record_bytes);
        for f in &scan.frames {
            decode_envelope(&buf, f, &segment.path)?;
        }
        let tail = match &scan.tail {
            Tail::Clean => "clean".to_string(),
            Tail::Torn { offset, reason } => format!("torn at {offset}: {reason}"),
            Tail::Corrupt { offset, reason } => format!("corrupt at {offset}: {reason}"),
        };
        out.push(SegmentReport {
            path: segment.path,
            first_sequence: segment.first_sequence,
            records: scan.frames.len(),
            bytes: segment.len,
            valid_bytes: scan.valid_len,
            tail,
        });
    }
    Ok(out)
}

/// Reads up to `limit` events with sequence `>= from` from the WAL.
pub fn read_events(
    layout: &DataLayout,
    from: u64,
    limit: usize,
    max_record_bytes: usize,
) -> Result<Vec<EventEnvelope>, StorageError> {
    let mut out = Vec::new();
    let segments = list_segments(&layout.wal_dir())?;
    for (i, segment) in segments.iter().enumerate() {
        // Skip segments that end before `from`.
        if let Some(next) = segments.get(i + 1)
            && next.first_sequence <= from
        {
            continue;
        }
        let buf = read_file(&segment.path)?;
        let scan = frame::scan(&buf, WAL_MAGIC, max_record_bytes);
        for f in scan.frames.iter().filter(|f| f.sequence >= from) {
            if out.len() >= limit {
                return Ok(out);
            }
            out.push(decode_envelope(&buf, f, &segment.path)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::LedgerEvent;
    use crate::domain::ids::TransactionId;
    use crate::store::wal::{SyncMode, Wal, WalOptions};
    use serde::Deserialize;
    use std::fs;

    /// Minimal replay target counting events.
    #[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
    struct Counter {
        last: u64,
        count: u64,
        last_ts: Option<DateTime<Utc>>,
    }

    impl ReplayTarget for Counter {
        fn last_sequence(&self) -> u64 {
            self.last
        }
        fn last_timestamp(&self) -> Option<DateTime<Utc>> {
            self.last_ts
        }
        fn apply_event(&mut self, envelope: &EventEnvelope) -> Result<(), String> {
            self.last = envelope.sequence;
            self.last_ts = Some(envelope.timestamp);
            self.count += 1;
            Ok(())
        }
    }

    fn write_events(layout: &DataLayout, n: usize, segment_max_bytes: u64) {
        layout.ensure().unwrap();
        let recovered = recover::<Counter>(layout, &RecoveryOptions::default()).unwrap();
        let mut wal = Wal::open(
            WalOptions {
                dir: layout.wal_dir(),
                segment_max_bytes,
                max_record_bytes: 1 << 20,
                sync: SyncMode::Never,
            },
            recovered.state.last,
            recovered.state.last_ts,
            recovered.active_segment,
        )
        .unwrap();
        for _ in 0..n {
            wal.append_with(|_, _| LedgerEvent::PendingPosted {
                transaction_id: TransactionId::generate(),
            })
            .unwrap();
        }
    }

    #[test]
    fn replays_all_segments() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        write_events(&layout, 25, 500);
        let r = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap();
        assert_eq!(r.state.count, 25);
        assert_eq!(r.report.last_sequence, 25);
        assert!(r.report.segments_scanned > 1);
        assert!(r.report.tail_damage.is_none());
        // Appending resumes at the right place.
        write_events(&layout, 5, 500);
        let r = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap();
        assert_eq!(r.state.count, 30);
    }

    #[test]
    fn torn_tail_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        write_events(&layout, 3, 1 << 20);
        let seg = list_segments(&layout.wal_dir()).unwrap().pop().unwrap();
        let len = seg.len;
        let f = fs::OpenOptions::new().write(true).open(&seg.path).unwrap();
        f.set_len(len - 10).unwrap();
        drop(f);
        let r = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap();
        assert_eq!(r.state.count, 2);
        let damage = r.report.tail_damage.unwrap();
        assert_eq!(damage.kind, "torn");
        assert!(damage.repaired);
        // File was physically truncated to the last valid frame.
        let after = fs::metadata(&seg.path).unwrap().len();
        assert_eq!(Some((seg.path.clone(), after)), r.active_segment);
        let again = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap();
        assert!(again.report.tail_damage.is_none());
    }

    #[test]
    fn read_only_recovery_does_not_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        write_events(&layout, 4, 1 << 20);
        let seg = list_segments(&layout.wal_dir()).unwrap().pop().unwrap();
        let f = fs::OpenOptions::new().write(true).open(&seg.path).unwrap();
        f.set_len(seg.len + 5).unwrap();
        drop(f);
        let opts = RecoveryOptions {
            up_to_sequence: Some(100),
            ..Default::default()
        };
        let r = recover::<Counter>(&layout, &opts).unwrap();
        assert_eq!(r.state.count, 4);
        assert!(!r.report.tail_damage.unwrap().repaired);
        assert_eq!(fs::metadata(&seg.path).unwrap().len(), seg.len + 5);
        let bounded = RecoveryOptions {
            up_to_sequence: Some(2),
            ..Default::default()
        };
        assert_eq!(
            recover::<Counter>(&layout, &bounded).unwrap().state.count,
            2
        );
    }

    #[test]
    fn corruption_in_middle_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        write_events(&layout, 3, 1 << 20);
        let seg = list_segments(&layout.wal_dir()).unwrap().pop().unwrap();
        let mut bytes = fs::read(&seg.path).unwrap();
        bytes[30] ^= 0xFF;
        fs::write(&seg.path, &bytes).unwrap();
        let err = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap_err();
        assert!(matches!(err, StorageError::Corrupt { .. }), "{err}");
        let opts = RecoveryOptions {
            truncate_corruption: true,
            ..Default::default()
        };
        let r = recover::<Counter>(&layout, &opts).unwrap();
        assert_eq!(r.state.count, 0);
        assert_eq!(r.report.tail_damage.unwrap().kind, "corrupt");
    }

    #[test]
    fn snapshot_plus_tail_replay_and_gap_detection() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        write_events(&layout, 10, 400);
        let full = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap();
        // Snapshot at sequence 6, then recover: only 4 events replayed.
        let snap = Counter {
            last: 6,
            count: 6,
            last_ts: None,
        };
        snapshot::write_snapshot(&layout.snapshot_dir(), 6, &snap).unwrap();
        let r = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap();
        assert_eq!(r.report.snapshot_sequence, Some(6));
        assert_eq!(r.report.events_replayed, 4);
        assert_eq!(r.state.count, full.state.count);

        // Removing the first segment without a covering snapshot is a gap.
        fs::remove_dir_all(layout.snapshot_dir()).unwrap();
        let first = list_segments(&layout.wal_dir()).unwrap().remove(0);
        fs::remove_file(first.path).unwrap();
        let err = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap_err();
        assert!(matches!(err, StorageError::SequenceGap { .. }), "{err}");
    }

    #[test]
    fn inspect_and_read_events() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        write_events(&layout, 12, 400);
        let reports = inspect_wal(&layout, 1 << 20).unwrap();
        assert_eq!(reports.iter().map(|r| r.records).sum::<usize>(), 12);
        assert!(reports.iter().all(|r| r.tail == "clean"));
        let events = read_events(&layout, 5, 3, 1 << 20).unwrap();
        assert_eq!(
            events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
        assert!(read_events(&layout, 100, 10, 1 << 20).unwrap().is_empty());
    }

    #[test]
    fn empty_data_dir_recovers_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        let r = recover::<Counter>(&layout, &RecoveryOptions::default()).unwrap();
        assert_eq!(r.state, Counter::default());
        assert!(r.active_segment.is_none());
    }
}
