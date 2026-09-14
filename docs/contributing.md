---
title: Contributing Guide
description: Development setup, contribution workflow, pull request expectations, and docs update guidance.
---

# Contributing Guide

This page is the hands-on development guide. For project rules and the Contributor License Agreement, see [../CONTRIBUTING.md](../CONTRIBUTING.md) and [../CLA.md](../CLA.md).

## Setup

```bash
git clone https://github.com/iMagdy/ktesio.git
cd ktesio
cargo build
cargo test --workspace --all-targets
```

A bare `cargo` builds and tests against the repo's MSRV pin (`rust-toolchain.toml` → Rust 1.96.1) — no `cargo +1.96.1` typing needed; CI's latest-stable jobs run `cargo +stable`. Note that mise/asdf shim `cargo` via `RUSTUP_TOOLCHAIN` and override the pin — see [testing.md](testing.md) ("Toolchain") if bare `cargo --version` does not report 1.96.1.

## Development Loop

```bash
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
python3 scripts/check_docs.py
```

The commands above run under the same MSRV pin (1.96.1 via `rust-toolchain.toml`), while CI's "latest stable" jobs lint and test with `cargo +stable` — an occasional new-stable clippy or rustfmt nit can appear in CI that your local MSRV run did not (reproduce with `cargo +stable clippy …`). See [testing.md](testing.md) ("Toolchain") for the full split and the mise/asdf override caveat.

## Adding CLI Behavior

- Update `crates/kt/src/main.rs` command parsing.
- Add or update a module under `crates/kt/src/cli/`.
- Add unit tests for command logic with explicit project roots.
- Add integration tests under `crates/kt/tests/` for user-facing workflows.
- Update [commands.md](commands.md) and [get-started.md](get-started.md) when behavior changes.

## Test Fixtures

Integration tests use local temporary git repositories through `crates/kt/tests/helpers/mod.rs`. Avoid network-only tests in the default suite.

## Pull Requests

- Keep changes focused.
- Use conventional commit messages.
- By opening a pull request, you agree to the [Contributor License Agreement](../CLA.md).
- Include docs and tests in the same change when behavior changes.
- Make sure CI passes before requesting review.
- If a review defers a finding as "unreachable", the deferral must be proven across every shipped surface (CLI + library + cross-lifetime) at test-grade evidence — see "Engineering patterns" in [AGENTS.md](../AGENTS.md).
