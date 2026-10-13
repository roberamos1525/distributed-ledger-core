//! Matching of statement lines against ledger lines.
//!
//! Two passes, both deterministic:
//!
//! 1. **Reference pass** – a statement line with a reference is paired with an
//!    unmatched ledger line carrying the same reference (or transaction id).
//!    If amount and date are within tolerance the pair is *matched*; otherwise
//!    it is reported as a *discrepancy* (same item, different values).
//! 2. **Amount/date pass** – remaining statement lines are paired with the
//!    closest remaining ledger line whose amount is within the amount
//!    tolerance and whose date is within the date window (closest date first,
//!    then closest amount, then lowest sequence).
//!
//! Whatever is left is reported as unmatched on either side.

use chrono::NaiveDate;
use serde::Serialize;

use crate::domain::ids::TransactionId;
use crate::reconciliation::statement::StatementLine;

/// A ledger-side line for reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerLine {
    pub sequence: u64,
    pub transaction_id: TransactionId,
    pub date: NaiveDate,
    /// Signed amount on the account's normal side.
    pub amount: i64,
    pub reference: Option<String>,
    pub description: String,
}

impl LedgerLine {
    fn has_reference(&self, reference: &str) -> bool {
        self.reference.as_deref() == Some(reference) || self.transaction_id.to_string() == reference
    }
}

/// Matching tolerances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Tolerance {
    /// Maximum absolute difference in days.
    pub date_days: u32,
    /// Maximum absolute amount difference in minor units.
    pub amount: i64,
}

/// How a pair was matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMethod {
    Reference,
    AmountDate,
}

/// A matched pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Match {
    pub method: MatchMethod,
    pub statement: StatementLine,
    pub ledger: LedgerLine,
    /// statement amount - ledger amount
    pub amount_difference: i64,
    /// statement date - ledger date, in days
    pub date_difference_days: i64,
}

/// Why a reference-matched pair is a discrepancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscrepancyKind {
    AmountMismatch,
    DateOutOfWindow,
    AmountAndDateMismatch,
}

/// Same reference on both sides but values outside tolerance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Discrepancy {
    pub kind: DiscrepancyKind,
    pub statement: StatementLine,
    pub ledger: LedgerLine,
    pub amount_difference: i64,
    pub date_difference_days: i64,
}

/// Matching output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MatchResult {
    pub matched: Vec<Match>,
    pub discrepancies: Vec<Discrepancy>,
    pub unmatched_statement: Vec<StatementLine>,
    pub unmatched_ledger: Vec<LedgerLine>,
}

fn diffs(statement: &StatementLine, ledger: &LedgerLine) -> (i64, i64) {
    (
        statement.amount.saturating_sub(ledger.amount),
        (statement.date - ledger.date).num_days(),
    )
}

/// Runs both matching passes.
pub fn match_lines(
    statement: &[StatementLine],
    ledger: &[LedgerLine],
    tol: Tolerance,
) -> MatchResult {
    let mut ledger_used = vec![false; ledger.len()];
    let mut statement_used = vec![false; statement.len()];
    let mut result = MatchResult::default();
    let within = |amount_diff: i64, date_diff: i64| {
        (
            amount_diff.unsigned_abs() <= tol.amount.unsigned_abs(),
            date_diff.unsigned_abs() <= tol.date_days as u64,
        )
    };

    // Pass 1: by reference.
    for (si, line) in statement.iter().enumerate() {
        let Some(reference) = line.reference.as_deref() else {
            continue;
        };
        let best = ledger
            .iter()
            .enumerate()
            .filter(|(li, l)| !ledger_used[*li] && l.has_reference(reference))
            .min_by_key(|(_, l)| {
                let (a, d) = diffs(line, l);
                (a.unsigned_abs(), d.unsigned_abs(), l.sequence)
            });
        let Some((li, candidate)) = best else {
            continue;
        };
        ledger_used[li] = true;
        statement_used[si] = true;
        let (amount_difference, date_difference_days) = diffs(line, candidate);
        match within(amount_difference, date_difference_days) {
            (true, true) => result.matched.push(Match {
                method: MatchMethod::Reference,
                statement: line.clone(),
                ledger: candidate.clone(),
                amount_difference,
                date_difference_days,
            }),
            (amount_ok, date_ok) => result.discrepancies.push(Discrepancy {
                kind: match (amount_ok, date_ok) {
                    (false, true) => DiscrepancyKind::AmountMismatch,
                    (true, false) => DiscrepancyKind::DateOutOfWindow,
                    _ => DiscrepancyKind::AmountAndDateMismatch,
                },
                statement: line.clone(),
                ledger: candidate.clone(),
                amount_difference,
                date_difference_days,
            }),
        }
    }

    // Pass 2: by amount and date proximity.
    for (si, line) in statement.iter().enumerate() {
        if statement_used[si] {
            continue;
        }
        let best = ledger
            .iter()
            .enumerate()
            .filter(|(li, l)| {
                let (a, d) = diffs(line, l);
                !ledger_used[*li] && within(a, d) == (true, true)
            })
            .min_by_key(|(_, l)| {
                let (a, d) = diffs(line, l);
                (d.unsigned_abs(), a.unsigned_abs(), l.sequence)
            });
        if let Some((li, candidate)) = best {
            ledger_used[li] = true;
            statement_used[si] = true;
            let (amount_difference, date_difference_days) = diffs(line, candidate);
            result.matched.push(Match {
                method: MatchMethod::AmountDate,
                statement: line.clone(),
                ledger: candidate.clone(),
                amount_difference,
                date_difference_days,
            });
        }
    }

    result.unmatched_statement = statement
        .iter()
        .zip(&statement_used)
        .filter(|(_, used)| !**used)
        .map(|(l, _)| l.clone())
        .collect();
    result.unmatched_ledger = ledger
        .iter()
        .zip(&ledger_used)
        .filter(|(_, used)| !**used)
        .map(|(l, _)| l.clone())
        .collect();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 3, d).unwrap()
    }

    fn st(line: usize, d: u32, amount: i64, reference: Option<&str>) -> StatementLine {
        StatementLine {
            line,
            date: date(d),
            amount,
            reference: reference.map(str::to_string),
            description: None,
        }
    }

    fn lg(sequence: u64, d: u32, amount: i64, reference: Option<&str>) -> LedgerLine {
        LedgerLine {
            sequence,
            transaction_id: TransactionId::generate(),
            date: date(d),
            amount,
            reference: reference.map(str::to_string),
            description: String::new(),
        }
    }

    const TOL: Tolerance = Tolerance {
        date_days: 2,
        amount: 0,
    };

    #[test]
    fn exact_reference_matches() {
        let r = match_lines(
            &[st(1, 1, 100, Some("A"))],
            &[lg(1, 1, 100, Some("A"))],
            TOL,
        );
        assert_eq!(r.matched.len(), 1);
        assert_eq!(r.matched[0].method, MatchMethod::Reference);
        assert!(r.unmatched_ledger.is_empty() && r.unmatched_statement.is_empty());
    }

    #[test]
    fn reference_with_wrong_amount_is_discrepancy() {
        let r = match_lines(
            &[st(1, 1, 120, Some("A"))],
            &[lg(1, 1, 100, Some("A"))],
            TOL,
        );
        assert_eq!(r.discrepancies.len(), 1);
        assert_eq!(r.discrepancies[0].kind, DiscrepancyKind::AmountMismatch);
        assert_eq!(r.discrepancies[0].amount_difference, 20);
        let r = match_lines(
            &[st(1, 10, 100, Some("A"))],
            &[lg(1, 1, 100, Some("A"))],
            TOL,
        );
        assert_eq!(r.discrepancies[0].kind, DiscrepancyKind::DateOutOfWindow);
        let r = match_lines(&[st(1, 10, 1, Some("A"))], &[lg(1, 1, 100, Some("A"))], TOL);
        assert_eq!(
            r.discrepancies[0].kind,
            DiscrepancyKind::AmountAndDateMismatch
        );
    }

    #[test]
    fn transaction_id_works_as_reference() {
        let ledger = lg(1, 1, 50, None);
        let reference = ledger.transaction_id.to_string();
        let r = match_lines(&[st(1, 1, 50, Some(&reference))], &[ledger], TOL);
        assert_eq!(r.matched[0].method, MatchMethod::Reference);
    }

    #[test]
    fn amount_date_pass_prefers_closest_date() {
        let ledger = vec![lg(1, 1, 100, None), lg(2, 3, 100, None)];
        let r = match_lines(&[st(1, 3, 100, None)], &ledger, TOL);
        assert_eq!(r.matched.len(), 1);
        assert_eq!(r.matched[0].ledger.sequence, 2);
        assert_eq!(r.matched[0].method, MatchMethod::AmountDate);
        assert_eq!(r.unmatched_ledger.len(), 1);
    }

    #[test]
    fn tolerances_are_respected() {
        let loose = Tolerance {
            date_days: 1,
            amount: 5,
        };
        let r = match_lines(&[st(1, 2, 103, None)], &[lg(1, 1, 100, None)], loose);
        assert_eq!(r.matched.len(), 1);
        assert_eq!(r.matched[0].amount_difference, 3);
        assert_eq!(r.matched[0].date_difference_days, 1);
        let r = match_lines(&[st(1, 4, 100, None)], &[lg(1, 1, 100, None)], loose);
        assert!(r.matched.is_empty());
        assert_eq!(r.unmatched_statement.len(), 1);
        assert_eq!(r.unmatched_ledger.len(), 1);
    }

    #[test]
    fn each_ledger_line_is_used_once() {
        let statement = vec![
            st(1, 1, 100, None),
            st(2, 1, 100, None),
            st(3, 1, 100, Some("X")),
        ];
        let ledger = vec![lg(1, 1, 100, None), lg(2, 1, 100, None)];
        let r = match_lines(&statement, &ledger, TOL);
        assert_eq!(r.matched.len(), 2);
        assert_eq!(r.unmatched_statement.len(), 1);
        // The referenced line had no ledger counterpart with that reference
        // and no amount match left, so it is the unmatched one.
        assert_eq!(r.unmatched_statement[0].line, 3);
    }

    #[test]
    fn duplicated_references_pair_by_closest_values() {
        let statement = vec![st(1, 1, 200, Some("R")), st(2, 1, 100, Some("R"))];
        let ledger = vec![lg(1, 1, 100, Some("R")), lg(2, 1, 200, Some("R"))];
        let r = match_lines(&statement, &ledger, TOL);
        assert_eq!(r.matched.len(), 2);
        assert!(r.discrepancies.is_empty());
    }
}
