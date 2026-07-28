# Contributing to Execution Ledger

Thanks for considering a contribution. This project is a Rust multi-service backend for durable job orchestration on Azure — see [README.md](README.md) for what it does and how the pieces fit together.

## Before you start

For anything beyond a small, obvious fix (typos, a clearly broken doc example), open an issue first to discuss the change. This avoids spending effort on a PR that doesn't fit the project's direction.

## Development setup

The fastest path to a working local environment is the devcontainer — see [Try it locally](README.md#try-it-locally-no-azure-account-needed) in the README. It provisions a local Postgres with the settings this project needs (including logical replication for the Drasi WAL reader) and wires the runtime environment variables.

## Making a change

1. Fork the repository and create a branch from `main`.
2. Keep changes focused — a PR should do one thing. Unrelated cleanup, formatting sweeps, or refactors belong in a separate PR.
3. Follow the existing code's conventions, error-handling patterns, and doc-comment style rather than introducing a new one.
4. Add or update tests for any behavior change. See [`crates/execution-ledger-domain/src/connectors/`](crates/execution-ledger-domain/src/connectors/) for the established test pattern: validation branches and no-op/compensate branches are tested directly (no mocking needed since they return before any network call); tests that exercise the actual behavior under test use `-> Result<(), E>` + `?` or a `matches!`/`map_or_else` comparison rather than `.unwrap()`.
5. Update documentation when behavior, configuration, or connector contracts change — including the relevant `connector-packs/*/runbook.md` if a connector pack is affected.

## Before opening a pull request

Run the same checks CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The workspace denies `unwrap()`, `expect()`, and `panic!()` outside item-level `#[allow(...)]` exceptions that carry a reason comment. An allowance is only appropriate over a provably infallible, literal construction (a hardcoded test fixture, a literal well-formed URL) — never over the actual `Result`/`Option` a test exists to exercise, and never at module or crate scope. Ask in your PR if unsure.

## Adding a connector

See [connector-packs/README.md](connector-packs/README.md#adding-a-new-connector) for the step-by-step guide: adding a `ConnectorKind` variant, implementing `execute()`/`compensate()`, and wiring dispatch in `crates/execution-ledger-domain/src/connectors/mod.rs`.

## Adding a connector pack

Connector packs are customer- or scenario-specific job templates and runbooks, kept separate from core runtime code. See [connector-packs/README.md](connector-packs/README.md) for the folder contract and [`connector-packs/example-finance-settlement-pack/`](connector-packs/example-finance-settlement-pack/) as a scaffold.

## Reporting bugs

Open a GitHub issue with: what you expected, what happened, the command or API call that triggered it, and relevant log output (with secrets/credentials redacted). For security vulnerabilities, see [SECURITY.md](SECURITY.md) instead of a public issue.

## Code of conduct

Be respectful and constructive. Disagreement about technical direction is normal and welcome; personal attacks are not.
