//! Typed errors for the ledger engine and its storage layer.
//!
//! Every error carries a stable machine-readable `code()` that the REST layer
//! maps to an HTTP status and puts into the JSON error envelope.

use std::io;

use crate::domain::account::AccountStatus;
use crate::domain::ids::{AccountId, TransactionId};
use crate::domain::transaction::TransactionStatus;

/// Result alias used throughout the engine.
pub type LedgerResult<T> = Result<T, LedgerError>;

/// Errors produced by command validation, state transitions and persistence.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("validation failed: {0}")]
    Validation(String),

    #[error("account {0} not found")]
    AccountNotFound(AccountId),

    #[error("account {0} already exists")]
    AccountExists(AccountId),

    #[error("transaction {0} not found")]
    TransactionNotFound(TransactionId),

    #[error("account {account} is {status}; operation not permitted")]
    AccountNotActive {
        account: AccountId,
        status: AccountStatus,
    },

    #[error("invalid account status transition for {account}: {from} -> {to}")]
    InvalidStatusTransition {
        account: AccountId,
        from: AccountStatus,
        to: AccountStatus,
    },

    #[error("account {account} cannot be closed: {reason}")]
    AccountNotEmpty { account: AccountId, reason: String },

    #[error(
        "insufficient funds in {account}: available {available}, required {required}, floor {floor}"
    )]
    InsufficientFunds {
        account: AccountId,
        available: i64,
        required: i64,
        floor: String,
    },

    #[error(
        "postings do not balance for currency {currency}: debits {debits} != credits {credits}"
    )]
    Unbalanced {
        currency: String,
        debits: i64,
        credits: i64,
    },

    #[error("currency mismatch on {account}: account is {expected}, posting is {actual}")]
    CurrencyMismatch {
        account: AccountId,
        expected: String,
        actual: String,
    },

    #[error("version conflict on {account}: expected {expected}, actual {actual}")]
    VersionConflict {
        account: AccountId,
        expected: u64,
        actual: u64,
    },

    #[error("transaction {id} is {status}; expected {expected}")]
    InvalidTransactionState {
        id: TransactionId,
        status: TransactionStatus,
        expected: TransactionStatus,
    },

    #[error("transaction {0} has already been fully reversed")]
    AlreadyReversed(TransactionId),

    #[error("reversal rejected for {id}: {reason}")]
    InvalidReversal { id: TransactionId, reason: String },

    #[error("idempotency key reused with a different request payload")]
    IdempotencyMismatch,

    #[error("a request with this idempotency key is still in progress")]
    RequestInProgress,

    #[error("pending transaction {0} has expired")]
    Expired(TransactionId),

    #[error("arithmetic overflow while computing {0}")]
    Overflow(&'static str),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("ledger is read-only: {0}")]
    ReadOnly(String),
}

impl LedgerError {
    /// Stable error code used in API envelopes and logs.
    pub fn code(&self) -> &'static str {
        match self {
            LedgerError::Validation(_) => "validation_error",
            LedgerError::AccountNotFound(_) => "account_not_found",
            LedgerError::AccountExists(_) => "account_exists",
            LedgerError::TransactionNotFound(_) => "transaction_not_found",
            LedgerError::AccountNotActive { .. } => "account_not_active",
            LedgerError::InvalidStatusTransition { .. } => "invalid_status_transition",
            LedgerError::AccountNotEmpty { .. } => "account_not_empty",
            LedgerError::InsufficientFunds { .. } => "insufficient_funds",
            LedgerError::Unbalanced { .. } => "unbalanced_transaction",
            LedgerError::CurrencyMismatch { .. } => "currency_mismatch",
            LedgerError::VersionConflict { .. } => "version_conflict",
            LedgerError::InvalidTransactionState { .. } => "invalid_transaction_state",
            LedgerError::AlreadyReversed(_) => "already_reversed",
            LedgerError::InvalidReversal { .. } => "invalid_reversal",
            LedgerError::IdempotencyMismatch => "idempotency_key_reused",
            LedgerError::RequestInProgress => "request_in_progress",
            LedgerError::Expired(_) => "transaction_expired",
            LedgerError::Overflow(_) => "arithmetic_overflow",
            LedgerError::Storage(_) => "storage_error",
            LedgerError::ReadOnly(_) => "read_only",
        }
    }

    /// Convenience constructor for validation failures.
    pub fn validation(msg: impl Into<String>) -> Self {
        LedgerError::Validation(msg.into())
    }
}

/// Errors raised by the WAL, snapshot and recovery machinery.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("i/o error during {context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("corrupt record in {file} at offset {offset}: {reason}")]
    Corrupt {
        file: String,
        offset: u64,
        reason: String,
    },

    #[error("sequence gap: expected {expected}, found {found} in {file}")]
    SequenceGap {
        expected: u64,
        found: u64,
        file: String,
    },

    #[error("data directory {0} is locked by another process")]
    Locked(String),

    #[error("write-ahead log is unavailable after a failed write: {0}")]
    Poisoned(String),

    #[error("replay failed at sequence {sequence}: {reason}")]
    Replay { sequence: u64, reason: String },

    #[error("no snapshot available: {0}")]
    NoSnapshot(String),
}

impl StorageError {
    /// Wraps an `io::Error` with a human readable context.
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        StorageError::Io {
            context: context.into(),
            source,
        }
    }
}

impl From<serde_json::Error> for StorageError {
    fn from(e: serde_json::Error) -> Self {
        StorageError::Serde(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(LedgerError::validation("x").code(), "validation_error");
        assert_eq!(
            LedgerError::IdempotencyMismatch.code(),
            "idempotency_key_reused"
        );
        let storage = LedgerError::from(StorageError::NoSnapshot("none".into()));
        assert_eq!(storage.code(), "storage_error");
    }

    #[test]
    fn io_errors_carry_context() {
        let e = StorageError::io("open wal", io::Error::other("boom"));
        assert!(e.to_string().contains("open wal"));
        assert!(e.to_string().contains("boom"));
    }
}
