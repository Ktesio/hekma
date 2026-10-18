---
title: 'Skip-note visibility: uncaptured stderr notes for the real-agent smokes (fd-2 direct write)'
type: 'bugfix'
created: '2026-10-18'
status: 'done'
review_loop_iteration: 0
baseline_commit: '31519f74bad018a151d48870a4b8e33a1a01641a'
context: []
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The real-agent smoke tests' skip NOTES (the `HEKMA_TEST_REAL_AGENT_SMOKES=0` opt-out note, the `HEKMA_TEST_REAL_AGENT_TURNS` unset note, and the binary-absent turn-skip notes in `crates/hekma-engine/tests/acp_lifecycle.rs`) are `eprintln!`'d — but libtest CAPTURES the stderr of passing tests, so under a plain `cargo test` run the notes are invisible: the surfaced-not-silent posture (AI-18) only shows under the documented `--nocapture` invocations (the 15-1 review's deferred finding; testing.md currently discloses the limitation).

**Approach:** A tiny test-support helper — `note_uncaptured(msg)` — that writes directly to the process's REAL stderr (file descriptor 2 via a `File` created from the raw fd, which bypasses libtest's thread-local capture sink), used by the smokes' skip paths INSTEAD of `eprintln!`. A plain `cargo test` run then shows every skip note on the terminal; `--nocapture` behavior is unchanged (same bytes, same stream). The helper lives in `hekma-conformance`'s test-support (the smokes' existing shared home) so any future suite can reuse it.

## Boundaries & Constraints

**Always:**
- The helper writes the SAME note text to fd 2 that `eprintln!` would have written (including the trailing newline) — the change is the CHANNEL, never the wording.
- Only the SKIP-NOTE sites switch to the helper (the 6 `eprintln!` note sites in acp_lifecycle.rs: the opt-out notes, the turn-env unset note, the binary-absent turn notes); diagnostics that libtest legitimately shows on FAILURE stay as-is.
- The fd-2 write is best-effort: a write failure is swallowed (a note is never worth failing a test over), mirroring `eprintln!`'s own panics-on-broken-pipe tolerance... NO — `eprintln!` PANICS on a broken pipe; the helper must NOT panic (tests must not fail because a note couldn't be written). Swallow silently on error (the note is the best-effort surface; the test outcome is the truth).
- A unit test pins the helper: writing through it reaches a REAL pipe read end (spawn-style or fd-level), and a write to a closed pipe does not panic.
- testing.md's limitation sentence is updated: the notes are now visible under plain `cargo test` (the `--nocapture` mention stays only where it serves diagnostics).

**Ask First:** Nothing anticipated.

**Never:** No changes to test outcomes, skip conditions, or note wording; no new dependencies; no changes outside the test-support helper + the note call sites + the docs sentence; no capture-bypass for anything except these skip notes (failure diagnostics keep libtest's normal behavior).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Plain `cargo test`, smoke skipped | binary absent / env unset | the skip note APPEARS on the process stderr (terminal) even though the test shows `ok` | N/A |
| `--nocapture` run | same | identical bytes as before (same stream, same wording) | N/A |
| Broken/closed stderr pipe | fd 2 unwritable | the note write fails silently; the test continues and passes | swallowed, never a panic |
| Failing test diagnostics | any test failure | unchanged — libtest shows captured stderr as always | unchanged |

</frozen-after-approval>

## Code Map

- `crates/hekma-engine/tests/acp_lifecycle.rs` — the 6 note sites: `real_smokes_opted_out` callers (~:1052-1057, :1108-1113, :1229-1234 region post-15-1), `turn_smokes_enabled`'s unset note, the binary-absent turn note; the silent binary-absent returns in the register/start/send/stop smokes (:1064, :1117 comment-marked) GAIN notes (today they return silently — bring them to parity with the turn smoke's noted skip).
- `crates/hekma-conformance/src/test_support.rs` (or a new `test_notes` module beside it) — the helper's home; pure `std`, `#[cfg(any(test, feature = "..."))]`-free (the crate is test-infra by design; the existing visibility rules govern).
- `crates/hekma-conformance` unit tests — the helper's pins (pipe round-trip + no-panic-on-closed-pipe).
- `docs/testing.md` — the limitation sentence (search `--nocapture` in the real-agent smokes paragraph) flips to the new posture.

## Tasks & Acceptance

**Execution:**
- [x] `hekma-conformance` — `note_uncaptured(msg: &str)` writing to fd 2 via `std::fs::File::from_raw_fd(2)` (Unix) / the Windows equivalent (`std::os::windows::io` raw handle), best-effort, never panicking; unit pins for both the round-trip and the closed-pipe arm.
- [x] `acp_lifecycle.rs` — the 6 skip-note sites switch to the helper; the silent binary-absent returns in the three register/start/send/stop smokes gain the same noted skip.
- [x] `docs/testing.md` — the visibility sentence updated.
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` — story 16-1 row; `deferred-work.md` — the 15-1 entry's resolved marker at merge.

**Acceptance Criteria:**
- Given a plain `cargo test -p hekma-engine --test acp_lifecycle real_` with a real binary present but the turn env unset, when the run completes, then the turn-skip note is VISIBLE on the terminal while the test reports `ok`.
- Given `--nocapture`, then the output is byte-identical to today's.
- Given a closed stderr pipe, then the helper's write fails without panicking (the pinned arm).
- Gates: fmt / clippy / workspace tests / check_docs green; no test outcome changes anywhere.

## Spec Change Log

- 2026-10-18 (implementation closeout): landed as specced — helper in a new
  `test_notes` module beside `test_support` (re-exported at the crate root),
  the 6 note sites switched, the 3 silent binary-absent skips gained notes,
  testing.md flipped. Real-run evidence on this machine (opencode/hermes-acp/
  gemini all present, turn env unset): plain `cargo test -p hekma-engine
  --test acp_lifecycle real_` shows both TURN-unset notes while the turn
  tests report `ok` (the acceptance grep counts 2 with NO `--nocapture`);
  the `SMOKES=0` run shows all 5 notes under plain and `--nocapture` runs
  BYTE-IDENTICAL (sorted-line diff empty). One environment note, NOT a
  regression: `real_gemini_acp_smoke_register_start_send_stop_when_present`
  fails identically on the baseline tree (this machine's Gemini CLI is
  logged out — the documented -32000 session/new refusal; the I/O matrix's
  failing-test diagnostics arm, unchanged by this story).

## Design Notes

- Why fd-2 direct write: libtest's capture intercepts the `println!`/`eprintln!` macros through a thread-local sink; a `File` constructed from the raw fd bypasses the macros entirely, so the bytes land on the process's real stderr. This is the standard escape hatch (used by several test-helper crates) and needs no nightly features and no dependencies.
- The `ManuallyDrop`/`into_raw_fd` discipline: the File must NOT close fd 2 on drop (the process owns it) — construct, write, forget.

## Verification

**Commands:**
- `cargo test -p hekma-engine --test acp_lifecycle real_ 2>&1 | grep -c "note: real-agent"` — ≥1 note visible with NO `--nocapture` (the AC's live check; requires a real binary present — this machine has opencode/hermes-acp).
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` + fmt + clippy + check_docs — green, outcomes unchanged.
