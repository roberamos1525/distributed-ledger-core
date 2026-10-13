//! Request and response bodies of the REST API, plus their conversion into
//! engine commands.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::api::error::ApiError;
use crate::domain::account::{AccountStatus, AccountType, BalancePolicy};
use crate::domain::events::ReversedLeg;
use crate::domain::ids::{AccountId, TransactionId};
use crate::domain::money::{Currency, CurrencyCode};
use crate::domain::transaction::{Direction, TransactionStatus};
use crate::engine::commands::{
    CreateTransaction, OpenAccount, PostingMode, PostingSpec, ReverseTransaction,
};
use crate::engine::queries::{AccountFilter, HistoryOrder, TransactionFilter};
use crate::reconciliation::StatementFormat;

/// Default page size for list endpoints.
pub const DEFAULT_LIMIT: usize = 50;

/// An amount given either as integer minor units (`1050`) or as a decimal
/// string in the account's currency (`"10.50"`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum AmountInput {
    Minor(i64),
    Decimal(String),
}

impl AmountInput {
    pub fn resolve(&self, currency: &Currency) -> Result<i64, ApiError> {
        match self {
            AmountInput::Minor(v) => Ok(*v),
            AmountInput::Decimal(s) => currency
                .parse_amount(s)
                .map_err(|e| ApiError::bad_request(e.to_string())),
        }
    }
}

/// `POST /v1/accounts`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAccountRequest {
    #[serde(default)]
    pub id: Option<AccountId>,
    pub name: String,
    #[serde(rename = "type")]
    pub account_type: AccountType,
    pub currency: String,
    #[serde(default)]
    pub policy: Option<BalancePolicy>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl From<CreateAccountRequest> for OpenAccount {
    fn from(r: CreateAccountRequest) -> Self {
        OpenAccount {
            id: r.id,
            name: r.name,
            account_type: Some(r.account_type),
            currency: r.currency,
            policy: r.policy,
            metadata: r.metadata,
        }
    }
}

/// `POST /v1/accounts/{id}/status`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateStatusRequest {
    pub status: AccountStatus,
    #[serde(default)]
    pub expected_version: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `PATCH /v1/accounts/{id}/metadata`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMetadataRequest {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
    #[serde(default)]
    pub expected_version: Option<u64>,
}

/// `PUT /v1/accounts/{id}/policy`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePolicyRequest {
    pub policy: BalancePolicy,
    #[serde(default)]
    pub expected_version: Option<u64>,
}

/// One requested posting.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostingRequest {
    pub account_id: AccountId,
    pub direction: Direction,
    pub amount: AmountInput,
    #[serde(default)]
    pub currency: Option<CurrencyCode>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModeRequest {
    #[default]
    Posted,
    Pending,
}

/// `POST /v1/transactions`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTransactionRequest {
    pub postings: Vec<PostingRequest>,
    #[serde(default)]
    pub mode: ModeRequest,
    #[serde(default)]
    pub pending_ttl_secs: Option<u64>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub expected_versions: BTreeMap<AccountId, u64>,
}

impl CreateTransactionRequest {
    /// Converts into an engine command; `currency_of` resolves an account's
    /// currency for decimal amounts.
    pub fn into_command(
        self,
        currency_of: impl Fn(&AccountId) -> Result<Currency, ApiError>,
    ) -> Result<CreateTransaction, ApiError> {
        if self.mode == ModeRequest::Posted && self.pending_ttl_secs.is_some() {
            return Err(ApiError::bad_request(
                "pending_ttl_secs is only valid with mode=pending",
            ));
        }
        let mut postings = Vec::with_capacity(self.postings.len());
        for p in self.postings {
            let amount = match &p.amount {
                AmountInput::Minor(v) => *v,
                decimal => decimal.resolve(&currency_of(&p.account_id)?)?,
            };
            postings.push(PostingSpec {
                account_id: p.account_id,
                direction: p.direction,
                amount,
                currency: p.currency,
            });
        }
        Ok(CreateTransaction {
            postings,
            mode: match self.mode {
                ModeRequest::Posted => PostingMode::Posted,
                ModeRequest::Pending => PostingMode::Pending {
                    ttl: self.pending_ttl_secs.map(Duration::from_secs),
                },
            },
            description: self.description,
            reference: self.reference,
            metadata: self.metadata,
            expected_versions: self.expected_versions,
            idempotency: None,
        })
    }
}

/// `POST /v1/transactions/{id}/void`
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoidRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

/// One leg of a partial reversal.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReversalLegRequest {
    pub posting_index: usize,
    pub amount: AmountInput,
}

/// `POST /v1/transactions/{id}/reversals`
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseRequest {
    #[serde(default)]
    pub legs: Option<Vec<ReversalLegRequest>>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl ReverseRequest {
    /// Converts into an engine command; `currency_of_leg` resolves the
    /// currency of the original posting at an index.
    pub fn into_command(
        self,
        original: TransactionId,
        currency_of_leg: impl Fn(usize) -> Result<Currency, ApiError>,
    ) -> Result<ReverseTransaction, ApiError> {
        let legs = match self.legs {
            None => None,
            Some(legs) => Some(
                legs.into_iter()
                    .map(|l| {
                        let amount = match &l.amount {
                            AmountInput::Minor(v) => *v,
                            decimal => decimal.resolve(&currency_of_leg(l.posting_index)?)?,
                        };
                        Ok(ReversedLeg {
                            posting_index: l.posting_index,
                            amount,
                        })
                    })
                    .collect::<Result<Vec<_>, ApiError>>()?,
            ),
        };
        Ok(ReverseTransaction {
            original,
            legs,
            description: self.description,
            reference: self.reference,
            metadata: self.metadata,
        })
    }
}

/// `POST /v1/reconciliations`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileRequestBody {
    pub account_id: AccountId,
    pub format: StatementFormat,
    /// Statement content (CSV text or a JSON document as a string).
    pub data: String,
    #[serde(default)]
    pub date_tolerance_days: Option<u32>,
    #[serde(default)]
    pub amount_tolerance: Option<AmountInput>,
    #[serde(default)]
    pub from: Option<NaiveDate>,
    #[serde(default)]
    pub to: Option<NaiveDate>,
}

/// Query of `GET /v1/accounts`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListAccountsQuery {
    #[serde(default, rename = "type")]
    pub account_type: Option<AccountType>,
    #[serde(default)]
    pub status: Option<AccountStatus>,
    #[serde(default)]
    pub currency: Option<CurrencyCode>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

impl ListAccountsQuery {
    pub fn filter(&self) -> AccountFilter {
        AccountFilter {
            account_type: self.account_type,
            status: self.status,
            currency: self.currency,
        }
    }
}

/// Query of `GET /v1/transactions`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListTransactionsQuery {
    #[serde(default)]
    pub status: Option<TransactionStatus>,
    #[serde(default)]
    pub account_id: Option<AccountId>,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

impl ListTransactionsQuery {
    pub fn filter(&self) -> TransactionFilter {
        TransactionFilter {
            status: self.status,
            account_id: self.account_id.clone(),
            reference: self.reference.clone(),
        }
    }
}

/// Query of `GET /v1/accounts/{id}/balance`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BalanceQuery {
    #[serde(default)]
    pub as_of_sequence: Option<u64>,
    #[serde(default)]
    pub as_of_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderParam {
    #[default]
    Asc,
    Desc,
}

/// Query of `GET /v1/accounts/{id}/history`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryQuery {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub order: OrderParam,
}

impl HistoryQuery {
    pub fn order(&self) -> HistoryOrder {
        match self.order {
            OrderParam::Asc => HistoryOrder::Ascending,
            OrderParam::Desc => HistoryOrder::Descending,
        }
    }
}

/// Query of `GET /v1/audit`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    #[serde(default)]
    pub after: Option<u64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Envelope for paginated lists.
#[derive(Debug, Serialize)]
pub struct ListResponse<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
}

/// Encodes an opaque cursor (`kind:value`, hex encoded).
pub fn encode_cursor(kind: &str, value: &str) -> String {
    hex::encode(format!("{kind}:{value}"))
}

/// Decodes a cursor of the expected kind.
pub fn decode_cursor(kind: &str, cursor: &str) -> Result<String, ApiError> {
    let invalid = || ApiError::bad_request("invalid cursor");
    let bytes = hex::decode(cursor).map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    let (found_kind, value) = text.split_once(':').ok_or_else(invalid)?;
    if found_kind != kind {
        return Err(invalid());
    }
    Ok(value.to_string())
}

/// Decodes a numeric cursor.
pub fn decode_numeric_cursor<T: std::str::FromStr>(
    kind: &str,
    cursor: &str,
) -> Result<T, ApiError> {
    decode_cursor(kind, cursor)?
        .parse()
        .map_err(|_| ApiError::bad_request("invalid cursor"))
}

/// Validates a page size.
pub fn page_limit(limit: Option<usize>) -> Result<usize, ApiError> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(0) => Err(ApiError::bad_request("limit must be at least 1")),
        Some(l) if l > crate::engine::queries::MAX_PAGE_SIZE => {
            Err(ApiError::bad_request(format!(
                "limit must be at most {}",
                crate::engine::queries::MAX_PAGE_SIZE
            )))
        }
        Some(l) => Ok(l),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usd() -> Currency {
        Currency::new("USD", 2).unwrap()
    }

    #[test]
    fn amount_input_forms() {
        let minor: AmountInput = serde_json::from_value(json!(1050)).unwrap();
        let decimal: AmountInput = serde_json::from_value(json!("10.50")).unwrap();
        assert_eq!(minor.resolve(&usd()).unwrap(), 1050);
        assert_eq!(decimal.resolve(&usd()).unwrap(), 1050);
        let bad: AmountInput = serde_json::from_value(json!("10.505")).unwrap();
        assert!(bad.resolve(&usd()).is_err());
        assert!(serde_json::from_value::<AmountInput>(json!(10.5)).is_err());
    }

    #[test]
    fn transaction_request_conversion() {
        let req: CreateTransactionRequest = serde_json::from_value(json!({
            "postings": [
                {"account_id": "cash", "direction": "debit", "amount": "12.34"},
                {"account_id": "equity", "direction": "credit", "amount": 1234, "currency": "USD"}
            ],
            "mode": "pending",
            "pending_ttl_secs": 60,
            "reference": "R1"
        }))
        .unwrap();
        let cmd = req.into_command(|_| Ok(usd())).unwrap();
        assert_eq!(cmd.postings[0].amount, 1234);
        assert_eq!(cmd.postings[1].currency.unwrap().as_str(), "USD");
        assert_eq!(
            cmd.mode,
            PostingMode::Pending {
                ttl: Some(Duration::from_secs(60))
            }
        );
    }

    #[test]
    fn transaction_request_rejections() {
        let ttl_without_pending: CreateTransactionRequest = serde_json::from_value(json!({
            "postings": [], "pending_ttl_secs": 5
        }))
        .unwrap();
        assert!(ttl_without_pending.into_command(|_| Ok(usd())).is_err());
        assert!(
            serde_json::from_value::<CreateTransactionRequest>(json!({
                "postings": [], "unknown": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CreateTransactionRequest>(json!({
                "postings": [{"account_id": "bad id", "direction": "debit", "amount": 1}]
            }))
            .is_err()
        );
    }

    #[test]
    fn reverse_request_conversion() {
        let req: ReverseRequest = serde_json::from_value(json!({
            "legs": [{"posting_index": 0, "amount": "1.00"}, {"posting_index": 1, "amount": 100}]
        }))
        .unwrap();
        let id = TransactionId::generate();
        let cmd = req.into_command(id, |_| Ok(usd())).unwrap();
        let legs = cmd.legs.unwrap();
        assert_eq!(legs[0].amount, 100);
        assert_eq!(legs[1].amount, 100);
        let full = ReverseRequest::default()
            .into_command(id, |_| Ok(usd()))
            .unwrap();
        assert!(full.legs.is_none());
    }

    #[test]
    fn cursors_roundtrip_and_validate_kind() {
        let c = encode_cursor("acct", "assets:cash");
        assert_eq!(decode_cursor("acct", &c).unwrap(), "assets:cash");
        assert!(decode_cursor("tx", &c).is_err());
        assert!(decode_cursor("acct", "zz").is_err());
        let n = encode_cursor("tx", "42");
        assert_eq!(decode_numeric_cursor::<u64>("tx", &n).unwrap(), 42);
        assert!(decode_numeric_cursor::<u64>("tx", &encode_cursor("tx", "x")).is_err());
    }

    #[test]
    fn page_limits() {
        assert_eq!(page_limit(None).unwrap(), DEFAULT_LIMIT);
        assert_eq!(page_limit(Some(10)).unwrap(), 10);
        assert!(page_limit(Some(0)).is_err());
        assert!(page_limit(Some(10_000)).is_err());
    }

    #[test]
    fn account_request_shape() {
        let req: CreateAccountRequest = serde_json::from_value(json!({
            "id": "cust-1", "name": "Customer 1", "type": "liability", "currency": "EUR",
            "policy": {"kind": "overdraft", "limit": 1000}, "metadata": {"segment": "retail"}
        }))
        .unwrap();
        let cmd: OpenAccount = req.into();
        assert_eq!(cmd.account_type, Some(AccountType::Liability));
        assert_eq!(cmd.policy, Some(BalancePolicy::Overdraft { limit: 1000 }));
        assert!(
            serde_json::from_value::<CreateAccountRequest>(json!({
                "name": "x", "type": "cash", "currency": "USD"
            }))
            .is_err()
        );
    }
}
