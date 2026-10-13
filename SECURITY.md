# Security

## Reporting a vulnerability

Please report suspected vulnerabilities privately to the maintainer (Robert
Ramos) rather than opening a public issue. Include a description, affected
version/commit, reproduction steps and impact. You can expect an initial
response within a few working days.

## Security model

| Concern | Implementation |
|---|---|
| Authentication | Static API keys (`ldg_` + 32 random bytes from the OS RNG). Only SHA-256 hashes are configured; presented keys are hashed and compared in constant time against every configured hash. |
| Authorization | Three ordered roles (`reader` < `writer` < `admin`) enforced per endpoint; failures are counted in `ledger_auth_failures_total`. |
| Secrets | Keys come from `LEDGER_API_KEYS` or the config file (hashes only). `.env` is git-ignored; `.env.example` contains placeholders only. `ledgerctl check-config` redacts hashes. |
| Input validation | Strict JSON DTOs (`deny_unknown_fields`), validated ids, bounded metadata and text fields, exact decimal parsing, bounded page sizes, statement line limits, request body size limit (default 1 MiB), request timeout. |
| Error hygiene | Storage errors are returned as a generic `internal storage error`; file paths and OS errors are only logged. |
| Integrity | CRC32 on every WAL / snapshot / audit / idempotency record; SHA-256 hash chain on the audit trail with a verification endpoint; invariant checks endpoint and CLI. |
| Auditability | Every mutating API call (successful or rejected) is recorded with actor, action, resource, outcome and request id. |
| Process isolation | Exclusive OS lock on the data directory prevents two writers. The Docker image runs as a non-root user with a read-only root filesystem in `compose.yaml` (Docker artifacts are untested locally). |

## Known limitations

- No TLS: terminate TLS in a reverse proxy or service mesh.
- No key rotation API, key expiry or rate limiting; rotate keys by changing
  configuration and restarting.
- The audit chain detects tampering but cannot prevent an attacker with write
  access to the data directory from rewriting the whole chain consistently.
  Ship audit records or periodic `last_hash` values to external storage if
  that threat matters.
- CRC32 detects accidental corruption, not malicious modification of the WAL.
- `/metrics` is public by default (`server.metrics_public = true`); disable it
  if the port is reachable from untrusted networks.
