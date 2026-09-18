---
title: GitHub Repository Audit Checklist
description: Repository hardening checks and evidence for the public Hekma GitHub project.
---

# GitHub Repository Audit Checklist

Status date: 2026-07-01.

This checklist tracks repository-level hardening for the public `Ktesio/hekma`
repository. Treat the GitHub API, workflow results, and repository files as the
source of truth.

## Repository Settings

- [x] Repository is public and default branch is `main`.
- [x] Issues are enabled.
- [x] Wiki is disabled.
- [x] Discussions are enabled for community Q&A.
- [x] Projects are disabled until a public roadmap project is intentionally used.
- [x] Forking remains enabled for community contribution.
- [ ] Web commit sign-off requirement removed (DCO retired in favor of the CLA; disable this in repository settings).
- [x] Delete branch on merge is enabled.
- [x] Auto-merge and update-branch support are enabled.
- [x] Merge commits and rebase merges are disabled.
- [x] Squash merge is the only allowed merge method.
- [ ] Repository license is source-available (Ktesio Noncommercial-Attribution License 1.0.0, a custom license); GitHub shows it as "Other" / a non-standard license.

Evidence:

- `gh api repos/Ktesio/hekma`
- `gh api repos/Ktesio/hekma/license`
- Observed: `license.spdx_id` is `null` and GitHub displays the custom license as "Other"; the fetched `LICENSE` is titled "Ktesio Noncommercial-Attribution License 1.0.0" (PolyForm Noncommercial 1.0.0 terms plus the Attribution condition).

## Branch And Tag Rulesets

- [x] Default branch ruleset is active.
- [x] Default branch cannot be deleted.
- [x] Default branch cannot be force-pushed.
- [x] Pull requests are required for default branch changes.
- [x] At least one approving review is required.
- [x] Stale approvals are dismissed after new pushes.
- [x] Code owner review is required.
- [x] Last push approval is intentionally disabled until the project has at
  least two maintainers.
- [x] Review conversations must be resolved.
- [x] Required status checks are strict.
- [ ] Required checks include `fmt`, `clippy`, `test`, `build`, `docs`, and `coverage` (remove the retired `dco` check from the branch ruleset).
- [x] CodeQL code scanning is required for high-or-higher security alerts and errors.
- [x] Code quality errors are blocked.
- [x] Release tag ruleset is active for `v*` tags.
- [x] Release tags cannot be deleted or force-pushed.
- [x] Release tag creation is restricted through the ruleset with maintainer bypass.

Evidence:

- `gh api repos/Ktesio/hekma/rulesets`
- `gh api repos/Ktesio/hekma/rulesets/17082021`
- `gh api repos/Ktesio/hekma/rulesets/17082811`

Note: last push approval is useful when another maintainer can approve a
maintainer-pushed fix. With only one direct maintainer, it turns normal reviewed
contribution flow into routine admin bypass.

## GitHub Actions

- [x] Actions are enabled.
- [x] Allowed Actions are restricted to selected actions.
- [x] SHA pinning is required.
- [x] GitHub-owned actions are allowed.
- [x] No third-party release-packaging action is required.
- [x] Default workflow token permissions are read-only.
- [x] Workflows request write permissions only where needed.
- [x] Third-party workflow actions are pinned by SHA.
- [x] CI validates formatting, clippy, tests, build, docs, and coverage.
- [x] Release workflow uses the protected `release` environment.

Evidence:

- `gh api repos/Ktesio/hekma/actions/permissions`
- `gh api repos/Ktesio/hekma/actions/permissions/selected-actions`
- `gh api repos/Ktesio/hekma/actions/permissions/workflow`
- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`

## Security

- [x] Security policy exists.
- [x] Private vulnerability reporting is enabled.
- [x] Dependabot vulnerability alerts are enabled.
- [x] Dependabot security updates are enabled.
- [x] Secret scanning is enabled.
- [x] Secret scanning push protection is enabled.
- [x] CodeQL default setup is configured for Actions, Python, and Rust.
- [x] Code scanning alerts are currently clear.
- [x] Secret scanning alerts are currently clear.
- [x] Dependabot alerts are currently clear.

Evidence:

- `gh api repos/Ktesio/hekma/private-vulnerability-reporting`
- `gh api -i repos/Ktesio/hekma/vulnerability-alerts`
- `gh api repos/Ktesio/hekma/automated-security-fixes`
- `gh api repos/Ktesio/hekma/code-scanning/default-setup`
- `gh api repos/Ktesio/hekma/code-scanning/alerts`
- `gh api repos/Ktesio/hekma/secret-scanning/alerts`
- `gh api repos/Ktesio/hekma/dependabot/alerts`

## Community Health Files

- [x] `README.md` exists.
- [x] `LICENSE` exists and contains the Ktesio Noncommercial-Attribution License 1.0.0 text (PolyForm Noncommercial 1.0.0 terms plus the Attribution condition) with a commercial-use notice.
- [x] `SECURITY.md` exists.
- [x] `CONTRIBUTING.md` exists.
- [x] `CODE_OF_CONDUCT.md` exists.
- [x] `CLA.md` exists.
- [x] `TRADEMARK.md` exists.
- [x] `SUPPORT.md` exists.
- [x] `SPONSORS.md` exists.
- [x] `.github/CODEOWNERS` exists.
- [x] `.github/pull_request_template.md` exists.
- [x] Issue forms exist for bugs, features, and questions.
- [x] `.github/FUNDING.yml` exists.
- [x] Community health profile reports 100%.

Evidence:

- `gh api repos/Ktesio/hekma/community/profile`
- `gh api repos/Ktesio/hekma/contents/.github/ISSUE_TEMPLATE`
- Local files in the repository root and `.github/`

## Dependency And Maintenance Automation

- [x] Dependabot config exists for Cargo.
- [x] Dependabot config exists for GitHub Actions.
- [x] Dependabot labels exist: `dependencies`, `rust`, and `github-actions`.
- [x] Maintenance labels exist for areas, security, breaking changes, and repro needs.
- [x] Dependabot PR #8 was merged for GitHub Actions updates.
- [x] Dependabot PR #9 was merged for Cargo updates.
- [x] Dependabot PR #9 clippy compatibility fix was included before merge.

Evidence:

- `.github/dependabot.yml`
- `gh pr list --repo Ktesio/hekma --state open`
- `gh api repos/Ktesio/hekma/labels`

## Release Readiness

- [x] `release` environment exists.
- [x] `release` environment requires `iMagdy` approval.
- [x] Homebrew tap variables are configured.
- [ ] `CARGO_REGISTRY_TOKEN` is configured as a `release` environment secret.
- [ ] `HOMEBREW_TAP_TOKEN` is configured as a `release` environment secret.

Evidence:

- `gh api repos/Ktesio/hekma/environments`
- `gh api repos/Ktesio/hekma/environments/release/secrets`
- `gh api repos/Ktesio/hekma/actions/variables`

The unchecked release secrets are intentionally not filled with placeholder
values. They require real credentials with publish access to crates.io and the
Homebrew tap repository.

## Verification Commands

- [x] `cargo fmt --check`
- [x] `cargo clippy --all-targets -- -D warnings`
- [x] `cargo test --all-targets`
- [x] `python3 scripts/check_docs.py`
- [x] Latest `main` CI and CodeQL checks are green.

Evidence:

- Local command output from the hardening work.
- `gh api repos/Ktesio/hekma/commits/main/check-runs`
