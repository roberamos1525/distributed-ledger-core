//! Identifier types and shared field validation rules.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::error::{LedgerError, LedgerResult};

/// Maximum length of an account identifier.
pub const MAX_ACCOUNT_ID_LEN: usize = 64;
/// Maximum number of metadata entries on an account or transaction.
pub const MAX_METADATA_ENTRIES: usize = 32;
/// Maximum length of a metadata key.
pub const MAX_METADATA_KEY_LEN: usize = 64;
/// Maximum length of a metadata value.
pub const MAX_METADATA_VALUE_LEN: usize = 512;
/// Maximum length of free-text descriptions.
pub const MAX_DESCRIPTION_LEN: usize = 1024;
/// Maximum length of an external reference.
pub const MAX_REFERENCE_LEN: usize = 128;

/// Identifier of a ledger account.
///
/// Account ids are human friendly (`assets:cash:usd`, `cust-42`) and limited to
/// ASCII alphanumerics plus `:`, `-`, `_` and `.`, which keeps them safe to use
/// in URLs, log lines and file names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    /// Validates and constructs an account id.
    pub fn new(raw: impl Into<String>) -> LedgerResult<Self> {
        let raw = raw.into();
        validate_account_id(&raw)?;
        Ok(AccountId(raw))
    }

    /// Generates a random account id with the `acc_` prefix.
    pub fn generate() -> Self {
        AccountId(format!("acc_{}", Uuid::new_v4().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_account_id(raw: &str) -> LedgerResult<()> {
    if raw.is_empty() {
        return Err(LedgerError::validation("account id must not be empty"));
    }
    if raw.len() > MAX_ACCOUNT_ID_LEN {
        return Err(LedgerError::validation(format!(
            "account id exceeds {MAX_ACCOUNT_ID_LEN} characters"
        )));
    }
    if let Some(bad) = raw
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.')))
    {
        return Err(LedgerError::validation(format!(
            "account id contains invalid character {bad:?}"
        )));
    }
    if raw.starts_with(['.', ':', '-']) {
        return Err(LedgerError::validation(
            "account id must start with a letter, digit or underscore",
        ));
    }
    Ok(())
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for AccountId {
    type Err = LedgerError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        AccountId::new(s)
    }
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        AccountId::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Identifier of a transaction. UUIDv7 so ids sort roughly by creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransactionId(Uuid);

impl TransactionId {
    pub fn generate() -> Self {
        TransactionId(Uuid::now_v7())
    }

    pub fn from_uuid(id: Uuid) -> Self {
        TransactionId(id)
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for TransactionId {
    type Err = LedgerError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s)
            .map(TransactionId)
            .map_err(|_| LedgerError::validation(format!("invalid transaction id {s:?}")))
    }
}

/// Validates a metadata map: bounded size, non-empty printable keys.
pub fn validate_metadata(metadata: &BTreeMap<String, String>) -> LedgerResult<()> {
    if metadata.len() > MAX_METADATA_ENTRIES {
        return Err(LedgerError::validation(format!(
            "metadata may contain at most {MAX_METADATA_ENTRIES} entries"
        )));
    }
    for (key, value) in metadata {
        if key.is_empty() || key.len() > MAX_METADATA_KEY_LEN {
            return Err(LedgerError::validation(format!(
                "metadata key {key:?} must be 1..={MAX_METADATA_KEY_LEN} characters"
            )));
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(LedgerError::validation(format!(
                "metadata key {key:?} may only contain ASCII alphanumerics, '_', '-' or '.'"
            )));
        }
        if value.len() > MAX_METADATA_VALUE_LEN {
            return Err(LedgerError::validation(format!(
                "metadata value for {key:?} exceeds {MAX_METADATA_VALUE_LEN} bytes"
            )));
        }
        if value.chars().any(char::is_control) {
            return Err(LedgerError::validation(format!(
                "metadata value for {key:?} contains control characters"
            )));
        }
    }
    Ok(())
}

/// Validates a bounded free-text field (descriptions, names, reasons).
pub fn validate_text(field: &str, value: &str, max: usize) -> LedgerResult<()> {
    if value.len() > max {
        return Err(LedgerError::validation(format!(
            "{field} exceeds {max} bytes"
        )));
    }
    if value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(LedgerError::validation(format!(
            "{field} contains control characters"
        )));
    }
    Ok(())
}

/// Validates an external reference (bank reference, invoice number, ...).
pub fn validate_reference(reference: &str) -> LedgerResult<()> {
    if reference.trim().is_empty() {
        return Err(LedgerError::validation("reference must not be blank"));
    }
    validate_text("reference", reference, MAX_REFERENCE_LEN)?;
    if reference.contains(['\n', '\t']) {
        return Err(LedgerError::validation("reference must be a single line"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_accepts_hierarchical_names() {
        for ok in ["assets:cash:usd", "cust-42", "a", "_internal", "x.y_z-1"] {
            assert!(AccountId::new(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn account_id_rejects_bad_input() {
        let too_long = "a".repeat(MAX_ACCOUNT_ID_LEN + 1);
        for bad in [
            "",
            "has space",
            "slash/no",
            ":leading",
            "-dash",
            "ünïcode",
            too_long.as_str(),
        ] {
            assert!(AccountId::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn account_id_deserialization_validates() {
        let ok: AccountId = serde_json::from_str("\"cash\"").unwrap();
        assert_eq!(ok.as_str(), "cash");
        assert!(serde_json::from_str::<AccountId>("\"bad id\"").is_err());
    }

    #[test]
    fn generated_ids_are_valid_and_unique() {
        let a = AccountId::generate();
        let b = AccountId::generate();
        assert_ne!(a, b);
        assert!(AccountId::new(a.as_str()).is_ok());
        let t1 = TransactionId::generate();
        let t2 = TransactionId::generate();
        assert_ne!(t1, t2);
        assert_eq!(t1.to_string().parse::<TransactionId>().unwrap(), t1);
        assert!("nope".parse::<TransactionId>().is_err());
    }

    #[test]
    fn metadata_limits() {
        let mut m = BTreeMap::new();
        m.insert("region".to_string(), "eu-west".to_string());
        assert!(validate_metadata(&m).is_ok());
        m.insert("bad key".to_string(), "v".to_string());
        assert!(validate_metadata(&m).is_err());
        let mut big = BTreeMap::new();
        for i in 0..=MAX_METADATA_ENTRIES {
            big.insert(format!("k{i}"), "v".to_string());
        }
        assert!(validate_metadata(&big).is_err());
        let mut long = BTreeMap::new();
        long.insert("k".to_string(), "v".repeat(MAX_METADATA_VALUE_LEN + 1));
        assert!(validate_metadata(&long).is_err());
        let mut ctrl = BTreeMap::new();
        ctrl.insert("k".to_string(), "a\u{0007}".to_string());
        assert!(validate_metadata(&ctrl).is_err());
    }

    #[test]
    fn text_and_reference_rules() {
        assert!(validate_text("description", "multi\nline ok", 100).is_ok());
        assert!(validate_text("description", "bell\u{0007}", 100).is_err());
        assert!(validate_text("description", "abcdef", 3).is_err());
        assert!(validate_reference("INV-2024-001").is_ok());
        assert!(validate_reference("   ").is_err());
        assert!(validate_reference("two\nlines").is_err());
    }
}
