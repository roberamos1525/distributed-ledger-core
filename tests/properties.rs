//! Property tests: arbitrary sequences of ledger operations always preserve
//! the double-entry invariants, unbalanced input is always rejected, and a
//! replay of the log reproduces the live state exactly.

mod common;

use common::*;
use ledger_core::domain::account::AccountType;
use ledger_core::domain::events::ReversedLeg;
use ledger_core::domain::transaction::TransactionStatus;
use ledger_core::engine::{CreateTransaction, PostingSpec, ReverseTransaction};
use proptest::prelude::*;

const IDS: [&str; 5] = ["p0", "p1", "p2", "p3", "p4"];

#[derive(Debug, Clone)]
enum Op {
    /// Balanced by construction: each pair debits one account and credits another.
    Post(Vec<(usize, usize, i64)>),
    Pending(Vec<(usize, usize, i64)>),
    /// Debit total differs from credit total by `skew`.
    Unbalanced(usize, usize, i64, i64),
    Commit(usize),
    Void(usize),
    Reverse(usize, Option<i64>),
}

fn pairs() -> impl Strategy<Value = Vec<(usize, usize, i64)>> {
    proptest::collection::vec((0..IDS.len(), 0..IDS.len(), 1i64..10_000), 1..4)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => pairs().prop_map(Op::Post),
        2 => pairs().prop_map(Op::Pending),
        1 => (0..IDS.len(), 0..IDS.len(), 1i64..1000, 1i64..50).prop_map(|(a, b, x, s)| Op::Unbalanced(a, b, x, s)),
        1 => (0usize..64).prop_map(Op::Commit),
        1 => (0usize..64).prop_map(Op::Void),
        2 => ((0usize..64), proptest::option::of(1i64..5_000)).prop_map(|(i, p)| Op::Reverse(i, p)),
    ]
}

fn postings(pairs: &[(usize, usize, i64)]) -> Vec<PostingSpec> {
    pairs
        .iter()
        .flat_map(|(d, c, amount)| {
            [
                PostingSpec::debit(IDS[*d], *amount).unwrap(),
                PostingSpec::credit(IDS[*c], *amount).unwrap(),
            ]
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn invariants_hold_for_any_operation_sequence(ops in proptest::collection::vec(op(), 1..40)) {
        let (dir, ledger) = ledger();
        open_accounts(&ledger, &IDS, AccountType::Asset);
        let mut created = Vec::new();
        for op in &ops {
            match op {
                Op::Post(p) => {
                    let tx = ledger.create_transaction(CreateTransaction::posted(postings(p))).unwrap();
                    created.push(tx.transaction.id);
                }
                Op::Pending(p) => {
                    let tx = ledger.create_transaction(CreateTransaction::pending(postings(p), None)).unwrap();
                    created.push(tx.transaction.id);
                }
                Op::Unbalanced(a, b, x, skew) => {
                    let cmd = CreateTransaction::posted(vec![
                        PostingSpec::debit(IDS[*a], *x + *skew).unwrap(),
                        PostingSpec::credit(IDS[*b], *x).unwrap(),
                    ]);
                    let err = ledger.create_transaction(cmd).unwrap_err();
                    prop_assert_eq!(err.code(), "unbalanced_transaction");
                }
                Op::Commit(i) | Op::Void(i) => {
                    if let Some(id) = created.get(i % created.len().max(1)) {
                        let status = ledger.transaction(id).unwrap().status;
                        let result = if matches!(op, Op::Commit(_)) {
                            ledger.post_pending(*id)
                        } else {
                            ledger.void_pending(*id, None)
                        };
                        prop_assert_eq!(result.is_ok(), status == TransactionStatus::Pending);
                    }
                }
                Op::Reverse(i, partial) => {
                    if let Some(id) = created.get(i % created.len().max(1)) {
                        let tx = ledger.transaction(id).unwrap();
                        let cmd = match partial {
                            // Reverse the same amount on the first debit/credit pair.
                            Some(p) if tx.postings.len() >= 2 => {
                                let amount = (*p).min(tx.remaining_reversible(0)).min(tx.remaining_reversible(1));
                                if amount == 0 { continue; }
                                ReverseTransaction::partial(*id, vec![
                                    ReversedLeg { posting_index: 0, amount },
                                    ReversedLeg { posting_index: 1, amount },
                                ])
                            }
                            _ => ReverseTransaction::full(*id),
                        };
                        let result = ledger.reverse(cmd);
                        let reversible = tx.status == TransactionStatus::Posted
                            && !tx.is_reversal()
                            && !tx.is_fully_reversed();
                        prop_assert_eq!(result.is_ok(), reversible, "{:?}", result.err());
                        if let Ok(rev) = result {
                            created.push(rev.id);
                        }
                    }
                }
            }
        }
        let report = ledger.check_invariants();
        prop_assert!(report.ok, "{:?}", report.violations);
        let tb = ledger.trial_balance();
        prop_assert!(tb.balanced);
        let live = ledger.capture_state();
        drop(ledger);
        let replayed = open_ledger(dir.path());
        prop_assert_eq!(replayed.capture_state(), live);
    }

    #[test]
    fn point_in_time_balance_matches_running_sum(amounts in proptest::collection::vec(1i64..1_000_000, 1..25), probe in 0usize..25) {
        let (_dir, ledger) = ledger();
        open_accounts(&ledger, &["x", "y"], AccountType::Asset);
        let mut sequences = Vec::new();
        for amount in &amounts {
            sequences.push(transfer(&ledger, "x", "y", *amount).unwrap().created_sequence);
        }
        let k = probe % amounts.len();
        let expected: i64 = amounts[..=k].iter().sum();
        let x = ledger_core::domain::ids::AccountId::new("x").unwrap();
        let at = ledger.balance(&x, ledger_core::engine::AsOf::Sequence(sequences[k])).unwrap();
        prop_assert_eq!(at.posted, expected);
    }
}
