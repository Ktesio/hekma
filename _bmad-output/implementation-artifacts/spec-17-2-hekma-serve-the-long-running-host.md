---
title: '`hekma serve` — the long-running host'
type: 'feature'
created: '2026-10-19'
status: 'done'
review_loop_iteration: 0
baseline_commit: '99a4aa08a7d264fde614939acf419e8355cd8dfc'
context:
  - '_bmad-output/implementation-artifacts/epic-17-context.md'
  - '_bmad-output/implementation-artifacts/spec-17-1-host-mode-spec-the-shim-crate-and-the-lock-model.md'
  - '_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md (§4.4 Story 15.2 → renumbered 17.2)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-20 + Amendments 2026-09-30/2026-10-19; AD-2; AD-13/AD-14/AD-18)'
  - '_bmad-output/planning-artifacts/host-mode-decisions-2026-10-19.md'
  - '_bmad-output/implementation-artifacts/sprint-status.yaml:756-768 (the AI-46 adoption strand this story closes)'
  - '_bmad-output/implementation-artifacts/deferred-work.md:247-249 (one-agent-per-container Host topology)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Hekma supervises only for a command's lifetime (no crash detection, enforcement, or event delivery between commands; `lib.rs:147-152`), and an engine-observed agent adopted after an engine restart holds a dead injected `base_url` (the AI-46 adoption strand, `sprint-status.yaml:756-768`). A Host needs a foreground process owning one Engine continuously: exactly one owner per state directory, a stable metering listen port, a minimal child environment, and a bounded shutdown that never SIGKILLs an agent early (FR-41, `epics.md:1125-1133`).

**Approach:** Build the ratified `hekma-host` crate (AD-2 amendment 2026-10-19, spine :231-233) as the long-running host; expose it CLI-first as `hekma serve`. No HTTP here — the Control API is 17-3 (axum stays out of 17-2's tree); the engine stays transport-agnostic (AD-20) and the CLI source synchronous (embed-clean audit).

## Tasks & Acceptance

**Execution:**
- [x] `crates/hekma-host/` — NEW workspace crate (bin + lib) in root `Cargo.toml` members; deps ONLY `hekma-engine` (public API) + `tokio`. The lib owns a tokio `Runtime` internally (the `Engine`'s internal-runtime shape); the bin is a thin wrapper. Unit tests for new pure logic.
- [x] `hekma serve` — `Commands::Serve` in `crates/hekma/src/lib.rs` + `cli/serve.rs`: a plain BLOCKING call into `hekma_host::serve` (no async/tokio tokens in CLI source; the manifest gains `hekma-host`, not tokio — embed-clean pins hold; extend the audit only if the scan needs the new file enumerated). Flags: `--stop-window <secs>` (default 30) and the fixed metering listen port (below). Tests: parse/dispatch pins.
- [x] The lock — before `Engine::open`, acquire an advisory lock on `<state-root>/host.lock`, the state root via the PUBLIC `EnginePaths` seam (the engine stays the sole path authority; record the new state-dir entry in `paths.rs`'s layout docs, same change). std `File::try_lock` (stabilized 1.89; MSRV 1.96.1 — no new dependency). Held for the host's whole life; a second `hekma serve` on that root is refused with a diagnostic naming the holder PID + remediation, exit 2. Tests: second-owner refusal; release on clean exit; stale-free across killed hosts.
- [x] `metering.listen` — ADDITIVE engine minor (announced under the semver gate): an open-time options seam (new `EngineOptions` + `Engine::open_with_options`; the existing constructors delegate byte-identically) threading an optional fixed loopback PORT through `start_observed_listener` to `ObservedListener::start`. The loopback-only engine-computed-ADDRESS invariant stands (`listener.rs:15-18`): a port, never an address. Default 0 = today's ephemeral. This closes the AI-46 strand — the host re-binds the same port across restarts, so an adopted observed instance's `base_url` stays live (option 2, `sprint-status.yaml:765-767`); the 11-3 stranded-listener diagnostic STAYS. Tests: fixed-port bind; restart re-bind; adopted-instance liveness; ephemeral default unchanged.
- [x] Child environment allowlist — ADDITIVE `SpawnSpec` field (default inherit-all, byte-identical): under serve, children inherit ONLY declared env, not the host's environment; both backends apply it clear-then-apply (OS code stays in `backends/`, AD-4). Tests: declared-only under serve; plain CLI inherit unchanged; Windows leg compile-check per the 11-5 pattern.
- [x] SIGTERM drain-then-stop — the host owns process signals (AD-20; the engine still may not): on SIGTERM (and SIGINT, the foreground equivalent), enumerate running instances (`Engine::fleet`) and stop each via `Engine::stop(name, window)` with `--stop-window` (the bounded ladder). The `Engine` is NOT dropped until every stop settles (the `Drop` teardown — never before the window ends). "Drain" here is the bounded stop ladder; the AD-15 drain composite arrives with Epic 18 (16-4). Tests: slow-exiting fake agent stopped within the window, not killed early; no-instance shutdown; repeated signal is idempotent.
- [x] Diagnostics — every host notice (started + state root + lock held, signal received, per-instance stop outcome, shutdown complete, refusals) through the `DiagnosticSink` installed at `Engine::open_with_diagnostics`; nothing silent (AI-18). Tests: emission + refusal-wording pins.
- [x] CI + docs in the same change — the boundary gate already allowlists `host` (`ci.yml:658`; pin `scripts/test_automation.py:545`): verify green with the new `hekma → hekma-host` edge (update the gate's error wording), record the actual `cargo tree` diff in the PR. Docs: a `hekma serve` section in `docs/commands.md` + README hosting mention (the container page is 17-5's). Coverage ≥ 95% (tarpaulin aggregate).

**Acceptance Criteria:**
- Given a host serving a state directory, when a second `hekma serve` starts on that root, then it is refused naming the holder PID + remediation and exits nonzero without opening an Engine.
- Given `hekma serve` with running instances, when SIGTERM arrives, then every instance stops within the bounded window, no agent is SIGKILLed by the host's exit before the window ends, and each outcome is surfaced.
- Given a fixed `metering.listen` port, when the host restarts and an observed instance is adopted, then its `base_url` stays live against the re-bound listener; given the port is unset, behavior is byte-identical to today.
- Given serve-mode spawn, a child's environment is exactly its declared keys; given plain `hekma agent start`, child env behavior is unchanged.
- Given 17-2's diff, `cargo tree -p hekma-host -e normal` shows only hekma-engine + tokio trees, axum is absent workspace-wide, and the AD-2 gate is green as wired.
- Given handoff, the four house gates pass with coverage ≥ 95%.

</frozen-after-approval>

## Design Notes

- **Why `hekma serve` is a CLI subcommand delegating to the `hekma-host` lib** (resolved, not Ask First): the story title is `hekma serve` (`epics.md:1125`; epic-context UX) and CLI-first is a house gate; the ratified AD-2 amendment added `host` to the allowlist scanning `cargo tree -p hekma` (`ci.yml:644,:658`) — an entry with no effect unless the `hekma` crate may take that edge. AD-20's "the `hekma` CLI stays synchronous" protects the embed-clean audited shape (no tokio dep in `crates/hekma/Cargo.toml`, no async/tokio/`Runtime::new` tokens — `embed_clean.rs:1428-1487`), which a blocking `hekma_host::serve()` satisfies: the host owns the runtime, as sync kt already drives `Engine`'s internal runtime via `blocking()`. The ratified "bin + lib" gives `hekma-host`'s bin to the container story (17-5).
- **No new dependencies:** the lock is std `File::try_lock`; tokio is already the engine's runtime (AD-13); axum lands with 17-3 under the ratified NFR-8 review.
- **Signals per OS (supervisor note):** Unix handles SIGTERM and SIGINT (the foreground equivalent); Windows has no SIGTERM — `ctrl_c` is the equivalent console signal (tokio's `signal::windows`). The bounded stop ladder is identical on both.
- **`--stop-window` default derives from the engine's `DEFAULT_STOP_WINDOW` constant** (cited in the Code Map) — the flag cites it, never re-hardcodes a number.
- **One fixed port, many instances:** with the first Host's one-agent-per-container topology (`deferred-work.md:247-249`), one fixed port is the honest minimal surface; a second concurrent observed instance collides at bind and surfaces the typed bind error at start (Ask First #2).

## I/O Impact Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Clean serve | free state root | lock acquired; Engine opened (adoption runs); notices on the sink; runs until signal | lock/open failure: named diagnostic + exit 2, nothing spawned |
| Second owner | host already on that root | refusal naming holder PID + remediation | exit 2; no Engine opened |
| SIGTERM with instances | N running agents | each stopped within `--stop-window`; outcomes surfaced; exit 0 | failed stop surfaces the reason per instance |
| Fixed port + restart | adopted observed instance | `base_url` stays live; metering continues | second observed instance: surfaced bind error naming the port |
| Invalid flag / window 0 | parse-time | clap error + exit 2 before lock or Engine work | same |

## Code Map

- `crates/hekma/src/lib.rs` — `Commands` (:92), dispatch (:324); `Start`'s detach honesty doc (:147-152), the supervision-gap wording serve retires.
- `crates/hekma/src/cli/agent.rs` — CLI engine pattern: `Engine::open` + `.blocking()` (:202, :480); `note_if_best_effort` stderr precedent.
- `crates/hekma-engine/src/engine.rs` — `Engine` (:139), internal runtime (:141), `Drop` teardown (:149-176), `open` (:208), `open_with_diagnostics` (:227), `open_inner` (:239), `fleet` (:585), `stop` (:941).
- `crates/hekma-engine/src/paths.rs` — `EnginePaths` (:253), resolution (:331-341): the state-root seam + the layout-docs convention `host.lock` records in.
- `crates/hekma-engine/src/domain/supervisor/spawn.rs` — `start_observed_listener` (:862-902; bind :902), the port seam.
- `crates/hekma-engine/src/metering/listener.rs` — AC-B bind docs (:13-22); `ListenerError::BindFailed` (:205-210); `start` (:279); `base_url` (:338).
- `crates/hekma-engine/src/ports/process_backend.rs` — `SpawnSpec` (:46-52); apply sites `backends/unix/mod.rs:166-171`, `backends/windows/mod.rs:321-322`.
- `crates/hekma-engine/src/domain/supervisor/lifecycle.rs` — `Supervisor::stop` ladder (:86); `DEFAULT_STOP_WINDOW` (re-export `domain/mod.rs:68`).
- `crates/hekma-engine/tests/embed_clean.rs` — CLI sync-only audit (:1428-1487): what `serve.rs` must satisfy.
- `.github/workflows/ci.yml` — allowlist `|host` (:658), gate wording (:666), tarpaulin ≥95 (:1246, :1307-1308); `scripts/test_automation.py:545` pin (already `|host`).
- `_bmad-output/implementation-artifacts/sprint-status.yaml` — AI-46 strand (:756-768); 17-2 tracker (:182).

## Verification

**Commands:**
- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace --all-targets` — pass, including the new lock/port/env/shutdown pins.
- `python3 scripts/check_docs.py` — pass.
- `cargo tree -p hekma-host -e normal` — hekma-engine + tokio trees only; `cargo tree -p hekma -e normal | grep axum` — empty.
- `cargo run -p hekma -- serve --help`, plus a two-terminal smoke (serve; second serve refused; SIGTERM stops a fake agent within the window) — the AC behaviors.

## Ask First

1. **The spine's axum tree-diff timing.** ~~AD-20's settled note puts the axum `cargo tree` diff "in 17-2's PR" (spine :195-198), but 17-2 ships no axum (17-3's dependency per this spec's scope). Confirm the diff records in **17-3's** PR; 17-2's PR records only the `hekma-host`/tokio edge diff.~~ **Ruled by the supervisor 2026-10-19:** the diff records in **17-3's PR** (where axum enters). The spine's two references are an application cross-reference slip, not a decision change — corrected in this story's change (spine :196 + :241-242) with an erratum line in the proposal's §5 record. 17-2's PR records only the `hekma-host`/tokio edge diff.
2. **Fixed-port collision semantics.** ~~Option A (this spec's default): one fixed `metering.listen` port; a second concurrent engine-observed instance fails its start with the surfaced bind error — minimal, matches one-agent-per-container. Option B: a port-base with per-instance offsets (a larger, separate engine surface). Silence = A.~~ **Ruled by the supervisor 2026-10-19: Option A** — the surfaced typed bind error matches AI-18 (surfaced, not silent) and the one-agent-per-container topology; auto-offsets would hide operator error.

## Spec Change Log

- 2026-10-19 (supervisor approval pass): both Ask Firsts ruled — #1 the axum tree diff records
  in 17-3's PR (spine cross-reference slip corrected in this change: AD-20 settled note +
  Amendments 2026-10-19 bullet, plus an erratum line in the proposal's §5 record; the ratified
  decision itself is unchanged); #2 Option A (surfaced typed bind error; AI-18; one-agent-per-
  container). Design Notes gained the per-OS signal mapping (Unix SIGTERM+SIGINT; Windows
  ctrl_c) and the `--stop-window` default deriving from `DEFAULT_STOP_WINDOW` (cite, never
  re-hardcode). Spec approved → in-progress.
- 2026-10-19 (review round, ~30 findings triaged — 23 patches / 2 deferrals / rest absorbed
  or rejected): the lifecycle set was load-bearing — signals now arm BEFORE `Engine::open`
  (a signal during adoption is captured and held; registration failure is typed, not a
  panic); repeat signals during drain are swallowed, not lethal; stops are issued
  concurrently and joined with honest per-instance outcomes (`ExitedOnItsOwn` ≠ `Failed`)
  plus a bounded newcomer pass (surfaced, never silently Dropped); startup failures split
  usage (2) vs system (1) per the house exit table — SUPERSEDES the frozen I/O matrix's
  draft "exit 2" for lock/open failure (consistency fix, flagged in the PR body for the
  merge review); `--stop-window` bounded 1..=86400 on every surface; the env allowlist
  gained a documented OS baseline floor (Unix PATH/HOME; Windows SystemRoot/SystemDrive/
  ComSpec/TEMP/TMP/PATH) so real children start; the lock refusal names the live holder
  with no file-removal advice; the reaper re-bind decision extracted to a pure seam
  (unit-tested everywhere — tarpaulin runs linux-only where the e2e skips); supervisor-path
  port-collision, exit-0-on-failed-stop, update-check exemption, and CLI flag legs all
  pinned. Deferrals (deferred-work.md, with triggers): real Windows Ctrl+C e2e (console-
  event harness), and parallel stop ladders (the supervisor mutex serializes escalated
  stops — Σ(window) worst case; an AD-18 draft-then-ratify matter, the host-side
  issue-all/join seam is ready). Gates green; spec → done.
- 2026-10-19 (Windows CI verdict on the holder record): the "refusal names the live
  holder" AC is platform-conditional — `File::try_lock` maps to `LockFileEx` on Windows,
  whose byte-range locks are PER-HANDLE, so the live holder's `pid=` record is unreadable
  through a second handle while the lock is held and the refusal honestly carries
  `holder: None` there (the module's documented best-effort-record case; the record
  becomes readable again once the holder releases or dies). The safety property (the LOCK
  refuses; the first holder is unaffected) is platform-uniform and pinned in-process on
  both; the pid-naming leg asserts `Some(own pid)` on Unix and `None` on Windows — a
  runtime branch, not `#[cfg]`, per the boundary gate's data-driven OS rule. No product
  code changed: test expectation + doc comments only.

## Suggested Review Order

**The host lifecycle (entry point)**

- The whole serve flow — window validation → state root → lock → runtime → armed signals → engine open → wait → drain. Signals arm BEFORE the engine opens (review round's load-bearing fix).
  [`hekma-host/src/lib.rs:267`](../../crates/hekma-host/src/lib.rs#L267)

- The per-OS shutdown-signal source (SIGTERM/SIGINT on Unix, ctrl_c on Windows); registration failure is a typed startup error.
  [`signals.rs`](../../crates/hekma-host/src/signals.rs)

**The drain**

- Concurrent issue + join (`JoinSet`), honest per-instance outcomes, the bounded newcomer pass — and the TRUE wall-clock disclosure (escalated stops queue under the supervisor mutex; deferred with trigger).
  [`drain.rs:143`](../../crates/hekma-host/src/drain.rs#L143)

**The single-writer lock**

- Advisory `File::try_lock` before `Engine::open`; refusal names the live holder PID — no file-removal advice.
  [`lock.rs:64`](../../crates/hekma-host/src/lock.rs#L64)

**The engine seams (additive, byte-identical defaults)**

- The fixed metering listen PORT threaded to `ObservedListener::start_on_port` — never an address; collisions are typed `ObservedMetering` refusals, never silent hops.
  [`spawn.rs:916`](../../crates/hekma-engine/src/domain/supervisor/spawn.rs#L916)

- The AI-46 closure's decision seam — pure, unit-tested everywhere (the e2e keeps its #109 skips).
  [`reaper.rs:828`](../../crates/hekma-engine/src/domain/supervisor/reaper.rs#L828)

- The child-env allowlist with the OS baseline floor: clear → floor → declared.
  [`unix/mod.rs:188`](../../crates/hekma-engine/src/backends/unix/mod.rs#L188) · [`windows/mod.rs:349`](../../crates/hekma-engine/src/backends/windows/mod.rs#L349) · [`process_backend.rs:182`](../../crates/hekma-engine/src/ports/process_backend.rs#L182)

**CLI + exit codes + docs**

- The blocking serve subcommand (sync-only CLI source; embed-clean holds).
  [`serve.rs:34`](../../crates/hekma/src/cli/serve.rs#L34)

- The exit-code split (usage vs system) with the finding-8/9 rationale inline.
  [`exit_code.rs:28`](../../crates/hekma/src/exit_code.rs#L28)
