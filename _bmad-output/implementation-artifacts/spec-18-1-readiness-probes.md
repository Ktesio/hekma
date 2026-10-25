---
title: 'Readiness probes — declared readiness gates starting → running (Adapter Contract v1.1.0)'
type: 'feature'
created: '2026-10-23'
status: 'review'
review_loop_iteration: 1
baseline_commit: '9275d843b2394fda60e0556dbf8378067862f36f'
context:
  - '_bmad-output/implementation-artifacts/epic-18-context.md'
  - '_bmad-output/implementation-artifacts/spec-19-1-governance-conformance-vectors.md (the shared-decision-core precedent)'
  - '_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md (§4.4 Story 16.1 → renumbered 18.1; §4.1 the contract-minor rule)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-15 amendment 2026-09-30: declared readiness gates starting → running)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** A service agent is ready later than its process exists — it must bind a port, load models, or write a ready line before it can serve. Today `starting → running` fires when the process has merely survived a fixed 300 ms window (`watch_startup`, spawn.rs:980; `READINESS_WINDOW`, supervisor/mod.rs:98) — an agent that is alive but not yet ready is reported `running`, and one that never becomes ready stays `running` forever (epics.md Epic 18 preamble).

**Approach:** Adapter-DECLARED readiness probes gate the transition (AD-15 amendment, ratified 2026-09-30): a new `[readiness]` manifest section (Adapter Contract additive minor **v1.0.0 → v1.1.0**) declares one probe — an HTTP GET with an expected status, a file's maximum age, or a log-line match — each with its own bounded timeout. The spawn path runs the probe after the survival window (and after the ACP handshake, whose session-exists gate is unchanged): the instance reaches `running` only when the probe passes; a timeout lands it `failed` with the probe NAMED (a launch-failure-shaped terminal, not an auto-restart — restart-policy classes are 18-3). No probe declared → byte-identical behavior to today. Honest per-OS semantics where a probe kind needs them (mtime granularity). Per the contract's own rule an older engine still rejects a v1.1.0 manifest that uses the new section — stated on the release surface (AI-55).

## Tasks & Acceptance

**Execution:**
- [x] Adapter Contract v1.1.0 — `crates/hekma-adapter-api/src/manifest.rs`: new `Readiness` section struct (`#[serde(deny_unknown_fields)]`, the house style) — exactly ONE probe per manifest, shaped `{ kind, timeout_ms }` where kind is a tagged enum: `http_get { url, expected_status }`, `file_fresh { path, max_age_ms }`, `log_contains { line }`. Validation: `timeout_ms` in `1..=600_000`; `http_get.url` must be `http://` (the loopback-probe posture; no TLS, no other schemes); `file_fresh.max_age_ms <= timeout_ms`; `log_contains.line` non-empty, no NULs. `CONTRACT_VERSION` → `1.1.0`; negotiation unchanged (major match; minor/patch additive) — an ENGINE at 1.0.x still REJECTS a manifest declaring the section (deny_unknown_fields gives this for free — pin it with a test). Manifest docs + freeze ledger updated (`docs/manifest.md`, `docs/adapter-contract.md`).
- [x] Engine probe runner — `crates/hekma-engine/src/domain/readiness.rs` NEW: a pure-decision + bounded-I/O probe module. Poll loop (fixed engine-owned cadence, `READINESS_POLL` sibling) bounded by the declared timeout, checking process liveness each pass (an exit during the probe wait takes the EXISTING crash path — the probe never masks a death). Kinds: `http_get` via the engine's existing hyper client stack (no new dependency; total request timeout = remaining budget; ANY response with the expected status passes; connection-refused = not-yet, NOT a failure), `file_fresh` via std `fs::metadata` modified-elapsed ≤ max_age (per-OS honesty: Windows FAT 2 s mtime granularity documented; the check is monotonic-clock-based, `SystemTime` deltas only), `log_contains` via the engine-owned agent-log path (bounded incremental read from the spawn cursor; substring match — the engine is regex-free by design). Return type: `Ready | TimedOut { probe_description, waited_ms } | ProcessExited(exit)` — pure-rendered description ("readiness probe http_get http://127.0.0.1:8080/health (expected 200)").
- [x] Spawn-path wiring — `supervisor/spawn.rs`: after the survival window and the ACP handshake, if a probe is declared, run it; on `Ready` → today's `starting → running` (cause unchanged); on `TimedOut` → the `fail_launch_detail` path with the probe named in the detail AND a NEW `TransitionCause::ReadinessTimeout { probe, waited_ms }` on the terminal event (additive cause variant; event schema note in docs); on `ProcessExited` → the existing crash path verbatim. The probe wait runs under the SAME lock posture as the ACP handshake (the AD-17 bounded-wait shape — a never-ready agent refuses the start within its declared timeout instead of wedging supervision). Detached starts: the probe runs in the same in-call spawn path (the survival window already does), so `--detach` returns ready-or-failed, bounded — the CLI note names the wait.
- [x] Facade + surfaces — no new facade verb (readiness rides `start`); `instance_status`/fleet carry the readiness outcome when it failed (the cause + probe name in the status detail, the same projection `LaunchError` detail uses today). Control API: unchanged routes (the failure is visible through existing status/fleet projections). CLI: `kt agent start` output notes the probe wait when declared (one line, the surfaced-not-silent rule); `kt agent show` names the probe on a readiness failure.
- [x] Conformance fixtures — `crates/hekma-conformance/src/bin/fake_agent.rs`: `--http-ready PORT:STATUS` (a one-shot pure-std TcpListener that answers one GET then closes — no hyper), `--ready-file PATH:DELAY_MS` (writes a fresh file after a delay), `--ready-line LINE:DELAY_MS` (prints the line after a delay), and `--never-ready HANG_MS` (binds nothing, writes nothing). Pure std, the arg-scripted pattern.
- [x] Tests — unit: probe validation matrix, description rendering, timeout math (deadline never exceeded), log-cursor incremental read; integration (`tests/lifecycle.rs` sibling or a new `readiness.rs` test file in hekma-engine): http-pass → running; http-timeout → failed with probe named in state + event cause; file-fresh pass/timeout; log-contains pass/timeout; exit-during-probe → crash path (not a probe timeout); no-probe byte-identity (the 300 ms window alone, unchanged events); v1.0.0 engine rejects the new section (negotiation pin); detached start returns failed on timeout (bounded). CLI e2e: start on a never-ready fake → exit code + stderr name the probe (exit-code class: InvalidState family per the frozen table — NO new numbers).
- [x] Docs + release surface — `docs/manifest.md` (the section reference), `docs/adapter-contract.md` (v1.1.0 + the older-engine rejection rule + freeze ledger entry), `docs/commands.md` (start's probe wait + the detached note), `docs/agents.md` if the example manifests change, `CHANGELOG.md` + `RELEASE_NOTES.md` ANNOUNCE the contract minor and the older-engine rejection rule (AI-55 two-pass on the release surface). Engine version: the additive `TransitionCause` variant + readiness module ride the 19-1-pending 0.9.0 (or bump again if 19-1 already shipped it — take whatever the workspace table says, additive minor).

**Acceptance Criteria:**
- Given a manifest declaring an HTTP probe against a fake that serves it, the instance reaches `running` only after the probe passes (the event order proves it: probe pass precedes `starting → running`).
- Given a never-ready agent with a declared probe, the instance lands `failed` with the probe named in the status detail AND `TransitionCause::ReadinessTimeout` on the terminal event, within the declared timeout (never later).
- Given an agent that EXITS during the probe wait, the existing crash path runs (crash cause, restart policy) — the probe never masks a death.
- Given a manifest with NO probe, behavior is byte-identical to today (events, timings, exit codes — pinned by a test).
- Given an engine built at contract 1.0.0 (simulated by the negotiation pin), a manifest declaring `[readiness]` is rejected with a diagnostic naming the section.
- Given handoff: the four house gates pass; coverage ≥ 95%; docs + release surface state the v1.1.0 rule (AI-55).

</frozen-after-approval>

## Design Notes

- **Terminal-failure posture (the load-bearing ruling).** A readiness timeout lands `failed` like a launch failure — NO auto-restart. Settled text says only "lands the instance in `failed` with the probe named"; restart-requested/clean classes and the stable-window reset are 18-3's verbs, and inventing a restart here would pre-empt that story's contract. An operator (or the Host) restarts deliberately.
- **One probe, not a list.** The epic names three kinds as alternatives ("an HTTP GET … a file's age, or a log-line match"), not a pipeline. One `kind` keeps validation, description-rendering, and the failure naming honest; a probe LIST (ordered gates) is a forward seam the section's shape (a table) can grow into without a breaking change.
- **Ordering: survival window → ACP handshake → probe.** The 300 ms window stays ALWAYS (process-level liveness, unchanged for no-probe manifests); the probe runs last so an acp instance probes with its session established (14-1's "running only once a session exists" precedent) and a plain instance probes a process that already survived infancy. Lock posture copies the handshake's bounded-wait ruling (AD-17): the declared timeout bounds the hold.
- **Engine-owned cadence, declared budget.** The manifest declares only the timeout (operator intent); the poll interval is engine mechanics (`READINESS_POLL` sibling, ~100 ms), like the survival window today. Fewer manifest knobs, no timing contracts to freeze.
- **`log_contains` reads the ENGINE-owned attributed capture path** (agent.log), from the spawn-time cursor — the same file the log surfaces read; NOT the raw agent stdout (which may be metadata-mode-off in 17-4 capture modes — document that `log_contains` under `capture = "off"` is unsatisfiable and the start fails at timeout, honestly named).
- **HTTP probe posture:** loopback-first (`http://` only) — the probe targets the AGENT's own listener on the same host; TLS and remote URLs are refused at validation (a remote readiness target is a Host concern, not an adapter contract one). Connection-refused is a not-yet (the port may not be bound yet), not an error — only the timeout fails.
- **Naming.** `readiness` section, `ReadinessTimeout` cause, `readiness.rs` module. The existing `READINESS_WINDOW` const is RENAMED to `STARTUP_SURVIVAL_WINDOW` (with a comment naming 18-1) so "readiness" unambiguously means the declared probe — the collision the survey flagged. No other renames.
- **Per-OS honesty.** mtime granularity (Windows FAT 2 s / NTFS 100 ns) documented in the section's docs; `file_fresh` compares `SystemTime` deltas only (no wall-clock arithmetic); HTTP + log probes are OS-neutral by construction. No `#[cfg]` outside the house backends (OS-cfg gate).

## I/O Impact Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Probe passes (http) | fake serves `expected_status` within timeout | `starting → running` after the pass; unchanged ready cause | probe error ≠ pass; only status match passes |
| Never ready | no bind / no file / no line | `failed` + `ReadinessTimeout{probe, waited_ms}` within timeout | detail names the probe + expected condition |
| Exit during wait | process dies mid-probe | the EXISTING crash path (crash cause, restart policy) | probe result discarded, honestly |
| No probe declared | v1.1.0 manifest without the section | byte-identical today (pinned) | — |
| v1.0.0 engine + the section | simulated negotiation | rejected, diagnostic names `[readiness]` | contract-incompatible class |
| Bad manifest | timeout 0 / https URL / empty line | registration refused with the field named | validation before any spawn |
| `--detach` + probe | never-ready fake | start returns after the bounded wait with the failure surfaced | same terminal state as attached |
| `capture = "off"` + `log_contains` | 17-4 capture mode off | unsatisfiable → timeout failure, docs state it | named in the probe description docs |

## Code Map

- `crates/hekma-adapter-api/src/manifest.rs` — `Manifest` (:45), `Lifecycle` (:88), `OpTemplate` (:102), the section-struct pattern (:74-188); `src/lib.rs:104` `CONTRACT_VERSION`.
- `crates/hekma-adapter-api/src/lib.rs:184` — `negotiate_contract_version` (unchanged; the rejection pin tests it).
- `crates/hekma-engine/src/domain/readiness.rs` — NEW (probe runner + pure validation/description fns).
- `crates/hekma-engine/src/domain/supervisor/mod.rs:97-101` — `READINESS_WINDOW`/`READINESS_POLL` (rename + the probe sibling cadence).
- `crates/hekma-engine/src/domain/supervisor/spawn.rs` — `watch_startup` call (:631), the ACP handshake block (:716+), `starting → running` (:820-840), `fail_launch_detail` (the timeout terminal path).
- `crates/hekma-engine/src/domain/event.rs:99-220` — `TransitionCause` (the new `ReadinessTimeout` variant + its wire/render).
- `crates/hekma-engine/src/domain/error.rs:279` — `EngineError` (if the timeout needs a typed arm beside the launch-detail path — follow `LaunchError`'s shape).
- `crates/hekma-engine/src/metering/listener.rs` — the existing hyper client usage pattern (the probe's HTTP leg reuses this stack; NO new dependency).
- `crates/hekma-conformance/src/bin/fake_agent.rs` — the new fixture flags (pure std: `TcpListener`, file write, stdout line).
- `crates/hekma/src/cli/agent.rs` — start's probe note + show's failure naming (the `note_if_best_effort` surfaced-note pattern).
- `docs/manifest.md`, `docs/adapter-contract.md`, `docs/commands.md`, `CHANGELOG.md`, `RELEASE_NOTES.md`.

## Verification

**Commands:**
- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` — pass, including the readiness matrix + the byte-identity pin.
- `python3 scripts/check_docs.py` — pass.
- Empirical probe drills (record in the PR): (a) never-ready fake → `kt agent start` fails within timeout + `show` names the probe; (b) ready fake → running only after the ready event; (c) exit-during-probe → crash cause, not probe timeout.

## Ask First

None. The AD-15 amendment is ratified verbatim; the terminal-failure posture, one-probe shape, ordering, and naming resolve from settled text as recorded in Design Notes. The contract minor itself was ratified at proposal level ("additive Adapter Contract minor (v1.1.0)… stated on the release surface").

## Spec Change Log

- 2026-10-23 (draft): authored against main 9275d84 (stacks on 19-1's branch for the engine-version slot); autonomous mode per Islam's 2026-10-23 cadence ruling.
- 2026-10-24 (review round 1): the 4-layer adversarial review returned 30 patches in 8 groups (A–H), ALL applied in this round's single commit — A: the HTTP probe parses the status line as soon as `\r\n` arrives (keep-alive answers pass immediately, no EOF wait), a hard deadline wall bounds every socket op (the module-doc "no overshoot" claim rewritten to scheduling-only honesty), the authority parses as a literal `IpAddr` (no `to_socket_addrs`/DNS), fragments rejected at validation. B: manifest validation enforces the literal loopback authority (127.0.0.0/8 or ::1, bracketed IPv6, no userinfo, no hostnames incl. `localhost`), `expected_status` 100..=599, `max_age_ms` ≥ 1, C0/C1 control characters rejected in `url`/`path`/`line` (the "NUL" wording kept), the wrong "600_001 is not TOML syntax" comment corrected (TOML allows digit separators — the range check is the rejection), `file_fresh` requires a regular file. C: timeout bounds tightened (≤ budget+1500 ms), byte-identity ceiling 2 s, a unit drill proves `run_probe` honors its budget, the file-fresh pass test now proves a genuine wait (marker at 1 s, ≥ 800 ms). D: wrong-status (503) probe refuses the start; detached never-ready + detached-late-pass drills; `resolve_readiness_probe` unit tests (drift + native); a PASS diagnostic ("readiness probe passed after Nms") beside the wait-begins note; the 1.0.0-manifest-with-readiness compat pin; a metadata-mode log-contains test; `fake_agent --never-ready` fails fast on a malformed value. E: the TimedOut arm re-polls once before condemning (an exit takes the crash path), teardown escalates to a zero-window force-kill, a queued restart plan is announced with the engine-cadence caveat, the poison-recovery doc states its honest scope, poll errors surface in the timeout detail. F: `waited_ms` doc states actual-elapsed; the `readiness-timeout` serde-tag old-reader break announced (CHANGELOG + RELEASE_NOTES); the conformance report's `contract_version` wire value 1.0.0 → 1.1.0 announced; three stale `Readiness::probe` refs fixed to `Readiness::gate`. G: `log_contains` rescans from 0 when the log shrank below the cursor (rotation honesty); the acp JSON-RPC needle-matching caveat documented. H: docs updated in step (manifest rows, adapter-contract rules, the supervision-pause note for multi-instance hosts). Recorded rulings from the review: the pass diagnostic doubles as the AC "probe pass precedes `running`" ordering evidence; AC2's "never later" is literally enforced by test; the acp+readiness combined ordering stays code-order with per-stage pins; path traversal in `file_fresh` was REJECTED as a concern (the manifest is a trusted surface). Corrections to the frozen block recorded here (the block is frozen): the `--http-ready` fixture is NOT "one-shot" — it is a persistent accept loop answering every connection for the process's lifetime (the engine's probe reconnects on each poll); this spec's frozen Task text overstates it, and `fake_agent`'s docs now say the honest thing. 3 items deferred to `deferred-work.md` (the supervision-lock hold, the killed-mid-probe orphan, the `free_port` TOCTOU).
