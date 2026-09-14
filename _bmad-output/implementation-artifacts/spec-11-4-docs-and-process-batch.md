---
title: '11-4 Docs & process batch — engineering patterns, contributor notes, CLI diagnostic/list polish'
type: 'chore'
created: '2026-09-11'
status: 'done'
review_loop_iteration: 0
baseline_commit: '0db4c7a44672978835d3eee0ef2f499ea6b8bc12'
context:
  - '{project-root}/_bmad-output/implementation-artifacts/epic-11-context.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Nine retro items (AI-18/19/21/22/25/30/34/43/45) record documentation and small-surface debts: the engine's signature honest-state pattern and the deferred-as-unreachable review rule live only in retro prose, the away-mode recovery drill is tribal knowledge, contributor docs don't explain the MSRV/stable toolchain split, `config get` rejects table prefixes without naming the children, the `config_json` docstring names accessors that don't exist, `cargo doc --document-private-items` fails on `kt`, the human `list`'s Metering-Source split is documented ambiguously, and the Usage column's dollar figure carries an inline estimate label the Budget column already moved to its header.

**Approach:** Add an "Engineering patterns" section to AGENTS.md holding the surfaced-not-silent pattern (with current code exemplars), the AI-19 review rule (adopted ONCE here — 11-7's duplicate is then a no-op), and the away-mode recovery drill; add one-line MSRV/mise pointers to the stale bare-`cargo` spots; make the `config get` table-prefix diagnostic name the child leaves; fix the docstring accessor names and the two rustdoc link errors; clarify the Metering-Source split wording (comment + docs — NOT a new column; the 80-col split is the ratified design); and give the `list` Usage column the Budget column's header-qualified, bare-dollar treatment.

## Boundaries & Constraints

**Always:** AI-18/19/21 land in AGENTS.md (the always-loaded surface). Every rustdoc fix verified by `RUSTDOCFLAGS="-D warnings" cargo doc -p ktesio --no-deps --document-private-items` going green (BOTH known errors: main.rs:115 regex pattern parsed as a link; agent.rs config_get docstring's nonexistent `ResolvedValue` link — fix in the same change). The `config get` table-prefix diagnostic stays stderr + non-zero exit with the honest-state comment preserved. `config set/get` behavior tests updated where output is pinned. Docs updated in the same change: docs/commands.md (config get, list), docs/contributing.md + .github/pull_request_template.md (MSRV pointers). Full battery green.

**Ask First:** Adding a Metering Source column to human `list` (contradicts the ratified 80-col split — the item is the wording fix). Any change to the compact list's other columns.

**Never:** No behavior changes beyond the `config get` diagnostic message text and the `list` Usage cell/header rendering. Do not fix AI-30's retro sub-items (b)/(c)/(d) — dead `source_label` accessor, table misalignment, empty-config json test — they are outside the ratified "fix accessor names" scope; leave flagged. Do not touch the #109 deadlock skips or any other story's tests.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| `config get <name> <table-prefix>` | key has no value but IS a prefix of effective keys (e.g. `budget` when `budget.tokens…` exist) | stderr diagnostic names the child leaves (e.g. "set one of: budget.tokens_per_run, budget.breach_action, …"); exit non-zero | honest rejection preserved |
| `config get <name> <leaf>` | exact key with a value | unchanged stdout behavior | unchanged |
| `config get <name> <unknown>` | key is neither a value nor a prefix | the existing unknown-key diagnostic (suggestion path) | unchanged |
| `cargo doc -p ktesio --document-private-items` with `-D warnings` | current tree | zero unresolved-link errors (main.rs regex + config_get docstring fixed) | gate green |
| `kt agent list` (human, compact) | instance with usage dollars | Usage header carries the estimate qualifier (like Budget's "est. $"); the cell's dollar renders bare (no inline label); `show`/`usage` keep their inline-labeled wide forms | 80-col split intact; Metering split wording clarified |

</frozen-after-approval>

## Code Map

- `AGENTS.md` (39 lines) -- new `## Engineering patterns` H2 after line 39 holds AI-18 (surfaced-not-silent, exemplars: pause best-effort stderr notes `crates/kt/src/cli/agent.rs:1134-1162`; orphan adoption honesty in engine.rs/ports; FleetEntry `METERING_SEED_CELL` honest `—` at agent.rs:699/764/818), AI-19 (deferred-as-unreachable must be PROVEN across every surface before it ships as unreachable — the retro wording is sprint-status.yaml:329-338), AI-21 (the away-mode drill: verify-from-disk via the local gates, resume with recovered context, author incrementally in small writes — sprint-status.yaml:362-371); AI-22 adds ONE pointer line to the gates/bullets (~:21-23)
- `docs/contributing.md` -- §Setup :15-16 and §Development Loop :22-24 get the one-sentence MSRV pointer (authoritative note: docs/testing.md:23-29 — rust-toolchain.toml pins 1.96.1; bare cargo = MSRV, CI `+stable`; RUSTUP_TOOLCHAIN/mise/asdf caveat); §Pull Requests ~:40 optional cross-ref for AI-19
- `.github/pull_request_template.md` -- Verification checklist :7-9 gets the same one-line MSRV pointer
- `crates/kt/src/cli/agent.rs` -- `config_get` ~1530; the no-value branch 1564-1576 builds `AgentUnknownConfigKey` (message at 1570) — extend for the table-prefix case by scanning `effective.iter()` for keys starting with `{key}.`; docstring ~1655-1663 (AI-30: claims `EffectiveConfig::value_display`/`source_label`, body uses `ResolvedValue::display()` ~1691 and `resolved.source.as_str()` ~1695); `list` ~836: column set 884-893, Usage header inline ~890, `usage_cell` ~630-646 (renders `render_dollars(dollars, label)` inline), used at ~911 (narrow `14, 24` widths); the Budget pattern to copy: `BUDGET_LIST_HEADER` ~618 ("Budget (tok, est. $)"), `DollarLabel::InHeader` ~676-681/~728-753, `render_dollars_bare` cap ~749-751; `usage_cell_show` ~650 keeps the inline label; the Metering-split comment block ~868-874 (AI-43 wording)
- `crates/kt/src/main.rs` -- ~115: `/// Fleet-unique instance name (^[a-z0-9][a-z0-9_-]*$)` — the `[a-z0-9_-]` parses as an intra-doc link; wrap the regex in an explicit code span
- `crates/kt/tests/agent_cli.rs` -- config-get suite ~3024/~3090/~3474 (add the table-prefix e2e: exit≠0 + stderr names children); human-list column pins ~1898 and ~2711-2715 (extend for the new Usage header)
- `crates/kt/src/cli/agent.rs` tests -- budget cell exemplar ~3508/~3549 (add the bare-form/InHeader usage_cell pin)
- `docs/commands.md` -- §config get ~314 (new diagnostic), §list (Usage column description + the clarified Metering split)
- AI-34 verification command: `RUSTDOCFLAGS="-D warnings" cargo doc -p ktesio --no-deps --document-private-items` (package id is `ktesio`, NOT `kt`)

## Tasks & Acceptance

**Execution:**
- [ ] `AGENTS.md` -- AI-18/19/21: append the `## Engineering patterns` section (surfaced-not-silent with the three current exemplars + the rule "a diagnostic that only lives in a comment is a lie waiting to happen"; the AI-19 review rule; the away-mode drill) -- the pattern and the drill become tracked, loaded instructions
- [ ] `AGENTS.md` + `docs/contributing.md` + `.github/pull_request_template.md` -- AI-22: one-sentence MSRV/toolchain-split pointers at the three stale bare-`cargo` spots, citing docs/testing.md:23-29 -- contributors stop fighting the wrong toolchain
- [ ] `crates/kt/src/cli/agent.rs` -- AI-25: table-prefix diagnostic names the child leaves (cap the list sensibly, e.g. first N + "…"); keep exit/stderr semantics; comment updated -- operator steered in one step
- [ ] `crates/kt/tests/agent_cli.rs` -- AI-25 e2e: table prefix exits non-zero with children named on stderr; leaf and unknown paths unchanged -- pinned
- [ ] `crates/kt/src/cli/agent.rs` -- AI-30: the ~1655 docstring names the REAL accessors (`ResolvedValue::display()`, the source tag's `as_str()`) -- docstring tells the truth
- [ ] `crates/kt/src/main.rs` + `crates/kt/src/cli/agent.rs` -- AI-34: fix the main.rs:115 regex code-span AND the config_get docstring's `ResolvedValue` intra-doc link error (~1531); verify `RUSTDOCFLAGS="-D warnings" cargo doc -p ktesio --no-deps --document-private-items` exits 0 -- rustdoc gate green
- [ ] `crates/kt/src/cli/agent.rs` -- AI-43: rewrite the ~868-874 split comment + docs/commands.md list section so the Metering-Source split is stated plainly (WHERE the value appears: `show`, `list --json`, `usage`) -- AC-C wording fix, no column
- [ ] `crates/kt/src/cli/agent.rs` -- AI-45: Usage column gets `USAGE_LIST_HEADER`-style qualifier ("Usage (tok, est. $)") beside `BUDGET_LIST_HEADER` ~618; `usage_cell` gains the `DollarLabel` parameterization so `list` renders the dollar bare while `usage_cell_show` keeps the inline label -- parity with the ratified Budget treatment
- [ ] `crates/kt/src/cli/agent.rs` tests + `crates/kt/tests/agent_cli.rs` -- AI-45 pins: the bare-form unit pin beside ~3508; e2e header pins at ~1898/~2711 updated -- rendering locked
- [ ] `docs/commands.md` -- AI-25/43/45 sections updated (config get diagnostic; list columns + metering split) -- doc currency
- [ ] Full battery at story end: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, `python3 scripts/check_docs.py` -- all green

**Acceptance Criteria:**
- Given AGENTS.md, when a new session loads it, then the surfaced-not-silent pattern, the AI-19 review rule, and the away-mode drill are present as tracked instructions with working code references
- Given `kt agent config get <name> budget` where budget.* leaves exist, when it runs, then stderr names the leaves and the exit is non-zero
- Given the rustdoc command with `-D warnings`, when it runs, then it exits 0
- Given `kt agent list` with dollar usage, when rendered, then the Usage header carries the estimate qualifier and the cell shows the bare dollar; `show` is unchanged
- Given the full battery, when run at story end, then all green

## Spec Change Log

- 2026-09-11 — Approval note: approved on autopilot per the standing per-epic workflow; the frozen block restates the Islam-approved sprint change proposal 2026-09-10 §3 Story 11-4 table. Two recorded readings: (1) AI-19 is adopted HERE (11-4 precedes 11-7 in the ratified order) — 11-7's duplicate becomes "verify present, mark done"; (2) AI-43 is the WORDING fix per the proposal's "AC-C literal wording fix" — the 80-col Metering split is the ratified design and stays.

## Verification

**Commands:**
- `cargo fmt --all --check` -- expected: no diffs
- `cargo clippy --workspace --all-targets -- -D warnings` -- expected: zero warnings
- `RUSTDOCFLAGS="-D warnings" cargo doc -p ktesio --no-deps --document-private-items` -- expected: exit 0
- `cargo test --workspace --all-targets` -- expected: all pass
- `python3 scripts/check_docs.py` -- expected: pass

## Suggested Review Order

**AGENTS.md engineering patterns (AI-18/19/21/22)**

- The surfaced-not-silent pattern, the AI-19 review rule, the away-mode drill.
  [`AGENTS.md:41`](../../AGENTS.md#L41)

**CLI diagnostics & list polish (AI-25/45/43)**

- Table-prefix diagnostic: names child leaves, capped, with the depth-2 unit case.
  [`agent.rs:1601`](../../crates/kt/src/cli/agent.rs#L1601)
- The DollarLabel-parameterized usage_cell + the min-width header guarantee.
  [`agent.rs:648`](../../crates/kt/src/cli/agent.rs#L648)
- The call-site e2e: Usage cell bare, header qualified.
  [`agent_cli.rs:2711`](../../crates/kt/tests/agent_cli.rs#L2711)

**Rustdoc truthfulness (AI-30/34)**

- Real accessor names in the config_json/config_get docstrings; the regex code-span.
  [`agent.rs:1655`](../../crates/kt/src/cli/agent.rs#L1655)
