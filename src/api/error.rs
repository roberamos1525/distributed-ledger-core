//! Consistent JSON error envelope for every API failure.
//!
//! ```json
//! {"error": {"code": "insufficient_funds", "message": "...", "details": {...}}}
//! ```

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

use crate::error::LedgerError;

/// An API error with HTTP status, stable code and human readable message.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

#[derive(Serialize)]
struct Body<'a> {
    error: Inner<'a>,
}

#[derive(Serialize)]
struct Inner<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code: code.to_string(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "validation_error", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }

    /// The JSON body of this error (also used for idempotent replays).
    pub fn body(&self) -> Value {
        serde_json::to_value(Body {
            error: Inner {
                code: &self.code,
                message: &self.message,
                details: self.details.as_ref(),
            },
        })
        .unwrap_or_else(|_| json!({"error": {"code": "internal_error", "message": "error serialization failed"}}))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(code = %self.code, message = %self.message, "request failed");
        }
        (self.status, Json(self.body())).into_response()
    }
}

/// Maps engine errors to HTTP statuses.
pub fn status_for(error: &LedgerError) -> StatusCode {
    use LedgerError::*;
    match error {
        Validation(_) => StatusCode::BAD_REQUEST,
        AccountNotFound(_) | TransactionNotFound(_) => StatusCode::NOT_FOUND,
        AccountExists(_)
        | VersionConflict { .. }
        | InvalidTransactionState { .. }
        | AlreadyReversed(_)
        | IdempotencyMismatch
        | RequestInProgress
        | Expired(_) => StatusCode::CONFLICT,
        AccountNotActive { .. }
        | InvalidStatusTransition { .. }
        | AccountNotEmpty { .. }
        | InsufficientFunds { .. }
        | Unbalanced { .. }
        | CurrencyMismatch { .. }
        | InvalidReversal { .. }
        | Overflow(_) => StatusCode::UNPROCESSABLE_ENTITY,
        Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ReadOnly(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

impl From<LedgerError> for ApiError {
    fn from(error: LedgerError) -> Self {
        let status = status_for(&error);
        // Never leak file paths or OS errors to clients.
        let message = match &error {
            LedgerError::Storage(_) => "internal storage error".to_string(),
            other => other.to_string(),
        };
        let details = match &error {
            LedgerError::InsufficientFunds {
                account,
                available,
                required,
                ..
            } => Some(json!({"account_id": account, "available": available, "required": required})),
            LedgerError::VersionConflict {
                account,
                expected,
                actual,
            } => Some(
                json!({"account_id": account, "expected_version": expected, "actual_version": actual}),
            ),
            LedgerError::Unbalanced {
                currency,
                debits,
                credits,
            } => Some(json!({"currency": currency, "debits": debits, "credits": credits})),
            _ => None,
        };
        if let LedgerError::Storage(e) = &error {
            tracing::error!(error = %e, "storage failure");
        }
        ApiError {
            status,
            code: error.code().to_string(),
            message,
            details,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::AccountId;
    use crate::error::StorageError;

    #[test]
    fn envelope_shape() {
        let e = ApiError::bad_request("bad").with_details(json!({"field": "x"}));
        let body = e.body();
        assert_eq!(body["error"]["code"], "validation_error");
        assert_eq!(body["error"]["message"], "bad");
        assert_eq!(body["error"]["details"]["field"], "x");
        let plain = ApiError::not_found("gone").body();
        assert!(plain["error"].get("details").is_none());
    }

    #[test]
    fn ledger_errors_map_to_statuses() {
        let id = AccountId::new("a").unwrap();
        let cases = [
            (LedgerError::validation("x"), 400),
            (LedgerError::AccountNotFound(id.clone()), 404),
            (LedgerError::AccountExists(id.clone()), 409),
            (LedgerError::IdempotencyMismatch, 409),
            (
                LedgerError::InsufficientFunds {
                    account: id.clone(),
                    available: 1,
                    required: 2,
                    floor: "0".into(),
                },
                422,
            ),
            (LedgerError::ReadOnly("x".into()), 503),
        ];
        for (err, status) in cases {
            assert_eq!(ApiError::from(err).status.as_u16(), status);
        }
    }

    #[test]
    fn storage_errors_are_sanitized() {
        let e = ApiError::from(LedgerError::Storage(StorageError::Corrupt {
            file: "C:/secret/path.wal".into(),
            offset: 1,
            reason: "x".into(),
        }));
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!e.message.contains("secret"));
    }

    #[test]
    fn insufficient_funds_has_details() {
        let e = ApiError::from(LedgerError::InsufficientFunds {
            account: AccountId::new("wallet").unwrap(),
            available: 10,
            required: 25,
            floor: "0".into(),
        });
        assert_eq!(e.details.unwrap()["required"], 25);
    }
}
