---
title: Release Notes
description: Generated release summaries, asset tables, checksums, and changes for published Ktesio versions.
---

# Release Notes

Release notes are generated when a version tag is published.

The tag workflow updates the GitHub Release immediately and then opens a pull request that refreshes this file and the root `CHANGELOG.md`.

> **Compatibility notice — announced ahead of the next release (Adapter Contract v1 freeze).**
> Per the deprecation policy ratified by Islam on 2026-09-04 (*within a major, deprecations announced ≥1 minor ahead via CHANGELOG/RELEASE_NOTES + doc notices; removals only at next major; enforced by semver-checks CI*):
>
> - **The Adapter Contract is frozen at v1 (`1.0.0`)**. The engine negotiates at registration: a manifest with a different contract **major** fails to load naming both versions and the rule (*compatible iff the major versions match*). Pre-v1 `0.x` values are not grandfathered — set `contract_version = "1.0.0"` in your `adapter.toml`. Strict `X.Y.Z` parsing; prerelease/build suffixes negotiate by major. See the new [Adapter Contract](adapter-contract.md#versioning) page.
> - **New `--json` surface on the memory commands** (the ONE announced key-set edit, frozen with contract v1): `kt agent memory attach --json` emits `{ schema_version, instance, kind, guarantee, dir, declared }` with the typed snake_case strings (`filesystem`/`native`, `managed_dir_byte_durable`/`home_persistence_only`); `kt agent memory detach --json` emits `{ schema_version, instance }`. Human output is unchanged; the Story 4-3 frozen key-set assertions were re-pinned in the same change and no other frozen set moved.
> - `[interaction].channel` accepts `"http"` in addition to `"stdio"` (additive vocabulary; the engine does not branch on it in v1).
> - The CI `semver` job's cache is version-keyed, version-verified on restore, and conditionally saved (AI-3); it now also runs an armed in-repo baseline check against the contract-v1 freeze commit (any breaking public-surface change vs the freeze baseline fails CI — announced or not; announcement enforcement is review discipline until the crates.io baseline exists), while the crates.io-published-baseline comparison remains dormant (notice-only) until the crates publish (story 7-4).
> - **`ConformanceReport` (the conformance Test Kit's report) carries `contract_version`** — the Adapter Contract version the run was negotiated under. Additive and `#[serde(default)]`; archived `schema_version: 1` reports still deserialize; the schema version stays `1`.
> - **`AgentAdapter` default lifecycle bodies return a new `Unavailable` reason string** (frozen-v1 surface text change, an announced, compatibility-neutral wording fix in place (the ratified deprecation policy's >=1-minor / remove-at-major clauses govern type and key-set removals, not correcting stale text)): the stale seed text ("…not implemented until story 1-4") becomes "this adapter does not implement lifecycle ops (the trait's inert default body)". Text-only — no types, variants, or key-sets change.
>
> *Placement note: this banner sits above the first `## ` heading deliberately — `scripts/generate_release_docs.py::upsert_release_section` inserts generated release sections above the first `## ` heading, so a `## Unreleased` heading would be buried at the next tag; the header region is never touched. At the release cut that ships the freeze, move this content into the release section and delete the banner.*

> **License change — announced ahead of the next release (Ktesio Noncommercial-Attribution License 1.0.0).**
> `LICENSE` is retitled and amended in place: the PolyForm Noncommercial 1.0.0 terms are kept, and one new condition is added — **Attribution**. Whenever you distribute the software, distribute a modified version of it, use it in your own product or distribution, or operate it to provide functionality to third parties, you must prominently credit the Ktesio project and its author ("Islam Magdy", the copyright holder) in at least one place a reasonable user or recipient would readily see (your product's documentation, an "About" or credits screen, or a public README all qualify); you may not state or imply that the author endorses you or your use. The commercial mechanism is unchanged: all commercial use remains unlicensed without the copyright holder's separate written license.
>
> - **Existing noncommercial users: your usage rights are unchanged — with one new requirement.** Noncommercial use, modification, and sharing stay free under the same terms; but any product or distribution using Ktesio now carries the visible-credit requirement above. Private, internal use that reaches no third party owes no credit.
> - **Commercial users: contact for a license.** Request one through https://github.com/Ktesio/ktesio.
> - Packaging metadata moves with the retitle in the same change: Cargo uses `license-file = "LICENSE"` (a custom license has no SPDX id), the Homebrew formula declares `license :any`, and the README badge and License section name the new license. Ktesio remains **source-available** — not open source.

> **Unreleased — epic 12 in review: durable detach + the production-usable observed channel.**
> Landed on the epic branch (one PR), announced ahead of the release that ships it:
>
> - **`kt agent start --detach`** — spawn an agent that keeps running after the command exits; the next `kt` command re-adopts it through the existing adoption path, and detached-ness is durable across N commands (the flag rides the write-ahead record, so every later engine re-holds the handle disarmed). The cost is stated everywhere the flag appears (`--help` + a stderr notice): between commands there is no crash detection, no budget enforcement, and no event delivery. **`start --detach` on an engine-observed instance is refused before any side effect with a new documented exit code (5)** — the loopback metering listener dies with the starting command.
> - **`EngineError::DetachRefused` is a NEW variant on the exhaustive `EngineError` enum** (the refusal's typed surface, with `name` + `detail`). Hosts matching `EngineError` exhaustively need the new arm (or a `_` wildcard); nothing else on the engine surface moved.
> - **The state store's spawn record gains a `detached` column — schema v6** (additive `ALTER TABLE … ADD COLUMN … NOT NULL DEFAULT 0`; existing databases migrate automatically on open, every pre-v6 record reading as the supervised, non-detached shape it was). No other schema or wire change.
> - **New TLS dependency family in `ktesio-engine`: `rustls` + `hyper-rustls` + `webpki-roots` (+ `ring`, already in the graph via the download client)** — vendored TLS with no system library, `default-features = false`, ring provider, one rustls+ring pair workspace-wide (CI-gated). This is what makes `metering.upstream_base_url` accept `https://` for the engine-observed metering channel; streams are metered via the (upstream-visible) `stream_options.include_usage` injection on chat-completions requests.
> - **A new `audit` CI job** runs `cargo audit` against `Cargo.lock` on every push/PR (plus a scheduled re-scan): a RustSec finding fails the job unless it carries a documented, reviewed waiver in `audit.toml`.

> **Embedding distribution — PUBLISHED (v0.7.0, 2026-09-09).**
> The epic's distribution capstone executed on Islam's explicit go (release issue #176): the three library crates are on crates.io. **Shipped in-repo:** the embedding quickstart — a complete host example (`crates/ktesio-engine/examples/embedding-quickstart.rs`) linking only the engine's public facade (and nothing else — no test fixtures, no helper crates), which CI compiles on all three OS legs and RUNS hermetically on ubuntu; the new [Embedding the engine](embedding.md) page (git-pinned dependency form, facade surface, event-bus contract, walkthrough); the publish runbook — every held action (publish-flag flip → `cargo publish -p ktesio-adapter-api` → `-p ktesio-adapters-hermes` → `-p ktesio-engine` → the `vX.Y.Z` tag → the Homebrew tap verification) with exact commands, preconditions, and order in [the release process](release-process.md#publishing-the-engine-crates); and the `ktesio-engine` in-repo semver baseline armed in CI alongside `ktesio-adapter-api`'s (each crate guarded against its own freeze commit — the engine's embedding surface froze with story 7-3). Hosts depend on the published crates (`ktesio-engine = "0.1"`); the git-pin guidance remains as a fallback. The tag releases the `ktesio` CLI (0.7.0), the binaries, and the Homebrew tap update in the same sweep.

> **Epic 7 — the engine becomes an embeddable library: event bus, embed-clean guarantee, measured performance budgets.**
> Three headline surfaces of the embedding epic are new and shippable in-repo today (all three library crates now on crates.io):
>
> - **The engine event bus** (story 7-2): subscribe to the engine's committed events without polling — `Engine::subscribe()` (async, a raw broadcast receiver) and `Blocking::subscribe()` (sync, an `EventSubscription` whose `recv`/`try_recv` bridge through the engine runtime). Every payload is a versioned serde struct (`EngineEvent` wrapping lifecycle `TransitionEvent`, `BudgetBreachEvent`, and `UsageUpdateEvent` — each carrying its own `schema_version`, the exact wire shapes `kt --json` documents). The bus is bounded at `EVENT_BUS_CAPACITY` (1024): a subscriber that falls behind observes `Lagged(n)` naming the dropped prefix and resynchronizes at the tail — a slow subscriber can never stall supervision. Delivery is at-most-once in the crash window between a durable append and its publish; the durable record stays complete, the query APIs (`transition_events`, `budget_breach_events`, ledger reads) read the past directly, and `resync_events` (Epic 10, below) now heals that window in one call.
> - **The embed-clean guarantee** (story 7-3): the engine embeds into a host's process cleanly — no stdin reads, no interactive prompts, no TTY detection, no host-environment mutation, and no process-global handlers or global state (several engines coexist in one process, each rooted at its own state directory). The full API is reachable behind the `blocking()` facade — the same surface `kt` itself drives — and CI audits the guarantee durably (a two-engine collision test, a source-level no-TTY/no-prompt/no-global-state audit with narrow named allowlist entries, and a whole-crate blocking-coverage inventory).
> - **The performance budgets are measured, not assumed** (story 7-5): the NFR-4 budgets — reads < 1 s on a 25-instance Fleet; supervision overhead ≤ 2% CPU and ≤ 50 MiB RSS per running instance — are benchmarked by a real harness (`crates/ktesio-engine/examples/perf-budgets.rs`) on a real running fixture, gated in CI by a dedicated ubuntu `perf-budgets` job whose JSON report is the measured record (reads p99 and RSS gate hard everywhere; CPU gates strict locally with the documented ×1.5 shared-runner tolerance over the median of three windows in CI). See [Testing](testing.md#performance-budgets-nfr-4-story-75).

> **Epic 10 — embedder hardening: crash-window resync, host-owned diagnostics, ratified subscriber budgets.**
> Three new public surfaces of the embedding epic, additive throughout (the semver gate sees new items, never a breaking change):
>
> - **`resync_events` + `ResyncCursor`/`ResyncBatch`** (story 10-3): the one-call remedy for the event bus's documented at-most-once crash window — `Engine::resync_events(name, after)` / `Blocking::resync_events` read the instance's COMMITTED truth (transitions, breaches, ledger rows) past a per-family cursor and return it as the exact `EngineEvent` payloads the live bus delivers, plus the cursor to continue from (idempotent; serializes snake_case so a host can persist it). Crash-recovery reads are torn-tail tolerant — one unparseable trailing line carrying the torn-append signature (no final newline) is skipped, with the skip SURFACED on `ResyncBatch::torn_tail_skipped` (an additive `#[serde(default)]` field — archived batches still deserialize); every other malformed line is a typed error. A cursor past a family's committed count (a truncated/rotated log) is a typed error naming the family and both counts — never a silent re-delivery. Both combine orders are documented with their tradeoffs: subscribe-first is gap-free (dedup the overlap with your cursor), resync-first needs a quiescent agent to be gap-free.
> - **`DiagnosticSink` + `Engine::open_with_diagnostics` / `with_diagnostics` / `Blocking::with_diagnostics`** (story 10-2): route the engine's two stderr diagnostics (the DC-10 memory-delivery notice and the enforcement breadcrumb) into a host-provided `Arc<Mutex<Box<dyn Write + Send>>>` — same bytes, one line each. Install at open (in place before any supervision work) or rotate any time later (the outgoing writer is flushed on rotation); write errors AND write panics are best-effort swallowed (a host bug never unwinds through supervision); installing is one-way — it replaces, never removes, so returning to the stderr default means re-opening the engine. With no sink, the stderr behavior is byte-identical (pinned by a subprocess suite), and with a sink installed stderr stays silent.
> - **Ratified subscriber-active budgets** (story 10-3): the perf-budgets harness gains a subscriber-overhead addendum with three hard gates on the one-active-subscriber deltas vs the zero-subscriber baseline — CPU Δ ≤ 0.25 percentage points, RSS Δ ≤ 2 MiB, read-p99 Δ ≤ 100 ms per running instance (strict locally; the documented ×1.5 shared-runner tolerance in CI) — ratified from measured deltas recorded in [Testing](testing.md#performance-budgets-nfr-4-story-75).
> - **Audit teeth**: the embed-clean audit pins the sink plumbing count==1 (the emission choke point, both routes into it, the single stderr default arm) and its stdio-reach scan covers fully-qualified, bare imported-path, and hand-written `_print` write forms — a stdio write via an imported path cannot evade the single-writer count.


> **ktesio-engine 0.3.0 — the epic-12 breaking release (published 2026-09-16, Islam's explicit go; announced ahead in the epic-12 PR).**
> The engine's crate version moves 0.2.0 → 0.3.0 to carry story 12-1's detached-start surface — the CI semver gate's crates.io loop flagged all three against the published 0.2.0 (its second real firing):
>
> - **`EngineError::DetachRefused` is a NEW variant on the exhaustive `EngineError` enum**: a `--detach` start of an engine-observed instance is refused with a named reason (its loopback metering listener dies with the CLI command — the same loud strand as adoption), never a silent fallback to attached. Host `match`es over `EngineError` need the new arm (or a `_` wildcard).
> - **`SpawnRecord` gains the pub field `detach`** — exhaustive struct literals over `SpawnRecord` in host code need the new field.
> - **`ProcessBackend::adopt` takes a second parameter (`detached: bool`)** — port-trait implementors must update the signature (the Windows backend's adopted shape is inherently drop-disarmed; the Unix backend uses the flag to re-hold a detached record's handle with its Drop disarmed).
> - Hosts on the crates.io pin move `ktesio-engine = "0.2"` → `"0.3"` (embedding.md); the git-pin alternative is unchanged.

> **ktesio-engine 0.2.0 — the library's first breaking release (published 2026-09-15, Islam's explicit go; announced ahead in PR #181).**
> The engine's crate version moves 0.1.0 → 0.2.0 to carry the epic-11 surface extension that the CI semver gate flagged (its first real firing — first against the in-repo freeze baseline, then against the PUBLISHED 0.1.0 baseline once the crates.io release-to-release loop armed):
>
> - **`EngineError::ResumeUnsupported` is a NEW variant on the exhaustive `EngineError` enum** (AI-7, story 11-1): a `resume` on a PAUSED instance whose adapter declares pause `unsupported` for the current OS fails fast with a DEDICATED diagnostic — it names the state, the adapter's pause declaration, and the escape hatch (`stop` works without pause support) — instead of the bare pause-unsupported error that would strand the operator. Downstream `match`es over `EngineError` in host code need the new arm (or a `_` wildcard); NOTHING else on the engine surface moved (semver-checked release-to-release against published 0.1.0, and against the in-repo freeze baseline `bee7d48` on every push).
> - Hosts on the crates.io pin move `ktesio-engine = "0.1"` → `"0.2"`; the git-pin alternative is unchanged (see [Embedding the engine](embedding.md)).
> - `ktesio-adapter-api` and `ktesio-adapters-hermes` are unchanged at 0.1.0 — no republish.

## v0.6.0

Ktesio is repositioned as an agent runner: `kt` runs AI agents like services — supervising their lifecycle, metering real token usage, and enforcing dollar budgets.

### Removed

- The legacy skill-manager command surface is **removed** at 0.6.0. The commands `kt init`, `kt install`, `kt search`, `kt upgrade`, `kt publish`, `kt list`, `kt show`, `kt doctor`, `kt uninstall`, and the `kt remove` alias no longer exist; `kt` is no longer a skills package manager.

### Changed

- The single canonical way to operate the Fleet is the agent runner under `kt agent …` — `kt agent list` and `kt agent show <name>` replace the removed top-level `kt list`/`kt show`, alongside `kt agent register`/`start`/`stop`/`pause`/`resume` and `kt agent config …`. See the [command reference](commands.md) for the full agent-runner surface.
- `kt --help` and the crate metadata now describe the agent runner rather than a skills package manager.
- Continuity is preserved: the `ktesio` crate name, the `kt` binary, the install channels, and `kt self-update` are unchanged.

## v0.5.0

Comparison: [v0.4.0...v0.5.0](https://github.com/Ktesio/ktesio/compare/v0.4.0...v0.5.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.5.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.5.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.5.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.5.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.5.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-checksums.txt) | - |

### Features

- add fumadocs documentation site ([ba48fe0](https://github.com/Ktesio/ktesio/commit/ba48fe0))

### Fixes

- migrate ureq usage for cargo dependency updates (#44) ([29de3e8](https://github.com/Ktesio/ktesio/commit/29de3e8))

### Documentation

- update release notes for v0.4.0 (#37) ([3fdf921](https://github.com/Ktesio/ktesio/commit/3fdf921))

### Maintenance

- bump version to 0.5.0 ([08c20ec](https://github.com/Ktesio/ktesio/commit/08c20ec))
- untrack BMAD artifacts and relicense under PolyForm Noncommercial 1.0.0 (#49) ([59c8d19](https://github.com/Ktesio/ktesio/commit/59c8d19))
- bump the docs-dependencies group in /docs with 4 updates (#42) ([d977de7](https://github.com/Ktesio/ktesio/commit/d977de7))
- bump actions/checkout from 6.0.2 to 6.0.3 in the github-actions group (#40) ([a90451b](https://github.com/Ktesio/ktesio/commit/a90451b))

### Other Changes

- ```text feat: add BMad Method v6.8.0 skills — agents, workflows, and core tools ([21a9ad1](https://github.com/Ktesio/ktesio/commit/21a9ad1))

## v0.4.0

Comparison: [v0.3.1...v0.4.0](https://github.com/Ktesio/ktesio/compare/v0.3.1...v0.4.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.4.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.4.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.4.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.4.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-checksums.txt) | - |

### Features

- add update notice and self-update (#34) ([5fcfb60](https://github.com/Ktesio/ktesio/commit/5fcfb60))
- add hosted installers (#36) ([2fe0f82](https://github.com/Ktesio/ktesio/commit/2fe0f82))

### Documentation

- update release notes for v0.3.1 (#32) ([6f50c2e](https://github.com/Ktesio/ktesio/commit/6f50c2e))

### Maintenance

- bump version to 0.4.0 ([580dfa1](https://github.com/Ktesio/ktesio/commit/580dfa1))
- add kt-release skill ([ff17e29](https://github.com/Ktesio/ktesio/commit/ff17e29))

## v0.3.1

Comparison: [v0.3.0...v0.3.1](https://github.com/Ktesio/ktesio/compare/v0.3.0...v0.3.1)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.3.1-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.3.1-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.3.1-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.3.1-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.3.1-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-checksums.txt) | - |

### Features

- discover fallback skills from agents directory (#31) ([634c99a](https://github.com/Ktesio/ktesio/commit/634c99a))

### Documentation

- merge install details into quickstart ([7913033](https://github.com/Ktesio/ktesio/commit/7913033))
- update quickstart install paths ([1a94d4f](https://github.com/Ktesio/ktesio/commit/1a94d4f))
- update release notes for v0.3.0 (#28) ([05651e6](https://github.com/Ktesio/ktesio/commit/05651e6))

### Maintenance

- bump version to 0.3.1 ([dc0965b](https://github.com/Ktesio/ktesio/commit/dc0965b))

## v0.3.0

Comparison: [v0.2.0...v0.3.0](https://github.com/Ktesio/ktesio/compare/v0.2.0...v0.3.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.3.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.3.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.3.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.3.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.3.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-checksums.txt) | - |

### Features

- show init adoption progress ([7ac8362](https://github.com/Ktesio/ktesio/commit/7ac8362))

### Fixes

- recognize publish docs examples ([8c1443c](https://github.com/Ktesio/ktesio/commit/8c1443c))

### Documentation

- update release notes for v0.2.0 (#27) ([12ce314](https://github.com/Ktesio/ktesio/commit/12ce314))

### Maintenance

- bump version to 0.3.0 ([dabb5a0](https://github.com/Ktesio/ktesio/commit/dabb5a0))

## v0.2.0

Comparison: [v0.1.1...v0.2.0](https://github.com/Ktesio/ktesio/compare/v0.1.1...v0.2.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.2.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.2.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.2.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.2.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.2.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-checksums.txt) | - |

### Features

- add dependency publish manifest model ([784a02b](https://github.com/Ktesio/ktesio/commit/784a02b))
- polish CLI terminal output (#26) ([86e13be](https://github.com/Ktesio/ktesio/commit/86e13be))
- add skills search and shorthand installs (#23) ([17b4e4d](https://github.com/Ktesio/ktesio/commit/17b4e4d))

### Documentation

- update release notes for v0.1.1 (#11) ([883c7a3](https://github.com/Ktesio/ktesio/commit/883c7a3))

### CI

- remove OCI release packaging (#24) ([2e9cc1d](https://github.com/Ktesio/ktesio/commit/2e9cc1d))

### Other Changes

- [codex] Add adoption CLI workflows (#17) ([d66f73f](https://github.com/Ktesio/ktesio/commit/d66f73f))
- [codex] Add README banner (#12) ([c41c7ed](https://github.com/Ktesio/ktesio/commit/c41c7ed))

## v0.1.1

Comparison: Initial release history

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.1.1-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.1.1-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.1.1-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.1.1-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.1.1-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-checksums.txt) | - |

### Features

- improve cli visuals and help ([052ca93](https://github.com/Ktesio/ktesio/commit/052ca93))
- add release automation and open source polish ([f8ef392](https://github.com/Ktesio/ktesio/commit/f8ef392))
- Add GitHub CI pipeline for PR checks (#4) ([171de35](https://github.com/Ktesio/ktesio/commit/171de35))
- integrate GitHub issue tracking into task implementation and PR workflow ([4b4fdb7](https://github.com/Ktesio/ktesio/commit/4b4fdb7))
- add integration tests and improve unit test coverage ([2f6bf07](https://github.com/Ktesio/ktesio/commit/2f6bf07))
- add skill install fallback discovery ([7e6b43f](https://github.com/Ktesio/ktesio/commit/7e6b43f))
- add comprehensive documentation and test coverage ([b5fc1a5](https://github.com/Ktesio/ktesio/commit/b5fc1a5))
- implement agentic skills package manager CLI ([73e6ac3](https://github.com/Ktesio/ktesio/commit/73e6ac3))

### Fixes

- allow partial skill manifests ([a5f5dc4](https://github.com/Ktesio/ktesio/commit/a5f5dc4))
- install exported skill content safely ([2f525b2](https://github.com/Ktesio/ktesio/commit/2f525b2))

### Documentation

- mark dependabot updates merged ([a73ebeb](https://github.com/Ktesio/ktesio/commit/a73ebeb))
- clarify solo maintainer branch policy ([36a1b74](https://github.com/Ktesio/ktesio/commit/36a1b74))
- add repository audit checklist ([d978ca7](https://github.com/Ktesio/ktesio/commit/d978ca7))
- correct repository name and path in quick start instructions ([10da4ff](https://github.com/Ktesio/ktesio/commit/10da4ff))
- add test coverage and documentation currency principles (v1.1.0) ([c72c185](https://github.com/Ktesio/ktesio/commit/c72c185))

### Tests

- increase coverage for cli helpers (#7) ([7f29853](https://github.com/Ktesio/ktesio/commit/7f29853))

### CI

- publish only release asset files ([00e7fd3](https://github.com/Ktesio/ktesio/commit/00e7fd3))
- publish crate before release artifacts ([88438e5](https://github.com/Ktesio/ktesio/commit/88438e5))
- identify crates io release check ([dc2f96d](https://github.com/Ktesio/ktesio/commit/dc2f96d))
- use current intel macos release runner ([ad0e63d](https://github.com/Ktesio/ktesio/commit/ad0e63d))
- exempt dependabot prs from dco by author ([537742b](https://github.com/Ktesio/ktesio/commit/537742b))
- align dco checks with automation ([5682357](https://github.com/Ktesio/ktesio/commit/5682357))
- publish release artifacts to homebrew and crates (#6) ([d571bd5](https://github.com/Ktesio/ktesio/commit/d571bd5))

### Maintenance

- prepare 0.1.1 release ([285f059](https://github.com/Ktesio/ktesio/commit/285f059))
- rename project to ktesio (#10) ([d2cfa1f](https://github.com/Ktesio/ktesio/commit/d2cfa1f))
- bump cargo dependency group ([3ac0ab6](https://github.com/Ktesio/ktesio/commit/3ac0ab6))
- bump github actions group ([4a0ed6e](https://github.com/Ktesio/ktesio/commit/4a0ed6e))
- use canonical apache license text ([e5acc16](https://github.com/Ktesio/ktesio/commit/e5acc16))
- harden repository governance ([c1463f4](https://github.com/Ktesio/ktesio/commit/c1463f4))

### Other Changes

- Add license, homepage, repository, and readme to Cargo.toml ([d636953](https://github.com/Ktesio/ktesio/commit/d636953))
- apply code formatting and update Rust edition to 2024 ([32fbc59](https://github.com/Ktesio/ktesio/commit/32fbc59))
- speckit ([8d14960](https://github.com/Ktesio/ktesio/commit/8d14960))
- Initial commit from Specify template ([76d7354](https://github.com/Ktesio/ktesio/commit/76d7354))
