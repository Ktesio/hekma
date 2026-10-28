---
title: 'Adapter gallery — ten seed entries, honesty tables, CI-validated manifests'
type: 'feature'
created: '2026-10-24'
status: 'done'
review_loop_iteration: 0
baseline_commit: '42fe5c50e807a5ab631c525dc8ce6d0ce65f3e0b'
context:
  - '_bmad-output/implementation-artifacts/epic-20-context.md'
  - 'docs/agents.md'
  - 'docs/adapter-contract.md'
  - 'docs/manifest.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** A newcomer has no validated index of which agents run under Hekma today — docs/agents.md covers four agents, carries no per-capability honesty tables, and nine of the ten seed agents have no entry at all. FR-48 (Epic 20) calls for a curated adapter gallery: each seed agent a docs page with an honesty table in the Capability Declaration vocabulary, plus CI validation of every shipped manifest by the conformance kit.

**Approach:** A gallery index + ten per-agent pages under `docs/adapters/` (registered in the docs site and scanned by check_docs), honesty tables using the exact contract vocabulary (`guaranteed`/`best-effort`/`unsupported`, metering `self-reported`/`engine-observed`, the honest `—` + tiers-attempted notice where no tier yields), validated-against pins and the re-validation duty per entry. Manifest entries (Aider is the only seed that is manifest-based; the other nine are `--kind acp` registrations) ship as `adapter.toml` under `docs/adapters/manifests/<agent>/`, validated by a new workspace test that PATH-shadows the manifest's bare-word exec with the bundled `fake_agent` (the `hermes_shim` precedent) and runs the full mock conformance suite per manifest — CI never launches real agents.

## Boundaries & Constraints

**Always:**
- Every capability/metering claim uses the contract vocabulary verbatim; absence is the explicit `—` + tiers-attempted notice (the FR-40 precedent), never a blank, fabricated pass, or implied zero.
- Every honesty-table row carries an evidence class: `run-verified` (version-pinned, from docs/agents.md), `contract-validated` (TCK against the fake), or `characterized` (upstream docs, dated, NOT run-verified) — and upstream stability status (stable/experimental/preview/third-party adapter) for every entry.
- The published manifest execs are bare program names resolved on PATH (the contract's own resolution rule) — real binaries for users; CI shadows them via PATH.
- Public pages carry no planning provenance (check_docs ban: no story/epic numbers, no `_bmad-output`, no BMAD wording; FR-N/AD-N spellings are permitted).
- The gallery re-states the PATH requirement + re-validation duty per entry (the docs/agents.md discipline).
- Docs updated in the same change: README points at the gallery; docs/agents.md cross-links it; the gallery links to manifest.md + adapter-contract.md.

**Ask First:** (standing instruction 2026-10-24: collect to end — each gate below was pre-decided and is listed in the Spec Change Log) page layout = flat `docs/adapters/<agent>.md` + `docs/adapters/manifests/<agent>/adapter.toml`; full 12-section TCK per manifest entry with a measured local runtime budget (subset via `SectionSelection` only if the lane grows past ~5 min); the deployed-site port (hekma.ktesio.dev builds from the ktesio.dev repo) lands as a companion PR in that repo, not here.

**Never:** No engine/CLI code changes beyond the test file; no new CI workflow (the gallery lane is a workspace test riding existing lanes); no real-agent launches in CI; no composite Action or `hekma adapter add` (deferred); no claims beyond what validation proves; no planning-provenance wording on public pages.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| CI validates a manifest entry | `docs/adapters/manifests/<agent>/adapter.toml`, exec bare word shadowed by `fake_agent` on PATH | full TCK runs; `is_conformant()` true on all three OS lanes | on Fail: test prints the section + reason verbatim |
| Manifest drifts (page claims, manifest fails) | edited manifest or overclaiming page | gallery test or check_docs fails the build | named reason, never silent |
| Agent with no metering tier | e.g. an ACP agent without sentinel/override | page shows `—` + tiers-attempted notice wording from fleet.rs | N/A — honesty is the output |
| User copies an ACP snippet | registration snippet from a page | `hekma agent register --kind acp` with the documented `acp.command`/`acp.args` succeeds on a machine hosting the binary | launch-failure diagnostic names the executable (PATH rule) |

</frozen-after-approval>

## Code Map

- `docs/meta.json` + `docs/source.config.ts` — page registration (stem-without-.md / filename-with-.md); source.config.ts tolerates missing entries but register properly anyway (17-5 lesson: both are load-bearing)
- `scripts/check_docs.py:15-19` — `DOC_PATHS` globs; ADD `docs/adapters/*.md` or gallery pages escape every gate (provenance ban :92-102, links, fences); `--self-test` :106-116 runs in CI ci.yml:623
- `crates/hekma-conformance/tests/third_party_manifest.rs` — the pattern for authoring + validating a manifest: `fake_agent_bin()` absolute path (:25), `[lifecycle.start]` exec+args (:33-35); gallery test instead PATH-shadows a COPY of fake_agent named as the manifest's bare exec (pattern: `crates/hekma-engine/tests/hermes.rs:482-527` install_shim + PathGuard prepend)
- `crates/hekma-conformance/src/tck.rs:740-742` — `run_mock_conformance(dir)`; 12 fixed sections (:546-581); `ConformanceReport::is_conformant/failures` (:339-357); pre-section failure prefixes (:818-825)
- `crates/hekma-conformance/src/test_support.rs:747-749` — `fake_agent_bin()` resolves the built helper with on-demand cargo-build fallback (stale-helper trap: rm+rebuild, docs/testing.md)
- `docs/agents.md` — the four repo-documented agents' exact claims (pins, commands, metering tiers) the pages must not contradict; the PATH + re-validation duty wording to re-use
- `docs/adapter-contract.md:29-35` / `docs/manifest.md:137-169` — vocabulary (guaranteed/best-effort/unsupported per-OS; self-reported/engine-observed); manifest exec = "a program name on PATH, or an absolute path" (:139)
- `crates/hekma-engine/src/domain/fleet.rs:433-436,569` — the exact tiers-attempted notice template + `METERING_SEED_CELL = "—"` the pages quote
- Seed facts (2026-10-23 analyst pass, preserved: `~/.hermes/planning-2026-10-23-adoptability/out-mariam.log` §B): ACP-native — OpenCode (`opencode acp`, run-verified v2.0.28), Hermes (`hermes-acp`, run-verified v0.21.7+500), Gemini CLI (`--experimental-acp`, smoke-listed), Copilot CLI (`--acp --stdio`, public preview, characterized), Cursor CLI (`agent acp`, hidden/advanced, characterized), goose (`goose acp`, experimental, characterized); third-party ACP adapter — Claude Code (`claude-code-acp`, supply-chain decision is the operator's), Codex (`codex-acp`), Crush (`crush-acp`, unofficial); manifest-based — Aider (engine-observed via `--openai-api-base`/`OPENAI_API_BASE` override hypothesis, no sentinel; characterized)

## Tasks & Acceptance

**Execution:**
- [x] `docs/adapters.md` — NEW gallery index: what the gallery is, the vocabulary legend (three evidence classes, capability vocabulary, the `—` notice), link to every entry
- [x] `docs/adapters/{copilot-cli,hermes,opencode,claude-code,codex,gemini-cli,goose,crush,aider,cursor-cli}.md` — NEW: per-agent page — one-line intro; registration snippet (exact `acp.command`/`acp.args` from Code Map, or the manifest link for Aider); honesty table (pause/interaction per-OS, metering source, tokens column with `—`+notice where applicable); validated-against pin (only where one exists — the four from docs/agents.md); upstream stability + evidence class; PATH requirement; re-validation duty
- [x] `docs/adapters/manifests/aider/adapter.toml` — NEW: the one manifest entry — bare-word `exec = "aider"`, launch args, `engine-observed` metering declaration, per-OS capability honesty, `{env:...}`-free (no substitution exists; keep it literal)
- [x] `crates/hekma-conformance/tests/gallery_manifests.rs` — NEW: for each dir in `../../docs/adapters/manifests/*/` — read adapter.toml, copy `fake_agent` to a temp dir named as the manifest's exec word, prepend that dir to PATH, `run_mock_conformance(dir)`, assert conformant with failures printed; ALSO assert a page exists at `docs/adapters/<agent>.md` (drift guard); serialize (process-global env)
- [x] `scripts/check_docs.py` — add `docs/adapters/*.md` to DOC_PATHS; run `--self-test`
- [x] `docs/meta.json` + `docs/source.config.ts` — register the 11 new pages (Reference group)
- [x] `README.md` + `docs/agents.md` — pointers to the gallery (agents.md keeps its pins; the gallery does not duplicate the deprecated-hermes-kind notice without linking it) — *(2026-10-24: README pointers applied; the docs/agents.md one-line reverse pointer is BLOCKED by the environment's agent-instruction-file write guard on the `agents.md` filename (approval timed out, no user present) — the exact sentence is recorded in the run's decision list for Islam to apply or bless; check_docs/link integrity does not depend on it)* — README done 2026-10-24; the docs/agents.md edit was DENIED by the agents.md write-guard (autonomous run, no user consent surface) — apply the one-line pointer at PR open
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` — tick 20-3 per house convention on PR open (row updated 2026-10-24 with the implementation record + the blocked item; flip to done at PR open)

**Acceptance Criteria:**
- Given the gallery test on any OS lane, when a shipped manifest or page-manifest pair drifts, then the build fails with a named reason.
- Given a reader opening any page, every claim carries its evidence class and vocabulary token verbatim; no planning-provenance terms (check_docs green).
- Given `python3 scripts/check_docs.py` and `npm run --prefix docs build`, both pass with the 11 pages registered.
- Given the TCK run for `aider`, the full fixed section suite passes against the PATH-shadowed fake (measure; record runtime in the PR; subset only if a lane grows past ~5 min — record which sections and why). *(Amended 2026-10-24: the kit's fixed set is what ships — the earlier "12-section" count was this spec's slip; the test asserts `TckSection::ALL` in fixed order, measured ≈3.9 s, no subset needed.)*

## Spec Change Log

- 2026-10-24 (pre-approval, standing autonomous instruction): Checkpoint-1 gates pre-decided and recorded: (1) flat-page + `manifests/` layout (fumadocs files-list friendly, keeps machine-readable validation root separate); (2) full-suite TCK per manifest with measured budget; (3) the ktesio.dev site port is a companion PR in that repo — this repo's docs/ stays the source of truth; (4) PATH-shadowing over absolute fake paths (ships the same manifest users copy; the contract's own PATH resolution); (5) only Aider ships a manifest (the other nine are `acp`-kind registrations — shipping manifests for them would be dishonest scaffolding); (6) spec exceeds the 1600-token guideline — [K] keep: the gallery is one cohesive deliverable (index + pages + manifests + one validation test), splitting fragments the PR without decoupling risk.
- 2026-10-24 (step-04 review, loop 0 — patches only, no loopback): 3 reviewers (blind-hunter, edge-case-hunter, verification-gap) at max effort. Triage: 2 findings REJECTED on execution evidence (the "stale 12-section set" and "missing --dump pair makes the lane red" claims — TckSection::ALL is 8 on this baseline and the suite runs green; the 12-section TCK lives on the in-flight epic-18 branch); ~16 findings PATCHED (test hardened: stray-entry panic, packaged-checkout visible skip, empty/whitespace exec asserts, page-exec-word drift guard, three-surface nav drift guard, expected-set derived from TckSection::ALL; SAFETY comment corrected; aider page copy-destination + /v1 caveat + exact drift-guard wording; cursor exec-collision note; index compound upstream-status values; docs README link; docs-probe pins the gallery URL+title; RELEASE_NOTES Unreleased entry); 3 DEFERRED (fragment-anchor validation, lane pacing as the gallery grows, hermes.rs comment); 1 known-blocked item stays in the decision list (docs/agents.md reverse pointer — platform write guard on the agents.md filename). *(Provenance note: this entry was briefly written OVER the pre-approval entry — an append-only violation — and restored in the same session before commit.)*

## Design Notes

- The gallery test must `rm -f` its stale-helper copies before rebuilding (the stale-helper trap) and assert `fake_agent_bin()` built on demand; copy per manifest with the manifest's exact exec word (`.exe` suffix on Windows via `std::env::consts::EXE_SUFFIX`).
- Aider's manifest declares `engine-observed` metering; the TCK's observed section wires its own loopback stub via `metering.upstream_base_url` (tck.rs:1021-1030) — network-free by construction. The page says explicitly: tokens flow only when the user wires the base-URL override; otherwise `—` + notice.
- Cross-platform: the PATH-prepend + copy trick is the documented hermes_shim pattern; on Windows the copy needs the `.exe` suffix and the temp dir must not collide with the concurrent engine-integration-serial group (this test serializes internally; it does not touch that group).

## Verification

**Commands:**
- `cargo test -p hekma-conformance --test gallery_manifests -- --nocapture` — expected: all manifests conformant, per-manifest runtime printed
- `python3 scripts/check_docs.py && python3 scripts/check_docs.py --self-test` — expected: green with the new glob
- `npm run --prefix docs build` — expected: site builds, 11 new pages in the index
- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings` — expected: green
