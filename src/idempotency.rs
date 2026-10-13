//! Request-level idempotency with payload fingerprinting, TTL expiry and
//! crash-safe persistence.
//!
//! A client sends an `Idempotency-Key` header with a mutating request. The
//! first request with a key executes and its response is stored; retries with
//! the same key *and the same payload fingerprint* receive the stored response
//! without re-executing. Reusing a key with a different payload is rejected,
//! and a retry that arrives while the first attempt is still running gets an
//! "in progress" answer.
//!
//! Completed records are appended to `idempotency.log` using the shared
//! checksummed framing and reloaded on start-up; expired records are dropped
//! and the file is rewritten when it accumulates enough dead entries.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::StorageError;
use crate::store::frame::{self, IDEMPOTENCY_MAGIC, Tail};
use crate::store::fs_util::{atomic_write, read_file, truncate_file};

/// Maximum accepted length of a client-supplied idempotency key.
pub const MAX_KEY_LEN: usize = 255;
const MAX_RECORD_BYTES: usize = 8 << 20;

/// Store configuration.
#[derive(Debug, Clone)]
pub struct IdempotencyConfig {
    pub ttl: Duration,
    pub max_entries: usize,
    pub sync: bool,
}

impl Default for IdempotencyConfig {
    fn default() -> Self {
        IdempotencyConfig {
            ttl: Duration::from_secs(24 * 3600),
            max_entries: 100_000,
            sync: true,
        }
    }
}

/// Validates a client-supplied key: 1..=255 visible ASCII characters.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(format!(
            "idempotency key must be 1..={MAX_KEY_LEN} characters"
        ));
    }
    if !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("idempotency key must contain only visible ASCII characters".into());
    }
    Ok(())
}

/// Fingerprint of a request: SHA-256 over method, path and canonical JSON body.
///
/// `serde_json::Value` objects are ordered maps, so serializing a parsed body
/// yields a canonical form independent of client key order and whitespace.
pub fn fingerprint(method: &str, path: &str, body: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update([0u8]);
    hasher.update(path.as_bytes());
    hasher.update([0u8]);
    hasher.update(body.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

/// A stored response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

/// A completed, persisted idempotency record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub fingerprint: String,
    pub response: StoredResponse,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

enum Entry {
    InFlight { fingerprint: String },
    Complete(IdempotencyRecord),
}

/// Outcome of starting an idempotent request.
pub enum Begin {
    /// First time this key is seen: execute, then call [`IdempotencyGuard::complete`].
    Proceed(IdempotencyGuard),
    /// Same key and payload already completed: return the stored response.
    Replay(StoredResponse),
    /// Same key, different payload.
    Mismatch,
    /// Same key and payload, first attempt still running.
    InProgress,
}

struct Inner {
    entries: HashMap<String, Entry>,
    /// Completed records ordered by expiry, for purging and eviction.
    by_expiry: BTreeSet<(DateTime<Utc>, String)>,
    file: File,
    next_sequence: u64,
    /// Records in the file that are no longer live.
    dead_records: usize,
}

/// Persistent idempotency store.
pub struct IdempotencyStore {
    path: PathBuf,
    config: IdempotencyConfig,
    inner: Mutex<Inner>,
}

/// Reservation for an in-flight request; releases the key if dropped
/// without completing (e.g. the handler failed with a server error).
pub struct IdempotencyGuard {
    store: Arc<IdempotencyStore>,
    key: String,
    fingerprint: String,
    done: bool,
}

impl IdempotencyGuard {
    /// Stores the response for future replays.
    pub fn complete(mut self, response: StoredResponse) -> Result<(), StorageError> {
        self.done = true;
        self.store
            .finish(&self.key, &self.fingerprint, response, Utc::now())
    }

    /// Releases the key without storing a response, so the client may retry.
    pub fn abandon(mut self) {
        self.done = true;
        self.store.release(&self.key);
    }
}

impl Drop for IdempotencyGuard {
    fn drop(&mut self) {
        if !self.done {
            self.store.release(&self.key);
        }
    }
}

/// What loading the store found on disk.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LoadReport {
    pub loaded: usize,
    pub expired: usize,
    pub damaged_tail: Option<String>,
}

impl IdempotencyStore {
    /// Opens the store, loading live records and compacting the file.
    pub fn open(
        path: impl Into<PathBuf>,
        config: IdempotencyConfig,
    ) -> Result<(Arc<Self>, LoadReport), StorageError> {
        let path = path.into();
        let now = Utc::now();
        let mut report = LoadReport::default();
        let mut records: HashMap<String, IdempotencyRecord> = HashMap::new();
        if path.exists() {
            let buf = read_file(&path)?;
            let scan = frame::scan(&buf, IDEMPOTENCY_MAGIC, MAX_RECORD_BYTES);
            for f in &scan.frames {
                match serde_json::from_slice::<IdempotencyRecord>(&buf[f.payload.clone()]) {
                    Ok(record) if record.expires_at > now => {
                        records.insert(record.key.clone(), record);
                    }
                    Ok(_) => report.expired += 1,
                    Err(e) => tracing::warn!(error = %e, "skipping undecodable idempotency record"),
                }
            }
            match &scan.tail {
                Tail::Clean => {}
                Tail::Torn { reason, .. } | Tail::Corrupt { reason, .. } => {
                    // The store is a cache of responses; keeping the valid
                    // prefix is the safe choice. Transaction creation has a
                    // second, WAL-backed idempotency check in the engine.
                    tracing::warn!(%reason, "discarding damaged idempotency log tail");
                    report.damaged_tail = Some(reason.clone());
                    truncate_file(&path, scan.valid_len)?;
                }
            }
        }
        report.loaded = records.len();
        // Rewrite the file with only live records.
        let mut bytes = Vec::new();
        let mut seq = 0u64;
        let mut ordered: Vec<&IdempotencyRecord> = records.values().collect();
        ordered.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.key.cmp(&b.key)));
        for record in &ordered {
            seq += 1;
            bytes.extend(frame::encode(
                IDEMPOTENCY_MAGIC,
                seq,
                &serde_json::to_vec(record)?,
            ));
        }
        atomic_write(&path, &bytes)?;
        let file = open_append(&path)?;
        let by_expiry = records
            .values()
            .map(|r| (r.expires_at, r.key.clone()))
            .collect();
        let entries = records
            .into_iter()
            .map(|(k, r)| (k, Entry::Complete(r)))
            .collect();
        let store = Arc::new(IdempotencyStore {
            path,
            config,
            inner: Mutex::new(Inner {
                entries,
                by_expiry,
                file,
                next_sequence: seq + 1,
                dead_records: 0,
            }),
        });
        Ok((store, report))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of completed, unexpired records.
    pub fn len(&self) -> usize {
        self.inner.lock().by_expiry.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Starts an idempotent request.
    pub fn begin(self: &Arc<Self>, key: &str, fingerprint: &str) -> Begin {
        let now = Utc::now();
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        match inner.entries.get(key) {
            Some(Entry::Complete(record)) if record.expires_at > now => {
                return if record.fingerprint == fingerprint {
                    Begin::Replay(record.response.clone())
                } else {
                    Begin::Mismatch
                };
            }
            Some(Entry::InFlight {
                fingerprint: running,
            }) => {
                return if running == fingerprint {
                    Begin::InProgress
                } else {
                    Begin::Mismatch
                };
            }
            Some(Entry::Complete(record)) => {
                // Expired but not yet purged: forget it.
                let marker = (record.expires_at, record.key.clone());
                inner.by_expiry.remove(&marker);
                inner.dead_records += 1;
            }
            None => {}
        }
        inner.entries.insert(
            key.to_string(),
            Entry::InFlight {
                fingerprint: fingerprint.to_string(),
            },
        );
        Begin::Proceed(IdempotencyGuard {
            store: self.clone(),
            key: key.to_string(),
            fingerprint: fingerprint.to_string(),
            done: false,
        })
    }

    fn release(&self, key: &str) {
        let mut inner = self.inner.lock();
        if matches!(inner.entries.get(key), Some(Entry::InFlight { .. })) {
            inner.entries.remove(key);
        }
    }

    fn finish(
        &self,
        key: &str,
        fingerprint: &str,
        response: StoredResponse,
        now: DateTime<Utc>,
    ) -> Result<(), StorageError> {
        let ttl = chrono::Duration::from_std(self.config.ttl).unwrap_or(chrono::Duration::days(1));
        let record = IdempotencyRecord {
            key: key.to_string(),
            fingerprint: fingerprint.to_string(),
            response,
            created_at: now,
            expires_at: now + ttl,
        };
        let payload = serde_json::to_vec(&record)?;
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        let seq = inner.next_sequence;
        let bytes = frame::encode(IDEMPOTENCY_MAGIC, seq, &payload);
        let write = inner.file.write_all(&bytes).and_then(|_| {
            if self.config.sync {
                inner.file.sync_data()
            } else {
                Ok(())
            }
        });
        if let Err(e) = write {
            inner.entries.remove(key);
            return Err(StorageError::io("append idempotency record", e));
        }
        inner.next_sequence += 1;
        inner.by_expiry.insert((record.expires_at, key.to_string()));
        inner
            .entries
            .insert(key.to_string(), Entry::Complete(record));
        while inner.by_expiry.len() > self.config.max_entries {
            let Some(oldest) = inner.by_expiry.pop_first() else {
                break;
            };
            inner.entries.remove(&oldest.1);
            inner.dead_records += 1;
        }
        Ok(())
    }

    /// Drops expired records; rewrites the file when more than half of it is
    /// dead. Returns the number of records removed.
    pub fn purge_expired(&self, now: DateTime<Utc>) -> Result<usize, StorageError> {
        let mut inner = self.inner.lock();
        let mut removed = 0;
        while let Some(first) = inner.by_expiry.first().cloned() {
            if first.0 > now {
                break;
            }
            inner.by_expiry.pop_first();
            if matches!(inner.entries.get(&first.1), Some(Entry::Complete(_))) {
                inner.entries.remove(&first.1);
            }
            removed += 1;
        }
        inner.dead_records += removed;
        let live = inner.by_expiry.len();
        if inner.dead_records > 0 && inner.dead_records >= live {
            self.rewrite(&mut inner)?;
        }
        Ok(removed)
    }

    fn rewrite(&self, inner: &mut Inner) -> Result<(), StorageError> {
        let mut bytes = Vec::new();
        let mut seq = 0;
        for (_, key) in inner.by_expiry.iter() {
            if let Some(Entry::Complete(record)) = inner.entries.get(key) {
                seq += 1;
                bytes.extend(frame::encode(
                    IDEMPOTENCY_MAGIC,
                    seq,
                    &serde_json::to_vec(record)?,
                ));
            }
        }
        atomic_write(&self.path, &bytes)?;
        inner.file = open_append(&self.path)?;
        inner.next_sequence = seq + 1;
        inner.dead_records = 0;
        Ok(())
    }

    /// Forces a rewrite of the backing file (used by `ledgerctl compact`).
    pub fn compact(&self) -> Result<(), StorageError> {
        let mut inner = self.inner.lock();
        self.rewrite(&mut inner)
    }
}

fn open_append(path: &Path) -> Result<File, StorageError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| StorageError::io(format!("open {}", path.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> IdempotencyConfig {
        IdempotencyConfig {
            ttl: Duration::from_secs(60),
            max_entries: 100,
            sync: false,
        }
    }

    fn response(n: u64) -> StoredResponse {
        StoredResponse {
            status: 201,
            body: json!({ "n": n }),
        }
    }

    #[test]
    fn fingerprint_is_canonical() {
        let a: serde_json::Value = serde_json::from_str(r#"{"b":1,"a":[1,2]}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{ "a": [1,2], "b": 1 }"#).unwrap();
        assert_eq!(fingerprint("POST", "/x", &a), fingerprint("POST", "/x", &b));
        assert_ne!(fingerprint("POST", "/x", &a), fingerprint("POST", "/y", &a));
        assert_ne!(
            fingerprint("POST", "/x", &a),
            fingerprint("POST", "/x", &json!({"b": 2, "a": [1, 2]}))
        );
    }

    #[test]
    fn key_validation() {
        assert!(validate_key("abc-123_XYZ").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("has space").is_err());
        assert!(validate_key(&"k".repeat(MAX_KEY_LEN + 1)).is_err());
    }

    #[test]
    fn replay_mismatch_and_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = IdempotencyStore::open(dir.path().join("idem.log"), config()).unwrap();
        let guard = match store.begin("k1", "fp") {
            Begin::Proceed(g) => g,
            _ => panic!("expected proceed"),
        };
        assert!(matches!(store.begin("k1", "fp"), Begin::InProgress));
        assert!(matches!(store.begin("k1", "other"), Begin::Mismatch));
        guard.complete(response(1)).unwrap();
        match store.begin("k1", "fp") {
            Begin::Replay(r) => assert_eq!(r, response(1)),
            _ => panic!("expected replay"),
        }
        assert!(matches!(store.begin("k1", "other"), Begin::Mismatch));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn dropped_guard_releases_key() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = IdempotencyStore::open(dir.path().join("idem.log"), config()).unwrap();
        match store.begin("k", "fp") {
            Begin::Proceed(g) => drop(g),
            _ => panic!(),
        }
        assert!(matches!(store.begin("k", "fp"), Begin::Proceed(_)));
        match store.begin("k2", "fp") {
            Begin::Proceed(g) => g.abandon(),
            _ => panic!(),
        }
        assert!(matches!(store.begin("k2", "fp"), Begin::Proceed(_)));
    }

    #[test]
    fn records_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idem.log");
        {
            let (store, _) = IdempotencyStore::open(&path, config()).unwrap();
            for i in 0..5 {
                if let Begin::Proceed(g) = store.begin(&format!("k{i}"), "fp") {
                    g.complete(response(i)).unwrap();
                }
            }
        }
        let (store, report) = IdempotencyStore::open(&path, config()).unwrap();
        assert_eq!(report.loaded, 5);
        match store.begin("k3", "fp") {
            Begin::Replay(r) => assert_eq!(r, response(3)),
            _ => panic!("expected replay after restart"),
        }
    }

    #[test]
    fn torn_tail_is_discarded_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idem.log");
        {
            let (store, _) = IdempotencyStore::open(&path, config()).unwrap();
            for i in 0..3 {
                if let Begin::Proceed(g) = store.begin(&format!("k{i}"), "fp") {
                    g.complete(response(i)).unwrap();
                }
            }
        }
        let len = std::fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 3).unwrap();
        drop(f);
        let (store, report) = IdempotencyStore::open(&path, config()).unwrap();
        assert_eq!(report.loaded, 2);
        assert!(report.damaged_tail.is_some());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn expiry_and_purge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idem.log");
        let (store, _) = IdempotencyStore::open(&path, config()).unwrap();
        for i in 0..4 {
            if let Begin::Proceed(g) = store.begin(&format!("k{i}"), "fp") {
                g.complete(response(i)).unwrap();
            }
        }
        let later = Utc::now() + chrono::Duration::seconds(120);
        assert_eq!(store.purge_expired(later).unwrap(), 4);
        assert!(store.is_empty());
        assert!(matches!(store.begin("k1", "fp"), Begin::Proceed(_)));
        // The file was rewritten without the dead records.
        let (_, report) = IdempotencyStore::open(&path, config()).unwrap();
        assert_eq!(report.loaded, 0);
    }

    #[test]
    fn capacity_evicts_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.max_entries = 3;
        let (store, _) = IdempotencyStore::open(dir.path().join("idem.log"), cfg).unwrap();
        for i in 0..5 {
            if let Begin::Proceed(g) = store.begin(&format!("k{i}"), "fp") {
                g.complete(response(i)).unwrap();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(store.len(), 3);
        assert!(matches!(store.begin("k0", "fp"), Begin::Proceed(_)));
        assert!(matches!(store.begin("k4", "fp"), Begin::Replay(_)));
        store.compact().unwrap();
    }
}
