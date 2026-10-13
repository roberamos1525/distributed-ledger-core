# Testing

## Commands

```bash
cargo test --all-targets                    # everything below
cargo test --lib                            # unit tests only
cargo test --test api_transactions          # one integration suite
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo bench --no-run                        # compile benchmarks
cargo bench                                 # run benchmarks (criterion)
```

## Inventory

| Suite | Location | What it covers |
|---|---|---|
| Unit tests | `#[cfg(test)]` modules in `src/` | money parsing/formatting, ids and validation, balance effects and policies, balancing rules, event serde, frame codec, WAL rotation/compaction, snapshots, recovery edge cases, event application, locking, engine commands, queries, reports, idempotency store, audit chain, statement parsing, matcher, config, metrics registry, auth, DTOs, error mapping |
| Property tests | `src/domain/money.rs`, `src/domain/transaction.rs`, `src/store/frame.rs`, `tests/properties.rs` | decimal format/parse round-trip; mirrored postings always balance; any WAL prefix scans without false corruption; random sequences of post/pending/commit/void/reverse/unbalanced operations keep all invariants and replay to an identical state; point-in-time balances equal running sums |
| HTTP integration | `tests/api_*.rs` | in-process server on an ephemeral port exercised with a real HTTP client: accounts, policies, statuses, versions, transactions, two-phase transfers, expiry sweeper, reversals, idempotency (including across restarts), pagination, point-in-time balances, reconciliation, reports, audit list/verify/tamper detection, auth/roles, body limit, content types, error envelopes, request ids, metrics, compaction + restart |
| Crash recovery | `tests/recovery.rs` | WAL truncated mid-record, truncation at many offsets, garbage tail, flipped bit in a middle record, snapshot vs log replay equality, corrupt newest snapshot fallback, crash after compaction |
| Concurrency | `tests/concurrency.rs` | 8 threads × 150 random three-leg transactions with random lock orders (no deadlock, invariants hold, replay identical), overdraft floor under 16-thread contention, optimistic version retries without lost updates, concurrent identical idempotency keys, post/void races, snapshots during writes |
| CLI | `tests/cli.rs` | the `ledgerctl` binary: hash-key, replay (with/without snapshots), balance as of a sequence, events, trial-balance, verify (clean and tampered WAL), snapshot, compact, lock handling, config validation |
| Benchmarks | `benches/posting.rs`, `benches/replay.rs` | posting throughput (two-leg, eight-leg, 4-thread contended) and replay speed (WAL only vs snapshot + tail) |

## Last recorded run

See the "Verification" section of `README.md` for the exact command and
counts recorded for the current version.

## Writing new tests

- Use `tests/common::TestServer` for HTTP tests; it creates a temp data
  directory, admin/writer/reader keys, binds `127.0.0.1:0` and shuts down
  gracefully (`stop()` returns the directory for restart scenarios).
- Use `tests/common::ledger()` / `open_ledger()` for engine-level tests.
- When adding state-changing behaviour, assert that `capture_state()` after a
  reopen equals the live state.
