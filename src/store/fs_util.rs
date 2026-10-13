//! Small filesystem helpers: data directory layout, process locking and
//! crash-safe file replacement.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::StorageError;

/// Paths of every file the ledger keeps under its data directory.
#[derive(Debug, Clone)]
pub struct DataLayout {
    root: PathBuf,
}

impl DataLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DataLayout { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn wal_dir(&self) -> PathBuf {
        self.root.join("wal")
    }

    pub fn snapshot_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }

    pub fn audit_path(&self) -> PathBuf {
        self.root.join("audit.log")
    }

    pub fn idempotency_path(&self) -> PathBuf {
        self.root.join("idempotency.log")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("LOCK")
    }

    /// Creates the directory tree if it does not exist yet.
    pub fn ensure(&self) -> Result<(), StorageError> {
        for dir in [self.root.clone(), self.wal_dir(), self.snapshot_dir()] {
            fs::create_dir_all(&dir)
                .map_err(|e| StorageError::io(format!("create {}", dir.display()), e))?;
        }
        Ok(())
    }
}

/// Lock mode for the data directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Single writer (the server, or mutating CLI commands).
    Exclusive,
    /// Read-only inspection; compatible with other shared holders only.
    Shared,
}

/// An OS-level advisory lock on the data directory, released on drop.
#[derive(Debug)]
pub struct DirLock {
    file: File,
    path: PathBuf,
}

impl DirLock {
    pub fn acquire(layout: &DataLayout, mode: LockMode) -> Result<Self, StorageError> {
        let path = layout.lock_path();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| StorageError::io(format!("open {}", path.display()), e))?;
        let result = match mode {
            LockMode::Exclusive => file.try_lock(),
            LockMode::Shared => file.try_lock_shared(),
        };
        match result {
            Ok(()) => Ok(DirLock { file, path }),
            Err(TryLockError::WouldBlock) => {
                Err(StorageError::Locked(layout.root().display().to_string()))
            }
            Err(TryLockError::Error(e)) => {
                Err(StorageError::io(format!("lock {}", path.display()), e))
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Writes `bytes` to `path` atomically: write a temp file, fsync it, then
/// rename over the destination and fsync the parent directory.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp)
            .map_err(|e| StorageError::io(format!("create {}", tmp.display()), e))?;
        file.write_all(bytes)
            .map_err(|e| StorageError::io(format!("write {}", tmp.display()), e))?;
        file.sync_all()
            .map_err(|e| StorageError::io(format!("fsync {}", tmp.display()), e))?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        StorageError::io(format!("rename {} -> {}", tmp.display(), path.display()), e)
    })?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Flushes directory metadata (new/renamed entries) to disk.
///
/// Windows does not support opening directories for fsync, and NTFS journals
/// metadata itself, so this is a no-op there.
pub fn sync_dir(dir: &Path) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| StorageError::io(format!("fsync dir {}", dir.display()), e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Reads a file fully, mapping errors with context.
pub fn read_file(path: &Path) -> Result<Vec<u8>, StorageError> {
    fs::read(path).map_err(|e| StorageError::io(format!("read {}", path.display()), e))
}

/// Truncates `path` to `len` bytes and syncs it.
pub fn truncate_file(path: &Path, len: u64) -> Result<(), StorageError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| StorageError::io(format!("open {}", path.display()), e))?;
    file.set_len(len)
        .map_err(|e| StorageError::io(format!("truncate {}", path.display()), e))?;
    file.sync_all()
        .map_err(|e| StorageError::io(format!("fsync {}", path.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_paths_are_under_root() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        layout.ensure().unwrap();
        assert!(layout.wal_dir().is_dir());
        assert!(layout.snapshot_dir().is_dir());
        assert!(layout.audit_path().starts_with(dir.path()));
        assert!(layout.idempotency_path().starts_with(dir.path()));
    }

    #[test]
    fn exclusive_lock_excludes_others() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DataLayout::new(dir.path());
        layout.ensure().unwrap();
        let first = DirLock::acquire(&layout, LockMode::Exclusive).unwrap();
        assert!(matches!(
            DirLock::acquire(&layout, LockMode::Exclusive),
            Err(StorageError::Locked(_))
        ));
        assert!(DirLock::acquire(&layout, LockMode::Shared).is_err());
        assert!(first.path().ends_with("LOCK"));
        drop(first);
        let shared_a = DirLock::acquire(&layout, LockMode::Shared).unwrap();
        let shared_b = DirLock::acquire(&layout, LockMode::Shared).unwrap();
        assert!(DirLock::acquire(&layout, LockMode::Exclusive).is_err());
        drop((shared_a, shared_b));
        assert!(DirLock::acquire(&layout, LockMode::Exclusive).is_ok());
    }

    #[test]
    fn atomic_write_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(read_file(&path).unwrap(), b"second");
        assert!(!path.with_extension("tmp").exists());
        truncate_file(&path, 3).unwrap();
        assert_eq!(read_file(&path).unwrap(), b"sec");
    }
}
