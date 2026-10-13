# Contributing

Thanks for your interest in improving distributed-ledger-core.

## Workflow

1. Create a branch from `main`.
2. Make focused changes with tests.
3. Run the full local check before opening a pull request:

   ```bash
   cargo fmt --all
   cargo clippy --all-targets -- -D warnings
   cargo test --all-targets
   cargo bench --no-run
   ```

4. Describe the behaviour change and how it was verified in the PR.

## Ground rules

- **Money is integer minor units.** Never introduce floating point for
  amounts. Use checked arithmetic; overflow must surface as an error.
- **State changes only through events.** New behaviour that changes state
  needs a `LedgerEvent` variant (or reuses one) and must be applied in
  `engine::state::apply_event`, so replay stays identical to live execution.
  Add a replay-equality assertion to the tests.
- **Respect lock ordering**: commit gate → one transaction lock → account
  locks (sorted, via `LockedAccounts::acquire`) → WAL mutex.
- **Event compatibility.** Events are persisted forever. Add fields with
  `#[serde(default)]`; never rename or remove fields or variants without a
  migration plan.
- **Errors** are typed (`LedgerError`, `StorageError`) with stable `code()`
  strings; the API maps them in `api::error`. Adding a code is an API change
  — document it in `API.md`.
- **Validate at the boundary**: request DTOs use `deny_unknown_fields`, and
  every external value (ids, amounts, metadata, cursors, statements) is
  validated before it reaches the engine.
- Keep modules wired in and tested; no dead code or placeholder scaffolding.

## Tests

- Unit tests live beside the code (`#[cfg(test)]`).
- `tests/` holds integration suites (HTTP API, recovery, concurrency,
  property tests, CLI). Shared helpers are in `tests/common/mod.rs`.
- Tests must not depend on wall-clock timing beyond generous timeouts and must
  use temporary directories.

## Commit messages

Use the imperative mood ("Add partial reversal validation"), explain *why* in
the body when it is not obvious, and reference issues where relevant.
