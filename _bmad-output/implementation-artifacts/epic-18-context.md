# Epic 18 Context: Service-Grade Lifecycle

<!-- Compiled from planning artifacts. Edit freely. Regenerate with compile-epic-context if planning docs change. -->

> **Numbering note:** Epic 18 was renumbered from Epic 16 on 2026-10-19 — remote main's autonomous Fibery-backlog tranches consumed epic numbers 15/16 while this approved 2026-09-30 correct-course output sat uncommitted locally (`_bmad-output/implementation-artifacts/sprint-status.yaml:176-180`). The sprint change proposal's "Epic 16" is today's **Epic 18**; its "Story 16.x" map to 18.x.

## Goal

A service agent is ready later than its process exists, can hang while alive, asks its supervisor to restart it with an exit code, and must drain before a stop — none of which today's lifecycle can express (readiness = a fixed 300 ms survival window; a hang is indistinguishable from healthy; every unrequested exit is a crash; stop has no in-flight-work wait). This epic realizes FR-42 as an additive Adapter Contract minor (v1.1.0). Per the contract's own rule, an older engine still rejects a newer same-major manifest that uses a new section — stated on the release surface (AI-55). Sources: `_bmad-output/planning-artifacts/epics.md:1160-1199`, `_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md` (APPROVED 2026-09-30).

## Stories

- Story 18.1: Readiness probes
- Story 18.2: Liveness and the unresponsive failure
- Story 18.3: Exit-code classes and the stable-window reset
- Story 18.4: Cooperative pause commands and the drain verb
- Story 18.5: A service-mode gateway as a manifest adapter

## Requirements & Constraints

From FR-42 (PRD §4.13) and AD-15 (spine Amendments 2026-09-30):

- **Declared readiness gates `starting → running`**: an HTTP GET with an expected status, a file's age, or a log-line match — each with a timeout. A timeout lands the instance `failed` with the probe named. Honest per-OS semantics where a probe kind needs them.
- **Declared liveness**: a heartbeat file's maximum age, or an HTTP check. A failure moves `running → failed` with cause `unresponsive`; the Restart Policy applies. Liveness never probes a paused instance.
- **Declared exit-code classes**: restart-requested codes (e.g. 75) relaunch WITHOUT charging the crash loop; declared clean codes are not crashes. The consecutive-failure counter resets after a stable running window. FR-9's `always` is recorded as subsumed.
- **Cooperative pause command pair + `drain`**: a declared pause/resume command pair drives pause and resume (still surfaced best-effort when it only stops new work). `drain` = stop new work (the pause command, if declared) → wait for idle, bounded → stop; the outcome records completed versus timed out; NO new lifecycle state.
- **A service-mode gateway as a manifest adapter**: a reference manifest (API server, readiness on its models endpoint, heartbeat liveness, exit-75 restart, cooperative pause) + a conformance-kit fake + TCK sections for readiness, liveness, exit classes and drain. The `hermes`-kind deprecation (14-5) gains its service-mode successor here — removal waits for this story (AD-19 amendment).
- Non-goals that bind: Hekma stays cooperative, not a sandbox (NFR-6); no hosted control plane (PRD §8); the drain composite adds no lifecycle state (AD-15).

## Technical Decisions

- **AD-15 (2026-09-30 amendment)** — the five rules above; lifecycle state machine stays DATA (7 states, no new edges — failures are CAUSES, not states).
- **Adapter Contract v1.1.0** — additive minor; older same-major engines reject manifests using new sections (deny_unknown_fields); the rule is stated on the release surface (AI-55).
- **Existing seams (survey 2026-10-23):** the fixed survival window `READINESS_WINDOW`/`READINESS_POLL` (`supervisor/mod.rs:97-101`) + `watch_startup` (`spawn.rs:631/980`); `starting → running` at `spawn.rs:820-840` (after the ACP handshake's bounded wait — the AD-17 bounded-hold precedent); the reaper's exit classification (`reaper.rs:407-454`, all unrequested exits = `Crashed`) and `plan_restart` (:503, `restart_count` charged, reset only by operator start); `RestartPolicy` Never|OnFailure + `MAX_CONSECUTIVE_FAILURES = 5` (`domain/restart.rs`); pause dispatch by Capability SupportLevel (`supervisor/lifecycle.rs:547+`); hekma-host's `drain.rs` module doc EXPLICITLY reserves the AD-15 drain composite for this epic ("deliberately NOT this" — the shutdown stop-ladder keeps its name); TCK `TckSection` enum (`tck.rs:399`) + the fake binaries' arg-scripted pattern.
- **Naming collisions to respect:** `READINESS_WINDOW` (renamed `STARTUP_SURVIVAL_WINDOW` in 18-1 so "readiness" = the declared probe); `drain` has three existing senses (host shutdown ladder, observed-queue drain, stdio drain) — 18-4's verb is the AD-15 composite and its docs must state the split; `probe_alive` (ProcessBackend) is the OS process-liveness primitive, unrelated to declared probes; `liveness` is free for product use (only a perf-fixture uses it).
- **Execution order:** after 19-1 (which runs first overall per the ratified handoff); 18-1 → 18-5 sequential (each story's sections build on the previous — the gateway manifest in 18-5 declares everything 18-1..18-4 shipped).

## UX & Interaction Patterns

- Terminal product; no new CLI verbs except the drain composite (`kt agent drain`? — 18-4 decides the exact verb shape, CLI-first gate applies).
- Every failure names its probe/class/window (surfaced, not silent — AI-18); no new exit-code numbers (DC-4 pattern).
- `kt agent start` blocks through readiness (bounded); `--detach` returns after the bounded wait, failure surfaced.

## Cross-Story Dependencies

- 18-1 gates 18-2 (liveness reuses the probe runner's kinds), 18-5 (the gateway manifest declares readiness), and partially 18-4 (drain's "stop new work" uses the pause pair).
- 18-3 is independent of 18-1/18-2 (reaper-side classification).
- 18-5 TCK sections cover all four prior stories' behaviors — it closes the epic.
- hekma-host Control API gains NO new routes in 18-1..18-3 (failures visible through status/fleet); 18-4's drain verb surfaces through the API per its story text.
- House gates on every story (AGENTS.md).
