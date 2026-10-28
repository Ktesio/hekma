---
title: 'Agents in CI — the GitHub Actions recipe and dogfood lane'
type: 'feature'
created: '2026-10-24'
status: 'done'
review_loop_iteration: 0
baseline_commit: '42fe5c50e807a5ab631c525dc8ce6d0ce65f3e0b'
context:
  - '_bmad-output/implementation-artifacts/epic-20-context.md'
  - 'docs/commands.md'
  - 'docs/hosting.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Teams letting review bots loose in CI have no recipe for bounding what an agent can burn — FR-47 (Epic 20) calls for a documented GitHub Actions recipe (budget-capped agent, wall-clock bounded, usage into `$GITHUB_STEP_SUMMARY`, exit-code honesty) exercised by a dogfood lane in this repository's own CI, not merely documented.

**Approach:** A docs page (`docs/agents-in-ci.md`) carrying the user-facing recipe — tag-pinned static-musl curl+sha256 download (the hosting.md pattern), agent registration with a token budget + breach action, wall-clock bounds, a step-summary renderer from `--json`, and how a workflow branches on the stable exit codes — plus a new dedicated ubuntu CI job that runs the recipe's mechanics end-to-end against the bundled `fake_agent` deterministically (no external agent, service, or secret).

## Boundaries & Constraints

**Always:**
- The lane is deterministic: the bundled `fake_agent` with `--emit-usage` count-bounded sentinels and a deliberately tiny ceiling (the proven 90-token/event-3 recipe, uj3.rs:100-186), polling for the KNOWN committed state — never wall-clock sleeps.
- The lane uses the BRANCH-BUILT `hekma` binary (its own commit under test, no egress); the PAGE documents the tag-pinned musl release download for users. Both paths shown honestly.
- The step summary renders from `agent usage --json` + `agent show --json`: tokens by kind, micro-dollar totals with `estimate_label`, cached tokens omitted when absent (never a fabricated 0 — AI-18), dollars omitted when no rate is set.
- Breach honesty: no dedicated breach exit code exists (codes 0-6 only) — the lane proves the breach by the paused Fleet row + budget fields on `show --json` and names the breach in the summary; no path masks a breach as success.
- Both lane legs are wall-clock bounded: job `timeout-minutes` explicit, and the lane has a success leg (higher ceiling, exit 0, summary written) AND a breach leg (tiny ceiling, paused, breach named).
- Freshness guard: `rm -f` the built helpers before rebuilding (the stale-helper trap, ci.yml:204-222 convention).
- Docs page registers in `docs/meta.json` + `docs/source.config.ts`; bash fences must pass check_docs' command allowlists; no planning provenance.

**Ask First:** (standing instruction 2026-10-24: collect to end — pre-decided, listed in the Spec Change Log) lane = branch-built binary; recipe page = tag-pinned curl+sha256; breach proof via Fleet row not exit code; no installer pin/musl knob in this story (deferred-work entry instead); `HEKMA_TEST_REAL_AGENT_SMOKES` not needed (the lane runs the CLI directly, no cargo test suite involved).

**Never:** No engine/CLI code changes; no secrets in CI; no real-agent or live-web fetches in the lane (docs-probe separation); no composite Action (deferred); no installer-script contract changes; no new exit codes.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Success leg | fake agent, ceiling above its fixed usage | exit 0; summary lists tokens + labeled dollars | N/A |
| Breach leg | ceiling 90 tokens, sentinels 10in/20out | paused at event 3; summary names the breach + action; lane asserts paused row + budget fields | if not paused within the bound: lane fails naming the poll tail |
| No rate configured | usage without cost.rate set | dollar fields omitted from summary (not zero) | N/A |
| Wall-clock bound hit | lane over `timeout-minutes` | job fails at the bound; the page documents `timeout-minutes` as the user-visible bound | generic cancel, not a hang |

</frozen-after-approval>

## Code Map

- `.github/workflows/ci.yml` — workflow conventions: SHA-pinned actions with version comments, `permissions: contents: read`, explicit `timeout-minutes` per job, the docs job :607-631 and perf-budgets :483-535 as exemplars; rm+build helper freshness guard :204-222; step-summary precedent at :1632-1642 (`>> "${GITHUB_STEP_SUMMARY:-/dev/null}"`)
- Budget/usage surfaces (all in `docs/commands.md`): exit codes :543-574 (0-6; NO breach code); `agent usage --json` shape :134-192 (schema_version 2, micro-dollars, `estimate_label`, optional cached fields skip-if-none, dollars omitted without a rate); `agent config set` budget keys :519-531 (`budget.tokens.cumulative`, `budget.breach_action` pause/stop/warn, `cost.rate.*`); re-read-on-ingest :413
- Deterministic breach recipe — `crates/hekma-conformance/src/uj3.rs:100-186` (ceiling 90, sentinels 10/20 → breach at event 3) and `crates/hekma/tests/agent_cli.rs:3392-3470` (the CLI-level poll-to-paused pattern); `fake_agent` usage flags `crates/hekma-conformance/src/bin/fake_agent.rs:173-201` (`--emit-usage N`, fixed sentinels, overrides), crash/exit flags :41-61
- `docs/hosting.md:209-230` — the tag-pinned curl+sha256 musl download pattern the page re-uses verbatim
- `scripts/check_docs.py` — bash-fence command allowlists (:133-162) the page's fences must satisfy; planning-provenance ban
- `_bmad-output/implementation-artifacts/sprint-status.yaml` — tick 20-2 on PR open

## Tasks & Acceptance

**Execution:**
- [x] `docs/agents-in-ci.md` — NEW: the recipe — (1) get hekma on the runner (tag-pinned musl curl+sha256, checksum-verified); (2) register the agent (inline `adapter.toml` for any process, or `--kind acp`), set `budget.tokens.cumulative` + `budget.breach_action` + optional `cost.rate.*`; (3) run bounded (`timeout-minutes` + hekma's bounded commands); (4) render `$GITHUB_STEP_SUMMARY` from `usage --json`/`show --json` (shell snippet handling micro-dollars, estimate label, absent-cached/absent-dollars honesty); (5) branch on exit codes 0/2/3/4/5/6 with the no-breach-code honesty note (spend checks read usage totals, breaches read the Fleet row); (6) the secret: wiring note for auth-gated agents (`secret:NAME` refs, never literals in YAML)
- [x] `.github/workflows/ci.yml` — NEW top-level job `agents-in-ci` (ubuntu, `timeout-minutes: 10`, contents: read inherited): checkout → build branch `hekma` + `fake_agent` (rm+build guard) → success leg (inline manifest, absolute fake path, generous ceiling, exit 0, summary written, tokens asserted) → breach leg (uj3 recipe: ceiling 90 → poll `show --json` for paused + budget fields within the bound → summary names breach; `continue-on-error: false`) — both legs write `## Agents in CI` step-summary blocks with the `:-/dev/null` fallback
- [x] `docs/meta.json` + `docs/source.config.ts` — register `agents-in-ci` page
- [x] `README.md` — one-line pointer to the recipe (beside the gallery pointer if 20.3 lands first)
- [x] `_bmad-output/implementation-artifacts/deferred-work.md` — entry: installer pin/target knob (`install.sh` resolves latest-release gnu only; the CI recipe needs direct curl+sha256 until it grows a `HEKMA_VERSION`/musl knob) — trigger: a user asks for pinned installs
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` — tick 20-2 per house convention on PR open

**Acceptance Criteria:**
- Given the lane on a PR, both legs pass deterministically (no wall-clock sleeps; the breach leg proves the paused row + breach fields from `show --json`).
- Given the rendered step summary, tokens/dollars appear with `estimate_label`; cached/dollars absent fields stay absent (never 0).
- Given `python3 scripts/check_docs.py` + `npm run --prefix docs build`, both pass with the new page.
- Given a reader copying the page into their workflow, every command exists on the frozen CLI surface (allowlist-verified fences) and the musl download is checksum-verified.

## Spec Change Log

- 2026-10-24 (pre-approval, standing autonomous instruction): Checkpoint-1 gates pre-decided: (1) the DOGFOOD lane builds the branch binary (tests this commit, no egress) while the PAGE documents the released musl artifact — resolves the network-free vs released-artifact tension honestly; (2) breach has no exit code — proof = paused Fleet row + budget fields via `show --json` (the uj3/agent_cli precedent), spend checks read usage totals; (3) no installer pin/musl knob — out of scope, deferred-work entry instead (installer contract is a release surface); (4) `HEKMA_TEST_REAL_AGENT_SMOKES` untouched (lane runs the CLI, not the cargo suite); (5) page named `agents-in-ci.md` (verb-first, matches command-reference style); spec kept under the token guideline.
- 2026-10-24 (step-04 review, loop 0 — patches only, no loopback): 3 reviewers (blind-hunter, edge-case-hunter, verification-gap) at max effort; verification-gap returned ZERO behavioral gaps (the lane's assertions traced to pinned contracts at every boundary) and proved two review findings were artifacts of the diff snapshot, not the tree. Patches applied: timeout 10→15 (cold-build headroom, build-job precedent); ACP detach caveat on the page (§2 + §5 exit-5 note — `--detach` is refused for `acp`, attached-in-step is the CI shape); state-dir isolation taught (§3, rerunnability on self-hosted runners); label fallback `// "estimated"` → `// empty` with the pair-is-pinned note (never invent a label); §5 example switched to `--detach` (exit 0 = started-detached, semantics now true); poll guards (`jq || continue`) and the `remaining = 0` breach conjunction in the taught loop (an operator pause is not a breach); lane jq reads guarded with blob-dump failures; success leg now asserts the budget block, the exact 1650-µ$-estimated math (AI-55: money surfaces get proven), and the post-stop `stopped` state; breach leg now ASSERTS the dollar-fields absence before rendering the honest `—` cell; renderer aligned to the docs §4 snippet with the claim reworded truthfully; fixture comment scopes its cross-OS claim to the conformance kit; `KTESIO_NO_UPDATE_CHECK` marked belt-and-braces (CI=true already skips). Both legs re-rehearsed locally after every patch: GREEN (success: `$0.001650 (estimated)`; breach: paused 90/0/pause/150 + asserted absence). Deferred: none new. Rejected: the doubled-fixture-path and garbled-link findings (diff-generation artifacts — the tree was verified correct); the "no BMAD/Fibery artifacts" finding (diff scope; sprint-status is in the commit and Fibery mirrors at PR open).

## Design Notes

- Lane legs share one state dir per leg (fresh `HEKMA_STATE_DIR` under `$RUNNER_TEMP`), run sequentially in one job; the manifest is authored inline (heredoc-free — write via a step `run:` block using printf or a checked-in fixture under `.github/fixtures/`; prefer the fixture for reviewability).
- The poll loop: bounded retries (e.g. 60 × 1s) around `hekma agent show <name> --json` grepping the state + breach fields — fail with the last JSON blob printed, never a bare timeout.
- Micro-dollars rendering: `awk`/`jq`-free arithmetic is fine in the summary snippet (runners have jq; use jq on the page, but the lane may use it too — ubuntu-latest ships jq).

## Verification

**Commands:**
- `python3 scripts/check_docs.py && python3 scripts/check_docs.py --self-test` — green with the new page + fences
- `npm run --prefix docs build` — site builds with the page registered
- Local lane rehearsal (best-effort, Linux): run the two legs' scripts against a locally built `hekma` + `fake_agent` with `GITHUB_STEP_SUMMARY=$PWD/summary.md` — expected: exit 0 on both legs with the summary file populated (the workflow itself is CI-verified on the PR)
- `act`-style dry-run is NOT required; the PR's CI run is the lane's own proof
