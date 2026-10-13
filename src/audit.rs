//! Hash-chained, append-only audit trail.
//!
//! Each record stores the SHA-256 hash of its predecessor (`prev_hash`) and
//! its own hash computed over a canonical encoding of all its fields
//! including `prev_hash`. Changing, removing or reordering any record breaks
//! the chain from that point on, which [`verify_file`] detects.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::StorageError;
use crate::store::frame::{self, AUDIT_MAGIC, Tail};
use crate::store::fs_util::{read_file, truncate_file};

/// `prev_hash` of the very first record.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const MAX_RECORD_BYTES: usize = 1 << 20;

/// What happened, supplied by the caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub actor: String,
    pub action: String,
    pub resource: String,
    pub outcome: String,
    pub request_id: Option<String>,
    pub details: serde_json::Value,
}

impl AuditEntry {
    pub fn new(actor: &str, action: &str, resource: impl Into<String>) -> Self {
        AuditEntry {
            actor: actor.to_string(),
            action: action.to_string(),
            resource: resource.into(),
            outcome: "success".into(),
            request_id: None,
            details: serde_json::Value::Null,
        }
    }

    pub fn with_outcome(mut self, outcome: impl Into<String>) -> Self {
        self.outcome = outcome.into();
        self
    }

    pub fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

/// A persisted audit record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub entry: AuditEntry,
    pub prev_hash: String,
    pub hash: String,
}

/// Canonical hash input: every field except `hash`, in a fixed order.
#[derive(Serialize)]
struct HashInput<'a> {
    sequence: u64,
    timestamp: &'a DateTime<Utc>,
    actor: &'a str,
    action: &'a str,
    resource: &'a str,
    outcome: &'a str,
    request_id: &'a Option<String>,
    details: &'a serde_json::Value,
    prev_hash: &'a str,
}

/// Computes the chained hash of a record.
pub fn compute_hash(
    sequence: u64,
    timestamp: &DateTime<Utc>,
    entry: &AuditEntry,
    prev_hash: &str,
) -> String {
    let input = HashInput {
        sequence,
        timestamp,
        actor: &entry.actor,
        action: &entry.action,
        resource: &entry.resource,
        outcome: &entry.outcome,
        request_id: &entry.request_id,
        details: &entry.details,
        prev_hash,
    };
    // Serializing a plain struct with a JSON `Value` (ordered map) is
    // deterministic, so the same record always hashes the same way.
    let bytes = serde_json::to_vec(&input).expect("audit hash input is serializable");
    hex::encode(Sha256::digest(&bytes))
}

struct Inner {
    file: File,
    last_sequence: u64,
    last_hash: String,
}

/// The audit log writer.
pub struct AuditLog {
    path: PathBuf,
    sync: bool,
    inner: Mutex<Inner>,
}

/// Result of verifying the chain.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerifyReport {
    pub ok: bool,
    pub records: u64,
    pub last_sequence: u64,
    pub last_hash: String,
    pub first_invalid_sequence: Option<u64>,
    pub error: Option<String>,
}

impl AuditLog {
    /// Opens (or creates) the audit log, repairing a torn tail.
    pub fn open(path: impl Into<PathBuf>, sync: bool) -> Result<Self, StorageError> {
        let path = path.into();
        let (last_sequence, last_hash) = if path.exists() {
            let buf = read_file(&path)?;
            let scan = frame::scan(&buf, AUDIT_MAGIC, MAX_RECORD_BYTES);
            match &scan.tail {
                Tail::Clean => {}
                Tail::Torn { reason, .. } => {
                    tracing::warn!(%reason, "truncating torn audit log tail");
                    truncate_file(&path, scan.valid_len)?;
                }
                Tail::Corrupt { offset, reason } => {
                    return Err(StorageError::Corrupt {
                        file: path.display().to_string(),
                        offset: *offset,
                        reason: reason.clone(),
                    });
                }
            }
            match scan.frames.last() {
                Some(f) => {
                    let record: AuditRecord = serde_json::from_slice(&buf[f.payload.clone()])?;
                    (record.sequence, record.hash)
                }
                None => (0, GENESIS_HASH.to_string()),
            }
        } else {
            (0, GENESIS_HASH.to_string())
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| StorageError::io(format!("open {}", path.display()), e))?;
        Ok(AuditLog {
            path,
            sync,
            inner: Mutex::new(Inner {
                file,
                last_sequence,
                last_hash,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn last_sequence(&self) -> u64 {
        self.inner.lock().last_sequence
    }

    /// Appends a record, chaining it to the previous one.
    pub fn append(&self, entry: AuditEntry) -> Result<AuditRecord, StorageError> {
        let mut inner = self.inner.lock();
        let sequence = inner.last_sequence + 1;
        let timestamp = Utc::now();
        let hash = compute_hash(sequence, &timestamp, &entry, &inner.last_hash);
        let record = AuditRecord {
            sequence,
            timestamp,
            entry,
            prev_hash: inner.last_hash.clone(),
            hash,
        };
        let payload = serde_json::to_vec(&record)?;
        let bytes = frame::encode(AUDIT_MAGIC, sequence, &payload);
        inner
            .file
            .write_all(&bytes)
            .and_then(|_| {
                if self.sync {
                    inner.file.sync_data()
                } else {
                    Ok(())
                }
            })
            .map_err(|e| StorageError::io("append audit record", e))?;
        inner.last_sequence = sequence;
        inner.last_hash = record.hash.clone();
        Ok(record)
    }

    /// Reads records with `sequence > after`, up to `limit`.
    pub fn list(&self, after: u64, limit: usize) -> Result<Vec<AuditRecord>, StorageError> {
        // Hold the writer lock so we never read a half-written frame.
        let _inner = self.inner.lock();
        let buf = read_file(&self.path)?;
        let scan = frame::scan(&buf, AUDIT_MAGIC, MAX_RECORD_BYTES);
        let mut out = Vec::new();
        for f in scan
            .frames
            .iter()
            .filter(|f| f.sequence > after)
            .take(limit)
        {
            out.push(serde_json::from_slice(&buf[f.payload.clone()])?);
        }
        Ok(out)
    }

    /// Verifies the on-disk chain.
    pub fn verify(&self) -> Result<VerifyReport, StorageError> {
        let _inner = self.inner.lock();
        verify_file(&self.path)
    }
}

/// Verifies an audit file: framing, sequence continuity, hash links and
/// record hashes. Reports the first broken record.
pub fn verify_file(path: &Path) -> Result<VerifyReport, StorageError> {
    let buf = if path.exists() {
        read_file(path)?
    } else {
        Vec::new()
    };
    let scan = frame::scan(&buf, AUDIT_MAGIC, MAX_RECORD_BYTES);
    let mut prev_hash = GENESIS_HASH.to_string();
    let mut expected = 1u64;
    let mut records = 0u64;
    let fail = |seq: u64, error: String, records: u64, last_hash: String| VerifyReport {
        ok: false,
        records,
        last_sequence: seq.saturating_sub(1),
        last_hash,
        first_invalid_sequence: Some(seq),
        error: Some(error),
    };
    for f in &scan.frames {
        let record: AuditRecord = match serde_json::from_slice(&buf[f.payload.clone()]) {
            Ok(r) => r,
            Err(e) => {
                return Ok(fail(
                    expected,
                    format!("undecodable record: {e}"),
                    records,
                    prev_hash,
                ));
            }
        };
        if record.sequence != expected {
            return Ok(fail(
                expected,
                format!("expected sequence {expected}, found {}", record.sequence),
                records,
                prev_hash,
            ));
        }
        if record.prev_hash != prev_hash {
            return Ok(fail(
                expected,
                "prev_hash does not match the previous record's hash".into(),
                records,
                prev_hash,
            ));
        }
        let computed = compute_hash(
            record.sequence,
            &record.timestamp,
            &record.entry,
            &record.prev_hash,
        );
        if computed != record.hash {
            return Ok(fail(
                expected,
                "record hash does not match its contents".into(),
                records,
                prev_hash,
            ));
        }
        prev_hash = record.hash;
        expected += 1;
        records += 1;
    }
    if let Tail::Torn { reason, .. } | Tail::Corrupt { reason, .. } = &scan.tail {
        return Ok(fail(
            expected,
            format!("damaged frame: {reason}"),
            records,
            prev_hash,
        ));
    }
    Ok(VerifyReport {
        ok: true,
        records,
        last_sequence: expected - 1,
        last_hash: prev_hash,
        first_invalid_sequence: None,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(i: u64) -> AuditEntry {
        AuditEntry::new("admin-key", "transaction.create", format!("tx-{i}"))
            .with_details(json!({ "amount": i * 100 }))
            .with_request_id(Some(format!("req-{i}")))
    }

    fn write(path: &Path, n: u64) -> AuditLog {
        let log = AuditLog::open(path, false).unwrap();
        for i in 0..n {
            log.append(entry(i)).unwrap();
        }
        log
    }

    #[test]
    fn chain_links_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = write(&path, 3);
        let records = log.list(0, 10).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].prev_hash, GENESIS_HASH);
        assert_eq!(records[1].prev_hash, records[0].hash);
        assert_eq!(records[2].prev_hash, records[1].hash);
        let report = log.verify().unwrap();
        assert!(report.ok);
        assert_eq!(report.records, 3);
        assert_eq!(report.last_hash, records[2].hash);
        assert_eq!(log.list(1, 1).unwrap()[0].sequence, 2);
    }

    #[test]
    fn chain_continues_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        drop(write(&path, 2));
        let log = AuditLog::open(&path, false).unwrap();
        assert_eq!(log.last_sequence(), 2);
        log.append(entry(9)).unwrap();
        assert!(log.verify().unwrap().ok);
    }

    #[test]
    fn tampering_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        drop(write(&path, 4));
        // Rewrite record 2 with a modified actor but a valid frame checksum,
        // simulating an attacker who can rewrite the file.
        let buf = std::fs::read(&path).unwrap();
        let scan = frame::scan(&buf, AUDIT_MAGIC, MAX_RECORD_BYTES);
        let mut out = Vec::new();
        for f in &scan.frames {
            let mut record: AuditRecord = serde_json::from_slice(&buf[f.payload.clone()]).unwrap();
            if record.sequence == 2 {
                record.entry.actor = "mallory".into();
            }
            out.extend(frame::encode(
                AUDIT_MAGIC,
                record.sequence,
                &serde_json::to_vec(&record).unwrap(),
            ));
        }
        std::fs::write(&path, out).unwrap();
        let report = verify_file(&path).unwrap();
        assert!(!report.ok);
        assert_eq!(report.first_invalid_sequence, Some(2));
        assert_eq!(report.records, 1);
    }

    #[test]
    fn deleted_record_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        drop(write(&path, 3));
        let buf = std::fs::read(&path).unwrap();
        let scan = frame::scan(&buf, AUDIT_MAGIC, MAX_RECORD_BYTES);
        let mut out = Vec::new();
        for f in scan.frames.iter().filter(|f| f.sequence != 2) {
            out.extend_from_slice(&buf[f.offset as usize..f.payload.end]);
        }
        std::fs::write(&path, out).unwrap();
        let report = verify_file(&path).unwrap();
        assert_eq!(report.first_invalid_sequence, Some(2));
    }

    #[test]
    fn torn_tail_is_repaired_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        drop(write(&path, 3));
        let len = std::fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - 4)
            .unwrap();
        assert!(!verify_file(&path).unwrap().ok);
        let log = AuditLog::open(&path, false).unwrap();
        assert_eq!(log.last_sequence(), 2);
        assert!(log.verify().unwrap().ok);
    }

    #[test]
    fn empty_log_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let report = verify_file(&dir.path().join("missing.log")).unwrap();
        assert!(report.ok);
        assert_eq!(report.records, 0);
        assert_eq!(report.last_hash, GENESIS_HASH);
    }
}
