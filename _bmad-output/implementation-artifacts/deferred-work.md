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
- source_spec: `_bmad-output/implementation-artifacts/spec-6-2-round1-triage-fixes.md`
  summary: hermes.rs module doc claims later tests start from a pristine PATH — true only on the happy path while the restore is not panic-safe.
  evidence: `crates/ktesio-engine/tests/hermes.rs:21-28` "restored at teardown so any test added to this binary later starts from a pristine environment" holds only if no test panics between install and restore; wording should soften or follow the RAII fix above.
- source_spec: `_bmad-output/implementation-artifacts/spec-6-2-round1-triage-fixes.md`
  summary: The engine-tests OS-cfg allowlist covers the whole `crates/ktesio-engine/tests/` directory while only `memory.rs:605` uses cfg — narrow the allowlist to the single file.
  evidence: `.github/workflows/ci.yml` allowlist entry whitelists the directory; the corrected comment (vg-o2) names memory.rs:605 as the sole user. Narrowing is a CI behavior change, out of the comment-only scope of vg-o2.
- source_spec: `_bmad-output/implementation-artifacts/spec-6-2-round1-triage-fixes.md`
  summary: No test pins that a manifest declaring kind `hermes` WITHOUT a `[lifecycle.start]` table yields NoStartTemplate with no builtin-table fallback.
  evidence: The new precedence test (blind-19) covers only manifest-with-start beating the builtin table; the no-start-table complement relies on pre-existing engine semantics in `resolve_start_launch` (`crates/ktesio-engine/src/adapter/mod.rs:275`).
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
- source_spec: `spec-epic-12-durable-detach-observed-channel.md`
  summary: The engine-level cross-lifetime detach e2e (`detached_start_survives_a_clean_engine_exit_and_the_next_engine_adopts`) still runtime-skips on the linux-x86 CI leg (pre-existing #109 mitigation); the CLI-level e2e covers the behavior there, but the facade-level harness runs only on macOS/Windows legs.
  evidence: epic-12 step-04 verification-gap reviewer (rounds 1+2, 2026-09-15) — compensation verified (unskipped `kt`-binary e2e on Linux); fold into the #109 re-exec-deadlock follow-up.
- source_spec: `spec-epic-12-durable-detach-observed-channel.md`
  summary: pause × detach cross-lifetime edge — a detached handle paused (SIGSTOP) in-process and then dropped at command exit strands the child frozen and unsupervised until a later command resumes/stops it.
  evidence: epic-12 step-04 edge-case-hunter (round 1, 2026-09-15) — needs a product call (resume-before-release vs kill vs document-only; documented-only in commands.md today); the backend handle does not track pause state, so it is not a trivial patch.
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
  resolved-note: ROOT CAUSE found 2026-10-17 (story 15-4 investigation): std's default
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
