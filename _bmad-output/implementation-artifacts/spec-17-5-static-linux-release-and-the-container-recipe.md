---
title: 'Static Linux release and the container recipe'
type: 'feature'
created: '2026-10-20'
status: 'done'
review_loop_iteration: 0
baseline_commit: '46c4c52658e5fa880fb260de45dc3287bd09de0d'
context:
  - '_bmad-output/implementation-artifacts/epic-17-context.md'
  - '_bmad-output/implementation-artifacts/spec-17-4-export-for-ephemeral-hosts-outbox-and-log-capture-modes.md'
  - '_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md (§4.4 Story 15.5 → renumbered 17.5, :378-384; the gap row :52; the threat-model finding :54-61)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-20 :178-200; NFR-6/NFR-8 binds :179; the Deferred list''s static-aarch64 item :359)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Hekma's Linux release artifact is glibc-only x86_64 (`.github/workflows/release.yml:20-22`), so a Host cannot run it in a minimal container (scratch/distroless, no glibc) — the gap the proposal names row-for-row (`sprint-change-proposal-2026-09-30-hosted-platform-readiness.md:52`). And nothing in the docs tells a Host how to run the Service Host as a container supervisor: how the host relates to PID 1, how the drain ladder meets the runtime's stop grace, where the state directory goes, or that the meter stays cooperative inside a container (NFR-6).

**Approach:** Two deliverables, no Rust code. (1) CI: add a static musl `x86_64-unknown-linux-musl` leg to the release matrix (proposal :379, :101) — `hekma`, `hkm` and `hekma-host` shipped as one checksummed archive through the EXISTING release flow (per-archive `.sha256` → aggregate `checksums.txt` → draft release upload); `aarch64` is deferred (deferred-work.md entry with a trigger). The musl toolchain is a CI build-time provision ONLY — no Cargo.toml/Cargo.lock change, the `cargo tree` diff is EMPTY (NFR-8 posture recorded below). (2) Docs: a new page, "Running Hekma as a container supervisor", with a worked recipe covering the story's four bullets (proposal :380-384) — host-as-container-command vs init-supervised service; drain vs stop grace; state-directory placement on a volume (tying to 17-4's outbox/export); the cooperative-meter caveat.

## Tasks & Acceptance

**Execution:**

- [x] `.github/workflows/release.yml` — add the musl leg to the `build` job's matrix (`:16-34`): `{os: ubuntu-latest, target: x86_64-unknown-linux-musl, archive: tar.gz}` plus a matrix flag (e.g. `musl: true`) gating three small additions: (a) an "Install musl tools" step (`sudo apt-get update && sudo apt-get install -y musl-tools`; the `Install Rust` step `:39-40` already adds the target via `${{ matrix.target }}`); (b) the Build step's env for that leg — `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc` and `CC_x86_64_unknown_linux_musl=musl-gcc` (the `cc`-based C deps, `libsqlite3-sys` and `ring`, compile through musl-gcc); (c) a "Verify static" step asserting each packaged binary reports statically linked (`file <bin> | grep -q 'statically linked'`) — a silently-dynamic musl artifact fails the job, never ships. Packaging (`Package Unix`, `:46-62`) copies `hekma-host` beside `hekma`/`hkm` ON THIS LEG ONLY; the per-archive `.sha256` (`:58-62`), artifact upload (`:77-84`), aggregate `hekma-<tag>-checksums.txt` (`:163`) and draft-release upload (`:191-197`) flow unchanged. The glibc archive's contents are UNCHANGED (the `scripts/public/install.sh` contract installs `hekma` + `hkm` from it, `install.sh:282-295`, `:406`).
- [x] `scripts/generate_release_docs.py` — `TARGETS` (`:16-19`) gains `("Linux x64 (static musl)", "x86_64-unknown-linux-musl", "tar.gz")` so the release body's asset table includes the archive + sha256 row (`:190-194`). `HOMEBREW_TARGETS` in `scripts/generate_homebrew_formula.py:24-27` is deliberately UNTOUCHED — Homebrew installs the glibc Linux artifact; the musl archive is for containers/scratch, and the formula generator tolerates extra aggregate entries (its missing-asset check `:86-89` only requires its own targets).
- [x] `scripts/test_automation.py` — keep the pins true: the asset-table and workflow-contains-target tests iterate `release_docs.TARGETS` and self-update (`:39-44`, `:87-88`); extend the hardcoded checksum-parse fixture (`:126-129`) with the musl asset (proves the aggregate can grow) and add the negative pin that the generated Homebrew formula contains NO musl reference (the existing `assertNotIn` pattern, `:143`). The suite runs in CI's docs job (`.github/workflows/ci.yml:611-612`).
- [x] `docs/hosting.md` — NEW page, title "Running Hekma as a container supervisor", registered in `docs/meta.json` (Reference group, `:10-16`) AND `docs/source.config.ts` (the fumadocs file list, `:4-20` — both are load-bearing; the CI docs job builds the site, `ci.yml:613-614`). Content, the four story bullets (`:380-384`): (a) the host as the container command (PID 1: the host arms its own SIGTERM/SIGINT streams, `crates/hekma-host/src/signals.rs:21-25`, `:56-68`, so TERM reaches it without an init) or as an init-supervised service (`docker --init`/tini — required when something else must be PID 1, and the init must forward TERM); (b) drain vs stop grace: `STOPSIGNAL SIGTERM`, the runtime's grace (`docker stop -t` / `stop_grace_period`) must exceed `--stop-window` plus headroom (host startup, adoption, the per-instance ladder after the window — `docs/commands.md:432`), with the honest statement that the drain is BOUNDED and the SIGKILL after the grace is the platform's call, not Hekma's (the host never escalates before its window elapses, `commands.md:423`; a second TERM mid-drain is swallowed and noted, `signals.rs:69-79` — patience wins, repeated TERM does not); (c) state-directory placement: `--state-dir` (or `HEKMA_STATE_DIR`, `crates/hekma-host/src/main.rs:24-25`, `:176-178`) on a mounted volume — everything durable lives there (host.lock, secrets, the state DB, the log families, the outbox pair, `docs/commands.md:448-450`), one host per state dir (the lock refusal names the holder PID, `commands.md:454`), and a replacement container re-mounting the volume resumes — the export ack cursor included (17-4's contract); cross-link the metadata-mode crash window (`commands.md:444`) since a SIGKILL'd container is exactly that window; (d) the cooperative-meter caveat (NFR-6): the observed meter is cooperative — an agent that ignores the injected `base_url` walks around it (`docs/design/metering-agents-you-dont-control.md:94`); the container adds NO fence and is not a sandbox; enforcement of record stays at the Host's external choke point (proposal `:54-61`). Include one worked, version-pinned recipe: multi-stage `FROM scratch` image, `COPY` the three binaries out of the release archive, checksum verification against the `.sha256`, `ENTRYPOINT ["/hekma-host"]`, `ENV HEKMA_STATE_DIR=/data`, `VOLUME /data`, `STOPSIGNAL SIGTERM`, and the Control API turned on with `--listen`/`--bearer-secret` (`--bearer-secret` names an env var — the natural container wiring); note the non-loopback bind works for containers but is bearer-only, no TLS (`docs/control-api.md:39`). No BMAD/story provenance in the page (the check_docs gate).
- [x] Cross-links in the same change — `docs/commands.md`'s serve section (`:421`) and `docs/control-api.md` each gain a pointer to the new page; README's hosting sentence points at it. Keep the diff to pointers; the pages above stay the contracts.
- [x] `_bmad-output/implementation-artifacts/deferred-work.md` — new entry (the story-scoped section convention, e.g. `## Deferred from: implementation of spec-17-5 (<date>)`): static `aarch64` Linux (musl, and glibc) is DEFERRED; trigger: the first Host runs ARM containers, or an ARM Linux release runner is available; evidence: the spine's Deferred item (`ARCHITECTURE-SPINE.md:359`), the story bullet (`proposal:379`), the matrix (`release.yml:16-34`). Fibery mirror per AGENTS.md in the same change.
- [x] NFR-8 posture recorded in the PR — the diff against the baseline (`46c4c52`) touches NO Cargo.toml and NO Cargo.lock: `git diff 46c4c52 -- '**/Cargo.toml' Cargo.lock` is EMPTY; the musl toolchain is a build-time CI provision and enters no runtime graph (AD-20 binds NFR-8, `ARCHITECTURE-SPINE.md:179`; the lean-deps policy, `epics.md:77`).

**Acceptance Criteria:**

- Given a `v*` tag push, the release build runs FIVE legs — the four existing plus `x86_64-unknown-linux-musl` — and the musl leg produces an archive `hekma-<tag>-x86_64-unknown-linux-musl.tar.gz` containing statically linked `hekma`, `hkm`, `hekma-host` (+ README/LICENSE), with a `.sha256` beside it; a dynamic artifact fails the job.
- Given the publish job, the musl archive and its sha256 appear on the draft release, in the aggregate checksums file, and as a row in the generated release-notes asset table; the Homebrew formula is unchanged and contains no musl reference.
- Given the docs build, the new page compiles into the site (nav + collection), passes `check_docs.py` (no broken links, no planning provenance), and covers all four story bullets plus the recipe.
- Given a reader following the recipe, the worked example is faithful to the shipped surfaces: `hekma-host`'s exact flags (`main.rs:170-197`), the stop-window/grace mapping, the volume-backed `--state-dir`, and the NFR-6 caveat in plain words.
- Given handoff: the four house gates + `test_automation.py` pass, the Cargo diff is empty (recorded in the PR), and the aarch64 deferral sits in `deferred-work.md` with a trigger.

</frozen-after-approval>

## Design Notes

- **Musl build mechanism: `musl-tools` + plain cargo — chosen over `cross` and `cargo-zigbuild`.** The workspace's only C-compiling deps in the shipping graphs are `rusqlite`'s bundled SQLite (`Cargo.toml:95`, `libsqlite3-sys`) and the vendored rustls+`ring` pair (`Cargo.toml:91-92`; both verified in `cargo tree -p hekma/-p hekma-host -e normal`). Both compile with a plain C compiler: `apt-get install musl-tools` (musl-gcc) + rustup's target + the two env vars above is the whole mechanism — three lines of CI, one distro package, no container, no second toolchain. `cross` (docker-per-target images, a pinned wrapper binary, cache/canonical-`+stable` friction) and `cargo-zigbuild` (a zig download, an unusual linker with its own C/asm failure modes) buy multi-target breadth this story does not need — it adds exactly ONE target.
- **NFR-8 posture (explicit):** the mechanism is BUILD-TIME ONLY. No crate is added, no feature flipped, no lockfile byte changes; musl-tools exists only on the release runner. The "lean runtime deps" gate is untouched, and the empty Cargo diff is the proof the PR records. This is the same posture the spine already took for CI-side tooling (tarpaulin, semver-checks): a build mechanism, not a dependency.
- **What ships static:** `hekma` + `hkm` (the CLI and its alias, the `kt` surface) AND `hekma-host` (the standalone host binary that exists precisely so a container can run the host without the CLI — `crates/hekma-host/src/main.rs:3-6`). All three are the container-relevant set; the recipe's `ENTRYPOINT` is `hekma-host` (its `--state-dir`/`--stop-window`/`--metering-listen`/`--listen`/`--bearer-secret`/`--log-capture` surface is flag-for-flag the serve surface, one surface to learn). The glibc archive stays `hekma` + `hkm` — the installer's pinned contract (`scripts/test_automation.py:1138`) — so the story does not disturb existing consumers.
- **The recipe is docs-only.** The proposal asks for "a docs page" (`:103`, `:380`); a checked-in Dockerfile would be an untested artifact with no CI leg to keep it honest — exactly the drift the docs-currency gate exists to prevent. The worked example lives in the page, version-pinned by the reader at copy time.
- **No Rust code changes.** The host surfaces the recipe needs all exist: signals (`signals.rs`), the bounded drain (`drain_and_stop`, `drain.rs`; default 30 s, `crates/hekma-engine/src/domain/supervisor/mod.rs:87`, bounded 1..=86400, `crates/hekma-host/src/lib.rs:101-106`), the lock (`lock.rs`; refusal exit 2, `commands.md:454`), `--state-dir`/env overrides (`serve.rs:32-48`, `main.rs:24-25`), `--log-capture` parity (`main.rs:111-127`). The only reserved decision found was `hekma-host`'s publication (see Ask First).
- **MSRV/stable:** the musl leg builds on latest stable like every release leg (`release.yml:41-45`'s explicit `+stable`); nothing in the workspace is musl-specific (`nix`/`tokio`/`ring` all build there), so no cfg and no code change.

## I/O Impact Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| `v*` tag push | release workflow | 5 matrix legs; musl leg installs musl-tools, builds, packages `hekma`+`hkm`+`hekma-host` | musl-tools install or C-dep build failure → leg fails, other legs unaffected (`fail-fast: false`) |
| Static-ness check | musl leg, post-build | each binary `file`s as statically linked | dynamic → grep fails → job fails before packaging |
| Release body generation | publish job | asset table gains the musl row + sha256; aggregate checksums include it | `test_automation.py` asset-table/workflow pins fail if TARGETS and the workflow disagree |
| Homebrew formula publish | same run | unchanged; formula names only the four Homebrew targets | formula generation tolerates the extra aggregate entry (`:86-89`); the new negative pin keeps it musl-free |
| Docs site build | docs job | new page compiles; nav + collection resolve | broken link / missing registration → `check_docs.py` or the npm build fails |
| Container stop, grace ≥ window | `docker stop -t N`, `N` > `--stop-window` + headroom | SIGTERM → full drain → exit 0 | repeated TERM mid-drain swallowed + noted; drain outcome surfaced per instance |
| Container stop, grace < window | runtime impatience | drain is cut at the grace; the platform SIGKILLs — the docs say so plainly | the metadata crash-window disclosure applies (`commands.md:444`) |
| Second container, same volume | state dir already locked | refusal exit 2 naming the live holder PID | never advises deleting the lock (`commands.md:454`) |
| Agent bypasses the injected `base_url` | any container posture | meter shows the honest gap (no fabricated figures) | docs state the cooperative scope; enforcement stays at the Host's choke point (NFR-6) |

## Code Map

- `.github/workflows/release.yml` — tag trigger (`:3-6`); matrix + glibc leg (`:16-34`, the leg at `:20-22`); `Install Rust` adds `matrix.target` (`:39-40`); Build `+stable` (`:41-45`); Package Unix + per-archive sha256 (`:46-62`); upload (`:77-84`); publish download (`:152-156`); aggregate checksums (`:163`); release upload (`:191-197`).
- `.github/workflows/ci.yml` — the docs job's gates: check_docs (`:607-608`), release-docs smoke (`:609-610`), `test_automation.py` (`:611-612`), docs site build (`:613-614`).
- `scripts/generate_release_docs.py:14-20` — `TARGETS`; `:190-194` — the asset-table rows. `scripts/generate_homebrew_formula.py:24-27` — `HOMEBREW_TARGETS` (unchanged); `:86-89` — the missing-asset check.
- `scripts/test_automation.py` — `:39-44` (asset-table pin), `:87-88` (workflow-contains-target), `:126-129` (checksum-parse fixture), `:139-143` (formula pins), `:1138` (installer's `hekma + hkm` contract).
- `scripts/public/install.sh:282-295`, `:406` — target detection + asset name (UNTOUCHED this story).
- `docs/meta.json:10-16`; `docs/source.config.ts:4-20` — the two page registrations.
- `docs/commands.md` — serve section (`:421`), `--stop-window` (`:432`), metadata crash window (`:444`), state-dir-on-volume (`:448-450`), lock (`:454`).
- `docs/control-api.md:39` — non-loopback bind honesty. `docs/design/metering-agents-you-dont-control.md:94` — "not a sandbox".
- `crates/hekma-host/src/main.rs` — the standalone binary (`:3-6`), env overrides (`:24-25`), flag loop (`:38-131`), usage text (`:170-197`). `crates/hekma-host/src/signals.rs` — Unix TERM/INT (`:21-25`, `:56-68`), repeat-swallow (`:69-79`). `crates/hekma-host/src/lib.rs:101-106` — window bounds. `crates/hekma-host/src/drain.rs` — `drain_and_stop`/`DrainOutcome`.
- `crates/hekma/src/cli/serve.rs:32-48` — the `serve` flag block. `crates/hekma-engine/src/domain/supervisor/mod.rs:87` — `DEFAULT_STOP_WINDOW` (30 s).
- `Cargo.toml:91-95` — rustls+ring, rusqlite bundled (the C deps the musl toolchain must compile). `crates/hekma-host/Cargo.toml:10-13` — `publish = false` and the comment that reserves publication for THIS story.
- `_bmad-output/implementation-artifacts/deferred-work.md:243` — the proposal-deferral section to extend; spine `:359` — the static-aarch64 Deferred item.

## Verification

**Commands (all runnable locally; no musl toolchain assumed):**
- `cargo fmt --all --check` — clean (trivially: no Rust diff expected).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace --all-targets` — pass (no Rust diff expected; proves it).
- `python3 scripts/check_docs.py` — pass (new page: links, no planning provenance).
- `PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_automation.py` — pass (the updated release pins).
- `python3 scripts/generate_release_docs.py v0.0.0 --output-dir /tmp/opencode/release-docs-smoke` — the generated body's asset table carries the musl row (the CI smoke, `ci.yml:609-610`).
- `git diff 46c4c52 -- '**/Cargo.toml' 'Cargo.lock'` — EMPTY output (the NFR-8 posture proof).
- Optional, machine-permitting: `rustup target add x86_64-unknown-linux-musl && CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc CC_x86_64_unknown_linux_musl=musl-gcc cargo +stable build --release --target x86_64-unknown-linux-musl -p hekma -p hekma-host` then `file` the three binaries — the musl leg reproduced locally. A YAML syntax check of `release.yml` (`python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"`, PyYAML) is a bonus; `test_automation.py`'s textual pins are the PyYAML-free structural gate.
- CI-side: the release workflow runs only on `v*` tags (`release.yml:3-6`), so the musl leg's first end-to-end proof (build → static assert → release page) lands with the next tagged release; this story's CI proof is the docs job (the pins + smoke) green on the PR. A maintainer may validate early with a throwaway tag.

## Ask First

- **`hekma-host` stays `publish = false`?** The crate's manifest reserves the publication/version-alignment decision for this story (`crates/hekma-host/Cargo.toml:10-13`). Recommendation: keep it unpublished — the story contract (proposal `:379-384`) asks for release-archive binaries, not a crates.io crate; publication drags a name-availability + version-train decision no container needs. Confirm the deferral stands (publication remains a future human decision).
- **The installer keeps handing out glibc on Linux?** `install.sh`'s `detect_release_target` (`:282-295`) has no musl arm; this story leaves it that way (the static archive is a release-page download for containers/scratch; switching the Linux default to musl touches the installer and its pinned tests and is a real behavioral change). Confirm the scope line — a follow-up "prefer the static archive on Linux" can be deferred with a trigger if wanted.

## Spec Change Log

- 2026-10-20: drafted (subagent, baseline `46c4c52`). Awaiting supervisor review.
- 2026-10-20 (supervisor approval): approved as drafted — anchors spot-verified (release matrix/installer pins/fumadocs double registration all real); the musl-tools mechanism and the empty-Cargo-diff NFR-8 posture are sound; the docs-only recipe and no-Rust-code call are right for the story's shape. Ask First rulings: (1) `hekma-host` STAYS `publish = false` — the container surface is the release archive; publication remains a future human decision (the deferral stands, evidence `crates/hekma-host/Cargo.toml:10-13`). (2) The installer keeps serving glibc on Linux — the static archive is a manual release-page download for containers/scratch; the implementer adds a matching small deferred-work entry ("installer prefers the static archive on Linux", trigger: a Host asks for it) beside the aarch64 deferral in the same change. Spec → in-progress.
- 2026-10-20 (review round, 17 findings — all applied): **release-blocker, empirically proven by the reviewer** — rustc's `x86_64-unknown-linux-musl` default is static-PIE and `file(1)` prints `static-pie linked`, so the drafted `grep -q 'statically linked'` gate would have false-failed a VALID artifact on every tagged release and (publish `needs: build`) skipped the entire publish job; the gate now matches `static(ally|-pie) linked` with the pin flipped in lockstep so they cannot drift. Also: the musl archive's CONTENTS are now asserted (`tar -tzf` requires hekma/hkm/hekma-host before upload — previously only workflow source text was pinned); the musl build env moved to a structurally-gated `$GITHUB_ENV` step (no empty-string exports on the other four legs); the `if: matrix.musl` gating itself is pinned per step. Docs honesty set (AI-18): scratch runs only static agents (dynamic runtimes need distroless/alpine; scratch DNS caveats stated); PID-1 inherits re-parented orphans (`--init`/tini when agents spawn subprocesses); **exit 0 ≠ clean drain** (on-failure restart policies never see a degraded drain — the truth is the per-instance outcome lines); the state dir needs a local/block-backed volume (flock + SQLite vs NFS/EFS); the x86_64-only scope is stated at first mention; `--metering-listen`'s recipe omission is explained (host restart = container restart in the ENTRYPOINT shape); sidecar agents cannot reach a loopback-injected `base_url`; `/v0/version` is bearer-guarded (not a turnkey probe); the two-pass drain math (≈2×Σ(window) worst case) is in the grace guidance; the recipe's alpine fetch stage is digest-pinned. Neighbors: `docs/installation.md` gained the static-archive pointer; the deferred-work aarch64 anchor was refreshed to the post-change matrix range. The Fibery deferral mirror landed in the same change (two new Hardening-backlog tasks: the aarch64 artifacts and the installer-prefers-musl entries). Gates green: fmt / clippy `-D warnings` / workspace tests (no Rust diff — the 17-4 battery's state holds; known env smoke only) / `check_docs.py` 29 / `test_automation.py` 35 / release-docs smoke (musl row) / YAML parse / **empty Cargo diff** (NFR-8 proof, recorded for the PR). Unproven locally (stated honestly): the musl compile itself and the `file` wording end-to-end — `musl-gcc` is absent on this machine; CI's release leg owns the first live proof at the next `v*` tag. Spec → done.

## Suggested Review Order

**The gate that almost broke every release (start here)**

- "Verify static" with the both-wordings regex, the archive-content tar assertions, and the structurally-gated musl env step.
  [`release.yml`](../../.github/workflows/release.yml) — the static gate around `:77-96`, "Configure the musl build env" `:53-63`, "Verify archive contents" `:115-128`

**The pins that keep the workflow honest**

- The musl leg's pin test: the regex pin, the three tar lines, and the per-step `if: matrix.musl` gating assertions.
  [`test_automation.py`](../../scripts/test_automation.py) — the musl test around `:124-165`

**The page**

- The container-supervisor recipe: scratch-vs-runtime agents + DNS, PID-1 reaping, exit-0 honesty, volume semantics, arch scope, the digest-pinned worked example, stop-grace math.
  [`hosting.md`](../../docs/hosting.md) · registrations in [`meta.json`](../../docs/meta.json) + [`source.config.ts`](../../docs/source.config.ts) · pointers from [`installation.md`](../../docs/installation.md), [`commands.md`](../../docs/commands.md), [`control-api.md`](../../docs/control-api.md), [`README.md`](../../README.md)

**The record**

- TARGETS row + asset-table smoke output; the two deferrals + refreshed anchors.
  [`generate_release_docs.py`](../../scripts/generate_release_docs.py) · [`deferred-work.md`](../../_bmad-output/implementation-artifacts/deferred-work.md)
