//! `ledger-server`: the HTTP ledger service.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use ledger_core::app::{App, shutdown_signal};
use ledger_core::config::Config;
use ledger_core::telemetry;

#[derive(Debug, Parser)]
#[command(
    name = "ledger-server",
    version,
    about = "Double-entry ledger REST service"
)]
struct Args {
    /// Path to the TOML configuration file (env: LEDGER_CONFIG).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Validate configuration and exit.
    #[arg(long)]
    check_config: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let config = match Config::load(args.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = telemetry::init(&config.logging) {
        eprintln!("logging error: {e}");
        return ExitCode::from(2);
    }
    if args.check_config {
        println!("configuration OK");
        return ExitCode::SUCCESS;
    }
    let bind = config.server.bind;
    let app = match tokio::task::spawn_blocking(move || App::open(config)).await {
        Ok(Ok(app)) => app,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "failed to start");
            eprintln!("startup error: {e}");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("startup task failed: {e}");
            return ExitCode::from(1);
        }
    };
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, %bind, "cannot bind");
            eprintln!("cannot bind {bind}: {e}");
            return ExitCode::from(1);
        }
    };
    match app.serve(listener, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "server error");
            ExitCode::from(1)
        }
    }
}
