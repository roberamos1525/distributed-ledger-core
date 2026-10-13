# Storage format

## Data directory

```
<data_dir>/
  LOCK                         OS file lock (exclusive: server/mutating CLI, shared: read-only CLI)
  wal/00000000000000000001.wal segment named after its first event sequence
  wal/00000000000000004211.wal
  snapshots/snapshot-00000000000000004000.snap
  audit.log                    hash-chained audit records
  idempotency.log              stored idempotent responses
```

## Record framing

All four files use the same frame (little endian):

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic: `LWAL`, `LSNP`, `LAUD` or `LIDM` |
| 4 | 4 | payload length |
| 8 | 8 | sequence |
| 16 | 4 | CRC32 over sequence ‖ length ‖ payload |
| 20 | n | payload (JSON) |

Scanning classifies what follows the last valid frame:

| Situation | Classification |
|---|---|
| end of file exactly at a frame boundary | clean |
| fewer than 20 bytes left, or payload extends past EOF | torn |
| only zero bytes left | torn |
| CRC mismatch on the **last** frame of the file | torn |
| CRC mismatch with more data after the frame | corrupt |
| wrong magic, or length above the record limit | corrupt |

Torn tails are the expected result of a crash during a write and are
truncated on start-up (`storage.repair_torn_tail`, default on). Corruption
stops start-up unless `storage.truncate_corruption` is enabled, which discards
the corrupt record and everything after it in the newest segment.

## WAL

- One frame per event; payload is the JSON `EventEnvelope`; the header
  sequence must equal the payload sequence.
- New segments start when the active one would exceed
  `storage.segment_max_bytes`. The previous segment is fsynced first.
- `fsync`: `always` (after each append), `batch` (every N appends), `never`.
- On an I/O error the segment is truncated back to its previous length; if
  that fails the WAL is poisoned and the ledger becomes read-only.
- Compaction removes closed segments whose events are all ≤ the newest
  snapshot sequence. The active segment is never removed.

## Snapshots

- One `LSNP` frame whose sequence is the last included event and whose
  payload is the JSON `LedgerState` (accounts with full history, transactions,
  last sequence and timestamp).
- Written to a temporary file, fsynced, renamed into place (and the directory
  fsynced on Unix).
- `snapshot_retain` newest snapshots are kept.
- Recovery uses the newest snapshot that verifies; a snapshot whose file name
  and header disagree is rejected.

## Recovery algorithm

```
state = newest valid snapshot or empty
expected = state.last_sequence + 1
for segment in segments (sorted):
    check first frame sequence == segment name
    for frame in valid frames:
        env = decode(frame)
        if env.sequence < expected: skip (covered by snapshot)
        if env.sequence != expected: error SequenceGap
        state.apply(env); expected += 1
    handle tail (clean / torn → truncate if newest / corrupt → error)
resume appending at the newest segment's valid length
```
