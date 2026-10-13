//! Embedding the ledger engine directly (no HTTP): open a ledger, create a
//! chart of accounts, run a two-phase transfer and a partial reversal, then
//! print the trial balance.
//!
//! Run with: `cargo run --example quickstart`

use std::sync::Arc;
use std::time::Duration;

use ledger_core::domain::account::AccountType;
use ledger_core::domain::events::ReversedLeg;
use ledger_core::domain::ids::AccountId;
use ledger_core::engine::{
    AsOf, CreateTransaction, Ledger, LedgerOptions, OpenAccount, PostingSpec, ReverseTransaction,
};
use ledger_core::metrics::LedgerMetrics;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("ledger-quickstart-{}", std::process::id()));
    let ledger = Ledger::open(
        LedgerOptions::ephemeral(&dir),
        Arc::new(LedgerMetrics::new()),
    )?;

    for (id, kind) in [
        ("bank", AccountType::Asset),
        ("capital", AccountType::Equity),
        ("wallet", AccountType::Liability),
        ("sales", AccountType::Revenue),
    ] {
        ledger.open_account(OpenAccount::new(id, kind, "USD")?)?;
    }

    // The owner invests 1,000.00.
    ledger.create_transaction(CreateTransaction::posted(vec![
        PostingSpec::debit("bank", 100_000)?,
        PostingSpec::credit("capital", 100_000)?,
    ]))?;

    // A customer top-up is held as pending, then posted.
    let hold = ledger.create_transaction(CreateTransaction::pending(
        vec![
            PostingSpec::debit("bank", 20_000)?,
            PostingSpec::credit("wallet", 20_000)?,
        ],
        Some(Duration::from_secs(600)),
    ))?;
    ledger.post_pending(hold.transaction.id)?;

    // A sale paid from the wallet, then a partial refund of 15.00.
    let sale = ledger.create_transaction(CreateTransaction::posted(vec![
        PostingSpec::debit("wallet", 5_000)?,
        PostingSpec::credit("sales", 5_000)?,
    ]))?;
    ledger.reverse(ReverseTransaction::partial(
        sale.transaction.id,
        vec![
            ReversedLeg {
                posting_index: 0,
                amount: 1_500,
            },
            ReversedLeg {
                posting_index: 1,
                amount: 1_500,
            },
        ],
    ))?;

    let wallet = ledger.balance(&AccountId::new("wallet")?, AsOf::Latest)?;
    println!("wallet balance: {} minor units", wallet.posted);
    let tb = ledger.trial_balance();
    println!("trial balance balanced: {}", tb.balanced);
    println!("{}", serde_json::to_string_pretty(&tb)?);

    drop(ledger);
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
