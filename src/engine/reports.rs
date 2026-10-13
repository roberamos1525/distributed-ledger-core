//! Trial balance and ledger invariant verification.
//!
//! Both reports operate on a consistent [`LedgerState`] so the exact same
//! code serves the live API (via a brief consistent capture) and offline
//! tooling (`ledgerctl verify` after a replay).

use std::collections::BTreeMap;

use serde::Serialize;

use crate::domain::account::AccountType;
use crate::domain::ids::{AccountId, TransactionId};
use crate::domain::money::CurrencyCode;
use crate::domain::transaction::{Direction, TransactionKind, TransactionStatus, check_balanced};
use crate::engine::ledger::Ledger;
use crate::engine::state::LedgerState;

/// One account line of a trial balance.
#[derive(Debug, Clone, Serialize)]
pub struct TrialBalanceLine {
    pub account_id: AccountId,
    pub name: String,
    pub account_type: AccountType,
    /// Debit balance (0 when the account has a credit balance).
    pub debit: i64,
    /// Credit balance (0 when the account has a debit balance).
    pub credit: i64,
}

/// Trial balance for one currency.
#[derive(Debug, Clone, Serialize)]
pub struct CurrencyTrialBalance {
    pub currency: CurrencyCode,
    pub lines: Vec<TrialBalanceLine>,
    pub total_debits: i128,
    pub total_credits: i128,
    /// Net normal-side balance per account type.
    pub by_type: BTreeMap<AccountType, i128>,
    pub balanced: bool,
}

/// Trial balance across all currencies.
#[derive(Debug, Clone, Serialize)]
pub struct TrialBalance {
    pub as_of_sequence: u64,
    pub currencies: Vec<CurrencyTrialBalance>,
    pub balanced: bool,
}

/// Builds a trial balance from posted balances.
pub fn trial_balance(state: &LedgerState) -> TrialBalance {
    let mut per_currency: BTreeMap<CurrencyCode, CurrencyTrialBalance> = BTreeMap::new();
    for acct in state.accounts.values() {
        let a = &acct.account;
        let entry = per_currency
            .entry(a.currency)
            .or_insert_with(|| CurrencyTrialBalance {
                currency: a.currency,
                lines: Vec::new(),
                total_debits: 0,
                total_credits: 0,
                by_type: BTreeMap::new(),
                balanced: true,
            });
        let posted = acct.balances.posted;
        let (debit, credit) = if posted >= 0 {
            (posted, 0)
        } else {
            (0, -posted)
        };
        entry.total_debits += debit as i128;
        entry.total_credits += credit as i128;
        *entry.by_type.entry(a.account_type).or_insert(0) += acct.posted_balance() as i128;
        entry.lines.push(TrialBalanceLine {
            account_id: a.id.clone(),
            name: a.name.clone(),
            account_type: a.account_type,
            debit,
            credit,
        });
    }
    let mut currencies: Vec<_> = per_currency.into_values().collect();
    for c in &mut currencies {
        c.balanced = c.total_debits == c.total_credits;
    }
    TrialBalance {
        as_of_sequence: state.last_sequence,
        balanced: currencies.iter().all(|c| c.balanced),
        currencies,
    }
}

/// Result of an invariant check.
#[derive(Debug, Clone, Serialize)]
pub struct InvariantReport {
    pub ok: bool,
    pub as_of_sequence: u64,
    pub accounts_checked: usize,
    pub transactions_checked: usize,
    /// Sum of all signed posted balances per currency (must be zero).
    pub posted_sums: BTreeMap<CurrencyCode, i128>,
    pub violations: Vec<String>,
}

/// Verifies the ledger's structural invariants:
///
/// * the sum of all posted balances is zero per currency;
/// * pending debits equal pending credits per currency;
/// * every account's pending counters equal the legs of its pending
///   transactions, and its newest history entry matches its balances;
/// * every transaction balances per currency, references existing accounts in
///   their currency, and reversal bookkeeping is consistent.
pub fn check_invariants(state: &LedgerState) -> InvariantReport {
    let mut violations = Vec::new();
    let mut posted_sums: BTreeMap<CurrencyCode, i128> = BTreeMap::new();
    let mut pending_sums: BTreeMap<CurrencyCode, (i128, i128)> = BTreeMap::new();

    for acct in state.accounts.values() {
        let c = acct.account.currency;
        *posted_sums.entry(c).or_insert(0) += acct.balances.posted as i128;
        let p = pending_sums.entry(c).or_insert((0, 0));
        p.0 += acct.balances.pending_debits as i128;
        p.1 += acct.balances.pending_credits as i128;
        match acct.history.last() {
            Some(last) => {
                if last.posted_balance != acct.posted_balance()
                    || last.available_balance != acct.available_balance()
                {
                    violations.push(format!(
                        "account {}: last history entry ({}/{}) disagrees with balances ({}/{})",
                        acct.id(),
                        last.posted_balance,
                        last.available_balance,
                        acct.posted_balance(),
                        acct.available_balance()
                    ));
                }
            }
            None => {
                if acct.balances != Default::default() {
                    violations.push(format!(
                        "account {}: non-zero balances without history",
                        acct.id()
                    ));
                }
            }
        }
    }
    for (currency, sum) in &posted_sums {
        if *sum != 0 {
            violations.push(format!(
                "currency {currency}: posted balances sum to {sum}, expected 0"
            ));
        }
    }
    for (currency, (debits, credits)) in &pending_sums {
        if debits != credits {
            violations.push(format!(
                "currency {currency}: pending debits {debits} != pending credits {credits}"
            ));
        }
    }

    let mut expected_pending: BTreeMap<&AccountId, (i64, i64)> = BTreeMap::new();
    let mut reversal_totals: BTreeMap<TransactionId, i128> = BTreeMap::new();
    for tx in state.transactions.values() {
        if let Err(e) = check_balanced(&tx.postings) {
            violations.push(format!("transaction {}: {e}", tx.id));
        }
        if tx.reversed_amounts.len() != tx.postings.len() {
            violations.push(format!(
                "transaction {}: reversal bookkeeping has {} entries for {} postings",
                tx.id,
                tx.reversed_amounts.len(),
                tx.postings.len()
            ));
        }
        for (i, p) in tx.postings.iter().enumerate() {
            match state.accounts.get(&p.account_id) {
                None => violations.push(format!(
                    "transaction {}: posting {i} references unknown account {}",
                    tx.id, p.account_id
                )),
                Some(acct) if acct.account.currency != p.currency => violations.push(format!(
                    "transaction {}: posting {i} currency {} differs from account {} ({})",
                    tx.id, p.currency, p.account_id, acct.account.currency
                )),
                Some(_) => {}
            }
            let reversed = tx.reversed_amounts.get(i).copied().unwrap_or(0);
            if reversed < 0 || reversed > p.amount {
                violations.push(format!(
                    "transaction {}: posting {i} reversed amount {reversed} outside 0..={}",
                    tx.id, p.amount
                ));
            }
            if tx.status == TransactionStatus::Pending {
                let e = expected_pending.entry(&p.account_id).or_insert((0, 0));
                match p.direction {
                    Direction::Debit => e.0 += p.amount,
                    Direction::Credit => e.1 += p.amount,
                }
            }
        }
        if let TransactionKind::Reversal { original } = tx.kind {
            let total: i128 = tx.postings.iter().map(|p| p.amount as i128).sum();
            *reversal_totals.entry(original).or_insert(0) += total;
            match state.transactions.get(&original) {
                Some(orig) if orig.reversals.contains(&tx.id) => {}
                _ => violations.push(format!(
                    "reversal {}: original {original} does not link back to it",
                    tx.id
                )),
            }
        }
    }
    for tx in state.transactions.values() {
        let reversed: i128 = tx.reversed_amounts.iter().map(|a| *a as i128).sum();
        let recorded = reversal_totals.get(&tx.id).copied().unwrap_or(0);
        if reversed != recorded {
            violations.push(format!(
                "transaction {}: reversed amounts {reversed} != reversal postings {recorded}",
                tx.id
            ));
        }
    }
    for acct in state.accounts.values() {
        let (d, c) = expected_pending.get(acct.id()).copied().unwrap_or((0, 0));
        if acct.balances.pending_debits != d || acct.balances.pending_credits != c {
            violations.push(format!(
                "account {}: pending {}/{} but pending transactions sum to {d}/{c}",
                acct.id(),
                acct.balances.pending_debits,
                acct.balances.pending_credits
            ));
        }
    }

    InvariantReport {
        ok: violations.is_empty(),
        as_of_sequence: state.last_sequence,
        accounts_checked: state.accounts.len(),
        transactions_checked: state.transactions.len(),
        posted_sums,
        violations,
    }
}

impl Ledger {
    /// Trial balance over a consistent capture of the live ledger.
    pub fn trial_balance(&self) -> TrialBalance {
        trial_balance(&self.capture_summary())
    }

    /// Invariant check over a consistent capture of the live ledger.
    pub fn check_invariants(&self) -> InvariantReport {
        check_invariants(&self.capture_summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::{Account, AccountState, AccountStatus, BalancePolicy};
    use crate::domain::events::{EventEnvelope, LedgerEvent};
    use crate::domain::transaction::{Posting, TransactionRecord};
    use chrono::Utc;

    fn state_with(entries: &[(&str, AccountType, &str)]) -> LedgerState {
        let mut state = LedgerState::default();
        for (id, t, c) in entries {
            let now = Utc::now();
            let env = EventEnvelope {
                sequence: state.last_sequence + 1,
                timestamp: now,
                event: LedgerEvent::AccountOpened {
                    account: Account {
                        id: AccountId::new(*id).unwrap(),
                        name: id.to_string(),
                        account_type: *t,
                        currency: CurrencyCode::new(c).unwrap(),
                        status: AccountStatus::Open,
                        policy: BalancePolicy::Unrestricted,
                        metadata: Default::default(),
                        version: 1,
                        created_at: now,
                        created_sequence: 1,
                        updated_at: now,
                    },
                },
            };
            state.apply(&env).unwrap();
        }
        state
    }

    fn post(
        state: &mut LedgerState,
        legs: &[(&str, Direction, i64, &str)],
        status: TransactionStatus,
    ) -> TransactionId {
        let postings: Vec<Posting> = legs
            .iter()
            .map(|(a, d, amt, c)| Posting {
                account_id: AccountId::new(*a).unwrap(),
                direction: *d,
                amount: *amt,
                currency: CurrencyCode::new(c).unwrap(),
            })
            .collect();
        let id = TransactionId::generate();
        let env = EventEnvelope {
            sequence: state.last_sequence + 1,
            timestamp: Utc::now(),
            event: LedgerEvent::TransactionRecorded {
                transaction: TransactionRecord {
                    id,
                    kind: TransactionKind::Standard,
                    status,
                    reversed_amounts: vec![0; postings.len()],
                    postings,
                    description: String::new(),
                    reference: None,
                    metadata: Default::default(),
                    created_at: Utc::now(),
                    created_sequence: state.last_sequence + 1,
                    posted_at: None,
                    posted_sequence: None,
                    voided_at: None,
                    void_reason: None,
                    expires_at: None,
                    reversals: vec![],
                    idempotency: None,
                },
            },
        };
        state.apply(&env).unwrap();
        id
    }

    #[test]
    fn trial_balance_balances_per_currency() {
        let mut state = state_with(&[
            ("cash", AccountType::Asset, "USD"),
            ("equity", AccountType::Equity, "USD"),
            ("eur", AccountType::Asset, "EUR"),
            ("eur-eq", AccountType::Equity, "EUR"),
        ]);
        post(
            &mut state,
            &[
                ("cash", Direction::Debit, 500, "USD"),
                ("equity", Direction::Credit, 500, "USD"),
            ],
            TransactionStatus::Posted,
        );
        post(
            &mut state,
            &[
                ("eur", Direction::Debit, 70, "EUR"),
                ("eur-eq", Direction::Credit, 70, "EUR"),
            ],
            TransactionStatus::Posted,
        );
        let tb = trial_balance(&state);
        assert!(tb.balanced);
        assert_eq!(tb.currencies.len(), 2);
        let usd = tb
            .currencies
            .iter()
            .find(|c| c.currency.as_str() == "USD")
            .unwrap();
        assert_eq!(usd.total_debits, 500);
        assert_eq!(usd.total_credits, 500);
        assert_eq!(usd.by_type[&AccountType::Asset], 500);
        assert_eq!(usd.by_type[&AccountType::Equity], 500);
        let report = check_invariants(&state);
        assert!(report.ok, "{:?}", report.violations);
        assert_eq!(report.posted_sums[&CurrencyCode::new("USD").unwrap()], 0);
    }

    #[test]
    fn detects_tampered_balances() {
        let mut state = state_with(&[
            ("cash", AccountType::Asset, "USD"),
            ("equity", AccountType::Equity, "USD"),
        ]);
        post(
            &mut state,
            &[
                ("cash", Direction::Debit, 500, "USD"),
                ("equity", Direction::Credit, 500, "USD"),
            ],
            TransactionStatus::Posted,
        );
        let cash = AccountId::new("cash").unwrap();
        state.accounts.get_mut(&cash).unwrap().balances.posted += 1;
        let report = check_invariants(&state);
        assert!(!report.ok);
        assert!(report.violations.iter().any(|v| v.contains("sum to 1")));
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.contains("last history entry"))
        );
        assert!(!trial_balance(&state).balanced);
    }

    #[test]
    fn detects_pending_mismatch_and_broken_reversal_links() {
        let mut state = state_with(&[
            ("cash", AccountType::Asset, "USD"),
            ("equity", AccountType::Equity, "USD"),
        ]);
        let pending = post(
            &mut state,
            &[
                ("cash", Direction::Debit, 40, "USD"),
                ("equity", Direction::Credit, 40, "USD"),
            ],
            TransactionStatus::Pending,
        );
        assert!(check_invariants(&state).ok);
        state.transactions.get_mut(&pending).unwrap().status = TransactionStatus::Posted;
        let report = check_invariants(&state);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.contains("pending transactions sum"))
        );

        let mut state2 = state_with(&[
            ("cash", AccountType::Asset, "USD"),
            ("equity", AccountType::Equity, "USD"),
        ]);
        let tx = post(
            &mut state2,
            &[
                ("cash", Direction::Debit, 10, "USD"),
                ("equity", Direction::Credit, 10, "USD"),
            ],
            TransactionStatus::Posted,
        );
        state2.transactions.get_mut(&tx).unwrap().reversed_amounts = vec![5, 5];
        let report = check_invariants(&state2);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.contains("reversal postings"))
        );
    }

    #[test]
    fn detects_orphan_balances() {
        let mut state = state_with(&[("cash", AccountType::Asset, "USD")]);
        let cash = AccountId::new("cash").unwrap();
        let acct: &mut AccountState = state.accounts.get_mut(&cash).unwrap();
        acct.balances.pending_credits = 3;
        let report = check_invariants(&state);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.contains("without history"))
        );
    }
}
