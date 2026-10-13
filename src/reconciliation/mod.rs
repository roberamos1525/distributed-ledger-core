//! Reconciliation of ledger activity against external statements.

pub mod matcher;
pub mod statement;

use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use serde::Serialize;

use crate::domain::ids::AccountId;
use crate::domain::money::CurrencyCode;
use crate::engine::ledger::Ledger;
use crate::engine::queries::AsOf;
use crate::error::{LedgerError, LedgerResult};

pub use matcher::{Discrepancy, LedgerLine, Match, MatchResult, Tolerance};
pub use statement::{Statement, StatementFormat, StatementLine};

/// Maximum width of a reconciliation period.
pub const MAX_PERIOD_DAYS: i64 = 366;

/// Parameters of a reconciliation run.
#[derive(Debug, Clone)]
pub struct ReconcileRequest {
    pub account_id: AccountId,
    pub format: StatementFormat,
    pub data: String,
    pub tolerance: Tolerance,
    /// Period start; defaults to the earliest statement date.
    pub from: Option<NaiveDate>,
    /// Period end; defaults to the latest statement date.
    pub to: Option<NaiveDate>,
    pub max_lines: usize,
}

/// Statement vs ledger closing balance.
#[derive(Debug, Clone, Serialize)]
pub struct BalanceCheck {
    pub statement_closing: i64,
    pub ledger_closing: i64,
    pub difference: i64,
    pub matches: bool,
}

/// Aggregate figures.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub statement_lines: usize,
    pub ledger_lines: usize,
    pub matched: usize,
    pub discrepancies: usize,
    pub unmatched_statement: usize,
    pub unmatched_ledger: usize,
    pub statement_total: i128,
    pub ledger_total: i128,
    pub difference: i128,
}

/// Full reconciliation report.
#[derive(Debug, Clone, Serialize)]
pub struct ReconciliationReport {
    pub account_id: AccountId,
    pub currency: CurrencyCode,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub tolerance: Tolerance,
    /// `reconciled` when everything matched and balances agree.
    pub status: &'static str,
    pub summary: Summary,
    pub balance_check: Option<BalanceCheck>,
    #[serde(flatten)]
    pub result: MatchResult,
}

/// Runs a reconciliation of one account against an external statement.
pub fn reconcile(ledger: &Ledger, req: &ReconcileRequest) -> LedgerResult<ReconciliationReport> {
    let view = ledger.account(&req.account_id)?;
    let currency = ledger
        .currencies()
        .get(view.account.currency)
        .ok_or_else(|| LedgerError::validation("account currency is not registered"))?;
    let statement = statement::parse(req.format, &req.data, &currency, req.max_lines)?;
    let from = req.from.or_else(|| statement.first_date()).ok_or_else(|| {
        LedgerError::validation("empty statement requires an explicit 'from' date")
    })?;
    let to = req
        .to
        .or_else(|| statement.last_date())
        .ok_or_else(|| LedgerError::validation("empty statement requires an explicit 'to' date"))?;
    if to < from {
        return Err(LedgerError::validation("'to' must not be before 'from'"));
    }
    if (to - from).num_days() > MAX_PERIOD_DAYS {
        return Err(LedgerError::validation(format!(
            "reconciliation period may span at most {MAX_PERIOD_DAYS} days"
        )));
    }
    if let Some(outside) = statement
        .lines
        .iter()
        .find(|l| l.date < from || l.date > to)
    {
        return Err(LedgerError::validation(format!(
            "statement line {} dated {} is outside the period {from}..{to}",
            outside.line, outside.date
        )));
    }

    // Ledger entries inside the period, widened by the date tolerance so
    // that items booked a day or two apart can still pair up.
    let slack = chrono::Duration::days(req.tolerance.date_days as i64);
    let start = Utc.from_utc_datetime(&(from - slack).and_time(NaiveTime::MIN));
    let end_of = |d: NaiveDate| {
        Utc.from_utc_datetime(
            &d.and_hms_milli_opt(23, 59, 59, 999)
                .expect("valid end-of-day time"),
        )
    };
    let (account, entries) =
        ledger.posted_entries_between(&req.account_id, start, end_of(to + slack))?;
    let mut ledger_lines = Vec::with_capacity(entries.len());
    for e in entries {
        let tx = ledger.transaction(&e.transaction_id)?;
        ledger_lines.push(LedgerLine {
            sequence: e.sequence,
            transaction_id: e.transaction_id,
            date: e.timestamp.date_naive(),
            amount: e.normal_amount(account.account_type),
            reference: tx.reference,
            description: tx.description,
        });
    }

    let mut result = matcher::match_lines(&statement.lines, &ledger_lines, req.tolerance);
    // Ledger lines that only fell inside the widened window are not
    // "unmatched" — they belong to neighbouring periods.
    result
        .unmatched_ledger
        .retain(|l| l.date >= from && l.date <= to);
    let ledger_in_period: Vec<&LedgerLine> = ledger_lines
        .iter()
        .filter(|l| l.date >= from && l.date <= to)
        .collect();

    let balance_check = statement.closing_balance.map(|statement_closing| {
        let ledger_closing = ledger
            .balance(&req.account_id, AsOf::Time(end_of(to)))
            .map(|b| b.posted)
            .unwrap_or(0);
        BalanceCheck {
            statement_closing,
            ledger_closing,
            difference: statement_closing.saturating_sub(ledger_closing),
            matches: statement_closing == ledger_closing,
        }
    });

    let statement_total = statement.total();
    let ledger_total: i128 = ledger_in_period.iter().map(|l| l.amount as i128).sum();
    let summary = Summary {
        statement_lines: statement.lines.len(),
        ledger_lines: ledger_in_period.len(),
        matched: result.matched.len(),
        discrepancies: result.discrepancies.len(),
        unmatched_statement: result.unmatched_statement.len(),
        unmatched_ledger: result.unmatched_ledger.len(),
        statement_total,
        ledger_total,
        difference: statement_total - ledger_total,
    };
    let clean = summary.discrepancies == 0
        && summary.unmatched_statement == 0
        && summary.unmatched_ledger == 0
        && balance_check.as_ref().is_none_or(|b| b.matches);
    Ok(ReconciliationReport {
        account_id: req.account_id.clone(),
        currency: account.currency,
        from,
        to,
        tolerance: req.tolerance,
        status: if clean { "reconciled" } else { "discrepancies" },
        summary,
        balance_check,
        result,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::AccountType;
    use crate::engine::commands::{CreateTransaction, OpenAccount, PostingSpec};
    use crate::engine::ledger::LedgerOptions;
    use crate::metrics::LedgerMetrics;
    use std::sync::Arc;

    fn setup() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(
            LedgerOptions::ephemeral(dir.path()),
            Arc::new(LedgerMetrics::new()),
        )
        .unwrap();
        ledger
            .open_account(OpenAccount::new("bank", AccountType::Asset, "USD").unwrap())
            .unwrap();
        ledger
            .open_account(OpenAccount::new("sales", AccountType::Revenue, "USD").unwrap())
            .unwrap();
        for (amount, reference) in [(10_000, "INV-1"), (2_500, "INV-2"), (700, "INV-3")] {
            let mut cmd = CreateTransaction::posted(vec![
                PostingSpec::debit("bank", amount).unwrap(),
                PostingSpec::credit("sales", amount).unwrap(),
            ]);
            cmd.reference = Some(reference.to_string());
            ledger.create_transaction(cmd).unwrap();
        }
        (dir, ledger)
    }

    fn request(data: String, format: StatementFormat) -> ReconcileRequest {
        ReconcileRequest {
            account_id: AccountId::new("bank").unwrap(),
            format,
            data,
            tolerance: Tolerance {
                date_days: 1,
                amount: 0,
            },
            from: None,
            to: None,
            max_lines: 1000,
        }
    }

    #[test]
    fn clean_reconciliation() {
        let (_dir, ledger) = setup();
        let today = Utc::now().date_naive();
        let csv = format!(
            "date,amount,reference\n{today},100.00,INV-1\n{today},25.00,INV-2\n{today},7.00,\n"
        );
        let report = reconcile(&ledger, &request(csv, StatementFormat::Csv)).unwrap();
        assert_eq!(report.status, "reconciled", "{report:?}");
        assert_eq!(report.summary.matched, 3);
        assert_eq!(report.summary.difference, 0);
    }

    #[test]
    fn reports_discrepancies_and_unmatched() {
        let (_dir, ledger) = setup();
        let today = Utc::now().date_naive();
        let json = format!(
            r#"{{"closing_balance":"200.00","lines":[
                {{"date":"{today}","amount":"100.00","reference":"INV-1"}},
                {{"date":"{today}","amount":"30.00","reference":"INV-2"}},
                {{"date":"{today}","amount":"1.23"}}]}}"#
        );
        let report = reconcile(&ledger, &request(json, StatementFormat::Json)).unwrap();
        assert_eq!(report.status, "discrepancies");
        assert_eq!(report.summary.matched, 1);
        assert_eq!(report.summary.discrepancies, 1);
        assert_eq!(report.result.discrepancies[0].amount_difference, 500);
        assert_eq!(report.summary.unmatched_statement, 1);
        assert_eq!(report.summary.unmatched_ledger, 1);
        let check = report.balance_check.unwrap();
        assert_eq!(check.ledger_closing, 13_200);
        assert!(!check.matches);
    }

    #[test]
    fn period_validation() {
        let (_dir, ledger) = setup();
        let mut req = request("date,amount\n".into(), StatementFormat::Csv);
        assert!(reconcile(&ledger, &req).is_err());
        req.from = NaiveDate::from_ymd_opt(2024, 1, 10);
        req.to = NaiveDate::from_ymd_opt(2024, 1, 1);
        assert!(reconcile(&ledger, &req).is_err());
        req.to = NaiveDate::from_ymd_opt(2026, 1, 1);
        assert!(reconcile(&ledger, &req).is_err());
        let mut outside = request(
            "date,amount\n2024-02-01,1.00\n".into(),
            StatementFormat::Csv,
        );
        outside.from = NaiveDate::from_ymd_opt(2024, 1, 1);
        outside.to = NaiveDate::from_ymd_opt(2024, 1, 31);
        assert!(reconcile(&ledger, &outside).is_err());
        let mut unknown = request("date,amount\n".into(), StatementFormat::Csv);
        unknown.account_id = AccountId::new("nope").unwrap();
        assert_eq!(
            reconcile(&ledger, &unknown).unwrap_err().code(),
            "account_not_found"
        );
    }

    #[test]
    fn empty_period_with_explicit_dates() {
        let (_dir, ledger) = setup();
        let mut req = request("date,amount\n".into(), StatementFormat::Csv);
        req.from = NaiveDate::from_ymd_opt(2020, 1, 1);
        req.to = NaiveDate::from_ymd_opt(2020, 1, 31);
        let report = reconcile(&ledger, &req).unwrap();
        assert_eq!(report.status, "reconciled");
        assert_eq!(report.summary.ledger_lines, 0);
    }
}
