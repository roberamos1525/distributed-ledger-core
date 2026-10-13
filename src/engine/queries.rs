//! Read-side queries: lookups, filtered listings with cursor pagination,
//! current and point-in-time balances, and account history.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::domain::account::{Account, AccountStatus, AccountType, HistoryEntry};
use crate::domain::ids::{AccountId, TransactionId};
use crate::domain::money::CurrencyCode;
use crate::domain::transaction::{Direction, TransactionRecord, TransactionStatus};
use crate::engine::ledger::Ledger;
use crate::error::{LedgerError, LedgerResult};

/// Hard cap on page sizes regardless of what the caller asks for.
pub const MAX_PAGE_SIZE: usize = 500;

/// A page of results plus the cursor for the next page, if any.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T, C> {
    pub items: Vec<T>,
    pub next_cursor: Option<C>,
}

/// Account attributes together with its current balances.
#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    pub balance: BalanceView,
}

/// Balances on the account's normal side.
#[derive(Debug, Clone, Serialize)]
pub struct BalanceView {
    pub account_id: AccountId,
    pub currency: CurrencyCode,
    pub posted: i64,
    pub available: i64,
    pub pending_debits: i64,
    pub pending_credits: i64,
    /// Sequence the balance is valid at.
    pub as_of_sequence: Option<u64>,
    pub as_of_time: Option<DateTime<Utc>>,
    pub version: u64,
}

/// Point in time for a balance query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsOf {
    Latest,
    Sequence(u64),
    Time(DateTime<Utc>),
}

/// Filters for listing accounts.
#[derive(Debug, Clone, Default)]
pub struct AccountFilter {
    pub account_type: Option<AccountType>,
    pub status: Option<AccountStatus>,
    pub currency: Option<CurrencyCode>,
}

impl AccountFilter {
    fn matches(&self, a: &Account) -> bool {
        self.account_type.is_none_or(|t| t == a.account_type)
            && self.status.is_none_or(|s| s == a.status)
            && self.currency.is_none_or(|c| c == a.currency)
    }
}

/// Filters for listing transactions.
#[derive(Debug, Clone, Default)]
pub struct TransactionFilter {
    pub status: Option<TransactionStatus>,
    pub account_id: Option<AccountId>,
    pub reference: Option<String>,
}

impl TransactionFilter {
    fn matches(&self, tx: &TransactionRecord) -> bool {
        self.status.is_none_or(|s| s == tx.status)
            && self
                .account_id
                .as_ref()
                .is_none_or(|id| tx.postings.iter().any(|p| &p.account_id == id))
            && self
                .reference
                .as_ref()
                .is_none_or(|r| tx.reference.as_deref() == Some(r.as_str()))
    }
}

/// Ordering of history pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryOrder {
    #[default]
    Ascending,
    Descending,
}

fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_PAGE_SIZE)
}

impl Ledger {
    /// Returns a copy of a transaction.
    pub fn transaction(&self, id: &TransactionId) -> LedgerResult<TransactionRecord> {
        Ok(self.tx_cell(id)?.lock().clone())
    }

    /// Returns an account with its current balances.
    pub fn account(&self, id: &AccountId) -> LedgerResult<AccountView> {
        let cell = self.account_cell(id)?;
        let state = cell.lock();
        Ok(AccountView {
            account: state.account.clone(),
            balance: current_balance(&state),
        })
    }

    /// Lists accounts in id order, starting after `after`.
    pub fn list_accounts(
        &self,
        filter: &AccountFilter,
        after: Option<&AccountId>,
        limit: usize,
    ) -> Page<AccountView, AccountId> {
        let limit = clamp_limit(limit);
        let cells: Vec<_> = {
            let index = self.accounts.read();
            match after {
                Some(after) => index
                    .range::<AccountId, _>((
                        std::ops::Bound::Excluded(after),
                        std::ops::Bound::Unbounded,
                    ))
                    .map(|(_, c)| c.clone())
                    .collect(),
                None => index.values().cloned().collect(),
            }
        };
        let mut items = Vec::new();
        let mut next_cursor = None;
        for cell in cells {
            let state = cell.lock();
            if !filter.matches(&state.account) {
                continue;
            }
            if items.len() == limit {
                next_cursor = items.last().map(|v: &AccountView| v.account.id.clone());
                break;
            }
            items.push(AccountView {
                account: state.account.clone(),
                balance: current_balance(&state),
            });
        }
        Page { items, next_cursor }
    }

    /// Lists transactions in creation order, starting after sequence `after`.
    pub fn list_transactions(
        &self,
        filter: &TransactionFilter,
        after: Option<u64>,
        limit: usize,
    ) -> Page<TransactionRecord, u64> {
        let limit = clamp_limit(limit);
        let ids: Vec<(u64, TransactionId)> = {
            let order = self.tx_order.read();
            let start = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
            order
                .range((start, std::ops::Bound::Unbounded))
                .map(|(seq, id)| (*seq, *id))
                .collect()
        };
        let mut items: Vec<TransactionRecord> = Vec::new();
        let mut next_cursor = None;
        for (_, id) in ids {
            let Ok(cell) = self.tx_cell(&id) else {
                continue;
            };
            let tx = cell.lock().clone();
            if !filter.matches(&tx) {
                continue;
            }
            if items.len() == limit {
                next_cursor = items.last().map(|t| t.created_sequence);
                break;
            }
            items.push(tx);
        }
        Page { items, next_cursor }
    }

    /// Balance of an account now or at a past point.
    pub fn balance(&self, id: &AccountId, as_of: AsOf) -> LedgerResult<BalanceView> {
        let cell = self.account_cell(id)?;
        let state = cell.lock();
        let historical = match as_of {
            AsOf::Latest => return Ok(current_balance(&state)),
            AsOf::Sequence(seq) => {
                if seq > self.last_sequence() {
                    return Err(LedgerError::validation(format!(
                        "sequence {seq} is beyond the last committed sequence {}",
                        self.last_sequence()
                    )));
                }
                state.balance_at_sequence(seq)
            }
            AsOf::Time(t) => state.balance_at_time(t),
        };
        let (as_of_sequence, as_of_time) = match as_of {
            AsOf::Sequence(s) => (Some(s), None),
            AsOf::Time(t) => (historical.sequence, Some(t)),
            AsOf::Latest => unreachable!("handled above"),
        };
        // Pending amounts at a past point are derived from the difference
        // between posted and available on the normal side.
        let reserved = historical.posted_balance - historical.available_balance;
        let (pending_debits, pending_credits) = match state.account.account_type.normal_side() {
            Direction::Debit => (0, reserved),
            Direction::Credit => (reserved, 0),
        };
        Ok(BalanceView {
            account_id: state.account.id.clone(),
            currency: state.account.currency,
            posted: historical.posted_balance,
            available: historical.available_balance,
            pending_debits,
            pending_credits,
            as_of_sequence,
            as_of_time,
            version: state.account.version,
        })
    }

    /// A page of account history. The cursor is the position in the
    /// account's append-only history, which stays stable as entries are added.
    pub fn history(
        &self,
        id: &AccountId,
        cursor: Option<usize>,
        limit: usize,
        order: HistoryOrder,
    ) -> LedgerResult<Page<HistoryEntry, usize>> {
        let limit = clamp_limit(limit);
        let cell = self.account_cell(id)?;
        let state = cell.lock();
        let len = state.history.len();
        let page = match order {
            HistoryOrder::Ascending => {
                let start = cursor.unwrap_or(0).min(len);
                let end = (start + limit).min(len);
                Page {
                    items: state.history[start..end].to_vec(),
                    next_cursor: (end < len).then_some(end),
                }
            }
            HistoryOrder::Descending => {
                let end = cursor.unwrap_or(len).min(len);
                let start = end.saturating_sub(limit);
                let mut items = state.history[start..end].to_vec();
                items.reverse();
                Page {
                    items,
                    next_cursor: (start > 0).then_some(start),
                }
            }
        };
        Ok(page)
    }

    /// History entries that changed the posted balance within a time window,
    /// together with the account's type and currency.
    pub fn posted_entries_between(
        &self,
        id: &AccountId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> LedgerResult<(Account, Vec<HistoryEntry>)> {
        let cell = self.account_cell(id)?;
        let state = cell.lock();
        let entries = state
            .history
            .iter()
            .filter(|e| e.kind.affects_posted() && e.timestamp >= from && e.timestamp <= to)
            .cloned()
            .collect();
        Ok((state.account.clone(), entries))
    }
}

fn current_balance(state: &crate::domain::account::AccountState) -> BalanceView {
    BalanceView {
        account_id: state.account.id.clone(),
        currency: state.account.currency,
        posted: state.posted_balance(),
        available: state.available_balance(),
        pending_debits: state.balances.pending_debits,
        pending_credits: state.balances.pending_credits,
        as_of_sequence: state.history.last().map(|e| e.sequence),
        as_of_time: None,
        version: state.account.version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::commands::{CreateTransaction, OpenAccount, PostingSpec};
    use crate::engine::ledger::LedgerOptions;
    use crate::metrics::LedgerMetrics;
    use std::sync::Arc;

    fn ledger_with_activity() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(
            LedgerOptions::ephemeral(dir.path()),
            Arc::new(LedgerMetrics::new()),
        )
        .unwrap();
        for (id, t, c) in [
            ("cash", AccountType::Asset, "USD"),
            ("equity", AccountType::Equity, "USD"),
            ("eur-cash", AccountType::Asset, "EUR"),
            ("eur-equity", AccountType::Equity, "EUR"),
        ] {
            ledger
                .open_account(OpenAccount::new(id, t, c).unwrap())
                .unwrap();
        }
        for amount in 1..=5 {
            let mut cmd = CreateTransaction::posted(vec![
                PostingSpec::debit("cash", amount * 100).unwrap(),
                PostingSpec::credit("equity", amount * 100).unwrap(),
            ]);
            cmd.reference = Some(format!("REF-{amount}"));
            ledger.create_transaction(cmd).unwrap();
        }
        (dir, ledger)
    }

    #[test]
    fn account_listing_filters_and_paginates() {
        let (_dir, ledger) = ledger_with_activity();
        let page = ledger.list_accounts(&AccountFilter::default(), None, 3);
        assert_eq!(page.items.len(), 3);
        let next = page.next_cursor.clone().unwrap();
        let rest = ledger.list_accounts(&AccountFilter::default(), Some(&next), 3);
        assert_eq!(rest.items.len(), 1);
        assert!(rest.next_cursor.is_none());
        let eur = AccountFilter {
            currency: Some(CurrencyCode::new("EUR").unwrap()),
            ..Default::default()
        };
        assert_eq!(ledger.list_accounts(&eur, None, 10).items.len(), 2);
        let assets = AccountFilter {
            account_type: Some(AccountType::Asset),
            status: Some(AccountStatus::Open),
            ..Default::default()
        };
        assert_eq!(ledger.list_accounts(&assets, None, 10).items.len(), 2);
    }

    #[test]
    fn transaction_listing_filters_and_paginates() {
        let (_dir, ledger) = ledger_with_activity();
        let page = ledger.list_transactions(&TransactionFilter::default(), None, 2);
        assert_eq!(page.items.len(), 2);
        let mut seen = page.items.len();
        let mut cursor = page.next_cursor;
        while let Some(c) = cursor {
            let p = ledger.list_transactions(&TransactionFilter::default(), Some(c), 2);
            seen += p.items.len();
            cursor = p.next_cursor;
        }
        assert_eq!(seen, 5);
        let by_ref = TransactionFilter {
            reference: Some("REF-3".into()),
            ..Default::default()
        };
        let found = ledger.list_transactions(&by_ref, None, 10);
        assert_eq!(found.items.len(), 1);
        assert_eq!(found.items[0].postings[0].amount, 300);
        let other_account = TransactionFilter {
            account_id: Some(AccountId::new("eur-cash").unwrap()),
            ..Default::default()
        };
        assert!(
            ledger
                .list_transactions(&other_account, None, 10)
                .items
                .is_empty()
        );
    }

    #[test]
    fn point_in_time_balances() {
        let (_dir, ledger) = ledger_with_activity();
        let cash = AccountId::new("cash").unwrap();
        let latest = ledger.balance(&cash, AsOf::Latest).unwrap();
        assert_eq!(latest.posted, 1500);
        // Accounts opened at sequences 1..=4, transactions at 5..=9.
        let at_6 = ledger.balance(&cash, AsOf::Sequence(6)).unwrap();
        assert_eq!(at_6.posted, 300);
        let at_4 = ledger.balance(&cash, AsOf::Sequence(4)).unwrap();
        assert_eq!(at_4.posted, 0);
        assert!(ledger.balance(&cash, AsOf::Sequence(99)).is_err());
        let past = Utc::now() - chrono::Duration::days(1);
        assert_eq!(ledger.balance(&cash, AsOf::Time(past)).unwrap().posted, 0);
        let future = Utc::now() + chrono::Duration::days(1);
        assert_eq!(
            ledger.balance(&cash, AsOf::Time(future)).unwrap().posted,
            1500
        );
    }

    #[test]
    fn history_pages_both_directions() {
        let (_dir, ledger) = ledger_with_activity();
        let cash = AccountId::new("cash").unwrap();
        let first = ledger
            .history(&cash, None, 2, HistoryOrder::Ascending)
            .unwrap();
        assert_eq!(first.items[0].amount, 100);
        assert_eq!(first.next_cursor, Some(2));
        let second = ledger
            .history(&cash, first.next_cursor, 10, HistoryOrder::Ascending)
            .unwrap();
        assert_eq!(second.items.len(), 3);
        assert!(second.next_cursor.is_none());

        let newest = ledger
            .history(&cash, None, 2, HistoryOrder::Descending)
            .unwrap();
        assert_eq!(newest.items[0].amount, 500);
        assert_eq!(newest.items[1].amount, 400);
        let older = ledger
            .history(&cash, newest.next_cursor, 10, HistoryOrder::Descending)
            .unwrap();
        assert_eq!(older.items.len(), 3);
        assert_eq!(older.items[2].amount, 100);
        assert!(
            ledger
                .history(
                    &AccountId::new("nope").unwrap(),
                    None,
                    1,
                    HistoryOrder::Ascending
                )
                .is_err()
        );
    }

    #[test]
    fn posted_entries_window() {
        let (_dir, ledger) = ledger_with_activity();
        let cash = AccountId::new("cash").unwrap();
        let (account, entries) = ledger
            .posted_entries_between(
                &cash,
                Utc::now() - chrono::Duration::hours(1),
                Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        assert_eq!(account.account_type, AccountType::Asset);
        assert_eq!(entries.len(), 5);
        let view = ledger.account(&cash).unwrap();
        assert_eq!(view.balance.posted, 1500);
        assert_eq!(view.account.version, 6);
    }
}
