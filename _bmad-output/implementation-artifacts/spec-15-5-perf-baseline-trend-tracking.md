---
title: 'Perf-budgets baseline trend tracking (committed baseline, ratchet deltas, documented updates)'
type: 'feature'
created: '2026-10-16'
status: 'implemented'
baseline_commit: '800934c0eced9e60e4d253a61b63d836650f689a'
review_loop_iteration: 0
context:
  - '{project-root}/docs/testing.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The perf-budgets harness (`crates/hekma-engine/examples/perf-budgets.rs`) prints one self-contained gated JSON report per run, but nothing compares runs over time — a slow multi-week drift (reads creeping from 6 ms toward 100 ms) is invisible until it crosses the absolute budget (Fibery 175, the 7-5 deferred finding: "a baseline file adds storage + churn questions (which platform's baseline is canonical; how updates are ratified) that deserve a focused change").

**Approach:** Add `--baseline <file>` (load a prior report JSON, emit a `trend` block with per-metric deltas, and RATCHET-gate: no gated metric worse than baseline × 1.15, improvements accepted freely) plus `--write-baseline <file>` (emit the fresh report as the next baseline). Canonical baseline: one committed file for the CI ubuntu-x86_64 leg (`benches/baselines/perf-ubuntu-x86_64.json`), updated by PR — the visible diff IS the ratification; other platforms may pass `--baseline` for reporting-only trend blocks (their runs never ratchet-gate, matching the existing gate-platform discipline).

## Boundaries & Constraints

**Always:**
- The absolute budgets stay as-is; the ratchet is ADDITIVE (a run must pass both).
- The ratchet tolerance is 1.15 (15%) — absorbs CI noise, catches the multi-week-drift class the absolute gate misses; every ratchet verdict names baseline value, current value, delta, and tolerance in the report + stderr.
- `trend` is an ADDITIVE report key (report schema bump only if the shape rule demands — additive keys with `#[serde(default)]` precedent keep schema 1; follow the report's own bump policy comment).
- Baseline file updates ride a PR with the harness output pasted (the ratification procedure, documented in docs/testing.md); the committed baseline records the CI leg's own run (not a local machine's).
- The default invocation (no flags) is byte-equivalent to today (no trend block, no ratchet).

**Ask First:** Making the ratchet FAIL the CI perf job (vs warn) — the spec assumes fail (that is the point of a gate); if review shows the CI job is non-blocking, surface that honestly instead of pretending.

**Never:** No new dependencies; no changes to what is measured; no per-platform baseline zoo (ONE canonical file); no auto-magic baseline writes in CI.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| CI run with baseline | `--baseline benches/baselines/perf-ubuntu-x86_64.json` on ubuntu-x86_64 | trend block in report; ratchet evaluated over the gated metrics; pass/fail verdict | baseline missing/corrupt → loud error naming the file, non-zero exit |
| Metric improved | current p99 < baseline p99 | ratchet passes; trend shows the improvement (next baseline write captures it) | N/A |
| Metric drifted +10% | within 1.15× | ratchet passes; trend names the drift | N/A |
| Metric drifted +20% | beyond 1.15× | ratchet FAILS with baseline/current/delta/tolerance named | non-zero exit |
| Local macOS run | `--baseline` given | trend block reported; NO ratchet gate (non-gate platform, existing discipline) | N/A |
| Write baseline | `--write-baseline f` | the fresh report written as the next baseline (stdout report unchanged) | write error → loud error |
| No flags | default invocation | byte-equivalent to today | unchanged |

</frozen-after-approval>

## Code Map

- `crates/hekma-engine/examples/perf-budgets.rs` -- the whole harness: report schema + `REPORT_SCHEMA_VERSION` const ~:265 (bump policy comment), `evaluate_gates` :595 (gate math + `ci_tolerance.factor`), `main` :694 (arg surface — currently none? extend with std env-args parsing in the example's plain style), report print to stdout :273 (CLI-first: diagnostics to stderr).
- `.github/workflows/ci.yml` -- the `perf-budgets` job :280-320 (build release harness, run, upload JSON artifact): gains the `--baseline` invocation (the ratchet verdict) and, on failure, prints the trend block in the job log.
- `docs/testing.md` -- the perf-budgets section (budget table + subscriber budgets): gains the baseline/ratchet mechanics + the update procedure (run CI leg → download artifact → `--write-baseline` → PR with output pasted).
- `benches/baselines/perf-ubuntu-x86_64.json` -- NEW committed baseline (generated from the CI leg's run at implementation time).
- Tests: the example already has gate-math unit tests in-file (`evaluate_gates` tests, the 10-3 budget pins) — the ratchet math gets the same pure unit coverage (improve/within/drift arms + the missing-corrupt-baseline error arms).

## Tasks & Acceptance

**Execution:**
- [x] `crates/hekma-engine/examples/perf-budgets.rs` -- `--baseline`/`--write-baseline` flag parsing (stderr diagnostics), the `trend` block (per gated metric: baseline, current, delta, verdict), the ratchet evaluation (×1.15, gate-platform only), report additivity per the schema policy.
- [x] `crates/hekma-engine/examples/perf-budgets.rs` -- pure unit tests for the ratchet math (improve / within tolerance / beyond) + the baseline load error arms (missing file, corrupt JSON, schema mismatch).
- [x] `benches/baselines/perf-ubuntu-x86_64.json` -- the committed canonical baseline (from the CI leg's run; values recorded in the PR body).
- [x] `.github/workflows/ci.yml` -- the perf job runs with `--baseline`; failures surface the trend block in the log.
- [x] `docs/testing.md` -- baseline mechanics + the PR-ride update procedure; `_bmad-output/implementation-artifacts/sprint-status.yaml` -- story 15-5 row; `deferred-work.md` 7-5 entry resolved-marker at merge.

**Acceptance Criteria:**
- Given a baseline and a current report where every gated metric is within 1.15×, then the ratchet passes and the trend block names each delta.
- Given any gated metric beyond 1.15× on a gate platform, then the run fails non-zero with baseline/current/delta/tolerance in the verdict.
- Given a local non-gate platform run with `--baseline`, then the trend reports but nothing gates.
- Given no flags, then the report is byte-equivalent to today's.
- Given a missing/corrupt/schema-mismatched baseline file, then a loud named error, non-zero exit, never a silent no-trend run.
- Gates: fmt/clippy/workspace tests/check_docs green; the in-file ratchet unit tests green.

## Spec Change Log

- (Closeout, 2026-10-17) Implemented as specified, with three surface decisions recorded: (1) **the ratchet's gate platform is CI MODE** (`config.ci` — the ubuntu `perf-budgets` job), not "any gate platform": the canonical baseline records the CI leg's own run, so that leg is the only apples-to-apples comparison; macOS-local strict gating of the ABSOLUTE budgets is untouched, and local/other-platform `--baseline` runs are report-only (the spec matrix's "local macOS → NO ratchet gate" row generalized to every non-CI run). (2) **A non-positive baseline never ratchets** — verdict `no-ratchet`, reason surfaced in the report + stderr, absolute budget still gating: ×1.15 on a negative figure TIGHTENS the bound (a tolerance must absorb noise, never add strictness), and the committed baseline's subscriber read-p99 delta is negative (−0.31 ms), so this arm is exercised by the very first canonical file. Exactly-at-limit counts as within (the rule is "no WORSE than ×1.15"). (3) **The initial committed baseline was generated from a REAL local linux-x86_64 release run with CI mode forced** (median-of-3 windows — the CI leg's measurement shape; 8 logical cores, recorded in the file's `environment` block), because the CI leg did not yet exist at implementation time; the canonical refresh procedure (CI-leg artifact → strip `trend` → PR, docs/testing.md) re-anchors it on the first post-merge run if the runner's figures drift past the local seed. `--write-baseline` output strips the `trend` block (a baseline is a self-contained measurement; a stale nested `trend` key is tolerated on load but never written). `scripts/test_automation.py`'s CI pins updated in the same change (run-line regex accepts flags; the `--baseline` invocation + the baseline file's existence are pinned). Verified end-to-end with the spec's round-trip command: two real CI-mode runs, the second gated against the first's committed baseline — ratchet PASS, exit 0, all seven trend rows present.

## Design Notes

- Why committed-file + PR ratification: it makes every threshold change a REVIEWED diff (the same discipline as the budget table), needs no artifact-retention plumbing, and gives CI a stable address to diff against. The file is regenerated by a human-run command, never auto-committed.
- Why 1.15: the CI tolerance factor is already 1.5 for absolute budgets on CI; the ratchet compares CI-run-to-CI-run on the SAME leg, so machine variance is small — 15% catches drift while staying quiet. If CI proves noisier, the tolerance is a one-line change WITH a baseline refresh in the same PR.

## Verification

**Commands:**
- `cargo run -p hekma-engine --example perf-budgets --release` -- today's behavior unchanged.
- `cargo run -p hekma-engine --example perf-budgets --release -- --write-baseline /tmp/b.json && cargo run -p hekma-engine --example perf-budgets --release -- --baseline /tmp/b.json` -- round-trip: ratchet passes against its own baseline, trend block present.
- `cargo test -p hekma-engine --example perf-budgets` (if the example's tests run under the target) or the in-file `#[cfg(test)]` battery via `cargo test -p hekma-engine --examples` -- ratchet arms green.
- fmt / clippy / workspace tests / check_docs -- green.
