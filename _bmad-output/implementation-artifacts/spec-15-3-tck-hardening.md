---
title: 'TCK hardening: public seams where they exist, error honesty, stage-differentiated failures, configurability'
type: 'refactor'
created: '2026-10-16'
status: 'implemented'
baseline_commit: 'e8392650eb57bde401dbf4f8e1703733a22a991c'
review_loop_iteration: 0
context:
  - '{project-root}/docs/adapter-contract.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The Conformance Test Kit reads engine STORAGE internals instead of public seams (adapter.json snapshot layout, state.db SQL, agents/<name>/logs/agent.log literals — breaks silently if engine storage changes), conflates DB errors with zero rows ("have 0" timeout reasons hide the real error), mislabels every post-registration failure as "registration failed" across all 8 sections, and is non-configurable (private 30s section timeout, fixed temp state root, no section selection) — limiting third-party CI use (Fibery 173, the four 6-4 deferred findings).

**Approach:** Route every read through an existing public seam where one exists (`read_agent_log`, `fleet_entry`, `agent_home`, `paths::PathLayout`, `adapter::resolve`); make the row-count helpers return `Result` with named errors propagated immediately (never timed out as "have 0"); replace the stringly failure path with a stage enum rendering per-stage prefixes; add an additive `TckConfig` (`run_conformance_with`) with today's behavior as `Default`. Where NO public seam exists (usage-event ROW COUNTS for the replay-dedup proof; the registered-snapshot launch read), keep the direct read but route the path through `PathLayout` and DOCUMENT the residual frozen-v1 schema coupling honestly — no new engine public API in this story.

## Boundaries & Constraints

**Always:**
- Additive-only on the public surface: `TckConfig` + `run_conformance_with` are NEW; `run_conformance`/`run_mock_conformance` signatures unchanged (the two consumer suites and third-party dev-tests keep compiling untouched).
- The ConformanceReport JSON shape is UNCHANGED (schema_version stays 1; no new SectionResult variant — section selection yields FEWER SectionReport entries, and completeness is re-specified as "complete for the selected set").
- Exact NotApplicable reason strings that consumers pin stay byte-identical unless a test update in this change says otherwise.
- 95% coverage gate: every new arm (Result propagation, stage prefixes, config defaults/overrides) gets a covered test; hard-to-trigger I/O arms are covered via pure render/unit seams.
- Docs currency: `docs/adapter-contract.md`'s "What is checked" section gains the configurability + residual-coupling notes in this change.

**Ask First:** Adding any NEW public engine API (e.g. a usage-event count read or a registered-snapshot read on `Engine`/`Blocking`) — explicitly out of scope here; if a review finding demands one, halt and ask.

**Never:** No changes to the engine crate; no SectionResult enum change or schema bump; no behavior change to WHAT the sections verify — only how reads route, how errors surface, and how the harness is configured.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Section polls read logs | any section needing agent output | `read_agent_log`/`read_agent_log_since` (attributed view; exact-line + `heartbeat`-prefix checks unchanged) | read error propagates as the section's fail reason, named |
| Metering-source check | snapshot read | `fleet_entry(name).metering_source` (persisted snapshot via the public view) | entry read error = named fail reason |
| --dump args check | manifest subject | `adapter::resolve(&adapter_ref).launch()` — resolve-from-SOURCE semantics documented (post-registration manifest edits diverge; the registered snapshot stays private) | resolve error = named fail reason |
| Replay-dedup row counts | state.db unreadable/corrupt | `Err` with the open/query error named, propagated IMMEDIATELY (no 30s "have 0" timeout) | the section fails with the real error |
| Post-registration failure stages | tempdir / Engine::open / register / capabilities / snapshot each failing | each renders its OWN stage prefix (e.g. "engine open failed: …") on all sections of the selected set | all-sections-complete-fail report preserved |
| Default invocation | `run_conformance(adapter)` | byte-for-byte today's behavior (30s, fresh tempdir, all 8 sections) | unchanged |
| Configured invocation | `TckConfig{timeout, state_root: Some(p), sections: subset}` | selected sections only; caller-owned state root used as-is | StateRoot stage arm triggers for real on an unusable root |

</frozen-after-approval>

## Code Map

Investigated 2026-10-16 (anchors verified; all `crates/hekma-conformance/src/tck.rs` unless prefixed):
- D1 anchors — `read_snapshot` 654-676 (adapter.json + hand-parsed keys; engine's `AdapterSnapshot` private at `crates/hekma-engine/src/domain/registry.rs:99-124`); `read_launch_dump_path` 1727-1739; `usage_row_count` 744-757 + `observed_row_count` 787-801 (direct rusqlite on state.db; schema `crates/hekma-engine/src/store/sqlite.rs:96-105`, frozen v1); `agent_log_path` 819-825 (+ consumers 1415, 2198; `heartbeat_lines` 844-848); config_mapping home literal 1667.
- Public seams to adopt — `Blocking::read_agent_log`/`_since` engine.rs:1550/1555; `Blocking::fleet_entry` engine.rs:1452 (`metering_source` from the persisted snapshot, `domain/fleet.rs:430-465`); `Engine::agent_home` engine.rs:395 + public `paths::PathLayout` (paths.rs:338-403, state_db :357); `hekma_engine::adapter::resolve` adapter/mod.rs:700 (`launch()` :92-119, StartLaunch :215-222); O(1) `fleet_entry(name)` replaces the TCK's own scan helper (tck.rs:804-815).
- D2 anchors — `.unwrap_or(0)` at 754/756/798/800; timeout reason "have {count}" 778-780; inline observed poll 1946-1958 (dedupe into the shared helper).
- D3 anchors — `run_conformance` 350-359 maps ALL errors via `registration_failure_report` 595-639; mislabeled stages inside `conformance_inner` 388-419 (tempdir 388, open 392, register 396, capabilities 402, snapshot 419); panic path 372-382.
- D4 anchors — `SECTION_TIMEOUT` 104 (10 sites); tempdir 388-391; unconditional section push 456-561; public invocation surface lib.rs:61-64.
- Tests to update — 2520-2539 (registration-prefix pin → stage prefixes), 2913-2936 (zero-rows tolerance pin → inverted to Err), 2753-2799 + 2804-2829 (snapshot/dump parsing pins → rewritten for the seam reads), 2482/2527/2597/2665/3880 (8-completeness → selected-set completeness), hermes_tck.rs:115/146-176 + third_party_manifest.rs (must stay green UNTOUCHED — the additivity proof).

## Tasks & Acceptance

**Execution:**
- [x] `crates/hekma-conformance/src/tck.rs` -- route log reads through `read_agent_log`(+`_since`), metering-source through `fleet_entry`, the agent-home literal through `agent_home`, the dump-args read through `adapter::resolve` (source semantics documented in a comment + docs) -- public seams where they exist.
- [x] `crates/hekma-conformance/src/tck.rs` -- `usage_row_count`/`observed_row_count` return `Result<u64, String>`; `wait_for_usage_rows_for` + the observed poll propagate the first read error immediately; row-count reads route the state.db path through `PathLayout` and carry a documented residual-coupling comment (frozen v1 schema, intra-workspace, CI-tested).
- [x] `crates/hekma-conformance/src/tck.rs` -- stage enum (StateRoot/EngineOpen/Registration/CapabilitiesRead/SnapshotRead) with per-stage rendered prefixes; pure render unit test covers every arm; Registration keeps a real trigger (nonexistent manifest), SnapshotRead gains a real corrupt-state trigger where cheap.
- [x] `crates/hekma-conformance/src/tck.rs` + `lib.rs` -- additive `TckConfig` (section_timeout, state_root, sections) + `run_conformance_with`; `run_conformance` = Default delegation; subset selection yields fewer entries; unit tests for defaults/overrides/subset.
- [x] `docs/adapter-contract.md` -- "What is checked": configurability + resolve-from-source + residual-coupling notes.
- [x] `docs/adapter-contract.md` + `crates/hekma-conformance/src/tck.rs` -- the `--dump` SUBJECT-DELIVERY proof convention (Fibery 174): document the convention (a manifest adapter that declares config delivery SHOULD provide a `--dump` seam the TCK drives to prove the SUBJECT received the delivered values — closing the gap where only the hermes builtin's delivery had a subject leg) and give the TCK memory section its manifest-subject `--dump` leg (assert the subject's dump echoes the delivered memory dir; keep the env-var probe leg for adapters without a dump seam, with the honest reason string).
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- story 15-3 row; deferred-work.md: mark the four 6-4 findings resolved by this story.

**Acceptance Criteria:**
- Given a state.db that fails to open, when the usage sections run, then the section fails FAST with the open error named (no 30s "have 0" timeout) — the inverted pin proves it.
- Given each pre-section stage failing, then every section of the selected set carries THAT stage's prefix (all-sections-complete preserved) — the pure-render test covers all five arms.
- Given `run_conformance(adapter)` with no config, then behavior and report shape are byte-identical to today (hermes_tck + third_party_manifest suites green UNTOUCHED).
- Given `TckConfig` with a subset + caller state root + custom timeout, then exactly the selected sections run over the caller's root with the caller's timeout.
- No `agents/<name>/logs/agent.log` or `state.db` path literal remains outside `PathLayout` routing; no adapter.json read remains where a public seam exists.
- Gates: fmt/clippy/workspace tests/check_docs green; tarpaulin-covered new arms.

## Spec Change Log

- (Review round 1, 2026-10-17) 22 findings triaged (blind 14, edge 4, vg 3), all patches — no loopbacks. Headline corrections: (1) a dump-carrying manifest mapping memory.dir to a NON-env target now SKIPS the subject leg with a surfaced note (the probe twin remains the proof) instead of FAILING — restoring the spec's Never clause (no behavior change to what sections verify); the tightened rule is documented in adapter-contract.md. (2) Caller-supplied state roots must be FRESH (empty dirs) — a non-empty root is a named StateRoot-stage error, which simultaneously kills the stale-dump false-pass, the fixed-instance-name collisions, and the residual-state class. (3) The memory subject leg idempotently stops/terminalizes before attach (no cross-section cascade misattribution) and the detach error can no longer be silently dropped. Plus: dump artifacts removed pre-start in both proofs; one parsed-declaration helper kills the memory_dir_env_target TOCTOU; the native arm routes through the helper; TCK sqlite connections get busy_timeout (transient BUSY no longer fails a poll); the log polls keep the cursor + document the rotated-attributed-view semantics; empty SectionSelection::Only is a named failure (never vacuously conformant); prepare_state_root validates creatability; resolve-from-source divergences name the re-register remedy in their timeout reasons; the EngineOpen stage gets a real pipeline drive (corrupt state.db); the default-path registration detail keeps its `register: ` infix (byte-parity with the prefix pin's promise); TckConfig derives PartialEq/Eq; stage raises strip inner helper prefixes; the zero-timeout test's bound tightened; hekma-conformance's additive surface announced in CHANGELOG/RELEASE_NOTES (path-only crate, no version bump); testing.md's rev-pin section names the bump-at-merge requirement; deferred-work's resolution wording scoped to tck.rs (uj3 stays by-design test-infra).
- (Closeout, 2026-10-17) Merged as 65e7b45 (PR #282) after the CI-shape fixes (the session-leg gate + the boundary allowlist with its automation pin). All gates green; the additivity proof (consumer suites untouched) held on every leg.

## Design Notes

- Why no new engine API: the deferred item's spirit is "stop hard-coding internals"; where the engine HAS a seam we use it, and where it does not (row counts, registered snapshot) the coupling is frozen-v1 schema + intra-workspace + CI-tested — documented, not hidden. Expanding the engine's public surface for a test-kit need is a product call, out of scope.
- `fleet_entry(name).metering_source` reads the PERSISTED snapshot through the public view — strictly better than re-parsing adapter.json; keep the "launchable ⇔ snapshot carries launch" check on resolve-from-source and say so.
- Subset semantics (fewer entries, no enum change) keeps third-party report consumers parsing unchanged; `is_conformant()` over a subset stays well-defined.

## Verification

**Commands:**
- `cargo test -p hekma-conformance` + `cargo test -p hekma-engine --test hermes_tck` -- both green, consumer suites untouched.
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` + fmt + clippy + check_docs -- green.
