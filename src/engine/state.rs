//! Event application: the single code path that turns events into state.
//!
//! [`apply_event`] is used both by replay (on the owned [`LedgerState`]) and by
//! the live engine (on a view over locked accounts), which guarantees that a
//! rebuilt ledger is identical to the one that produced the log.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::account::{AccountState, Effect, EntryContext, EntryKind};
use crate::domain::events::{EventEnvelope, LedgerEvent};
use crate::domain::ids::{AccountId, TransactionId};
use crate::domain::transaction::{
    Posting, TransactionRecord, TransactionStatus, affected_accounts,
};
use crate::error::{LedgerError, LedgerResult};
use crate::store::recovery::ReplayTarget;

/// Mutable access to the parts of the ledger an event may touch.
pub trait LedgerView {
    fn account_mut(&mut self, id: &AccountId) -> Option<&mut AccountState>;
    fn insert_account(&mut self, state: AccountState) -> LedgerResult<()>;
    fn transaction_mut(&mut self, id: &TransactionId) -> Option<&mut TransactionRecord>;
    fn insert_transaction(&mut self, tx: TransactionRecord) -> LedgerResult<()>;
}

fn account<'a, V: LedgerView>(
    view: &'a mut V,
    id: &AccountId,
) -> LedgerResult<&'a mut AccountState> {
    view.account_mut(id)
        .ok_or_else(|| LedgerError::AccountNotFound(id.clone()))
}

fn transaction<'a, V: LedgerView>(
    view: &'a mut V,
    id: &TransactionId,
) -> LedgerResult<&'a mut TransactionRecord> {
    view.transaction_mut(id)
        .ok_or(LedgerError::TransactionNotFound(*id))
}

fn expect_status(tx: &TransactionRecord, expected: TransactionStatus) -> LedgerResult<()> {
    if tx.status != expected {
        return Err(LedgerError::InvalidTransactionState {
            id: tx.id,
            status: tx.status,
            expected,
        });
    }
    Ok(())
}

/// Applies all legs to their accounts and bumps each touched account's
/// version exactly once.
fn apply_legs<V: LedgerView>(
    view: &mut V,
    ctx: &EntryContext,
    postings: &[Posting],
    kind: EntryKind,
    effect: Effect,
) -> LedgerResult<()> {
    for p in postings {
        account(view, &p.account_id)?.record(ctx, kind, effect, p.direction, p.amount)?;
    }
    for id in affected_accounts(postings) {
        account(view, &id)?.touch(ctx.timestamp);
    }
    Ok(())
}

/// Applies one event to a ledger view.
pub fn apply_event<V: LedgerView>(view: &mut V, env: &EventEnvelope) -> LedgerResult<()> {
    let ts = env.timestamp;
    match &env.event {
        LedgerEvent::AccountOpened { account: acct } => {
            view.insert_account(AccountState::new(acct.clone()))
        }
        LedgerEvent::AccountStatusChanged { account_id, to, .. } => {
            let a = account(view, account_id)?;
            a.account.status = *to;
            a.touch(ts);
            Ok(())
        }
        LedgerEvent::AccountMetadataUpdated {
            account_id,
            metadata,
        } => {
            let a = account(view, account_id)?;
            a.account.metadata = metadata.clone();
            a.touch(ts);
            Ok(())
        }
        LedgerEvent::AccountPolicyChanged { account_id, policy } => {
            let a = account(view, account_id)?;
            a.account.policy = *policy;
            a.touch(ts);
            Ok(())
        }
        LedgerEvent::TransactionRecorded { transaction: tx } => {
            let (kind, effect) = match tx.status {
                TransactionStatus::Posted => (EntryKind::Posted, Effect::Post),
                TransactionStatus::Pending => (EntryKind::PendingCreated, Effect::Reserve),
                TransactionStatus::Voided => {
                    return Err(LedgerError::validation(
                        "a transaction cannot be recorded as voided",
                    ));
                }
            };
            let ctx = EntryContext {
                sequence: env.sequence,
                timestamp: ts,
                transaction_id: tx.id,
            };
            apply_legs(view, &ctx, &tx.postings, kind, effect)?;
            view.insert_transaction(tx.clone())
        }
        LedgerEvent::PendingPosted { transaction_id } => {
            let postings = {
                let tx = transaction(view, transaction_id)?;
                expect_status(tx, TransactionStatus::Pending)?;
                tx.status = TransactionStatus::Posted;
                tx.posted_at = Some(ts);
                tx.posted_sequence = Some(env.sequence);
                tx.postings.clone()
            };
            let ctx = EntryContext {
                sequence: env.sequence,
                timestamp: ts,
                transaction_id: *transaction_id,
            };
            apply_legs(
                view,
                &ctx,
                &postings,
                EntryKind::PendingPosted,
                Effect::Commit,
            )
        }
        LedgerEvent::PendingVoided {
            transaction_id,
            reason,
            ..
        } => {
            let postings = {
                let tx = transaction(view, transaction_id)?;
                expect_status(tx, TransactionStatus::Pending)?;
                tx.status = TransactionStatus::Voided;
                tx.voided_at = Some(ts);
                tx.void_reason = reason.clone();
                tx.postings.clone()
            };
            let ctx = EntryContext {
                sequence: env.sequence,
                timestamp: ts,
                transaction_id: *transaction_id,
            };
            apply_legs(
                view,
                &ctx,
                &postings,
                EntryKind::PendingVoided,
                Effect::Release,
            )
        }
        LedgerEvent::TransactionReversed {
            original_id,
            reversal,
            legs,
        } => {
            {
                let original = transaction(view, original_id)?;
                expect_status(original, TransactionStatus::Posted)?;
                for leg in legs {
                    let remaining = original.remaining_reversible(leg.posting_index);
                    if leg.amount <= 0 || leg.amount > remaining {
                        return Err(LedgerError::InvalidReversal {
                            id: *original_id,
                            reason: format!(
                                "leg {} amount {} exceeds remaining {remaining}",
                                leg.posting_index, leg.amount
                            ),
                        });
                    }
                    original.reversed_amounts[leg.posting_index] += leg.amount;
                }
                original.reversals.push(reversal.id);
            }
            let ctx = EntryContext {
                sequence: env.sequence,
                timestamp: ts,
                transaction_id: reversal.id,
            };
            apply_legs(
                view,
                &ctx,
                &reversal.postings,
                EntryKind::Reversal,
                Effect::Post,
            )?;
            view.insert_transaction(reversal.clone())
        }
    }
}

/// Owned, single-threaded ledger state. Used for replay, snapshots and
/// offline tooling; the live engine converts to and from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerState {
    pub last_sequence: u64,
    pub last_timestamp: Option<DateTime<Utc>>,
    pub accounts: BTreeMap<AccountId, AccountState>,
    pub transactions: BTreeMap<TransactionId, TransactionRecord>,
}

impl LedgerView for LedgerState {
    fn account_mut(&mut self, id: &AccountId) -> Option<&mut AccountState> {
        self.accounts.get_mut(id)
    }

    fn insert_account(&mut self, state: AccountState) -> LedgerResult<()> {
        if self.accounts.contains_key(state.id()) {
            return Err(LedgerError::AccountExists(state.id().clone()));
        }
        self.accounts.insert(state.id().clone(), state);
        Ok(())
    }

    fn transaction_mut(&mut self, id: &TransactionId) -> Option<&mut TransactionRecord> {
        self.transactions.get_mut(id)
    }

    fn insert_transaction(&mut self, tx: TransactionRecord) -> LedgerResult<()> {
        if self.transactions.contains_key(&tx.id) {
            return Err(LedgerError::validation(format!(
                "duplicate transaction id {}",
                tx.id
            )));
        }
        self.transactions.insert(tx.id, tx);
        Ok(())
    }
}

impl LedgerState {
    /// Applies an event, enforcing strict sequence continuity.
    pub fn apply(&mut self, env: &EventEnvelope) -> LedgerResult<()> {
        if env.sequence != self.last_sequence + 1 {
            return Err(LedgerError::validation(format!(
                "out-of-order event: expected sequence {}, got {}",
                self.last_sequence + 1,
                env.sequence
            )));
        }
        apply_event(self, env)?;
        self.last_sequence = env.sequence;
        self.last_timestamp = Some(env.timestamp);
        Ok(())
    }

    /// Index of idempotency keys recorded on transactions.
    pub fn idempotency_index(&self) -> HashMap<String, (TransactionId, String)> {
        self.transactions
            .values()
            .filter_map(|tx| {
                tx.idempotency
                    .as_ref()
                    .map(|tag| (tag.key.clone(), (tx.id, tag.fingerprint.clone())))
            })
            .collect()
    }
}

impl ReplayTarget for LedgerState {
    fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    fn last_timestamp(&self) -> Option<DateTime<Utc>> {
        self.last_timestamp
    }

    fn apply_event(&mut self, envelope: &EventEnvelope) -> Result<(), String> {
        self.apply(envelope).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::{Account, AccountStatus, AccountType, BalancePolicy};
    use crate::domain::events::ReversedLeg;
    use crate::domain::money::CurrencyCode;
    use crate::domain::transaction::{Direction, TransactionKind};

    struct Builder {
        state: LedgerState,
    }

    impl Builder {
        fn new() -> Self {
            Builder {
                state: LedgerState::default(),
            }
        }

        fn push(&mut self, event: LedgerEvent) -> LedgerResult<()> {
            let env = EventEnvelope {
                sequence: self.state.last_sequence + 1,
                timestamp: Utc::now(),
                event,
            };
            self.state.apply(&env)
        }

        fn open(&mut self, id: &str, account_type: AccountType) {
            let now = Utc::now();
            self.push(LedgerEvent::AccountOpened {
                account: Account {
                    id: AccountId::new(id).unwrap(),
                    name: id.into(),
                    account_type,
                    currency: CurrencyCode::new("USD").unwrap(),
                    status: AccountStatus::Open,
                    policy: BalancePolicy::Unrestricted,
                    metadata: BTreeMap::new(),
                    version: 1,
                    created_at: now,
                    created_sequence: self.state.last_sequence + 1,
                    updated_at: now,
                },
            })
            .unwrap();
        }
    }

    fn tx(status: TransactionStatus, debit: &str, credit: &str, amount: i64) -> TransactionRecord {
        let usd = CurrencyCode::new("USD").unwrap();
        TransactionRecord {
            id: TransactionId::generate(),
            kind: TransactionKind::Standard,
            status,
            postings: vec![
                Posting {
                    account_id: AccountId::new(debit).unwrap(),
                    direction: Direction::Debit,
                    amount,
                    currency: usd,
                },
                Posting {
                    account_id: AccountId::new(credit).unwrap(),
                    direction: Direction::Credit,
                    amount,
                    currency: usd,
                },
            ],
            description: String::new(),
            reference: None,
            metadata: BTreeMap::new(),
            created_at: Utc::now(),
            created_sequence: 0,
            posted_at: None,
            posted_sequence: None,
            voided_at: None,
            void_reason: None,
            expires_at: None,
            reversed_amounts: vec![0, 0],
            reversals: vec![],
            idempotency: None,
        }
    }

    fn balance(state: &LedgerState, id: &str) -> i64 {
        state.accounts[&AccountId::new(id).unwrap()].posted_balance()
    }

    #[test]
    fn posted_transaction_moves_balances_and_versions() {
        let mut b = Builder::new();
        b.open("cash", AccountType::Asset);
        b.open("equity", AccountType::Equity);
        b.push(LedgerEvent::TransactionRecorded {
            transaction: tx(TransactionStatus::Posted, "cash", "equity", 1000),
        })
        .unwrap();
        assert_eq!(balance(&b.state, "cash"), 1000);
        assert_eq!(balance(&b.state, "equity"), 1000);
        let cash = &b.state.accounts[&AccountId::new("cash").unwrap()];
        assert_eq!(cash.account.version, 2);
        assert_eq!(cash.history.len(), 1);
        assert_eq!(b.state.last_sequence, 3);
    }

    #[test]
    fn pending_lifecycle() {
        let mut b = Builder::new();
        b.open("cash", AccountType::Asset);
        b.open("wallet", AccountType::Liability);
        let pending = tx(TransactionStatus::Pending, "cash", "wallet", 300);
        let id = pending.id;
        b.push(LedgerEvent::TransactionRecorded {
            transaction: pending,
        })
        .unwrap();
        let wallet = AccountId::new("wallet").unwrap();
        assert_eq!(b.state.accounts[&wallet].balances.pending_credits, 300);
        assert_eq!(balance(&b.state, "wallet"), 0);
        b.push(LedgerEvent::PendingPosted { transaction_id: id })
            .unwrap();
        assert_eq!(balance(&b.state, "wallet"), 300);
        assert_eq!(b.state.transactions[&id].status, TransactionStatus::Posted);
        // Posting twice is rejected.
        let err = b
            .push(LedgerEvent::PendingPosted { transaction_id: id })
            .unwrap_err();
        assert_eq!(err.code(), "invalid_transaction_state");
    }

    #[test]
    fn void_releases_pending() {
        let mut b = Builder::new();
        b.open("cash", AccountType::Asset);
        b.open("wallet", AccountType::Liability);
        let pending = tx(TransactionStatus::Pending, "wallet", "cash", 50);
        let id = pending.id;
        b.push(LedgerEvent::TransactionRecorded {
            transaction: pending,
        })
        .unwrap();
        b.push(LedgerEvent::PendingVoided {
            transaction_id: id,
            reason: Some("cancelled".into()),
            expired: false,
        })
        .unwrap();
        let cash = &b.state.accounts[&AccountId::new("cash").unwrap()];
        assert!(!cash.balances.has_pending());
        assert_eq!(
            b.state.transactions[&id].void_reason.as_deref(),
            Some("cancelled")
        );
    }

    #[test]
    fn reversal_updates_original_and_balances() {
        let mut b = Builder::new();
        b.open("cash", AccountType::Asset);
        b.open("revenue", AccountType::Revenue);
        let original = tx(TransactionStatus::Posted, "cash", "revenue", 500);
        let original_id = original.id;
        b.push(LedgerEvent::TransactionRecorded {
            transaction: original,
        })
        .unwrap();
        let mut reversal = tx(TransactionStatus::Posted, "revenue", "cash", 200);
        reversal.kind = TransactionKind::Reversal {
            original: original_id,
        };
        b.push(LedgerEvent::TransactionReversed {
            original_id,
            reversal,
            legs: vec![
                ReversedLeg {
                    posting_index: 0,
                    amount: 200,
                },
                ReversedLeg {
                    posting_index: 1,
                    amount: 200,
                },
            ],
        })
        .unwrap();
        assert_eq!(balance(&b.state, "cash"), 300);
        let orig = &b.state.transactions[&original_id];
        assert_eq!(orig.reversed_amounts, vec![200, 200]);
        assert_eq!(orig.reversals.len(), 1);

        // Over-reversal is rejected by the apply path itself.
        let mut too_much = tx(TransactionStatus::Posted, "revenue", "cash", 400);
        too_much.kind = TransactionKind::Reversal {
            original: original_id,
        };
        let err = b
            .push(LedgerEvent::TransactionReversed {
                original_id,
                reversal: too_much,
                legs: vec![ReversedLeg {
                    posting_index: 0,
                    amount: 400,
                }],
            })
            .unwrap_err();
        assert_eq!(err.code(), "invalid_reversal");
    }

    #[test]
    fn out_of_order_and_unknown_references_fail() {
        let mut b = Builder::new();
        let env = EventEnvelope {
            sequence: 5,
            timestamp: Utc::now(),
            event: LedgerEvent::PendingPosted {
                transaction_id: TransactionId::generate(),
            },
        };
        assert!(b.state.apply(&env).is_err());
        let err = b
            .push(LedgerEvent::AccountStatusChanged {
                account_id: AccountId::new("ghost").unwrap(),
                from: AccountStatus::Open,
                to: AccountStatus::Frozen,
                reason: None,
            })
            .unwrap_err();
        assert_eq!(err.code(), "account_not_found");
        let err = b
            .push(LedgerEvent::TransactionRecorded {
                transaction: tx(TransactionStatus::Voided, "a", "b", 1),
            })
            .unwrap_err();
        assert_eq!(err.code(), "validation_error");
    }

    #[test]
    fn account_attribute_events() {
        let mut b = Builder::new();
        b.open("cash", AccountType::Asset);
        let id = AccountId::new("cash").unwrap();
        let mut md = BTreeMap::new();
        md.insert("desk".to_string(), "emea".to_string());
        b.push(LedgerEvent::AccountMetadataUpdated {
            account_id: id.clone(),
            metadata: md.clone(),
        })
        .unwrap();
        b.push(LedgerEvent::AccountPolicyChanged {
            account_id: id.clone(),
            policy: BalancePolicy::NoOverdraft,
        })
        .unwrap();
        b.push(LedgerEvent::AccountStatusChanged {
            account_id: id.clone(),
            from: AccountStatus::Open,
            to: AccountStatus::Frozen,
            reason: None,
        })
        .unwrap();
        let a = &b.state.accounts[&id].account;
        assert_eq!(a.metadata, md);
        assert_eq!(a.policy, BalancePolicy::NoOverdraft);
        assert_eq!(a.status, AccountStatus::Frozen);
        assert_eq!(a.version, 4);
        // Duplicate account creation fails.
        let dup = b.state.accounts[&id].account.clone();
        assert!(b.push(LedgerEvent::AccountOpened { account: dup }).is_err());
    }

    #[test]
    fn idempotency_index_and_serde_roundtrip() {
        let mut b = Builder::new();
        b.open("cash", AccountType::Asset);
        b.open("equity", AccountType::Equity);
        let mut t = tx(TransactionStatus::Posted, "cash", "equity", 10);
        t.idempotency = Some(crate::domain::transaction::IdempotencyTag {
            key: "k1".into(),
            fingerprint: "fp".into(),
        });
        let id = t.id;
        b.push(LedgerEvent::TransactionRecorded { transaction: t })
            .unwrap();
        let idx = b.state.idempotency_index();
        assert_eq!(idx["k1"], (id, "fp".to_string()));
        let json = serde_json::to_vec(&b.state).unwrap();
        let back: LedgerState = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, b.state);
    }
}
