#!/usr/bin/env bash
# End-to-end demo: generates throwaway API keys, starts ledger-server on a
# temporary data directory, exercises the main endpoints with curl, then
# stops the server. Requires: cargo, curl. Optional: jq for pretty output.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --quiet --bins

BIN=./target/debug
DATA=$(mktemp -d)
PORT=${PORT:-18080}
BASE="http://127.0.0.1:${PORT}"

pretty() {
  if command -v jq >/dev/null; then jq .; else cat; echo; fi
}

# Prints "<plaintext key> <sha256>" for a new key.
new_key() {
  local out
  out=$("$BIN/ledgerctl" hash-key --role "$1" --id "$2")
  local key hash
  key=$(printf '%s\n' "$out" | grep '"key":' | cut -d'"' -f4)
  hash=$(printf '%s\n' "$out" | grep '"key_sha256":' | cut -d'"' -f4)
  echo "$key $hash"
}

read -r ADMIN ADMIN_HASH <<<"$(new_key admin ops)"
read -r WRITER WRITER_HASH <<<"$(new_key writer app)"

export LEDGER_DATA_DIR="$DATA"
export LEDGER_BIND="127.0.0.1:${PORT}"
export LEDGER_FSYNC=never
export LEDGER_LOG_FORMAT=pretty
export LEDGER_API_KEYS="ops:admin:${ADMIN_HASH},app:writer:${WRITER_HASH}"

"$BIN/ledger-server" &
SERVER=$!
cleanup() {
  kill "$SERVER" 2>/dev/null || true
  wait "$SERVER" 2>/dev/null || true
  rm -rf "$DATA"
}
trap cleanup EXIT

for _ in $(seq 1 50); do
  curl -fs "$BASE/health" >/dev/null && break
  sleep 0.1
done

w() { curl -s -H "Authorization: Bearer $WRITER" -H "Content-Type: application/json" "$@"; }
a() { curl -s -H "Authorization: Bearer $ADMIN" -H "Content-Type: application/json" "$@"; }

echo "== open accounts"
w -X POST "$BASE/v1/accounts" -d '{"id":"bank","name":"Operating bank","type":"asset","currency":"USD"}' | pretty
w -X POST "$BASE/v1/accounts" -d '{"id":"capital","name":"Owner capital","type":"equity","currency":"USD"}' >/dev/null
w -X POST "$BASE/v1/accounts" -d '{"id":"sales","name":"Sales","type":"revenue","currency":"USD"}' >/dev/null

echo "== fund (idempotent) and sell"
FUND='{"description":"Initial capital","postings":[{"account_id":"bank","direction":"debit","amount":"5000.00"},{"account_id":"capital","direction":"credit","amount":"5000.00"}]}'
w -X POST "$BASE/v1/transactions" -H "Idempotency-Key: fund-1" -d "$FUND" | pretty
w -i -X POST "$BASE/v1/transactions" -H "Idempotency-Key: fund-1" -d "$FUND" | grep -i "idempotent-replayed" || true
w -X POST "$BASE/v1/transactions" -d '{"reference":"INV-1001","postings":[{"account_id":"bank","direction":"debit","amount":"1250.00"},{"account_id":"sales","direction":"credit","amount":"1250.00"}]}' >/dev/null

echo "== balance and trial balance"
w "$BASE/v1/accounts/bank/balance" | pretty
w "$BASE/v1/reports/trial-balance" | pretty

echo "== audit chain"
a "$BASE/v1/audit/verify" | pretty
