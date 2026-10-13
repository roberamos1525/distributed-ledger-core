//! Request handlers grouped by resource.

pub mod accounts;
pub mod audit;
pub mod reconciliation;
pub mod reports;
pub mod system;
pub mod transactions;

use serde::Serialize;
use serde_json::Value;

use crate::api::error::ApiError;
use crate::domain::ids::{AccountId, TransactionId};

/// Serializes a response body.
pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value)
        .map_err(|e| ApiError::internal(format!("serialization failed: {e}")))
}

/// Parses an account id from a path segment.
pub(crate) fn parse_account_id(raw: &str) -> Result<AccountId, ApiError> {
    AccountId::new(raw).map_err(|e| ApiError::bad_request(e.to_string()))
}

/// Parses a transaction id from a path segment.
pub(crate) fn parse_tx_id(raw: &str) -> Result<TransactionId, ApiError> {
    raw.parse::<TransactionId>()
        .map_err(|e| ApiError::bad_request(e.to_string()))
}
