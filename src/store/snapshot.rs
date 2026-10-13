//! Point-in-time snapshots of the materialized ledger state.
//!
//! A snapshot is a single checksummed frame (see [`frame`]) whose sequence
//! field is the last event sequence included in the state. Files are written
//! atomically (temp file + rename) and named `snapshot-<sequence>.snap` so the
//! newest one sorts last. Recovery falls back to older snapshots when the
//! newest one fails verification.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::StorageError;
use crate::store::frame::{self, SNAPSHOT_MAGIC, Tail};
use crate::store::fs_util::{atomic_write, read_file, sync_dir};

const PREFIX: &str = "snapshot-";
const EXT: &str = "snap";

/// Description of a snapshot file on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotMeta {
    pub sequence: u64,
    pub path: PathBuf,
    pub size: u64,
}

fn snapshot_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("{PREFIX}{sequence:020}.{EXT}"))
}

/// Serializes `state` and writes it as the snapshot for `sequence`.
pub fn write_snapshot<T: Serialize>(
    dir: &Path,
    sequence: u64,
    state: &T,
) -> Result<SnapshotMeta, StorageError> {
    fs::create_dir_all(dir)
        .map_err(|e| StorageError::io(format!("create {}", dir.display()), e))?;
    let payload = serde_json::to_vec(state)?;
    if payload.len() > u32::MAX as usize {
        return Err(StorageError::Serde(
            "snapshot payload exceeds 4 GiB frame limit".into(),
        ));
    }
    let bytes = frame::encode(SNAPSHOT_MAGIC, sequence, &payload);
    let path = snapshot_path(dir, sequence);
    atomic_write(&path, &bytes)?;
    Ok(SnapshotMeta {
        sequence,
        size: bytes.len() as u64,
        path,
    })
}

/// Lists snapshot files, newest first.
pub fn list_snapshots(dir: &Path) -> Result<Vec<SnapshotMeta>, StorageError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(StorageError::io(format!("list {}", dir.display()), e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| StorageError::io("read snapshot dir entry", e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXT) {
            continue;
        }
        let Some(sequence) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix(PREFIX))
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let size = entry
            .metadata()
            .map_err(|e| StorageError::io(format!("stat {}", path.display()), e))?
            .len();
        out.push(SnapshotMeta {
            sequence,
            path,
            size,
        });
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.sequence));
    Ok(out)
}

/// Loads and verifies a snapshot file.
pub fn load_snapshot<T: DeserializeOwned>(meta: &SnapshotMeta) -> Result<T, StorageError> {
    let buf = read_file(&meta.path)?;
    let scan = frame::scan(&buf, SNAPSHOT_MAGIC, u32::MAX as usize);
    let corrupt = |reason: String| StorageError::Corrupt {
        file: meta.path.display().to_string(),
        offset: 0,
        reason,
    };
    match (&scan.tail, scan.frames.as_slice()) {
        (Tail::Clean, [only]) => {
            if only.sequence != meta.sequence {
                return Err(corrupt(format!(
                    "file name says sequence {} but header says {}",
                    meta.sequence, only.sequence
                )));
            }
            serde_json::from_slice(&buf[only.payload.clone()])
                .map_err(|e| corrupt(format!("undecodable snapshot: {e}")))
        }
        (Tail::Clean, frames) => Err(corrupt(format!(
            "expected exactly one frame, found {}",
            frames.len()
        ))),
        (Tail::Torn { reason, .. } | Tail::Corrupt { reason, .. }, _) => {
            Err(corrupt(reason.clone()))
        }
    }
}

/// Result of looking for the newest usable snapshot.
#[derive(Debug)]
pub struct LatestSnapshot<T> {
    pub loaded: Option<(T, SnapshotMeta)>,
    /// Snapshots that failed verification, with the reason.
    pub skipped: Vec<(SnapshotMeta, String)>,
}

/// Loads the newest snapshot that passes verification, optionally ignoring
/// snapshots newer than `max_sequence`.
pub fn load_latest<T: DeserializeOwned>(
    dir: &Path,
    max_sequence: Option<u64>,
) -> Result<LatestSnapshot<T>, StorageError> {
    let mut skipped = Vec::new();
    for meta in list_snapshots(dir)? {
        if max_sequence.is_some_and(|max| meta.sequence > max) {
            continue;
        }
        match load_snapshot::<T>(&meta) {
            Ok(state) => {
                return Ok(LatestSnapshot {
                    loaded: Some((state, meta)),
                    skipped,
                });
            }
            Err(e) => {
                tracing::warn!(snapshot = %meta.path.display(), error = %e, "skipping unusable snapshot");
                skipped.push((meta, e.to_string()));
            }
        }
    }
    Ok(LatestSnapshot {
        loaded: None,
        skipped,
    })
}

/// Deletes all but the newest `keep` snapshots. Returns removed paths.
pub fn prune_snapshots(dir: &Path, keep: usize) -> Result<Vec<PathBuf>, StorageError> {
    let keep = keep.max(1);
    let mut removed = Vec::new();
    for meta in list_snapshots(dir)?.into_iter().skip(keep) {
        fs::remove_file(&meta.path)
            .map_err(|e| StorageError::io(format!("remove {}", meta.path.display()), e))?;
        removed.push(meta.path);
    }
    if !removed.is_empty() {
        sync_dir(dir)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Dummy {
        value: u64,
        name: String,
    }

    fn dummy(value: u64) -> Dummy {
        Dummy {
            value,
            name: format!("state-{value}"),
        }
    }

    #[test]
    fn write_list_and_load() {
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(dir.path(), 5, &dummy(5)).unwrap();
        write_snapshot(dir.path(), 12, &dummy(12)).unwrap();
        let list = list_snapshots(dir.path()).unwrap();
        assert_eq!(
            list.iter().map(|m| m.sequence).collect::<Vec<_>>(),
            vec![12, 5]
        );
        let latest = load_latest::<Dummy>(dir.path(), None).unwrap();
        let (state, meta) = latest.loaded.unwrap();
        assert_eq!(state, dummy(12));
        assert_eq!(meta.sequence, 12);
        let bounded = load_latest::<Dummy>(dir.path(), Some(10)).unwrap();
        assert_eq!(bounded.loaded.unwrap().0, dummy(5));
    }

    #[test]
    fn corrupt_snapshot_falls_back_to_older() {
        let dir = tempfile::tempdir().unwrap();
        write_snapshot(dir.path(), 1, &dummy(1)).unwrap();
        let newest = write_snapshot(dir.path(), 2, &dummy(2)).unwrap();
        let mut bytes = fs::read(&newest.path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x55;
        fs::write(&newest.path, bytes).unwrap();
        let latest = load_latest::<Dummy>(dir.path(), None).unwrap();
        assert_eq!(latest.skipped.len(), 1);
        assert_eq!(latest.loaded.unwrap().0, dummy(1));
    }

    #[test]
    fn renamed_snapshot_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let meta = write_snapshot(dir.path(), 3, &dummy(3)).unwrap();
        let wrong = snapshot_path(dir.path(), 9);
        fs::rename(&meta.path, &wrong).unwrap();
        let list = list_snapshots(dir.path()).unwrap();
        assert!(load_snapshot::<Dummy>(&list[0]).is_err());
    }

    #[test]
    fn prune_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        for seq in 1..=5 {
            write_snapshot(dir.path(), seq, &dummy(seq)).unwrap();
        }
        let removed = prune_snapshots(dir.path(), 2).unwrap();
        assert_eq!(removed.len(), 3);
        let left: Vec<_> = list_snapshots(dir.path())
            .unwrap()
            .into_iter()
            .map(|m| m.sequence)
            .collect();
        assert_eq!(left, vec![5, 4]);
    }

    #[test]
    fn empty_directory_has_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let latest = load_latest::<Dummy>(&dir.path().join("nope"), None).unwrap();
        assert!(latest.loaded.is_none());
        assert!(latest.skipped.is_empty());
    }
}
