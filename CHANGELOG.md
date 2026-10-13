# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-10-12

### Added

- Double-entry domain model: account types with normal sides, statuses,
  per-account negative-balance policies, ISO 4217 currencies with minor units
  and exact decimal parsing.
- Atomic multi-leg transactions balanced per currency; pending (two-phase)
  transfers with post/void and automatic expiry.
- Full and partial reversals with per-leg tracking and double-reversal
  protection.
- Per-account locking with deterministic ordering and optimistic version
  checks.
- Segmented, CRC32-framed write-ahead log; snapshots with fallback; crash
  recovery with torn-write repair; compaction.
- Point-in-time balances (by sequence or time) and cursor-paginated history.
- HTTP and engine-level idempotency with request fingerprinting, TTL and
  persistence.
- Reconciliation against CSV/JSON statements with tolerance windows; trial
  balance and invariant checks.
- Hash-chained audit trail with verification.
- axum REST API with API-key roles, JSON error envelope, body limit, timeout
  and request ids.
- Prometheus metrics registry and structured `tracing` logs.
- TOML + environment configuration with validation.
- `ledgerctl` CLI: replay, verify, snapshot, compact, events, balance,
  trial-balance, hash-key, check-config.
- criterion benchmarks, property tests, HTTP integration tests, crash-recovery
  and concurrency stress tests.
- Dockerfile, compose file and CI workflow (not executed locally).
