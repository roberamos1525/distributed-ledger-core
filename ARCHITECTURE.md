# Architecture

## Module layout

```
src/
  domain/          pure types, no I/O
    money.rs         CurrencyCode, Currency, CurrencyRegistry, exact decimal parsing
    ids.rs           AccountId, TransactionId (UUIDv7), field validation
    account.rs       AccountType, AccountStatus, BalancePolicy, Balances, AccountState, history
    transaction.rs   Direction, Posting, TransactionRecord, balancing rules
    events.rs        LedgerEvent, EventEnvelope
  store/           persistence (pure Rust, file based)
    frame.rs         CRC32 record framing + torn/corrupt tail classification
    wal.rs           segmented write-ahead log, rotation, compaction
    snapshot.rs      atomic snapshot files, fallback, pruning
    recovery.rs      snapshot + WAL replay, torn-tail repair, WAL inspection
    fs_util.rs       data layout, directory lock, atomic writes
  engine/          command handling
    state.rs         apply_event(): the single event-application code path; LedgerState
    locks.rs         deterministic per-account locking
    commands.rs      command inputs, engine limits, default policies
    ledger.rs        Ledger: open/recover, commands, snapshots, compaction
    queries.rs       lookups, listings, point-in-time balances, history pages
    reports.rs       trial balance, invariant check
  idempotency.rs   request fingerprinting, TTL store, persistence
  audit.rs         hash-chained audit log and verification
  reconciliation/  statement parsing (CSV/JSON) and matching
  metrics/         small Prometheus registry + LedgerMetrics handles
  config.rs        defaults, TOML, env overrides, validation
  telemetry.rs     tracing subscriber setup
  api/             axum layer: auth, errors, extractors, DTOs, middleware, handlers
  app.rs           service assembly, background tasks, graceful shutdown
  cli.rs           ledgerctl implementation
  bin/             ledger-server, ledgerctl entry points
```

Lower layers never depend on higher ones: `domain` ← `store` ← `engine` ←
(`idempotency`, `audit`, `reconciliation`) ← `api` ← `app` / `cli`. The store
does not know the engine's state type; it replays into any `ReplayTarget`.

## Write path

```
HTTP request
  └─ request-id middleware → timeout → body limit → API-key auth
       └─ handler: role check, JSON validation, idempotency (begin)
            └─ spawn_blocking → Ledger command
                 1. validate shape (no locks)
                 2. commit gate (shared)
                 3. lock transaction (if any), then accounts in sorted id order
                 4. validate against state: status, currency, versions, policy
                 5. WAL append (mutex): assigns sequence + monotonic timestamp,
                    serializes, CRC-frames, writes, fsyncs per policy
                 6. apply_event() on the locked state (same code as replay)
                 7. publish new records in indexes, release locks
            └─ audit record (hash chained), idempotency (complete)
```

Because the in-memory state only ever changes by applying an event that is
already durable in the WAL, a restart reproduces the exact same state. The
recovery and concurrency tests assert this byte-for-byte (`LedgerState`
equality) after randomized concurrent workloads.

If an append fails, the segment is truncated back to its previous length. If
even that fails, or if an already persisted event cannot be applied (a bug),
the ledger switches to **read-only** mode: writes return `503 read_only`,
`/ready` reports it, reads keep working.

## Concurrency model

* Account state lives in `Arc<Mutex<AccountState>>` cells in an ordered index.
* A command locks every account it touches **in ascending id order**. Since
  all threads use the same global order, no wait cycle — and therefore no
  deadlock — is possible regardless of how postings are listed.
* Global lock order: commit gate → at most one transaction lock → account
  locks (sorted) → WAL mutex. Index maps are held only briefly for lookups.
* Disjoint transactions validate and apply in parallel; only the WAL append is
  serialized.
* Optimistic concurrency: every event bumps the version of each account it
  touches; commands can carry `expected_version(s)` and fail with
  `409 version_conflict` on mismatch.
* Snapshots, trial balance and invariant checks take the commit gate
  exclusively for the duration of an in-memory copy, which guarantees they see
  a state corresponding to exactly one WAL sequence.
* Engine calls run on tokio's blocking pool (`spawn_blocking`) so locks and
  fsync never block async worker threads.

## Recovery

1. Acquire an exclusive OS lock on `<data>/LOCK`.
2. Load the newest snapshot that passes CRC and header checks; corrupt
   snapshots are skipped (logged and reported).
3. Scan WAL segments in order, skipping events covered by the snapshot,
   enforcing strict sequence continuity (gaps are errors).
4. An incomplete record at the end of the **newest** segment is a torn write:
   it is truncated (configurable). Invalid records followed by more data are
   corruption and stop start-up unless `truncate_corruption` is enabled.
5. Re-open the newest segment for appending at its valid length.

## Idempotency (two layers)

1. **HTTP layer** (`idempotency.rs`): `(api-key id, Idempotency-Key)` →
   fingerprint (SHA-256 of method, path and canonical JSON body) and the stored
   response. Replays return the same status/body with `Idempotent-Replayed:
   true`; a different payload returns `409 idempotency_key_reused`; a
   concurrent duplicate returns `409 request_in_progress`. 2xx and most 4xx
   responses are stored; 5xx, 401/403, 408, 409 and 429 are not.
2. **Engine layer**: the scoped key and fingerprint are stored inside the
   `TransactionRecord` (and therefore in the WAL). If the HTTP record was lost
   (crash between WAL commit and idempotency write), the engine still detects
   the duplicate and returns the original transaction.

## Observability

* Logs: one structured access-log line per request (method, route, status,
  latency, request id, principal) plus engine/recovery events.
* Metrics (`/metrics`): request counts and latency histograms per route,
  in-flight gauge, events appended by type, command outcomes by error code,
  lock wait and WAL append latency, snapshot counts/durations, idempotency
  outcomes, auth failures, audit records, reconciliation results, and gauges
  for accounts, transactions, pending transactions and last sequence.

## Key design decisions

| Decision | Rationale |
|---|---|
| Integer minor units, `i64`, checked arithmetic | Exactness; overflow is an error, never a wrap |
| One event per command | Atomicity: a multi-leg transaction is a single record, so it is either fully durable or absent |
| Same `apply_event` for live and replay | Eliminates divergence between running and recovered state |
| JSON payloads inside binary CRC frames | Debuggable records (`ledgerctl events`) with robust corruption detection |
| Sorted lock acquisition instead of a global lock | Parallelism for disjoint accounts with a simple deadlock-freedom argument |
| History position as cursor | Stable under appends, unlike offset pagination; multiple legs can share a sequence |
| Own metrics registry | Small, dependency-free, and fully covered by tests |
