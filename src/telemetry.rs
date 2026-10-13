//! Structured logging setup via `tracing`.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

use crate::config::{LogFormat, LoggingConfig};

/// Builds the log filter. `RUST_LOG`, when set, takes precedence over the
/// configured level so operators can raise verbosity without editing config.
pub fn build_filter(config: &LoggingConfig) -> Result<EnvFilter, String> {
    match std::env::var("RUST_LOG") {
        Ok(directive) if !directive.trim().is_empty() => {
            EnvFilter::try_new(directive).map_err(|e| format!("invalid RUST_LOG: {e}"))
        }
        _ => EnvFilter::try_new(&config.level)
            .map_err(|e| format!("invalid logging.level {:?}: {e}", config.level)),
    }
}

/// Installs the global subscriber. Safe to call more than once; later calls
/// are ignored (useful in tests).
pub fn init(config: &LoggingConfig) -> Result<(), String> {
    let filter = build_filter(config)?;
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_span_events(FmtSpan::NONE);
    let result = match config.format {
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .try_init(),
        LogFormat::Pretty => builder.compact().try_init(),
    };
    if let Err(e) = result {
        // A subscriber is already installed (e.g. by a test harness).
        tracing::debug!(error = %e, "logging already initialised");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_validation() {
        let ok = LoggingConfig {
            level: "ledger_core=debug,info".into(),
            format: LogFormat::Pretty,
        };
        if std::env::var("RUST_LOG").is_err() {
            assert!(build_filter(&ok).is_ok());
            let bad = LoggingConfig {
                level: "ledger_core=notalevel".into(),
                format: LogFormat::Json,
            };
            assert!(build_filter(&bad).is_err());
        }
        assert!(init(&ok).is_ok());
        assert!(init(&ok).is_ok());
    }
}
