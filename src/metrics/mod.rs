//! Application metrics: a registry plus strongly typed handles for every
//! metric the service records.

pub mod registry;

use std::sync::Arc;

pub use registry::{Counter, Family, Gauge, Histogram, LATENCY_BUCKETS, Registry};

/// All metrics exported by the ledger service.
pub struct LedgerMetrics {
    registry: Registry,
    pub http_requests: Arc<Family<Counter>>,
    pub http_latency: Arc<Family<Histogram>>,
    pub http_in_flight: Arc<Gauge>,
    pub events_appended: Arc<Family<Counter>>,
    pub commands: Arc<Family<Counter>>,
    pub lock_wait: Arc<Histogram>,
    pub wal_append: Arc<Histogram>,
    pub last_sequence: Arc<Gauge>,
    pub accounts: Arc<Gauge>,
    pub transactions: Arc<Gauge>,
    pub pending_transactions: Arc<Gauge>,
    pub snapshots: Arc<Family<Counter>>,
    pub snapshot_duration: Arc<Histogram>,
    pub pending_expired: Arc<Counter>,
    pub idempotency: Arc<Family<Counter>>,
    pub idempotency_entries: Arc<Gauge>,
    pub auth_failures: Arc<Family<Counter>>,
    pub audit_records: Arc<Counter>,
    pub reconciliations: Arc<Family<Counter>>,
}

impl Default for LedgerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl LedgerMetrics {
    pub fn new() -> Self {
        let r = Registry::new();
        LedgerMetrics {
            http_requests: r.counter_family(
                "ledger_http_requests_total",
                "HTTP requests by method, matched route and status code.",
                &["method", "route", "status"],
            ),
            http_latency: r.histogram_family(
                "ledger_http_request_duration_seconds",
                "HTTP request latency by method and matched route.",
                &["method", "route"],
                LATENCY_BUCKETS,
            ),
            http_in_flight: r.gauge(
                "ledger_http_requests_in_flight",
                "HTTP requests currently being served.",
            ),
            events_appended: r.counter_family(
                "ledger_events_appended_total",
                "Events appended to the write-ahead log by event type.",
                &["type"],
            ),
            commands: r.counter_family(
                "ledger_commands_total",
                "Ledger commands by name and outcome (ok or error code).",
                &["command", "outcome"],
            ),
            lock_wait: r.histogram(
                "ledger_lock_wait_seconds",
                "Time spent acquiring per-account locks.",
                LATENCY_BUCKETS,
            ),
            wal_append: r.histogram(
                "ledger_wal_append_seconds",
                "Time spent appending (and syncing) one WAL record.",
                LATENCY_BUCKETS,
            ),
            last_sequence: r.gauge(
                "ledger_last_sequence",
                "Sequence number of the last committed event.",
            ),
            accounts: r.gauge("ledger_accounts", "Number of accounts."),
            transactions: r.gauge("ledger_transactions", "Number of transactions."),
            pending_transactions: r.gauge(
                "ledger_pending_transactions",
                "Number of transactions in the pending state.",
            ),
            snapshots: r.counter_family(
                "ledger_snapshots_total",
                "Snapshots taken by outcome.",
                &["outcome"],
            ),
            snapshot_duration: r.histogram(
                "ledger_snapshot_duration_seconds",
                "Time to capture and persist a snapshot.",
                LATENCY_BUCKETS,
            ),
            pending_expired: r.counter(
                "ledger_pending_expired_total",
                "Pending transactions voided automatically after expiry.",
            ),
            idempotency: r.counter_family(
                "ledger_idempotency_requests_total",
                "Idempotent requests by result (new, replayed, conflict, in_progress).",
                &["result"],
            ),
            idempotency_entries: r.gauge(
                "ledger_idempotency_entries",
                "Live entries in the idempotency store.",
            ),
            auth_failures: r.counter_family(
                "ledger_auth_failures_total",
                "Rejected requests by reason (missing, invalid, forbidden).",
                &["reason"],
            ),
            audit_records: r.counter(
                "ledger_audit_records_total",
                "Records appended to the audit trail.",
            ),
            reconciliations: r.counter_family(
                "ledger_reconciliations_total",
                "Reconciliation runs by result (balanced, discrepancies).",
                &["result"],
            ),
            registry: r,
        }
    }

    /// Records the outcome of an engine command.
    pub fn record_command(&self, command: &str, outcome: Result<(), &str>) {
        let outcome = match outcome {
            Ok(()) => "ok",
            Err(code) => code,
        };
        self.commands.with(&[command, outcome]).inc();
    }

    /// Renders all metrics in Prometheus text format.
    pub fn render(&self) -> String {
        self.registry.render()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_metrics_render_with_ledger_prefix() {
        let m = LedgerMetrics::new();
        m.record_command("create_transaction", Ok(()));
        m.record_command("create_transaction", Err("insufficient_funds"));
        m.last_sequence.set(42.0);
        let text = m.render();
        assert!(text.contains(
            "ledger_commands_total{command=\"create_transaction\",outcome=\"insufficient_funds\"} 1"
        ));
        assert!(text.contains("ledger_last_sequence 42"));
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            assert!(line.starts_with("ledger_"), "{line}");
        }
    }
}
