# Changelog

All notable changes to Hekma (Ktesio through v0.7.0) are generated from git history when a version tag is published.

Release automation updates this file with a pull request after each `vMAJOR.MINOR.PATCH` tag.

> **Unreleased — epic 14 in review: ACP backend support (the builtin `acp` kind).**
> Announced ahead of the release that ships it:
>
> - **New builtin `acp` kind** — any Agent Client Protocol (ACP) v1 agent runs under full supervision via `kt agent register <name> --kind acp` + the `acp.command`/`acp.args` config keys. `send` = one `session/prompt` turn (returns immediately; chunks + stop reason stream to the log/events); `stop` sends `session/cancel` first; a concurrent second prompt is refused (typed, surfaced); permissions are denied with a surfaced diagnostic; detached acp starts are refused (the transport needs the pipes). Client capabilities advertised = the protocol default (no fs/terminal/elicitation in v1).
> - **`EngineError::AcpTurnInFlight`** — new variant on the exhaustive `EngineError` enum (hosts matching exhaustively need the arm or `_`). Maps to exit-code row 4; no new exit code.
> - **`SpawnSpec.pipe_stdout` (new public field) + `ProcessBackend::take_stdin`/`take_stdout` (defaulted trait methods)** — the ACP transport owns the child's stdio halves; every raw line still lands in the instance log. Defaulted methods keep existing backends compiling; exhaustive `SpawnSpec` constructors outside the crate need the new field. `hekma-engine` → **0.5.0**.
> - **ACP metering**: `usage_update` is context-grain and surfaced only (never billed); billing-grade tokens (input/output/**cached**) come from the epic's tiered paths (observed base-URL / stderr sentinel); no source → the honest not-available marker. Cached tokens become first-class across parse/ledger/rate/budgets/surfaces for every kind.

> **Compatibility notice — announced ahead of the next release (Adapter Contract v1 freeze).**
> Per the deprecation policy ratified by Islam on 2026-09-04 (*within a major, deprecations announced ≥1 minor ahead via CHANGELOG/RELEASE_NOTES + doc notices; removals only at next major; enforced by semver-checks CI*):
>
> - **The Adapter Contract is frozen at v1 (`1.0.0`)** (previously the `0.4.0` seed). The engine now **negotiates** at registration (FR-30): a manifest loads only when its `contract_version` **major** matches the engine's; a mismatch fails with both versions named and the rule quoted — *compatible iff the major versions match*. **Pre-v1 `0.x` manifests are not grandfathered** (the contract was never published under 0.x): set your manifest's `contract_version` to `"1.0.0"`. The parse is strict `X.Y.Z` (no `v` prefix, no `1`/`1.0` partials); prerelease/build suffixes parse and negotiate by major. See the new [Adapter Contract](docs/adapter-contract.md#versioning) page.
> - **New `--json` wire surface on the memory commands** — the ONE announced key-set edit (transferred from Story 5-1's DC-6 obligation, deferred by Story 5-2, and frozen with contract v1): `kt agent memory attach <name> --kind <kind> --json` emits a versioned document (`schema_version` `1`, `instance`, `kind`, `guarantee`, `dir`, `declared`) carrying the backing kind and guarantee level in their typed snake_case strings (`filesystem`/`native`, `managed_dir_byte_durable`/`home_persistence_only`); `kt agent memory detach <name> --json` emits the minimal versioned confirmation (`schema_version`, `instance`). Human output is unchanged. The Story 4-3 frozen key-set assertions were re-pinned to include these documents in the same change; every other frozen key-set is untouched.
> - **`[interaction].channel` gains the `"http"` value** (CP-6.5-a, additive vocabulary): an HTTP-native agent may name its real interaction transport. The engine does not branch on the channel in v1. Additive — nothing that parsed before stops parsing.
> - The CI `semver` job's toolchain cache is now keyed on the resolved `cargo-semver-checks` version, verified against the restored binary, and saved only after a real install (AI-3). The job now also runs an **armed in-repo baseline check** (`cargo-semver-checks --baseline-rev` against the contract-v1 freeze commit `4119db3`): any breaking change to the public `ktesio-adapter-api` surface vs the freeze baseline (removal, rename, signature change) fails CI during the pre-publish window — whether or not it was announced; announcement enforcement is review discipline until the crates.io baseline lets the gate diff release-to-release. The crates.io-published-baseline comparison stays dormant (notice-only) until the crates publish to crates.io (story 7-4).
> - **`ConformanceReport` (the conformance Test Kit's report) carries `contract_version`** — the Adapter Contract version the run was negotiated under. Additive and `#[serde(default)]` (empty = produced by a pre-field harness), so archived `schema_version: 1` reports still deserialize; the schema version stays `1` per the report family's documented bump policy. Third parties asserting on reports see one new key.
> - **`AgentAdapter` default lifecycle bodies return a new `Unavailable` reason string** (a frozen-v1 surface TEXT change, an announced, compatibility-neutral wording fix in place (the ratified deprecation policy's >=1-minor / remove-at-major clauses govern type and key-set removals, not correcting stale text)): the seed text `"lifecycle execution is not implemented until story 1-4"` — a stale development-story reference in a public error surface — is replaced by `"this adapter does not implement lifecycle ops (the trait's inert default body)"`. No types, variants, or key-sets change; only the human-readable `reason` text inside `AdapterError::Unavailable` produced by the trait's DEFAULT bodies (any adapter overriding the ops is unaffected). Code that string-matched the old seed text must match the new one.
>
> *Placement note (mechanics): this banner sits ABOVE the first `## ` heading on purpose — `scripts/generate_release_docs.py::upsert_release_section` inserts every generated release section directly above the first `## ` heading, so a `## Unreleased` heading would be pushed below the fold at the very next tag. The script never touches the header region, so the banner stays visible through the next release cut. At that cut, the release author moves this notice's content into the release section and deletes the banner.*

> **License change — announced ahead of the next release (Ktesio Noncommercial-Attribution License 1.0.0).**
> `LICENSE` is retitled and amended in place: the PolyForm Noncommercial 1.0.0 terms are kept, and one new condition is added — **Attribution**. Whenever you distribute the software, distribute a modified version of it, use it in your own product or distribution, or operate it to provide functionality to third parties, you must prominently credit the Ktesio project and its author ("Islam Magdy", the copyright holder) in at least one place a reasonable user or recipient would readily see (your product's documentation, an "About" or credits screen, or a public README all qualify); you may not state or imply that the author endorses you or your use. The commercial mechanism is unchanged: all commercial use remains unlicensed without the copyright holder's separate written license.
>
> - **Existing noncommercial users: your usage rights are unchanged — with one new requirement.** Noncommercial use, modification, and sharing stay free under the same terms; but any product or distribution using Ktesio now carries the visible-credit requirement above. Private, internal use that reaches no third party owes no credit.
> - **Commercial users: contact for a license.** Request one through https://github.com/Ktesio/ktesio.
> - Packaging metadata moves with the retitle in the same change: Cargo uses `license-file = "LICENSE"` (a custom license has no SPDX id), the Homebrew formula declares `license :any`, and the README badge and License section name the new license. Ktesio remains **source-available** — not open source.

> **Embedding distribution — PUBLISHED (v0.7.0, 2026-09-09).**
> The epic's distribution capstone executed on Islam's explicit go (release issue #176): the three library crates are on crates.io. **Shipped in-repo:** the embedding quickstart — a complete host example at `crates/ktesio-engine/examples/embedding-quickstart.rs` that links only the engine's public facade (and nothing else — no test fixtures, no helper crates) and that CI compiles on all three OS legs and RUNS hermetically on ubuntu; the new [Embedding the engine](docs/embedding.md) documentation page (git-pinned dependency form, facade surface, event-bus contract, walkthrough); the publish runbook — every held action (publish-flag flip → `cargo publish -p ktesio-adapter-api` → `-p ktesio-adapters-hermes` → `-p ktesio-engine` → the `vX.Y.Z` tag → the Homebrew tap verification) written with exact commands, preconditions, and order in [the release process](docs/release-process.md#publishing-the-engine-crates); and the `ktesio-engine` in-repo semver baseline armed in CI alongside `ktesio-adapter-api`'s (each crate guarded against its own freeze commit — the engine's embedding surface froze with story 7-3, since its unpublished surface legitimately grew after the earlier contract freeze). Hosts depend on the published crates (`ktesio-engine = "0.1"`); the git-pin guidance remains as a fallback. The tag releases the `ktesio` CLI (0.7.0), the binaries, and the Homebrew tap update in the same sweep.

> **Epic 7 — the engine becomes an embeddable library: event bus, embed-clean guarantee, measured performance budgets.**
> Three headline surfaces of the embedding epic are new and shippable in-repo today (all three library crates now on crates.io):
>
> - **The engine event bus** (story 7-2): hosts and other Rust consumers can subscribe to the engine's committed events without polling — `Engine::subscribe()` (async, a raw broadcast receiver) and `Blocking::subscribe()` (sync, an `EventSubscription` whose `recv`/`try_recv` bridge through the engine runtime). Every payload is a versioned serde struct (`EngineEvent` wrapping lifecycle `TransitionEvent`, `BudgetBreachEvent`, and `UsageUpdateEvent` — each carrying its own `schema_version`, the exact wire shapes `kt --json` documents). The bus is bounded at `EVENT_BUS_CAPACITY` (1024): a subscriber that falls behind observes `Lagged(n)` naming the dropped prefix and resynchronizes at the tail — a slow subscriber can never stall supervision. Delivery is at-most-once in the crash window between a durable append and its publish; the durable record stays complete, the query APIs (`transition_events`, `budget_breach_events`, ledger reads) read the past directly, and `resync_events` (Epic 10, below) now heals that window in one call.
> - **The embed-clean guarantee** (story 7-3): the engine embeds into a host's process cleanly — it never reads stdin, never prints an interactive prompt, never detects or branches on a TTY, never mutates the host's process environment, and installs no process-global handlers or global state that could collide with a host runtime (several engines coexist in one process, each rooted at its own state directory). The full API is reachable behind the `blocking()` facade — the same surface `kt` itself drives — and CI audits that guarantee durably (a two-engine collision test, a source-level no-TTY/no-prompt/no-global-state audit with narrow named allowlist entries, and a whole-crate blocking-coverage inventory).
> - **The performance budgets are measured, not assumed** (story 7-5): the NFR-4 budgets — reads < 1 s on a 25-instance Fleet; supervision overhead ≤ 2% CPU and ≤ 50 MiB RSS per running instance — are benchmarked by a real harness (`crates/ktesio-engine/examples/perf-budgets.rs`) on a real running fixture, gated in CI by a dedicated ubuntu `perf-budgets` job whose JSON report is the measured record (reads p99 and RSS gate hard everywhere; CPU gates strict locally with the documented ×1.5 shared-runner tolerance over the median of three windows in CI). The measured figures and the gate policy are documented in [Testing](docs/testing.md#performance-budgets-nfr-4-story-75).

> **Epic 10 — embedder hardening: crash-window resync, host-owned diagnostics, ratified subscriber budgets.**
> Three new public surfaces of the embedding epic, additive throughout (the semver gate sees new items, never a breaking change):
>
> - **`resync_events` + `ResyncCursor`/`ResyncBatch`** (story 10-3): the one-call remedy for the event bus's documented at-most-once crash window — `Engine::resync_events(name, after)` / `Blocking::resync_events` read the instance's COMMITTED truth (transitions, breaches, ledger rows) past a per-family cursor and return it as the exact `EngineEvent` payloads the live bus delivers, plus the cursor to continue from (idempotent; serializes snake_case so a host can persist it). Crash-recovery reads are torn-tail tolerant — one unparseable trailing line carrying the torn-append signature (no final newline) is skipped, with the skip SURFACED on `ResyncBatch::torn_tail_skipped` (an additive `#[serde(default)]` field — archived batches still deserialize); every other malformed line is a typed error. A cursor past a family's committed count (a truncated/rotated log) is a typed error naming the family and both counts — never a silent re-delivery. Both combine orders are documented with their tradeoffs: subscribe-first is gap-free (dedup the overlap with your cursor), resync-first needs a quiescent agent to be gap-free.
> - **`DiagnosticSink` + `Engine::open_with_diagnostics` / `with_diagnostics` / `Blocking::with_diagnostics`** (story 10-2): route the engine's two stderr diagnostics (the DC-10 memory-delivery notice and the enforcement breadcrumb) into a host-provided `Arc<Mutex<Box<dyn Write + Send>>>` — same bytes, one line each. Install at open (in place before any supervision work) or rotate any time later (the outgoing writer is flushed on rotation); write errors AND write panics are best-effort swallowed (a host bug never unwinds through supervision); installing is one-way — it replaces, never removes, so returning to the stderr default means re-opening the engine. With no sink, the stderr behavior is byte-identical (pinned by a subprocess suite), and with a sink installed stderr stays silent.
> - **Ratified subscriber-active budgets** (story 10-3): the perf-budgets harness gains a subscriber-overhead addendum with three hard gates on the one-active-subscriber deltas vs the zero-subscriber baseline — CPU Δ ≤ 0.25 percentage points, RSS Δ ≤ 2 MiB, read-p99 Δ ≤ 100 ms per running instance (strict locally; the documented ×1.5 shared-runner tolerance in CI) — ratified from measured deltas recorded in [Testing](docs/testing.md#performance-budgets-nfr-4-story-75).
> - **Audit teeth**: the embed-clean audit pins the sink plumbing count==1 (the emission choke point, both routes into it, the single stderr default arm) and its stdio-reach scan covers fully-qualified, bare imported-path, and hand-written `_print` write forms — a stdio write via an imported path cannot evade the single-writer count.


> **ktesio-engine 0.3.0 — the epic-12 breaking release (published 2026-09-16, Islam's explicit go; announced ahead in the epic-12 PR).**
> The engine's crate version moves 0.2.0 → 0.3.0 to carry story 12-1's detached-start surface — the CI semver gate's crates.io loop flagged all three against the published 0.2.0 (its second real firing):
>
> - **`EngineError::DetachRefused` is a NEW variant on the exhaustive `EngineError` enum**: a `--detach` start of an engine-observed instance is refused with a named reason (its loopback metering listener dies with the CLI command — the same loud strand as adoption), never a silent fallback to attached. Host `match`es over `EngineError` need the new arm (or a `_` wildcard).
> - **`SpawnRecord` gains the pub field `detach`** — exhaustive struct literals over `SpawnRecord` in host code need the new field.
> - **`ProcessBackend::adopt` takes a second parameter (`detached: bool`)** — port-trait implementors must update the signature (the Windows backend's adopted shape is inherently drop-disarmed; the Unix backend uses the flag to re-hold a detached record's handle with its Drop disarmed).
> - Hosts on the crates.io pin move `ktesio-engine = "0.2"` → `"0.3"` (docs/embedding.md); the git-pin alternative is unchanged.

> **ktesio-engine 0.2.0 — the library's first breaking release (published 2026-09-15, Islam's explicit go; announced ahead in PR #181).**
> The engine's crate version moves 0.1.0 → 0.2.0 to carry the epic-11 surface extension that the CI semver gate flagged (its first real firing — first against the in-repo freeze baseline, then against the PUBLISHED 0.1.0 baseline once the crates.io release-to-release loop armed):
>
> - **`EngineError::ResumeUnsupported` is a NEW variant on the exhaustive `EngineError` enum** (AI-7, story 11-1): a `resume` on a PAUSED instance whose adapter declares pause `unsupported` for the current OS fails fast with a DEDICATED diagnostic — it names the state, the adapter's pause declaration, and the escape hatch (`stop` works without pause support) — instead of the bare pause-unsupported error that would strand the operator. Downstream `match`es over `EngineError` in host code need the new arm (or a `_` wildcard); NOTHING else on the engine surface moved (semver-checked release-to-release against published 0.1.0, and against the in-repo freeze baseline `bee7d48` on every push).
> - Hosts on the crates.io pin move `ktesio-engine = "0.1"` → `"0.2"`; the git-pin alternative is unchanged (see [Embedding the engine](docs/embedding.md)).
> - `ktesio-adapter-api` and `ktesio-adapters-hermes` are unchanged at 0.1.0 — no republish.

> **Hekma — the v0.8.0 rename (Ktesio → Hekma), shipping in this release.**
> The product formerly released as Ktesio ships as **Hekma**: the commands are `hekma` (primary) and `hkm` (short alias — same CLI, both standalone); the crate is `hekma` (the `ktesio` crate is frozen at 0.7.0 on crates.io, preserved, never yanked); the engine family continues its line as `hekma-engine` 0.4.0, `hekma-adapter-api` 0.2.0, `hekma-adapters-hermes` 0.2.0; the Homebrew formula renames in the same tap to `ktesio/tap/hekma`; docs live at `hekma.ktesio.dev`. **Ktesio remains the publisher** — Hekma is a ktesio.dev project; the license (Ktesio Noncommercial-Attribution License 1.0.0) is unchanged.
> **For `kt` users (deliberate, no compatibility window):** old `kt` binaries cannot self-update to 0.8.0 — release archives carry only the `hekma-*` family. Re-run the installer (`curl -fsSL https://cli.ktesio.dev/hekma/install.sh | sh`), or `cargo install hekma --force`, or `brew upgrade ktesio/tap/hekma`. **Your data is untouched** — the same state directory is read, nothing is moved or reset; `KTESIO_*` environment settings keep working (with new `HEKMA_*` aliases); JSON output, exit codes, the `KTESIO_USAGE` sentinel, `HERMES_HOME`, and the adapter contract are unchanged. Full details: the [migration guide](docs/migration.md).


## v0.8.1

Comparison: [v0.8.0...v0.8.1](https://github.com/Ktesio/hekma/compare/v0.8.0...v0.8.1)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [hekma-v0.8.1-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [hekma-v0.8.1-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [hekma-v0.8.1-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [hekma-v0.8.1-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [hekma-v0.8.1-checksums.txt](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-checksums.txt) | - |

### Fixes

- matrix REPO -> Ktesio/hekma (canonical post-rename) ([3377a2d](https://github.com/Ktesio/hekma/commit/3377a2d))
- drop v0.6.0 from the matrix floor — no GitHub release exists ([f2b7794](https://github.com/Ktesio/hekma/commit/f2b7794))
- matrix harness — assert empty-fleet JSON correctly on pre-fleet hops ([24900ec](https://github.com/Ktesio/hekma/commit/24900ec))

### Documentation

- brew trust-gate note (verified live during the v0.8.0 cutover) ([6b2f4ed](https://github.com/Ktesio/hekma/commit/6b2f4ed))

### Maintenance

- v0.8.1 — canonical URLs to Ktesio/hekma; docs probe drops the retired legacy host ([74fdcae](https://github.com/Ktesio/hekma/commit/74fdcae))

## v0.8.0

Comparison: [v0.7.0...v0.8.0](https://github.com/Ktesio/ktesio/compare/v0.7.0...v0.8.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [hekma-v0.8.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [hekma-v0.8.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [hekma-v0.8.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [hekma-v0.8.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [hekma-v0.8.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-checksums.txt) | - |

### Features

- Hemaka — the v0.8.0 rename (Ktesio → Hemaka) (#187) ([ad0feaf](https://github.com/Ktesio/ktesio/commit/ad0feaf))
- durable detach + the production-usable observed channel (#184) ([49da96b](https://github.com/Ktesio/ktesio/commit/49da96b))
- bump ktesio-engine to 0.2.0 — the announced breaking release ([d087ed0](https://github.com/Ktesio/ktesio/commit/d087ed0))
- epic 11 — Technical Debt & Process Cleanup (all 7 stories) (#181) ([bee7d48](https://github.com/Ktesio/ktesio/commit/bee7d48))
- epic 10 — consolidate & harden the embedding surface (#180) ([b590dc8](https://github.com/Ktesio/ktesio/commit/b590dc8))

### Fixes

- correct the product name — Hemaka → Hekma, maka → hkm (#188) ([54dc79c](https://github.com/Ktesio/ktesio/commit/54dc79c))
- adopted Windows stop could never confirm death — add SYNCHRONIZE + exit-code probe ([c6ecadb](https://github.com/Ktesio/ktesio/commit/c6ecadb))

### Documentation

- v0.8.0 cutover — publishes recorded, embedding flip, fresh semver baselines @54dc79c ([ba7406d](https://github.com/Ktesio/ktesio/commit/ba7406d))
- 0.3.0 published — docs flip + decision log (Islam's go) ([4299bde](https://github.com/Ktesio/ktesio/commit/4299bde))
- step-5 flip — ktesio-engine 0.2.0 is LIVE; pins move to "0.2" ([fa76eaa](https://github.com/Ktesio/ktesio/commit/fa76eaa))
- announce engine 0.2.0 (the first breaking library release); record the go + the semver-baseline KEEP decision ([5152977](https://github.com/Ktesio/ktesio/commit/5152977))
- record the v0.7.0 manual tap push in the decision log ([5747d50](https://github.com/Ktesio/ktesio/commit/5747d50))

### Tests

- bounded reader grace (no join) in the detach harness — the hang is a PRODUCT finding ([56ee766](https://github.com/Ktesio/ktesio/commit/56ee766))
- in-harness progress markers (entry / post-spawn / 10s ticks) for the Windows detach hang ([66654e1](https://github.com/Ktesio/ktesio/commit/66654e1))
- incremental pipe readers + non-blocking timeout dump in the detach harness ([e5092ce](https://github.com/Ktesio/ktesio/commit/e5092ce))
- kill-before-join in the bounded harness — the join deadlocked the dump ([d773932](https://github.com/Ktesio/ktesio/commit/d773932))
- the detach-test forensic dump goes to stdout (nextest TMT shows stdout only) ([2e5546d](https://github.com/Ktesio/ktesio/commit/2e5546d))
- bounded legs + forensic dump in the 12-1 detach CLI test ([139f7c2](https://github.com/Ktesio/ktesio/commit/139f7c2))
- detach-leg markers on stdout (nextest TMT shows stdout only) ([40be515](https://github.com/Ktesio/ktesio/commit/40be515))
- leg markers in the 12-1 detach CLI test (Windows hang triage) ([08ae1d5](https://github.com/Ktesio/ktesio/commit/08ae1d5))

### Maintenance

- lockfiles for the one-shot ktesio-* deprecation shims (v0.8.0 release series) ([f8d6fb1](https://github.com/Ktesio/ktesio/commit/f8d6fb1))
- record the 12-1 Windows findings — SYNCHRONIZE stop fix landed; the stdio pipe-hold gap routed ([a7fcc1b](https://github.com/Ktesio/ktesio/commit/a7fcc1b))
- bump ktesio-engine to 0.3.0 — the epic-12 breaking release ([394e660](https://github.com/Ktesio/ktesio/commit/394e660))
- point github-sync-map project number at the Ktesio org copy (project 3) ([d32570d](https://github.com/Ktesio/ktesio/commit/d32570d))
- point project links at the Ktesio org copy (project 3) ([3ec8f86](https://github.com/Ktesio/ktesio/commit/3ec8f86))
- update repository references for Ktesio org migration ([e05bc93](https://github.com/Ktesio/ktesio/commit/e05bc93))
- post-merge epic-12 followups — semver baseline to 49da96b, tracker done ([8842b8b](https://github.com/Ktesio/ktesio/commit/8842b8b))
- untrack the local .video_agent artifact + ignore it ([9ee4df7](https://github.com/Ktesio/ktesio/commit/9ee4df7))
- RATIFIED — AI-20/AI-47 applied verbatim as epic-12 (Islam: recommended across the board) ([27ecf07](https://github.com/Ktesio/ktesio/commit/27ecf07))
- AI-58 draft — the AI-20 + AI-47 product-call proposals (awaiting ratification) ([de78001](https://github.com/Ktesio/ktesio/commit/de78001))
- epic-11 retrospective — accepted-with-open-items, 7 action items ([5d84e7f](https://github.com/Ktesio/ktesio/commit/5d84e7f))
- epic-11 complete — flip 11-1..11-7 + epic to done (merged as bee7d48) ([1ef8439](https://github.com/Ktesio/ktesio/commit/1ef8439))
- bump the ktesio-engine semver baseline to the epic-11 merge commit (bee7d48) ([4e12ef8](https://github.com/Ktesio/ktesio/commit/4e12ef8))
- epic 11 opened — Technical Debt & Process Cleanup ([f99e670](https://github.com/Ktesio/ktesio/commit/f99e670))
- epic 11 opened — Technical Debt & Process Cleanup (sprint change proposal approved) ([2f2b763](https://github.com/Ktesio/ktesio/commit/2f2b763))
- epic-5 + epic-7 retrospectives complete ([29bff1d](https://github.com/Ktesio/ktesio/commit/29bff1d))
- epic 10 -> done (PR #180 merged as b590dc8); 10-1..10-3 -> done ([ba524c5](https://github.com/Ktesio/ktesio/commit/ba524c5))
- epic 10 opened — consolidate & harden the embedding surface (sprint change proposal approved) ([999164c](https://github.com/Ktesio/ktesio/commit/999164c))

### Other Changes

- is_multiple_of — the newer stable-clippy lint the CI gate runs ([35a7525](https://github.com/Ktesio/ktesio/commit/35a7525))

## v0.6.0

Ktesio is repositioned as an agent runner: `kt` runs AI agents like services — supervising their lifecycle, metering real token usage, and enforcing dollar budgets.

### Removed

- The legacy skill-manager command surface is **removed** at 0.6.0. The commands `kt init`, `kt install`, `kt search`, `kt upgrade`, `kt publish`, `kt list`, `kt show`, `kt doctor`, `kt uninstall`, and the `kt remove` alias no longer exist; `kt` is no longer a skills package manager.

### Changed

- The single canonical way to operate the Fleet is the agent runner under `kt agent …` — `kt agent list` and `kt agent show <name>` replace the removed top-level `kt list`/`kt show`, alongside `kt agent register`/`start`/`stop`/`pause`/`resume` and `kt agent config …`. See the [command reference](docs/commands.md) for the full agent-runner surface.
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
