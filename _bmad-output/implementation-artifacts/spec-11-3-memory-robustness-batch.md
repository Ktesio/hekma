---
title: '11-3 Memory robustness batch — memory.dir strip, migration atomicity, conflict pin, adoption stranding'
type: 'bugfix'
created: '2026-09-11'
status: 'done'
review_loop_iteration: 0
baseline_commit: '4c498502dd215d466859c63c0180b7894d7ab0fc'
context:
  - '{project-root}/_bmad-output/implementation-artifacts/epic-11-context.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Four retro items (epic-5-retro-items 1/3/4 + AI-46) record memory and persistence robustness gaps: a hand-set `memory.dir` leaks into the delivered config when invocation overrides force a config re-fold, a mid-migration crash can brick the state DB (non-idempotent DDL re-runs against half-applied schema), the registry's reverse memory-kind conflict is untested while a tracked note misstates that fact, and an adopted engine-observed instance silently keeps calling a dead loopback port (stranded listener).

**Approach:** Strip `memory.dir` in the invocation-override re-fold too; wrap each schema-version step in a `BEGIN IMMEDIATE` transaction (making a crashed migration resumable as the comments already claim); add the reverse-conflict registry test and correct the deferred-work note; and on adoption of an engine-observed instance, surface the dead-upstream condition loudly (diagnostic naming the stranded listener + remediation) instead of leaving the silent strand — the engine cannot rewrite the adopted child's already-injected env, so honest surfacing is the coherent fix.

## Boundaries & Constraints

**Always:** Each item ships with a test asserting the new behavior. The migration fix must preserve all existing migration tests (v1→v2 row preservation, forward-compat) and make a crash-mid-migration DB reopen successfully with the correct `user_version`. The AI-46 diagnostic goes through the existing `emit_diagnostic` AD-12 channel and its `tests/embed_clean.rs` allowlist entry lands in the same change. Docs updated in the same change: `docs/commands.md` memory.dir strip claims (become true — verify), `docs/architecture.md` adoption-limit sentence (rewrite for the new diagnostic). Full battery green (fmt/clippy/tests/check_docs).

**Ask First:** Any change that relaunches adopted processes at `Engine::open` (a behavior change beyond surfacing). Any schema change beyond transactional wrapping of the existing DDL.

**Never:** No new dependencies. No change to `resolve_config_mapping` precedence or the strip semantics for the base (non-override) path — the base strip at `supervisor.rs` start_inner is already correct. Do not touch the observed-listener lifecycle beyond the adoption-branch diagnostic.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Hand-set memory.dir + invocation overrides | operator ran `config set svc memory.dir <path>`; instance is engine-observed (override branch refolds all layers) | the refolded mapping is stripped of `memory.dir` exactly like the base path; the decoy value reaches neither the delivered env/config nor the effective-config snapshot | no silent leak (A1) |
| Crash mid-migration | process dies between applying version N's DDL and the `user_version` stamp (simulated: tables exist, `user_version` = 0) | reopen succeeds: remaining steps apply inside their transactions, version stamps correctly, prior rows survive | no "table already exists" failure (B3) |
| Reverse memory-kind conflict | backing row says `native`; `attach_memory(kind: filesystem)` requested | `RegistryError::MemoryBackingKindConflict { attached: "native", requested: "filesystem" }`; existing row/status untouched | symmetric to the forward case (A5) |
| Adopted engine-observed instance | engine restarts; orphan adopted live; its injected `base_url` points at the previous engine's dead listener | adoption emits a diagnostic naming the stranded observed listener + the dead port and the stop/start remediation; instance marked un-observed (as today) | not silent (AI-46); embed_clean allowlist extended in same change |

</frozen-after-approval>

## Code Map

- `crates/ktesio-engine/src/domain/supervisor.rs` -- `start_inner`: base strip at ~888 (`effective.remove(MEMORY_DIR_KEY)`); the GAP is the override fold ~968–976 where `registry.effective_config` re-derives `mapping_effective` unstripped before `apply_config_mapping` (~1007); DC-10 notice ~978–993 (pattern for the new adoption diagnostic's wording style); `emit_diagnostic` ~737; `adopt_orphans` live-match branch ~2448–2522 — comment ~2479–2495 documents the stranding verbatim, `observed_listener: None` at ~2496; AI-46 builds on 11-1's cloned `name`/`run_id`/`metering_source` locals + the AI-44 `enforce_budget` call at ~2521 — implement ON TOP of the current tree
- `crates/ktesio-engine/src/store/sqlite.rs` -- `migrate()` ~312–351: per-version `execute_batch(SCHEMA_VN)` then a single `PRAGMA user_version = 5` stamp; stale comment ~324–327 claims resumability the code doesn't have; `SCHEMA_V1` ~55–83 (plain CREATE TABLE), V3/V4 ALTERs ~129–143, V5 ~145–152; module doc ~10–13; test exemplar `migration_v1_db_upgrades_to_v2_preserving_rows` ~1578–1610 (manually builds a v1 DB with `SqliteStore::configure` + `PRAGMA user_version = 1`), forward-compat test ~1217–1250
- `crates/ktesio-engine/src/domain/registry.rs` -- `attach_memory` conflict check ~642–649; forward-only test `attaching_a_different_kind_over_an_existing_one_is_rejected` ~3010–3039 (add the symmetric sibling directly after); idempotent-same-kind tests ~2985/~3047; `resolve_secrets` ~1070 (`secret:` predicate precedent); `memory_key_declared` ~741 (the `adapter_launch_facts` → `resolve_config_mapping` → `mapping.target(key)` pattern)
- `crates/ktesio-engine/src/domain/config.rs` -- `MEMORY_DIR_KEY` ~148
- `crates/ktesio-engine/tests/memory.rs` -- fixture manifest declaring `[config."memory.dir"]` ~85; dump/snapshot asserts ~461/~551; spoof section header ~557
- `crates/ktesio-engine/tests/observed_metering.rs` -- `write_observed_manifest` ~171; upstream stub ~82; `wait_for_observed_rows` ~233
- `crates/ktesio-engine/tests/adoption.rs` -- two-engine pattern: `run_engine1` ~179, `kill_pid` ~130, `wait_for_agent_pid` ~158; newest neighbors `ai44_adopting_an_over_budget_run_enforces_the_budget` ~931 and the paused-row variant ~846
- `crates/ktesio-engine/tests/embed_clean.rs` -- the exactly-two-sites stderr allowlist (17 lines already touched by 11-1); add the new adoption-diagnostic entry here in the same change
- `_bmad-output/implementation-artifacts/deferred-work.md` -- lines 38–41: the bullet claiming the reverse conflict is untested "at both registry and CLI layers" is WRONG (CLI-level reverse was proven in 5-1); rewrite per A5 and mark `resolved:` when the test lands
- `docs/commands.md` -- ~250 and ~342 claim hand-set values "are stripped from the operator layers at resolve time" — false today in the override branch, true after item 1; verify/keep
- `docs/architecture.md` -- ~54: the engine-observed adoption limit sentence ("…the full fix … is a tracked follow-up") — rewrite for the new diagnostic; ~134/140 lifecycle/pause paragraphs are 11-1 territory, do not touch

## Tasks & Acceptance

**Execution:**
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- Item 1 (A1): after the override re-fold (~972–974), strip `MEMORY_DIR_KEY` from `mapping_effective` exactly as the base path does; note in a comment that a hand-set reserved key must not resurrect via the re-fold -- closes the strip gap
- [ ] Integration test (tests/observed_metering.rs or tests/memory.rs spoof section): observed manifest whose `[config]` maps `memory.dir`; `facade.set_config(name, "memory.dir", <decoy>)` (pattern: tests/cost.rs:166); start; poll the dump; assert the decoy is absent from the delivered env/config AND from `effective-config.json` (pattern: memory.rs:461/551) -- proves A1 end-to-end
- [x] `crates/ktesio-engine/src/store/sqlite.rs` -- Item 3 (B3): wrap EACH version step's `execute_batch` in `BEGIN IMMEDIATE` … `COMMIT` (single batch string per version, stamp included or immediately after within the same transaction — rusqlite `execute_batch` handles multi-statement); fix the stale comment ~324–327 to describe the new actual guarantee; align the module doc ~10–13 -- crash-atomic migrations
- [x] `crates/ktesio-engine/src/store/sqlite.rs` (tests) -- crash simulation: apply `SCHEMA_V1` manually but LEAVE `user_version = 0`; `SqliteStore::open`; assert success, stamped version, and preserved rows (pattern-match ~1578–1610) -- proves B3
- [x] `crates/ktesio-engine/src/domain/registry.rs` (tests) -- Item 4 (A5): symmetric reverse-conflict test directly after ~3039: attach `Native`, request `Filesystem`, assert `MemoryBackingKindConflict { attached: "native", requested: "filesystem" }` + row/status untouched -- pins A5
- [x] `_bmad-output/implementation-artifacts/deferred-work.md` -- rewrite the ~40–41 bullet: reverse direction was untested at the REGISTRY layer only (CLI-level reverse proven in 5-1); append a `resolved:` trailer naming this story's test -- fixes the misstated direction
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-46: in the `adopt_orphans` live-match branch, when the adopted instance is engine-observed (`metering_source == "engine-observed"` and a listener is NOT restored), emit a diagnostic via `emit_diagnostic` naming: instance, the stranded observed listener, the injected `base_url` host/port from the adopted env snapshot (read it from the spawn record's recorded launch facts if available — otherwise the diagnostic names the condition without the port), and the stop/start remediation; keep the current un-observed marking and state semantics EXACTLY -- honest surfacing, no relaunch
- [x] `crates/ktesio-engine/tests/adoption.rs` -- AI-46 test using the two-engine pattern with an engine-observed manifest variant (copy the small `write_observed_manifest` shape into adoption.rs per its `write_fake_manifest` idiom — do NOT grow the deferred-work-noted duplication beyond one helper): assert the diagnostic fragment reaches stderr/the sink after adoption -- proves AI-46
- [x] `crates/ktesio-engine/tests/embed_clean.rs` -- extend the stderr allowlist with the new adoption-diagnostic site (exact source-text pin, same style as the existing entries) -- keeps the embed gate honest
- [x] `docs/commands.md` + `docs/architecture.md` -- verify the strip claims now hold (keep wording); rewrite the architecture.md ~54 adoption-limit sentence to state the new diagnostic behavior and that a stop/start re-anchors the listener -- doc currency
- [ ] Full battery at story end: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, `python3 scripts/check_docs.py` -- all green

**Acceptance Criteria:**
- Given a hand-set `memory.dir` and an engine-observed instance, when the instance starts through the override branch, then the decoy value appears in no delivered surface (env, config file, effective snapshot)
- Given a state DB frozen mid-migration (tables exist, version 0), when the engine reopens it, then migration completes transactionally, version stamps, rows survive, and no error surfaces
- Given a native backing attached, when filesystem is requested at the registry layer, then the conflict error names attached/requested and nothing mutates
- Given an adopted engine-observed instance whose injected upstream is dead, when adoption settles, then a diagnostic names the stranded listener and the remediation, and the embed-clean gate passes with the allowlist entry
- Given the full battery, when run at story end, then all green with coverage ≥95%

## Spec Change Log

- 2026-09-11 — Approval note: approved on autopilot per the standing per-epic workflow; the frozen block restates the Islam-approved sprint change proposal 2026-09-10 §3 Story 11-3 table. AI-46 design choice resolved within the ratified intent: the "clear the stale base_url" one-liner cannot mean mutating the adopted child's injected env (impossible from the engine); of the two coherent shapes, honest diagnostic surfacing is chosen over relaunch-at-open because relaunching processes during `Engine::open` would contradict adoption's keep-them-running semantics and was never commissioned. Recorded here so the choice is reviewable. Continuity from 11-1: `adopt_orphans` now carries 11-1's cloned locals + AI-44's post-adoption `enforce_budget` — build on the current tree, not a stale base.

## Verification

**Commands:**
- `cargo fmt --all --check` -- expected: no diffs
- `cargo clippy --workspace --all-targets -- -D warnings` -- expected: zero warnings
- `cargo test --workspace --all-targets` -- expected: all pass
- `python3 scripts/check_docs.py` -- expected: pass

## Suggested Review Order

**Migration crash-atomicity (B3)**

- Per-step BEGIN IMMEDIATE transactions with the stamp inside each step.
  [`sqlite.rs:345`](../../crates/ktesio-engine/src/store/sqlite.rs#L345)
- Module doc: the guarantee AND its honest residual boundary (legacy ALTER-window freeze fails loud).
  [`sqlite.rs:14`](../../crates/ktesio-engine/src/store/sqlite.rs#L14)
- Resume test (frozen at v1, version 0 → reopens) and the loud-failure boundary pin.
  [`sqlite.rs:1668`](../../crates/ktesio-engine/src/store/sqlite.rs#L1668)

**memory.dir strip gap (A1)**

- The conditional strip in the override re-fold (fires only when the engine injects nothing).
  [`supervisor.rs:1100`](../../crates/ktesio-engine/src/domain/supervisor.rs#L1100)
- End-to-end test with the positive control proving the strip (not a dead mapping) blocks the decoy.
  [`observed_metering.rs:739`](../../crates/ktesio-engine/tests/observed_metering.rs#L739)

**Adoption stranding (AI-46)**

- The stranded-listener diagnostic + the loud metering-source read fallback.
  [`supervisor.rs:2870`](../../crates/ktesio-engine/src/domain/supervisor.rs#L2870)
- Two-engine integration test; embed_clean allowlist pin.
  [`adoption.rs:1200`](../../crates/ktesio-engine/tests/adoption.rs#L1200)

**Reverse conflict (A5)**

- The symmetric registry test + the corrected deferred-work note.
  [`registry.rs:3046`](../../crates/ktesio-engine/src/domain/registry.rs#L3046)

**Docs**

- architecture.md adoption sentence rewritten for the diagnostic behavior.
  [`architecture.md:54`](../../docs/architecture.md#L54)
