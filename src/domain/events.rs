//! Ledger events: the append-only source of truth.
//!
//! Every state change is expressed as exactly one event. Events carry all the
//! data needed to rebuild state deterministically on replay; they never refer
//! to wall-clock time other than the timestamp in their envelope.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::account::{Account, AccountStatus, BalancePolicy};
use crate::domain::ids::{AccountId, TransactionId};
use crate::domain::transaction::TransactionRecord;

/// One reversed leg of an original transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReversedLeg {
    pub posting_index: usize,
    pub amount: i64,
}

/// Domain events persisted in the write-ahead log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEvent {
    AccountOpened {
        account: Account,
    },
    AccountStatusChanged {
        account_id: AccountId,
        from: AccountStatus,
        to: AccountStatus,
        reason: Option<String>,
    },
    AccountMetadataUpdated {
        account_id: AccountId,
        metadata: BTreeMap<String, String>,
    },
    AccountPolicyChanged {
        account_id: AccountId,
        policy: BalancePolicy,
    },
    /// A new transaction, either directly posted or pending.
    TransactionRecorded {
        transaction: TransactionRecord,
    },
    /// Second phase of a two-phase transfer: pending becomes posted.
    PendingPosted {
        transaction_id: TransactionId,
    },
    /// Second phase alternative: pending amounts are released.
    PendingVoided {
        transaction_id: TransactionId,
        reason: Option<String>,
        expired: bool,
    },
    /// A reversal transaction linked to an original posted transaction.
    TransactionReversed {
        original_id: TransactionId,
        reversal: TransactionRecord,
        legs: Vec<ReversedLeg>,
    },
}

impl LedgerEvent {
    /// Short stable name used for metrics and logging.
    pub fn kind(&self) -> &'static str {
        match self {
            LedgerEvent::AccountOpened { .. } => "account_opened",
            LedgerEvent::AccountStatusChanged { .. } => "account_status_changed",
            LedgerEvent::AccountMetadataUpdated { .. } => "account_metadata_updated",
            LedgerEvent::AccountPolicyChanged { .. } => "account_policy_changed",
            LedgerEvent::TransactionRecorded { .. } => "transaction_recorded",
            LedgerEvent::PendingPosted { .. } => "pending_posted",
            LedgerEvent::PendingVoided { .. } => "pending_voided",
            LedgerEvent::TransactionReversed { .. } => "transaction_reversed",
        }
    }

    /// Transaction referenced by the event, if any.
    pub fn transaction_id(&self) -> Option<TransactionId> {
        match self {
            LedgerEvent::TransactionRecorded { transaction } => Some(transaction.id),
            LedgerEvent::PendingPosted { transaction_id }
            | LedgerEvent::PendingVoided { transaction_id, .. } => Some(*transaction_id),
            LedgerEvent::TransactionReversed { reversal, .. } => Some(reversal.id),
            _ => None,
        }
    }
}

/// An event with its position in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub event: LedgerEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_with_type_tag() {
        let id = TransactionId::generate();
        let env = EventEnvelope {
            sequence: 7,
            timestamp: Utc::now(),
            event: LedgerEvent::PendingVoided {
                transaction_id: id,
                reason: Some("customer cancelled".into()),
                expired: false,
            },
        };
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["event"]["type"], "pending_voided");
        assert_eq!(json["sequence"], 7);
        let back: EventEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back, env);
        assert_eq!(back.event.kind(), "pending_voided");
        assert_eq!(back.event.transaction_id(), Some(id));
    }

    #[test]
    fn account_events_have_no_transaction() {
        let ev = LedgerEvent::AccountPolicyChanged {
            account_id: AccountId::new("a").unwrap(),
            policy: BalancePolicy::Unrestricted,
        };
        assert_eq!(ev.transaction_id(), None);
        assert_eq!(ev.kind(), "account_policy_changed");
    }
}
