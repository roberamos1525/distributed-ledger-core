//! Segmented, checksummed write-ahead log of ledger events.
//!
//! Segments live in `<data>/wal/` and are named after the sequence number of
//! their first event (`00000000000000000001.wal`). New events are appended to
//! the newest ("active") segment; once it exceeds the configured size a new
//! segment is started. Compaction removes segments that are entirely covered
//! by a snapshot.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::events::{EventEnvelope, LedgerEvent};
use crate::error::StorageError;
use crate::store::frame::{self, WAL_MAGIC};
use crate::store::fs_util::sync_dir;

const SEGMENT_EXT: &str = "wal";

/// Durability policy for appends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SyncMode {
    /// `fsync` after every append (safest, slowest).
    Always,
    /// `fsync` after every `every` appends; a crash may lose the unsynced tail.
    Batch { every: u32 },
    /// Never `fsync` explicitly; rely on the OS. For tests and benchmarks.
    Never,
}

/// Tunables for the WAL.
#[derive(Debug, Clone)]
pub struct WalOptions {
    pub dir: PathBuf,
    pub segment_max_bytes: u64,
    pub max_record_bytes: usize,
    pub sync: SyncMode,
}

/// A segment file discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SegmentInfo {
    pub path: PathBuf,
    pub first_sequence: u64,
    pub len: u64,
}

/// Lists WAL segments sorted by their first sequence number.
pub fn list_segments(dir: &Path) -> Result<Vec<SegmentInfo>, StorageError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(StorageError::io(format!("list {}", dir.display()), e)),
    };
    let mut segments = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| StorageError::io("read wal dir entry", e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(SEGMENT_EXT) {
            continue;
        }
        let Some(first_sequence) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let len = entry
            .metadata()
            .map_err(|e| StorageError::io(format!("stat {}", path.display()), e))?
            .len();
        segments.push(SegmentInfo {
            path,
            first_sequence,
            len,
        });
    }
    segments.sort_by_key(|s| s.first_sequence);
    Ok(segments)
}

/// Path of the segment whose first event has `first_sequence`.
pub fn segment_path(dir: &Path, first_sequence: u64) -> PathBuf {
    dir.join(format!("{first_sequence:020}.{SEGMENT_EXT}"))
}

/// Decodes the payload of a WAL frame into an envelope, checking that the
/// sequence number in the frame header matches the one in the payload.
pub fn decode_envelope(
    buf: &[u8],
    frame: &frame::Frame,
    file: &Path,
) -> Result<EventEnvelope, StorageError> {
    let envelope: EventEnvelope =
        serde_json::from_slice(&buf[frame.payload.clone()]).map_err(|e| StorageError::Corrupt {
            file: file.display().to_string(),
            offset: frame.offset,
            reason: format!("undecodable event: {e}"),
        })?;
    if envelope.sequence != frame.sequence {
        return Err(StorageError::Corrupt {
            file: file.display().to_string(),
            offset: frame.offset,
            reason: format!(
                "header sequence {} does not match payload sequence {}",
                frame.sequence, envelope.sequence
            ),
        });
    }
    Ok(envelope)
}

/// The append side of the write-ahead log.
#[derive(Debug)]
pub struct Wal {
    opts: WalOptions,
    file: File,
    path: PathBuf,
    len: u64,
    last_sequence: u64,
    last_timestamp: Option<DateTime<Utc>>,
    unsynced: u32,
    poisoned: Option<String>,
    appended_bytes: u64,
}

impl Wal {
    /// Opens the WAL for appending after recovery.
    ///
    /// `active` is the newest segment and its valid length as determined by
    /// recovery; when `None` a fresh segment is created for `last_sequence + 1`.
    pub fn open(
        opts: WalOptions,
        last_sequence: u64,
        last_timestamp: Option<DateTime<Utc>>,
        active: Option<(PathBuf, u64)>,
    ) -> Result<Self, StorageError> {
        fs::create_dir_all(&opts.dir)
            .map_err(|e| StorageError::io(format!("create {}", opts.dir.display()), e))?;
        let (path, len) = match active {
            Some((path, len)) => (path, len),
            None => (segment_path(&opts.dir, last_sequence + 1), 0),
        };
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| StorageError::io(format!("open {}", path.display()), e))?;
        // Drop anything past the last valid frame so new records are never
        // followed by stale bytes from a torn write.
        file.set_len(len)
            .map_err(|e| StorageError::io(format!("truncate {}", path.display()), e))?;
        file.seek(SeekFrom::Start(len))
            .map_err(|e| StorageError::io(format!("seek {}", path.display()), e))?;
        Ok(Wal {
            opts,
            file,
            path,
            len,
            last_sequence,
            last_timestamp,
            unsynced: 0,
            poisoned: None,
            appended_bytes: 0,
        })
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub fn last_timestamp(&self) -> Option<DateTime<Utc>> {
        self.last_timestamp
    }

    pub fn active_segment(&self) -> &Path {
        &self.path
    }

    /// Bytes appended since the WAL was opened.
    pub fn appended_bytes(&self) -> u64 {
        self.appended_bytes
    }

    pub fn is_poisoned(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    /// Assigns the next sequence number and a monotonic timestamp, builds the
    /// event with them, and durably appends it.
    ///
    /// On an I/O failure the segment is truncated back to its previous length
    /// so the log never contains a partially written record. If even that
    /// fails the WAL is poisoned and refuses further writes.
    pub fn append_with(
        &mut self,
        build: impl FnOnce(u64, DateTime<Utc>) -> LedgerEvent,
    ) -> Result<EventEnvelope, StorageError> {
        if let Some(reason) = &self.poisoned {
            return Err(StorageError::Poisoned(reason.clone()));
        }
        let sequence = self.last_sequence + 1;
        let now = Utc::now();
        let timestamp = match self.last_timestamp {
            Some(last) if last > now => last,
            _ => now,
        };
        let envelope = EventEnvelope {
            sequence,
            timestamp,
            event: build(sequence, timestamp),
        };
        let payload = serde_json::to_vec(&envelope)?;
        if payload.len() > self.opts.max_record_bytes {
            return Err(StorageError::Serde(format!(
                "event of {} bytes exceeds the WAL record limit of {}",
                payload.len(),
                self.opts.max_record_bytes
            )));
        }
        let bytes = frame::encode(WAL_MAGIC, sequence, &payload);
        if self.len > 0 && self.len + bytes.len() as u64 > self.opts.segment_max_bytes {
            self.rotate(sequence)?;
        }
        self.write_frame(&bytes)?;
        self.last_sequence = sequence;
        self.last_timestamp = Some(timestamp);
        Ok(envelope)
    }

    fn write_frame(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        let previous_len = self.len;
        let result = self.file.write_all(bytes).and_then(|_| self.maybe_sync());
        if let Err(e) = result {
            let rollback = self
                .file
                .set_len(previous_len)
                .and_then(|_| self.file.seek(SeekFrom::Start(previous_len)).map(|_| ()));
            if let Err(rollback_err) = rollback {
                self.poisoned = Some(format!(
                    "append failed ({e}) and rollback failed ({rollback_err})"
                ));
            }
            return Err(StorageError::io(
                format!("append to {}", self.path.display()),
                e,
            ));
        }
        self.len += bytes.len() as u64;
        self.appended_bytes += bytes.len() as u64;
        Ok(())
    }

    fn maybe_sync(&mut self) -> std::io::Result<()> {
        match self.opts.sync {
            SyncMode::Always => self.file.sync_data(),
            SyncMode::Batch { every } => {
                self.unsynced += 1;
                if self.unsynced >= every.max(1) {
                    self.unsynced = 0;
                    self.file.sync_data()
                } else {
                    Ok(())
                }
            }
            SyncMode::Never => Ok(()),
        }
    }

    /// Forces buffered data to disk.
    pub fn sync(&mut self) -> Result<(), StorageError> {
        self.unsynced = 0;
        self.file
            .sync_data()
            .map_err(|e| StorageError::io(format!("fsync {}", self.path.display()), e))
    }

    fn rotate(&mut self, next_sequence: u64) -> Result<(), StorageError> {
        self.sync()?;
        let path = segment_path(&self.opts.dir, next_sequence);
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| StorageError::io(format!("create {}", path.display()), e))?;
        sync_dir(&self.opts.dir)?;
        tracing::debug!(segment = %path.display(), "rotated WAL segment");
        self.file = file;
        self.path = path;
        self.len = 0;
        Ok(())
    }

    /// Starts a new segment so that everything up to the current sequence is
    /// in closed segments (used before compaction).
    pub fn seal_active_segment(&mut self) -> Result<(), StorageError> {
        if self.len == 0 {
            return Ok(());
        }
        self.rotate(self.last_sequence + 1)
    }

    /// Removes closed segments whose events are all `<= sequence`.
    /// Returns the removed paths. The active segment is never removed.
    pub fn remove_segments_through(&mut self, sequence: u64) -> Result<Vec<PathBuf>, StorageError> {
        let segments = list_segments(&self.opts.dir)?;
        let mut removed = Vec::new();
        for pair in segments.windows(2) {
            let (segment, next) = (&pair[0], &pair[1]);
            if segment.path == self.path {
                break;
            }
            // Every event in `segment` precedes `next.first_sequence`.
            if next.first_sequence <= sequence + 1 {
                fs::remove_file(&segment.path).map_err(|e| {
                    StorageError::io(format!("remove {}", segment.path.display()), e)
                })?;
                removed.push(segment.path.clone());
            } else {
                break;
            }
        }
        if !removed.is_empty() {
            sync_dir(&self.opts.dir)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::TransactionId;

    fn opts(dir: &Path, segment_max_bytes: u64) -> WalOptions {
        WalOptions {
            dir: dir.to_path_buf(),
            segment_max_bytes,
            max_record_bytes: 1 << 20,
            sync: SyncMode::Never,
        }
    }

    fn event() -> LedgerEvent {
        LedgerEvent::PendingPosted {
            transaction_id: TransactionId::generate(),
        }
    }

    #[test]
    fn appends_assign_sequences_and_monotonic_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(opts(dir.path(), 1 << 20), 0, None, None).unwrap();
        let a = wal.append_with(|_, _| event()).unwrap();
        let b = wal
            .append_with(|seq, _| {
                assert_eq!(seq, 2);
                event()
            })
            .unwrap();
        assert_eq!((a.sequence, b.sequence), (1, 2));
        assert!(b.timestamp >= a.timestamp);
        assert_eq!(wal.last_sequence(), 2);
        assert!(wal.appended_bytes() > 0);
    }

    #[test]
    fn timestamps_never_go_backwards() {
        let dir = tempfile::tempdir().unwrap();
        let future = Utc::now() + chrono::Duration::hours(1);
        let mut wal = Wal::open(opts(dir.path(), 1 << 20), 10, Some(future), None).unwrap();
        let env = wal.append_with(|_, _| event()).unwrap();
        assert_eq!(env.sequence, 11);
        assert_eq!(env.timestamp, future);
    }

    #[test]
    fn rotates_segments_and_compacts() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(opts(dir.path(), 300), 0, None, None).unwrap();
        for _ in 0..10 {
            wal.append_with(|_, _| event()).unwrap();
        }
        let segments = list_segments(dir.path()).unwrap();
        assert!(segments.len() > 2, "expected rotation, got {segments:?}");
        assert_eq!(segments[0].first_sequence, 1);

        let removed = wal.remove_segments_through(4).unwrap();
        let remaining = list_segments(dir.path()).unwrap();
        assert!(!removed.is_empty());
        // The first remaining segment must still contain sequence 5.
        assert!(remaining[0].first_sequence <= 5);
        // Removing through the latest sequence keeps the active segment.
        wal.seal_active_segment().unwrap();
        wal.remove_segments_through(wal.last_sequence()).unwrap();
        let remaining = list_segments(dir.path()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].first_sequence, 11);
        assert_eq!(remaining[0].len, 0);
    }

    #[test]
    fn oversized_records_are_rejected_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts(dir.path(), 1 << 20);
        o.max_record_bytes = 16;
        let mut wal = Wal::open(o, 0, None, None).unwrap();
        assert!(wal.append_with(|_, _| event()).is_err());
        assert_eq!(wal.last_sequence(), 0);
        let segments = list_segments(dir.path()).unwrap();
        assert_eq!(segments[0].len, 0);
    }

    #[test]
    fn written_frames_decode_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(opts(dir.path(), 1 << 20), 0, None, None).unwrap();
        let env = wal.append_with(|_, _| event()).unwrap();
        wal.sync().unwrap();
        let path = wal.active_segment().to_path_buf();
        let buf = fs::read(&path).unwrap();
        let scan = frame::scan(&buf, WAL_MAGIC, 1 << 20);
        assert!(scan.is_clean());
        let decoded = decode_envelope(&buf, &scan.frames[0], &path).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn ignores_foreign_files_in_wal_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        fs::write(dir.path().join("abc.wal"), b"x").unwrap();
        assert!(list_segments(dir.path()).unwrap().is_empty());
        assert!(
            list_segments(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }
}
