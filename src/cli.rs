//! `ledgerctl`: offline operations on a ledger data directory.
//!
//! Read-only commands (`replay`, `verify`, `events`, `balance`,
//! `trial-balance`) take a shared lock on the data directory and never modify
//! files; they fail while the server (which holds an exclusive lock) is
//! running unless `--no-lock` is given. Mutating commands (`snapshot`,
//! `compact`) open the ledger exactly like the server does.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::api::auth::{generate_key, hash_key};
use crate::audit;
use crate::config::{Config, ConfigError};
use crate::domain::ids::AccountId;
use crate::engine::ledger::Ledger;
use crate::engine::reports::{check_invariants, trial_balance};
use crate::engine::state::LedgerState;
use crate::error::{LedgerError, StorageError};
use crate::idempotency::IdempotencyStore;
use crate::metrics::LedgerMetrics;
use crate::store::fs_util::{DataLayout, DirLock, LockMode};
use crate::store::recovery::{self, RecoveryOptions, recover};

/// Command line interface definition.
#[derive(Debug, Parser)]
#[command(
    name = "ledgerctl",
    version,
    about = "Maintenance tool for distributed-ledger-core"
)]
pub struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, global = true, env = "LEDGER_CONFIG")]
    pub config: Option<PathBuf>,
    /// Data directory (overrides the configuration).
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    /// Skip the data directory lock for read-only commands (may observe a
    /// record that is being written).
    #[arg(long, global = true)]
    pub no_lock: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Rebuild state from snapshot + WAL and print a summary.
    Replay {
        /// Stop after this event sequence (point-in-time rebuild).
        #[arg(long)]
        to_sequence: Option<u64>,
        /// Replay the full WAL instead of starting from a snapshot.
        #[arg(long)]
        ignore_snapshots: bool,
    },
    /// Verify WAL checksums, snapshot consistency, ledger invariants and the
    /// audit hash chain. Exits with status 1 if anything is wrong.
    Verify {
        #[arg(long)]
        skip_audit: bool,
    },
    /// Write a snapshot of the current state.
    Snapshot,
    /// Snapshot, then delete WAL segments and old snapshots it covers, and
    /// rewrite the idempotency store without expired entries.
    Compact,
    /// Print events as JSON lines.
    Events {
        #[arg(long, default_value_t = 1)]
        from: u64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Print an account balance, optionally as of an event sequence.
    Balance {
        account: String,
        #[arg(long)]
        as_of_sequence: Option<u64>,
    },
    /// Print the trial balance.
    TrialBalance,
    /// Generate a new API key and the config entry with its hash.
    HashKey {
        /// Hash this key instead of generating a new one.
        #[arg(long)]
        key: Option<String>,
        #[arg(long, default_value = "admin")]
        role: String,
        #[arg(long, default_value = "ops")]
        id: String,
    },
    /// Validate the configuration and print the effective values.
    CheckConfig,
}

/// Errors surfaced to the CLI user.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("{0}")]
    Usage(String),
    #[error("output error: {0}")]
    Output(#[from] std::io::Error),
}

fn load_config(cli: &Cli) -> Result<Config, CliError> {
    let mut config = match &cli.config {
        Some(path) => Config::from_file(path)?,
        None => Config::default(),
    };
    config.apply_env(std::env::vars())?;
    if let Some(dir) = &cli.data_dir {
        config.storage.data_dir = dir.clone();
    }
    config.validate()?;
    Ok(config)
}

fn print(out: &mut impl Write, value: &Value) -> Result<(), CliError> {
    writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )?;
    Ok(())
}

fn read_only_state(
    cli: &Cli,
    config: &Config,
    to_sequence: Option<u64>,
    ignore_snapshots: bool,
) -> Result<(LedgerState, recovery::RecoveryReport, Option<DirLock>), CliError> {
    let layout = DataLayout::new(&config.storage.data_dir);
    if !layout.root().exists() {
        return Err(CliError::Usage(format!(
            "data directory {} does not exist",
            layout.root().display()
        )));
    }
    let lock = if cli.no_lock {
        None
    } else {
        Some(DirLock::acquire(&layout, LockMode::Shared)?)
    };
    let opts = RecoveryOptions {
        repair_torn_tail: false,
        truncate_corruption: false,
        max_record_bytes: config.storage.max_record_bytes,
        // A bound makes recovery read-only; u64::MAX keeps everything.
        up_to_sequence: Some(to_sequence.unwrap_or(u64::MAX)),
        ignore_snapshots,
    };
    let recovered = recover::<LedgerState>(&layout, &opts)?;
    Ok((recovered.state, recovered.report, lock))
}

/// Executes a parsed command, writing output to `out`. Returns the process
/// exit code.
pub fn run(cli: Cli, out: &mut impl Write) -> Result<i32, CliError> {
    match &cli.command {
        Command::HashKey { key, role, id } => {
            if crate::config::Role::parse(role).is_none() {
                return Err(CliError::Usage(format!(
                    "unknown role {role:?} (expected reader, writer or admin)"
                )));
            }
            let generated = key.is_none();
            let secret = match key {
                Some(k) => k.clone(),
                None => generate_key().map_err(CliError::Usage)?,
            };
            let hash = hash_key(&secret);
            print(
                out,
                &json!({
                    "key": if generated { Value::String(secret) } else { Value::Null },
                    "key_sha256": hash,
                    "config_toml": format!(
                        "[[auth.api_keys]]\nid = \"{id}\"\nrole = \"{role}\"\nkey_sha256 = \"{hash}\""
                    ),
                    "env": format!("LEDGER_API_KEYS={id}:{role}:{hash}"),
                }),
            )?;
            Ok(0)
        }
        Command::CheckConfig => {
            let config = load_config(&cli)?;
            let mut value = serde_json::to_value(&config).unwrap_or(Value::Null);
            // Hashes are not secrets, but there is no reason to echo them.
            if let Some(keys) = value["auth"]["api_keys"].as_array_mut() {
                for k in keys {
                    k["key_sha256"] = json!("<redacted>");
                }
            }
            print(out, &json!({"valid": true, "config": value}))?;
            Ok(0)
        }
        Command::Replay {
            to_sequence,
            ignore_snapshots,
        } => {
            let config = load_config(&cli)?;
            let (state, report, _lock) =
                read_only_state(&cli, &config, *to_sequence, *ignore_snapshots)?;
            let tb = trial_balance(&state);
            print(
                out,
                &json!({
                    "last_sequence": state.last_sequence,
                    "accounts": state.accounts.len(),
                    "transactions": state.transactions.len(),
                    "trial_balance_balanced": tb.balanced,
                    "recovery": report,
                }),
            )?;
            Ok(0)
        }
        Command::Verify { skip_audit } => {
            let config = load_config(&cli)?;
            verify(&cli, &config, *skip_audit, out)
        }
        Command::Snapshot => {
            let config = load_config(&cli)?;
            let ledger = Ledger::open(config.ledger_options(), Arc::new(LedgerMetrics::new()))?;
            let meta = ledger.snapshot()?;
            print(
                out,
                &json!({"snapshot": meta, "recovery": ledger.recovery_report()}),
            )?;
            Ok(0)
        }
        Command::Compact => {
            let config = load_config(&cli)?;
            let ledger = Ledger::open(config.ledger_options(), Arc::new(LedgerMetrics::new()))?;
            let report = ledger.compact()?;
            let layout = DataLayout::new(&config.storage.data_dir);
            let (idem, idem_report) =
                IdempotencyStore::open(layout.idempotency_path(), config.idempotency_config())?;
            idem.compact()?;
            print(
                out,
                &json!({"compaction": report, "idempotency": {"live_records": idem.len(), "expired_dropped": idem_report.expired}}),
            )?;
            Ok(0)
        }
        Command::Events { from, limit } => {
            let config = load_config(&cli)?;
            let layout = DataLayout::new(&config.storage.data_dir);
            let _lock = if cli.no_lock {
                None
            } else {
                Some(DirLock::acquire(&layout, LockMode::Shared)?)
            };
            for env in
                recovery::read_events(&layout, *from, *limit, config.storage.max_record_bytes)?
            {
                writeln!(out, "{}", serde_json::to_string(&env).unwrap_or_default())?;
            }
            Ok(0)
        }
        Command::Balance {
            account,
            as_of_sequence,
        } => {
            let config = load_config(&cli)?;
            let id = AccountId::new(account.as_str())?;
            let (state, _, _lock) = read_only_state(&cli, &config, *as_of_sequence, false)?;
            let acct = state
                .accounts
                .get(&id)
                .ok_or_else(|| LedgerError::AccountNotFound(id.clone()))?;
            let currency = config
                .ledger_options()
                .currencies
                .get(acct.account.currency);
            let fmt = |v: i64| currency.map(|c| c.format_amount(v));
            print(
                out,
                &json!({
                    "account_id": id,
                    "currency": acct.account.currency,
                    "as_of_sequence": state.last_sequence,
                    "posted": acct.posted_balance(),
                    "available": acct.available_balance(),
                    "posted_decimal": fmt(acct.posted_balance()),
                    "status": acct.account.status,
                    "version": acct.account.version,
                }),
            )?;
            Ok(0)
        }
        Command::TrialBalance => {
            let config = load_config(&cli)?;
            let (state, _, _lock) = read_only_state(&cli, &config, None, false)?;
            let tb = trial_balance(&state);
            print(out, &serde_json::to_value(&tb).unwrap_or(Value::Null))?;
            Ok(if tb.balanced { 0 } else { 1 })
        }
    }
}

fn verify(
    cli: &Cli,
    config: &Config,
    skip_audit: bool,
    out: &mut impl Write,
) -> Result<i32, CliError> {
    let layout = DataLayout::new(&config.storage.data_dir);
    let mut problems: Vec<String> = Vec::new();
    let segments = recovery::inspect_wal(&layout, config.storage.max_record_bytes)?;
    for s in &segments {
        if s.tail != "clean" {
            problems.push(format!("{}: {}", s.path.display(), s.tail));
        }
    }
    // Rebuild twice: from the newest snapshot and from the log alone (when
    // the log still starts at sequence 1). Both must agree.
    let rebuilt = match read_only_state(cli, config, None, false) {
        Ok(r) => Some(r),
        Err(e) => {
            problems.push(format!("recovery failed: {e}"));
            None
        }
    };
    let full_log_available = segments.first().is_none_or(|s| s.first_sequence == 1);
    let mut snapshot_matches_log = Value::Null;
    let mut invariants = Value::Null;
    let mut recovery_report = Value::Null;
    let mut last_sequence = Value::Null;
    if let Some((from_snapshot, report, _lock)) = &rebuilt {
        if full_log_available && report.snapshot_sequence.is_some() {
            let opts = RecoveryOptions {
                repair_torn_tail: false,
                truncate_corruption: false,
                max_record_bytes: config.storage.max_record_bytes,
                up_to_sequence: Some(u64::MAX),
                ignore_snapshots: true,
            };
            let from_log = recover::<LedgerState>(&layout, &opts)?.state;
            let same = &from_log == from_snapshot;
            if !same {
                problems
                    .push("state rebuilt from the WAL differs from snapshot-based recovery".into());
            }
            snapshot_matches_log = json!(same);
        }
        let report_inv = check_invariants(from_snapshot);
        problems.extend(report_inv.violations.iter().cloned());
        invariants = serde_json::to_value(&report_inv).unwrap_or(Value::Null);
        recovery_report = serde_json::to_value(report).unwrap_or(Value::Null);
        last_sequence = json!(from_snapshot.last_sequence);
    }
    let audit_report = if skip_audit {
        Value::Null
    } else {
        let r = audit::verify_file(&layout.audit_path())?;
        if !r.ok {
            problems.push(format!(
                "audit chain broken at record {:?}: {}",
                r.first_invalid_sequence,
                r.error.clone().unwrap_or_default()
            ));
        }
        serde_json::to_value(&r).unwrap_or(Value::Null)
    };
    let ok = problems.is_empty();
    print(
        out,
        &json!({
            "ok": ok,
            "last_sequence": last_sequence,
            "wal_segments": segments,
            "recovery": recovery_report,
            "snapshot_matches_log": snapshot_matches_log,
            "invariants": invariants,
            "audit": audit_report,
            "problems": problems,
        }),
    )?;
    Ok(if ok { 0 } else { 1 })
}
