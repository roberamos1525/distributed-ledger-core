//! Chart of accounts: account types, statuses, balance policies and the
//! per-account materialized state (balances + history).

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::ids::{AccountId, TransactionId};
use crate::domain::money::CurrencyCode;
use crate::domain::transaction::Direction;
use crate::error::{LedgerError, LedgerResult};

/// The five classic account classes of double-entry bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

impl AccountType {
    pub const ALL: [AccountType; 5] = [
        AccountType::Asset,
        AccountType::Liability,
        AccountType::Equity,
        AccountType::Revenue,
        AccountType::Expense,
    ];

    /// The side on which this account type increases.
    pub fn normal_side(self) -> Direction {
        match self {
            AccountType::Asset | AccountType::Expense => Direction::Debit,
            AccountType::Liability | AccountType::Equity | AccountType::Revenue => {
                Direction::Credit
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AccountType::Asset => "asset",
            AccountType::Liability => "liability",
            AccountType::Equity => "equity",
            AccountType::Revenue => "revenue",
            AccountType::Expense => "expense",
        }
    }

    pub fn parse(raw: &str) -> LedgerResult<Self> {
        AccountType::ALL
            .into_iter()
            .find(|t| t.as_str() == raw)
            .ok_or_else(|| LedgerError::validation(format!("unknown account type {raw:?}")))
    }
}

impl fmt::Display for AccountType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle status of an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// Accepts postings.
    Open,
    /// Temporarily blocked: no new postings or pending commits; voids allowed.
    Frozen,
    /// Terminal: requires zero balance and no pending amounts.
    Closed,
}

impl AccountStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStatus::Open => "open",
            AccountStatus::Frozen => "frozen",
            AccountStatus::Closed => "closed",
        }
    }

    pub fn parse(raw: &str) -> LedgerResult<Self> {
        match raw {
            "open" => Ok(AccountStatus::Open),
            "frozen" => Ok(AccountStatus::Frozen),
            "closed" => Ok(AccountStatus::Closed),
            other => Err(LedgerError::validation(format!(
                "unknown account status {other:?}"
            ))),
        }
    }

    /// Whether a transition from `self` to `to` is permitted.
    pub fn can_transition_to(self, to: AccountStatus) -> bool {
        matches!(
            (self, to),
            (AccountStatus::Open, AccountStatus::Frozen)
                | (AccountStatus::Frozen, AccountStatus::Open)
                | (AccountStatus::Open, AccountStatus::Closed)
                | (AccountStatus::Frozen, AccountStatus::Closed)
        )
    }
}

impl fmt::Display for AccountStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Negative-balance (overdraft) policy, evaluated against the account's
/// *available* balance on its normal side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BalancePolicy {
    /// The available balance may never drop below zero.
    NoOverdraft,
    /// The available balance may drop to `-limit` minor units.
    Overdraft { limit: i64 },
    /// No lower bound (typical for equity, revenue and expense accounts).
    Unrestricted,
}

impl BalancePolicy {
    /// Lowest permitted available balance, or `None` when unrestricted.
    pub fn floor(self) -> Option<i64> {
        match self {
            BalancePolicy::NoOverdraft => Some(0),
            BalancePolicy::Overdraft { limit } => Some(-limit),
            BalancePolicy::Unrestricted => None,
        }
    }

    pub fn validate(self) -> LedgerResult<()> {
        if let BalancePolicy::Overdraft { limit } = self
            && limit < 0
        {
            return Err(LedgerError::validation("overdraft limit must be >= 0"));
        }
        Ok(())
    }

    pub fn describe(self) -> String {
        match self.floor() {
            Some(f) => f.to_string(),
            None => "unbounded".to_string(),
        }
    }
}

/// Static and slowly changing account attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub account_type: AccountType,
    pub currency: CurrencyCode,
    pub status: AccountStatus,
    pub policy: BalancePolicy,
    pub metadata: BTreeMap<String, String>,
    /// Incremented by every event touching the account (optimistic concurrency).
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub created_sequence: u64,
    pub updated_at: DateTime<Utc>,
}

/// Raw balance counters. `posted` is signed debit-positive: debits add,
/// credits subtract. Pending amounts are tracked per side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balances {
    pub posted: i64,
    pub pending_debits: i64,
    pub pending_credits: i64,
}

/// How a posting leg affects balances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Directly posted.
    Post,
    /// Reserved as pending (first phase of a two-phase transfer).
    Reserve,
    /// Pending amount committed to posted.
    Commit,
    /// Pending amount released (voided).
    Release,
}

impl Balances {
    /// Posted balance expressed on the account's normal side.
    pub fn normal_posted(&self, account_type: AccountType) -> i64 {
        match account_type.normal_side() {
            Direction::Debit => self.posted,
            // Negating an i64 can only overflow at i64::MIN, which the checked
            // arithmetic in `apply` never lets us reach for credit-normal accounts.
            Direction::Credit => self.posted.saturating_neg(),
        }
    }

    /// Available balance: posted minus pending amounts that would reduce it.
    pub fn available(&self, account_type: AccountType) -> i64 {
        match account_type.normal_side() {
            Direction::Debit => self.posted.saturating_sub(self.pending_credits),
            Direction::Credit => self
                .posted
                .saturating_neg()
                .saturating_sub(self.pending_debits),
        }
    }

    /// Applies one leg with checked arithmetic.
    pub fn apply(&mut self, effect: Effect, direction: Direction, amount: i64) -> LedgerResult<()> {
        let signed = match direction {
            Direction::Debit => amount,
            Direction::Credit => amount.checked_neg().ok_or(LedgerError::Overflow("leg"))?,
        };
        let overflow = || LedgerError::Overflow("account balance");
        match effect {
            Effect::Post => {
                self.posted = self.posted.checked_add(signed).ok_or_else(overflow)?;
            }
            Effect::Reserve => {
                let pending = self.pending_mut(direction);
                *pending = pending.checked_add(amount).ok_or_else(overflow)?;
            }
            Effect::Commit => {
                self.release_pending(direction, amount)?;
                self.posted = self.posted.checked_add(signed).ok_or_else(overflow)?;
            }
            Effect::Release => self.release_pending(direction, amount)?,
        }
        if self.posted == i64::MIN {
            return Err(overflow());
        }
        Ok(())
    }

    fn pending_mut(&mut self, direction: Direction) -> &mut i64 {
        match direction {
            Direction::Debit => &mut self.pending_debits,
            Direction::Credit => &mut self.pending_credits,
        }
    }

    fn release_pending(&mut self, direction: Direction, amount: i64) -> LedgerResult<()> {
        let pending = self.pending_mut(direction);
        if *pending < amount {
            return Err(LedgerError::validation(format!(
                "pending {direction} balance {pending} is smaller than release amount {amount}"
            )));
        }
        *pending -= amount;
        Ok(())
    }

    pub fn has_pending(&self) -> bool {
        self.pending_debits != 0 || self.pending_credits != 0
    }
}

/// Kind of history entry recorded for an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Posted,
    PendingCreated,
    PendingPosted,
    PendingVoided,
    Reversal,
}

impl EntryKind {
    /// Whether the entry changed the posted balance.
    pub fn affects_posted(self) -> bool {
        matches!(
            self,
            EntryKind::Posted | EntryKind::PendingPosted | EntryKind::Reversal
        )
    }
}

/// One line of an account's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub transaction_id: TransactionId,
    pub kind: EntryKind,
    pub direction: Direction,
    pub amount: i64,
    /// Posted balance on the normal side after this entry.
    pub posted_balance: i64,
    /// Available balance on the normal side after this entry.
    pub available_balance: i64,
}

impl HistoryEntry {
    /// Signed amount from the account's normal-side perspective
    /// (positive = the account's balance increased).
    pub fn normal_amount(&self, account_type: AccountType) -> i64 {
        if self.direction == account_type.normal_side() {
            self.amount
        } else {
            -self.amount
        }
    }
}

/// Fully materialized account: attributes, balances and history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub account: Account,
    pub balances: Balances,
    pub history: Vec<HistoryEntry>,
}

/// Event coordinates shared by all legs recorded for one event.
#[derive(Debug, Clone, Copy)]
pub struct EntryContext {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub transaction_id: TransactionId,
}

/// Point-in-time balance computed from history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HistoricalBalance {
    pub posted_balance: i64,
    pub available_balance: i64,
    /// Sequence of the last entry at or before the requested point.
    pub sequence: Option<u64>,
}

impl AccountState {
    pub fn new(account: Account) -> Self {
        AccountState {
            account,
            balances: Balances::default(),
            history: Vec::new(),
        }
    }

    pub fn id(&self) -> &AccountId {
        &self.account.id
    }

    pub fn posted_balance(&self) -> i64 {
        self.balances.normal_posted(self.account.account_type)
    }

    pub fn available_balance(&self) -> i64 {
        self.balances.available(self.account.account_type)
    }

    /// Applies a leg and records a history entry.
    pub fn record(
        &mut self,
        ctx: &EntryContext,
        kind: EntryKind,
        effect: Effect,
        direction: Direction,
        amount: i64,
    ) -> LedgerResult<()> {
        self.balances.apply(effect, direction, amount)?;
        let entry = HistoryEntry {
            sequence: ctx.sequence,
            timestamp: ctx.timestamp,
            transaction_id: ctx.transaction_id,
            kind,
            direction,
            amount,
            posted_balance: self.posted_balance(),
            available_balance: self.available_balance(),
        };
        self.history.push(entry);
        Ok(())
    }

    /// Marks the account as modified by the event at `timestamp`.
    pub fn touch(&mut self, timestamp: DateTime<Utc>) {
        self.account.version += 1;
        self.account.updated_at = timestamp;
    }

    /// Balance as of the given event sequence (inclusive).
    pub fn balance_at_sequence(&self, sequence: u64) -> HistoricalBalance {
        let idx = self.history.partition_point(|e| e.sequence <= sequence);
        self.balance_before_index(idx)
    }

    /// Balance as of the given instant (inclusive).
    pub fn balance_at_time(&self, at: DateTime<Utc>) -> HistoricalBalance {
        let idx = self.history.partition_point(|e| e.timestamp <= at);
        self.balance_before_index(idx)
    }

    fn balance_before_index(&self, idx: usize) -> HistoricalBalance {
        match idx.checked_sub(1).and_then(|i| self.history.get(i)) {
            Some(e) => HistoricalBalance {
                posted_balance: e.posted_balance,
                available_balance: e.available_balance,
                sequence: Some(e.sequence),
            },
            None => HistoricalBalance {
                posted_balance: 0,
                available_balance: 0,
                sequence: None,
            },
        }
    }

    /// Ensures the account accepts new postings.
    pub fn ensure_open(&self) -> LedgerResult<()> {
        if self.account.status != AccountStatus::Open {
            return Err(LedgerError::AccountNotActive {
                account: self.account.id.clone(),
                status: self.account.status,
            });
        }
        Ok(())
    }

    /// Checks the optimistic concurrency expectation.
    pub fn ensure_version(&self, expected: Option<u64>) -> LedgerResult<()> {
        match expected {
            Some(expected) if expected != self.account.version => {
                Err(LedgerError::VersionConflict {
                    account: self.account.id.clone(),
                    expected,
                    actual: self.account.version,
                })
            }
            _ => Ok(()),
        }
    }

    /// Validates that the account may move to `to`.
    pub fn check_status_transition(&self, to: AccountStatus) -> LedgerResult<()> {
        let from = self.account.status;
        if !from.can_transition_to(to) {
            return Err(LedgerError::InvalidStatusTransition {
                account: self.account.id.clone(),
                from,
                to,
            });
        }
        if to == AccountStatus::Closed {
            if self.balances.posted != 0 {
                return Err(LedgerError::AccountNotEmpty {
                    account: self.account.id.clone(),
                    reason: format!("posted balance is {}", self.posted_balance()),
                });
            }
            if self.balances.has_pending() {
                return Err(LedgerError::AccountNotEmpty {
                    account: self.account.id.clone(),
                    reason: "account has pending amounts".into(),
                });
            }
        }
        Ok(())
    }

    /// Checks the balance policy for a projected set of balances.
    ///
    /// A change is rejected only if it leaves the available balance below the
    /// policy floor *and* lowers it; this lets corrective postings through for
    /// accounts that are already below a (since tightened) floor.
    pub fn check_policy(&self, projected: &Balances) -> LedgerResult<()> {
        let account_type = self.account.account_type;
        let before = self.balances.available(account_type);
        let after = projected.available(account_type);
        if let Some(floor) = self.account.policy.floor()
            && after < floor
            && after < before
        {
            return Err(LedgerError::InsufficientFunds {
                account: self.account.id.clone(),
                available: before,
                required: before.saturating_sub(after),
                floor: self.account.policy.describe(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(account_type: AccountType, policy: BalancePolicy) -> AccountState {
        let now = Utc::now();
        AccountState::new(Account {
            id: AccountId::new("acct").unwrap(),
            name: "Test".into(),
            account_type,
            currency: CurrencyCode::new("USD").unwrap(),
            status: AccountStatus::Open,
            policy,
            metadata: BTreeMap::new(),
            version: 1,
            created_at: now,
            created_sequence: 1,
            updated_at: now,
        })
    }

    #[test]
    fn normal_sides() {
        assert_eq!(AccountType::Asset.normal_side(), Direction::Debit);
        assert_eq!(AccountType::Expense.normal_side(), Direction::Debit);
        assert_eq!(AccountType::Liability.normal_side(), Direction::Credit);
        assert_eq!(AccountType::Equity.normal_side(), Direction::Credit);
        assert_eq!(AccountType::Revenue.normal_side(), Direction::Credit);
        assert_eq!(AccountType::parse("revenue").unwrap(), AccountType::Revenue);
        assert!(AccountType::parse("cash").is_err());
    }

    #[test]
    fn status_transitions() {
        use AccountStatus::*;
        assert!(Open.can_transition_to(Frozen));
        assert!(Frozen.can_transition_to(Open));
        assert!(Open.can_transition_to(Closed));
        assert!(!Closed.can_transition_to(Open));
        assert!(!Open.can_transition_to(Open));
        assert_eq!(AccountStatus::parse("frozen").unwrap(), Frozen);
        assert!(AccountStatus::parse("deleted").is_err());
    }

    #[test]
    fn balances_post_reserve_commit_release() {
        let mut b = Balances::default();
        b.apply(Effect::Post, Direction::Debit, 100).unwrap();
        assert_eq!(b.posted, 100);
        b.apply(Effect::Reserve, Direction::Credit, 30).unwrap();
        assert_eq!(b.available(AccountType::Asset), 70);
        b.apply(Effect::Commit, Direction::Credit, 30).unwrap();
        assert_eq!(b.posted, 70);
        assert_eq!(b.pending_credits, 0);
        b.apply(Effect::Reserve, Direction::Debit, 5).unwrap();
        b.apply(Effect::Release, Direction::Debit, 5).unwrap();
        assert!(!b.has_pending());
        assert!(b.apply(Effect::Release, Direction::Debit, 1).is_err());
    }

    #[test]
    fn credit_normal_available_counts_pending_debits() {
        let mut b = Balances::default();
        b.apply(Effect::Post, Direction::Credit, 500).unwrap();
        b.apply(Effect::Reserve, Direction::Debit, 200).unwrap();
        assert_eq!(b.normal_posted(AccountType::Liability), 500);
        assert_eq!(b.available(AccountType::Liability), 300);
    }

    #[test]
    fn overflow_is_detected() {
        let mut b = Balances {
            posted: i64::MAX,
            ..Default::default()
        };
        assert!(b.apply(Effect::Post, Direction::Debit, 1).is_err());
        let mut c = Balances {
            posted: i64::MIN + 1,
            ..Default::default()
        };
        assert!(c.apply(Effect::Post, Direction::Credit, 1).is_err());
    }

    #[test]
    fn policy_enforcement() {
        let mut acct = account(AccountType::Liability, BalancePolicy::NoOverdraft);
        let mut projected = acct.balances;
        projected.apply(Effect::Post, Direction::Debit, 10).unwrap();
        let err = acct.check_policy(&projected).unwrap_err();
        assert_eq!(err.code(), "insufficient_funds");

        acct.account.policy = BalancePolicy::Overdraft { limit: 10 };
        assert!(acct.check_policy(&projected).is_ok());
        projected.apply(Effect::Post, Direction::Debit, 1).unwrap();
        assert!(acct.check_policy(&projected).is_err());

        acct.account.policy = BalancePolicy::Unrestricted;
        assert!(acct.check_policy(&projected).is_ok());
        assert!(BalancePolicy::Overdraft { limit: -1 }.validate().is_err());
    }

    #[test]
    fn policy_allows_improvements_below_floor() {
        let mut acct = account(AccountType::Asset, BalancePolicy::NoOverdraft);
        acct.balances.posted = -50;
        let mut projected = acct.balances;
        projected.apply(Effect::Post, Direction::Debit, 20).unwrap();
        assert!(acct.check_policy(&projected).is_ok());
    }

    #[test]
    fn history_and_point_in_time() {
        let mut acct = account(AccountType::Asset, BalancePolicy::Unrestricted);
        let t0 = Utc::now();
        let tx = TransactionId::generate();
        for (i, amount) in [100, 50, 25].into_iter().enumerate() {
            let ts = t0 + chrono::Duration::seconds(i as i64);
            let ctx = EntryContext {
                sequence: (i as u64 + 1) * 10,
                timestamp: ts,
                transaction_id: tx,
            };
            acct.record(
                &ctx,
                EntryKind::Posted,
                Effect::Post,
                Direction::Debit,
                amount,
            )
            .unwrap();
        }
        assert_eq!(acct.posted_balance(), 175);
        assert_eq!(acct.balance_at_sequence(5).posted_balance, 0);
        assert_eq!(acct.balance_at_sequence(5).sequence, None);
        assert_eq!(acct.balance_at_sequence(10).posted_balance, 100);
        assert_eq!(acct.balance_at_sequence(25).posted_balance, 150);
        assert_eq!(acct.balance_at_sequence(u64::MAX).posted_balance, 175);
        let at = t0 + chrono::Duration::milliseconds(1500);
        assert_eq!(acct.balance_at_time(at).posted_balance, 150);
        assert_eq!(acct.history[2].normal_amount(AccountType::Asset), 25);
        assert_eq!(acct.history[2].normal_amount(AccountType::Liability), -25);
    }

    #[test]
    fn closing_requires_empty_account() {
        let mut acct = account(AccountType::Asset, BalancePolicy::Unrestricted);
        acct.balances.posted = 5;
        assert_eq!(
            acct.check_status_transition(AccountStatus::Closed)
                .unwrap_err()
                .code(),
            "account_not_empty"
        );
        acct.balances.posted = 0;
        acct.balances.pending_debits = 1;
        assert!(acct.check_status_transition(AccountStatus::Closed).is_err());
        acct.balances.pending_debits = 0;
        assert!(acct.check_status_transition(AccountStatus::Closed).is_ok());
        assert!(acct.check_status_transition(AccountStatus::Open).is_err());
    }

    #[test]
    fn version_and_open_checks() {
        let mut acct = account(AccountType::Asset, BalancePolicy::Unrestricted);
        assert!(acct.ensure_version(Some(1)).is_ok());
        assert!(acct.ensure_version(None).is_ok());
        assert_eq!(
            acct.ensure_version(Some(2)).unwrap_err().code(),
            "version_conflict"
        );
        acct.touch(Utc::now());
        assert_eq!(acct.account.version, 2);
        acct.account.status = AccountStatus::Frozen;
        assert_eq!(acct.ensure_open().unwrap_err().code(), "account_not_active");
    }

    #[test]
    fn policy_serialization_shape() {
        let json = serde_json::to_value(BalancePolicy::Overdraft { limit: 500 }).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "overdraft", "limit": 500}));
        let parsed: BalancePolicy =
            serde_json::from_value(serde_json::json!({"kind": "no_overdraft"})).unwrap();
        assert_eq!(parsed, BalancePolicy::NoOverdraft);
    }
}
