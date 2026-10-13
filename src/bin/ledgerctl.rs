//! `ledgerctl` entry point. All logic lives in [`ledger_core::cli`].

use std::process::ExitCode;

use clap::Parser;
use ledger_core::cli::{Cli, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut stdout = std::io::stdout().lock();
    match run(cli, &mut stdout) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code.clamp(1, 255) as u8),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}
