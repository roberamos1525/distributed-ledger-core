//! Command inputs accepted by the engine, plus engine-wide limits.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::account::{AccountStatus, AccountType, BalancePolicy};
use crate::domain::events::ReversedLeg;
use crate::domain::ids::{
    AccountId, MAX_DESCRIPTION_LEN, TransactionId, validate_metadata, validate_reference,
    validate_text,
};
use crate::domain::money::CurrencyCode;
use crate::domain::transaction::{Direction, IdempotencyTag};
use crate::error::{LedgerError, LedgerResult};

/// Maximum length of an account name.
pub const MAX_ACCOUNT_NAME_LEN: usize = 256;
/// Maximum length of a status change / void reason.
pub const MAX_REASON_LEN: usize = 512;

/// Engine limits applied to every command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineLimits {
    /// Maximum number of postings in one transaction.
    pub max_postings: usize,
    /// Maximum amount (minor units) of a single posting.
    pub max_amount: i64,
    /// Upper bound for a pending transaction's time-to-live.
    pub max_pending_ttl: Duration,
}

impl Default for EngineLimits {
    fn default() -> Self {
        EngineLimits {
            max_postings: 64,
            max_amount: 1_000_000_000_000_000,
            max_pending_ttl: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

/// Default balance policy for each account type, used when an account is
/// opened without an explicit policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultPolicies(pub BTreeMap<AccountType, BalancePolicy>);

impl Default for DefaultPolicies {
    fn default() -> Self {
        let mut m = BTreeMap::new();
        m.insert(AccountType::Asset, BalancePolicy::NoOverdraft);
        m.insert(AccountType::Liability, BalancePolicy::NoOverdraft);
        m.insert(AccountType::Equity, BalancePolicy::Unrestricted);
        m.insert(AccountType::Revenue, BalancePolicy::Unrestricted);
        m.insert(AccountType::Expense, BalancePolicy::Unrestricted);
        DefaultPolicies(m)
    }
}

impl DefaultPolicies {
    pub fn for_type(&self, account_type: AccountType) -> BalancePolicy {
        self.0
            .get(&account_type)
            .copied()
            .unwrap_or(BalancePolicy::NoOverdraft)
    }
}

/// Open a new account.
#[derive(Debug, Clone, Default)]
pub struct OpenAccount {
    pub id: Option<AccountId>,
    pub name: String,
    pub account_type: Option<AccountType>,
    pub currency: String,
    pub policy: Option<BalancePolicy>,
    pub metadata: BTreeMap<String, String>,
}

impl OpenAccount {
    pub fn new(id: &str, account_type: AccountType, currency: &str) -> LedgerResult<Self> {
        Ok(OpenAccount {
            id: Some(AccountId::new(id)?),
            name: id.to_string(),
            account_type: Some(account_type),
            currency: currency.to_string(),
            policy: None,
            metadata: BTreeMap::new(),
        })
    }

    pub fn with_policy(mut self, policy: BalancePolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn validate(&self) -> LedgerResult<AccountType> {
        if self.name.trim().is_empty() {
            return Err(LedgerError::validation("account name must not be blank"));
        }
        validate_text("name", &self.name, MAX_ACCOUNT_NAME_LEN)?;
        validate_metadata(&self.metadata)?;
        if let Some(policy) = self.policy {
            policy.validate()?;
        }
        self.account_type
            .ok_or_else(|| LedgerError::validation("account_type is required"))
    }
}

/// One requested posting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingSpec {
    pub account_id: AccountId,
    pub direction: Direction,
    pub amount: i64,
    /// Optional currency assertion; must match the account's currency.
    pub currency: Option<CurrencyCode>,
}

impl PostingSpec {
    pub fn debit(account: &str, amount: i64) -> LedgerResult<Self> {
        Ok(PostingSpec {
            account_id: AccountId::new(account)?,
            direction: Direction::Debit,
            amount,
            currency: None,
        })
    }

    pub fn credit(account: &str, amount: i64) -> LedgerResult<Self> {
        Ok(PostingSpec {
            account_id: AccountId::new(account)?,
            direction: Direction::Credit,
            amount,
            currency: None,
        })
    }
}

/// Whether a new transaction posts immediately or is held as pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PostingMode {
    #[default]
    Posted,
    /// First phase of a two-phase transfer, optionally expiring.
    Pending { ttl: Option<Duration> },
}

/// Create a balanced multi-leg transaction.
#[derive(Debug, Clone, Default)]
pub struct CreateTransaction {
    pub postings: Vec<PostingSpec>,
    pub mode: PostingMode,
    pub description: String,
    pub reference: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// Optimistic concurrency: expected account versions.
    pub expected_versions: BTreeMap<AccountId, u64>,
    pub idempotency: Option<IdempotencyTag>,
}

impl CreateTransaction {
    pub fn posted(postings: Vec<PostingSpec>) -> Self {
        CreateTransaction {
            postings,
            ..Default::default()
        }
    }

    pub fn pending(postings: Vec<PostingSpec>, ttl: Option<Duration>) -> Self {
        CreateTransaction {
            postings,
            mode: PostingMode::Pending { ttl },
            ..Default::default()
        }
    }

    /// Validation that needs no ledger state.
    pub fn validate_shape(&self, limits: &EngineLimits) -> LedgerResult<()> {
        validate_text("description", &self.description, MAX_DESCRIPTION_LEN)?;
        if let Some(r) = &self.reference {
            validate_reference(r)?;
        }
        validate_metadata(&self.metadata)?;
        if let PostingMode::Pending { ttl: Some(ttl) } = self.mode {
            if ttl.is_zero() {
                return Err(LedgerError::validation("pending ttl must be positive"));
            }
            if ttl > limits.max_pending_ttl {
                return Err(LedgerError::validation(format!(
                    "pending ttl exceeds the maximum of {}s",
                    limits.max_pending_ttl.as_secs()
                )));
            }
        }
        for account in self.expected_versions.keys() {
            if !self.postings.iter().any(|p| &p.account_id == account) {
                return Err(LedgerError::validation(format!(
                    "expected_versions references {account}, which has no posting"
                )));
            }
        }
        Ok(())
    }
}

/// Reverse a posted transaction, fully or partially.
#[derive(Debug, Clone)]
pub struct ReverseTransaction {
    pub original: TransactionId,
    /// Legs to reverse; `None` reverses everything not yet reversed.
    pub legs: Option<Vec<ReversedLeg>>,
    pub description: Option<String>,
    pub reference: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

impl ReverseTransaction {
    pub fn full(original: TransactionId) -> Self {
        ReverseTransaction {
            original,
            legs: None,
            description: None,
            reference: None,
            metadata: BTreeMap::new(),
        }
    }

    pub fn partial(original: TransactionId, legs: Vec<ReversedLeg>) -> Self {
        ReverseTransaction {
            legs: Some(legs),
            ..Self::full(original)
        }
    }

    pub fn validate_shape(&self) -> LedgerResult<()> {
        if let Some(d) = &self.description {
            validate_text("description", d, MAX_DESCRIPTION_LEN)?;
        }
        if let Some(r) = &self.reference {
            validate_reference(r)?;
        }
        validate_metadata(&self.metadata)?;
        if let Some(legs) = &self.legs {
            if legs.is_empty() {
                return Err(LedgerError::validation(
                    "partial reversal requires at least one leg",
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            for leg in legs {
                if !seen.insert(leg.posting_index) {
                    return Err(LedgerError::validation(format!(
                        "posting index {} listed more than once",
                        leg.posting_index
                    )));
                }
                if leg.amount <= 0 {
                    return Err(LedgerError::validation(
                        "reversal leg amounts must be positive",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Change an account's lifecycle status.
#[derive(Debug, Clone)]
pub struct ChangeStatus {
    pub account_id: AccountId,
    pub status: AccountStatus,
    pub expected_version: Option<u64>,
    pub reason: Option<String>,
}

/// Merge-patch an account's metadata.
#[derive(Debug, Clone)]
pub struct UpdateMetadata {
    pub account_id: AccountId,
    pub set: BTreeMap<String, String>,
    pub remove: Vec<String>,
    pub expected_version: Option<u64>,
}

/// Replace an account's balance policy.
#[derive(Debug, Clone)]
pub struct ChangePolicy {
    pub account_id: AccountId,
    pub policy: BalancePolicy,
    pub expected_version: Option<u64>,
}

/// Validates an optional free-text reason.
pub fn validate_reason(reason: &Option<String>) -> LedgerResult<()> {
    if let Some(r) = reason {
        validate_text("reason", r, MAX_REASON_LEN)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policies_per_type() {
        let p = DefaultPolicies::default();
        assert_eq!(p.for_type(AccountType::Asset), BalancePolicy::NoOverdraft);
        assert_eq!(
            p.for_type(AccountType::Revenue),
            BalancePolicy::Unrestricted
        );
        let empty = DefaultPolicies(BTreeMap::new());
        assert_eq!(
            empty.for_type(AccountType::Equity),
            BalancePolicy::NoOverdraft
        );
    }

    #[test]
    fn open_account_validation() {
        let ok = OpenAccount::new("cash", AccountType::Asset, "USD").unwrap();
        assert_eq!(ok.validate().unwrap(), AccountType::Asset);
        let mut blank = ok.clone();
        blank.name = "  ".into();
        assert!(blank.validate().is_err());
        let mut no_type = ok.clone();
        no_type.account_type = None;
        assert!(no_type.validate().is_err());
        let bad_policy = ok.with_policy(BalancePolicy::Overdraft { limit: -5 });
        assert!(bad_policy.validate().is_err());
    }

    #[test]
    fn transaction_shape_validation() {
        let limits = EngineLimits {
            max_pending_ttl: Duration::from_secs(60),
            ..Default::default()
        };
        let postings = vec![
            PostingSpec::debit("a", 1).unwrap(),
            PostingSpec::credit("b", 1).unwrap(),
        ];
        let ok = CreateTransaction::pending(postings.clone(), Some(Duration::from_secs(30)));
        assert!(ok.validate_shape(&limits).is_ok());
        let too_long = CreateTransaction::pending(postings.clone(), Some(Duration::from_secs(61)));
        assert!(too_long.validate_shape(&limits).is_err());
        let zero = CreateTransaction::pending(postings.clone(), Some(Duration::ZERO));
        assert!(zero.validate_shape(&limits).is_err());
        let mut stray = CreateTransaction::posted(postings.clone());
        stray
            .expected_versions
            .insert(AccountId::new("other").unwrap(), 1);
        assert!(stray.validate_shape(&limits).is_err());
        let mut bad_ref = CreateTransaction::posted(postings);
        bad_ref.reference = Some(" ".into());
        assert!(bad_ref.validate_shape(&limits).is_err());
    }

    #[test]
    fn reversal_shape_validation() {
        let id = TransactionId::generate();
        assert!(ReverseTransaction::full(id).validate_shape().is_ok());
        assert!(
            ReverseTransaction::partial(id, vec![])
                .validate_shape()
                .is_err()
        );
        let dup = vec![
            ReversedLeg {
                posting_index: 0,
                amount: 1,
            },
            ReversedLeg {
                posting_index: 0,
                amount: 1,
            },
        ];
        assert!(
            ReverseTransaction::partial(id, dup)
                .validate_shape()
                .is_err()
        );
        let neg = vec![ReversedLeg {
            posting_index: 0,
            amount: -1,
        }];
        assert!(
            ReverseTransaction::partial(id, neg)
                .validate_shape()
                .is_err()
        );
        assert!(validate_reason(&Some("x".repeat(MAX_REASON_LEN + 1))).is_err());
        assert!(validate_reason(&None).is_ok());
    }
}
