//! Pure domain model: money, accounts, transactions and events.
//!
//! Nothing in this module performs I/O or locking; it is shared by the live
//! engine, replay, snapshots and reporting.

pub mod account;
pub mod events;
pub mod ids;
pub mod money;
pub mod transaction;

pub use account::{
    Account, AccountState, AccountStatus, AccountType, BalancePolicy, Balances, Effect,
    EntryContext, EntryKind, HistoricalBalance, HistoryEntry,
};
pub use events::{EventEnvelope, LedgerEvent, ReversedLeg};
pub use ids::{AccountId, TransactionId};
pub use money::{Currency, CurrencyCode, CurrencyRegistry, Money};
pub use transaction::{
    Direction, IdempotencyTag, Posting, TransactionKind, TransactionRecord, TransactionStatus,
};
