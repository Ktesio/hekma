---
title: 'Between-commands raw-log backlog: bounded follow prime (ratified surface)'
type: 'feature'
created: '2026-10-18'
status: 'in-progress'
review_loop_iteration: 0
baseline_commit: 'f6dbc02d62ba1ef3e15dbd451796b04d439a5ee8'
context: []
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Output an agent writes to its raw `agent.log` while NO engine session exists — between commands, before a `logs --follow` command starts — is invisible on every surface: the follow loop's raw cursor primes at the CURRENT end (the pre-existing backlog never streams), and one-shot `logs` reads the attributed capture only (spec-15-2 review's deferred finding; the gap is reachable today only through attributed generations a live tailer ever wrote).

**Approach (RATIFIED by Islam, 2026-10-18 — the bounded-follow-backlog option):** `hekma agent logs --follow` primes its raw fallback cursor up to N COMPLETE lines before the raw file's end (N default 200, tunable via the env `HEKMA_LOGS_BACKLOG_LINES`), so the between-commands gap lines stream immediately when the follow starts, carrying the existing un-attributed passthrough qualifier (once per session, unchanged). The attributed-capture read path and one-shot `logs` are UNTOUCHED. Backlog priming applies only when the raw fallback is active at all (the same staleness conditions 15-2 shipped).

## Boundaries & Constraints

**Always:**
- The prime reads at most N COMPLETE lines (never a partial trailing line — the cursor still starts at a line boundary); lines beyond N are skipped (the bounded window), and the skipped-beyond count is named in the qualifier note when non-zero (surfaced, not silent).
- N parsing: a non-numeric or non-positive env value falls back to the default WITH a stderr note (never a crash, never a silent 0/∞).
- No duplication: if the attributed capture is fresh (no raw fallback active), behavior is byte-identical to today — no backlog, no extra note.
- The qualifier text keeps its once-per-session shape; the backlog lines carry the SAME un-attributed framing (they are raw passthrough).
- Env name + default + fallback documented in `docs/commands.md`'s logs section and `docs/troubleshooting.md` if it documents the follow fallback.

**Ask First:** Nothing anticipated.

**Never:** No `--raw` flag (explicitly not chosen); no change to one-shot `logs`; no unbounded replay (the whole-file replay would duplicate attributed history — the ratified rejection rationale); no new config keys on instances (this is a CLI-session env, not fleet config).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Gap lines exist, follow starts | raw grew while no engine lived; ≤N new lines | the gap lines stream at follow start (raw `agent-out` LogLines), qualifier once | N/A |
| Gap larger than N | >N new raw lines | the LAST N stream; the qualifier names the skipped-beyond count | N/A |
| Attributed capture fresh | a live tailer covered the period | byte-identical to today (no backlog, no note) | N/A |
| Env set to garbage | `HEKMA_LOGS_BACKLOG_LINES=abc` / `0` / `-5` | default 200 used + a stderr note naming the bad value | never crashes |
| Raw file smaller than N | few lines total | all complete lines prime | N/A |
| Partial trailing line | file ends mid-line | the partial line is NOT primed (cursor at the boundary; it completes later as usual) | N/A |

</frozen-after-approval>

## Code Map

- `crates/hekma/src/cli/agent.rs` — the follow loop's raw-fallback wiring (the 15-2 `plan_follow`/prime path + `LOGS_FOLLOW_POLL_INTERVAL` region); the prime site computes N (env parse + fallback note) and seeks the raw cursor to the start of the Nth-from-last complete line.
- `crates/hekma-engine/src/domain/supervisor/interaction.rs` — `read_raw_output_since` and the follow/prime helpers (a `prime_backlog_lines(path, n)` helper: read the tail, find the line boundary, return the cursor offset + skipped-beyond count; pure enough to unit-test).
- Tests: engine-side unit pins for `prime_backlog_lines` (N < total, N ≥ total, partial trailing line, empty file) + the env-parse fallback arm; CLI-level e2e: write raw lines with no engine session → start `logs --follow` in a new command → the gap lines appear (the 15-2 raw-fallback e2e shape in `crates/hekma/tests/agent_cli.rs`, extended with a pre-follow gap write).
- `docs/commands.md` — the logs/--follow section gains the backlog sentence (default, env, the bounded window's skipped-beyond honesty).

## Tasks & Acceptance

**Execution:**
- [ ] `interaction.rs` — `prime_backlog_lines` helper + unit pins (boundary math, partial line, under/over N, empty).
- [ ] `cli/agent.rs` — env parse (`HEKMA_LOGS_BACKLOG_LINES`, default 200, garbage→default+note), the prime wiring, the skipped-beyond count in the qualifier.
- [ ] CLI e2e — the pre-follow gap-write scenario (gap streams; >N names the skip).
- [ ] `docs/commands.md` — the backlog paragraph; sprint-status 16-2 row; `deferred-work.md` — the 15-2 entry's resolved marker at merge.

**Acceptance Criteria:**
- Given raw lines written with no engine session, when a later `logs --follow` starts, then those lines (up to N) stream immediately with the qualifier.
- Given more than N gap lines, then exactly the last N stream and the qualifier names the skipped count.
- Given a fresh attributed capture, then behavior and output are byte-identical to today.
- Given a garbage env value, then the default applies with a stderr note (pinned).
- Gates: fmt / clippy / workspace tests / check_docs green.

## Spec Change Log

## Design Notes

- The prime belongs in the CLI session (a display concern), reading via the engine's raw-tail seam — the engine helper stays pure (offset math), keeping it unit-testable without a live follow.
- Why an env and not a flag: the ratified option explicitly avoids new CLI surface; the env tunes pathological cases (huge gaps, tiny terminals) without a deprecation path.

## Verification

**Commands:**
- The CLI e2e (new + extended) green; `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` + fmt + clippy + check_docs green.
