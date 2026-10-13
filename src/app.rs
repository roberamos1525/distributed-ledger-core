//! Service assembly: opens all stores, builds the router, runs background
//! maintenance tasks and serves HTTP with graceful shutdown.

use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::api::{self, AppState};
use crate::audit::AuditLog;
use crate::config::Config;
use crate::engine::ledger::Ledger;
use crate::error::{LedgerError, StorageError};
use crate::idempotency::IdempotencyStore;
use crate::metrics::LedgerMetrics;
use crate::store::fs_util::DataLayout;

/// Errors that prevent the service from starting.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("ledger: {0}")]
    Ledger(#[from] LedgerError),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// A fully assembled service.
pub struct App {
    state: AppState,
}

impl App {
    /// Opens the ledger (running recovery), the idempotency store and the
    /// audit log under the configured data directory.
    pub fn open(config: Config) -> Result<App, StartupError> {
        let metrics = Arc::new(LedgerMetrics::new());
        let ledger = Arc::new(Ledger::open(config.ledger_options(), metrics.clone())?);
        let layout = DataLayout::new(&config.storage.data_dir);
        let (idempotency, idem_report) =
            IdempotencyStore::open(layout.idempotency_path(), config.idempotency_config())?;
        tracing::info!(
            loaded = idem_report.loaded,
            expired = idem_report.expired,
            damaged = idem_report.damaged_tail.is_some(),
            "idempotency store loaded"
        );
        let audit = Arc::new(AuditLog::open(
            layout.audit_path(),
            config.storage.audit_fsync,
        )?);
        if config.auth.api_keys.is_empty() {
            tracing::warn!("no API keys configured: every /v1 request will be rejected");
        }
        let state = AppState::new(config, ledger, idempotency, audit, metrics);
        Ok(App { state })
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn router(&self) -> Router {
        api::router(self.state.clone())
    }

    /// Starts periodic maintenance: pending-transaction expiry, idempotency
    /// purging and automatic snapshots.
    pub fn spawn_background(&self, shutdown: watch::Receiver<bool>) -> Vec<JoinHandle<()>> {
        let config = self.state.config().clone();
        let mut handles = Vec::new();

        let ledger = self.state.ledger().clone();
        handles.push(spawn_periodic(
            "pending-expiry",
            Duration::from_secs(config.ledger.pending_sweep_interval_secs),
            shutdown.clone(),
            move || {
                let expired = ledger.expire_pending(chrono::Utc::now());
                if expired > 0 {
                    tracing::info!(expired, "voided expired pending transactions");
                }
            },
        ));

        let idempotency = self.state.idempotency().clone();
        handles.push(spawn_periodic(
            "idempotency-purge",
            Duration::from_secs(config.idempotency.purge_interval_secs),
            shutdown.clone(),
            move || match idempotency.purge_expired(chrono::Utc::now()) {
                Ok(0) => {}
                Ok(n) => tracing::debug!(purged = n, "purged expired idempotency records"),
                Err(e) => tracing::warn!(error = %e, "idempotency purge failed"),
            },
        ));

        if config.storage.snapshot_every_events > 0 {
            let ledger = self.state.ledger().clone();
            let every = config.storage.snapshot_every_events;
            handles.push(spawn_periodic(
                "auto-snapshot",
                Duration::from_secs(config.storage.snapshot_check_interval_secs),
                shutdown,
                move || {
                    if ledger.events_since_snapshot() >= every
                        && let Err(e) = ledger.snapshot()
                    {
                        tracing::error!(error = %e, "automatic snapshot failed");
                    }
                },
            ));
        }
        handles
    }

    /// Serves HTTP on `listener` until `shutdown` resolves, then stops the
    /// background tasks, flushes the WAL and writes a final snapshot.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let (stop_tx, stop_rx) = watch::channel(false);
        let tasks = self.spawn_background(stop_rx);
        let router = self.router();
        let grace = Duration::from_secs(self.state.config().server.shutdown_grace_secs);
        tracing::info!(addr = %listener.local_addr()?, "ledger server listening");
        let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
        let server = axum::serve(listener, router).with_graceful_shutdown(async move {
            shutdown.await;
            let _ = signalled_tx.send(());
        });
        // Connections get `shutdown_grace_secs` to finish after the signal;
        // anything still open after that is dropped.
        let result = tokio::select! {
            r = server.into_future() => r,
            _ = async {
                if signalled_rx.await.is_ok() {
                    tokio::time::sleep(grace).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                tracing::warn!(grace_secs = grace.as_secs(), "grace period elapsed; closing remaining connections");
                Ok(())
            }
        };
        let _ = stop_tx.send(true);
        for task in tasks {
            let _ = task.await;
        }
        self.shutdown().await;
        result
    }

    /// Flushes state to disk; called on graceful shutdown.
    pub async fn shutdown(&self) {
        let ledger = self.state.ledger().clone();
        let outcome = tokio::task::spawn_blocking(move || {
            ledger.sync()?;
            if ledger.events_since_snapshot() > 0 {
                ledger.snapshot()?;
            }
            Ok::<_, LedgerError>(())
        })
        .await;
        match outcome {
            Ok(Ok(())) => tracing::info!("ledger flushed; shutdown complete"),
            Ok(Err(e)) => tracing::error!(error = %e, "error while flushing ledger on shutdown"),
            Err(e) => tracing::error!(error = %e, "shutdown task failed"),
        }
    }
}

/// Runs `work` on the blocking pool every `period` until shutdown.
fn spawn_periodic(
    name: &'static str,
    period: Duration,
    mut shutdown: watch::Receiver<bool>,
    work: impl Fn() + Send + Sync + 'static,
) -> JoinHandle<()> {
    let work = Arc::new(work);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let work = work.clone();
                    if let Err(e) = tokio::task::spawn_blocking(move || work()).await {
                        tracing::error!(task = name, error = %e, "background task panicked");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::debug!(task = name, "background task stopping");
                        break;
                    }
                }
            }
        }
    })
}

/// Resolves when the process receives Ctrl-C (and SIGTERM on Unix).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
