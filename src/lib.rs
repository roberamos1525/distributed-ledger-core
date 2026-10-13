//! `ledger_core`: a double-entry financial ledger with event sourcing.
//!
//! Layering (lower layers never depend on higher ones):
//!
//! * [`domain`] — money, accounts, transactions, events (pure types)
//! * [`store`] — checksummed framing, WAL, snapshots, recovery
//! * [`engine`] — command handling, locking, replay, queries, reports
//! * [`idempotency`], [`audit`], [`reconciliation`], [`metrics`]
//! * [`api`] — axum REST layer; [`app`] — service assembly; [`cli`] — ledgerctl

pub mod api;
pub mod app;
pub mod audit;
pub mod cli;
pub mod config;
pub mod domain;
pub mod engine;
pub mod error;
pub mod idempotency;
pub mod metrics;
pub mod reconciliation;
pub mod store;
pub mod telemetry;
