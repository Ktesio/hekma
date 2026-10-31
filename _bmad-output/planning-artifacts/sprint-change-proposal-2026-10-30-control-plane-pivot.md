# Sprint Change Proposal — Control-Plane Pivot

**Date:** 2026-10-30 · **Trigger owner:** Islam (strategic pivot, brainstorm session 2026-10-29) · **Mode:** Batch
**Route:** Correct Course → PRD and spine re-baseline, Epic 19 re-scope, new Epics 21–25 · **Scope classification:** Major
**Status:** APPROVED by Islam 2026-10-30, as recommended: D1(a) `warn` default, D2(a), D3(a), D4 left open for
22.3's spec, D5(a). Sprint-status updated the same day (Section 4.4); PRD, spine and epics.md follow via the handoff.

**Input:** `_bmad-output/brainstorming/brainstorm-hekma-value-proposition-pivot-2026-10-29/brainstorm-intent.md`
(accepted value proposition, seven principles, next horizon A/B/C/G-core, later D/E/F, open questions).
**Analysis provenance:** one read-only analysis pass over the PRD (with addendum), ARCHITECTURE-SPINE, epics.md,
sprint-status.yaml, the 2026-09-30 and 2026-10-23 proposals, host-mode-decisions, `docs/commands.md`,
`docs/telemetry.md`, `docs/control-api.md`, the crates and `origin/main`. Key citations were spot-checked by hand.

---

## Section 1 — Issue Summary

**Problem.** Hekma's adoption story leads with cost enforcement: token and dollar budgets that pause or stop an
agent. That is not enough reason to adopt it. The agents it supervises today (OpenCode, Hermes, ACP tools) are
individual-level tools, while businesses build agent workloads with in-process frameworks (CrewAI, LangGraph)
and fear exactly what Hekma does by default: a production workflow paused on a cost threshold.

**Decision from the trigger session.** The accepted value proposition:

> Hekma is the control plane for agent workloads — one way to run, see and steer any agent (CrewAI, LangGraph,
> Hermes, OpenCode or a raw model call), from a laptop to a fleet, without ever being the reason prod stops.

**Principles set by Islam:** signal, don't kill (make it loud); run duration never changes how a run behaves;
callers cannot override non-negotiables; one consistent interface regardless of agent tech; scale to millions of
runs; CLI for emergencies, API + OTel for daily work; scent check off / shadow / enforce.

**Continuity, not reversal.** PRD §1 already says "consistency itself is the product. Agents become
interchangeable workloads." The pivot keeps that bet and widens *what* counts as a workload (in-process framework
runs, raw model calls) and *where* it runs (not only processes Hekma spawned). Everything shipped stays valid as
the deepest control rung, "supervised".

**Evidence.** User pains recorded in the session: Hermes bypasses Hekma by calling the provider directly;
LiteLLM shows usage but cannot control a specific session; framework teams cannot adopt a tool that may pause
prod; a Hekma restart stops every owned run.

## Section 2 — Impact Analysis

### 2.1 Capability map (next horizon)

| # | Capability | Today | Evidence |
|---|---|---|---|
| A1 | Same verbs for any runtime (`ps/logs/top/pause/resume/stop/budget`) | Partial | Uniform over `acp`, manifest and hermes kinds (FR-4..8, FR-22, FR-25); only for processes Hekma spawned; no `ps/top/budget` |
| A2 | Swap-the-engine portability | Partial | Instance name is the stable key (FR-46 labels); no flow identity independent of the runtime |
| A3 | `hekma top` live TUI | Gap | SSE `/v0/events` (17-3) exists; tokens/s and burn rate need 19-4 |
| A4 | Complement gateways (identity on every forwarded call) | Partial | `metering.upstream_base_url` can target LiteLLM/Portkey; no identity header added |
| B1 | Control-depth ladder + CONTROL column | Partial | Everything today is "supervised"; Supervision column (15-2) |
| B2 | Wire hold / release / reroute / reject | Reject planned (19-2); rest Gap | Pause is SIGSTOP or cooperative (FR-7, 18-4); multi-route upstreams Deferred |
| B3 | Self-announce | Gap | Registration is operator-driven (FR-1) |
| B4 | Leases, `silent` status | Partial | Liveness → `failed`/`unresponsive` (FR-42); no `silent` |
| C1 | Per-run key, refresh only by the original process | Gap | Run id (AD-7, 15-2) is not a credential |
| C2 | Key custody (virtual provider keys) | Gap | Listener forwards the agent's own `Authorization` |
| C3 | Bypass detection | Gap | FR-43 / NFR-6 accept bypass as cooperative; attested egress Deferred |
| C4 | Scent check off / shadow / enforce | Gap | Nearest: Breach Action `warn` |
| G1 | Shim per run | Gap | `--detach` + AD-5 adoption is the nearest |
| G2 | Durable registry | Runs, budgets, spend shipped; leases and holds Gap | AD-5, AD-6, 17-4 |
| G3 | Zero-downtime daemon restart/upgrade | Gap, conflicts | FR-41: SIGTERM stops every Agent Instance |
| G4 | Nodes + hub (tailnet) | Gap, conflicts | Control API loopback-only (AD-20); §8 "no hosted control plane" |
| G5 | Selectors, pagination, filtering | Gap | `hekma agent list` takes no filters; no labels |
| G6 | Exception-first views | Gap | — |
| G7 | OTel cardinality discipline | Partial, conflicts | `hekma.run_id` is a metric attribute (AD-21, telemetry.md); traces Deferred |

Later themes, for the record: D (policy) — live budgets shipped (FR-18), Governance Vectors shipped (FR-44), typed
refusal planned (19-2), windowed budgets Deferred; E (customer safety) — all Gap; F (proof) — OTLP shipped,
append-only ledger and outbox exist but are not hash-chained, approval routing Deferred. **Naming collision for
E:** the existing "outbox" (17-4) is the event export journal; the customer-facing outbox needs another name.

### 2.2 Epic impact

- **Epics 1–7, 9–17, 20:** done, unaffected. They are the supervised rung.
- **Epic 18:** all five stories merged (#335, #336, #338, #339, #342); tracker drift only.
- **Epic 19 (in progress):** 19-1 merged (#334). 19-2 changes (becomes the reject rung under policy modes,
  never a default). 19-3 is absorbed by the Run entity (21.1). 19-4 is kept, extended with run identity, and
  becomes a prerequisite for `top` and reroute. 19-5 unchanged.
- **New Epics 21–25** cover the A/B/C/G-core gaps (Section 4.3).

### 2.3 Artifact conflicts

| # | Plan-of-record text | Conflict |
|---|---|---|
| 1 | PRD §3 Breach Action: "`pause` (default) … `[FIXED — pause default ratified by Islam 2026-07-02.]`"; FR-21 | Signal, don't kill. Reverses a FIXED decision (D1) |
| 2 | FR-41: "On SIGTERM the host drains and stops every Agent Instance"; AD-20 | Hekma must never be the reason prod stops |
| 3 | PRD title/§1 "Unified Personal-Agent Runtime Engine"; §2.2 Non-Users; §8 "Not team/multi-user governance", "No hosted control plane"; AD-20 | Positioning and audiences change |
| 4 | §3 Glossary "Agent" = personal or coding agent program; AD-7 Run = span of one Agent Instance process; FR-1 operator-only registration; FR-19 metering mandatory at registration | Runs inside frameworks, raw calls, observed and registered runs |
| 5 | AD-7 "loopback-only HTTP forward listener" | Cloud functions and VMs cannot reach it; hub cannot federate |
| 6 | AD-18 "the engine is not in the agent request path" | Wire hold and reroute put it there |
| 7 | AD-17/18 coarse two-mutex; NFR-4 sized at a 25-instance Fleet; one SQLite; no pagination | Millions of runs (AD-18 reopen trigger 4 fires) |
| 8 | AD-21 `hekma.run_id` as a metric attribute | Cardinality discipline; needs `otlp-metrics v2` |
| 9 | PRD §1 "`kt` is the engine's first and complete consumer"; Control API "unstable 0.x" | API + OTel become the daily surface |
| 10 | FR-43 / NFR-6 "the fence is as cooperative as the meter" | Key custody and bypass detection replace that stance |
| 11 | FR-18 / Control API: any state-dir user or bearer can raise ceilings | Callers cannot override non-negotiables (full fix is D, later) |
| 12 | PRD, Glossary, NFR-8, spine still say "Ktesio"/"kt" | Cosmetic, but misleads new readers |

No UX specification exists (3.3 N/A). Secondary artifacts: `README.md`, `docs/commands.md`, `docs/telemetry.md`,
`docs/control-api.md` and the product page in the tracker mirror carry the old framing.

## Section 3 — Recommended Approach

**Direct Adjustment, preceded by a PRD and spine re-baseline. Scope: Major.**

- **Rollback: not viable.** Shipped work is the supervised rung and the substrate everything builds on.
- **MVP Review: not needed.** The v1 MVP is shipped and stays valid; nothing is cut.
- **Why Major:** it repositions the product (title, vision, audiences, non-goals), reverses a FIXED decision,
  amends five spine invariants (AD-7, AD-18, AD-20, AD-21, and the FR-41 consequence) and adds three ADs.
- **Effort:** High overall, but sliced: Epic 21 is small and unblocks everything. **Risk:** Medium; the
  riskiest pieces (wire hold, shim, hub) come last and each has a reopen point.
- **Timeline:** Epic 19 continues (19-2 and 19-4 first, since an external Host consumer waits on them);
  Epic 21 starts in parallel once the re-baseline is approved.

## Section 4 — Detailed Change Proposals

### 4.1 PRD (`prds/prd-ktesio-2026-07-02/prd.md`) — via bmad-prd edit

**P1 · Title and §1 Vision**

OLD: `# PRD: Ktesio — Unified Personal-Agent Runtime Engine`
NEW: `# PRD: Hekma — the Control Plane for Agent Workloads`

§1: keep paragraphs 1–3 (the consistency bet), renamed Ktesio → Hekma and `kt` → `hekma`, and add:

> Hekma is the control plane for agent workloads — one way to run, see and steer any agent (CrewAI, LangGraph,
> Hermes, OpenCode or a raw model call), from a laptop to a fleet, without ever being the reason prod stops.
> Control comes in depths: Hekma can observe a run, govern its traffic on the wire, accept its registration, or
> supervise its process; every run shows which depth applies. The CLI serves individuals and emergencies; the
> Control API and OTLP telemetry are the daily surface for teams.

Rationale: the accepted proposition; widens "workload" without dropping the consistency bet.

**P2 · §2 Target User and §2.2 Non-Users**

ADD to §2: *Framework teams* — engineers running CrewAI/LangGraph (or similar) flows in production who need
uniform control and telemetry with little or no integration work, and no unexpected shutdowns.
OLD §2.2: "Teams wanting shared policy, org budgets, RBAC, or audit — that governance layer belongs to a Host,
not to Ktesio." / "Users wanting a hosted dashboard or SaaS — Ktesio ships a CLI and an embeddable engine only."
NEW §2.2: "Users wanting a hosted SaaS or a web dashboard — Hekma ships a CLI, an embeddable engine, a
self-hosted Control API and OTLP telemetry; dashboards live in the operator's observability stack. Org RBAC and
audit evidence are planned later (themes D/F), not v-next."

**P3 · §3 Glossary**

- **Agent** NEW: "a third-party agent workload that Hekma governs: an agent program (personal or coding agent),
  an agent flow inside a framework process (e.g. CrewAI, LangGraph), or a direct model-calling client."
- **Run** ADD (closes the AD-7 open item): "one unit of agent work with a stable run id, a control depth
  (observed / wire / registered / supervised), labels, and budgets. A supervised Agent Instance's process span
  is one Run; one process may host many Runs."
- **Control Depth** ADD: the four rungs above.
- **Run Key** ADD: "a short-lived per-Run credential; refreshable only by the process that registered the Run."
- **Breach Action** OLD: "`pause` (default), `stop`, or `warn`. `[FIXED — pause default ratified by Islam
  2026-07-02.]`" NEW: "`warn` (default), `pause`, `stop`, or `block`. `[FIXED — warn default ratified by Islam
  2026-10-30, superseding the 2026-07-02 pause default.]`" (D1a, approved)

**P4 · FR-21 consequence** OLD: "with `pause` the ratified shipped default" NEW: "with `warn` the ratified
default; a breach always emits a loud signal (event, `ps` anomaly, OTLP counter, response header where the
wire allows) whatever the action." (D1a, approved)

**P5 · FR-41 consequence** OLD: "On SIGTERM the host drains and stops every Agent Instance within a bounded
window" NEW: "On SIGTERM the host drains and stops every Agent Instance within a bounded window, unless the
host runs in handoff mode (FR-55), in which case Runs stay up under their shims and a restarted host
reattaches."

**P6 · FR-1 / FR-19 notes.** FR-1 ADD: "Runs may also be created by self-announce (FR-52) or wire discovery
(FR-51)." FR-19 ADD: "Metering at registration remains mandatory for supervised Agent Instances; observed and
registered Runs are metered on the wire when their traffic passes through Hekma."

**P7 · New FRs (stubs for the PM to write in full)**

| FR | Title | Theme |
|---|---|---|
| FR-49 | Runs as first-class entities with control depth and labels | A, B |
| FR-50 | `hekma ps`, selectors on every verb, pagination, exception-first views | A, G |
| FR-51 | Wire-discovered Runs through a shared proxy endpoint | B |
| FR-52 | Self-announce and leases (`silent` status) | B |
| FR-53 | Run keys, virtual provider keys and scent-check modes | C |
| FR-54 | Bypass detection | C |
| FR-55 | Shim per Run and zero-downtime host restart/upgrade | G |
| FR-56 | Wire hold, release and reroute | B |
| FR-57 | `hekma top` live view | A |
| FR-58 | Nodes and hub | G |

**P8 · NFR-4** ADD: "Run-scale budgets replace the 25-instance placeholder after the Epic 24 benchmark: read
paths paginated and indexed; targets measured at 10k / 100k / 1M Runs."

**P9 · §8 Non-Goals** (closes §13 Q5) NEW list:
- Not an agent framework or authoring toolkit. (kept)
- Not an LLM/model server. (kept)
- **Not a hosted SaaS or web UI.** Hekma is a self-hosted control plane, OTel-first and UI-optional.
- **Org governance (RBAC, audit evidence, four-eyes) is later, not v-next** (themes D/F).
- No provider-side billing enforcement. (kept)
- No deep per-agent feature parity. (kept)
- No general-purpose skills package management. (kept)
- No security sandboxing claim (NFR-6), **except** key custody and bypass detection as specified in FR-53/FR-54.

**P10 · §6 Privacy.** Traces/spans (Epic 24) and hub federation (Epic 25) each need a new dated amendment when
their epic starts; nothing changes now.

### 4.2 Architecture (`ARCHITECTURE-SPINE.md`) — via the Architect

| Change | What |
|---|---|
| **AD-7 amend** | Run = PRD Glossary Run (not only an Instance process span). The forward listener stays loopback by default; an opt-in authenticated non-loopback bind (TLS + run-key auth) is allowed. |
| **AD-18 amend** | Reopen trigger (4) "sustained fleet-scale growth" fires. The engine may sit in the request path for per-Run dispositions (pass / hold / reroute / reject), held lock-free so agent slowness still never holds engine locks. Activate the archived per-instance plan, Steps 0–4. |
| **AD-20 amend** | "Not a hosted control plane" becomes "a self-hosted control plane, never a hosted service". Add handoff mode: SIGTERM in handoff mode leaves Runs under their shims. |
| **AD-21 amend** | `hekma.run_id` leaves metric attributes for exemplars/traces; flow and policy labels go on metrics. Versioned `otlp-metrics v2` under the two-pass review rule. |
| **AD-22 new** | Run keys and key custody: minting, hashing at rest, refresh bound to the originating process, the listener swaps a run key for the real provider key from `SecretResolver`; scent modes off / shadow / enforce. |
| **AD-23 new** | Shim per Run: a small per-Run process owns the child, its pipes and log capture; the host reattaches over a socket or named pipe. Amends AD-4, AD-5 and AD-12 accordingly, per OS. |
| **AD-24 new** | Nodes and hub: local rollups, hub merge, hub-routed verbs with selectors, per-node auth. Drafted with Epic 25, not now. |
| **Deferred list** | Reactivate "multi-route upstreams" (reroute) and "traces/spans"; "privilege separation and attested egress" is superseded by AD-22 plus bypass detection. |

### 4.3 Epics (`epics.md`)

**Epic 19 amendments**
- **19-2 the wire-fence `block` action:** ADD AC "`block` is never a default; it obeys the scent and policy
  mode (shadow logs what it would refuse)." Keeps its lock-free flag design; becomes the reject rung of 23.4.
- **19-3 Task scope:** REMOVE; absorbed into 21.1 (one Run concept, no Task/Run split). Its latch re-arm moves
  to 21.4.
- **19-4 per-call records:** ADD AC "every record carries run id, control depth and, once Epic 22 lands, the
  run-key identity." Moves ahead of 19-5.
- **19-5:** unchanged.

**Epic 21 — One Control Plane** (A, B1, G5, G6)
- 21.1 Run as a first-class entity (run table, labels, control depth; absorbs 19-3; ships the D1 breach default
  and loud signals)
- 21.2 `hekma ps` (CONTROL column, labels, `--selector`, `--limit/--page`, `--json`, exception-first, `--all`)
- 21.3 Selectors on every verb with multi-target dry-run and confirmation; Control API parity
- 21.4 `hekma budget` get/set/raise over selectors (raising re-arms latches)
- 21.5 `hekma top` live TUI (needs 19-4)
- 21.6 Gateway complement: identity headers on forwarded calls; LiteLLM/Portkey recipes

**Epic 22 — Run Identity and Key Custody** (C)
- 22.1 Run-key minting and refresh (AD-22)
- 22.2 Virtual provider keys in the listener
- 22.3 Scent-check modes off / shadow / enforce
- 22.4 Bypass detection, honest per OS
- 22.5 Authenticated non-loopback proxy endpoint (AD-7 amendment)

**Epic 23 — Attach and Wire Control** (B)
- 23.1 Wire-discovered Runs
- 23.2 Self-announce (header, OTel attribute or `POST /v0/runs`); thin CrewAI/LangGraph helpers validated on
  real flows
- 23.3 Leases and the `silent` status
- 23.4 Wire hold / release / reroute / reject (reject = 19-2 generalized; needs 19-4)

**Epic 24 — Operability at Scale** (G-core)
- 24.1 Shim per Run (AD-23)
- 24.2 Handoff mode: zero-downtime host restart and upgrade
- 24.3 Registry completeness: leases, holds, dispositions journaled; kill -9 mid-hold recovery test
- 24.4 Scale substrate: AD-18 Steps 0–4, indexed run tables, paginated reads, 10k/100k/1M benchmark
- 24.5 OTel cardinality and traces (`otlp-metrics v2`)

**Epic 25 — Nodes and Hub** (G4): node rollup endpoint, hub merge, hub-routed verbs, per-node auth (AD-24).
Planned last in the horizon; detailed when Epic 24 closes.

**Dependencies:** 21.1 first → 21.2/21.3 → 22.1 → 19-4 → 22.2/22.3 → 23.1–23.3 → 21.4/21.5 → 23.4 →
22.4/22.5 → 24.x → 25.

> **Post-approval note (2026-10-30):** the PRD and spine re-baselines added foundations this list did not have (store writer, Policies, CLI auto-start, inbound OTLP, bypass detection in the Shim). `epics.md` now holds the authoritative, renumbered story list and build order for Epics 19 and 21–25; 19-3 is absorbed into 21.2 (child Runs), and 19-2's AC follows the PRD ruling that `block` always refuses once configured.

### 4.4 Sprint status (`implementation-artifacts/sprint-status.yaml`)

- Drift fixes: `epic-18` and `18-1`..`18-5` → `done`; `19-1` → `done`; `19-3` → removed (absorbed); header
  comment and `last_updated` aligned to ISO; trailing "epic-20 open" note removed; `story_location` pointed at
  this repo.
- Add `epic-21`..`epic-25` and their stories as `backlog`, each with `-retrospective: optional`.

### 4.5 Secondary artifacts

`README.md` and the docs (`commands.md`, `telemetry.md`, `control-api.md`) take the new framing as each epic
lands, not up front. The tracker mirror (epics, stories, product page) mirrors the approved epics.

## Section 5 — Implementation Handoff

**Scope: Major.**

| Role | Responsibility | Deliverable |
|---|---|---|
| PM (bmad-prd edit) | P1–P10 | PRD re-baselined, status `review` → approved |
| Architect | AD-7, AD-18, AD-20, AD-21 amendments; AD-22, AD-23 drafted (AD-24 with Epic 25) | Spine amended, decisions ratified |
| PO / SM | Epic 19 amendments, Epics 21–25 appended, sprint-status fixed | epics.md and sprint-status.yaml updated in one change |
| Developer | Continues 19-2 and 19-4 with the amended ACs; starts 21.1 after the re-baseline | Stories via bmad-build |

**Success criteria:** the PRD and spine state the control-plane proposition with no contradiction left in
Section 2.3; every A/B/C/G-core capability in Section 2.1 maps to a story; sprint-status matches `origin/main`.

### Decisions (approved 2026-10-30: D1a, D2a, D3a, D4 open, D5a)

- **D1 — Breach default.** (a) `warn` + loud signals by default, `pause`/`stop`/`block` opt-in *(recommended,
  per "signal, don't kill")*; (b) keep `pause`. Changing it alters behaviour for existing users, so it ships as a
  documented default change in the release that carries 21.1.
- **D2 — Epic 19.** (a) keep 19-2 and 19-4 in Epic 19 with amended ACs, fold 19-3 into 21.1 *(recommended: an
  external Host consumer waits on 19-2)*; (b) fold 19-2 into 23.4 as well.
- **D3 — Hub.** (a) its own Epic 25, planned last *(recommended)*; (b) inside Epic 24.
- **D4 — Scent-check defaults per environment.** Open; decide in 22.3's spec (proposal: off for a laptop CLI,
  shadow under `hekma serve`, enforce opt-in).
- **D5 — Ktesio → Hekma rename in the PRD and spine.** (a) in the title and vision now, full sweep as separate
  housekeeping *(recommended)*; (b) full sweep in this change.

## Checklist status

| Item | Status | Note |
|---|---|---|
| 1.1 Trigger | [x] | Strategic pivot from the 2026-10-29 brainstorm; no single story |
| 1.2 Problem type | [x] | Strategic pivot / market change |
| 1.3 Evidence | [x] | Intent doc; user pains in §1 |
| 2.1 Current epic | [!] | Epic 19 re-scoped (D2) |
| 2.2 Epic-level changes | [x] | Epics 21–25 added |
| 2.3 Remaining epics | [x] | None planned after 20 |
| 2.4 Obsolete / new | [x] | 19-3 absorbed; five new epics |
| 2.5 Order | [x] | Section 4.3 dependencies |
| 3.1 PRD | [!] | P1–P10; D1 reverses a FIXED decision |
| 3.2 Architecture | [!] | Four amendments, three new ADs |
| 3.3 UX | [N/A] | No UX specification |
| 3.4 Other artifacts | [x] | Docs follow per epic; tracker mirror |
| 4.1 Direct Adjustment | Viable | High effort, medium risk |
| 4.2 Rollback | Not viable | Shipped work is the substrate |
| 4.3 MVP Review | Not viable | MVP shipped and valid |
| 4.4 Selected path | [x] | Direct Adjustment after re-baseline |
| 5.1–5.5 | [x] | This document |
| 6.1–6.2 | [x] | Reviewed against the analysis and spot-checks |
| 6.3 Approval | [x] | Approved by Islam 2026-10-30, all recommended options |
| 6.4 sprint-status | [x] | Applied 2026-10-30: drift fixed, 19-3 removed, epic-21..25 added as backlog |
| 6.5 Handoff | [x] | Routed to PM and Architect (Major); PO/SM appends epics after the re-baseline |
