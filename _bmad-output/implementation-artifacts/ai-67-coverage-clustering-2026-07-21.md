# AI-67 — CI coverage-job failure clustering (forensic case file)

- **Case slug:** ai-67-coverage-clustering
- **Opened:** 2026-07-21
- **Status:** Concluded — root cause identified, confidence HIGH
- **Mode:** READ-ONLY diagnostic. No workflow/source/config edits; no CI triggers (only read-only `gh` log reads).
- **Repo:** Ktesio/ktesio @ main

## Hand-off Brief (15-second read)

The coverage job is red because ~6 real-child-process-IO tests assert their stdin/stdout round-trip
completes within a **fixed wall-clock deadline** (5s / 10s / 20s), and under `cargo tarpaulin`'s LLVM
instrumentation on the free-tier ubuntu runner those round-trips are ~100x slower (the interaction
test binary alone took **87s** vs <60s for the whole uninstrumented suite), so a shifting subset of
those 6 tests **misses its deadline** every attempt. It is NOT OOM (14Gi RAM free, 0 swap used, flat
across attempts), NOT a degraded runner instance (three independent fresh runners fail the same pool),
and NOT accumulated retry residue (resources flat, and attempt 3 does *better*, not worse). The
3-attempt whole-job retry cannot converge because each attempt re-rolls the same deterministic latency
cliff on the same class of runner. **H-A refuted, H-B refuted as cause, H-C partially (same fragility
family as #109 but a latency-miss, not the D-state deadlock); the true root cause is H-D:
instrumentation-throughput starvation vs. hard-coded in-test wall-clock deadlines.**

## Case Info / Problem Statement

CI `coverage` job (cargo tarpaulin, per-crate split, free-tier `ubuntu-latest`) red at merge for most
epics. Within a single job attempt the same small cluster of real-child-IO tests fails together, while
a local run under the identical tarpaulin config passes. A whole-job retry wrapper (3 attempts) was
added but exhausts its budget. Diagnose the clustering before deciding paid runner vs. demote the gate.

## Evidence Inventory

| Source | Status | Notes |
|---|---|---|
| `.github/workflows/ci.yml` coverage job + retry wrapper (lines 433-668) | Read in full | Crux artifact |
| `.config/nextest.toml` | Read | Coverage uses tarpaulin, NOT nextest; adoption D-state serialized there (#106) |
| `crates/ktesio-engine/tests/interaction.rs` | Read in full | 4 of the 6 failing tests; deadlines here |
| `crates/ktesio-engine/src/backends/unix/mod.rs` (tests 1509-1817) | Read | 2 of the 6 failing tests; deadlines here |
| Coverage job logs: #119-final (job 88675584531), #117-PR (88224732788), #117-pushmain (88367810298) | Fetched + parsed | 3 fresh runners, 2 branches — decisive |
| Issue #109 / AI-60 | Cross-referenced via code comments | Adoption D-state; skipped on Linux CI here |

## Hypotheses (never deleted; Status updated)

### H-A — persistently-degraded runner INSTANCE — **REFUTED**
- Claim: one unlucky runner under memory/IO pressure fails every real-IO test in that attempt; a fresh
  runner (`gh run rerun`, new machine) would pass.
- **Refuting evidence:** THREE independent job runs (#119-final, #117-PR, #117-pushmain — two branches,
  three fresh GitHub VMs) all fail the IDENTICAL 6-test pool with the identical structure
  (interaction binary fails ×2 attempts, backend lib fails ×1). Resource readings are HEALTHY on every
  attempt of every run (14Gi available RAM, 0B swap used, 87-88G disk free) — nothing is degraded.
  Within #119-final, attempt 3's interaction tests PASSED after failing attempts 1-2 — the runner is
  not monotonically deteriorating. A fresh runner does NOT pass → refuted.

### H-B — state not reset between same-job retry attempts — **REFUTED as the CAUSE** (code smell is real)
- Claim: retry re-runs in the same workspace; leftover children / re-instrumented target / temp files /
  profraw degrade the retry.
- **Confirmed code fact (CF-1):** the retry resets ONLY `cov/`; see below. So residue genuinely persists.
- **Refuting evidence that residue is the cause:** `free -h`/`df -h` printed before the heavy
  `ktesio-engine` crate in each attempt are FLAT — attempt 1: 14Gi avail / 0B swap / 88G disk;
  attempt 2: 14Gi / 0B / 87G; attempt 3: 14Gi / 0B / 87G. No accumulation. Attempt 3 fared BETTER on
  the interaction cluster (passed) than attempts 1-2 — the OPPOSITE of residue-degradation. Failure
  mode is a wall-clock deadline miss, never a resource-exhaustion error (no ENOMEM/EMFILE/fork-failure/
  OOM-kill). The send_input failures are self-cleaning panics (Engine `Drop` → kill-on-drop fires on
  unwind), so they do not leak `fake_agent` children. → residue is not operative.

### H-C — #109 x86-ubuntu process-spawn fragility, via tarpaulin — **PARTIALLY SUPPORTED (family, not mechanism)**
- Same FRAGILITY FAMILY: all 6 failing tests are real-child-process-IO round-trip tests (spawn
  `fake_agent`, drive a stdin/stdout round trip through the OS) — the same workload class that wedged
  #106/#109 on the 2-core x86 ubuntu runner.
- DIFFERENT MECHANISM: #109 is an uninterruptible D-state DEADLOCK that hangs to timeout-cancel with no
  logs. Here the job completes in ~8 min WITH full logs and the tests fail FAST on their own deadline
  asserts. #109's actual deadlock harness (the adoption survivor tests) is SKIPPED on Linux CI here via
  `is_linux_ci()` (interaction.rs:414-416, 551; mirrored in adoption.rs). So this is NOT #109's
  deadlock — it is the same environmental slowness surfacing as a latency-deadline miss under tarpaulin.

### H-D — instrumentation throughput starvation vs. hard-coded in-test wall-clock deadlines — **CONFIRMED (root cause)**
- The 6 tests each assert a real child-process IO round trip lands within a FIXED wall-clock deadline:
  - `interaction.rs` `wait_for_stdin_line` → **20s** (interaction.rs:117); panic site `interaction.rs:124`
  - `backends/unix/mod.rs` `write_stdin_delivers_a_line_...` → **5s** (mod.rs:1550); panic site mod.rs:1557
  - `backends/unix/mod.rs` `spawn_captures_both_streams_...` → **10s** (mod.rs:1799); panic site mod.rs:1811
- Under `cargo tarpaulin --engine llvm` the whole graph is `-Cinstrument-coverage` compiled (visible in
  the log's rustc invocations: `--cfg=tarpaulin -Cinstrument-coverage`). Every hop of the round trip is
  instrumented → far slower. Direct evidence: the interaction integration binary reports
  `finished in 87.65s` under tarpaulin, vs the code's own note that the full 860-test suite runs "well
  under a minute" uninstrumented (interaction.rs:110-116). ~100x per-binary slowdown.
- Failure messages are uniformly "awaited output never appeared within the deadline," never a wrong
  value, never a hang, never OOM:
  - `never observed "stdin: from-a" in /tmp/.tmp*/agents/a/logs/agent.log` (interaction cluster)
  - `echoed stdin line never appeared` (backend write_stdin)
  - `never observed both streams in both captures; legacy=...` (backend spawn_captures)
- The author already diagnosed this partially and bumped `wait_for_stdin_line` 5s→20s (interaction.rs:
  106-117) "because 4 tests failed deterministically the first time through the coverage job —
  tarpaulin's per-line instrumentation overhead compounds across every hop." But (a) 20s is STILL
  sometimes insufficient, and (b) the two backend unit tests were NEVER bumped (5s / 10s) — which is
  exactly why they are the ones that tip on the attempt where interaction squeaks through.

## Confirmed Findings (graded)

### CF-1 — retry wrapper resets ONLY `cov/`, not process/target/temp/swap state
`ci.yml:576-667`. `run_coverage_attempt()` subshell's first line is `rm -rf cov && mkdir -p cov`
(ci.yml:577). The 3-attempt loop (ci.yml:657-667) re-invokes it with NO other cleanup: no child-process
kill (no `pkill fake_agent`), tarpaulin runs with explicit `--skip-clean` (ci.yml:587) so the
instrumented `target/` is reused, no temp-dir purge, and the disk-reclaim+8GB-swap step (ci.yml:498-517)
runs ONCE before the loop. TRUE as a code fact; but CF-4 shows this residue is not what fails the job.

### CF-2 — instrumented tests run fully serial within an attempt
`ci.yml:552-554` sets `RUST_TEST_THREADS=1` (+ `CARGO_PROFILE_DEV_DEBUG=1`); the 5 crates run
sequentially (ci.yml:582-590). So within an attempt there is no intra-run concurrency to blame — the
slowness is pure per-round-trip instrumentation cost on limited cores.

### CF-3 — the failing pool is a FIXED set of 6 real-child-IO tests, reproduced across 3 fresh runners
All failures in all three jobs come from exactly these 6:
- `interaction.rs`: `send_input_delivers_text_to_a_running_manifest_adapter_agent`,
  `send_input_works_identically_across_two_adapter_registrations`, `send_input_best_effort_still_delivers`,
  `a_stuck_instances_send_times_out_and_does_not_block_a_different_instances_send_beyond_the_bound`
- `backends::unix::tests`: `write_stdin_delivers_a_line_that_is_echoed_into_the_captured_log`,
  `spawn_captures_both_streams_attributed_and_stdout_only_in_the_legacy_log`
Cross-job counts (grep): interaction `6 passed; 4 failed` ×2 and backend `51x passed; 2 failed` ×1 in
EACH of the three jobs. No other test ever fails.

### CF-4 — the failing SUBSET SHIFTS per attempt within a job; resources stay flat (#119-final, decisive)
| attempt | ktesio-engine result | interaction binary | backend lib | Mem avail / swap used / disk free (before ktesio-engine) |
|---|---|---|---|---|
| 1 | FAILED | FAILED (4 tests), 87.65s | passed | 14Gi / 0B / 88G |
| 2 | FAILED | FAILED (4 tests), 87.64s | passed | 14Gi / 0B / 87G |
| 3 | FAILED | **passed** | FAILED (2 tests), 34.79s | 14Gi / 0B / 87G |
The interaction tests that failed twice PASSED on attempt 3; the lighter backend tests (5s/10s
deadlines) tipped instead. This shifting-subset-from-a-fixed-pool + flat-healthy-resources is the
signature of a latency cliff, not a bug, not a leak, not a bad instance.

### CF-5 — no OOM and no 180s tarpaulin cap hit
Grep for `Cannot allocate|out of memory|lost communication|Killed|Segmentation` → empty in the logs.
All "timeout" matches are the `--timeout 180` flag echo and test NAMES (`..._after_a_timeout_...`,
`kill_confirm_timeout...`). Binaries reported clean `test result: ...` lines at 87s / 34s (< 180s), so
no test was killed by tarpaulin's outer per-test cap. (Contrast: the historical AI-23/#101 OOM — "runner
lost communication ~49 min" — was the OLD single `--workspace` pass; that era's problem is SOLVED, the
runner now shows 15Gi total RAM. AI-67 is a DIFFERENT, later problem.)

## Deduced Conclusions

- **DC-1:** The clustering is a deterministic environmental *latency* effect: `tarpaulin`/LLVM
  instrumentation × limited-core serial execution × fixed in-test wall-clock deadlines → the real
  child-process IO round trips intermittently miss their deadlines. Which of the 6 tips over on a given
  attempt is stochastic; the pool and the mechanism are deterministic and reproduce on every fresh
  runner. (From CF-3 + CF-4 + CF-5 + H-D evidence.)
- **DC-2:** The whole-job retry cannot converge. Each attempt independently faces the same cliff on the
  same runner class; a fresh-machine rerun is equally slow. The per-attempt probability that ≥1 of the 6
  misses its deadline is high enough that "3 clean attempts in a row" is rare → chronic red. The retry is
  the wrong tool: it fights a deterministic slowness as if it were random flakiness, and roughly doubles
  wall-clock for nothing.
- **DC-3:** The author's justifying premise ("a DIFFERENT unrelated test each rerun ⇒ runner contention,
  not reproducible ⇒ a retry will pass") is HALF right: the subset does shift (correct) and it is
  environmental not a product bug (correct) — but it is NOT non-reproducible luck; it reproduces on every
  fresh runner, so the retry premise is unsound.

## Source Code Trace

- Deadlines to fix: interaction.rs:117 (20s), mod.rs:1550 (5s), mod.rs:1799 (10s). All three are
  poll-until-deadline-then-`assert!` loops; the assert fires when the deadline passes before the awaited
  child output lands. Raising/scaling the deadline does NOT weaken the assertion (it still proves the
  round trip WORKS) — it only widens the timing budget.
- `cfg!(tarpaulin)` is available under coverage (log shows rustc `--cfg=tarpaulin`), so tests can detect
  the instrumented build directly and scale their deadlines with zero effect on normal/nextest runs.
- Tarpaulin's own `--timeout 180` (ci.yml:587) remains the outer bound, so a GENUINE hang still fails
  loudly even after the in-test deadlines are widened.

## Final Conclusion

**Root cause (confidence HIGH):** H-D — under `cargo tarpaulin` LLVM instrumentation on the free-tier
ubuntu runner, the 6 real-child-process-IO tests' fixed wall-clock deadlines (5s/10s/20s) are too tight
for the ~100x-slower instrumented round trips, so a shifting subset misses its deadline on essentially
every attempt. H-A refuted (3 fresh runners, healthy+flat resources). H-B refuted as cause (flat
resources, attempt-3 improves, latency-not-exhaustion failure mode) — though CF-1's "retry resets only
`cov/`" is a real hygiene smell. H-C is the correct FAMILY (x86-ubuntu process-spawn fragility, #109) but
the wrong MECHANISM (latency-miss, not D-state deadlock; #109's deadlock harness is skipped here).

### Fix direction (for the orchestrator/Islam to action; investigation stops at diagnosis)
1. **Primary, cheapest, permanent:** scale the three in-test deadlines under coverage — e.g. gate on
   `cfg!(tarpaulin)` (or a `KTESIO_TEST_IO_DEADLINE_SECS` env) to use a large budget (e.g. 120s) when
   instrumented, keeping today's values for normal/nextest runs. Must include the two backend unit tests
   (mod.rs:1550 @5s, mod.rs:1799 @10s), which were never bumped. Then REMOVE the 3-attempt retry
   (DC-2). Tarpaulin `--timeout 180` still guards genuine hangs — raise it if a widened deadline needs it.
2. **Alternative:** exclude these ~6 OS-plumbing timing tests from the tarpaulin run (they are already
   fully exercised by the `test` job under nextest on the 3-OS matrix). Costs a coverage-denominator
   reconciliation; messier than #1.
3. **Paid runner (Islam's decision):** a larger/faster-core runner would likely clear even today's
   deadlines with NO code change and could go green immediately — but it treats the symptom (tests remain
   one instrumentation-spike from a flake) and costs indefinitely. Recommend the code fix (#1) first; hold
   the paid runner as a fallback. NB: core count is NOT in the logs (only 15Gi RAM is directly observed,
   contradicting the "7 GB / 2-core" premise the workflow comments still assume) — verify the current
   runner spec before sizing a paid tier.

### Recommended confirming experiment (I am read-only — do NOT run these to "test" the theory; recommend to Islam/orchestrator)
- Cheapest confirmation of H-D: on a scratch branch, widen the three deadlines (or gate on
  `cfg!(tarpaulin)`), DISABLE the retry, and observe the coverage job go green across several runs. Green
  with no retry ⇒ H-D confirmed and the fix proven.
- Independent cross-check: a single paid-runner trial run of the CURRENT code — if it goes green unchanged,
  that corroborates "throughput starvation," and quantifies whether cores alone suffice.

### Per-hypothesis next step
- H-A: none — refuted; do not spend effort on "unlucky instance."
- H-B: no cause-fix needed; IF the retry is kept as belt-and-suspenders, add `pkill -f fake_agent` +
  reset between attempts — but the primary recommendation is to remove the retry (DC-2).
- H-C: none beyond noting the shared fragility family; the #109 deadlock harness is already skipped here.
- H-D: action item #1 above.

**Confidence:** HIGH on the mechanism and the refutations (directly evidenced). MEDIUM only on the exact
remedy sizing (how large a deadline / which paid tier) — resolved by the confirming experiment above.
