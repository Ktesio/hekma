---
title: 'ACP 100%: real-agent verification for OpenCode + Hermes under the acp kind'
type: 'feature'
created: '2026-10-16'
status: 'in-progress'
baseline_commit: '9f58225a15a6d093fd7213db0a8c2f7b9fe34522'
review_loop_iteration: 0
context:
  - '{project-root}/docs/agents.md'
  - '{project-root}/_bmad-output/implementation-artifacts/sprint-status.yaml'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The builtin `acp` kind (epic-14) is proven against the contract twin
(`fake_acp_agent`) and one real agent (`hermes-acp`, run here: the existing smoke
passes), but opencode — which ships a native ACP server since v2
(`opencode acp`, verified live: initialize → session/new → full prompt turn in ~4s
against a free model) — has NO real-agent proof and is still documented as
"paper-validated, manifest-adapter only". "ACP works 100% and fully supports
OpenCode & Hermes" is therefore not yet an evidenced claim.

**Approach:** Add a real `opencode acp` smoke (skip-unless-present, same posture
as the hermes/gemini smokes) plus opt-in turn-COMPLETION smokes for both
opencode and hermes-acp that drive a full prompt turn through the engine library
and assert the reply text is retained; fix any engine defects the real agents
expose; update `docs/agents.md` (opencode: ACP-native row, run-verified pin
v2.0.28; hermes-acp: real-run evidence) and the CHANGELOG/RELEASE_NOTES.

## Boundaries & Constraints

**Always:**
- Real-agent smokes keep the house skip-unless-present posture (`probe_binary_present`)
  and the `HEKMA_TEST_REAL_AGENT_SMOKES=0` visible opt-out; CI (no binaries) stays
  deterministic and network-free.
- Turn-completion smokes are OPT-IN via a new env (default off): a real model call
  can cost money or hang on a logged-out agent — the gemini -32000 lesson
  generalized. Opt-out note style: surfaced on stderr, never silent.
- The engine stays agent-agnostic: no opencode- or hermes-specific branches in
  engine code; agent-specific knowledge lives in docs + smoke tests only.
- Billing honesty untouched: `usage_update` stays context-grain; no ledger rows
  from smokes; `acp_context_usage` may surface.
- Every doc claim carries its evidence (version pin + what was run).

**Ask First:** Any engine-behavior CHANGE beyond defect repair that a real agent
forces (protocol dialect accommodation beyond tolerant parsing counts as repair
only if a pinned test proves the old behavior was a defect).

**Never:** No vendored/binaries-in-repo; no network in CI tests; no new metering
tier; no changes to the deprecated `hermes` builtin kind; no gemini work beyond
keeping its existing smoke compiling (logged-out env failure is documented
behavior).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| opencode smoke, binary present | register acp + `acp.command=opencode`, `acp.args=acp` → start → send → stop | start reaches `running` (handshake + session/new ok), send accepted, stop lands `stopped` | launch failure = red test (honest verdict) |
| opencode absent | no `opencode` on PATH | smoke skips silently (standard posture) | N/A |
| Turn-completion, opt-in on | `HEKMA_TEST_REAL_AGENT_TURNS=1` + binary present | full `session/prompt` turn through the engine; the agent's reply text appears in the retained log; clean stop | turn not finished within bound → red with diagnostic; auth failure → red naming the env lever |
| Turn-completion, opt-in off | env unset | visible stderr skip note, no model call | N/A |
| opencode dialect extras | `authMethods` in initialize, `configOptions` in session/new, `available_commands_update`, `usage_update.cost`, thought chunks | tolerated: classified or skipped-with-notice per updates.rs; turn proceeds | unknown update kinds surface as `UnhandledUpdate` notice, stream continues |

</frozen-after-approval>

## Code Map

- `crates/hekma-engine/tests/acp_lifecycle.rs` -- home of all acp tests; smokes at
  ~L987-1169 (`probe_binary_present`, `real_smokes_opted_out`,
  `real_hermes_acp_smoke...`, `real_gemini_acp_smoke...`); `register_acp` helper;
  session-id helpers from 14-2 below. Add the opencode + turn smokes here.
- `crates/hekma-engine/src/acp/updates.rs` -- session/update classifier; already
  knows `agent_message_chunk`/`agent_thought_chunk`/`usage_update`(used,size,cost)/
  `available_commands_update`; unknown → `Unhandled` (surfaced).
- `crates/hekma-engine/src/acp/connection.rs` -- inbound dispatch (~L770-830):
  permission requests auto-denied; UsageUpdate → context_usage record + notice;
  turn-content updates deliberately no-op (raw line already appended to agent.log,
  AD-12 tailer attributes into output.log).
- `crates/hekma-engine/src/acp/client.rs` -- handshake spine: initialize →
  (session/load | session/new); counter/version rules; timeout budget.
- `docs/agents.md` -- validation table + per-agent sections; opencode section
  currently manifest/paper-only; hermes-acp named under `--kind acp` without a
  real-run pin.
- `CHANGELOG.md`, `RELEASE_NOTES.md` -- user-facing "ACP supports opencode" note.
- `_bmad-output/implementation-artifacts/sprint-status.yaml` -- add story rows
  under a new epic-15 section at implementation time (BMAD numbering).

## Tasks & Acceptance

**Execution:**
- [x] `crates/hekma-engine/tests/acp_lifecycle.rs` -- add `real_opencode_acp_smoke_register_start_send_stop_when_present` (mirror the hermes smoke; command `opencode`, args `acp`) -- the missing real-agent proof.
- [x] `crates/hekma-engine/tests/acp_lifecycle.rs` -- add `HEKMA_TEST_REAL_AGENT_TURNS` opt-in turn-completion smokes for opencode AND hermes-acp: start, send a deterministic prompt, bounded-wait the turn end (poll retained log for the sentinel reply via existing log-read seam or the connection's terminal state), assert reply retained, stop clean -- "100%" means a completed turn, not just an accepted send.
- [x] `crates/hekma-engine/tests/acp_lifecycle.rs` -- unit-pin (fake_acp_agent script) that initialize responses carrying `authMethods` and session/new carrying `configOptions` do not fail the handshake -- opencode's real dialect, contract-twin proven.
- [x] `docs/agents.md` -- opencode: new ACP-native row (Run-verified, pin `v2.0.28` @ this machine, evidence = the smokes), `--kind acp` instructions (`acp.command=opencode`, `acp.args=acp`), manifest path demoted to alternative; hermes-acp section gains the real-run pin; gemini smoke env caveat documented.
- [x] `CHANGELOG.md` + `RELEASE_NOTES.md` -- ACP: opencode supported and verified end-to-end (two-pass review per AI-55: release-surface change).
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- epic-15 story rows + this spec tracked.

**Acceptance Criteria:**
- Given opencode v2.0.28 on PATH, when running `cargo test -p hekma-engine --test acp_lifecycle real_`, then the opencode and hermes smokes pass and gemini's failure (if logged out) is its documented env-shaped verdict.
- Given `HEKMA_TEST_REAL_AGENT_TURNS=1`, when both turn smokes run, then each completes a full prompt turn through the engine with the reply retained in the log and a clean stop, within the bounded window.
- Given the turn env unset, when the suite runs, then the turn smokes visibly skip and no model call is made.
- Given the full local battery (fmt/clippy/test workspace, check_docs) with `HEKMA_TEST_REAL_AGENT_SMOKES=0`, then everything is green (CI-equivalent posture).
- Docs: every validation claim in `docs/agents.md` names its pin and evidence; no claim without a run behind it.

## Spec Change Log

## Design Notes

- Turn-end detection: prefer a poll on the retained log surface (public read seam
  used by `agent logs`) for the sentinel reply string over any new engine API —
  keeps the smoke at the operator's eye level. Bounded window (≤120s) with a
  red-on-timeout diagnostic naming the opt-in env.
- The reply sentinel must be forced-lowercase-unique (e.g. `hekma-acp-turn-ok-<pid>`);
  thought chunks may echo it — assert on an agent_message (text content) shape or
  the raw line's content field, tolerant of chunking.
- Turn smokes run `--test-threads=1`-safe: independent state roots (existing pattern).
- Why opt-in default-off for turns (not run-if-present): a present-but-unconfigured
  agent (no auth) makes turn completion an AUTH test, not an engine test — the
  exact gemini failure mode the opt-out env was invented for.

## Verification

**Commands:**
- `HEKMA_TEST_REAL_AGENT_SMOKES=1 cargo test -p hekma-engine --test acp_lifecycle real_ -- --nocapture --test-threads=1` -- opencode + hermes smokes green.
- `HEKMA_TEST_REAL_AGENT_TURNS=1 HEKMA_TEST_REAL_AGENT_SMOKES=1 cargo test -p hekma-engine --test acp_lifecycle turn -- --nocapture --test-threads=1` -- both turn smokes green.
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace` (background, gates) + `cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `python3 scripts/check_docs.py` -- all green.
