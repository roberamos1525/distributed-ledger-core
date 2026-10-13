//! The live, concurrent ledger engine.
//!
//! Commands follow one pattern:
//!
//! 1. validate the request shape (no locks),
//! 2. take the shared commit gate and lock the involved transaction/accounts
//!    in the global order (see [`super::locks`]),
//! 3. validate against current state (status, currency, versions, policies),
//! 4. append exactly one event to the WAL (assigning its sequence),
//! 5. apply that event through the same [`apply_event`] used by replay,
//! 6. publish new records to the indexes and release the locks.
//!
//! Because state only changes by applying persisted events, the in-memory
//! state is always reproducible from the log.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use crate::domain::account::{Account, AccountState, Balances, Effect};
use crate::domain::events::{EventEnvelope, LedgerEvent, ReversedLeg};
use crate::domain::ids::{AccountId, TransactionId, validate_metadata};
use crate::domain::money::CurrencyRegistry;
use crate::domain::transaction::{
    IdempotencyTag, Posting, TransactionKind, TransactionRecord, TransactionStatus, check_balanced,
    validate_postings,
};
use crate::engine::commands::{
    ChangePolicy, ChangeStatus, CreateTransaction, DefaultPolicies, EngineLimits, OpenAccount,
    PostingMode, ReverseTransaction, UpdateMetadata, validate_reason,
};
use crate::engine::locks::{AccountCell, LockedAccounts};
use crate::engine::state::{LedgerState, LedgerView, apply_event};
use crate::error::{LedgerError, LedgerResult, StorageError};
use crate::metrics::LedgerMetrics;
use crate::store::fs_util::{DataLayout, DirLock, LockMode};
use crate::store::recovery::{RecoveryOptions, RecoveryReport, recover};
use crate::store::snapshot::{self, SnapshotMeta};
use crate::store::wal::{SyncMode, Wal, WalOptions};

/// Shared, individually lockable transaction record.
pub(crate) type TxCell = Arc<Mutex<TransactionRecord>>;

/// Everything needed to open a ledger.
#[derive(Debug, Clone)]
pub struct LedgerOptions {
    pub data_dir: PathBuf,
    pub sync: SyncMode,
    pub segment_max_bytes: u64,
    pub max_record_bytes: usize,
    pub snapshot_retain: usize,
    pub recovery: RecoveryOptions,
    pub limits: EngineLimits,
    pub default_policies: DefaultPolicies,
    pub currencies: CurrencyRegistry,
}

impl LedgerOptions {
    /// Production-leaning defaults for the given data directory.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        LedgerOptions {
            data_dir: data_dir.into(),
            sync: SyncMode::Always,
            segment_max_bytes: 64 << 20,
            max_record_bytes: 4 << 20,
            snapshot_retain: 3,
            recovery: RecoveryOptions::default(),
            limits: EngineLimits::default(),
            default_policies: DefaultPolicies::default(),
            currencies: CurrencyRegistry::iso4217(),
        }
    }

    /// Fast settings for tests and benchmarks (no fsync).
    pub fn ephemeral(data_dir: impl Into<PathBuf>) -> Self {
        LedgerOptions {
            sync: SyncMode::Never,
            ..Self::new(data_dir)
        }
    }

    fn wal_options(&self, layout: &DataLayout) -> WalOptions {
        WalOptions {
            dir: layout.wal_dir(),
            segment_max_bytes: self.segment_max_bytes,
            max_record_bytes: self.max_record_bytes,
            sync: self.sync,
        }
    }
}

/// Engine-level idempotency slot for transaction creation.
enum IdemSlot {
    InFlight,
    Done {
        id: TransactionId,
        fingerprint: String,
    },
}

/// Result of creating a transaction.
#[derive(Debug, Clone)]
pub struct CreateOutcome {
    pub transaction: TransactionRecord,
    /// True when an earlier transaction with the same idempotency key and
    /// fingerprint was returned instead of creating a new one.
    pub replayed: bool,
}

/// Summary statistics for health and metrics endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct LedgerStats {
    pub last_sequence: u64,
    pub accounts: usize,
    pub transactions: usize,
    pub pending_transactions: usize,
    pub wal_bytes_appended: u64,
    pub last_snapshot_sequence: Option<u64>,
    pub poisoned: Option<String>,
}

/// Outcome of a compaction run.
#[derive(Debug, Clone, Serialize)]
pub struct CompactionReport {
    pub snapshot: SnapshotMeta,
    pub removed_segments: Vec<PathBuf>,
    pub removed_snapshots: Vec<PathBuf>,
}

/// View used to apply an event to locked live state.
struct CommitView<'a> {
    accounts: &'a mut LockedAccounts,
    transaction: Option<&'a mut TransactionRecord>,
    new_accounts: Vec<AccountState>,
    new_transactions: Vec<TransactionRecord>,
}

impl LedgerView for CommitView<'_> {
    fn account_mut(&mut self, id: &AccountId) -> Option<&mut AccountState> {
        self.accounts.get_mut(id)
    }

    fn insert_account(&mut self, state: AccountState) -> LedgerResult<()> {
        self.new_accounts.push(state);
        Ok(())
    }

    fn transaction_mut(&mut self, id: &TransactionId) -> Option<&mut TransactionRecord> {
        self.transaction.as_deref_mut().filter(|t| t.id == *id)
    }

    fn insert_transaction(&mut self, tx: TransactionRecord) -> LedgerResult<()> {
        self.new_transactions.push(tx);
        Ok(())
    }
}

struct CommitOutput {
    envelope: EventEnvelope,
    new_accounts: Vec<AccountState>,
    new_transactions: Vec<TransactionRecord>,
}

/// Removes an in-flight idempotency reservation unless completed.
struct IdemReservation<'a> {
    ledger: &'a Ledger,
    key: String,
    done: bool,
}

impl IdemReservation<'_> {
    fn complete(mut self, id: TransactionId, fingerprint: String) {
        self.ledger
            .idempotency
            .lock()
            .insert(self.key.clone(), IdemSlot::Done { id, fingerprint });
        self.done = true;
    }
}

impl Drop for IdemReservation<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.ledger.idempotency.lock().remove(&self.key);
        }
    }
}

/// The ledger engine. Cheap to share behind an `Arc`.
pub struct Ledger {
    pub(super) opts: LedgerOptions,
    pub(super) layout: DataLayout,
    _dir_lock: DirLock,
    pub(super) metrics: Arc<LedgerMetrics>,
    pub(super) gate: RwLock<()>,
    wal: Mutex<Wal>,
    pub(super) accounts: RwLock<BTreeMap<AccountId, AccountCell>>,
    pub(super) transactions: RwLock<HashMap<TransactionId, TxCell>>,
    pub(super) tx_order: RwLock<BTreeMap<u64, TransactionId>>,
    pub(super) pending: RwLock<BTreeMap<TransactionId, Option<DateTime<Utc>>>>,
    idempotency: Mutex<HashMap<String, IdemSlot>>,
    last_sequence: AtomicU64,
    last_snapshot_sequence: AtomicU64,
    poisoned: RwLock<Option<String>>,
    recovery_report: RecoveryReport,
}

impl Ledger {
    /// Opens (or creates) a ledger in `opts.data_dir`, running crash recovery.
    pub fn open(opts: LedgerOptions, metrics: Arc<LedgerMetrics>) -> LedgerResult<Self> {
        let layout = DataLayout::new(&opts.data_dir);
        layout.ensure()?;
        let dir_lock = DirLock::acquire(&layout, LockMode::Exclusive)?;
        let mut recovery_opts = opts.recovery.clone();
        recovery_opts.max_record_bytes = opts.max_record_bytes;
        recovery_opts.up_to_sequence = None;
        let recovered = recover::<LedgerState>(&layout, &recovery_opts)?;
        let report = recovered.report;
        let wal = Wal::open(
            opts.wal_options(&layout),
            recovered.state.last_sequence,
            recovered.state.last_timestamp,
            recovered.active_segment,
        )?;
        tracing::info!(
            last_sequence = report.last_sequence,
            snapshot = ?report.snapshot_sequence,
            replayed = report.events_replayed,
            duration_ms = report.duration_ms,
            "ledger recovered"
        );
        let ledger = Self::from_state(
            recovered.state,
            wal,
            opts,
            layout,
            dir_lock,
            metrics,
            report,
        );
        ledger.refresh_gauges();
        Ok(ledger)
    }

    fn from_state(
        state: LedgerState,
        wal: Wal,
        opts: LedgerOptions,
        layout: DataLayout,
        dir_lock: DirLock,
        metrics: Arc<LedgerMetrics>,
        recovery_report: RecoveryReport,
    ) -> Self {
        let idempotency = state
            .idempotency_index()
            .into_iter()
            .map(|(key, (id, fingerprint))| (key, IdemSlot::Done { id, fingerprint }))
            .collect();
        let mut tx_order = BTreeMap::new();
        let mut pending = BTreeMap::new();
        let mut transactions = HashMap::with_capacity(state.transactions.len());
        for (id, tx) in state.transactions {
            tx_order.insert(tx.created_sequence, id);
            if tx.status == TransactionStatus::Pending {
                pending.insert(id, tx.expires_at);
            }
            transactions.insert(id, Arc::new(Mutex::new(tx)));
        }
        let accounts = state
            .accounts
            .into_iter()
            .map(|(id, a)| (id, Arc::new(Mutex::new(a))))
            .collect();
        let last_snapshot = recovery_report.snapshot_sequence.unwrap_or(0);
        Ledger {
            opts,
            layout,
            _dir_lock: dir_lock,
            metrics,
            gate: RwLock::new(()),
            wal: Mutex::new(wal),
            accounts: RwLock::new(accounts),
            transactions: RwLock::new(transactions),
            tx_order: RwLock::new(tx_order),
            pending: RwLock::new(pending),
            idempotency: Mutex::new(idempotency),
            last_sequence: AtomicU64::new(state.last_sequence),
            last_snapshot_sequence: AtomicU64::new(last_snapshot),
            poisoned: RwLock::new(None),
            recovery_report,
        }
    }

    pub fn options(&self) -> &LedgerOptions {
        &self.opts
    }

    pub fn data_dir(&self) -> &Path {
        self.layout.root()
    }

    pub fn layout(&self) -> &DataLayout {
        &self.layout
    }

    pub fn currencies(&self) -> &CurrencyRegistry {
        &self.opts.currencies
    }

    pub fn metrics(&self) -> &Arc<LedgerMetrics> {
        &self.metrics
    }

    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.recovery_report
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence.load(Ordering::SeqCst)
    }

    /// Reason the ledger stopped accepting writes, if it did.
    pub fn poisoned(&self) -> Option<String> {
        self.poisoned.read().clone()
    }

    fn poison(&self, reason: String) {
        tracing::error!(%reason, "ledger switched to read-only mode");
        let mut p = self.poisoned.write();
        if p.is_none() {
            *p = Some(reason);
        }
    }

    fn ensure_writable(&self) -> LedgerResult<()> {
        match self.poisoned.read().as_ref() {
            Some(reason) => Err(LedgerError::ReadOnly(reason.clone())),
            None => Ok(()),
        }
    }

    /// Runs a command, recording its outcome in metrics.
    fn instrument<T>(&self, command: &str, f: impl FnOnce() -> LedgerResult<T>) -> LedgerResult<T> {
        let result = f();
        match &result {
            Ok(_) => self.metrics.record_command(command, Ok(())),
            Err(e) => {
                self.metrics.record_command(command, Err(e.code()));
                tracing::debug!(command, error = %e, "command rejected");
            }
        }
        result
    }

    /// Appends an event and applies it to the locked state.
    fn commit(
        &self,
        locked: &mut LockedAccounts,
        transaction: Option<&mut TransactionRecord>,
        build: impl FnOnce(u64, DateTime<Utc>) -> LedgerEvent,
    ) -> LedgerResult<CommitOutput> {
        self.metrics
            .lock_wait
            .observe(locked.waited().as_secs_f64());
        let started = Instant::now();
        let envelope = {
            let mut wal = self.wal.lock();
            match wal.append_with(build) {
                Ok(env) => env,
                Err(e) => {
                    if let Some(reason) = wal.is_poisoned() {
                        self.poison(reason.to_string());
                    }
                    return Err(e.into());
                }
            }
        };
        self.metrics
            .wal_append
            .observe(started.elapsed().as_secs_f64());
        self.metrics
            .events_appended
            .with(&[envelope.event.kind()])
            .inc();
        let mut view = CommitView {
            accounts: locked,
            transaction,
            new_accounts: Vec::new(),
            new_transactions: Vec::new(),
        };
        if let Err(e) = apply_event(&mut view, &envelope) {
            let reason = format!(
                "event {} was persisted but could not be applied: {e}",
                envelope.sequence
            );
            self.poison(reason.clone());
            return Err(LedgerError::ReadOnly(reason));
        }
        let CommitView {
            new_accounts,
            new_transactions,
            ..
        } = view;
        self.last_sequence
            .store(envelope.sequence, Ordering::SeqCst);
        self.metrics.last_sequence.set(envelope.sequence as f64);
        Ok(CommitOutput {
            envelope,
            new_accounts,
            new_transactions,
        })
    }

    /// Makes newly created transactions visible in the indexes.
    fn publish(&self, txs: Vec<TransactionRecord>) {
        for tx in txs {
            if tx.status == TransactionStatus::Pending {
                self.pending.write().insert(tx.id, tx.expires_at);
            }
            self.tx_order.write().insert(tx.created_sequence, tx.id);
            self.transactions
                .write()
                .insert(tx.id, Arc::new(Mutex::new(tx)));
        }
        self.refresh_gauges();
    }

    fn refresh_gauges(&self) {
        self.metrics.accounts.set(self.accounts.read().len() as f64);
        self.metrics
            .transactions
            .set(self.transactions.read().len() as f64);
        self.metrics
            .pending_transactions
            .set(self.pending.read().len() as f64);
        self.metrics.last_sequence.set(self.last_sequence() as f64);
    }

    pub(super) fn tx_cell(&self, id: &TransactionId) -> LedgerResult<TxCell> {
        self.transactions
            .read()
            .get(id)
            .cloned()
            .ok_or(LedgerError::TransactionNotFound(*id))
    }

    pub(super) fn account_cell(&self, id: &AccountId) -> LedgerResult<AccountCell> {
        self.accounts
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| LedgerError::AccountNotFound(id.clone()))
    }

    /// Validates projected balances for the given legs against each
    /// account's policy.
    fn check_projection(
        &self,
        locked: &LockedAccounts,
        postings: &[Posting],
        effect: Effect,
    ) -> LedgerResult<()> {
        let mut projected: BTreeMap<&AccountId, Balances> = BTreeMap::new();
        for p in postings {
            let account = locked.require(&p.account_id)?;
            projected
                .entry(&p.account_id)
                .or_insert(account.balances)
                .apply(effect, p.direction, p.amount)?;
        }
        if matches!(effect, Effect::Post | Effect::Reserve) {
            for (id, balances) in &projected {
                locked.require(id)?.check_policy(balances)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Account commands
    // ------------------------------------------------------------------

    /// Opens a new account.
    pub fn open_account(&self, cmd: OpenAccount) -> LedgerResult<Account> {
        self.instrument("open_account", || {
            self.ensure_writable()?;
            let account_type = cmd.validate()?;
            let currency = self.opts.currencies.resolve(&cmd.currency)?;
            let policy = cmd
                .policy
                .unwrap_or_else(|| self.opts.default_policies.for_type(account_type));
            let id = cmd.id.clone().unwrap_or_else(AccountId::generate);
            let _gate = self.gate.read();
            let mut index = self.accounts.write();
            if index.contains_key(&id) {
                return Err(LedgerError::AccountExists(id));
            }
            let mut locked = LockedAccounts::empty();
            let out = self.commit(&mut locked, None, |sequence, ts| {
                LedgerEvent::AccountOpened {
                    account: Account {
                        id: id.clone(),
                        name: cmd.name.trim().to_string(),
                        account_type,
                        currency: currency.code,
                        status: crate::domain::account::AccountStatus::Open,
                        policy,
                        metadata: cmd.metadata.clone(),
                        version: 1,
                        created_at: ts,
                        created_sequence: sequence,
                        updated_at: ts,
                    },
                }
            })?;
            let state = out
                .new_accounts
                .into_iter()
                .next()
                .ok_or_else(|| LedgerError::validation("account was not created"))?;
            let account = state.account.clone();
            index.insert(id, Arc::new(Mutex::new(state)));
            self.metrics.accounts.set(index.len() as f64);
            Ok(account)
        })
    }

    /// Freezes, unfreezes or closes an account.
    pub fn change_status(&self, cmd: ChangeStatus) -> LedgerResult<Account> {
        self.instrument("change_status", || {
            self.ensure_writable()?;
            validate_reason(&cmd.reason)?;
            let _gate = self.gate.read();
            let mut locked = LockedAccounts::acquire(&self.accounts, [cmd.account_id.clone()])?;
            let from = {
                let account = locked.require(&cmd.account_id)?;
                account.ensure_version(cmd.expected_version)?;
                account.check_status_transition(cmd.status)?;
                account.account.status
            };
            self.commit(&mut locked, None, |_, _| {
                LedgerEvent::AccountStatusChanged {
                    account_id: cmd.account_id.clone(),
                    from,
                    to: cmd.status,
                    reason: cmd.reason.clone(),
                }
            })?;
            Ok(locked.require(&cmd.account_id)?.account.clone())
        })
    }

    /// Merges metadata changes into an account.
    pub fn update_metadata(&self, cmd: UpdateMetadata) -> LedgerResult<Account> {
        self.instrument("update_metadata", || {
            self.ensure_writable()?;
            let _gate = self.gate.read();
            let mut locked = LockedAccounts::acquire(&self.accounts, [cmd.account_id.clone()])?;
            let metadata = {
                let account = locked.require(&cmd.account_id)?;
                account.ensure_version(cmd.expected_version)?;
                reject_closed(account)?;
                let mut metadata = account.account.metadata.clone();
                for key in &cmd.remove {
                    metadata.remove(key);
                }
                metadata.extend(cmd.set.clone());
                validate_metadata(&metadata)?;
                metadata
            };
            self.commit(&mut locked, None, |_, _| {
                LedgerEvent::AccountMetadataUpdated {
                    account_id: cmd.account_id.clone(),
                    metadata,
                }
            })?;
            Ok(locked.require(&cmd.account_id)?.account.clone())
        })
    }

    /// Replaces an account's balance policy.
    pub fn change_policy(&self, cmd: ChangePolicy) -> LedgerResult<Account> {
        self.instrument("change_policy", || {
            self.ensure_writable()?;
            cmd.policy.validate()?;
            let _gate = self.gate.read();
            let mut locked = LockedAccounts::acquire(&self.accounts, [cmd.account_id.clone()])?;
            {
                let account = locked.require(&cmd.account_id)?;
                account.ensure_version(cmd.expected_version)?;
                reject_closed(account)?;
            }
            self.commit(&mut locked, None, |_, _| {
                LedgerEvent::AccountPolicyChanged {
                    account_id: cmd.account_id.clone(),
                    policy: cmd.policy,
                }
            })?;
            Ok(locked.require(&cmd.account_id)?.account.clone())
        })
    }

    // ------------------------------------------------------------------
    // Transaction commands
    // ------------------------------------------------------------------

    fn reserve_idempotency(
        &self,
        tag: &IdempotencyTag,
    ) -> LedgerResult<Result<IdemReservation<'_>, TransactionId>> {
        let mut slots = self.idempotency.lock();
        match slots.get(&tag.key) {
            Some(IdemSlot::Done { id, fingerprint }) => {
                if *fingerprint == tag.fingerprint {
                    Ok(Err(*id))
                } else {
                    Err(LedgerError::IdempotencyMismatch)
                }
            }
            Some(IdemSlot::InFlight) => Err(LedgerError::RequestInProgress),
            None => {
                slots.insert(tag.key.clone(), IdemSlot::InFlight);
                Ok(Ok(IdemReservation {
                    ledger: self,
                    key: tag.key.clone(),
                    done: false,
                }))
            }
        }
    }

    /// Creates a balanced transaction, posted immediately or held pending.
    pub fn create_transaction(&self, cmd: CreateTransaction) -> LedgerResult<CreateOutcome> {
        self.instrument("create_transaction", || {
            self.ensure_writable()?;
            cmd.validate_shape(&self.opts.limits)?;
            let reservation = match &cmd.idempotency {
                Some(tag) => match self.reserve_idempotency(tag)? {
                    Ok(reservation) => Some(reservation),
                    Err(existing) => {
                        return Ok(CreateOutcome {
                            transaction: self.transaction(&existing)?,
                            replayed: true,
                        });
                    }
                },
                None => None,
            };

            let _gate = self.gate.read();
            let mut locked = LockedAccounts::acquire(
                &self.accounts,
                cmd.postings.iter().map(|p| p.account_id.clone()),
            )?;
            let mut postings = Vec::with_capacity(cmd.postings.len());
            for spec in &cmd.postings {
                let account = locked.require(&spec.account_id)?;
                account.ensure_open()?;
                if let Some(currency) = spec.currency
                    && currency != account.account.currency
                {
                    return Err(LedgerError::CurrencyMismatch {
                        account: spec.account_id.clone(),
                        expected: account.account.currency.to_string(),
                        actual: currency.to_string(),
                    });
                }
                postings.push(Posting {
                    account_id: spec.account_id.clone(),
                    direction: spec.direction,
                    amount: spec.amount,
                    currency: account.account.currency,
                });
            }
            validate_postings(
                &postings,
                self.opts.limits.max_postings,
                self.opts.limits.max_amount,
            )?;
            for (id, version) in &cmd.expected_versions {
                locked.require(id)?.ensure_version(Some(*version))?;
            }
            let (status, effect, ttl) = match cmd.mode {
                PostingMode::Posted => (TransactionStatus::Posted, Effect::Post, None),
                PostingMode::Pending { ttl } => (TransactionStatus::Pending, Effect::Reserve, ttl),
            };
            self.check_projection(&locked, &postings, effect)?;

            let id = TransactionId::generate();
            let legs = postings.len();
            let out = self.commit(&mut locked, None, |sequence, ts| {
                let posted = status == TransactionStatus::Posted;
                LedgerEvent::TransactionRecorded {
                    transaction: TransactionRecord {
                        id,
                        kind: TransactionKind::Standard,
                        status,
                        postings,
                        description: cmd.description.clone(),
                        reference: cmd.reference.clone(),
                        metadata: cmd.metadata.clone(),
                        created_at: ts,
                        created_sequence: sequence,
                        posted_at: posted.then_some(ts),
                        posted_sequence: posted.then_some(sequence),
                        voided_at: None,
                        void_reason: None,
                        expires_at: ttl
                            .and_then(|d| chrono::Duration::from_std(d).ok())
                            .map(|d| ts + d),
                        reversed_amounts: vec![0; legs],
                        reversals: Vec::new(),
                        idempotency: cmd.idempotency.clone(),
                    },
                }
            })?;
            drop(locked);
            let transaction = out
                .new_transactions
                .first()
                .cloned()
                .ok_or_else(|| LedgerError::validation("transaction was not recorded"))?;
            self.publish(out.new_transactions);
            if let (Some(reservation), Some(tag)) = (reservation, &cmd.idempotency) {
                reservation.complete(transaction.id, tag.fingerprint.clone());
            }
            tracing::debug!(
                transaction = %transaction.id,
                sequence = out.envelope.sequence,
                status = %transaction.status,
                "transaction recorded"
            );
            Ok(CreateOutcome {
                transaction,
                replayed: false,
            })
        })
    }

    /// Second phase: posts a pending transaction.
    pub fn post_pending(&self, id: TransactionId) -> LedgerResult<TransactionRecord> {
        self.instrument("post_pending", || {
            self.ensure_writable()?;
            let _gate = self.gate.read();
            let cell = self.tx_cell(&id)?;
            let mut tx = cell.lock_arc();
            ensure_pending(&tx)?;
            if tx.is_expired(Utc::now()) {
                return Err(LedgerError::Expired(id));
            }
            let mut locked = LockedAccounts::acquire(&self.accounts, tx.affected_accounts())?;
            for account_id in tx.affected_accounts() {
                locked.require(&account_id)?.ensure_open()?;
            }
            self.check_projection(&locked, &tx.postings, Effect::Commit)?;
            self.commit(&mut locked, Some(&mut tx), |_, _| {
                LedgerEvent::PendingPosted { transaction_id: id }
            })?;
            self.pending.write().remove(&id);
            self.refresh_gauges();
            Ok(tx.clone())
        })
    }

    /// Second phase alternative: voids a pending transaction.
    pub fn void_pending(
        &self,
        id: TransactionId,
        reason: Option<String>,
    ) -> LedgerResult<TransactionRecord> {
        self.instrument("void_pending", || self.void_inner(id, reason, false))
    }

    fn void_inner(
        &self,
        id: TransactionId,
        reason: Option<String>,
        expired: bool,
    ) -> LedgerResult<TransactionRecord> {
        self.ensure_writable()?;
        validate_reason(&reason)?;
        let _gate = self.gate.read();
        let cell = self.tx_cell(&id)?;
        let mut tx = cell.lock_arc();
        ensure_pending(&tx)?;
        let mut locked = LockedAccounts::acquire(&self.accounts, tx.affected_accounts())?;
        self.check_projection(&locked, &tx.postings, Effect::Release)?;
        self.commit(&mut locked, Some(&mut tx), |_, _| {
            LedgerEvent::PendingVoided {
                transaction_id: id,
                reason,
                expired,
            }
        })?;
        self.pending.write().remove(&id);
        self.refresh_gauges();
        Ok(tx.clone())
    }

    /// Voids every pending transaction whose expiry is at or before `now`.
    /// Returns the number of transactions voided.
    pub fn expire_pending(&self, now: DateTime<Utc>) -> usize {
        let due: Vec<TransactionId> = self
            .pending
            .read()
            .iter()
            .filter(|(_, expires)| expires.is_some_and(|e| e <= now))
            .map(|(id, _)| *id)
            .collect();
        let mut voided = 0;
        for id in due {
            match self.void_inner(id, Some("expired".into()), true) {
                Ok(_) => {
                    voided += 1;
                    self.metrics.pending_expired.inc();
                }
                // Raced with an explicit post/void: nothing to do.
                Err(LedgerError::InvalidTransactionState { .. }) => {}
                Err(e) => {
                    tracing::warn!(transaction = %id, error = %e, "failed to expire pending transaction")
                }
            }
        }
        voided
    }

    /// Reverses a posted transaction, fully or partially.
    pub fn reverse(&self, cmd: ReverseTransaction) -> LedgerResult<TransactionRecord> {
        self.instrument("reverse", || {
            self.ensure_writable()?;
            cmd.validate_shape()?;
            let _gate = self.gate.read();
            let cell = self.tx_cell(&cmd.original)?;
            let mut original = cell.lock_arc();
            let legs = plan_reversal(&original, cmd.legs.as_deref())?;
            let postings: Vec<Posting> = legs
                .iter()
                .map(|leg| {
                    let p = &original.postings[leg.posting_index];
                    Posting {
                        account_id: p.account_id.clone(),
                        direction: p.direction.opposite(),
                        amount: leg.amount,
                        currency: p.currency,
                    }
                })
                .collect();
            check_balanced(&postings).map_err(|e| LedgerError::InvalidReversal {
                id: cmd.original,
                reason: format!("reversal legs must balance per currency ({e})"),
            })?;
            let mut locked = LockedAccounts::acquire(
                &self.accounts,
                postings.iter().map(|p| p.account_id.clone()),
            )?;
            for p in &postings {
                locked.require(&p.account_id)?.ensure_open()?;
            }
            self.check_projection(&locked, &postings, Effect::Post)?;

            let reversal_id = TransactionId::generate();
            let original_id = cmd.original;
            let description = cmd
                .description
                .clone()
                .unwrap_or_else(|| format!("Reversal of {original_id}"));
            let reference = cmd.reference.clone().or_else(|| original.reference.clone());
            let legs_count = postings.len();
            let out = self.commit(&mut locked, Some(&mut original), |sequence, ts| {
                LedgerEvent::TransactionReversed {
                    original_id,
                    reversal: TransactionRecord {
                        id: reversal_id,
                        kind: TransactionKind::Reversal {
                            original: original_id,
                        },
                        status: TransactionStatus::Posted,
                        postings,
                        description,
                        reference,
                        metadata: cmd.metadata.clone(),
                        created_at: ts,
                        created_sequence: sequence,
                        posted_at: Some(ts),
                        posted_sequence: Some(sequence),
                        voided_at: None,
                        void_reason: None,
                        expires_at: None,
                        reversed_amounts: vec![0; legs_count],
                        reversals: Vec::new(),
                        idempotency: None,
                    },
                    legs,
                }
            })?;
            drop(locked);
            drop(original);
            let reversal = out
                .new_transactions
                .first()
                .cloned()
                .ok_or_else(|| LedgerError::validation("reversal was not recorded"))?;
            self.publish(out.new_transactions);
            Ok(reversal)
        })
    }

    // ------------------------------------------------------------------
    // Snapshots and maintenance
    // ------------------------------------------------------------------

    /// Captures a consistent copy of the full state. Briefly blocks writers.
    pub fn capture_state(&self) -> LedgerState {
        self.capture(true)
    }

    /// Like [`capture_state`](Self::capture_state) but keeps only the newest
    /// history entry per account; enough for reports and much cheaper.
    pub fn capture_summary(&self) -> LedgerState {
        self.capture(false)
    }

    fn capture(&self, full_history: bool) -> LedgerState {
        let _gate = self.gate.write();
        let (last_sequence, last_timestamp) = {
            let wal = self.wal.lock();
            (wal.last_sequence(), wal.last_timestamp())
        };
        let accounts = self
            .accounts
            .read()
            .iter()
            .map(|(id, cell)| {
                let state = cell.lock();
                let copy = if full_history {
                    state.clone()
                } else {
                    AccountState {
                        account: state.account.clone(),
                        balances: state.balances,
                        history: state.history.last().cloned().into_iter().collect(),
                    }
                };
                (id.clone(), copy)
            })
            .collect();
        let transactions = self
            .transactions
            .read()
            .iter()
            .map(|(id, cell)| (*id, cell.lock().clone()))
            .collect();
        LedgerState {
            last_sequence,
            last_timestamp,
            accounts,
            transactions,
        }
    }

    /// Writes a snapshot of the current state and prunes old snapshots.
    pub fn snapshot(&self) -> LedgerResult<SnapshotMeta> {
        let started = Instant::now();
        let result = (|| {
            let state = self.capture_state();
            let meta =
                snapshot::write_snapshot(&self.layout.snapshot_dir(), state.last_sequence, &state)?;
            snapshot::prune_snapshots(&self.layout.snapshot_dir(), self.opts.snapshot_retain)?;
            Ok::<_, StorageError>(meta)
        })();
        match &result {
            Ok(meta) => {
                self.last_snapshot_sequence
                    .store(meta.sequence, Ordering::SeqCst);
                self.metrics.snapshots.with(&["ok"]).inc();
                tracing::info!(
                    sequence = meta.sequence,
                    bytes = meta.size,
                    "snapshot written"
                );
            }
            Err(e) => {
                self.metrics.snapshots.with(&["error"]).inc();
                tracing::error!(error = %e, "snapshot failed");
            }
        }
        self.metrics
            .snapshot_duration
            .observe(started.elapsed().as_secs_f64());
        result.map_err(LedgerError::from)
    }

    /// Takes a snapshot and removes WAL segments fully covered by it.
    pub fn compact(&self) -> LedgerResult<CompactionReport> {
        let snapshot = self.snapshot()?;
        let removed_segments = {
            let mut wal = self.wal.lock();
            if wal.last_sequence() == snapshot.sequence {
                wal.seal_active_segment()?;
            }
            wal.remove_segments_through(snapshot.sequence)?
        };
        let removed_snapshots =
            snapshot::prune_snapshots(&self.layout.snapshot_dir(), self.opts.snapshot_retain)?;
        tracing::info!(
            sequence = snapshot.sequence,
            segments = removed_segments.len(),
            "compaction finished"
        );
        Ok(CompactionReport {
            snapshot,
            removed_segments,
            removed_snapshots,
        })
    }

    /// Number of events appended since the last snapshot.
    pub fn events_since_snapshot(&self) -> u64 {
        self.last_sequence()
            .saturating_sub(self.last_snapshot_sequence.load(Ordering::SeqCst))
    }

    /// Flushes the WAL to disk (used on shutdown).
    pub fn sync(&self) -> LedgerResult<()> {
        self.wal.lock().sync().map_err(LedgerError::from)
    }

    pub fn stats(&self) -> LedgerStats {
        let last_snapshot = self.last_snapshot_sequence.load(Ordering::SeqCst);
        LedgerStats {
            last_sequence: self.last_sequence(),
            accounts: self.accounts.read().len(),
            transactions: self.transactions.read().len(),
            pending_transactions: self.pending.read().len(),
            wal_bytes_appended: self.wal.lock().appended_bytes(),
            last_snapshot_sequence: (last_snapshot > 0).then_some(last_snapshot),
            poisoned: self.poisoned(),
        }
    }
}

fn reject_closed(account: &AccountState) -> LedgerResult<()> {
    if account.account.status == crate::domain::account::AccountStatus::Closed {
        return Err(LedgerError::AccountNotActive {
            account: account.account.id.clone(),
            status: account.account.status,
        });
    }
    Ok(())
}

fn ensure_pending(tx: &TransactionRecord) -> LedgerResult<()> {
    if tx.status != TransactionStatus::Pending {
        return Err(LedgerError::InvalidTransactionState {
            id: tx.id,
            status: tx.status,
            expected: TransactionStatus::Pending,
        });
    }
    Ok(())
}

/// Determines which legs a reversal covers.
fn plan_reversal(
    original: &TransactionRecord,
    requested: Option<&[ReversedLeg]>,
) -> LedgerResult<Vec<ReversedLeg>> {
    let invalid = |reason: String| LedgerError::InvalidReversal {
        id: original.id,
        reason,
    };
    if original.is_reversal() {
        return Err(invalid(
            "a reversal transaction cannot itself be reversed".into(),
        ));
    }
    if original.status != TransactionStatus::Posted {
        return Err(LedgerError::InvalidTransactionState {
            id: original.id,
            status: original.status,
            expected: TransactionStatus::Posted,
        });
    }
    if original.is_fully_reversed() {
        return Err(LedgerError::AlreadyReversed(original.id));
    }
    match requested {
        None => Ok((0..original.postings.len())
            .filter_map(|i| {
                let remaining = original.remaining_reversible(i);
                (remaining > 0).then_some(ReversedLeg {
                    posting_index: i,
                    amount: remaining,
                })
            })
            .collect()),
        Some(legs) => {
            for leg in legs {
                if leg.posting_index >= original.postings.len() {
                    return Err(invalid(format!(
                        "posting index {} out of range (transaction has {} postings)",
                        leg.posting_index,
                        original.postings.len()
                    )));
                }
                let remaining = original.remaining_reversible(leg.posting_index);
                if leg.amount > remaining {
                    return Err(invalid(format!(
                        "posting {} has only {remaining} left to reverse, requested {}",
                        leg.posting_index, leg.amount
                    )));
                }
            }
            let mut legs = legs.to_vec();
            legs.sort_by_key(|l| l.posting_index);
            Ok(legs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::{AccountStatus, AccountType, BalancePolicy};
    use crate::engine::commands::PostingSpec;

    fn open() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(
            LedgerOptions::ephemeral(dir.path()),
            Arc::new(LedgerMetrics::new()),
        )
        .unwrap();
        (dir, ledger)
    }

    fn seed(ledger: &Ledger) {
        ledger
            .open_account(OpenAccount::new("cash", AccountType::Asset, "USD").unwrap())
            .unwrap();
        ledger
            .open_account(OpenAccount::new("equity", AccountType::Equity, "USD").unwrap())
            .unwrap();
        ledger
            .open_account(OpenAccount::new("wallet", AccountType::Liability, "USD").unwrap())
            .unwrap();
        ledger
            .create_transaction(CreateTransaction::posted(vec![
                PostingSpec::debit("cash", 10_000).unwrap(),
                PostingSpec::credit("equity", 10_000).unwrap(),
            ]))
            .unwrap();
    }

    fn balance(ledger: &Ledger, id: &str) -> i64 {
        ledger
            .account_cell(&AccountId::new(id).unwrap())
            .unwrap()
            .lock()
            .posted_balance()
    }

    #[test]
    fn data_dir_is_locked_while_open() {
        let (dir, _ledger) = open();
        let second = Ledger::open(
            LedgerOptions::ephemeral(dir.path()),
            Arc::new(LedgerMetrics::new()),
        );
        assert!(matches!(
            second,
            Err(LedgerError::Storage(StorageError::Locked(_)))
        ));
    }

    #[test]
    fn duplicate_and_unknown_currency_accounts_rejected() {
        let (_dir, ledger) = open();
        seed(&ledger);
        let err = ledger
            .open_account(OpenAccount::new("cash", AccountType::Asset, "USD").unwrap())
            .unwrap_err();
        assert_eq!(err.code(), "account_exists");
        let err = ledger
            .open_account(OpenAccount::new("x", AccountType::Asset, "ZZZ").unwrap())
            .unwrap_err();
        assert_eq!(err.code(), "validation_error");
    }

    #[test]
    fn default_policy_blocks_overdraft() {
        let (_dir, ledger) = open();
        seed(&ledger);
        let err = ledger
            .create_transaction(CreateTransaction::posted(vec![
                PostingSpec::debit("wallet", 1).unwrap(),
                PostingSpec::credit("cash", 1).unwrap(),
            ]))
            .unwrap_err();
        assert_eq!(err.code(), "insufficient_funds");
        assert_eq!(balance(&ledger, "cash"), 10_000);
    }

    #[test]
    fn pending_reserves_available_balance() {
        let (_dir, ledger) = open();
        seed(&ledger);
        // The wallet is a liability with NoOverdraft: debiting it fails even
        // as a pending reservation.
        let err = ledger
            .create_transaction(CreateTransaction::pending(
                vec![
                    PostingSpec::debit("wallet", 6_000).unwrap(),
                    PostingSpec::credit("cash", 6_000).unwrap(),
                ],
                None,
            ))
            .unwrap_err();
        assert_eq!(err.code(), "insufficient_funds");

        // Reserving 6000 of the 10000 cash leaves only 4000 available.
        let pending = ledger
            .create_transaction(CreateTransaction::pending(
                vec![
                    PostingSpec::debit("equity", 6_000).unwrap(),
                    PostingSpec::credit("cash", 6_000).unwrap(),
                ],
                None,
            ))
            .unwrap()
            .transaction;
        let err = ledger
            .create_transaction(CreateTransaction::posted(vec![
                PostingSpec::debit("equity", 6_000).unwrap(),
                PostingSpec::credit("cash", 6_000).unwrap(),
            ]))
            .unwrap_err();
        assert_eq!(err.code(), "insufficient_funds");
        ledger.post_pending(pending.id).unwrap();
        assert_eq!(balance(&ledger, "cash"), 4_000);
        assert_eq!(
            ledger.post_pending(pending.id).unwrap_err().code(),
            "invalid_transaction_state"
        );
    }

    #[test]
    fn expiry_voids_pending() {
        let (_dir, ledger) = open();
        seed(&ledger);
        let tx = ledger
            .create_transaction(CreateTransaction::pending(
                vec![
                    PostingSpec::debit("equity", 100).unwrap(),
                    PostingSpec::credit("cash", 100).unwrap(),
                ],
                Some(std::time::Duration::from_secs(60)),
            ))
            .unwrap()
            .transaction;
        assert_eq!(ledger.expire_pending(Utc::now()), 0);
        let later = Utc::now() + chrono::Duration::seconds(120);
        assert_eq!(ledger.expire_pending(later), 1);
        let stored = ledger.transaction(&tx.id).unwrap();
        assert_eq!(stored.status, TransactionStatus::Voided);
        assert_eq!(stored.void_reason.as_deref(), Some("expired"));
        assert_eq!(ledger.stats().pending_transactions, 0);
    }

    #[test]
    fn idempotent_creation_returns_original() {
        let (_dir, ledger) = open();
        seed(&ledger);
        let mut cmd = CreateTransaction::posted(vec![
            PostingSpec::debit("equity", 5).unwrap(),
            PostingSpec::credit("cash", 5).unwrap(),
        ]);
        cmd.idempotency = Some(IdempotencyTag {
            key: "client:abc".into(),
            fingerprint: "fp1".into(),
        });
        let first = ledger.create_transaction(cmd.clone()).unwrap();
        let second = ledger.create_transaction(cmd.clone()).unwrap();
        assert!(!first.replayed);
        assert!(second.replayed);
        assert_eq!(first.transaction.id, second.transaction.id);
        assert_eq!(balance(&ledger, "cash"), 9_995);
        cmd.idempotency.as_mut().unwrap().fingerprint = "fp2".into();
        assert_eq!(
            ledger.create_transaction(cmd).unwrap_err().code(),
            "idempotency_key_reused"
        );
    }

    #[test]
    fn failed_creation_releases_idempotency_key() {
        let (_dir, ledger) = open();
        seed(&ledger);
        let mut cmd = CreateTransaction::posted(vec![
            PostingSpec::debit("equity", 50_000).unwrap(),
            PostingSpec::credit("cash", 50_000).unwrap(),
        ]);
        cmd.idempotency = Some(IdempotencyTag {
            key: "k".into(),
            fingerprint: "f".into(),
        });
        assert!(ledger.create_transaction(cmd.clone()).is_err());
        // Key is free again: still fails on funds, not on "in progress".
        assert_eq!(
            ledger.create_transaction(cmd).unwrap_err().code(),
            "insufficient_funds"
        );
    }

    #[test]
    fn frozen_accounts_block_postings_but_allow_voids() {
        let (_dir, ledger) = open();
        seed(&ledger);
        let pending = ledger
            .create_transaction(CreateTransaction::pending(
                vec![
                    PostingSpec::debit("equity", 10).unwrap(),
                    PostingSpec::credit("cash", 10).unwrap(),
                ],
                None,
            ))
            .unwrap()
            .transaction;
        ledger
            .change_status(ChangeStatus {
                account_id: AccountId::new("cash").unwrap(),
                status: AccountStatus::Frozen,
                expected_version: None,
                reason: Some("investigation".into()),
            })
            .unwrap();
        assert_eq!(
            ledger.post_pending(pending.id).unwrap_err().code(),
            "account_not_active"
        );
        ledger.void_pending(pending.id, None).unwrap();
    }

    #[test]
    fn reversal_rules() {
        let (_dir, ledger) = open();
        seed(&ledger);
        let tx = ledger
            .create_transaction(CreateTransaction::posted(vec![
                PostingSpec::debit("equity", 300).unwrap(),
                PostingSpec::credit("cash", 300).unwrap(),
            ]))
            .unwrap()
            .transaction;
        let partial = ledger
            .reverse(ReverseTransaction::partial(
                tx.id,
                vec![
                    ReversedLeg {
                        posting_index: 0,
                        amount: 100,
                    },
                    ReversedLeg {
                        posting_index: 1,
                        amount: 100,
                    },
                ],
            ))
            .unwrap();
        assert!(partial.is_reversal());
        // Unbalanced partial reversal is rejected.
        let err = ledger
            .reverse(ReverseTransaction::partial(
                tx.id,
                vec![ReversedLeg {
                    posting_index: 0,
                    amount: 50,
                }],
            ))
            .unwrap_err();
        assert_eq!(err.code(), "invalid_reversal");
        // Full reversal takes the remainder; a second one is refused.
        let rest = ledger.reverse(ReverseTransaction::full(tx.id)).unwrap();
        assert_eq!(rest.postings[0].amount, 200);
        assert_eq!(
            ledger
                .reverse(ReverseTransaction::full(tx.id))
                .unwrap_err()
                .code(),
            "already_reversed"
        );
        assert_eq!(
            ledger
                .reverse(ReverseTransaction::full(rest.id))
                .unwrap_err()
                .code(),
            "invalid_reversal"
        );
        assert_eq!(balance(&ledger, "cash"), 10_000);
    }

    #[test]
    fn policy_and_metadata_updates_bump_versions() {
        let (_dir, ledger) = open();
        seed(&ledger);
        let id = AccountId::new("wallet").unwrap();
        let mut set = BTreeMap::new();
        set.insert("tier".to_string(), "gold".to_string());
        let a = ledger
            .update_metadata(UpdateMetadata {
                account_id: id.clone(),
                set,
                remove: vec![],
                expected_version: Some(1),
            })
            .unwrap();
        assert_eq!(a.version, 2);
        let err = ledger
            .change_policy(ChangePolicy {
                account_id: id.clone(),
                policy: BalancePolicy::Overdraft { limit: 500 },
                expected_version: Some(1),
            })
            .unwrap_err();
        assert_eq!(err.code(), "version_conflict");
        let a = ledger
            .change_policy(ChangePolicy {
                account_id: id,
                policy: BalancePolicy::Overdraft { limit: 500 },
                expected_version: Some(2),
            })
            .unwrap();
        assert_eq!(a.policy, BalancePolicy::Overdraft { limit: 500 });
    }

    #[test]
    fn snapshot_compact_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = LedgerOptions::ephemeral(dir.path());
        opts.segment_max_bytes = 2_000;
        {
            let ledger = Ledger::open(opts.clone(), Arc::new(LedgerMetrics::new())).unwrap();
            seed(&ledger);
            for _ in 0..20 {
                ledger
                    .create_transaction(CreateTransaction::posted(vec![
                        PostingSpec::debit("equity", 1).unwrap(),
                        PostingSpec::credit("cash", 1).unwrap(),
                    ]))
                    .unwrap();
            }
            let report = ledger.compact().unwrap();
            assert!(!report.removed_segments.is_empty());
            assert_eq!(ledger.events_since_snapshot(), 0);
            ledger
                .create_transaction(CreateTransaction::posted(vec![
                    PostingSpec::debit("equity", 1).unwrap(),
                    PostingSpec::credit("cash", 1).unwrap(),
                ]))
                .unwrap();
        }
        let ledger = Ledger::open(opts, Arc::new(LedgerMetrics::new())).unwrap();
        let report = ledger.recovery_report();
        assert!(report.snapshot_sequence.is_some());
        assert_eq!(report.events_replayed, 1);
        assert_eq!(balance(&ledger, "cash"), 10_000 - 21);
    }
}
