# Data model

## Accounts

| Field | Type | Notes |
|---|---|---|
| `id` | string | `[A-Za-z0-9_][A-Za-z0-9:._-]{0,63}`; client supplied or `acc_<uuid>` |
| `name` | string | 1..256 bytes, no control characters |
| `account_type` | enum | `asset`, `liability`, `equity`, `revenue`, `expense` |
| `currency` | ISO 4217 code | immutable after creation |
| `status` | enum | `open`, `frozen`, `closed` (terminal) |
| `policy` | enum | `no_overdraft`, `overdraft{limit}`, `unrestricted` |
| `metadata` | map | ≤ 32 entries |
| `version` | u64 | starts at 1, +1 for every event touching the account |
| `created_sequence` | u64 | WAL sequence of `account_opened` |

### Normal sides and signs

Balances are stored as one signed counter, `posted = Σ debits − Σ credits`
(debit-positive), plus `pending_debits` and `pending_credits`. They are
*presented* on the account's normal side:

| Type | Normal side | `posted` shown as | `available` |
|---|---|---|---|
| asset, expense | debit | `posted` | `posted − pending_credits` |
| liability, equity, revenue | credit | `−posted` | `−posted − pending_debits` |

Pending amounts that would *increase* the account are not available until
posted; pending amounts that *decrease* it are subtracted immediately.

### Balance policies

A command is rejected with `insufficient_funds` when, for any touched
account, the projected available balance is below the policy floor **and**
lower than before (so a corrective posting into an account that is already
below a tightened floor is still allowed).

| Policy | Floor |
|---|---|
| `no_overdraft` | 0 |
| `overdraft {limit}` | −limit |
| `unrestricted` | none |

## Transactions

| Field | Notes |
|---|---|
| `id` | UUIDv7 (time ordered) |
| `kind` | `standard` or `reversal {original}` |
| `status` | `pending` → `posted` or `voided`; or created directly as `posted` |
| `postings[]` | `{account_id, direction, amount > 0, currency}` |
| `description`, `reference`, `metadata` | free text (bounded), external reference, map |
| `created_sequence`, `created_at` | from the WAL envelope |
| `posted_sequence`, `posted_at`, `voided_at`, `void_reason`, `expires_at` | lifecycle |
| `reversed_amounts[]` | per posting, cumulative amount already reversed |
| `reversals[]` | ids of reversal transactions |
| `idempotency` | `{key, fingerprint}` when created with an idempotency key |

Invariant: for every currency, Σ debit amounts = Σ credit amounts.

## Events

Each event is wrapped in an envelope `{sequence, timestamp, event}`;
`sequence` starts at 1 and increases by exactly one; timestamps never go
backwards.

| `event.type` | Payload | Effect |
|---|---|---|
| `account_opened` | full account | insert account |
| `account_status_changed` | `account_id, from, to, reason` | set status |
| `account_metadata_updated` | `account_id, metadata` (full map) | replace metadata |
| `account_policy_changed` | `account_id, policy` | replace policy |
| `transaction_recorded` | full transaction (posted or pending) | post or reserve legs |
| `pending_posted` | `transaction_id` | move pending → posted |
| `pending_voided` | `transaction_id, reason, expired` | release pending |
| `transaction_reversed` | `original_id, reversal (full record), legs[]` | post flipped legs, update original |

Every touched account gets one history entry per leg and one version bump per
event.

## History entries

`{sequence, timestamp, transaction_id, kind, direction, amount,
posted_balance, available_balance}` — balances are *after* the entry, on the
normal side. Point-in-time queries binary-search this list by sequence or
timestamp.

## Idempotency record

`{key: "<api key id>:<client key>", fingerprint: sha256(method ‖ path ‖
canonical JSON body), response: {status, body}, created_at, expires_at}`.

## Audit record

`{sequence, timestamp, actor, action, resource, outcome, request_id, details,
prev_hash, hash}` with `hash = sha256(JSON of every field except hash, in a
fixed order)` and `prev_hash` of the first record = 64 zeros.
