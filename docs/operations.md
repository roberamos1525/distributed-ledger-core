# Operations runbook

## Health checks

| Probe | Endpoint | Meaning |
|---|---|---|
| liveness | `GET /health` | process is serving HTTP |
| readiness | `GET /ready` | recovery finished and writes are accepted; `503 read_only` after an unrecoverable write failure |

## Key metrics and alerts

| Metric | Suggested alert |
|---|---|
| `ledger_http_requests_total{status=~"5.."}` | any sustained rate |
| `ledger_http_request_duration_seconds` | p99 above SLO |
| `ledger_wal_append_seconds` | p99 rising: disk latency (fsync) |
| `ledger_lock_wait_seconds` | p99 rising: hot accounts |
| `ledger_commands_total{outcome!="ok"}` | spikes of `storage_error` / `read_only` |
| `ledger_snapshots_total{outcome="error"}` | any |
| `ledger_auth_failures_total` | spikes (credential stuffing, misconfigured client) |
| `ledger_pending_transactions` | unbounded growth (clients not completing two-phase transfers) |

Also poll `GET /v1/reports/invariants` (returns `500` on any violation) and
`GET /v1/audit/verify` (returns `409` on a broken chain) from a monitor.

## Snapshots and compaction

- Automatic: every `storage.snapshot_check_interval_secs` the server checks
  whether `snapshot_every_events` events have been appended since the last
  snapshot. A final snapshot is written on graceful shutdown.
- Manual: `POST /v1/admin/snapshots`, or `ledgerctl snapshot` with the server
  stopped.
- Compaction: `POST /v1/admin/compactions` or `ledgerctl compact`. It
  snapshots first, then deletes WAL segments fully covered by that snapshot and
  prunes old snapshots. After compaction the snapshot is required for
  recovery — back it up.

## Backups

The data directory is the backup unit. Safe procedures:

1. **Cold**: stop the server (graceful shutdown flushes and snapshots), copy
   the whole directory, start the server.
2. **Warm**: trigger a snapshot, then copy `snapshots/` first and `wal/`
   second. A copy taken while writing may end in a torn record; recovery
   truncates it. Copy `audit.log` and `idempotency.log` as well.

Verify a backup with `ledgerctl --data-dir <copy> verify`.

## Restore / point-in-time inspection

```bash
ledgerctl --data-dir <dir> replay --to-sequence 1200     # state at sequence 1200
ledgerctl --data-dir <dir> balance bank --as-of-sequence 1200
ledgerctl --data-dir <dir> events --from 1190 --limit 20
```

## Incident: server will not start

1. Read the error. `storage: corrupt record in ... at offset N` means
   corruption in the WAL (not a torn tail).
2. Run `ledgerctl --data-dir <dir> verify` (with the server stopped) to see
   every segment's status.
3. Restore from backup if possible. As a last resort set
   `storage.truncate_corruption = true` for one start-up: everything after the
   corrupt record in the newest segment is discarded (the recovery report in
   `/v1/info` shows how many bytes). Corruption in an older segment always
   requires a backup.
4. `sequence gap` usually means WAL segments were deleted without a covering
   snapshot (e.g. a partial backup restore).

## Incident: `/ready` reports `read_only`

A WAL write failed and could not be rolled back (disk full, I/O error), or a
persisted event could not be applied. Reads continue; writes return `503`.
Fix the underlying problem, then restart; recovery truncates any partial
record. Investigate logs for `ledger switched to read-only mode`.

## Incident: data directory is locked

Another process holds `<data>/LOCK` (a second server, or `ledgerctl snapshot`
/ `compact`). The OS releases the lock when the process exits; it never needs
manual deletion.

## Key rotation

1. `ledgerctl hash-key --role <role> --id <new-id>`.
2. Add the new hash to `LEDGER_API_KEYS` alongside the old one; restart.
3. Move clients to the new key; remove the old entry; restart.

## Capacity notes

All state including per-account history is held in memory and serialized
in full into each snapshot, so memory use and snapshot size grow with the
number of accounts, history entries and transactions. No sizing measurements
have been published for this project; measure with representative data
(`ledgerctl replay` reports counts, and snapshot file sizes are visible in
`snapshots/`).
