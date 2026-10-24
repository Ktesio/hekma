# Deferred Work

<!-- Entry convention (epic-6 retro, item 8): `summary:` states the gap; `evidence:` says why it is
     real; `resolved: <what landed> <date>` is the LAST key of the entry, appended only when the fix
     ships (with the PR/commit ref). An entry WITHOUT a `resolved:` line is open — the file is swept
     at each retrospective. -->

Findings surfaced incidentally during quick-dev reviews that are out of scope for the triggering change. Collected for later focused attention.

## Resolution convention (adopted 2026-09-05, epic-6 retro action item 8 / issue #167)

When a deferred entry is fixed, its bullet gains a trailing marker line — `resolved: <ref> <date>` — naming the PR, commit, or change that resolved it and the date. Resolved entries STAY listed (the record of what was once deferred is part of the audit trail); a sweep greps for entries without a `resolved:` marker. Un-resolved entries have no marker, so "still open" is greppable as the absence of `resolved:` within the entry.

## From AI-17 (pin workspace toolchain to 1.96.1) — review, 2026-07-06

- **Contributor docs still tell contributors to run bare `cargo` (fmt/clippy/test).** With the new `rust-toolchain.toml`, bare `cargo` resolves to the MSRV (1.96.1) locally for contributors without a `RUSTUP_TOOLCHAIN` override, while CI's fmt/clippy/test jobs now gate on latest `stable` (explicit `+stable`). This local-vs-CI toolchain skew is intentional but is not documented in the other contributor-facing files. Consider a one-line note (or a `+stable` reproduction hint) in: `CONTRIBUTING.md` (~L89-91), `docs/contributing.md` (~L15-24), `AGENTS.md` (~L14-16), `.github/pull_request_template.md` (~L7-9), `docs/github-repository-audit-checklist.md` (~L167-169), `.agents/skills/kt-release/SKILL.md` (~L58), and `scripts/prepare_kt_release.py` (~L244-246). `docs/testing.md` already documents the split; the rest do not. Low severity (surfaces as an occasional new-stable clippy/rustfmt CI nit, not a shipped bug).
  resolved: 2026-09-22 hardening batch (after docs/contributing.md + the PR template
  landed the note earlier) — CONTRIBUTING.md, the repository-audit checklist, and
  .agents/skills/kt-release/SKILL.md now carry the MSRV-vs-+stable note; the
  named scripts/prepare_kt_release.py path no longer exists (the script lives in
  the skill, whose reference was fixed to the full path).

- **Coverage CI job rebuilds `cargo-tarpaulin` on every fresh runner (no binary cache).** Pre-existing (predates AI-17): the `coverage` job in `.github/workflows/ci.yml` runs an unguarded `cargo install cargo-tarpaulin` with no `~/.cargo/bin` cache, so it recompiles tarpaulin (~several minutes) every run. The `semver` job already added a `${{ runner.os }}-cargo-semver-checks-bin` cache + `command -v` guard (AI-1); the coverage job could adopt the same pattern for symmetry and CI speed.
  resolved: the AI-23 coverage-cache work — the coverage job now has a dedicated
  `cargo-tarpaulin-bin` cache step (restore-keys seeded) plus the `command -v`
  guarded `cargo +stable install` (marker appended by the 2026-09-22 hardening
  batch; the fix shipped before the convention caught up).

## From Story 5-1 (managed filesystem Memory Backing) — three-layer review, 2026-08-23

- source_spec: `5-1-attach-a-managed-filesystem-memory-backing`
  summary: Attach/detach vs start TOCTOU — the backing row read/write and the supervisor's start-path snapshot are not mutually atomic (attach landing between a start's backing read and spawn; detach clearing the row after the read but before launch), and the terminal-state guard's check is separate from the row write.
  evidence: Real windows under AD-17's ADOPTED coarse two-mutex model (registry-lock-only attach was the ratified Task 4.4 design); consequences are bounded (a started agent with an injected dir whose row then vanishes, self-correcting at the next stop/start) and single-operator CLI usage makes them theoretical today. Belongs to AI-63(b)/AD-17's replacement locking-model decision due before Epic 7, not to this story.
  resolved: story 15-6 (spec-15-6-memory-atomicity, 2026-10-17) — the windows are closed at the shared SQLite store, not with locks (std mutexes are process-local; the CLI's one-Engine-per-command shape races across processes). Attach/detach run guard + conflict check + row write inside ONE BEGIN IMMEDIATE transaction; start's `→ starting` transition persists state + reads the authoritative backing in the same CONDITIONAL transaction (a mid-window attach/detach rolls it back with a typed, retryable refusal naming both sides); the guard-vs-write gap inside attach is closed by the in-txn guard re-evaluation; `busy_timeout` (5000ms, the stop-ladder precedent) bounds concurrent writers with a typed busy error. Locked model untouched (AD-17/AD-18 stand). Windows pinned by `crates/hekma-engine/tests/memory_atomicity.rs` (two-Engine same-root harness) + store-level rollback pins in `store/sqlite.rs`.

- source_spec: `5-1-attach-a-managed-filesystem-memory-backing`
  summary: SQLite migration steps are not crash-atomic — each SCHEMA_Vn batch runs before its `PRAGMA user_version` stamp, so a crash between them re-runs the batch on reopen and dies on "table already exists".
  evidence: Pre-existing pattern for V1→V4 (this story only followed it for V5); never observed in the wild because the batch+stamp window is milliseconds and desktop state DBs are small. Proper fix = wrap each step in BEGIN IMMEDIATE…COMMIT across ALL versions, one focused migration-hardening change.
  resolved: story 11-3 (B3, PR #181 / bee7d48) 2026-09-11 — `migrate_step` runs each
  version's DDL and its user_version stamp inside ONE BEGIN IMMEDIATE transaction,
  with the legacy-freeze fail-loud contract pinned (`migration_legacy_freeze_in_the_alter_window_fails_loud`)
  (marker appended by the 2026-09-22 hardening batch; the fix shipped before the
  convention caught up).

- source_spec: `5-1-attach-a-managed-filesystem-memory-backing`
  summary: Semantic split between store and registry — `StateStore::upsert_memory_backing` documents REPLACE-on-re-attach (kind + timestamp overwritten) while `Registry::attach_memory` promises idempotent re-attach keeps the original timestamp and never changes kind; any future caller bypassing the registry guard can violate the A-6 invariant through sanctioned store behavior.
  evidence: Both behaviors are individually documented and tested; the invariant currently holds only because every caller goes through the registry. Hardening option: make the store reject kind-changes on an existing row (UNIQUE conflict → typed error) so the invariant lives below the registry too.
  resolved: 2026-09-22 hardening batch (PR #232 / a5a763a) — port-level A-6 now lives below the registry: `StateStore::upsert_memory_backing` REJECTS a kind-change on an existing row (`StoreError::MemoryBackingKindConflict` naming attached + requested, row byte-unchanged), pinned by `upsert_memory_backing_rejects_a_kind_change_and_keeps_the_row` (store/sqlite.rs); a same-kind upsert keeps the documented REPLACE shape (the registry passes the ORIGINAL timestamp, so idempotence holds end to end), and story 15-6's `attach_memory_backing_txn` re-runs the same conflict check under the BEGIN IMMEDIATE write lock.

- source_spec: `5-1-attach-a-managed-filesystem-memory-backing`
  summary: Integration test helpers (fake-manifest writer, dump polling, tree snapshotting in tests/memory.rs) duplicate shapes already living in sibling integration files rather than a shared test-support utility.
  evidence: Same pattern grew per-file across registration/lifecycle/pause/interaction/logs/metering; each story copied the smallest shape it needed. Cost compounds across Epics 6–7 when manifest fixtures evolve (e.g. contract_version bumps touch N copies). Candidate: a `tests/support/` module (or `ktesio-conformance` test-fixture exports) once Epic 6's conformance kit forces the shape anyway. **Census update (Epic-7 review, 2026-09-09):** the duplication has grown — alongside `uj3::write_flow_manifest` there are now FIVE more near-identical manifest TOML writers (three in `crates/ktesio-engine/tests/events_subscription.rs` — `write_fake_manifest`/`write_lingering_manifest`/`write_crash_once_manifest` — the perf-budgets harness's `write_heartbeat_manifest`, and the embedding quickstart's `write_manifest`, the last deliberately standalone host code a consolidation must NOT absorb), plus two lag-accumulating `try_recv` drain implementations. A consolidation pass should fold the test-side writers into one shared fixture export and note which copies are load-bearing.
  resolved: 2026-09-22 hardening batch — the seven identical write_fake_manifest copies
  (budget, crash, fleet_totals, metering, lifecycle, cost, adoption) now delegate to
  `hekma_conformance::test_support::ManifestFixture` (the perf harness already used
  `heartbeat` since 10-3). The four remaining local writers (interaction, pause,
  observed_metering, memory) are genuinely parametrized per-suite shapes
  (runtime capability levels / injected config sections), not duplication — the
  consolidation is complete under the census's own load-bearing-copies rule.

## Deferred from: code review of 5-2-delegate-to-native-memory-with-an-explicit-boundary (2026-08-24)

- DC-3 detach/status wording not extended to name the delegation sentence — deferred to Epic 6's status surface: detach is kind-blind metadata removal and the story's ratified human surface is attach-only (NFR-7 sentences live in attach confirmations + docs).
- Reverse conflict direction (filesystem requested over an attached native backing) was untested at the REGISTRY layer only — one symmetric `!=` comparison with no registry-level test; the CLI-level reverse was proven in story 5-1. Forward direction (native over filesystem) is covered at both layers. Candidate: a symmetry test with AI-63(b) work.
  resolved: story 11-3 (memory robustness batch) 2026-09-11 — `attaching_filesystem_over_an_attached_native_backing_is_rejected_symmetrically` in `crates/ktesio-engine/src/domain/registry.rs` pins the registry-level reverse conflict (`MemoryBackingKindConflict { attached: "native", requested: "filesystem" }`, row and managed-dir state untouched).

## Deferred from: one-shot blind-hunter pass on the round-1 triage fixes (2026-08-30)

- source_spec: `_bmad-output/implementation-artifacts/spec-6-2-round1-triage-fixes.md`
  summary: PATH save/restore in the hermes e2e test is not panic-safe — an assert between the shim install and teardown skips the restore.
  evidence: `crates/ktesio-engine/tests/hermes.rs` installs the PATH shim mid-test and restores it only as trailing teardown code; a RAII/Drop guard (like the existing `_shim` guard) would make the restore unconditional. The shim-install site predates this diff (blind-3 asked only for save/restore).
  resolved: 2026-09-22 hardening batch (PR #231 / d28c94e) — the restore now runs through the `PathGuard`'s `Drop`, so it fires even when an assert between shim install and test end panics (the suite's module doc states the panic-safe guarantee).
- source_spec: `_bmad-output/implementation-artifacts/spec-6-2-round1-triage-fixes.md`
  summary: hermes.rs module doc claims later tests start from a pristine PATH — true only on the happy path while the restore is not panic-safe.
  evidence: `crates/ktesio-engine/tests/hermes.rs:21-28` "restored at teardown so any test added to this binary later starts from a pristine environment" holds only if no test panics between install and restore; wording should soften or follow the RAII fix above.
  resolved: 2026-09-22 hardening batch (PR #231 / d28c94e) — with the `PathGuard` Drop restore the claim is now unconditional; the doc names the guard and its panic-safety explicitly.
- source_spec: `_bmad-output/implementation-artifacts/spec-6-2-round1-triage-fixes.md`
  summary: The engine-tests OS-cfg allowlist covers the whole `crates/ktesio-engine/tests/` directory while only `memory.rs:605` uses cfg — narrow the allowlist to the single file.
  evidence: `.github/workflows/ci.yml` allowlist entry whitelists the directory; the corrected comment (vg-o2) names memory.rs:605 as the sole user. Narrowing is a CI behavior change, out of the comment-only scope of vg-o2.
  resolved: 2026-09-22 hardening batch (PR #231 / d28c94e) — the boundary gate's OS-cfg allowlist now ENUMERATES the engine test files (atomic_config_writes.rs, memory.rs) instead of whitelisting the whole tests/ directory; a new cfg use in any other test file must go data-driven (`OsId::current`) or join the enumerated list with a justification comment in the same change.
- source_spec: `_bmad-output/implementation-artifacts/spec-6-2-round1-triage-fixes.md`
  summary: No test pins that a manifest declaring kind `hermes` WITHOUT a `[lifecycle.start]` table yields NoStartTemplate with no builtin-table fallback.
  evidence: The new precedence test (blind-19) covers only manifest-with-start beating the builtin table; the no-start-table complement relies on pre-existing engine semantics in `resolve_start_launch` (`crates/ktesio-engine/src/adapter/mod.rs:275`).
  resolved: 2026-09-22 hardening batch (PR #231 / d28c94e) — `resolve_start_launch_of_a_hermes_manifest_without_start_yields_no_start_template` (adapter/mod.rs, its comment names this deferral) pins the complement: kind `hermes` without `[lifecycle.start]` yields `NoStartTemplate`; the builtin gateway launch is never a fallback for a manifest re-read.
- source_spec: `_bmad-output/implementation-artifacts/spec-6-2-round1-triage-fixes.md`
  summary: No user-facing doc that `model` is a silent no-op for the hermes kind (Decision 6) — discoverable only in code comments/tests.
  evidence: `docs/commands.md` hermes paragraph documents HERMES_HOME mapping but not the deliberately unmapped `model` key; pre-existing gap unrelated to the 13 triaged findings.
  resolved: PR #152 (docs/editorial) 2026-09-04 — the hermes paragraph now names `model` as a deliberately unmapped key.

## Deferred from: code review of spec-6-3-govern-and-interact-with-hermes-end-to-end-uj-1-for-real (2026-08-31)

- source_spec: `_bmad-output/implementation-artifacts/spec-6-3-govern-and-interact-with-hermes-end-to-end-uj-1-for-real.md`
  summary: architecture.md:68 breach-record sentence's "(…; tokens only)" parenthetical is stale since story 3-3 — `BudgetBreachEvent` also carries `dimension` plus `dollar_limit`/`dollar_observed`/`estimate_label` on dollar breaches (event.rs:507-525).
  evidence: The rewritten Budget-enforcement paragraph kept the pre-existing parenthetical; the dollar fields shipped in story 3-3 and the sentence was out of this story's minimal-edit scope.
  resolved: epic-6-retro remediation PR (docs hygiene batch, #165/D2) 2026-09-05 — the sentence now names `dimension` and the additive dollar fields.

- source_spec: `_bmad-output/implementation-artifacts/spec-6-4-prove-any-adapter-with-the-conformance-test-kit.md`
  summary: TCK polling helpers hard-code engine storage internals (usage_events/agent_instances SQLite schema, agents/<name>/logs/agent.log path, adapter.json snapshot layout) instead of public engine seams.
  evidence: Story 6-4 adversarial review (blind-hunter); coupling is intra-workspace and CI-tested today, but breaks silently if engine storage/layout changes. Switch to public engine APIs (e.g. read_agent_log) when available.
  resolved: story 15-3 (2026-10-17) — every read with an existing public seam now routes through it
  (read_agent_log for log polls incl. the heartbeat-freeze proof, O(1) fleet_entry for the
  registered metering source, adapter::resolve for the --dump pair, EnginePaths for every
  state-root-relative path; scoped to tck.rs — uj3.rs keeps its own path joins BY DESIGN as
  the recorded test-infra module, not the kit surface); the two reads with NO seam (usage row counts, the registered-snapshot
  launch boolean) stay direct with the frozen-v1 residual coupling documented on the helpers and in
  docs/adapter-contract.md — no silent breakage remains.
- source_spec: `_bmad-output/implementation-artifacts/spec-6-4-prove-any-adapter-with-the-conformance-test-kit.md`
  summary: TCK usage/observed row-count helpers conflate DB open/query errors with zero rows, producing misleading "have 0" timeout reasons instead of the actual error.
  evidence: Story 6-4 adversarial review (blind-hunter + edge-case-hunter); diagnostic polish only — the section still fails on timeout. Propagate Result through the helpers when touched next.
  resolved: story 15-3 (2026-10-17) — usage_row_count/observed_row_count return Result with named
  open/query errors, the shared wait poll propagates the FIRST read error immediately (never a
  "have 0" timeout), and the inverted pin (usage_row_helpers_name_db_errors_and_propagate_them_
  immediately) proves the immediacy under the full 30s budget.
- source_spec: `_bmad-output/implementation-artifacts/spec-6-4-prove-any-adapter-with-the-conformance-test-kit.md`
  summary: All post-registration pre-section failures (tempdir, Engine::open, snapshot read) are mislabeled "registration failed" across all 8 report sections.
  evidence: Story 6-4 adversarial review (blind-hunter + edge-case-hunter EC1); needs a stage-differentiated error type plus new covered arms — deferred to avoid thinning the 95% coverage margin in this story.
  resolved: story 15-3 (2026-10-17) — the Stage enum (StateRoot/EngineOpen/Registration/
  CapabilitiesRead/SnapshotRead) renders per-stage prefixes on every selected section; all five
  arms pure-render-pinned, Registration keeps the byte-identical compat prefix, StateRoot has a
  real unusable-root trigger, SnapshotRead's corruption shapes are helper-level pinned (the engine
  rewrites the snapshot at registration, so a post-registration corruption is not cheaply
  reachable through the pipeline).
- source_spec: `_bmad-output/implementation-artifacts/spec-6-4-prove-any-adapter-with-the-conformance-test-kit.md`
  summary: TCK harness is non-configurable — private 30s section timeout, fixed temp state root, no per-section selection — limiting third-party CI environments.
  evidence: Story 6-4 adversarial review (blind-hunter); API-surface design beyond story 6-4's captured intent; candidate for a future ergonomics story alongside the 6-6 contract freeze.
  resolved: story 15-3 (2026-10-17) — additive TckConfig (section_timeout, state_root,
  SectionSelection) + run_conformance_with; run_conformance/run_mock_conformance delegate to the
  Default byte-for-byte (both consumer suites green untouched), subset runs yield fewer entries in
  the fixed order with the report shape unchanged (schema_version stays 1).

- source_spec: `_bmad-output/implementation-artifacts/spec-epic-6-retro-remediation.md` (epic-6 retro #163/B11 residual)
  summary: The subject-declared `memory.dir` delivery proof covers only the hermes builtin (an engine test over the shim `--dump` seam); a THIRD-PARTY manifest adapter's own declared delivery is still proven only on the TCK probe twin's env var — the memory section has no subject-delivery leg for manifest subjects.
  evidence: Retro remediation review (blind-hunter finding on the remediation diff, 2026-09-05): closing it generally needs a subject `--dump` seam convention for manifest adapters, which is a contract/docs decision (document the `--dump` proof convention) rather than test-only work.
  resolved: story 15-3 (2026-10-17, Fibery 174) — the `--dump` subject-delivery convention is
  documented in docs/adapter-contract.md, and the TCK memory section carries the manifest-subject
  leg: a manifest subject declaring memory.dir whose launch carries `--dump <path>` gets its OWN
  delivery proven (dump echoes the managed dir through its declared env var; a `--drop-env`
  negative control proves the leg runs); adapters without a seam keep the probe-twin proof.

## Deferred from: implementation of spec-7-2-subscribe-to-engine-events-with-stable-schemas (review triage, 2026-09-09)

- source_spec: `_bmad-output/implementation-artifacts/spec-7-2-subscribe-to-engine-events-with-stable-schemas.md`
  summary: Bus delivery is at-most-once in the crash window — a process crash between a durable event append and its bus publish loses that ONE event from the stream (the durable logs stay complete). A resync/gap-detection helper (e.g. a per-instance commit cursor a host could use to detect a gap and re-read through the query APIs) is DEFERRED, not built in-story.
  evidence: EC1-3 crash-window findings on the 7-2 review triage. Append-then-publish is the story's ratified order ("never publish what did not commit" — pinned by the VG1 append-failure silence tests); closing the window would need either publish-before-append (violates the committed-truth guarantee) or a durable per-subscriber cursor (a new persistence surface, inside the spec's Never list). The recourse is documented on the bus module and in docs/architecture.md: the query APIs (`transition_events`, `budget_breach_events`, ledger reads) return the committed truth regardless of any crash.
  resolved: story 10-3 shipped the helper — `Engine::resync_events` / `Blocking::resync_events` + `ResyncCursor` (committed transitions/breaches/ledger rows → `EngineEvent`s, exact per-family commit order, torn-tail tolerant, cursor-idempotent), acceptance-tested in `crates/ktesio-engine/tests/resync_events.rs` and documented in docs/embedding.md + testing.md and the bus module docs (2026-09-10)

## Deferred from: implementation of spec-7-3-embed-clean-no-tty-no-prompts-blocking-facade (review triage, 2026-09-09)

- source_spec: `_bmad-output/implementation-artifacts/spec-7-3-embed-clean-no-tty-no-prompts-blocking-facade.md`
  summary: A Host-provided diagnostic sink — the closure for the TWO allowlisted AD-12 stderr diagnostics (`domain/supervisor.rs`'s memory-delivery notice and enforcement breadcrumb). Routing them through a sink the Host injects (instead of best-effort `eprintln!`) is a PUBLIC-API design change, beyond the story's additive scope — deferred to a future story.
  evidence: The embed-clean audit (`crates/ktesio-engine/tests/embed_clean.rs`) allowlists exactly these two sites, each pinned by a fragment unique to its emission with a count==1 assertion; the allowlist exists precisely because the closure is an API change, not a deletion. Until closed, any new engine diagnostic repeats this review.
  resolved: story 10-2 (PR #180 / b590dc8) 2026-09-10 — DiagnosticSink shipped (opt-in, default stderr byte-identical), both AD-12 diagnostics routed through one choke point, the audit allowlist emptied with count==1 pins (marker appended by the 2026-09-22 retro sweep — the fix shipped before the convention caught up).

- source_spec: `_bmad-output/implementation-artifacts/spec-7-3-embed-clean-no-tty-no-prompts-blocking-facade.md`
  summary: The `RUN_NONCE` cross-process caveat — a documented LIMITATION, to be kept visible: the RunId uniqueness tie-breaker guarantees PER-PROCESS uniqueness only (exactly what a multi-engine single-process host needs); cross-process uniqueness is NOT claimed (separate roots keep separate ledgers; conflation would only matter for merged ledgers, which Ktesio does not do).
  evidence: Documented on the allowlist entry in `crates/ktesio-engine/tests/embed_clean.rs` and in the embed-clean section of `docs/testing.md`; recorded here so a future change that ever merges ledgers (or externally consumes Run ids as globally unique) re-triages the nonce deliberately instead of inheriting the caveat silently.

## Deferred from: implementation of spec-7-5-benchmark-the-performance-budgets (review triage, 2026-09-09)

- source_spec: `_bmad-output/implementation-artifacts/spec-7-5-benchmark-the-performance-budgets.md`
  summary: Trend tracking against prior runs (baseline-file comparison) — the perf harness prints one self-contained, gated report per run but nothing compares runs over time (e.g. a committed baseline JSON the harness diffs against, with a bounded regression delta and a documented update procedure).
  evidence: Each run's record is preserved (the CI perf-budgets job log + the uploaded JSON artifact, and local stdout), so the raw material exists; but detecting a slow multi-week drift (e.g. reads creeping from 6 ms toward 100 ms) is manual. A baseline file adds storage + churn questions (which platform's baseline is canonical; how updates are ratified) that deserve a focused change rather than riding this story's gate work.
  resolved: story 15-5 (2026-10-17) — `--baseline`/`--write-baseline` + the additive ×1.15 ratchet over every gated metric (improvements free; exactly-at-limit within; non-positive baselines surface `no-ratchet` and keep their absolute gate; gating CI mode only — the canonical baseline's own leg — other platforms report-only), the ONE committed canonical baseline `benches/baselines/perf-ubuntu-x86_64.json` updated by PR (the visible diff IS the ratification; procedure + provenance note in docs/testing.md), ratchet math and baseline error arms pinned by the example's gate-math tests.

- source_spec: `_bmad-output/implementation-artifacts/spec-7-5-benchmark-the-performance-budgets.md`
  summary: A RATIFIED subscriber-active budget is future work. NFR-4 is measured (and gated) with ZERO subscribers, while Epic 7 ships a subscription surface; the perf harness now MEASURES the subscriber-active overhead (one active subscriber; CPU/RSS per instance + read p99, with deltas vs the unsubscribed baseline in the report's `subscriber_overhead` block) but REPORTS it without gating — there is no ratified budget to gate against. A future spec should propose the budget (e.g. a bounded delta vs the unsubscribed figures) and flip the block from reported to gated.
  evidence: Epic-7 adversarial review (2026-09-09): the figures are now collected every run (so a future budget lands with historical data already recorded), but gating a number nobody ratified would be a silent budget invention — the same anti-pattern the 7-5 gate-math pins exist to prevent.
  resolved: story 10-3 ratified the budgets from the observed 2026-09-10 local run (CPU Δ −0.015 pts, RSS Δ +0.01 MiB, read-p99 Δ +0.7 ms) at 0.25 pct-points / 2 MiB / 100 ms with the CPU gate's tolerance shape (strict locally, ×1.5 on CI) — three hard gates appended to `evaluate_gates`, pinned by the example's gate-math tests, recorded in docs/testing.md's budget table (2026-09-10)

- source_spec: `spec-11-1-engine-robustness-batch.md`
  summary: Windows adopted-process exit-code retrieval — AI-13's "exit code unavailable" rationale is Unix-specific; on Windows the HANDLE opened at adoption could yield the code via GetExitCodeProcess.
  evidence: Blind-hunter finding from the 11-1 review (2026-09-11); belongs in the cross-platform batch (11-5).
  resolved: story 11-5 (2026-09-11) — the adopted-handle poll in `crates/ktesio-engine/src/backends/windows/mod.rs` (`WindowsProcess::reap_if_exited`, adopted arm) already reads the REAL exit code via `GetExitCodeProcess` and reports `Exited { code: Some(n) }`, so a Windows adopted exit flows into the crash cause's `Exited(Some)` arm carrying its true code; the supervisor's `CrashInput::Exited(None) if adopted` cause (domain/supervisor.rs) is now documented as the Unix-shaped case (Windows reaches it only for a genuinely unreadable code), and `ai13_adopted_process_exit_records_the_unavailable_exit_code_cause` (tests/adoption.rs) records the per-OS split. Windows-hosted proof: the `windows-latest` matrix leg runs the adoption suite + the cfg(windows)-hosted backend tests; compile-checked on Unix via `cargo check -p ktesio-engine --target x86_64-pc-windows-gnu`.
- source_spec: `spec-11-1-engine-robustness-batch.md`
  summary: Windows AI-14 fail-closed spawn arm has no runtime test on any host (compile-checked + clippy-clean on x86_64-pc-windows-gnu only).
  evidence: Blind-hunter + verification-gap findings from the 11-1 review (2026-09-11); needs a Windows CI leg → 11-5 (AI-29/35/38/54 batch).
  resolved: story 11-5 (2026-09-11) — a `#[cfg(test)]` module in `crates/ktesio-engine/src/backends/windows/mod.rs` (cfg-allowed in the boundary gate's backends home) hosts the arm's tests ON the `windows-latest` CI leg: `spawn_of_a_real_child_verifies_a_nonzero_creation_time_and_stops_clean` drives the production spawn of a real `fake_agent` and asserts the verified non-zero creation-time fingerprint (never the `0` sentinel), and `start_time_read_fails_closed_for_an_absent_pid` pins the failed-read path (`None` from the `process_start_time` seam) that the spawn arm fails closed on. Compile-checked on Unix via `cargo check -p ktesio-engine --lib --profile test --target x86_64-pc-windows-gnu`.
- source_spec: `spec-11-1-engine-robustness-batch.md`
  summary: Engine-observed usage channel remains lossy under store failure (drain_observed_for is deliberately best-effort; no cursor, no retry) — governance/billing durability incomplete for un-cooperative agents.
  evidence: Blind-hunter finding from the 11-1 review (2026-09-11); durability design for the observed channel needs a product/ops call → 11-6.
  resolved: epic-12 story 12-4 (2026-09-15) — Islam ratified the AI-20/AI-47 proposal set ("recommended across the board"); the observed drain now mirrors the self-reported AI-41 treatment in `crates/ktesio-engine/src/domain/supervisor.rs`: a commit failure parks the minted events (`Supervised::observed_park`, retried as-is — never re-minted), the retry is bounded at 3 same-front failures with a loud SKIPPED diagnostic, the pending buffer itself is CAPPED at `OBSERVED_PARK_MAX_EVENTS` (1024 — review loop 1: the mechanism without which the park was unshippable; an unbounded buffer would have traded a store outage for engine-memory exhaustion), overflowing drops the OLDEST events with a loud diagnostic, and the terminal drain announces the loss explicitly (mode-aware diagnostics, review round 2's wording fix) — never silent best-effort loss. Pinned by the supervisor's `an_observed_drain_*` / `an_observed_poisoned_event_*` / `an_oversized_observed_park_*` / `a_terminal_observed_drain_failure_*` tests; the shared ingest-failure diagnostic is caller-factual for both channels (and MODE-aware per review loop 1: a MidRun drain states the park-and-retry truth, a terminal drain announces the loss without a retry claim).
- source_spec: `spec-epic-12-durable-detach-observed-channel.md`
  summary: Fleet surfaces (`list`/`show`/`--json`) carry no detached/supervision-mode indicator, so an operator cannot see which running instances survive command exit or sit in the enforcement window.
  evidence: epic-12 step-04 blind-hunter (round 2, 2026-09-15) — the honesty lives only in the one-time start stderr note; a visible cell/field (the honest `—` cell pattern) is a product-surface decision.
  resolved: story 15-2 (PR #281 / 9a2ceaa, 2026-10-17) — the fleet surfaces carry the supervision indicator: `list`/`show` render the `detached` cell and `--json` the matching field, so instances surviving command exit (or sitting in the enforcement window) are visible in the fleet view (cli/agent.rs, "Story 15-2 (Fibery 176): the supervision indicator").
- source_spec: `spec-epic-12-durable-detach-observed-channel.md`
  summary: The engine-level cross-lifetime detach e2e (`detached_start_survives_a_clean_engine_exit_and_the_next_engine_adopts`) still runtime-skips on the linux-x86 CI leg (pre-existing #109 mitigation); the CLI-level e2e covers the behavior there, but the facade-level harness runs only on macOS/Windows legs.
  evidence: epic-12 step-04 verification-gap reviewer (rounds 1+2, 2026-09-15) — compensation verified (unskipped `kt`-binary e2e on Linux); fold into the #109 re-exec-deadlock follow-up.
- source_spec: `spec-epic-12-durable-detach-observed-channel.md`
  summary: pause × detach cross-lifetime edge — a detached handle paused (SIGSTOP) in-process and then dropped at command exit strands the child frozen and unsupervised until a later command resumes/stops it.
  evidence: epic-12 step-04 edge-case-hunter (round 1, 2026-09-15) — needs a product call (resume-before-release vs kill vs document-only; documented-only in commands.md today); the backend handle does not track pause state, so it is not a trivial patch.
  resolved: story 15-2 (PR #281 / 9a2ceaa, 2026-10-17) — ratified and implemented: `release_detached_paused` (supervisor/lifecycle.rs, "Story 15-2 (Fibery 178)") SIGCONT-resumes a detached paused child at the releasing engine's exit with a surfaced note — EXCEPT a budget-breach pause, which stays stopped (resuming it would run unsupervised in the enforcement-free window); a child that exited first and the breach-shaped stay-stopped arm each carry their own surfaced note.
- source_spec: `spec-epic-12-durable-detach-observed-channel.md`
  summary: A detached child that exits long after its handle dropped is reaped only once at drop — a long-lived embedding host can accumulate zombie entries until the host exits (kt CLI processes exit immediately, so CLI usage is unaffected).
  evidence: epic-12 step-04 edge-case-hunter (round 1, 2026-09-15) — a lazy-reaper arm in the backend (poll detached children this process spawned) is a small design task for an embedding-focused story.
  resolved: 2026-09-22 hardening batch — the detached arm of `UnixProcess::drop`
  hands a still-alive child to `spawn_detached_reaper` (a blocking wait thread,
  AD-12 thread precedent; zero shared state), pinned by
  `a_detached_handle_dropped_while_the_child_is_alive_reaps_the_later_exit`
  (ECHILD observable).
- source_spec: `spec-epic-12-durable-detach-observed-channel.md`
  summary: In-memory park loss at engine death (observed pending buffer + self-reported parked tail die with the process) is acknowledged only in a code comment — the AI-18 comment-only-caveat shape; the crash window skips un-drained usage silently at adoption (cursor re-drifts to log end).
  evidence: epic-12 step-04 blind-hunter (round 1, 2026-09-15) — pre-existing crash-window family from Epic 3; an adoption-time diagnostic naming the possible un-flushed usage window would make it surfaced-not-silent.
  resolved: 2026-09-22 hardening batch — the successful-adoption path emits the
  park-loss note through the DiagnosticSink choke point
  (`domain/supervisor/reaper.rs` adopt_orphans), pinned by
  `adopt_orphans_surfaces_the_park_loss_window_on_a_successful_adoption`.
- source_spec: `spec-epic-12-durable-detach-observed-channel.md` (story 12-1, triaged 2026-09-15 from the windows-latest CI hang)
  summary: On Windows, a DETACHED agent ends up holding its spawning `kt` command's stdio pipe write-ends — any script that CAPTURES `kt agent start --detach` output (pipes) blocks until the agent exits; interactive terminals are unaffected (no pipes).
  evidence: the CLI detach test's forensic harness (agent_cli.rs `run_kt_agent_bounded`, 56ee766) proved kt EXITS (~1s, try_wait Ok(Some)) while its stdout/stderr pipes never EOF; std's spawn is documented to restrict inheritance to the child's own three stdio handles (attribute list), so the leak mechanism is unexpected — investigate (suspects: the Stdio::from(file) inheritable-dup path interacting with the job assignment, or a std behavior change) before fixing (candidates: explicit handle pinning at the Windows detached spawn, or DETACHED_PROCESS when the capture files already carry the output).
  routed: engine Windows detached-spawn handle handling — next Windows-touching story.
  resolved: story 15-4 (PR #283 / 800934c, 2026-10-17) — root cause found and fixed: std's default
  Windows spawn passes NO PROC_THREAD_ATTRIBUTE_HANDLE_LIST while bInheritHandles stays
  TRUE, and every stdio std provides (Stdio::from(File)/null()/piped()) is created or
  duplicated INHERITABLE — the detached child inherits the WHOLE inheritable-handle table,
  including the spawner's own pipe write-ends. The in-repo belief that std restricts
  inheritance to the child's three stdio handles was FALSE. Fix: the engine clears
  HANDLE_FLAG_INHERIT on its own three stdio handles around a detached spawn (panic-safe
  guard). DETACHED_PROCESS alone would NOT fix it (console flags do not touch handle
  inheritance).

## Deferred from: three-layer review of spec-15-1 (2026-10-16)

- source_spec: `spec-15-1-acp-real-agent-verification-opencode-hermes`
  summary: The real-agent smokes' visible skip NOTES (both the HEKMA_TEST_REAL_AGENT_SMOKES=0 opt-out and the HEKMA_TEST_REAL_AGENT_TURNS unset skip) are eprintln'd but INVISIBLE under a plain `cargo test` run — libtest captures stderr of passing tests, so the surfaced-not-silent posture only shows under the documented `--nocapture` commands; testing.md now says so.
  evidence: Verification-gap reviewer (review round 1): `turn_smokes_enabled`/`real_smokes_opted_out` notes emit via eprintln; a plain harness run shows `... ok` with no note. Pre-existing family behavior (the 14-4 gemini opt-out note has the same shape); a harness-level fix (e.g. a --format jitter or a note-via-test-name) is a test-infra decision beyond this story.
  resolved: story 16-1 (spec-16-1-skip-note-visibility, 2026-10-18) — the notes now write straight to the process's real stderr (`hekma_conformance::note_uncaptured`, fd 2 via `File::from_raw_fd` + `ManuallyDrop`, best-effort never-panic), so every real-agent smoke skip note is visible under a plain `cargo test`; the 3 silent binary-absent skips gained the same noted skip, and testing.md's limitation sentence flipped. Shipped in PR #296.

## Deferred from: three-layer review of spec-15-2 (2026-10-16)

- source_spec: `spec-15-2-detached-continuation.md` (review round 1, 2026-10-16)
  summary: Between-commands gap lines written to the raw `agent.log` BEFORE a `logs --follow` command starts remain INVISIBLE on every surface — the follow loop's raw cursor primes at the CURRENT end (so the pre-existing raw backlog never streams), and the one-shot `logs` reads the attributed capture only. The backlog is reachable today only through the attributed generations a live tailer ever wrote.
  evidence: Spec 15-2 review round 1 triage (the round's one DEFERRED finding): the priming rule is deliberate (replaying the whole raw file would duplicate lines the attributed view already carries), but a gap with NO attributed copy — output written while no engine session existed, before the follow command started — has no surface at all. Surfacing it is a PRODUCT/SURFACE decision, not a mechanical fix: a bounded raw backlog (e.g. the last N raw lines below the cursor) or a `--raw` mode changes what `logs` means and must be ratified as a surface choice (AI-58 draft-then-ratify), not slipped in as a review patch.
  resolved: story 16-2 (spec-16-2-raw-backlog-follow-prime, RATIFIED by Islam as the bounded-follow-backlog option; PR #298 / 2a028cb, 2026-10-18) — the follow's raw fallback cursor now primes at the last N complete lines (default 200, HEKMA_LOGS_BACKLOG_LINES), gap lines stream at follow start under the once-per-session qualifier, a gap beyond the window names its skipped count, and garbage env falls back to the default with a stderr note.

## Deferred from: three-layer review of spec-15-6 (2026-10-17)

- source_spec: `spec-15-6-memory-atomicity.md` (review round 1, 2026-10-17)
  summary: A `record_transition` failure AFTER the `→ starting` store transaction commits leaves the state committed with its transition EVENT unrecorded (no `instance.log` append, no bus publish, no attributed capture line) — the start then aborts with `EngineError::Log` while the row says `starting` (reconciled to `failed` only by a later adoption/reaper pass). The event append runs deliberately AFTER the commit (a subscriber never sees an uncommitted event, story 7-2's ordering obligation), so the window is between the store COMMIT and the durable log append.
  evidence: The round's single DEFER: a PRE-EXISTING window the old autocommit `set_state` path shared verbatim (state commit → event append had the same gap); the 15-6 extraction into `transition_with_backing_snapshot` made it VISIBLE but did not widen it. Not a regression of this story — closing it means either folding the event append into the store transaction (durable log as a store write — an architecture change to AD-12's file-based event log) or a compensating state-revert on append failure (its own partial-failure window); both are design decisions for a focused story, not review patches. Stated honestly in docs/architecture.md's concurrency section.
  resolved: story 16-3 (spec-16-3-transition-append-compensation; PR #302, 2026-10-18) — a failed post-commit event append now compensates the row back to `prior` with the Log error naming the compensation; a failed compensation names both failures and leaves the row honestly at `new`; the success path is byte-identical. docs/architecture.md's concurrency section now states the compensated semantics.

## Deferred from: sprint-change-proposal-2026-09-30-hosted-platform-readiness (correct-course, Batch; approved 2026-09-30)

- source_spec: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md` (privilege separation + attested egress)
  summary: In-container enforcement that the agent cannot bypass (privilege separation between the Service Host and the agent, plus an attested-egress header an external choke point can verify) is DEFERRED; the first Host keeps enforcement of record at its external choke point.
  evidence: the proposal §1 threat-model finding; docs/design/metering-agents-you-dont-control.md:94 (the loopback bind is not a sandbox)
- source_spec: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md` (windowed + runtime-hour budgets)
  summary: Per-window (daily/monthly) and runtime-hour (compute-time) budgets stay DEFERRED; trigger: a Host asks for either against Hekma's own evaluator.
  evidence: PRD FR-18 note (per-window deferred); ARCHITECTURE-SPINE.md Deferred list
- source_spec: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md` (lifecycle hooks)
  summary: Pre-start / pre-stop lifecycle hooks (e.g. state hydrate/dehydrate for ephemeral disks) are DEFERRED; trigger: a Host shows its own hook machinery failing where engine hooks would not.
  evidence: the proposal §4.6 item 3
- source_spec: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md` (declarative fleet file)
  summary: A declarative fleet file reconciled at Service Host boot is DEFERRED; trigger: a Host runs more than one agent per Service Host (the first Host runs one per container).
  evidence: the proposal §4.6 item 4; Story 17.2 scope (renumbered from 15.2)
- source_spec: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md` (incremental SSE relay + multi-route upstreams)
  summary: The store-and-forward relay (whole-body buffering, up to 64 MiB) and the single upstream per instance stay; incremental SSE relay and multi-route upstreams are DEFERRED; trigger: a Host whose model path streams to the agent, or reaches more than one upstream.
  evidence: crates/hekma-engine/src/metering/listener.rs:147-162, 372-393
- source_spec: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md` (approval routing + `hekma conformance` CLI)
  summary: ACP `session/request_permission` stays auto-denied (D3) and the TCK stays a Rust library; routing approvals to the Host and a conformance CLI for non-Rust adapter authors are DEFERRED; triggers: the first Host that surfaces approvals to its users, and the first non-Rust adapter author.
  evidence: ARCHITECTURE-SPINE.md AD-19 (permission requests answered denied); hekma-conformance is publish=false and library-only
- source_spec: `spec-17-1-host-mode-spec-the-shim-crate-and-the-lock-model.md`
  summary: CI-runner serve-shape headroom watch — the run of record's tight budget (~1.03× CPU headroom at 1.94%/instance, effective 10) was measured on an 8-core local host; the blocking CI serve step runs on 2-4 vCPU shared runners where oversubscription shifts the process-wide aggregate math.
  evidence: docs/testing.md serve-shape measured record (2026-10-19 run + provenance note: heavier development loads measured 6.55-7.61%/instance); .github/workflows/ci.yml perf-budgets serve step. Trigger: the first red CI serve run — a maintainer call on the ×1.5 tolerance vs reader count, not a silent re-baseline.

## Deferred from: three-layer review of spec-17-2 (2026-10-19)

- source_spec: `spec-17-2-hekma-serve-the-long-running-host.md` (review round 1, 2026-10-19)
  summary: Real Windows Ctrl+C e2e for the host needs a console-event harness — the serve CLI e2e's POSIX legs use `kill`/SIGTERM shapes that have no Windows equivalent a test can inject without a helper that attaches to the child's console and fires CTRL_C_EVENT; on Windows the same host surface is exercised by the signal-injected factory tests (the lib-level `serve_with_signal` runs with an immediate factory) and by the real `ctrl_c` stream registration, but no test drives a REAL console event into a serving child.
  evidence: crates/hekma/tests/serve_cli.rs (the is_windows runtime-skip arms); crates/hekma-host/src/signals.rs (the windows arm registers the real handler; only the event injection is missing). Trigger: the first Windows-specific serve-shutdown defect — build the console-event harness (a helper that GenerateConsoleCtrlEvent(CTRL_C_EVENT) into a shared-console child) rather than arguing the path works.
- source_spec: `spec-17-2-hekma-serve-the-long-running-host.md` (review round 1, 2026-10-19)
  summary: Fleet-drain wall clock — the review's finding 4 asked for parallel stop ladders ("total ≈ one window, not Σ"); the host now ISSUES all stops concurrently and joins them (the correct seam), but the engine's stop ladder runs under the supervisor mutex for its whole duration (graceful window included — `Engine::stop` locks, then `supervisor.stop` → `backend.stop(handle, window)` inside the lock), so escalated stops QUEUE: worst case Σ(window) over the fleet, past a 90 s service-manager budget at ten agents × 30 s. Making the ladders parallel means releasing the supervision lock during the window phase — the archived per-instance lock-model plan, an architecture decision (draft-then-ratify), never a review patch; the coarse two-mutex model was measured and re-ratified by the serve-shape scenario for its shape.
  evidence: crates/hekma-engine/src/engine.rs (`Engine::stop`'s lock-then-ladder closure); crates/hekma-engine/src/domain/supervisor/lifecycle.rs (`stop_inner` → `backend.stop(&mut handle, window)` under `&mut self`); docs/testing.md serve-shape scenario (the ratified lock model); the host-side issue-all/join structure is in place (crates/hekma-host/src/drain.rs) and becomes parallel the moment the engine's stops do.

## Deferred from: implementation of spec-17-4 (2026-10-19)

- source_spec: `spec-17-4-export-for-ephemeral-hosts-outbox-and-log-capture-modes.md` (cross-lifetime capture-mode read honesty)
  summary: The capture-read refusals (`Off` refuses every capture read naming "log capture is off"; `Metadata` refuses the attributed read naming `--log-capture metadata`) consult the ENGINE INSTANCE's own mode (`Supervisor::log_capture_mode`), so they hold same-lifetime (the serve host's engine, any embedder passing `EngineOptions::log_capture`). A SECOND engine on the same state root — the ordinary `kt agent logs <name>` CLI process — opens with the DEFAULT mode (`full`), so an off-hosted state root's log read answers SUCCESS + EMPTY instead of the refusal: honest absence fails cross-lifetime because the mode under which the spawn ran was never persisted (the off-mode spawn deliberately wrote no files, so there is nothing to derive the fact from — the outbox-seq rederive pattern has no analog here).
  evidence: crates/hekma-engine/src/engine.rs `capture_read_refusal` + the four guarded read methods (mode from `inner.supervisor`); crates/hekma-engine/tests/outbox_export.rs pins the refusal same-lifetime only. Closing it means persisting the engine-wide capture posture at host open (a new small state-root record written by EVERY serve open — including `full`, so a mode switch un-stales it — and read by the guard when the opening engine's own mode is the default) — a durable state-schema addition and a host-vs-CLI open distinction: an architecture decision (draft-then-ratify per AI-58), not a review patch. Trigger: a Host (or the retrospective) treats the cross-process empty-read as a defect against AI-18's "surfaced, not silent".

## Deferred from: review round 1 of spec-19-1 (2026-10-23)

- source_spec: `spec-19-1-governance-conformance-vectors.md` (review round 1, 2026-10-23)
  summary: Skip-arm-on-read-error in `enforce_budget` — a failed totals/cost read degrades to 0 and the decision runs on the fabricated total (a zero ceiling fires a breach on a fabricated 0 total). Pre-existing 3-2/3-3 degrade-to-zero semantics; story 19-1 review round 1 added the surfaced-not-silent diagnostic (the supervisor's enforcement breadcrumb names the instance + the degraded scope, decisions unchanged). Changing the DECISION (skipping the arm / failing closed on a degraded read) is a money-path behavior change deferred to its own story.
  evidence: `crates/hekma-engine/src/domain/supervisor/usage.rs` — the four `surfaced_read` sites in `enforce_budget` (the zero substitute flows into `governance::GovernanceInput`); the behavior pinned by `a_failed_totals_read_surfaces_the_degradation_and_decides_on_the_zero` (supervisor tests, via the cfg(test) `totals_read_fault_names` seam): breach fires exactly once with limit 0 / observed 0 on degraded reads, diagnostic surfaced. Trigger: a Host reports a misfire (a breach attributed to a read failure rather than real usage).

## Deferred from: implementation of spec-17-5 (2026-10-20)

- source_spec: `spec-17-5-static-linux-release-and-the-container-recipe.md` (static aarch64 Linux — musl AND glibc)
  summary: Static aarch64 Linux release artifacts are DEFERRED — the release matrix gains only the `x86_64-unknown-linux-musl` leg this story; there is no ARM Linux archive (musl or glibc) for a Host to download, and the musl recipe is x86_64-only.
  evidence: the spine's Deferred item (`_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md:359`, the static-aarch64 entry); the story bullet (`_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md:379`, "aarch64 is deferred"); the release matrix (`.github/workflows/release.yml:16-43` — the four existing legs plus this story's x86_64 musl leg, still no ARM leg). Trigger: the first Host runs ARM containers, or an ARM Linux release runner is available.
- source_spec: `spec-17-5-static-linux-release-and-the-container-recipe.md` (installer prefers the static musl archive on Linux — supervisor ruling 2026-10-20)
  summary: The Linux installer keeps handing out the glibc archive; making it PREFER the static musl archive on Linux (so plain-installed binaries also run on scratch/distroless and musl systems) is DEFERRED — the static archive stays a manual release-page download for containers/scratch.
  evidence: supervisor ruling 2026-10-20 (spec change log, Ask First ruling 2): switching the Linux default is a real behavioral change — `scripts/public/install.sh` `detect_release_target` (`:282-295`, the target detection with no musl arm; `:406`, the asset name) and its pinned installer tests (`scripts/test_automation.py`, the `install prebuilt x86_64-unknown-linux-gnu (hekma + hkm)` contract) would all move. Trigger: a Host asks for it.


## Deferred from: review round 1 of spec-18-1 (2026-10-24)

- source_spec: `spec-18-1-readiness-probes.md` (review round 1, 2026-10-24)
  summary: The probe wait holds the supervision lock for up to the declared `timeout_ms`, so every OTHER instance's supervision (reaper passes, crash detection, restart execution) pauses while one instance's probe waits — the same accepted bounded-wait posture as the ACP handshake, and the declared budget caps the pause, but a multi-instance host starting an instance with a long probe budget freezes its co-hosted instances' supervision for that long. A lock-free wait (release the lock during the poll loop, or run the wait off-path) is the fix.
  evidence: `crates/hekma-engine/src/domain/supervisor/spawn.rs` — the readiness-probe block runs `run_probe` synchronously inside the start path's `&mut self` scope; docs/manifest.md's `[readiness]` section now states the pause and advises short budgets for multi-instance hosts. Same architecture class as the 17-2 fleet-drain wall-clock deferral (parallel stop ladders need the lock model revisited) — an architecture decision (draft-then-ratify per AI-58), never a review patch. Trigger: a Host runs several agents and reports co-hosted supervision gaps during starts.
- source_spec: `spec-18-1-readiness-probes.md` (review round 1, 2026-10-24)
  summary: A host killed MID-PROBE-WAIT (crash, kill -9) leaves the instance row `starting` with a possibly-orphaned process: the readiness-timeout failure never lands (the start path died holding the lock), and the open-time reconcile does not classify a `starting` row whose process is still alive as probe-failed — the next command adopts the surviving process and proceeds as if no gate had been declared. Honest classification needs a starting-row + live-process + declared-probe arm in the open/reconcile path (or a persisted probe-start marker the reconciler can time out).
  evidence: `crates/hekma-engine/src/domain/supervisor/spawn.rs` — the transition to `failed` runs only in-process (the `TimedOut` arm); the adoption path (`orphan`/open reconcile) has no readiness arm. The detached-start story's terminal-state guarantee holds only same-lifetime. Trigger: a Host kills the service mid-start and reports the instance stuck `starting` (or a running agent that never passed its gate).
- source_spec: `spec-18-1-readiness-probes.md` (review round 1, 2026-10-24)
  summary: The readiness tests' `free_port()` helper binds port 0, reads the port, and drops the listener — a TOCTOU window where another process can claim the port between the drop and the fake_agent's re-bind. A flake would look like a probe timeout or a bind failure in the wrong test. The std-only fix is a passed prebound `TcpListener` (the fake_agent inherits or re-uses it), which needs a fixture-side seam.
  evidence: `crates/hekma-engine/tests/readiness.rs` `free_port()` (bind 0 → local_addr → drop); `crates/hekma-conformance/src/bin/fake_agent.rs` `serve_http_ready` re-binds by number. Trigger: an observed CI flake on the http-get readiness tests — fix the seam, do not retry-loop the port grab.

## Deferred from: review round 1 of spec-18-2 (2026-10-24)

- source_spec: `spec-18-2-liveness-unresponsive.md` (review round 1, 2026-10-24)
  summary: Per-tick liveness evaluation budget (D1) — each reaper pass evaluates every `running` instance's declared check with no per-pass cap, so a host co-hosting N liveness-declaring instances pays N checks per ~250 ms tick (an http-get check is a real loopback connection). A per-pass budget or round-robin (cap checks per pass, rotate the remainder across passes — windows are seconds-wide, a one-pass deferral is invisible) is deferred.
  evidence: `crates/hekma-engine/src/domain/supervisor/reaper.rs` — the liveness arm iterates `self.running` unconditionally on every pass; docs/manifest.md's `[liveness]` section states the cost model honestly. Trigger: a Host co-hosts enough liveness-declaring instances that a pass's check fan-out is measurable.
- source_spec: `spec-18-2-liveness-unresponsive.md` (review round 1, 2026-10-24)
  summary: Clock-step test harness (B2a) — the liveness window tests sleep real wall-clock time (windows of 250-2500 ms, several seconds per drill) and are exposed to scheduler jitter at their bounds; a virtual-clock harness (step the reaper's clock deterministically) would make them instant and jitter-proof. Needs a clock seam through `Supervisor` (an injected `Instant`-source), an architecture-adjacent change to the reaper's shape — not a review patch.
  evidence: `crates/hekma-engine/tests/liveness.rs` (the wall-clock waits and `saturating_sub` slop bounds); `crates/hekma-engine/src/domain/supervisor/reaper.rs` (`std::time::Instant::now()` read directly in the window arms). Trigger: an observed timing flake in the liveness suite on CI, or the next window-semantics story that would otherwise add more sleeping drills.
- source_spec: `spec-18-2-liveness-unresponsive.md` (review round 1, 2026-10-24)
  summary: Open-window visibility (B2b, the P9 defer path) — while a declared window is OPEN (signal failing, not yet expired) the operator has no queryable view of that fact: `fleet`/`show` carry the lifecycle state and the last failure cause, not "a liveness window has been open for X ms". Surfacing it needs a new field on the versioned `FleetEntry` schema (a public wire/JSON surface change — additive, but a schema bump with conformance-report implications), never a silent in-memory-only read.
  evidence: `crates/hekma-engine/src/fleet.rs` (the versioned `FleetEntry`); the window state lives only in `Supervised::liveness_failing_since` (`crates/hekma-engine/src/domain/supervisor/mod.rs`), in-memory, dropped with the engine. Trigger: an operator asks to see open liveness windows (the diagnostic pair + the expiry are the only surface today).
- source_spec: `spec-18-2-liveness-unresponsive.md` (review round 1, 2026-10-24)
  summary: NTP clock-step resilience for file-age freshness (the production side of the round's clock findings; the B2a entry above is the test-harness cousin) — `file_fresh` derives a heartbeat's age from the file's WALL-CLOCK mtime against `SystemTime::now()`, so a FORWARD NTP step instantly stales every healthy heartbeat (fleet-wide StartWindow → mass unresponsive restarts; the `Instant`-based window only bounds each blast to one window) and a BACKWARD step grants a hung agent fresh-status for the whole step (a future mtime is clamped to age 0 = fresh). Amended at the 18-4 review round (2026-10-24, D3): the DRAIN's idle_file wait rides the SAME `file_fresh` predicate (2 s slack), so the same distortion applies — a forward step can stale a genuinely-idle agent's marker and burn a drain's bound; a backward step can read a still-working agent's marker as idle and complete the drain early. The drain-level mitigations are documented honesty (the 2 s slack, the bound, the wall-clock note in the manifest docs), not a fix. Deeper mitigations (mtime-monotonic cross-check, or deferring the first post-step failure) are heuristics with their own failure modes — not a review patch.
  evidence: `crates/hekma-engine/src/domain/probe_kinds.rs` (`file_fresh` — `metadata().modified()` vs `SystemTime::now()`, future-mtime clamp) — shared by the liveness heartbeat AND the drain idle wait (`crates/hekma-engine/src/domain/supervisor/drain.rs` calls it with `DRAIN_IDLE_FRESH_SLACK_MS`); the liveness window clock in `crates/hekma-engine/src/domain/liveness.rs` is `Instant`-based (monotonic) but the SIGNAL it observes is wall-clock. Trigger: a fleet on stepping-NTP infrastructure reports false liveness windows or missed staleness across a step — or a drain completes/times out wrongly across one.


## Deferred from: review round 1 of spec-18-3 (2026-10-24)

- source_spec: `spec-18-3-exit-code-classes.md` (review round 1, 2026-10-24 — patch-round finding)
  summary: A fake_agent fixture flag for a TRUE 7→75→7 interleave — per-launch exit selection. Today launch args are snapshotted at registration (a manifest rewrite cannot change a chain's relaunch args) and `--crash-times` bounds crashes to the FIRST N launches, so an armed self-crash always lands BEFORE any declared `--exit-with` on the same launch and the declared exit fires on EVERY launch. The result: charged crashes can only precede requested recycles in a chain, and the review's ideal drill — build a requested-recycle streak, then charge a STABLE crash, then prove the streak was forgiven (depth restarts at 1) — is unbuildable. The review round pinned the mechanics instead: the B3 rework (7,7,75,75) kills the charging/zeroing mutants and pins the surfaced depth, and the streak-drop is unconditional in `plan_restart` (A2/A6). A flag like `--exit-with-launches "4:=75"` (exit-with only on launch N+) — or an exit bound tied to the crash-state counter — would unlock the true interleave drill.
  evidence: `crates/hekma-conformance/src/bin/fake_agent.rs` — `--exit-with` fires unconditionally in the linger loop for every launch; `effective_crash_after` bounds only the crash; the C5 cross-check deliberately keeps bounded `--crash-times` configs legal at any deadline (the B3 shape depends on it); `crates/hekma-engine/tests/exit_codes.rs` `restart_requested_exit_relaunches_without_charging_the_counter` documents the shape deviation. Trigger: a story needs to prove stable-crash streak forgiveness end-to-end, or the review of a future restart-policy story asks for a charged-crash-after-recycles drill.
- source_spec: `spec-18-3-exit-code-classes.md` (review round 1, 2026-10-24 — item D1)
  summary: Adoption resolves liveness and exit codes via TWO INDEPENDENT reads of the same manifest file — `adapter_launch_facts` is called twice in the adoption path (once for the liveness resolution, once for the classes), so a manifest rewritten between the two reads yields MIXED-GENERATION supervision (liveness from generation A, exit codes from B) for the adopted instance's lifetime. The fix is a read-once resolver variant that resolves every declared section from ONE parsed document — it touches the three-sibling resolver API (`resolve_readiness_probe`/`resolve_liveness_check`/`resolve_exit_codes` all take the manifest path independently), an API-shape change beyond a review patch.
  evidence: `crates/hekma-engine/src/domain/supervisor/reaper.rs` — the adoption path's two `adapter_launch_facts` + read + parse round-trips; the "don't spam the same degradation twice" ordering note only holds because the liveness call happens to run first (blind-hunter finding, review round 1). Trigger: a host reports mixed-generation supervision (a liveness window from one manifest generation governing an exit-code table from another) after a manifest rewrite lands mid-adoption.


## Deferred from: review round 1 of spec-18-4 (2026-10-24)

- source_spec: `spec-18-4-cooperative-pause-drain.md` (review round 1, 2026-10-24 — item D1)
  summary: The drain's whole bounded phase runs UNDER THE INSTANCE'S SUPERVISION LOCK — step (a)'s command, step (b)'s idle/child wait, all of it — so OTHER instances' reaper-driven supervision (readiness probes, liveness checks, crash detection) pauses for up to the drain's bound while one drain waits (documented in the manifest docs, the readiness section's accepted-posture precedent). A3's child-poll inside the wait exists precisely BECAUSE of this hold: the ~250 ms reaper cannot poll the child for us from outside the lock, so the drain polls it directly. The rework — a wait that parks without holding the lock (a lock-free parked-drain state the reaper observes, or a supervisor-channel poll seam) — changes the supervisor's lock architecture, not a review patch. The current shape is honest about the trade and bounded by the drain's own three-tier bound.
  evidence: `crates/hekma-engine/src/domain/supervisor/drain.rs` (the wait loop polls `self.backend.poll` under the lock — the code comment names the defer); `crates/hekma-engine/src/domain/supervisor/mod.rs` (the supervision lock the reaper's per-instance work also takes). Trigger: a host co-hosting many instances reports supervision stalls (late probes/liveness windows) that correlate with long drains, or a future drain feature needs the reaper to act during the wait.
- source_spec: `spec-18-4-cooperative-pause-drain.md` (review round 1, 2026-10-24 — item D2)
  summary: The lifecycle-command overrun kill is SINGLE-PID — `run_bounded_command` kills the direct child only (`kill_and_reap_bounded` → `std::process::Child::kill`, plus the `KillOnDrop` fire-and-forget guard), NOT the child's process group / job object (the STOP ladder's escalation is group-based; this seam is not). A pause command that spawns its own children and then overruns its cap leaves those GRANDCHILDREN orphaned and running after the kill — the engine reports the overrun honestly (with the bounded-reap note when even the direct child would not confirm), but the collateral processes are the adapter's to manage. A process-group/job-object lifecycle for spawned commands needs a backend-trait seam (spawn-into-group + group-kill across Linux/macOS/Windows job objects) — an adapter-api/backend shape change, not a review patch.
  evidence: `crates/hekma-engine/src/domain/supervisor/drain.rs` (`run_bounded_command` — plain `Command::spawn`, `kill_and_reap_bounded`, `KillOnDrop`; no `process_group`/job-object plumbing, contrast the stop ladder's group-based escalation); the B7 TERM-immune drill (`sh -c "trap '' TERM; sleep 60"`) pins the direct-child kill only. Trigger: an adapter's pause command reports orphaned grandchildren after a cap kill (or a review of a future command-spawning surface asks for group semantics).
