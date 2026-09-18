---
name: kt-release
description: Start the Hekma release flow (the kt-release name predates the v0.8.0 Hekma rename) from the repository root. Use when Codex is asked to prepare, cut, tag, or publish a Hekma release by inferring the next semver bump from unreleased git history, updating Cargo.toml and Cargo.lock, running required Rust checks, committing the version bump with sign-off, pushing the release preparation commit, and pushing the matching v* tag. Patch and minor releases run automatically; major releases require clear user confirmation before any release mutation proceeds.
---

# Kt Release

## Overview

Automate the Hekma release trigger flow while preserving the repository's tag-driven release process. The helper script infers patch/minor/major from commit history since the latest semver tag, bumps Cargo metadata, validates, commits, pushes `main`, then pushes the lightweight release tag.

## Workflow

1. Start at the Hekma repository root (github.com/Ktesio/ktesio until the gated repo rename).
2. Run a dry run first:

```bash
python3 .agents/skills/kt-release/scripts/prepare_kt_release.py --dry-run
```

3. If the dry run reports `major`, stop and ask the user to clearly confirm the exact target tag before proceeding.
4. For patch or minor releases, proceed without asking:

```bash
python3 .agents/skills/kt-release/scripts/prepare_kt_release.py
```

5. For a confirmed major release only, rerun with the confirmation flag:

```bash
python3 .agents/skills/kt-release/scripts/prepare_kt_release.py --confirm-major
```

## Inference Rules

- Compare commits from the latest merged `vMAJOR.MINOR.PATCH` tag to `HEAD`.
- Infer `major` when any unreleased commit uses a Conventional Commit breaking marker (`type!:` or `type(scope)!:`) or a `BREAKING CHANGE:` / `BREAKING-CHANGE:` footer.
- Infer `minor` when any unreleased commit is `feat` and no breaking marker is present.
- Infer `patch` for all other releasable changes.
- If no unreleased commits exist, do not release.

## Safety Rules

- Do not ask questions for patch or minor releases.
- Do ask for explicit user confirmation before running the non-dry-run command for a major release. Name the exact target tag in the confirmation request.
- Do not bypass script failures. If the helper refuses because the checkout is dirty, not on `main`, behind/diverged from `origin/main`, or failing checks, report the blocking output.
- Expect network-affecting commands (`git fetch`, `git push`) and full Rust checks. Request sandbox escalation when required by the current environment.
- Do not manually create the tag if the script already failed before tag creation.

## Helper Script

Use `scripts/prepare_kt_release.py` as the source of truth for the release sequence. It performs these steps:

- Validate repository identity and clean release state.
- Fetch `origin main` and tags.
- Infer the release kind and target version.
- Update the root `Cargo.toml` (`[workspace.package]` version, inherited by the `hekma` package in `crates/hekma/`) and `Cargo.lock`.
- Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --all-targets`.
- Commit `Cargo.toml` and `Cargo.lock` with `chore(release): bump version to X.Y.Z` and `--signoff`.
- Push `HEAD:main`.
- Create and push the lightweight `vX.Y.Z` tag.

Use `--dry-run` when inspecting the result, `--confirm-major` only after the user confirms a major release, and `--skip-checks` only if the user explicitly asks to skip validation.
