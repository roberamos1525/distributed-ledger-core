//! Multi-leg transactions and the balancing rules they must satisfy.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::ids::{AccountId, TransactionId};
use crate::domain::money::CurrencyCode;
use crate::error::{LedgerError, LedgerResult};

/// Side of a posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Debit,
    Credit,
}

impl Direction {
    pub fn opposite(self) -> Direction {
        match self {
            Direction::Debit => Direction::Credit,
            Direction::Credit => Direction::Debit,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Debit => "debit",
            Direction::Credit => "credit",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single leg of a transaction. Amounts are always positive; the direction
/// carries the sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posting {
    pub account_id: AccountId,
    pub direction: Direction,
    pub amount: i64,
    pub currency: CurrencyCode,
}

/// Lifecycle state of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    Pending,
    Posted,
    Voided,
}

impl TransactionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TransactionStatus::Pending => "pending",
            TransactionStatus::Posted => "posted",
            TransactionStatus::Voided => "voided",
        }
    }

    pub fn parse(raw: &str) -> LedgerResult<Self> {
        match raw {
            "pending" => Ok(TransactionStatus::Pending),
            "posted" => Ok(TransactionStatus::Posted),
            "voided" => Ok(TransactionStatus::Voided),
            other => Err(LedgerError::validation(format!(
                "unknown transaction status {other:?}"
            ))),
        }
    }
}

impl fmt::Display for TransactionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether the transaction is an ordinary transfer or reverses another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransactionKind {
    Standard,
    Reversal { original: TransactionId },
}

/// Idempotency information persisted with a transaction so that duplicate
/// submissions can be detected even if the HTTP idempotency cache was lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyTag {
    pub key: String,
    pub fingerprint: String,
}

/// Full transaction record as stored in state, events and snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionRecord {
    pub id: TransactionId,
    pub kind: TransactionKind,
    pub status: TransactionStatus,
    pub postings: Vec<Posting>,
    pub description: String,
    pub reference: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub created_sequence: u64,
    pub posted_at: Option<DateTime<Utc>>,
    pub posted_sequence: Option<u64>,
    pub voided_at: Option<DateTime<Utc>>,
    pub void_reason: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Amount of each posting that has already been reversed.
    pub reversed_amounts: Vec<i64>,
    /// Reversal transactions linked to this one.
    pub reversals: Vec<TransactionId>,
    pub idempotency: Option<IdempotencyTag>,
}

impl TransactionRecord {
    /// Distinct accounts referenced by the postings, in sorted order.
    pub fn affected_accounts(&self) -> Vec<AccountId> {
        affected_accounts(&self.postings)
    }

    /// Remaining reversible amount for the posting at `index`.
    pub fn remaining_reversible(&self, index: usize) -> i64 {
        let reversed = self.reversed_amounts.get(index).copied().unwrap_or(0);
        self.postings
            .get(index)
            .map(|p| p.amount - reversed)
            .unwrap_or(0)
    }

    pub fn is_fully_reversed(&self) -> bool {
        (0..self.postings.len()).all(|i| self.remaining_reversible(i) == 0)
    }

    pub fn is_reversal(&self) -> bool {
        matches!(self.kind, TransactionKind::Reversal { .. })
    }

    /// Whether a pending transaction has passed its expiry time.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.status == TransactionStatus::Pending && self.expires_at.is_some_and(|e| e <= now)
    }

    /// Total amount moved per currency (sum of debits).
    pub fn totals(&self) -> BTreeMap<CurrencyCode, i64> {
        let mut totals = BTreeMap::new();
        for p in self
            .postings
            .iter()
            .filter(|p| p.direction == Direction::Debit)
        {
            *totals.entry(p.currency).or_insert(0i64) += p.amount;
        }
        totals
    }
}

/// Distinct accounts referenced by a set of postings, sorted for
/// deterministic lock ordering.
pub fn affected_accounts(postings: &[Posting]) -> Vec<AccountId> {
    postings
        .iter()
        .map(|p| p.account_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Structural validation of postings: count, positive bounded amounts.
pub fn validate_postings(
    postings: &[Posting],
    max_legs: usize,
    max_amount: i64,
) -> LedgerResult<()> {
    if postings.len() < 2 {
        return Err(LedgerError::validation(
            "a transaction needs at least two postings",
        ));
    }
    if postings.len() > max_legs {
        return Err(LedgerError::validation(format!(
            "a transaction may have at most {max_legs} postings"
        )));
    }
    for (i, p) in postings.iter().enumerate() {
        if p.amount <= 0 {
            return Err(LedgerError::validation(format!(
                "posting {i}: amount must be positive"
            )));
        }
        if p.amount > max_amount {
            return Err(LedgerError::validation(format!(
                "posting {i}: amount exceeds the maximum of {max_amount}"
            )));
        }
    }
    check_balanced(postings)?;
    Ok(())
}

/// Verifies that debits equal credits for every currency, returning the
/// debit total per currency.
pub fn check_balanced(postings: &[Posting]) -> LedgerResult<BTreeMap<CurrencyCode, i64>> {
    let mut sums: BTreeMap<CurrencyCode, (i64, i64)> = BTreeMap::new();
    for p in postings {
        let entry = sums.entry(p.currency).or_insert((0, 0));
        let slot = match p.direction {
            Direction::Debit => &mut entry.0,
            Direction::Credit => &mut entry.1,
        };
        *slot = slot
            .checked_add(p.amount)
            .ok_or(LedgerError::Overflow("transaction total"))?;
    }
    for (currency, (debits, credits)) in &sums {
        if debits != credits {
            return Err(LedgerError::Unbalanced {
                currency: currency.to_string(),
                debits: *debits,
                credits: *credits,
            });
        }
    }
    Ok(sums.into_iter().map(|(c, (d, _))| (c, d)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn leg(account: &str, direction: Direction, amount: i64, currency: &str) -> Posting {
        Posting {
            account_id: AccountId::new(account).unwrap(),
            direction,
            amount,
            currency: CurrencyCode::new(currency).unwrap(),
        }
    }

    #[test]
    fn balanced_two_leg() {
        let postings = vec![
            leg("cash", Direction::Debit, 100, "USD"),
            leg("revenue", Direction::Credit, 100, "USD"),
        ];
        let totals = check_balanced(&postings).unwrap();
        assert_eq!(totals[&CurrencyCode::new("USD").unwrap()], 100);
        assert!(validate_postings(&postings, 10, 1_000).is_ok());
    }

    #[test]
    fn multi_currency_must_balance_per_currency() {
        let ok = vec![
            leg("usd-cash", Direction::Debit, 100, "USD"),
            leg("fx-usd", Direction::Credit, 100, "USD"),
            leg("fx-eur", Direction::Debit, 90, "EUR"),
            leg("eur-cash", Direction::Credit, 90, "EUR"),
        ];
        assert!(check_balanced(&ok).is_ok());
        let bad = vec![
            leg("usd-cash", Direction::Debit, 100, "USD"),
            leg("eur-cash", Direction::Credit, 100, "EUR"),
        ];
        let err = check_balanced(&bad).unwrap_err();
        assert_eq!(err.code(), "unbalanced_transaction");
    }

    #[test]
    fn structural_validation() {
        let single = vec![leg("a", Direction::Debit, 1, "USD")];
        assert!(validate_postings(&single, 10, 100).is_err());
        let zero = vec![
            leg("a", Direction::Debit, 0, "USD"),
            leg("b", Direction::Credit, 0, "USD"),
        ];
        assert!(validate_postings(&zero, 10, 100).is_err());
        let big = vec![
            leg("a", Direction::Debit, 101, "USD"),
            leg("b", Direction::Credit, 101, "USD"),
        ];
        assert!(validate_postings(&big, 10, 100).is_err());
        let many: Vec<_> = (0..4)
            .map(|i| {
                leg(
                    &format!("a{i}"),
                    if i % 2 == 0 {
                        Direction::Debit
                    } else {
                        Direction::Credit
                    },
                    1,
                    "USD",
                )
            })
            .collect();
        assert!(validate_postings(&many, 3, 100).is_err());
        assert!(validate_postings(&many, 4, 100).is_ok());
    }

    #[test]
    fn overflowing_totals_are_rejected() {
        let postings = vec![
            leg("a", Direction::Debit, i64::MAX, "USD"),
            leg("b", Direction::Debit, i64::MAX, "USD"),
            leg("c", Direction::Credit, 1, "USD"),
        ];
        assert_eq!(
            check_balanced(&postings).unwrap_err().code(),
            "arithmetic_overflow"
        );
    }

    #[test]
    fn affected_accounts_sorted_and_deduplicated() {
        let postings = vec![
            leg("zeta", Direction::Debit, 5, "USD"),
            leg("alpha", Direction::Credit, 3, "USD"),
            leg("alpha", Direction::Credit, 2, "USD"),
        ];
        let ids: Vec<_> = affected_accounts(&postings)
            .into_iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
    }

    #[test]
    fn status_parsing() {
        assert_eq!(
            TransactionStatus::parse("posted").unwrap(),
            TransactionStatus::Posted
        );
        assert!(TransactionStatus::parse("done").is_err());
        assert_eq!(Direction::Debit.opposite(), Direction::Credit);
    }

    proptest! {
        /// Any set of postings built from mirrored pairs balances; perturbing
        /// a single leg breaks the balance.
        #[test]
        fn mirrored_pairs_always_balance(amounts in proptest::collection::vec(1i64..1_000_000, 1..20), bump in 1i64..100) {
            let mut postings = Vec::new();
            for (i, amount) in amounts.iter().enumerate() {
                postings.push(leg(&format!("d{i}"), Direction::Debit, *amount, "USD"));
                postings.push(leg(&format!("c{i}"), Direction::Credit, *amount, "USD"));
            }
            prop_assert!(check_balanced(&postings).is_ok());
            postings[0].amount += bump;
            prop_assert!(check_balanced(&postings).is_err());
        }
    }
}
