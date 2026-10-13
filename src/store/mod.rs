//! File-based persistence: checksummed framing, segmented WAL, snapshots and
//! crash recovery. Pure Rust, no external services.

pub mod frame;
pub mod fs_util;
pub mod recovery;
pub mod snapshot;
pub mod wal;

pub use fs_util::{DataLayout, DirLock, LockMode};
pub use recovery::{RecoveryOptions, RecoveryReport, ReplayTarget, recover};
pub use snapshot::SnapshotMeta;
pub use wal::{SyncMode, Wal, WalOptions};
