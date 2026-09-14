# Agent Notes

Ktesio runs third-party AI agents like services — personal agents such as
Hermes Agent or OpenClaw, and coding agents such as OpenCode or GitHub Copilot
CLI — with runtime controls, config, memory wiring, token limits, and cost caps.
It was repositioned from a skills package manager; the legacy skill-manager
command surface was removed in v0.6.0. Planning runs through the BMAD Method;
its artifacts (`_bmad-output/` — plans, specs, sprint status, retrospectives)
and its toolchain (`_bmad/`) are **tracked in git** and are part of the repo's
open workflow record (the 2026-08-27 reversal in `.gitignore` documents why:
the artifacts are the project's sprint record, and clones need the tooling).
Treat them as real, versioned project files — keep them current in the same
changes that make them stale.

When working here:

- Prefer the public docs in `README.md` and `docs/` for current, shipping
  behavior (the `kt agent` runner). Runner features land story by story;
  document each one in the same change that ships it.
- Before handing off code changes, run:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace --all-targets`
  - `python3 scripts/check_docs.py`
- A bare `cargo` uses the repo's MSRV pin (`rust-toolchain.toml` → Rust 1.96.1)
  while CI's latest-stable jobs run `cargo +stable` — and a version manager
  (mise/asdf) can override the pin via `RUSTUP_TOOLCHAIN`; see the Toolchain
  section of `docs/testing.md`.

## Durable engineering gates

Carried over from the retired project constitution; these hold across the
pivot and are re-ratified as the BMAD PRD/architecture lands:

- **CLI-first** — every feature is reachable via the `kt` CLI; output goes to
  stdout, diagnostics to stderr; all commands support `--help`/`--version`.
- **Test coverage MUST stay ≥ 95%** — enforced in CI via
  `cargo tarpaulin --workspace --fail-under 95`. New code ships with tests.
- **Documentation currency** — update `docs/` and `README.md` in the same
  change as the code they describe; stale docs are treated as a bug.
- **Cross-platform** — Linux, macOS, and Windows; use path-agnostic std APIs.
- **Graceful degradation** — partial failures report a clear reason and a
  remediation, and do not abort the whole operation.

## Engineering patterns

Patterns the code and reviews are held to — new code follows them, and review
enforces them. Surfaced here from the retro record (Epic-11, items AI-18/19/21;
the authoritative wording lives in
`_bmad-output/implementation-artifacts/sprint-status.yaml`).

- **Surfaced, not silent (AI-18).** When an operation degrades, is skipped, or
  only partially applies, that fact must be VISIBLE on the surface the operator
  actually watches — a stderr note, a diagnostic, an honest `—` cell — never a
  silent `Ok`/`None` with the caveat recorded only in a comment. A diagnostic
  that only lives in a comment is a lie waiting to happen. Current exemplars (paths current as of 2026-09; if they move, follow the symbol names):
  the best-effort pause/resume qualifier note `pause`/`resume` emit to stderr
  (`note_if_best_effort` in `crates/kt/src/cli/agent.rs`); orphan-adoption
  honesty in the engine (`ktesio-engine` supervisor + ports record the
  exit-code-unavailable fact instead of pretending a clean read); and the
  Fleet cells' honest `—` absence token (`FleetEntry::METERING_SEED_CELL`)
  where a fabricated `0`/`$0.00` would lie.
- **Deferred-as-unreachable must be proven (AI-19).** A review claim that a
  defect is unreachable — and may therefore ship deferred — must be PROVEN
  across every shipped surface (CLI + library + cross-lifetime) at the same
  evidence bar as a test. Lesson: 1-5 Decision #4 (guaranteed pause with no
  in-memory handle = silent no-op) was reachable through ordinary CLI usage and
  only surfaced in story 1-6.
- **Away-mode / interruption recovery drill (AI-21).** When a session is cut
  off mid-work (subagent session limit, API stall): (1) VERIFY true state from
  disk via the local gates (`cargo fmt/clippy/test`, `check_docs` — they cost
  no model budget), not from memory of what was "about to happen"; (2) resume
  with that recovered context loaded; (3) author large files incrementally in
  small writes, so a cut-off loses minutes, not the artifact.
- **Two-pass review covers the release surface (AI-55).** The two-pass default
  (primary + independent adversarial pass) that Epic 3 scoped to
  money/ledger/enforcement/network code extends to PUBLIC RELEASE-SURFACE
  changes: version bumps, `RELEASE_NOTES.md`/changelog entries, and crates.io
  package metadata (description/keywords). A wrong ledger figure and a wrong
  one-line install command fail the same way — publicly, and hard to retract
  once `cargo publish` has run (the story 9-2 lesson: one proportionate pass
  nearly shipped a mis-described binary).
- **Resume interrupted subagents — never restart them (AI-57).** When a
  subagent run is cut off (usage limit, API error) after it may have written
  artifacts: (1) re-derive true state from the durable record (task tracker,
  ticked spec checkboxes, `git status`/diff, CI status) — never from the
  agent's claimed report; (2) resume the NAMED agent with an explicit
  "here is exactly where you stopped" briefing; (3) restarting from zero is
  the last resort, not the default. (The subagent-level sibling of the
  away-mode drill above: that one recovers the SESSION, this one recovers the
  AGENT's work.) Proven: story 3-2's rate-limit cutoff and story 9-2's review
  pass surviving a ~10-hour session interruption with zero rework.
- **Architecture corrections are draft-then-ratify (AI-58).** An
  architecture-decision correction follows three steps: (1) the architect
  drafts a COMPLETE proposal (all options concrete, trade-offs stated)
  touching ZERO real planning artifacts; (2) the human picks from the bounded
  options; (3) the agent applies the ratified choice verbatim to the real
  artifacts. Never edit the spine first and ask forgiveness after. Provenance:
  the AD-16 / Epic-8 re-scope (story 9-3 Part B, ratified 2026-07-14;
  `epics.md` + issue #95).
