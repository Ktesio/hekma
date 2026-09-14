---
title: Release Process
description: How Ktesio release tags build binaries, publish crates, update Homebrew, and refresh release documentation.
---

# Release Process

Ktesio releases are driven by git tags.

## Tag Format

Use semantic version tags:

```text
vMAJOR.MINOR.PATCH
```

Example:

```bash
git tag v0.1.0
git push origin v0.1.0
```

## What the Tag Workflow Does

When a `v*` tag is pushed, `.github/workflows/release.yml`:

1. Builds Tier 1 CLI binaries for macOS Intel, macOS Apple Silicon, Windows x64, and Linux x64.
2. Archives each binary with a deterministic file name.
3. Generates per-asset `.sha256` files and one aggregate checksum file.
4. Creates a draft GitHub Release for the tag.
5. Uploads all release assets.
6. Publishes the `ktesio` crate to crates.io.
7. Publishes the GitHub Release with a clean asset table.
8. Updates the Homebrew tap formula for macOS Intel, macOS Apple Silicon, and Linux x64.
9. Opens a pull request updating `CHANGELOG.md` and `docs/RELEASE_NOTES.md`.

The docs PR happens after the tag because a tag points at an existing commit. The release page is updated immediately; repository docs are refreshed through the follow-up pull request.

## crates.io

Release tags publish the crate package to crates.io as:

```text
ktesio
```

The installed binary is `kt`, so users can install it with:

```bash
cargo install ktesio
```

Configure this repository secret before publishing a tag:

- `CARGO_REGISTRY_TOKEN`: crates.io API token with publish access to the `ktesio` crate.

The workflow verifies that `Cargo.toml` version matches the tag without the leading `v`. If the crate version is already published, the workflow skips the publish step so release reruns stay safe.

## Publishing the Engine Crates

Story 7-4 prepared everything needed to publish the embedding crates
(`ktesio-engine` + `ktesio-adapter-api`, plus the engine's builtin-adapter
dependency `ktesio-adapters-hermes`) and executed none of it. **Every HOLD
gate below opens only on Islam's explicit go** (the standing non-negotiable:
no deployments, no releases, no tags, nothing that costs money). Until that
go, all four internal crate manifests still carry `publish = false` (pinned
by `scripts/test_automation.py` — an accidental flip fails CI), hosts pin the
git dependency per [Embedding the engine](embedding.md), and this page is the
whole of the publish machinery.

### The go protocol

The go is Islam's EXPLICIT statement on the epic's release-tracking issue or
PR — a drive-by "ship it" elsewhere does not open these gates. The operator
executing this runbook records the go by linking that statement in the
decision log at the end of this section, then works top-to-bottom. Any
failure that stops the chain closes the go: resuming requires
re-confirmation.

### Preconditions (all must hold before the go)

1. CI green on `main`: fmt, clippy, the 3-OS test matrix, boundary, semver,
   coverage, docs.
2. **Versions set and concrete.** The release version bump is a real commit:
   the root `[workspace.package]` `version` (inherited by the `ktesio` CLI
   crate) bumped to the release version, the crate-level versions confirmed
   (`0.1.0` today for `ktesio-adapter-api`, `ktesio-adapters-hermes`,
   `ktesio-engine`), and `CHANGELOG.md` / `docs/RELEASE_NOTES.md` current for
   everything shipping. The tag in step 6 goes ON that release commit.
3. **Registry identity.** `cargo login` has been run locally with an account
   that will own the `ktesio-*` crate names (this is what the manual `cargo
   publish` steps authenticate with; `CARGO_REGISTRY_TOKEN` is the CI
   secret the tag workflow uses), and the one-time name-availability check
   below returned 404 (available) for all three.
4. **Dependency chain closed.** Every NORMAL dependency of the three
   publishable crates is either external (already on crates.io) or named in
   this chain. Today that is exactly `ktesio-adapter-api` +
   `ktesio-adapters-hermes` (both in the chain). If a new internal crate has
   since become a normal dependency, add it to the chain AHEAD of its
   dependents — the recovery is always "publish the missing dependency
   first", never a `--allow` style bypass.
5. Repository settings in place: `CARGO_REGISTRY_TOKEN` (per
   [crates.io](#cratesio)) and the Homebrew tap settings per
   [Homebrew](#homebrew).
6. The semver gate green against the in-repo freeze baselines (adapter-api,
   engine) in CI.

One-time name-availability check (expect `404` = available today; a `200`
means the name is TAKEN — stop and reconcile before anything else):

```bash
for crate in ktesio-adapter-api ktesio-adapters-hermes ktesio-engine; do
  printf '%s: ' "$crate"
  curl -s -o /dev/null -w '%{http_code}\n' "https://crates.io/api/v1/crates/$crate"
done
```

### Safe rehearsals (purely local, allowed any time)

```bash
cargo package --list -p ktesio-adapter-api
cargo package --list -p ktesio-adapters-hermes
cargo package --list -p ktesio-engine
```

`cargo package --list` builds nothing remote and contacts no registry. Even
`cargo publish --dry-run` is HELD under the standing instruction — do not run
it against the real registry without the go.

### The ordered publish-day commands

Order matters (precondition 4): crates.io refuses a package whose normal
dependencies are not already published, so the chain is adapter-api →
adapters-hermes → engine, each reviewed, then a from-crates.io host probe
BEFORE the tag (which releases the `ktesio` CLI crate, the binaries, the
GitHub Release, and the Homebrew tap update in one automated sweep).

**Step 0 — flip the publish flags (part of the same go).** Remove
`publish = false` from `crates/ktesio-adapter-api/Cargo.toml`,
`crates/ktesio-adapters-hermes/Cargo.toml`, and
`crates/ktesio-engine/Cargo.toml`, and update the three PUBLISH-HELD
comments, in the release commit from precondition 2.
(`ktesio-conformance` KEEPS its flag — it is the dev/test kit, nothing
published depends on it, and publishing it is a separate, undecided step.)
Note: this flip intentionally FAILS the `test_automation.py` hold-pin until
it lands as this step — that is the pin working.

**Steps 1–3 — the publishes (each IRREVERSIBLE).** crates.io versions are
immutable: there is no true delete, only yank. Before EACH publish, build and
review the exact tarball that would be uploaded:

```bash
cargo package --no-verify -p ktesio-adapter-api
tar -tzf target/package/ktesio-adapter-api-0.1.0.crate
```

(Read the version off the manifest; extract the `.crate` — a plain
`.tar.gz` — and review the file list and metadata: license file present, no
stray files, version and description match.) Release-surface changes (version bumps, RELEASE_NOTES/changelog entries, crates.io metadata) get the TWO-PASS review treatment — see "Two-pass review covers the release surface (AI-55)" in `AGENTS.md`. Then, one at a time, each
**HOLD — requires Islam's explicit go**:

1. `cargo +stable publish --locked -p ktesio-adapter-api`
2. `cargo +stable publish --locked -p ktesio-adapters-hermes` (a normal
   dependency of the engine — step 3 fails without it)
3. `cargo +stable publish --locked -p ktesio-engine`

A transient registry error: re-running that exact step is safe (crates.io
rejects duplicates).

**Step 4 — POST-PUBLISH VERIFY, before the tag (cheap and decisive).** Prove
the uploaded crate resolves for a real host from a FRESH out-of-tree project
with no git/path override:

```bash
cargo new /tmp/ktesio-host-probe
cd /tmp/ktesio-host-probe
cargo add ktesio-engine@0.1.0
cargo build
```

Only when this build pulls `ktesio-engine` from crates.io and compiles does
the chain proceed — the tag below fires release automation, and it must never
fire ahead of a broken publish. Same go; recorded checkpoint.

**Step 5 — POST-PUBLISH DOCS flip (the docs-currency gate).** On `main`,
in the same release-commit series: switch [Embedding the
engine](embedding.md)'s dependency form from the git pin to the published
version line (`ktesio-engine = "0.1"`) and rewrite its Availability section
from held to published; update the ONE remaining PUBLISH-HELD comment
(`ktesio-conformance`'s, whose flag stays); move the held-block banners in
`CHANGELOG.md` / `docs/RELEASE_NOTES.md` into their release sections per
their placement notes. **HOLD — requires Islam's explicit go.**

**Step 6 — the tag.** `git tag vX.Y.Z && git push origin vX.Y.Z` on the
release commit — arms release automation ([What the Tag Workflow
Does](#what-the-tag-workflow-does)). **HOLD — requires Islam's explicit go.**

**Step 7 — verify the Homebrew tap formula landed** for the new tag
(automated by the tag workflow; [Homebrew](#homebrew) lists the secrets it
needs). **HOLD — requires Islam's explicit go.** If it did NOT land:
re-run the release workflow first (its crates.io publish step skips
already-published artifacts, so reruns are safe); only if that fails, render
the formula with `python3 scripts/generate_homebrew_formula.py` and push it
to `iMagdy/homebrew-tap` manually, recording the manual push in the decision
log.

### The semver-gate flip at first publish

Once each crate exists on crates.io, the CI `semver` job's per-crate loop
stops skipping it (404 → 200) and release-to-release checking arms
automatically — no workflow edit needed. The recorded default for the in-repo
freeze baselines (adapter-api at the contract-v1 freeze, the engine at the
embedding-surface freeze): **KEEP them as fast pre-publish guards** — they
run on every push and depend on no registry — and revisit (retire or keep)
deliberately at the SECOND published release, once the crates.io baseline
has proven itself. The decision lives in this section's decision log; never
widen a gate or an allowlist to make a baseline pass.

### Decision log

- **Publish go:** GRANTED 2026-09-09 — Islam's explicit go recorded on the release-tracking issue
  [#176](https://github.com/iMagdy/ktesio/issues/176) ("GO — execute steps 0–7 now", libs 0.1.0 +
  kt/tag v0.7.0, #168 dispositions ratified in the same decision round). Steps 0–7 executed by the
  orchestrator same day. The one-time name check returned 200 for all three names — reconciled as
  Islam's OWN v0.0.1 placeholder reservations (created 2026-07-03, owners verified = iMagdy), so
  the publishes supersede his own placeholders.
- **Step 7 tap push — MANUAL (2026-09-09):** the workflow's tap checkout failed on auth (the
  `HOMEBREW_TAP_TOKEN` secret no longer fetches `iMagdy/homebrew-tap` — expired/rotated token;
  renew the secret before the next release). Executed the documented fallback: formula rendered
  locally from the v0.7.0 checksums and pushed manually to `iMagdy/homebrew-tap` (53dadf0).
  Everything else in steps 0–7 ran clean; `ktesio` 0.7.0 and the three library crates are live on
  crates.io; release v0.7.0 is published with all platform binaries.
- **Semver-baseline retire-or-keep:** open by default — KEEP until revisited
  at the second published release, per the flip note above.

## Homebrew

Homebrew publishing updates a tap formula from the release checksums. By default, the workflow writes:

```text
Formula/ktesio.rb
```

to:

```text
iMagdy/homebrew-tap
```

Configure these repository settings before publishing a tag:

- `HOMEBREW_TAP_TOKEN` secret: token with write access to the tap repository.
- `HOMEBREW_TAP_REPOSITORY` variable: optional `owner/repo` override. Defaults to `<release-owner>/homebrew-tap`.
- `HOMEBREW_TAP_BRANCH` variable: optional target branch override. Defaults to `main`.

The generated formula installs the prebuilt macOS or Linux archive for the user's platform and declares `git` as a runtime dependency.

## Installer Hosting

The public installer files live under:

```text
scripts/public/
```

Cloudflare Pages should serve only that isolated directory, not the full
automation-focused `scripts/` directory.

Pages configuration:

- Project: `ktesio-cli`
- Repository: `iMagdy/ktesio`
- Production branch: `main`
- Build command: `exit 0`
- Output directory: `scripts/public`
- Custom domain: `cli.ktesio.dev`

Use Cloudflare Pages Git integration for this project so deployments track the
repository. The installer endpoint should be configured through the Pages
custom-domain flow before relying on DNS records alone.

The installer binary fallback resolves the latest GitHub Release, downloads the
matching archive and `.sha256` file, verifies the checksum, and installs `kt`.
Keep the asset names below stable or update `scripts/public/install.sh`,
`scripts/public/install.ps1`, and the installer tests in the same change.

## Local Dry Run

Generate release notes without publishing anything:

```bash
python3 scripts/generate_release_docs.py v0.1.0 --output-dir target/release-docs-test
```

Update local docs for inspection:

```bash
python3 scripts/generate_release_docs.py v0.1.0 --update-files
```

## Asset Names

```text
ktesio-<tag>-x86_64-apple-darwin.tar.gz
ktesio-<tag>-aarch64-apple-darwin.tar.gz
ktesio-<tag>-x86_64-pc-windows-msvc.zip
ktesio-<tag>-x86_64-unknown-linux-gnu.tar.gz
ktesio-<tag>-checksums.txt
```
