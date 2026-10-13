# Getting started

## Prerequisites

- Rust toolchain (developed and verified with Rust 1.98, edition 2024)
- No C compiler, database or other service is required.
- `curl` (and optionally `jq`) to try the API by hand.

## Build and test

```bash
cargo build                    # debug build of the library and both binaries
cargo test --all-targets       # full test suite
cargo build --release          # optimized binaries in target/release/
```

## Create API keys

Keys are random secrets; the server only stores their SHA-256 hash.

```bash
cargo run --bin ledgerctl -- hash-key --role admin  --id ops
cargo run --bin ledgerctl -- hash-key --role writer --id payments-svc
cargo run --bin ledgerctl -- hash-key --role reader --id reporting
```

Each command prints the plaintext `key` (give it to the client, store it in a
secret manager) and ready-to-paste `env` / `config_toml` snippets containing
only the hash.

| Role | Can do |
|---|---|
| `reader` | all `GET` endpoints except the audit trail |
| `writer` | reader + create accounts, update metadata, create/post/void/reverse transactions, run reconciliations |
| `admin` | writer + account status and policy changes, audit list/verify, snapshots, compaction |

## Run the server

```bash
export LEDGER_API_KEYS="ops:admin:<hash1>,payments-svc:writer:<hash2>"
export LEDGER_DATA_DIR=./data
cargo run --release --bin ledger-server -- --config examples/ledger.toml
```

`ledger-server --check-config` validates the configuration and exits.
Stop the server with Ctrl-C (SIGTERM on Unix): it drains in-flight requests,
stops background tasks, fsyncs the WAL and writes a final snapshot.

## Configuration

Precedence: built-in defaults < TOML file (`--config` or `LEDGER_CONFIG`) <
environment variables. Unknown keys in the TOML file are rejected, and
validation reports all problems at once.

| Setting | Default | Env override |
|---|---|---|
| `server.bind` | `127.0.0.1:8080` | `LEDGER_BIND` |
| `server.max_body_bytes` | `1048576` | `LEDGER_MAX_BODY_BYTES` |
| `server.request_timeout_secs` | `30` | `LEDGER_REQUEST_TIMEOUT_SECS` |
| `server.metrics_public` | `true` | `LEDGER_METRICS_PUBLIC` |
| `storage.data_dir` | `./data` | `LEDGER_DATA_DIR` |
| `storage.fsync` | `always` (`batch`, `never`) | `LEDGER_FSYNC` |
| `storage.fsync_batch` | `32` | |
| `storage.segment_max_bytes` | 64 MiB | |
| `storage.max_record_bytes` | 4 MiB | |
| `storage.snapshot_every_events` | `10000` (0 = off) | `LEDGER_SNAPSHOT_EVERY` |
| `storage.snapshot_retain` | `3` | |
| `storage.snapshot_check_interval_secs` | `30` | |
| `storage.repair_torn_tail` | `true` | |
| `storage.truncate_corruption` | `false` | |
| `storage.audit_fsync` | `true` | |
| `ledger.max_postings` | `64` | |
| `ledger.max_amount` | `10^15` minor units | |
| `ledger.max_pending_ttl_secs` | 30 days | |
| `ledger.pending_sweep_interval_secs` | `5` | |
| `ledger.default_policies.<type>` | asset/liability: `no_overdraft`, others `unrestricted` | |
| `ledger.extra_currencies` | `[]` | |
| `idempotency.ttl_secs` | `86400` | `LEDGER_IDEMPOTENCY_TTL_SECS` |
| `idempotency.max_entries` | `100000` | |
| `idempotency.purge_interval_secs` | `60` | |
| `auth.api_keys` | `[]` | `LEDGER_API_KEYS` (`id:role:hash,...`) |
| `logging.level` | `info` | `LEDGER_LOG_LEVEL` (`RUST_LOG` wins if set) |
| `logging.format` | `json` | `LEDGER_LOG_FORMAT` |
| `reconciliation.date_tolerance_days` | `2` | |
| `reconciliation.amount_tolerance` | `0` | |
| `reconciliation.max_statement_lines` | `10000` | |

With no API keys configured the server still starts (health, readiness and
public metrics work) but every `/v1` request is rejected, and a warning is
logged.

## First requests

```bash
K="Authorization: Bearer <writer key>"
J="Content-Type: application/json"
curl -s -H "$K" -H "$J" -X POST localhost:8080/v1/accounts \
  -d '{"id":"bank","name":"Bank","type":"asset","currency":"USD"}'
curl -s -H "$K" -H "$J" -X POST localhost:8080/v1/accounts \
  -d '{"id":"capital","name":"Capital","type":"equity","currency":"USD"}'
curl -s -H "$K" -H "$J" -H "Idempotency-Key: fund-1" -X POST localhost:8080/v1/transactions \
  -d '{"postings":[{"account_id":"bank","direction":"debit","amount":"500.00"},
                   {"account_id":"capital","direction":"credit","amount":"500.00"}]}'
curl -s -H "$K" localhost:8080/v1/accounts/bank/balance
```

More examples: `examples/requests.http`, `examples/demo.sh`, and
`cargo run --example quickstart` for library use.

## Offline maintenance with ledgerctl

```bash
ledgerctl --data-dir ./data replay                 # rebuild and summarize
ledgerctl --data-dir ./data replay --to-sequence 120
ledgerctl --data-dir ./data verify                 # WAL, snapshot vs log, invariants, audit chain
ledgerctl --data-dir ./data events --from 1 --limit 20
ledgerctl --data-dir ./data balance bank --as-of-sequence 42
ledgerctl --data-dir ./data trial-balance
ledgerctl --data-dir ./data snapshot               # requires the server to be stopped
ledgerctl --data-dir ./data compact                # requires the server to be stopped
```

Read-only commands take a shared lock and therefore refuse to run while the
server holds the data directory, unless `--no-lock` is passed. Exit codes:
`0` success, `1` verification found problems, `2` usage/configuration/storage
error.
