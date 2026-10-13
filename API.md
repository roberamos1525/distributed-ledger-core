# REST API

Base URL: `http://<host>:<port>`. All `/v1` endpoints require an API key:

```
Authorization: Bearer <key>        (or)        X-API-Key: <key>
```

Request bodies must be JSON (`Content-Type: application/json`) and are limited
to `server.max_body_bytes`. Every response carries `X-Request-Id` (echoed if
the client sent a valid one, generated otherwise).

## Conventions

**Amounts** are integers in the currency's minor unit (`1050` = 10.50 USD,
`1050` = 1050 JPY). Wherever a request accepts an amount you may also send a
decimal string in the account's currency (`"10.50"`); more decimals than the
currency allows is a `400`.

**Errors** always use this envelope:

```json
{"error": {"code": "insufficient_funds", "message": "insufficient funds in wallet: ...",
           "details": {"account_id": "wallet", "available": 1000, "required": 1500}}}
```

| Status | Codes |
|---|---|
| 400 | `validation_error` (malformed JSON, unknown fields, bad ids, bad cursors, invalid amounts, ...) |
| 401 | `unauthorized` |
| 403 | `forbidden` |
| 404 | `account_not_found`, `transaction_not_found`, `not_found` |
| 405 | `method_not_allowed` |
| 408 | `request_timeout` |
| 409 | `account_exists`, `version_conflict`, `invalid_transaction_state`, `already_reversed`, `transaction_expired`, `idempotency_key_reused`, `request_in_progress` |
| 413 | `payload_too_large` |
| 415 | `unsupported_media_type` |
| 422 | `insufficient_funds`, `unbalanced_transaction`, `currency_mismatch`, `account_not_active`, `account_not_empty`, `invalid_status_transition`, `invalid_reversal`, `arithmetic_overflow` |
| 500 | `storage_error`, `internal_error` |
| 503 | `read_only` |

**Idempotency**: send `Idempotency-Key: <1..255 visible ASCII chars>` on any
`POST` that creates or changes something (accounts, status changes,
transactions, post/void, reversals). Keys are scoped to the calling API key and
expire after `idempotency.ttl_secs`. A retry with the same key and body returns
the stored response with `Idempotent-Replayed: true`; the same key with a
different body returns `409 idempotency_key_reused`.

**Pagination**: list endpoints return `{"data": [...], "next_cursor": "..."|null}`.
Pass `cursor=<next_cursor>` to continue; `limit` is 1..500 (default 50).
Cursors are opaque.

## System

| Method & path | Auth | Description |
|---|---|---|
| `GET /health` | none | Liveness: `{"status":"ok"}` |
| `GET /ready` | none | `200 {"status":"ready","last_sequence":N}` or `503 {"status":"read_only"}` |
| `GET /metrics` | none (or reader if `metrics_public=false`) | Prometheus text format |
| `GET /v1/info` | reader | Stats, last recovery report, caller identity |
| `POST /v1/admin/snapshots` | admin | Write a snapshot now → `201` snapshot metadata |
| `POST /v1/admin/compactions` | admin | Snapshot + delete covered WAL segments and old snapshots |

## Accounts

### `POST /v1/accounts` (writer) → `201`

```json
{"id": "wallet:c-42", "name": "Wallet c-42", "type": "liability", "currency": "USD",
 "policy": {"kind": "overdraft", "limit": 5000}, "metadata": {"customer": "c-42"}}
```

`id` is optional (generated as `acc_<uuid>`; allowed characters
`A-Z a-z 0-9 : - _ .`, max 64). `type`: `asset | liability | equity | revenue
| expense`. `policy.kind`: `no_overdraft | overdraft (with limit) |
unrestricted`; defaults come from `ledger.default_policies`. Metadata: at most
32 entries, keys `[A-Za-z0-9_.-]{1,64}`, values up to 512 bytes.

Response: the account with its balance:

```json
{"id": "wallet:c-42", "name": "Wallet c-42", "account_type": "liability", "currency": "USD",
 "status": "open", "policy": {"kind": "overdraft", "limit": 5000}, "metadata": {...},
 "version": 1, "created_at": "...", "created_sequence": 7, "updated_at": "...",
 "balance": {"account_id": "wallet:c-42", "currency": "USD", "posted": 0, "available": 0,
             "pending_debits": 0, "pending_credits": 0, "as_of_sequence": null,
             "as_of_time": null, "version": 1}}
```

### `GET /v1/accounts` (reader)

Query: `type`, `status`, `currency`, `cursor`, `limit`. Ordered by id.

### `GET /v1/accounts/{id}` (reader)

### `POST /v1/accounts/{id}/status` (admin)

```json
{"status": "frozen", "expected_version": 3, "reason": "kyc review"}
```

Transitions: open → frozen, frozen → open, open/frozen → closed. Closing
requires zero posted balance and no pending amounts. Frozen accounts reject new
postings and pending commits; voids are still allowed.

### `PATCH /v1/accounts/{id}/metadata` (writer)

```json
{"set": {"tier": "gold"}, "remove": ["region"], "expected_version": 4}
```

### `PUT /v1/accounts/{id}/policy` (admin)

```json
{"policy": {"kind": "unrestricted"}, "expected_version": 5}
```

### `GET /v1/accounts/{id}/balance` (reader)

Optional `as_of_sequence=<event sequence>` **or** `as_of_time=<RFC 3339>` for
a point-in-time balance. Balances are on the account's normal side (positive
= the account increased). `available` = posted minus pending amounts that would
reduce it. The response adds `posted_decimal` and `available_decimal`.

### `GET /v1/accounts/{id}/history` (reader)

Query: `cursor`, `limit`, `order=asc|desc`. Each entry:

```json
{"sequence": 12, "timestamp": "...", "transaction_id": "...", "kind": "posted",
 "direction": "credit", "amount": 2500, "posted_balance": 7500, "available_balance": 7500}
```

`kind`: `posted | pending_created | pending_posted | pending_voided | reversal`.

## Transactions

### `POST /v1/transactions` (writer) → `201` (or `200` on engine-level replay)

```json
{
  "description": "Top-up with fee",
  "reference": "TOPUP-1",
  "metadata": {"channel": "card"},
  "mode": "posted",
  "pending_ttl_secs": null,
  "expected_versions": {"wallet:c-42": 4},
  "postings": [
    {"account_id": "bank", "direction": "debit", "amount": "102.50"},
    {"account_id": "wallet:c-42", "direction": "credit", "amount": 10000, "currency": "USD"},
    {"account_id": "fees", "direction": "credit", "amount": 250}
  ]
}
```

Rules: 2..`max_postings` legs, positive amounts up to `max_amount`, debits
equal credits **per currency**, every account open, optional `currency`
assertion must match the account, every `expected_versions` entry must match
and must reference an account in the postings, balance policies must hold
after the transaction (pending reservations included). `mode: "pending"`
reserves the amounts; `pending_ttl_secs` makes the reservation expire (it is
voided automatically by the background sweeper).

Response: the transaction record (`id`, `kind`, `status`, `postings`,
`created_sequence`, `posted_at`, `expires_at`, `reversed_amounts`,
`reversals`, ...).

### `GET /v1/transactions` (reader)

Query: `status=pending|posted|voided`, `account_id`, `reference`, `cursor`,
`limit`. Ordered by creation sequence.

### `GET /v1/transactions/{id}` (reader)

### `POST /v1/transactions/{id}/post` (writer)

Commits a pending transaction. `409 invalid_transaction_state` if it is not
pending, `409 transaction_expired` if past its expiry.

### `POST /v1/transactions/{id}/void` (writer)

Optional body `{"reason": "customer cancelled"}`. Releases the reservation.

### `POST /v1/transactions/{id}/reversals` (writer) → `201`

Empty body: reverse everything not yet reversed. Partial:

```json
{"legs": [{"posting_index": 0, "amount": "10.00"}, {"posting_index": 1, "amount": 1000}],
 "description": "partial refund", "reference": "RMA-7"}
```

The selected legs are flipped and must balance per currency. Each leg's
cumulative reversed amount cannot exceed the original. Fully reversed →
`409 already_reversed`; reversing a reversal or a non-posted transaction is
rejected. The new transaction has `kind: {"type": "reversal", "original": "<id>"}`.

## Reports

### `GET /v1/reports/trial-balance` (reader)

Per currency: debit/credit balance per account, totals, net balance per
account type, `balanced` flag.

### `GET /v1/reports/invariants` (reader)

`200` with `{"ok": true, ...}` when all invariants hold, `500` with the list of
`violations` otherwise. Checks: posted balances sum to zero per currency,
pending debits equal pending credits per currency, account pending counters
match pending transactions, newest history entry matches balances, every
transaction balances and references existing accounts in their currency,
reversal bookkeeping is consistent.

## Reconciliation

### `POST /v1/reconciliations` (writer)

```json
{"account_id": "bank", "format": "csv",
 "data": "date,amount,reference,description\n2026-03-02,1250.00,INV-1001,Payment\n",
 "date_tolerance_days": 2, "amount_tolerance": "0.01",
 "from": "2026-03-01", "to": "2026-03-31"}
```

CSV needs `date` (YYYY-MM-DD) and `amount` columns; `reference` and
`description` are optional; column order and case do not matter. JSON
statements look like `examples/statement.json` (`amount` as decimal string or
`amount_minor`, optional `currency`, `opening_balance`, `closing_balance`).
Statement amounts are signed from the account's point of view (positive =
the account's normal balance increases).

Matching: first by reference (transaction reference or transaction id) —
pairs outside tolerance become `discrepancies` (`amount_mismatch`,
`date_out_of_window`, `amount_and_date_mismatch`); then by amount and date
proximity. The response contains `status` (`reconciled` | `discrepancies`),
`summary`, `matched`, `discrepancies`, `unmatched_statement`,
`unmatched_ledger` and, if a closing balance was supplied, `balance_check`.

## Audit

### `GET /v1/audit` (admin)

Query: `after=<sequence>`, `limit`. Records:

```json
{"sequence": 5, "timestamp": "...", "actor": "payments-svc", "action": "transaction.create",
 "resource": "0190f...", "outcome": "success", "request_id": "...", "details": {...},
 "prev_hash": "<sha256 of record 4>", "hash": "<sha256 of this record>"}
```

Actions: `account.open`, `account.status`, `account.metadata`,
`account.policy`, `transaction.create`, `transaction.post`,
`transaction.void`, `transaction.reverse`, `reconciliation.run`,
`admin.snapshot`, `admin.compact`. Rejected attempts are recorded with
`outcome: "rejected:<code>"`.

### `GET /v1/audit/verify` (admin)

Re-reads the audit file and recomputes the chain. `200 {"ok": true, "records": N,
"last_hash": ...}` or `409 {"ok": false, "first_invalid_sequence": K, "error": ...}`.
