---
title: '11-7 Orchestration & review process batch — pins, patterns, tracker closure'
type: 'chore'
created: '2026-09-11'
status: 'done'
review_loop_iteration: 0
baseline_commit: 'ac0ce32e1f54ed12495768b2223bcbe6a0438c87'
context:
  - '{project-root}/_bmad-output/implementation-artifacts/epic-11-context.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Six retro items (AI-17/19/55/56/57/58 + epic-6-retro-item-9) record process debts: test_automation's semver pins are file-wide substrings that could pass via the wrong site, the two-pass review default doesn't cover release-surface changes, three proven working patterns (resumable-subagent recovery, draft-then-ratify architect decisions) were never written down, a stale sprint-status sync target no longer exists, and the four #168-ratified carry-forward verdicts were never mirrored into the tracker.

**Approach:** Scope the semver pins to the semver job block using the file's existing slice idiom and pin the lazy-install branch shape; add AI-55/57/58 as Engineering-patterns bullets in AGENTS.md; mark AI-19 done (adopted by 11-4); mark AI-56 satisfied-by-events with the full record; mirror the four #168 verdicts into AI-63/65/66/69's entries (correcting AI-63's stale part-(b) tail) and close the carry-forward item.

## Boundaries & Constraints

**Always:** test_automation.py stays green after the pin scoping (`python3 scripts/test_automation.py`); the slice idiom used is the file's own existing technique; every sprint-status edit preserves ALL comments and structure; AI-63's corrected entry records the #168 retirement verdict verbatim in substance (accepted-tradeoff, per-op deadlines, reopen condition).

**Ask First:** Any disposition of the four carry-forward items OTHER than the #168-ratified verdicts (they are already ratified — do not relitigate).

**Never:** No changes to ci.yml in this story (the pins describe it; the workflow itself is 11-5's). No new playbook files — AGENTS.md Engineering patterns is the home (11-4 precedent).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Semver gate pin integrity | a baseline-pinned check-release invocation is dropped from ci.yml's semver job | test_automation FAILS (the pin is scoped to the job block, so another job's site cannot satisfy it) | caught in CI docs job |
| Lazy-install placement drift | the `command -v` guard or the version-validated install line moves/disappears | the scoped pin fails | same |
| Tracker closure | AI-63/65/66/69 + carry-forward entries | each records its #168 verdict (2026-09-09); no stale part-(b) framing remains on AI-63 | tracker matches the ratified record |

</frozen-after-approval>

## Code Map

- `scripts/test_automation.py` -- the weak pins ~:435–437 (file-wide assertIn for the install line, check-release, transient-skip regex); the scoped-slice idiom to copy: coverage step :166–183, semver job block :502–510, build :538–544, perf :581–587; other loose pins noted (targets :56, release strings :59–64) — IN SCOPE only for the semver block per the ratified item
- `ci.yml` (read-only this story) -- semver job lazy-install ~:589–615 (`command -v` guard, version-validated install), three check-release sites :654/:666/:674
- `AGENTS.md` -- `## Engineering patterns` :45–74 (11-4's section; AI-55/57/58 join it)
- `docs/release-process.md` -- AI-55 one-line anchor point
- `_bmad-output/implementation-artifacts/sprint-status.yaml` -- AI-19 :329–338 (flip done → AGENTS.md pointer); AI-55 :806–819; AI-56 :820–830 (satisfied-by-events: key deleted in ff1f669, epic-8 closed superseded 2026-09-09, epics.md:700–732 + issue #95 authoritative); AI-57 :831–845; AI-58 :846–858; AI-63 :975–985 (stale part-(b) tail), AI-65 :1038, AI-66 :1062, AI-69 :1158 (all done, no #168 note); carry-forward :1302–1311
- `_bmad-output/planning-artifacts/epics.md` -- the ratified 8-1 rewrite :700–732 (the authoritative wording AI-56 points to)

## Tasks & Acceptance

**Execution:**
- [x] `scripts/test_automation.py` -- AI-17: scope the three semver pins (install line, check-release, transient-skip regex) to the semver job block slice; assert the lazy-install branch shape (the `command -v` guard precedes the version-validated install) -- a dropped gate can no longer pass via another job's site
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- AI-19: flip to done citing AGENTS.md's Engineering patterns bullet (adopted by 11-4) -- verify-only for this story
- [x] `AGENTS.md` -- AI-55: the two-pass review default extends to public release-surface changes (version bumps, RELEASE_NOTES/changelog entries, crates.io metadata); one-line anchor in docs/release-process.md -- the asymmetry lesson is now policy
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- AI-56: mark done/obsolete recording the satisfied-by-events chain (key removed ff1f669; epic-8 closed superseded 2026-09-09; epics.md:700-732 + #95 authoritative) -- nothing left to sync
- [x] `AGENTS.md` -- AI-57: the resumable-subagent recovery pattern (re-derive from durable artifacts; resume the named agent with an explicit where-you-stopped briefing; never restart from zero) -- the twice-proven flow is written down
- [x] `AGENTS.md` -- AI-58: the draft-then-ratify architect pattern (complete proposal touching zero real artifacts → human picks from bounded options → apply the ratified choice verbatim) -- the AD-16/Epic-8 lesson is policy
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- epic-6-retro-9: mirror the four #168 verdicts (2026-09-09, "ratify all four as recommended") into AI-63 (RETIRED AS ACCEPTED TRADEOFF — per-op deadlines cap every lock-held stall; reopen on an observed multi-instance stall) / AI-65 (ADOPTED AS STANDING REVIEW RULE — value vs mechanism) / AI-66 (RETIRED AS SUBSUMED — the five structural enforcement patterns) / AI-69 (RATIFIED: the mapper pin is the permanent gate; live-D-state e2e a documented limitation), correcting AI-63's stale part-(b) tail; flip the carry-forward entry done -- the tracker matches the ratified record
- [ ] Gates: `python3 scripts/test_automation.py` green; `python3 scripts/check_docs.py` green; fmt/clippy/tests unchanged-green -- battery holds

**Acceptance Criteria:**
- Given ci.yml's semver job loses its baseline-pinned check-release step, when test_automation runs, then it fails naming the scoped pin (simulated by inspection of the slice boundaries; no ci.yml edit needed to prove the scoping)
- Given the tracker, when a reader opens AI-63/65/66/69 or the carry-forward entry, then each records its #168-ratified disposition and none carries stale open framing
- Given AGENTS.md, when loaded, then AI-55/57/58 are present as Engineering patterns
- Given the gates, when run, then test_automation + check_docs green with the battery unchanged

## Spec Change Log

- 2026-09-11 — Approval note: approved on autopilot per the standing per-epic workflow; the frozen block restates the Islam-approved sprint change proposal §Story 11-7 with the investigation's ratified-wording findings (AI-56 satisfied-by-events; epic-6-retro-9 verdicts pre-ratified on #168 — recorded, not re-decided). AI-19 is verify-only (11-4 adopted it).

## Verification

**Commands:**
- `python3 scripts/test_automation.py` -- expected: all green with the new scoped pins
- `python3 scripts/check_docs.py` -- expected: pass
- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --all-targets` -- expected: unchanged-green

## Suggested Review Order

**The pin scoping (AI-17)**

- The scoped assertions: exact count, ordering, line-exact forms.
  [`test_automation.py:521`](../../scripts/test_automation.py#L521)

**The three new patterns (AI-55/57/58)**

- AGENTS.md Engineering patterns additions + the release-process anchor.
  [`AGENTS.md:75`](../../AGENTS.md#L75)

**Tracker closures (AI-19/56, epic-6-retro-9)**

- The #168 verdict mirrors and the satisfied-by-events AI-56 record.
  [`sprint-status.yaml:975`](../../_bmad-output/implementation-artifacts/sprint-status.yaml#L975)
