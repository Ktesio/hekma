# Sprint Change Proposal — Adoptability (Observability, CI Recipes, the Adapter Gallery)

**Date:** 2026-10-23 · **Trigger owner:** Islam (product directive — brainstorm session, decisions ratified in-session) · **Mode:** Batch
**Route:** Correct Course → new Epic 20 · **Scope classification:** Moderate (one new epic + additive PRD/spine additions + one dated §6 amendment; no replan of shipped work)
**Status:** APPROVED 2026-10-23 (Islam, Batch mode) — proposal as drafted; D1 = option (a); D2 = verbatim; D3 = in-repo. Applied the same day on branch `docs/adoptability`.

**Ratified inputs from the trigger session (2026-10-23):** standard OpenTelemetry (OTLP) over a
Prometheus-first approach; the adapter gallery lives in docs; the seed list is GitHub Copilot CLI,
Hermes, OpenCode, Claude Code, Codex, Gemini CLI, goose, Crush, Aider, Cursor CLI. Epics 18
(Service-Grade Lifecycle) and 19 (Wire Governance) are in active development elsewhere — this
proposal disturbs neither.

**Analysis provenance:** executed by the BMAD roster (mariam/Analyst, waleed/Architect, yahya/PM)
under bmad-correct-course; their reports are distilled below with citations. Epic-19 seam citations
read from the epic-19 worktree copy (`feat/epic-19-1-governance-vectors`, uncommitted to main) —
provenance-flagged where used.

---

## Section 1 — Issue Summary

**Problem statement.** Hekma runs agents well, but a newcomer's first hour lacks the artifacts
adoption runs on. Governance metrics do not feed the Grafana/Datadog stacks enterprises already
live inside; there is no recipe for running a governed bot in CI (the "agents that can't run away
in your pipeline" fear is unserved); and there is no validated index of which agents run under
Hekma today beyond the per-adapter docs pages. Islam's brainstorm ratified three workstreams —
OTLP telemetry export, agents-in-CI, the adapter gallery — and the Analyst pass verified the repo
claims they rest on.

**Issue type:** new strategic requirement (adoption/positioning); nothing failed.

**Evidence — what the three workstreams build on (Analyst-verified):**

| Need | Today in Hekma | Evidence |
| --- | --- | --- |
| Metrics derived from committed engine events | `EngineEvent` = Transition / BudgetBreach / UsageUpdate on a bounded broadcast bus; payloads schema-versioned; subscribers are read-only and can never block supervision | `crates/hekma-engine/src/domain/bus.rs:113-123`, `:25-34`, `:35-48` |
| A host-owned exporter surface | `hekma-host` flags `--listen`/`--bearer-secret`/`--state-dir`/`--metering-listen`/`--log-capture`; the host is already the HTTP adapter (AD-20) | `crates/hekma-host/src/main.rs:47-111` |
| Durable export discipline exists to borrow from | The outbox: `GET /v0/export` + ack cursor, monotonic seq, written at the same three commit points | `docs/control-api.md:75-76`, `:180-301` |
| CI recipe artifact base | Static musl x86_64 leg ships `hekma` + `hkm` + `hekma-host`, static-ness gated; x86_64-only (pin `runs-on`) | `.github/workflows/release.yml:36-43`, `:71-125` |
| Gallery validation machinery | `run_mock_conformance(dir)` validates a bare `adapter.toml` from an ordinary network-free cargo test — but the TCK execs what the manifest names, so CI validates against bundled fakes/shims, never real agents | `crates/hekma-conformance/src/tck.rs:528-543`, `tests/third_party_manifest.rs:24-77` |
| Honesty-table precedent | docs/agents.md already carries per-agent validation pins, metering tiers and the re-validation duty — the gallery generalizes this page's discipline | `docs/agents.md:18-52` |

**The finding that shapes expectations (the honest flank).** Across the ten seed agents,
billing-grade token coverage today is **1 of 10** (Hermes' self-reported sentinel). Six of ten
classifications rest on external docs dated 2026-10-23 (upstream ACP surfaces move fast); the
repo itself documents four (Hermes, OpenCode, Gemini CLI, Claude Code-via-third-party-adapter).
The gallery's honesty tables therefore carry upstream stability status and the re-validation duty,
and this epic promises metering honesty, not metering coverage — the OTLP exporter will faithfully
export honest dashes where no tier yields usage. Engine-observed adoption (base-URL override) per
agent is the follow-on value path, not a v1 promise.

---

## Section 2 — Impact Analysis (checklist 2.1–3.4)

**Epic impact.** Epics 18 and 19 are in flight and untouched: no story of Epic 20 edits the
lifecycle declarations, the fence, Task scope, the vectors, reconciliation, or the Control API
route module. Impact = **one new epic (20)**. The coupling with 18/19 is temporal, not textual:
20.1's `gen_ai.client.operation_duration` has no producer until 19.4 and `estimate_state:
reconciled` none until 19.5 — both degrade gracefully (absent, never fabricated; small follow-up
derivations inside `otlp/` when those stories land).

**Story impact.** Three new stories (20.1–20.3), batch-epic shape (the 10–14 precedent): scope
paragraphs here, full ACs derived at the spec stage.

**Artifact conflicts.**

- **PRD** — additive §4.13 (FR-46..FR-48), §3 glossary entries, §9.3 v1.x line — plus **one dated
  §6 amendment** resolving the PRD's own flagged assumption (`[ASSUMPTION: zero remote telemetry in
  v1 — flag any exception to Islam explicitly]` — this proposal is that flag). No §8 non-goal is
  touched (none of the eight covers observability). §14 assumption #18 resolves with it.
- **Architecture spine** — additive: **AD-21** (new), dated notes on **AD-20**, **AD-14** (clarifying
  only), **AD-8** (the estimate label crosses into telemetry). Deferred list gains seven items (§4.5).
- **Adapter Contract** — unchanged.
- **UI/UX** — none (terminal product; new docs pages only).
- **Secondary artifacts:** two new CI lanes (gallery validation; CI-recipe dogfood lane as separate
  workflow jobs, not edits to existing steps), new docs pages (`telemetry.md`, the CI recipe page,
  `docs/adapters/`), `deferred-work.md` entries (§4.6), Fibery mirror (§4.8).

**Technical impact.**

- The OTLP decision under NFR-8 is the load-bearing choice: the Architect recommendation is the
  hand-rolled OTLP/HTTP+JSON exporter (zero new runtime crates; house precedents AD-19's hand-rolled
  ACP client and 17-4's std-only SHA-256), with the otel-SDK path recorded as the standing
  alternative behind explicit reopen triggers (D1/AD-21).
- The metric exposition (names, types, attributes, units) becomes a **versioned public surface**
  (AI-55 two-pass review applies — a dashboard is as public and retract-proof as an install
  command).
- The known exporter trade-off is named, not hidden: the live bus is at-most-once in the crash
  window, so v1 counters can undercount across a lag/crash — drops are surfaced (`hekma.otlp.dropped`
  self-metric + restart note, AI-18) and ledger-sourced re-derivation is deferred with a trigger (§4.6).
- No new exit codes (DC-4 pattern); new diagnostics join existing classes.

---

## Section 3 — Recommended Approach (checklist 4.1–4.4)

**Option 1 — Direct adjustment: add Epic 20. SELECTED.**
- Option 2 (rollback) — nothing to roll back.
- Option 3 (MVP review) — the v1 MVP is shipped and unaffected; these are v1.x additions.

- **Effort: Medium, cleanly split.** 20.3 and 20.2 are docs + CI-lane work on shipped surfaces and
  can start immediately, in parallel with 18/19. 20.1 is the engineering story (one new host module
  + flag plumbing + a collector fixture in CI); it sequences after 19.1 merges, branching from main
  (never stacked on the epic-19 branch).
- **Risk: Low-Medium.** Parallel-safety is by construction (additive new module; the only shared
  files are `lib.rs`/`main.rs`/`cli/serve.rs` flag plumbing, which neither 18 nor 19 modifies).
  The real risks are expectation risks — metering coverage honesty (Section 1) and the §6 privacy
  amendment being read as scope creep — both handled by wording, not code.

**Execution order:**
1. **20.3 (gallery)** — immediately; content + CI lane, depends on nothing in flight.
2. **20.2 (CI recipe)** — immediately, parallel; its dogfood lane lands on main's CI.
3. **20.1 (OTLP export)** — after 19.1 merges, from main; its 19.4/19.5-dependent metric emissions
   land as small follow-ups inside `otlp/` when those stories merge. The spec stage picks whether
   v0 waits for 19.3/19.5 attribution or ships per-Run/cumulative first — FR-46 is written so both
   orders satisfy it.

---

## Section 4 — Detailed Change Proposals

### 4.1 Decisions to ratify with this proposal (AI-58)

**D1 — AD-21: the host owns a minimal OTLP/HTTP+JSON metrics exporter; the engine stays
telemetry-free (Architect draft, option (a) recommended).**

- Placement: a module inside `hekma-host` (`src/otlp/`), not a new crate, not in the engine. It
  subscribes read-only to the public event bus and derives every metric from the existing
  versioned AD-14 payloads. No new event kinds, no engine API change.
- Hand-rolled, **zero new runtime deps** (the `cargo tree` diff is empty): OTLP/HTTP+JSON is a POST
  of a JSON metrics batch to `<endpoint>/v1/metrics`, serialized with in-tree `serde_json` over the
  hyper+rustls client the metering channel already proved. Transport is HTTP+JSON, never gRPC
  (tonic/prost = the largest dep delta available); binary protobuf encoding is deferred
  (volume-triggered).
- Never blocks supervision: publish-after-commit comes free from the bus contract; a `Lagged(n)`
  becomes a surfaced drop count, never an error into supervision; each POST is bounded (AD-17); no
  durable telemetry persistence — in-flight points lost on crash are surfaced once at restart. The
  outbox stays the only durable/acknowledged export surface (AD-14 note).
- Naming: OTel GenAI semconv first (`gen_ai.client.token_usage` with `gen_ai.token.type`
  input/output/cached; `gen_ai.client.operation_duration` only once 19.4 exists — absent, never
  fabricated); `hekma.*` only where no semconv exists (`hekma.budget.breaches`, `hekma.agent.state`,
  `hekma.usage.cost`, `hekma.otlp.dropped`). **Every currency-bearing metric carries
  `estimate_state: estimated|reconciled`** — a reconciled figure is a new series, never a
  replacement (AD-8 extension).
- Config (one surface, both `hekma serve` and `hekma-host`): `--otlp-endpoint` (default: off — the
  exporter exists only when configured, like `--listen`), `--otlp-headers` with `secret:NAME` values
  resolved at startup through `SecretResolver` (unresolvable = typed startup refusal; AD-10),
  `--otlp-flush-interval` (bounded, default 10s). Cumulative temporality with an honest
  restart-reset, stated in docs.
- Exposition versioned under an `otlp-metrics v1` marker in `docs/telemetry.md`, additive-only-until-
  frozen discipline; renames/drops are versioned release-surface changes (AI-55).
- Alternatives recorded: **(b) otel SDK** — rejected for v1, standing alternative; reopen triggers:
  a real traces consumer (post-19.4) or a vendor must-have the hand-rolled path cannot meet. If a
  second consumer ever appears, that is the moment for a `TelemetrySink` port. **(c) Prometheus
  scrape endpoint** — rejected by Islam 2026-10-23 (standard OpenTelemetry chosen); recorded: it
  would have bought a zero-config pull reader on the Control API at the cost of a second exposition
  format and no traces path. An OTel Collector in front of Hekma can still bridge to Prometheus.

**D2 — §6 Privacy amendment (PM draft, verbatim for ratification — the one non-additive change):**

> *(Amended 2026-10-23.)* The default stands: nothing leaves the machine unless the Operator or
> Host configures it to. One sanctioned exception — FR-46's OTLP metrics export: disabled unless a
> collector endpoint is explicitly configured; metrics only (counters and gauges derived from the
> Usage Ledger and supervision state); never request or response content, never secrets (FR-14).
> Ratified with `sprint-change-proposal-2026-10-23-adoptability.md`. §14 #18 resolves as: *zero
> remote telemetry by default; one opt-in, metrics-only, operator-endpointed exception.*

**D3 — Gallery validation model + CI-recipe scope.**
- Gallery CI validates every manifest against the conformance kit with bundled fake/shim execs (the
  `hermes_shim` precedent) — CI never launches real agents (the TCK execs what the manifest names;
  network-free is a CI invariant). Real-agent verification rides the existing docs/agents.md
  discipline: run-verified pins where a machine hosts the binary, honestly named gaps otherwise.
- The CI recipe ships in-repo: a docs recipe page + a dogfood lane in this repository's own CI. A
  separate public demo repo would need its own authorization and is not proposed here. The
  composite marketplace Action is deferred out of v1 (trigger: a third-party user asks).

### 4.2 PRD — `_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md`

(a) Frontmatter `updated: 2026-10-23`; (b) §3 gains the OTLP/Telemetry Export and Adapter Gallery
glossary lines; (c) §6 gains D2's dated blockquote; §14 #18 gains the resolution note; (d) new
§4.13 Adoptability with **FR-46 OTLP telemetry export** (opt-in/off-by-default enforced by test;
every exported dollar labeled estimated|reconciled; versioned exposition; no secrets/content — the
no-leak matrix extends to the export path; visible degradation, never delayed supervision),
**FR-47 Agents-in-CI recipe** (spend capped per run with the Breach Action inside the FR-21
latency bound and no path masks a breach as success; wall-clock bounded with the bound named;
usage into `$GITHUB_STEP_SUMMARY` from `--json`, labeled estimated; FR-26 exit-code honesty;
dogfooded as a lane in this repository's CI), **FR-48 Adapter gallery** (ten seed entries each a
validated manifest or honestly named gap; honesty tables in the Capability Declaration vocabulary
with explicit absence tokens; every manifest validated in CI by `hekma-conformance`; the honest
not-available marker naming the tier where no metering yields; no planning-provenance wording in
public pages; SM-C1 respected); (e) §9.3 gains: FR-46..FR-48 (Epic 20, added 2026-10-23) are v1.x
additions. Full house-style FR text (statement + testable consequences) is drafted and ready to
apply — see the PM pass distilled in the companion analysis record.

### 4.3 Epics — `_bmad-output/planning-artifacts/epics.md`

(a) Requirements Inventory: after FR-45, one-liners for FR-46..FR-48. (b) FR Coverage Map:
FR-46: Epic 20 (20.1) · FR-47: Epic 20 (20.2) · FR-48: Epic 20 (20.3). (c) Epic List entry
(newest-first):

> ### Epic 20: Adoptability *(opened 2026-10-23 via correct-course)*
> Opt-in OTLP metrics export into existing observability stacks, a budget-capped agents-in-CI
> recipe dogfooded in this repository's CI, and a conformance-validated adapter gallery with
> honesty tables. **FRs covered:** FR-46, FR-47, FR-48.

### 4.4 The epic section (appended to `epics.md`)

````markdown
## Epic 20: Adoptability

> **(2026-10-23 — opened via correct-course: `sprint-change-proposal-2026-10-23-adoptability.md`.)**
> The brainstorm trigger: Hekma runs agents well, but a newcomer's first hour lacks the artifacts
> adoption runs on — governance metrics do not feed an existing Grafana/Datadog stack, there is no
> recipe for running a governed bot in CI, and no validated index of which agents run under Hekma
> today. Islam approved the epic 2026-10-23 (standard OTLP; gallery in docs; the ten-agent seed
> list). Realizes FR-46, FR-47 and FR-48. Purely additive: this epic does not disturb Epics 18 or
> 19 — no story touches the lifecycle declarations, the fence, Task scope, the vectors, or
> reconciliation beyond reading their outputs. Batch-epic shape (the 10–14 precedent): full ACs
> are derived at the spec stage. 20.1 is a public-exposition + privacy-surface story: AI-55
> two-pass review applies.

### Story 20.1: OTLP telemetry export (FR-46, AD-21)
- Opt-in, off by default: a configured collector endpoint turns export on and nothing else does —
  no configuration, no egress, asserted by test (the §6 amendment's default).
- Hand-rolled OTLP/HTTP+JSON exporter in `hekma-host` (`src/otlp/`), zero new runtime deps (NFR-8);
  reads the public event bus read-only; derives metrics from the AD-14 payloads; never blocks
  supervision (bounded POSTs, surfaced drops, `hekma.otlp.dropped`).
- Semconv-first naming (`gen_ai.client.token_usage`, `gen_ai.client.operation_duration` once 19.4
  exists) with `hekma.*` only where no semconv exists; every currency metric carries
  `estimate_state: estimated|reconciled` (AD-8 extension).
- Config on one surface (`--otlp-endpoint`, `--otlp-headers` with `secret:NAME`, bounded flush
  interval); versioned exposition documented in `docs/telemetry.md` (`otlp-metrics v1`).
- No content, no secrets: the FR-14/FR-43 no-leak matrix extends to the export path.
- Grafana/Datadog ingest proven in CI against a collector fixture, not asserted from spec
  compliance.

### Story 20.2: Agents in CI — the GitHub Actions recipe (FR-47)
- A docs recipe page: a budget-capped agent lane (Token Budget + Cost Cap, Breach Action
  configured) driving a review-style task under `hekma`, on the static musl artifact (x86_64
  runners pinned).
- Wall-clock bounded (`timeout-minutes`); a lane that ends on a bound says which one — budget or
  clock — in the job output.
- Usage into `$GITHUB_STEP_SUMMARY`, rendered from `--json`: tokens and derived dollars, labeled
  estimated (FR-22/FR-23).
- Exit-code honesty (FR-26): success, breach and agent failure are distinguishable; no path masks
  a breach as success.
- Secret plumbing is first-class in the recipe (auth-gated handshakes refuse at `-32000` when
  logged out — the recipe must show `secret:` wiring, not footnote it).
- Dogfood proof: a Ktesio bot lane runs in this repository's own CI under the recipe — the lane is
  the recipe's test.
- The composite marketplace Action is deferred (trigger: a third-party user asks).

### Story 20.3: The adapter gallery (FR-48)
- Ten seed entries — Copilot CLI, Hermes, OpenCode, Claude Code, Codex, Gemini CLI, goose, Crush,
  Aider, Cursor CLI — each a validated manifest or an honestly named gap, under `docs/adapters/`.
- The honesty table per entry, in the Capability Declaration vocabulary (guaranteed / best-effort /
  unsupported) with explicit absence tokens — never a fabricated pass or implied zero (AI-18);
  tables carry upstream stability status and the re-validation duty (the docs/agents.md
  discipline, generalized).
- A CI validation lane runs `hekma-conformance` over every gallery manifest against bundled
  fake/shim execs (the `hermes_shim` precedent) — CI never launches real agents; a failing
  manifest, or an entry claiming more than its validation proves, fails the build.
- Metering honesty: where no tier yields usage, the entry shows the not-available marker naming
  the tier (the FR-40 precedent), never a blank.
- Public pages carry no planning-provenance wording — enforced by the `check_docs` gate; the
  authoring guide cites `docs/adapter-contract.md` (the conformance crate has no README).
- Additions respect SM-C1: an entry ships only when its controls behave uniformly.
````

### 4.5 Architecture spine — `ARCHITECTURE-SPINE.md`

(a) Frontmatter `updated: '2026-10-23'`; `binds` gains FR-46..FR-48. (b) New **AD-21** after AD-20,
text = D1 above in house format (Binds: FR-46, AD-2, AD-8, AD-10, AD-14, AD-17/18, AD-20, NFR-8;
Prevents: the heaviest dependency in workspace history for a metric-only v1, telemetry
back-pressure stalling supervision, an unlabeled dollar figure reaching a dashboard, a second
event dialect). (c) Dated amendment notes: **AD-20** (the host may also run the read-only OTLP
exporter beside the Control API — same boundary, never blocks supervision), **AD-14** (clarifying:
the exporter is a third *consumer* of the one event schema, not an export view and not an outbox
consumer; the outbox remains the only durable/acknowledged surface), **AD-8** (the estimate label
survives into telemetry; reconciled is a new series, never a replacement). (d) Deferred list
gains: full otel SDK/gRPC transport (behind AD-21's reopen triggers); traces/spans + a spans view
(post-19.4 consumer); Prometheus scrape endpoint; per-agent collector routing; OTLP binary
protobuf encoding; host self-metrics; exemplars/configurable temporality; ledger-sourced exporter
re-derivation (exact cumulative accuracy after a lag/crash — trigger: a consumer needs
lossless-grade metrics rather than surfaced-drop honesty).

### 4.6 `_bmad-output/implementation-artifacts/deferred-work.md`

New section `## Deferred from: sprint-change-proposal-2026-10-23-adoptability (correct-course, Batch)`:
1. Composite marketplace GitHub Action (recipe ships as workflow YAML first). Trigger: a
   third-party user asks.
2. Full otel SDK / gRPC transport. Trigger: a real traces consumer (post-19.4) or a vendor
   must-have the hand-rolled path cannot meet.
3. Prometheus native scrape endpoint. Trigger: a pull-model minority asks after the
   collector-bridge is documented.
4. Per-agent collector routing (per-instance endpoints / tenant headers). Trigger: a multi-tenant
   Host needs it.
5. Ledger-sourced exporter re-derivation (lossless cumulative accuracy). Trigger: a metrics
   consumer requires it despite surfaced-drop honesty.
6. Gallery install UX (`hekma adapter add <name>` fetching checksummed manifests). Trigger:
   gallery traffic justifies a submission+install flow.

### 4.7 `_bmad-output/implementation-artifacts/sprint-status.yaml`

In `development_status`, add (after the epic-19 block):

```yaml
  epic-20: backlog  # opened 2026-10-23 by sprint-change-proposal-2026-10-23-adoptability.md (Adoptability: OTLP export FR-46/AD-21; CI recipe FR-47; adapter gallery FR-48; §6 amendment D2; does not disturb epics 18/19)
  20-1-otlp-telemetry-export: backlog
  20-2-agents-in-ci-the-github-actions-recipe: backlog
  20-3-the-adapter-gallery: backlog
  epic-20-retrospective: optional
```

The header's `last_updated` refreshes, and the epic-20 rows note the numbering guard below.

**Numbering guard:** Epic 20 is free on local main and origin/main as of 2026-10-23 (highest =
19). Epics 18/19 are being worked elsewhere; if any parallel tranche consumes the number 20
before this proposal lands, the 2026-10-19 renumber protocol applies (land renumbered; this
document keeps the original as the historical record).

### 4.8 Fibery mirror (per `AGENTS.md`, in the same change)

In Ktesio → Product Development → the Hekma Product: one Feature (Epic 20) + three Tasks
(Story 20.1–20.3) keeping BMAD numbering and linking this proposal; the six §4.6 deferrals under
"Hardening & Housekeeping Backlog".

---

## Section 5 — Implementation Handoff

**Scope: Moderate** — one new epic, additive PRD/spine changes, one dated §6 amendment. The
architect touch (AD-21 + amendment notes) is drafted here for ratification in place (AI-58).

| Recipient | Responsibility |
| --- | --- |
| **Product Owner (Islam)** | Ratify the proposal, D1 (AD-21 option (a) vs (b)) and D2 (§6 amendment wording); confirm D3's in-repo CI-recipe scope. Pick 20.3/20.2 start. |
| **Architect** | At 20.1's spec stage: the exporter module design against AD-21 and the v0-ordering pick (wait for 19.3/19.5 attribution vs ship per-Run/cumulative first). |
| **Developer** (batch-epic flow) | 20.3/20.2 immediately in parallel with 18/19; 20.1 after 19.1 merges, from main. All four house gates per story, ≥95% coverage, docs in the same change; AI-55 two-pass on 20.1 (public exposition + privacy amendment) and on the gallery's public claims. |

**Success criteria:**
1. An operator points `--otlp-endpoint` at their collector and sees Hekma's fleet metrics in
   Grafana/Datadog — tokens by type, breaches, state, labeled cost — with no content or secrets in
   any payload; unset, nothing dials out (test-proven).
2. A newcomer copies the CI recipe and gets a budget-capped, wall-clock-bounded agent lane whose
   step summary shows tokens and estimated dollars, and whose exit code names what ended it; this
   repository's own CI runs one such lane.
3. The gallery's ten entries each carry a validated manifest or an honestly named gap; the CI lane
   fails on an invalid manifest or an overclaiming page; no table cell fabricates a pass.
4. Epics 18/19 land unaffected — zero merge conflicts from Epic 20 work (the shared flag files land
   in one commit, before parallel work resumes).
5. The frozen v1 surfaces (CLI, JSON, exit codes, adapter contract) show only additive, announced
   changes; the semver gate stays green.

---

## Approval

**APPROVED by Islam 2026-10-23** (Batch mode): the proposal as a whole, D1 = option (a) the
hand-rolled OTLP/HTTP+JSON exporter, D2 = the §6 amendment verbatim, D3 = in-repo CI recipe.
Applied the same day on branch `docs/adoptability`: PRD (§3, §4.13, §6, §9.3, §14 #18), epics
(inventory, coverage map, Epic List, the Epic 20 section), spine (AD-21 + the 2026-10-23
amendment notes + Deferred), deferred work (§4.6, six entries), tracker (§4.7 — validated with
the house parser), and the Fibery mirror (§4.8). The companion analysis record (Analyst/
Architect/PM passes with full citations) is preserved with the applying session and summarized
in Section 1.
