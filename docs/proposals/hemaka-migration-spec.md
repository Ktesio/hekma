# Hemaka Migration Specification — rev 3 (IMPLEMENTED; release gated)

Status: D1–D8 RATIFIED by the owner 2026-09-16 (Phase-1 question rounds,
recorded below). Implementation landed on branch `feat/hemaka-migration`
as a coordinated commit series; every irreversible step remains gated
pending its own scoped approval. This document is the deliverable
specification + identity/compatibility map + cutover plan + recovery
procedure + residual-name allowlist.

Prior revs: rev 1 (Nomarch draft) superseded wholesale by the owner's
"forget Nomarch, use Hemaka" answer; rev 2 (Hemaka, decisions D1–D4) —
both obsolete.

---

## 1. Ratified decisions (owner, 2026-09-16)

| # | Decision | Value |
|---|---|---|
| D1 | Product / executables | **Hemaka**; commands **`hemaka`** + **`maka`** (alias; both standalone, same CLI). Bare `nomarch` was ruled out (Debian Arc-extractor collision); `hemaka`/`maka` verified collision-free (crates.io 404, no Debian package, not in homebrew-core) |
| D2 | URL layout | Canonical docs **`hemaka.ktesio.dev`**; `docs.ktesio.dev` 308→canonical; landing `ktesio.dev/hemaka`; installers `cli.ktesio.dev/hemaka/install.sh|.ps1`; legacy installer URLs stay operational |
| D3 | Versions | CLI **0.8.0** (ktesio line continues; published `ktesio` frozen at 0.7.0); libraries continue their lines under new names: `hemaka-engine` **0.4.0**, `hemaka-adapter-api` **0.2.0**, `hemaka-adapters-hermes` **0.2.0**; `hemaka-conformance` 0.1.0 `publish=false` |
| D4 | Compatibility window | **NONE** — single `hemaka-*` asset family from 0.8.0; old manual-channel `kt self-update` breaks by design; installer/cargo/brew is the migration path |
| D5 | Migration floor | **ALL versions** (v0.1.1–v0.7.0), with test evidence via the migration-matrix harness |
| D6 | Banners | Both strings adopted (README/get-started product banner; agents/architecture isolation banner) |
| D7 | `kt` fate | **Clean break** — 0.8.0 ships `hemaka`+`maka` only; `kt` never ships again; existing `kt` keeps running on legacy data but never updates; never silently deleted by tooling |
| D8 | Phase 2 | Ratified + executed (this branch) |

Also resolved post-rev-2: the **newest full-resolution Hemaka artwork was
supplied 2026-09-16** (2172×724 PNG) and is installed at
`docs/assets/hemaka-banner.png` (README + site OG; the old
`ktesio-banner.png` remains as a historical asset). PR #186's older
Nomarch iteration is moot.

## 2. Implementation state (branch `feat/hemaka-migration`)

| Commit | Scope |
|---|---|
| `3ea7389` | Library crates `ktesio-*` → `hemaka-*` (dirs, manifests, identifiers) |
| `f0112ba` | CLI crate → package `hemaka`, bins `hemaka`+`maka` (lib split), self-update dual-binary replace, install-channel probes, `HEMAKA_STATE_DIR` alias + conflict rule, `HEMAKA_NO_UPDATE_CHECK`, `[hemaka]` diagnostic prefix, v0.8.0/0.4.0/0.2.0 version bumps |
| `d72a38c` | Installers (install.sh/ps1): hemaka+maka, legacy-kt channel migration, retirement notes, Windows dirs, `HEMAKA_INSTALL_*` aliases |
| `eff41c6` | release.yml (hemaka family, `-p hemaka`, Formula/hemaka.rb + formula_renames.json), generators, CI gates, **rename-aware semver** (surface-check script + external consumers + both-graph armed check), docs-probe dual (canonical + legacy redirect), check_docs/nextest/test_automation pins |
| `978c0fe` | Docs: README, all live pages, **migration guide** (registered), D6 banners, site metadata, artwork, TRADEMARK additive section, release-process update + decision-log appends |
| `df9dea1` | Migration-matrix harness + post-release evidence workflow |
| `9cb6573` | Stale `kt` doc-comment cleanup |

Local gates green: `cargo fmt --all --check`, `cargo clippy --workspace
--all-targets -D warnings`, `cargo test --workspace --all-targets`,
`check_docs.py` (27 files), `test_automation.py` (28 tests), docs
typecheck+build (Fumadocs static export), release-docs dry run, and
**both rename surface checks** (external consumers compile against
baseline `ktesio-adapter-api`@4119db3 and `ktesio-engine`@49da96b AND
HEAD's renamed crates).

## 3. Identity / compatibility map (final)

### Changed (product → Hemaka)
Crate names + dirs; binaries `hemaka`+`maka`; asset family
`hemaka-<tag>-<target>`; Formula/hemaka.rb class Hemaka (installs both
bins) + tap `formula_renames.json`; docs site identity + canonical host;
README/docs prose; CLI about/help/UI strings; miette codes `hemaka::*`;
engine diagnostic prefix `[hemaka]`; User-Agent `hemaka/<v>`; temp prefix
`hemaka-install.*`; Windows default dir `%LOCALAPPDATA%\hemaka\bin`;
release titles `Hemaka <tag>`.

### Preserved (Ktesio = publisher)
GitHub org `Ktesio`; repo `Ktesio/ktesio` (rename to `Ktesio/hemaka`
comes AFTER v0.8.0 ships — release URLs never depend on redirects;
canonical URL flip at 0.8.1); all `*.ktesio.dev` hosts; tap
`Ktesio/homebrew-tap` (NOT renamed; brew tap name `ktesio/tap`); "a
Ktesio project" framing; Cloudflare projects ktesio-cli/ktesio-docs;
`imagdy/tap` historical note.

### Preserved (compatibility contracts)
- **Data root `ProjectDirs("ktesio")` stays the default** — installations
  never move. `KTESIO_STATE_DIR` honored; `HEMAKA_STATE_DIR` alias;
  same-path both-set OK, different-paths = hard error; relative =
  rejected; access error ≠ empty install.
- `KTESIO_USAGE` sentinel (unchanged); `KTESIO_MEMORY_DIR` mock mapping
  (both definitions + parity test); `HERMES_HOME`; adapter contract
  `contract_version = "1.0.0"`; JSON shapes + schema versions; exit
  codes 0–6; `--version` identity (`hemaka <v>` for BOTH binaries).
- Update-check opt-out: either `KTESIO_NO_UPDATE_CHECK` or
  `HEMAKA_NO_UPDATE_CHECK` (or `CI`); cache leaf stays `<cache>/ktesio/`.
- Install-channel detection: `Cellar/hemaka` (new) and legacy `Cellar/
  ktesio` (installer-side migration), `brew list` four-name probe,
  `$CARGO_HOME/bin`, legacy Windows dirs.
- Cargo: `hemaka` owns `hemaka`+`maka`; `ktesio` frozen at 0.7.0 owns
  only `kt`; cargo-channel update = `cargo install hemaka --force`;
  guide documents `cargo uninstall ktesio` for the orphan.
- Old tags/releases/published crates untouched; nothing yanked; tag
  format `vMAJOR.MINOR.PATCH`; release-process decision log append-only.

### Preserved (legal — unchanged text, separate approval for any change)
LICENSE (title "Ktesio Noncommercial-Attribution License 1.0.0" — still
printed by `--help`, drift-guard test unchanged); CLA.md; PolyForm
attribution. TRADEMARK.md gained an ADDITIVE Hemaka-marks section
(merge-gated owner review, like the whole PR).

### Historical (no rewrite)
`_bmad-output/**` incl. `*-ktesio-2026-07-02` dirs; CHANGELOG +
RELEASE_NOTES history; git history; decision log; `imagdy/tap` note.

## 4. Residual-name allowlist (the intended `ktesio` that remains)

Verification grep = `rg -i ktesio` over tracked files, excluding
`_bmad-output/`, `CHANGELOG.md`, `docs/RELEASE_NOTES.md`, `cov/`,
`docs/proposals/`, `LICENSE`, `CLA.md`, `docs/assets/ktesio-banner.png`,
git history. Allowed remaining occurrences, by owner:

1. License title strings (HELP_FOOTER, formula comment, badge labels,
   README/docs license references).
2. Env/sentinel contract names: `KTESIO_STATE_DIR`,
   `KTESIO_NO_UPDATE_CHECK`, `KTESIO_USAGE `, `KTESIO_MEMORY_DIR`,
   `KTESIO_INSTALL_*` (+ test-only `KTESIO_INSTALL_TEST_*`,
   `KTESIO_*_HELPER` test seams).
3. `ProjectDirs("ktesio")` default data root + cache leaf
   `join("ktesio")` + their tests.
4. Repo/org/tap URLs: `github.com/Ktesio/ktesio`, `Ktesio/homebrew-tap`,
   `ktesio/tap/…`, `cli.ktesio.dev`, `docs.ktesio.dev` (legacy-redirect
   contexts), Cloudflare project names — until the gated repo rename +
   0.8.1 URL flip.
5. Migration surfaces: installers' `LEGACY_BIN="kt"` + legacy keg/dir
   probes; migration-matrix harness (`ktesio-v*` hops);
   `formula_renames.json` mapping; `ktesio-engine = "0.3.0"` in the
   both-graph fixture; `cargo uninstall ktesio` guidance.
6. check_docs `kt` allowlist entry (historical examples in the migration
   guide); release-process decision log; frozen-crate prose.

## 5. Dependency-ordered cutover plan

Preconditions (before ANY irreversible step): PR reviewed + merged
(merge = gated); HOMEBREW_TAP_TOKEN preflight (renewal UNVERIFIED — the
v0.7.0 run failed at "Checkout Homebrew tap"; verify via a no-op
dispatch; never print the secret); crates.io 404 re-check for all six
`hemaka*` names; CI green on the PR (incl. coverage ≥95 + semver +
both-graph skip-notices).

1. **Library publishes** ⛔ (ordered, each on its own go, runbook =
   docs/release-process.md): `hemaka-adapter-api` 0.2.0 →
   `hemaka-adapters-hermes` 0.2.0 → `hemaka-engine` 0.4.0; tarball
   review (two-pass per AI-55), from-crates.io host probe BEFORE the tag.
   Post-merge follow-up in the same window: pin fresh semver
   `--baseline-rev` baselines to the migration merge SHA (ci.yml +
   test_automation.py together).
2. **Tag v0.8.0** ⛔ → automation publishes crate `hemaka`, single
   `hemaka-*` family + checksums, `hemaka.rb` + `formula_renames.json`
   to the tap ⛔ (documented manual fallback if the token preflight
   failed). Verify the asset family + release before pointing anything
   at it. Old `kt` updaters now fail by design (D4) — expected.
3. **Migration-matrix evidence** ⛔ (dispatch after the release):
   kt v0.1.1→v0.7.0 hops green; brew rename-migration check from an old
   keg (does `brew upgrade ktesio/tap/ktesio` resolve through
   formula_renames — plausible, NOT promised until proven; the matrix +
   a manual brew check prove or correct the guide); installer matrices
   (fresh install, PATH precedence, custom CARGO_HOME/install dir,
   unrelated-command refusal, Windows replacement + uninstall, dry-run,
   env reach-through).
4. **Hosting cutover** ⛔ (outside repo CI, same session as the release
   to keep the docs-probe red-window minimal): deploy `hemaka.ktesio.dev`
   (Pages project for the docs export), 308s `docs.ktesio.dev`→canonical,
   `ktesio.dev/hemaka` landing, `cli.ktesio.dev/hemaka/install.sh|.ps1`
   routes with legacy `/install.sh` URLs intact. docs-probe goes green
   on its next run.
5. **Repo rename** ⛔ `Ktesio/ktesio` → `Ktesio/hemaka` (AFTER v0.8.0 is
   live; redirects then serve old hardcoded URLs; never recreate the old
   name). 0.8.1 flips canonical URLs (manifests, README, badges, formula
   homepage, LATEST_RELEASE_URLs) onto the new repo path.
6. **Later, separate decisions**: retire the migration-matrix schedule;
   remove legacy-kt migration notes; anything deferred.

## 6. Recovery

- Pre-merge: revert branch commits (all local).
- Post-publish, pre-tag: crates.io versions are immutable; recovery is
  fix-forward (0.4.1/0.2.1) — yank only under a separate explicit
  decision, never for rebranding.
- Post-tag: release assets change only via another gated release action;
  the tap recovery = revert tap commits (`Formula/hemaka.rb`,
  `formula_renames.json`) and push (gated).
- Repo rename: reversible via rename-back ONLY while `Ktesio/ktesio` is
  not recreated — this plan never recreates it.
- Hosting: redirects/config are Cloudflare-level and reversible without
  code changes; docs-probe verifies both directions.

## 7. External settings checklist (shipped in docs/migration.md)

| Setting | Status |
|---|---|
| `KTESIO_STATE_DIR` / `HEMAKA_STATE_DIR` | both honored; same-path OK; different = error |
| `KTESIO_NO_UPDATE_CHECK` / `HEMAKA_NO_UPDATE_CHECK` | either truthy disables (+`CI`) |
| `KTESIO_INSTALL_METHOD/_DIR/_DRY_RUN` / `HEMAKA_INSTALL_*` | both honored (alias preferred) |
| `KTESIO_USAGE` (agent→engine sentinel) | UNCHANGED |
| `KTESIO_MEMORY_DIR` (mock memory) | UNCHANGED |
| `HERMES_HOME` | UNCHANGED (third-party) |
| `KTESIO_INSTALL_TEST_*` + engine test helpers | test-only seams, names unchanged |

## 8. Checks executed vs. not (honest)

**Executed locally (green):** fmt, clippy `-D warnings`, workspace tests
(all targets), check_docs (27 files), test_automation (28), docs
typecheck + static build, release-docs dry run, rename surface checks
(both crates), installer dry-run smokes incl. a real legacy-kt migration
path, both-graph fixture compiles against the current-tree graph.

**NOT executed (blocked or CI-only — explicit):**
- Coverage (tarpaulin ≥95) — runs in CI on the branch push/PR; not run
  locally (90-minute CI-class job).
- MSRV job, 3-OS matrix, audit, perf-budgets — CI-only.
- Registry-side publishes, tag, tap push, hosting, repo rename — GATED
  (§5); each needs its own go.
- Migration matrix against real v0.8.0 artifacts — post-release by
  construction (there is no hemaka release to migrate to yet);
  pre-covered by dry-run smokes + installer tests.
- HOMEBREW_TAP_TOKEN renewal — unverified (no release run since the
  2026-09-09 failure); preflight required before step 2.
- `cargo publish --dry-run` against the real registry — HELD per the
  repo's stricter release rule.
