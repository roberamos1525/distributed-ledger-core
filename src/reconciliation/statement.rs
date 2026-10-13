//! External statement import (CSV and JSON).
//!
//! Amounts in statements are signed decimals from the account holder's point
//! of view: positive values increase the account's normal balance (money in
//! for an asset account such as a bank account), negative values decrease it.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::domain::money::Currency;
use crate::error::{LedgerError, LedgerResult};

/// Supported statement formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatementFormat {
    Csv,
    Json,
}

/// One line of an external statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatementLine {
    /// 1-based line number (CSV: data row number; JSON: array position).
    pub line: usize,
    pub date: NaiveDate,
    pub amount: i64,
    pub reference: Option<String>,
    pub description: Option<String>,
}

/// A parsed statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Statement {
    pub lines: Vec<StatementLine>,
    pub opening_balance: Option<i64>,
    pub closing_balance: Option<i64>,
}

impl Statement {
    pub fn first_date(&self) -> Option<NaiveDate> {
        self.lines.iter().map(|l| l.date).min()
    }

    pub fn last_date(&self) -> Option<NaiveDate> {
        self.lines.iter().map(|l| l.date).max()
    }

    /// Sum of all line amounts.
    pub fn total(&self) -> i128 {
        self.lines.iter().map(|l| l.amount as i128).sum()
    }
}

fn parse_date(raw: &str, line: usize) -> LedgerResult<NaiveDate> {
    let raw = raw.trim();
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").map_err(|_| {
        LedgerError::validation(format!(
            "line {line}: invalid date {raw:?} (expected YYYY-MM-DD)"
        ))
    })
}

fn parse_amount(raw: &str, currency: &Currency, line: usize) -> LedgerResult<i64> {
    currency
        .parse_amount(raw)
        .map_err(|e| LedgerError::validation(format!("line {line}: {e}")))
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Parses a CSV statement with a header row.
///
/// Required columns: `date`, `amount`. Optional: `reference`, `description`.
/// Column names are case-insensitive and may appear in any order; unknown
/// columns are ignored.
pub fn parse_csv(data: &str, currency: &Currency, max_lines: usize) -> LedgerResult<Statement> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .flexible(false)
        .from_reader(data.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| LedgerError::validation(format!("invalid CSV header: {e}")))?
        .clone();
    let find = |name: &str| {
        headers
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
    };
    let date_col =
        find("date").ok_or_else(|| LedgerError::validation("CSV is missing a 'date' column"))?;
    let amount_col = find("amount")
        .ok_or_else(|| LedgerError::validation("CSV is missing an 'amount' column"))?;
    let reference_col = find("reference");
    let description_col = find("description");

    let mut lines = Vec::new();
    for (i, row) in reader.records().enumerate() {
        let line = i + 1;
        if line > max_lines {
            return Err(LedgerError::validation(format!(
                "statement exceeds the limit of {max_lines} lines"
            )));
        }
        let row = row.map_err(|e| LedgerError::validation(format!("line {line}: {e}")))?;
        let field = |col: usize| row.get(col).unwrap_or("");
        lines.push(StatementLine {
            line,
            date: parse_date(field(date_col), line)?,
            amount: parse_amount(field(amount_col), currency, line)?,
            reference: non_empty(reference_col.map(field)),
            description: non_empty(description_col.map(field)),
        });
    }
    Ok(Statement {
        lines,
        opening_balance: None,
        closing_balance: None,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonStatement {
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    opening_balance: Option<String>,
    #[serde(default)]
    closing_balance: Option<String>,
    lines: Vec<JsonLine>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonLine {
    date: String,
    #[serde(default)]
    amount: Option<String>,
    #[serde(default)]
    amount_minor: Option<i64>,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Parses a JSON statement:
///
/// ```json
/// {"currency": "USD", "closing_balance": "120.00",
///  "lines": [{"date": "2024-03-01", "amount": "100.00", "reference": "INV-1"}]}
/// ```
///
/// Each line carries either `amount` (decimal string) or `amount_minor`
/// (integer minor units), not both.
pub fn parse_json(data: &str, currency: &Currency, max_lines: usize) -> LedgerResult<Statement> {
    let parsed: JsonStatement = serde_json::from_str(data)
        .map_err(|e| LedgerError::validation(format!("invalid JSON statement: {e}")))?;
    if let Some(code) = &parsed.currency
        && code != currency.code.as_str()
    {
        return Err(LedgerError::validation(format!(
            "statement currency {code} does not match account currency {}",
            currency.code
        )));
    }
    if parsed.lines.len() > max_lines {
        return Err(LedgerError::validation(format!(
            "statement exceeds the limit of {max_lines} lines"
        )));
    }
    let mut lines = Vec::with_capacity(parsed.lines.len());
    for (i, l) in parsed.lines.into_iter().enumerate() {
        let line = i + 1;
        let amount = match (l.amount, l.amount_minor) {
            (Some(text), None) => parse_amount(&text, currency, line)?,
            (None, Some(minor)) => minor,
            (Some(_), Some(_)) => {
                return Err(LedgerError::validation(format!(
                    "line {line}: specify either amount or amount_minor, not both"
                )));
            }
            (None, None) => {
                return Err(LedgerError::validation(format!(
                    "line {line}: amount is required"
                )));
            }
        };
        lines.push(StatementLine {
            line,
            date: parse_date(&l.date, line)?,
            amount,
            reference: non_empty(l.reference.as_deref()),
            description: non_empty(l.description.as_deref()),
        });
    }
    let balance = |raw: Option<String>| {
        raw.map(|r| {
            currency
                .parse_amount(&r)
                .map_err(|e| LedgerError::validation(format!("balance: {e}")))
        })
        .transpose()
    };
    Ok(Statement {
        lines,
        opening_balance: balance(parsed.opening_balance)?,
        closing_balance: balance(parsed.closing_balance)?,
    })
}

/// Parses a statement in the given format.
pub fn parse(
    format: StatementFormat,
    data: &str,
    currency: &Currency,
    max_lines: usize,
) -> LedgerResult<Statement> {
    match format {
        StatementFormat::Csv => parse_csv(data, currency, max_lines),
        StatementFormat::Json => parse_json(data, currency, max_lines),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd() -> Currency {
        Currency::new("USD", 2).unwrap()
    }

    #[test]
    fn csv_with_reordered_columns() {
        let data = "Reference,Amount,Date,Description,Extra\n\
                    INV-1, 100.00 ,2024-03-01,Invoice 1,x\n\
                    ,-5.5,2024-03-02,,y\n";
        let st = parse_csv(data, &usd(), 100).unwrap();
        assert_eq!(st.lines.len(), 2);
        assert_eq!(st.lines[0].amount, 10_000);
        assert_eq!(st.lines[0].reference.as_deref(), Some("INV-1"));
        assert_eq!(st.lines[1].amount, -550);
        assert_eq!(st.lines[1].reference, None);
        assert_eq!(st.total(), 9_450);
        assert_eq!(st.first_date(), NaiveDate::from_ymd_opt(2024, 3, 1));
        assert_eq!(st.last_date(), NaiveDate::from_ymd_opt(2024, 3, 2));
    }

    #[test]
    fn csv_errors_carry_line_numbers() {
        let missing = "date,reference\n2024-01-01,x\n";
        assert!(
            parse_csv(missing, &usd(), 10)
                .unwrap_err()
                .to_string()
                .contains("amount")
        );
        let bad_date = "date,amount\n2024-13-01,1.00\n";
        let err = parse_csv(bad_date, &usd(), 10).unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
        let bad_amount = "date,amount\n2024-01-01,1.00\n2024-01-02,1.001\n";
        let err = parse_csv(bad_amount, &usd(), 10).unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        let ragged = "date,amount\n2024-01-01,1.00,extra\n";
        assert!(parse_csv(ragged, &usd(), 10).is_err());
        let many = "date,amount\n2024-01-01,1\n2024-01-01,1\n2024-01-01,1\n";
        assert!(parse_csv(many, &usd(), 2).is_err());
    }

    #[test]
    fn json_statement() {
        let data = r#"{"currency":"USD","closing_balance":"95.00","lines":[
            {"date":"2024-03-01","amount":"100.00","reference":"A"},
            {"date":"2024-03-02","amount_minor":-500,"description":"fee"}]}"#;
        let st = parse_json(data, &usd(), 100).unwrap();
        assert_eq!(st.lines[0].amount, 10_000);
        assert_eq!(st.lines[1].amount, -500);
        assert_eq!(st.closing_balance, Some(9_500));
        assert_eq!(st.opening_balance, None);
    }

    #[test]
    fn json_validation() {
        let wrong_currency = r#"{"currency":"EUR","lines":[]}"#;
        assert!(parse_json(wrong_currency, &usd(), 10).is_err());
        let both = r#"{"lines":[{"date":"2024-01-01","amount":"1","amount_minor":100}]}"#;
        assert!(parse_json(both, &usd(), 10).is_err());
        let neither = r#"{"lines":[{"date":"2024-01-01"}]}"#;
        assert!(parse_json(neither, &usd(), 10).is_err());
        let unknown = r#"{"lines":[],"surprise":1}"#;
        assert!(parse_json(unknown, &usd(), 10).is_err());
        let too_many =
            r#"{"lines":[{"date":"2024-01-01","amount":"1"},{"date":"2024-01-01","amount":"1"}]}"#;
        assert!(parse_json(too_many, &usd(), 1).is_err());
        assert!(parse(StatementFormat::Json, "not json", &usd(), 1).is_err());
    }
}
