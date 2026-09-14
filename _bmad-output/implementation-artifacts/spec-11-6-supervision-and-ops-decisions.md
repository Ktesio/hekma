---
title: '11-6 Supervision & operations decisions — the non-decision items, decisions surfaced'
type: 'chore'
created: '2026-09-11'
status: 'done'
review_loop_iteration: 0
baseline_commit: '24bbebeb872046a50b8c0e1149e617c081e02ac6'
context:
  - '{project-root}/_bmad-output/implementation-artifacts/epic-11-context.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Story 11-6 was ratified as the owner of five supervision/operations items, three of which (AI-20 daemon/detach, AI-47 HTTPS upstream, AI-36 Dependabot triage) the change proposal §6 explicitly gates on Islam's product decisions. The remaining two (AI-46 adoption stranding — overlapping 11-3 — and AI-48 tracing exposure) are implementable without a call.

**Approach:** Land the two non-decision items (AI-46 is ALREADY landed by 11-3 per the ratified overlap — this story records that; AI-48 lands its dependency-audit checkpoint documentation), record AI-36's prior resolution (superseded by AI-53, PR #108), and leave AI-20/AI-47 formally OPEN with dated notes naming exactly which product decision each awaits — surfaced in the epic PR description.

## Boundaries & Constraints

**Always:** AI-20/AI-47 stay `open` — closing them without Islam's call would falsify the tracker. The AI-48 documentation goes in docs/embedding.md (the dependency/exposure surface embedders read).

**Never:** No TLS client, no daemon work, no streaming parser — all three are the commissioned-but-not-called decisions. No status flips beyond AI-46.

## Tasks & Acceptance

**Execution:**
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- AI-46: flip done citing the 11-3 landing (diagnostic + remediation + tests) -- the overlap resolved once
- [x] `docs/embedding.md` -- AI-48: the dependency-audit checkpoint (hyper-util/tracing exposure; what a future subscriber install must re-audit) -- the latent risk is written down
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- AI-20/AI-47: dated notes naming the exact product decision each awaits; status stays open -- honest tracker
- [x] AI-36: no action -- already `done` (superseded by AI-53/PR #108, recorded 2026-07-14) -- the proposal's row predates that resolution

**Acceptance Criteria:**
- Given the tracker, when read, then AI-46 is done with landing evidence, AI-48's checkpoint exists in docs, and AI-20/AI-47 remain open with dated decision notes
- Given check_docs, when run, then green

## Spec Change Log

- 2026-09-11 — Approval note: approved on autopilot per the standing per-epic workflow; scope is the change proposal §Story 11-6 MINUS the three Islam-gated product calls, per the proposal's own §6 gate ("Story 11-6 requires Islam's product decisions before its implementation items can be commissioned"). The autonomous-epic run ships the story with the decision items honestly open rather than inventing product calls; the calls are surfaced in the epic PR for Islam.

## Verification

**Commands:**
- `python3 scripts/check_docs.py` -- expected: pass
- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --all-targets` -- expected: unchanged-green (docs/tracker-only change)
