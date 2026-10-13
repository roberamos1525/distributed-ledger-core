//! End-to-end tests of the `ledgerctl` binary.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::*;
use ledger_core::api::auth::hash_key;
use ledger_core::domain::account::AccountType;
use ledger_core::store::frame;
use ledger_core::store::fs_util::DataLayout;
use ledger_core::store::wal::list_segments;
use serde_json::Value;

fn ledgerctl(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ledgerctl"))
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env_remove("LEDGER_CONFIG")
        .env("LEDGER_FSYNC", "never")
        .output()
        .expect("ledgerctl runs")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON ({e}): stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Creates a small ledger on disk and returns the sequence after the 2nd transfer.
fn seed(dir: &Path) -> u64 {
    let ledger = open_ledger(dir);
    open_accounts(&ledger, &["cash", "bank"], AccountType::Asset);
    transfer(&ledger, "cash", "bank", 100).unwrap();
    let mid = transfer(&ledger, "cash", "bank", 250)
        .unwrap()
        .created_sequence;
    ledger.snapshot().unwrap();
    transfer(&ledger, "bank", "cash", 50).unwrap();
    mid
}

#[test]
fn hash_key_generates_matching_hash() {
    let dir = tempfile::tempdir().unwrap();
    let out = ledgerctl(dir.path(), &["hash-key", "--role", "writer", "--id", "svc"]);
    assert!(out.status.success());
    let v = json(&out);
    let key = v["key"].as_str().unwrap();
    assert!(key.starts_with("ldg_"));
    assert_eq!(v["key_sha256"], hash_key(key));
    assert!(
        v["env"]
            .as_str()
            .unwrap()
            .starts_with("LEDGER_API_KEYS=svc:writer:")
    );
    let bad = ledgerctl(dir.path(), &["hash-key", "--role", "root"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn replay_balance_events_and_trial_balance() {
    let dir = tempfile::tempdir().unwrap();
    let mid = seed(dir.path());

    let replay = json(&ledgerctl(dir.path(), &["replay"]));
    assert_eq!(replay["last_sequence"], 5);
    assert_eq!(replay["transactions"], 3);
    assert_eq!(replay["trial_balance_balanced"], true);
    assert_eq!(replay["recovery"]["snapshot_sequence"], 4);

    let full = json(&ledgerctl(dir.path(), &["replay", "--ignore-snapshots"]));
    assert_eq!(full["recovery"]["events_replayed"], 5);

    let past = json(&ledgerctl(
        dir.path(),
        &["balance", "bank", "--as-of-sequence", &mid.to_string()],
    ));
    assert_eq!(past["posted"], -350);
    let now = json(&ledgerctl(dir.path(), &["balance", "bank"]));
    assert_eq!(now["posted"], -300);
    assert_eq!(now["posted_decimal"], "-3.00");

    let events = ledgerctl(dir.path(), &["events", "--from", "2", "--limit", "2"]);
    let lines: Vec<Value> = String::from_utf8_lossy(&events.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["sequence"], 2);
    assert_eq!(lines[1]["event"]["type"], "transaction_recorded");

    let tb = ledgerctl(dir.path(), &["trial-balance"]);
    assert!(tb.status.success());
    assert_eq!(json(&tb)["balanced"], true);
}

#[test]
fn verify_passes_then_detects_tampering() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let ok = ledgerctl(dir.path(), &["verify"]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stdout)
    );
    let v = json(&ok);
    assert_eq!(v["ok"], true);
    assert_eq!(v["snapshot_matches_log"], true);

    let seg = list_segments(&DataLayout::new(dir.path()).wal_dir())
        .unwrap()
        .remove(0);
    let mut bytes = std::fs::read(&seg.path).unwrap();
    let scan = frame::scan(&bytes, frame::WAL_MAGIC, 1 << 20);
    let target = scan.frames[1].payload.start + 5;
    bytes[target] ^= 0x10;
    std::fs::write(&seg.path, bytes).unwrap();
    let bad = ledgerctl(dir.path(), &["verify"]);
    assert_eq!(bad.status.code(), Some(1));
    let v = json(&bad);
    assert_eq!(v["ok"], false);
    assert!(!v["problems"].as_array().unwrap().is_empty());
}

#[test]
fn snapshot_and_compact_commands() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let snap = ledgerctl(dir.path(), &["snapshot"]);
    assert!(snap.status.success());
    assert_eq!(json(&snap)["snapshot"]["sequence"], 5);
    let compact = ledgerctl(dir.path(), &["compact"]);
    assert!(
        compact.status.success(),
        "{}",
        String::from_utf8_lossy(&compact.stderr)
    );
    let v = json(&compact);
    assert_eq!(v["compaction"]["snapshot"]["sequence"], 5);
    // Everything is still recoverable after compaction.
    let replay = json(&ledgerctl(dir.path(), &["replay"]));
    assert_eq!(replay["last_sequence"], 5);
    assert_eq!(replay["recovery"]["events_replayed"], 0);
}

#[test]
fn read_only_commands_respect_the_server_lock() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let ledger = open_ledger(dir.path());
    let locked = ledgerctl(dir.path(), &["replay"]);
    assert_eq!(locked.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&locked.stderr).contains("locked"));
    let bypass = ledgerctl(dir.path(), &["--no-lock", "replay"]);
    assert!(bypass.status.success());
    drop(ledger);
}

#[test]
fn check_config_validates_files() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.toml");
    std::fs::write(&good, "[server]\nrequest_timeout_secs = 5\n").unwrap();
    let out = ledgerctl(
        dir.path(),
        &["--config", good.to_str().unwrap(), "check-config"],
    );
    assert!(out.status.success());
    assert_eq!(json(&out)["config"]["server"]["request_timeout_secs"], 5);
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "[ledger]\nmax_postings = 1\n").unwrap();
    let out = ledgerctl(
        dir.path(),
        &["--config", bad.to_str().unwrap(), "check-config"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("max_postings"));
    let missing = ledgerctl(&dir.path().join("nope"), &["replay"]);
    assert_eq!(missing.status.code(), Some(2));
}
