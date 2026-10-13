# distributed-ledger-core

A double-entry financial ledger service written in Rust. It keeps an
append-only, checksummed event log as the source of truth, rebuilds state by
replay (with snapshots for fast recovery), and exposes a REST API with API-key
authentication, idempotent writes, reconciliation, a hash-chained audit trail
and Prometheus metrics. A companion CLI, `ledgerctl`, performs offline replay,
verification, snapshots and compaction.

> Despite the name, this is a **single-node** ledger. "Distributed" refers to
> the problem domain (many concurrent clients, crash safety); there is no
> replication or consensus. See [Limitations](#limitations).

## Implemented functionality

| Area | What is implemented |
|---|---|
| Chart of accounts | Asset / liability / equity / revenue / expense accounts, normal-balance sides, status lifecycle (open ↔ frozen → closed), metadata, per-account negative-balance policy (`no_overdraft`, `overdraft{limit}`, `unrestricted`) with configurable defaults per account type |
| Money | ISO 4217 currencies with minor units (0, 2 and 3-decimal currencies built in, custom currencies via config); all amounts are `i64` minor units with checked arithmetic; exact decimal-string parsing (`"10.50"`), no floats |
| Transactions | Atomic multi-leg postings that must balance **per currency**; posted or pending (two-phase) with optional expiry; post/void of pending transfers; background expiry sweeper |
| Reversals | Full and partial reversals linked to the original, cumulative per-leg tracking, double reversal and reversal-of-reversal rejected |
| Concurrency | Per-account locks acquired in deterministic (sorted) order; optimistic `expected_version(s)` checks on accounts and transactions; stress-tested |
| Idempotency | `Idempotency-Key` header with SHA-256 request fingerprinting, scoped per API key, TTL, capacity bound, persisted to a checksummed log; plus an engine-level, WAL-persisted key check for transaction creation |
| Event sourcing | Every change is one event in a segmented WAL (CRC32-framed records); state is rebuilt by replay; snapshots with fallback to older snapshots; torn-write detection and repair on recovery; compaction |
| Balances | Current, available (pending-aware) and point-in-time balances by event sequence or timestamp; account history with stable cursor pagination |
| Reconciliation | CSV/JSON statement import, matching by reference then by amount/date with tolerance windows, discrepancy and unmatched reports, closing-balance check; trial balance and invariant check (sum of balances per currency = 0, pending consistency, reversal bookkeeping) |
| Audit | Hash-chained (SHA-256) audit trail of every mutating API call, including rejected ones; on-disk chain verification endpoint and CLI |
| API | axum REST API, JSON error envelope, roles `reader` / `writer` / `admin`, hashed API keys, body-size limit, request timeout, request ids |
| Observability | `tracing` structured logs (JSON or pretty), Prometheus text endpoint from a small built-in registry (counters, gauges, histograms) |
| Config | Defaults → TOML file → `LEDGER_*` environment variables, with validation that reports every problem at once |
| Tooling | `ledgerctl`: replay, verify, snapshot, compact, events, balance, trial-balance, hash-key, check-config |
| Benchmarks | criterion benches for posting throughput and replay speed |

## Technology stack

Rust (edition 2024, developed with Rust 1.98) · tokio · axum 0.8 · tower ·
serde / serde_json / toml · tracing + tracing-subscriber · crc32fast · sha2 ·
parking_lot · chrono · uuid (v7 ids) · csv · clap · criterion · proptest ·
reqwest (tests only, no TLS). Everything is pure Rust; no C dependencies, no
external services.

## Quick start

```bash
# Build and run the tests
cargo build
cargo test --all-targets

# Generate an admin key (prints the plaintext key and its SHA-256 hash)
cargo run --bin ledgerctl -- hash-key --role admin --id ops

# Start the server with that key hash
LEDGER_API_KEYS="ops:admin:<hash>" LEDGER_DATA_DIR=./data cargo run --bin ledger-server

# Talk to it
curl -s localhost:8080/health
curl -s -H "Authorization: Bearer <key>" -H 'Content-Type: application/json' \
  -X POST localhost:8080/v1/accounts \
  -d '{"id":"bank","name":"Bank","type":"asset","currency":"USD"}'
```

`examples/demo.sh` runs a full scripted session (keys, server, requests,
shutdown). `cargo run --example quickstart` shows the engine used as a
library. See [GETTING_STARTED.md](GETTING_STARTED.md) for configuration and
[API.md](API.md) for every endpoint.

## Architecture in one paragraph

Requests are authenticated, validated and turned into engine commands. A
command locks the accounts it touches in sorted order, validates against
current state, appends **one** event to the WAL (which assigns its sequence
number), applies that event through the same function replay uses, and
releases the locks. Snapshots are taken under a brief exclusive "commit gate"
so they always correspond to an exact WAL position. On start-up the newest
valid snapshot is loaded and the WAL tail is replayed. Details:
[ARCHITECTURE.md](ARCHITECTURE.md), [docs/data-model.md](docs/data-model.md),
[docs/storage-format.md](docs/storage-format.md).

## Configuration

All settings have defaults; see [GETTING_STARTED.md](GETTING_STARTED.md#configuration),
[`examples/ledger.toml`](examples/ledger.toml) and [`.env.example`](.env.example).
The most important environment variables: `LEDGER_CONFIG`, `LEDGER_BIND`,
`LEDGER_DATA_DIR`, `LEDGER_FSYNC`, `LEDGER_API_KEYS`, `LEDGER_LOG_LEVEL`.

## Testing

```bash
cargo test --all-targets      # unit, property, integration, recovery, concurrency, CLI tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo bench --no-run          # benchmarks compile; `cargo bench` runs them
```

The suite contains unit tests beside the code, proptest property tests
(balancing invariants, replay determinism, decimal round-trips, frame
scanning), HTTP integration tests against an in-process server on an ephemeral
port, crash-recovery tests that truncate and corrupt the WAL, concurrency
stress tests, and end-to-end tests of the `ledgerctl` binary. See
[docs/testing.md](docs/testing.md) for the inventory and the last recorded
results.

## Verification

Recorded on 2026-10-12 (Windows 11, Rust 1.98.0, x86_64-pc-windows-msvc):

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | no warnings |
| `cargo test --all-targets` | **220 passed, 0 failed** � 166 unit (lib), 6 `api_accounts`, 7 `api_reconciliation_audit`, 10 `api_security`, 10 `api_transactions`, 6 `cli`, 6 `concurrency`, 2 `properties` (proptest, 24 cases each), 7 `recovery` |
| `cargo bench --no-run` | both benchmarks compile (not executed for numbers) |
| `cargo run --example quickstart` | runs; trial balance balanced |
| Smoke test | `ledger-server` started with a key from `ledgerctl hash-key`; `/health` 200, `/ready` 200, two accounts created (201), the same `Idempotency-Key` transaction posted twice (201 + `idempotent-replayed: true`), balance 500.00, unauthenticated request 401 with error envelope, invariants ok, audit chain verified (3 records), metrics exposed; after stopping the process `ledgerctl verify` on its data directory reported ok |

## Deployment

`Dockerfile` (multi-stage, non-root runtime, health check on `/ready`) and
`compose.yaml` (single node, named volume, read-only root filesystem) are
provided. **They were written carefully but were not built or run locally,
because Docker is not available on the development machine.** Operational
guidance (backups, snapshots, compaction, incident handling) is in
[docs/operations.md](docs/operations.md).

## Limitations

- Single node only: no replication, consensus, sharding or failover. The
  data directory is locked to one process.
- All state (including full per-account history) is held in memory; very
  large ledgers need more RAM and produce large snapshots (snapshots are full
  JSON copies of state, not incremental).
- WAL writes are serialized through one mutex; throughput is bounded by a
  single append stream (and by fsync latency with `fsync = "always"`).
- Trial balance, invariant checks and snapshots briefly block writers while
  capturing a consistent view.
- The audit log listing re-reads the audit file; fine for moderate sizes, not
  indexed.
- Reconciliation matching is greedy (deterministic, closest-first), not a
  globally optimal assignment; one account per run.
- No FX: multi-currency transactions must balance per currency (e.g. via FX
  clearing accounts); there are no exchange rates.
- API keys are static configuration (no rotation endpoint, no expiry, no rate
  limiting); there is no TLS termination (run behind a proxy).
- Request timeouts do not cancel an engine operation that already started; a
  timed-out write may still commit, so clients should retry with the same
  `Idempotency-Key`.
- Benchmarks compile and can be run, but no benchmark numbers are claimed
  here.
- Docker artifacts are untested (see above). The CI workflow is a local file
  only and has never been executed.

## Roadmap

- Replication of the WAL to followers and leader election.
- Incremental snapshots and history paging from disk to bound memory.
- Group commit (batching fsync across concurrent writers).
- Key rotation API, per-key rate limiting, mTLS.
- FX conversion support with rate sources; multi-account reconciliation.
- Indexed audit storage and export.

## AI-assisted development

This project was developed with AI assistance (Claude by Anthropic). The code,
tests and documentation were generated iteratively and then reviewed and
verified by building the project, running `cargo fmt`, `cargo clippy` with
warnings denied, the full test suite, compiling the benchmarks, and
smoke-testing the running server with real HTTP requests. Claims in these
documents are limited to what was actually verified; unverified items (Docker
images, CI execution, performance numbers) are called out explicitly.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) Robert Ramos.
