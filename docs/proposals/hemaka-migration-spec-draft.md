# Hemaka Migration Specification — DRAFT (rev 2, NOT RATIFIED)

Status: Phase-1 proposal under the draft-then-ratify rule (AGENTS.md AI-58).
Untracked draft under `docs/proposals/` — NOT in the Fumadocs page registry,
NOT validated by `scripts/check_docs.py` (which globs only `docs/*.md` depth
1), touches ZERO real planning artifacts. Nothing executes until the owner
ratifies the remaining opens and gives a scoped go per irreversible step.

Rev 2 (2026-09-16): product pivot **Nomarch → Hemaka** and **no
compatibility window** per the owner's Phase-1 answers (D1–D4 below,
recorded verbatim in §7). This rev supersedes the Nomarch draft entirely.

Brand model: **Ktesio (ktesio.com) remains the publisher and umbrella
brand; Hemaka becomes a ktesio.dev project.** Product rename under a
preserved publisher — not a global Ktesio→Hemaka replacement.

Audit basis: snapshot `4299bde` (2026-09-16); reconciled 2026-09-16 —
local `main` == `origin/main`, clean, zero drift. (All §1 evidence below
was verified against this state on 2026-09-16.)

---

## 1. Verified current state (evidence)

### 1.1 Repository and workspace

- Remote `https://github.com/Ktesio/ktesio.git`; default branch `main`.
- Workspace version `0.7.0` (CLI inherits); MSRV 1.96.1;
  `homepage`/`repository` = `https://github.com/Ktesio/ktesio`.
- Crates: `crates/kt` = package **`ktesio`** 0.7.0, bin **`kt`**;
  `ktesio-engine` 0.3.0; `ktesio-adapter-api` 0.1.0;
  `ktesio-adapters-hermes` 0.1.0; `ktesio-conformance` 0.1.0
  (`publish = false`; stale owner placeholder 0.0.1 on crates.io).

### 1.2 Registry (crates.io, checked 2026-09-16)

- Published: `ktesio` 0.7.0 (deps: `ktesio-engine ^0.1.0` — confirmed
  against the live dependency record; main uses engine 0.3.0),
  `ktesio-engine` 0.1.0–0.3.0, `ktesio-adapter-api` 0.1.0,
  `ktesio-adapters-hermes` 0.1.0, `ktesio-conformance` 0.0.1.
  → preserve all; yank nothing.
- **Name availability (404 = free), checked 2026-09-16**: `hemaka`,
  `maka`, `hemaka-engine`, `hemaka-adapter-api`, `hemaka-adapters-hermes`,
  `hemaka-conformance` — all free. Re-check 404 immediately before any
  publish.
- Command collisions: `hemaka` and `maka` — no exact Debian package, not
  in homebrew-core, not on crates.io. Both command names are clean
  (unlike `nomarch`, ruled out: Debian Arc archive extractor).

### 1.3 GitHub state

- Releases v0.1.1 … v0.7.0; latest v0.7.0 (2026-09-10) with
  `ktesio-v0.7.0-<target>` assets ×4 + checksums. Self-update shipped in
  **v0.4.0**; `ktesio-<tag>-<target>` naming unchanged since v0.3.0.
- Tap `Ktesio/homebrew-tap`: `Formula/ktesio.rb`, README; **no
  `formula_renames.json` yet**.
- v0.7.0 release run: builds green; `publish release` failed at exactly
  **"Checkout Homebrew tap"** — the recorded `HOMEBREW_TAP_TOKEN`
  failure ("renew the secret before the next release"). No release run
  since → **renewal UNVERIFIED; hard preflight**. Never print the secret.
- PR #186 (Nomarch banner) — closed unmerged; its artwork is the older
  iteration and is now moot anyway (Hemaka). **No Hemaka artwork exists
  in the repo — newest full-resolution assets are a required input.**
- Open PRs: dependency bumps + stale #39; none overlap the migration.

### 1.4 Release / installer / updater surface (name-coupled)

- `release.yml`: builds `kt` → `ktesio-<tag>-<target>.*` + `.sha256` +
  `ktesio-<tag>-checksums.txt`; publishes crate `ktesio` (version==tag);
  release titled "Ktesio <tag>"; writes `Formula/ktesio.rb` (class
  `Ktesio`, installs `kt`) to the tap via
  `scripts/generate_homebrew_formula.py`; opens release-docs PR.
- `scripts/public/install.sh|.ps1` (Cloudflare Pages project `ktesio-cli`
  at `cli.ktesio.dev`, serves `scripts/public`): `REPO=Ktesio/ktesio`,
  `TAP=ktesio/tap/ktesio`, `CRATE=ktesio`, `BIN=kt`;
  `releases/latest` → `ktesio-<tag>-<target>` + `.sha256`; extract only
  `kt`/`kt.exe`; refuse to overwrite a non-Ktesio `kt` (matches
  `--version` = `kt v?N`); channel detection `*/Cellar/ktesio/*`,
  `$CARGO_HOME/bin`, else manual; Windows default
  `%LOCALAPPDATA%\ktesio\bin` | `~/.ktesio/bin`; env
  `KTESIO_INSTALL_METHOD|_DIR|_DRY_RUN` (+ `KTESIO_INSTALL_TEST_*`).
- `kt self-update` (`crates/kt/src/cli/self_update.rs`): brew channel →
  `brew upgrade ktesio/tap/ktesio`; cargo channel →
  `cargo install ktesio --force`; manual channel → `releases/latest`,
  `ktesio-{tag}-{triple}.{ext}` + `.sha256`, extract only `kt`/`kt.exe`,
  atomic same-filename replace (an old updater can NEVER install a
  differently-named binary).
- `update_check.rs`: passive notice; opt-out `KTESIO_NO_UPDATE_CHECK`
  (+ `CI`); 1h cache `<user cache>/ktesio/update-check.json`;
  feed `releases/latest`.
- `install_channel.rs`: `Cellar/ktesio` probe; `brew list --formula
  ktesio | ktesio/tap/ktesio`; cargo-home probes.
- `generate_release_docs.py`: "# Ktesio {tag}", `ktesio-{tag}-*` table.
- `test_automation.py`: pins every string above (formula class, asset
  names, `publish --locked -p ktesio`, coverage crate list, boundary
  allowlist, semver baseline SHAs mirrored with ci.yml, probe URL).

### 1.5 State, wire, and third-party contracts (BINDING — preserved per brief)

- **Engine path authority** (`crates/ktesio-engine/src/paths.rs`):
  explicit override → `KTESIO_STATE_DIR` (absolute required) →
  `ProjectDirs::from("","","ktesio").data_dir()` — the **legacy data root
  directory name is `ktesio`**; layout `state.db`, `secrets.toml`,
  `agents/<name>/…`; stored paths absolute.
- **`KTESIO_USAGE ` stderr sentinel** (`ports/usage_source.rs`) —
  self-report wire protocol EMITTED BY third-party agents.
- **`KTESIO_MEMORY_DIR`** mock memory mapping — defined in engine
  `builtin.rs` + conformance `lib.rs` with a parity guard.
- **`HERMES_HOME`** — Hermes Agent's own env var (`memory.dir` mapping).
- Adapter contract `contract_version = "1.0.0"` (major-match
  negotiation); JSON document shapes + schema-version fields; 0–6
  exit-code contract; `kt --version` = `kt <v>`.
- These survive the rename REGARDLESS of the no-window artifact decision:
  data compatibility is independent of artifact compatibility.

### 1.6 CI gates (name-coupled)

- `boundary`: `cargo tree -p ktesio` + allowlist
  `ktesio-(engine|adapter-api|adapters-hermes)`.
- `semver`: in-repo freeze baselines `ktesio-adapter-api @ 4119db3`,
  `ktesio-engine @ 49da96b` — **baseline commits contain the OLD package
  names**, so same-name baseline diffing breaks by construction after a
  rename. Mirrored pins in `test_automation.py`.
- `coverage`: per-crate tarpaulin list (all five ktesio* crates); ≥95%.
- `docs`: `check_docs.py` (root + `docs/*.md`; `kt` command allowlist) +
  release-docs dry run + `test_automation.py` + Fumadocs build (deployed
  by Cloudflare Pages `ktesio-docs` OUTSIDE repo CI at docs.ktesio.dev).
- `docs-probe.yml`: daily 200-check against `https://docs.ktesio.dev/…`
  (the old host), content marker on `/release-notes`.
- `.config/nextest.toml`: serialized group `package(ktesio-engine)`.

### 1.7 Docs / brand / legal surface

- Fumadocs site: `meta.json` title "Ktesio"; `app/layout.tsx`
  metadataBase `https://docs.ktesio.dev`, OG "Ktesio Docs", banner
  `ktesio-banner.png` (2000×667).
- `README.md`: banner, "# Ktesio", badges, install commands
  (`curl … cli.ktesio.dev/install.sh | sh`, `irm …/install.ps1 | iex`,
  `cargo install ktesio`, `brew install ktesio/tap/ktesio`,
  historical `imagdy/tap` note).
- `main.rs`: clap `name="kt"`; help footer
  **"License: Ktesio Noncommercial-Attribution License 1.0.0"** (drift
  guard pins the title to LICENSE binding text).
- `LICENSE` (Ktesio Noncommercial-Attribution License 1.0.0, (c) 2026
  Islam Magdy, PolyForm-NC base + attribution), `CLA.md` (Owner Islam
  Magdy), `TRADEMARK.md` (Ktesio Marks policy).
- BMAD artifacts incl. dated `*-ktesio-2026-07-02` snapshot dirs —
  historical records.

---

## 2. Identity / compatibility classification map

### Category 1 — Product identity to CHANGE (Hemaka becomes the product)

| Surface | Change |
|---|---|
| GitHub repo `Ktesio/ktesio` | Rename to **`Ktesio/hemaka`** (gated, late; redirects then serve old hardcoded URLs; never recreate the old name) |
| Crate names | `ktesio`→**`hemaka`** (bins **`hemaka`** + **`maka`**), `ktesio-engine`→`hemaka-engine`, `ktesio-adapter-api`→`hemaka-adapter-api`, `ktesio-adapters-hermes`→`hemaka-adapters-hermes`, `ktesio-conformance`→`hemaka-conformance`; dirs `crates/ktesio-*`→`crates/hemaka-*` |
| Executables | `hemaka` (primary) + `maka` (short alias) — both in every archive/tap/cargo install, both standalone. `kt` fate = **open decision D7** (clean break vs courtesy) |
| Release assets | ONE family `hemaka-<tag>-<target>.*` + `.sha256` + `hemaka-<tag>-checksums.txt` — **no `ktesio-*` family, ever** (D4: no compatibility window) |
| Homebrew | `Formula/hemaka.rb` class **`Hemaka`** (installs `hemaka` + `maka`); tap-root `formula_renames.json` `{"ktesio":"hemaka"}`; tap repo NOT renamed; `brew install ktesio/tap/hemaka` |
| Docs | Title/OG "Hemaka"; canonical **`hemaka.ktesio.dev`** (D2); banner → newest Hemaka artwork (REQUIRED INPUT — none exists yet); nav + page registry + link map; install commands → `cli.ktesio.dev/hemaka/install.sh|.ps1`; legacy installer URLs stay operational |
| Wording | README/docs prose, crate `description`s, CLI about/help, UI messages, release titles "Hemaka <tag>", release-doc generator output → Hemaka-as-product, Ktesio-as-publisher |
| New-user paths | `cargo install hemaka`; `brew install ktesio/tap/hemaka`; Windows default `%LOCALAPPDATA%\hemaka\bin` (legacy `ktesio\bin` still recognized as an existing install to migrate) |
| New-code identity | User-Agent `hemaka/<v>`; temp prefix `hemaka-install.*`; new settings aliases `HEMAKA_STATE_DIR`, `HEMAKA_NO_UPDATE_CHECK`, `HEMAKA_INSTALL_*` |

### Category 2 — Publisher identity to PRESERVE (Ktesio)

GitHub org **Ktesio**; ktesio.com; ktesio.dev; `*.ktesio.dev` hosts
(`cli.ktesio.dev`, `hemaka.ktesio.dev` new, `docs.ktesio.dev`→redirect);
**`Ktesio/homebrew-tap` not renamed** (only its formula + README content
change; brew tap name stays `ktesio/tap`); Cloudflare projects
`ktesio-cli`/`ktesio-docs`; "Hemaka is a Ktesio project" framing;
`imagdy/tap` historical note stays (documents an old migration path).

### Category 3 — Legal attribution, SEPARATE approval (no silent amendment)

- `LICENSE`: proposal **unchanged** (binds the code regardless of product
  name; `--help` keeps printing the exact title; drift-guard test
  unchanged). Retitling = separate explicit decision.
- `CLA.md`: proposal unchanged.
- `TRADEMARK.md`: proposal — additive Phase-2 amendment naming Hemaka
  marks as controlled by the Ktesio project maintainers, gated on owner
  approval of exact wording.
- Formula license comment, license badge, PolyForm attribution:
  unchanged (restate LICENSE).

### Category 4 — Compatibility contracts to PRESERVE (binding regardless of D4)

1. **Legacy data root stays the default**: `ProjectDirs("ktesio")`
   remains the platform data-dir name; existing installations keep state
   IN PLACE. No move/reset/merge/delete — ever, in this migration.
2. **Path resolution precedence** unchanged (explicit override → env →
   platform). `KTESIO_STATE_DIR` keeps working. `HEMAKA_STATE_DIR` is an
   additive alias: both set to the same absolute path → OK; both set to
   different paths → hard error with a clear diagnostic; relative →
   rejected as today; an access error is an ERROR, never "empty
   installation".
3. **`KTESIO_USAGE ` sentinel** stays THE self-report wire format (agents
   emit it; we only parse). No `HEMAKA_USAGE` addition in this migration
   (surface stays minimal; revisit deliberately).
4. **`KTESIO_MEMORY_DIR`** mock mapping (engine + conformance, parity
   guard) unchanged. **`HERMES_HOME`** untouched.
5. **Update-check opt-out**: either `KTESIO_NO_UPDATE_CHECK` or
   `HEMAKA_NO_UPDATE_CHECK` truthy disables (plus `CI`); cache stays at
   the legacy `<cache>/ktesio/update-check.json` leaf.
6. **Exit codes 0–6, JSON shapes, schema-version fields, NDJSON logs**:
   byte-identical under `hemaka`/`maka` (no branding-driven schema or
   accounting change).
7. **No `--force` fight over `kt`**: one CLI implementation — `hemaka`
   crate owns `hemaka` + `maka`; the legacy `ktesio` crate is FROZEN at
   0.7.0 (never republished, never yanked, owns only `kt`). Cargo-channel
   self-update in hemaka runs `cargo install hemaka --force`.
8. **Old tags, old releases, old assets, published crates**: untouched.
   Tag format `vMAJOR.MINOR.PATCH`. Release-process decision log:
   append-only.
9. **Upgrade fidelity**: released-state upgrade tests must prove
   instance/run identity, usage, budgets, memory, permissions and
   absolute paths survive (Category 4.11 of the brief).

### Category 5 — Third-party identifiers to PRESERVE

Hermes Agent / OpenClaw / OpenCode / Copilot CLI names; `HERMES_HOME`;
adapter authors' manifests + `contract_version`; PolyForm attribution;
SHA-pinned actions; upstream deps; homebrew-core.

### Category 6 — Historical records to PRESERVE (no rewrite)

`_bmad-output/**` (incl. dated `*-ktesio-2026-07-02` dirs);
`CHANGELOG.md`/`RELEASE_NOTES.md` history (new entries switch wording);
git history; published crates + release assets; release-process decision
log; `imagdy/tap` note.

---

## 3. Consequences of NO COMPATIBILITY WINDOW (D4) — stated plainly

1. **Old manual-channel self-update breaks by design.** `kt self-update`
   (manual channel) in v0.4.0–v0.7.0 constructs
   `ktesio-v<tag>-<target>` URLs against `releases/latest`; the first
   Hemaka-only latest release 404s that download and the updater errors
   (checksum/asset fetch failure). This is accepted under D4. The
   migration guide must say exactly this and give the path: new
   installer / `cargo install hemaka --force` / brew / manual download.
2. **Brew channel may partially survive via `formula_renames.json`**
   (`ktesio`→`hemaka`): an old `kt self-update` on the brew channel runs
   `brew upgrade ktesio/tap/ktesio`, which brew resolves through the
   rename. PLAUSIBLE, NOT PROMISED — proven or ruled out by a Phase-2
   fixture against a real local tap before the guide claims anything.
3. **The new installer owns legacy detection**: it must recognize an
   existing `kt` (version output `kt v?N`), `Cellar/ktesio/*`,
   `$CARGO_HOME/bin/kt`, legacy Windows dirs — and migrate by channel:
   brew → `brew install|upgrade ktesio/tap/hemaka`; cargo →
   `cargo install hemaka --force` (+ guide note: `cargo uninstall
   ktesio` removes the orphaned `kt`); manual → install `hemaka`+`maka`
   beside/over the manual `kt` (never silently delete a user file
   without printing what happened; removal of the old `kt` binary is
   visible and explicit).
4. **One asset family** = simpler release pipeline, but the
   "verify BOTH families before exposing latest" pre-release check is
   replaced by "verify the hemaka family AND that no stale ktesio-*
   expectation remains in any installer/updater/tap/doc".
5. **`kt` retention is the remaining open scope decision (D7)** — see §4.

---

## 4. Decisions

### Ratified 2026-09-16 (owner, Phase-1 question round 1 — recorded)

- **D1 Product/executables**: "Forget about Nomarch. Update the plan to
  use Hemaka (`hemaka` and `maka` command)." → product Hemaka; primary
  executable `hemaka`; short alias `maka`. Both shipped in every
  artifact, both standalone. (Collision checks clean — §1.2.)
- **D2 URL layout**: `hemaka.ktesio.dev` canonical docs; `ktesio.dev/
  hemaka` landing; `docs.ktesio.dev` 308 → canonical (hosting layer);
  installers `cli.ktesio.dev/hemaka/install.sh|.ps1`; legacy installer
  URLs operational.
- **D3 Versions**: CLI **0.8.0** (continues 0.7.0); libraries continue
  their lines under new names: `hemaka-engine` **0.4.0**,
  `hemaka-adapter-api` **0.2.0**, `hemaka-adapters-hermes` **0.2.0**;
  `hemaka-conformance` 0.1.0 `publish=false`. Published `ktesio*`
  versions untouched.
- **D4 Compatibility window**: **none** — no dual archive families;
   single `hemaka-*` family from 0.8.0 on. Consequences accepted per §3.

### Open (round 2 — blocking Phase 2)

- **D7 `kt` executable fate**: (a) clean break — 0.8.0 ships
  `hemaka`+`maka` only, `kt` never ships again (aligns with D4; the
  brief's "retain kt" default is superseded by D1+D4 but dropping a
  shipped binary is a scope change the owner must own explicitly);
  (b) courtesy ship — `kt` also included in `hemaka-*` archives during a
  bounded period (helps muscle memory only; old updaters still can't
  fetch these archives); (c) keep `kt` alongside indefinitely.
- **D5 Migration floor / guide guarantee**: (a) all old versions
  (v0.1.1–v0.7.0) are migrate-able via the new installer (it is
  version-agnostic; tested hops from 0.4.0/0.5.0/0.6.0/0.7.0 + spot
  oldest) — recommended; (b) only v0.6.0+; (c) only v0.7.0.
- **D6 Banner wording** (exact strings for approval):
  - Product: "Hemaka is a Ktesio project. It is source-available under
    the Ktesio Noncommercial-Attribution License 1.0.0 — not OSI open
    source; commercial use requires written permission from the
    copyright holder."
  - Isolation: "Agent Home organizes and isolates an agent's files. It
    is not a security sandbox: supervised agents run as normal processes
    with your user's permissions."
  Placement: README + docs get-started (product); agents/architecture
  docs (isolation); no CLI-output churn this release.
- **D8 Ratify rev-2 spec and start Phase 2** on feature branches (all
  local gates; irreversible ops still individually gated).

---

## 5. Rename-aware gate strategy (CI keeps its teeth)

- **boundary**: allowlist → `hemaka-(engine|adapter-api|adapters-hermes)`;
  `cargo tree -p hemaka`; intent unchanged.
- **semver**: baselines 4119db3/49da96b contain old package names →
  same-name diffing breaks by construction. Strategy (no compatibility
  claimed from skipped checks):
  1. Fresh in-repo baselines AT the rename commit for `hemaka-engine` /
     `hemaka-adapter-api` (drift-proof after the rename).
  2. **External consumer fixtures** (CI-gated): out-of-tree crates
     depending on (a) published `ktesio-engine 0.3.0`, (b)
     `hemaka-engine 0.4.0`, (c) BOTH in one graph — compile + exercise
     the embedding facade (proves a user's full dependency graph
     migrates; aliases prove nothing).
  3. crates.io release-to-release loop re-arms at first `hemaka-*`
     publish.
  4. Mirrored pins in `test_automation.py` updated in the same change;
     never widen a gate to force a pass.
- **coverage**: per-crate list renamed; ≥95% merged gate unchanged.
- **docs job**: `check_docs.py` keeps the `kt` allowlist (compat
  examples) and gains `hemaka`/`maka` allowlists; `test_automation.py`
  pins → `class Hemaka`, `hemaka-v1.2.3-*` assets, `publish --locked -p
  hemaka`, `formula_renames.json` presence, boundary/semver mirrors.
- **docs-probe.yml**: canonical 200 + content marker at
  `hemaka.ktesio.dev`; legacy `docs.ktesio.dev/<page>` must answer 3xx
  → canonical (NOT 200). Registry still read from `docs/meta.json`.
- **nextest**: group filter → `package(hemaka-engine)`.
- **Residual-name allowlist** (explicit, reviewed): every intentional
  surviving `ktesio`/`KTESIO` occurrence post-migration — LICENSE/CLA
  titles + text, `KTESIO_*` env/sentinel contract names,
  `ProjectDirs("ktesio")` default root, `Cellar/ktesio` + legacy-dir
  detection, cache leaf, historical docs/changelog/BMAD records,
  decision log, `imagdy/tap` note, migration guide's legacy examples. A
  verification grep asserts NOTHING outside this inventory remains.

---

## 6. Dependency-ordered release / cutover plan (draft)

Every ⛔ step is irreversible and needs its own scoped approval;
`cargo publish --dry-run` against the real registry is held per the
repo's stricter rule; no earlier GO is reused.

1. **Inputs & preflights** (no repo mutations): owner supplies newest
   full-resolution Hemaka artwork; D5–D8 resolved; crates.io 404
   re-check; HOMEBREW_TAP_TOKEN preflight (no-op dispatch; never print
   it); Cloudflare redirect plan (hemaka.ktesio.dev deploy,
   docs.ktesio.dev 308s, cli.ktesio.dev/hemaka/*) drafted.
2. **Code migration PRs** on feature branches (repo still
   `Ktesio/ktesio`): (a) crate+dir renames; `hemaka`+`maka` bins; env
   alias layer + conflict rules + tests; (b) release.yml single
   `hemaka-*` family + `publish -p hemaka` + formula generator →
   `hemaka.rb` + `formula_renames.json` + generator/test sync;
   (c) installers/updaters: new URLs, legacy-kt detection + channel
   migration, both-keg detection, Windows dirs, env reach-through to the
   installing shell, tests; (d) CI gates + `test_automation.py` +
   nextest + `check_docs.py` + docs-probe (canonical vs redirect);
   (e) docs/README/banner/metadata/migration guide (registered in
   `source.config.ts` + `meta.json` + link map) + D6 banners;
   (f) external consumer fixtures + released-state upgrade/install
   compatibility fixtures (matrix per §3.3 + Category 4.9).
   All local gates green (`fmt`, `clippy`, `test --workspace
   --all-targets`, `check_docs`, `test_automation.py`, docs typecheck/
   build, coverage, boundary, semver) + two-pass review (primary +
   independent adversarial) on the release-surface set (AI-55).
3. **Library publishes** ⛔ (ordered, each on the go, runbook mirrors
   release-process.md with new decision-log entries):
   `hemaka-adapter-api` → `hemaka-adapters-hermes` → `hemaka-engine`;
   tarball review + from-crates.io host probe BEFORE the tag.
4. **Tag v0.8.0** ⛔ → automation publishes crate `hemaka`, single
   `hemaka-*` asset family + checksums, `hemaka.rb` +
   `formula_renames.json` to the tap ⛔ (manual fallback documented if
   the token preflight failed). Verify the family + absence of stale
   ktesio-* expectations before pointing anything at the release.
5. **Post-release verification matrix** (evidence pack): installer
   migration from real 0.4.0/0.5.0/0.6.0/0.7.0 installs (manual, cargo,
   brew channels; per D5 floor); fresh install, PATH precedence,
   unrelated-command collision refusal, Windows replacement +
   uninstall, custom `CARGO_HOME`/install dirs, dry-run, documented env
   overrides reaching the installer shell; brew: rename-migration from
   an installed old keg (does `brew upgrade ktesio/tap/ktesio` resolve
   through formula_renames?), fresh install, reinstall, uninstall;
   old-updater behavior confirmed-broken-with-clear-error (manual
   channel) per D4; released-state data-upgrade test (Category 4.9).
6. **Hosting layer** ⛔ (outside repo CI): `hemaka.ktesio.dev` deploy,
   `docs.ktesio.dev` 308s, `ktesio.dev/hemaka` landing,
   `cli.ktesio.dev/hemaka/*` live, legacy installer URLs intact;
   docs-probe (updated in 2d) asserts both.
7. **Repo rename** ⛔ `Ktesio/ktesio` → `Ktesio/hemaka` — AFTER v0.8.0
   is live (release flow never depends on redirects). Canonical URLs
   (manifests, README, badges, formula homepage) flip in a follow-up
   release (0.8.1). Never recreate `Ktesio/ktesio`.
8. **Later, separate decisions**: removing `kt` guidance from docs,
   tap README refresh cadence, anything deferred.

Recovery: branch PRs revertible; post-publish crates.io immutable
(yank only as a separate decision — not for rebranding); tap recovery =
revert tap commits; repo rename reversible via rename-back only while
the old name is not recreated (this plan never recreates it); hosting
redirects are config-level reversible.

---

## 7. Decision log (this spec)

- 2026-09-16 — Phase-1 round 1 (owner): D1 Hemaka/`hemaka`+`maka`
  (supersedes the Nomarch proposal wholesale, including PR #186's
  artwork); D2 hemaka.ktesio.dev; D3 CLI 0.8.0 + libs continue
  (engine 0.4.0 / adapter-api 0.2.0 / adapters-hermes 0.2.0);
  D4 NO compatibility window (no ktesio-* asset family; old manual
  self-updaters break by design — §3).
- 2026-09-16 — verified: hemaka/maka names free on crates.io, no Debian/
  homebrew-core collision; HOMEBREW_TAP_TOKEN failure reproduced in the
  v0.7.0 run log ("Checkout Homebrew tap"); no Hemaka artwork in repo.
- Pending: D5 (migration floor), D6 (banner strings), D7 (kt fate),
  D8 (ratify + Phase 2 go).
