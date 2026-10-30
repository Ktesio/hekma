---
title: Input reconciliation — PRD vs control-plane pivot inputs
date: 2026-10-30
prd: prd.md (updated 2026-10-30)
inputs:
  - ../../sprint-change-proposal-2026-10-30-control-plane-pivot.md (approved D1a, D2a, D3a, D4 open, D5a)
  - ../../../brainstorming/brainstorm-hekma-value-proposition-pivot-2026-10-29/brainstorm-intent.md
mode: read-only check
---

# Input reconciliation: PRD vs control-plane pivot

Ruled out of scope: "Ktesio"/`kt` outside the title and vision (D5a, separate housekeeping). Architecture-only
items (AD-18 lock design, AD-22/23/24 mechanics) are not counted as PRD gaps unless the PRD needs a capability
statement for them.

## Totals

| Check | Covered / faithful | Partial / residual | Gap / still contradicted / deviated |
|---|---|---|---|
| Capabilities (§2.1 = intent A/B/C/G-core, 19 items) | 16 | 1 (A1) | 2 GAP (A2, G7) |
| Extra intent items | 2 | 1 | 2 GAP (observed-depth path, theme E acknowledgement) |
| §2.3 conflicts (11 non-ruled) | 4 resolved | 3 resolved with stale residue (#1, #3, #4) | 2 still contradicted (#9, #11); #6, #8 architecture-only (#8 surfaces as G7) |
| P1–P10 | 7 faithful | — | 3 deviated (P5 substantive; P1, P7 benign) |
| Seven principles | 4 | 3 (3, 4, 6) | 0 outright GAP |

## 1. Capability coverage

| # | Capability (proposal §2.1 / intent bullet) | PRD coverage | Status |
|---|---|---|---|
| A1 | Same verbs for any runtime: `ps/logs/top/pause/resume/stop/budget` behave identically for CrewAI, LangGraph, Hermes, OpenCode or a raw HTTP call | FR-49 (verb fails fast if depth cannot honour it), FR-50 (`ps`, `budget`, Selectors, Control API parity), FR-57 (`top`), FR-56 (hold) | **Partial.** The PRD never says what `pause`/`resume`/`stop`/`logs` *do* at depths `wire` and `registered`. FR-25 `logs` is Agent-Instance-only; FR-56 calls hold "pausing the Run" but does not bind the `pause` verb to it; `stop` on a wire Run is undefined. As written, verbs differ by runtime, which is what A1 forbids. |
| A2 | Swap-the-engine portability: moving a flow between frameworks keeps dashboards, policies, alerts, runbooks | FR-49 labels ("the only grouping Selectors use") | **GAP.** No statement that labels (not Agent kind/runtime) key telemetry, budgets and policies, so a flow keeps its identity across runtimes. |
| A3 | `hekma top`: live TUI, tokens/s, burn rate, budget left, hotkeys (pause, kill, attach, raise limit) | FR-57 | Covered. Minor: hotkeys list pause, stop, hold, raise; intent's `attach` hotkey is missing. |
| A4 | Complement gateways: identity on every call; sit in front of LiteLLM/Portkey | FR-51 bullet 4 | Covered (placed in FR-51; wording is general enough to cover supervised Runs' forwarded calls too). |
| B1 | Control-depth ladder + CONTROL column | Glossary *Control Depth*; FR-49; FR-50 bullet 1 | Covered. Note: Control Depth `registered` collides with Lifecycle State `registered` (Glossary discipline). |
| B2 | Wire hold / release / reroute / reject, zero code via base-URL override | FR-56; FR-51 bullet 1 | Covered. |
| B3 | Self-announce (header, OTel, one call) | FR-52 bullet 1; FR-1 note | Covered. |
| B4 | Leases; expired = `silent`, not gone | Glossary *Lease*; FR-52 bullet 2; FR-50 exception view | Covered. Minor: no Run status vocabulary exists (`held`, `silent`, …); Lifecycle State is Agent-Instance-only. |
| C1 | Per-Run key, expires, refresh only by original process | Glossary *Run Key*; FR-53 bullet 1 | Covered. |
| C2 | Key custody, virtual provider keys | FR-53 bullet 2; FR-53 bullet 5 (secrets) | Covered. |
| C3 | Bypass detection on CLI and OTLP | FR-54 | Covered (honest per OS). |
| C4 | Scent check off / shadow / enforce per policy | Glossary *Scent-Check Mode*; FR-53 bullet 3; §13 Q9 (D4 open) | Covered. |
| G1 | Shim per Run | Glossary *Shim*; FR-55 bullet 1 | Covered. |
| G2 | Durable registry: runs, budgets, leases, holds; restart recovers ps and spend exactly | FR-55 bullets 2, 3, 5; FR-10 | Covered. Minor tension: FR-10 still allows losing ≤5s of usage on an Engine crash; intent says "exactly". Acceptable if stated. |
| G3 | Zero-downtime restart/upgrade under months-long runs | FR-55 (crash, restart, upgrade); FR-49 bullet 5 (duration) | Covered, but handoff is opt-in (see P5). |
| G4 | Nodes + hub (tailnet), local rollups, hub merges | Glossary *Node / Hub*; FR-58; §6 note | Covered. |
| G5 | Selectors on every verb, pagination, limits, filtering | Glossary *Selector*; FR-50 bullets 3–6 | Covered. |
| G6 | Exception-first views | FR-50 bullet 2; FR-57 bullet 3 | Covered. |
| G7 | OTel cardinality: run_id on traces/exemplars only; flow/policy labels on metrics | Nothing. FR-46 only says attributes are a semver'd surface; §6 note mentions trace export as "FR-46's successor", an FR that does not exist | **GAP.** It changes a public, versioned surface (FR-46), so the PRD needs a capability statement, not only AD-21. |

Extra intent items:

| Intent item | PRD coverage | Status |
|---|---|---|
| "Installs anywhere (local, cloud function, VM) or connects to Hekma endpoints" (principle 4) | FR-51 bullet 3 (authenticated non-loopback endpoint), NFR-2, NFR-8 | Covered. |
| Attach-only discovery: ps learns of Runs only from traffic | FR-51 bullet 2 (appears after first call; unattributed traffic listed) | Covered. |
| Depth `observed` (telemetry only) | Glossary only. No FR says how a Run becomes `observed` or where its usage comes from (OTLP ingest? gateway report?). The FR-19 note says observed Runs are "metered on the wire", which contradicts the definition (on the wire = depth `wire`). | **GAP.** |
| Later themes D/E/F acknowledged | D and F in §2.2, §8, §13 Q11. **E (customer safety at the wire) is not mentioned anywhere.** | **GAP** (acknowledgement only). |
| Open questions: OSS model, policy versioning, run keys optional, CrewAI/LangGraph validation | §13 Q2 (licensing), Q11, Q9, FR-52 bullet 3 + §14 #22 | Covered. "Adoption first, not monetization" is not stated; fine. |
| Scale "10k to 10M must be smooth" | NFR-4 amendment measures 10k/100k/1M (follows proposal P8) | Partial: PRD follows the approved proposal; 10M is unrecorded. |

## 2. Proposal §2.3 conflicts

| # | Conflict | Status | Residue / quote |
|---|---|---|---|
| 1 | Breach default `pause` (FIXED) vs signal, don't kill | **Resolved, stale residue.** Glossary, FR-21, §6 Safety, §13 Q3, §14 #7 and UJ-1 (now sets `pause` explicitly) all updated. | §4.5 Description still frames the headline as lifecycle action: "The headline guardrails. … Enforcement happens at the runner level — the Engine acts on the Agent Instance's lifecycle when a ceiling is hit". §2.1 JTBD: "trust it is enforced even when I am not watching". SM-2: "100% of runaway scenarios … end in the configured Breach Action" (`warn` ends nothing). |
| 2 | FR-41 SIGTERM stops every Agent Instance | **Resolved** per P5 (FR-41 + FR-55). | Default remains drain-and-stop because FR-55 makes handoff opt-in (see P5). |
| 3 | Title, §1, §2.2 Non-Users, §8 non-goals, AD-20 | **Resolved, stale residue.** Title, §1 para 1, §2.1 JTBD, §2.2, §8 rewritten. | §10 Why Now is unchanged and argues only the personal-agent runner and embeddable-engine window; no framework-team or control-plane rationale. §2.3 has no user journey for framework teams or the solo multiplexer, though §2.3 says "FRs reference these by ID" and FR-49..58 reference none. Glossary *Fleet* is still "all Agent Instances". (§1 paras 2–3 "personal agents" are kept by P1 on purpose.) |
| 4 | Agent = program; Run = process span; FR-1 operator-only; FR-19 mandatory | **Resolved, introduced inconsistency.** Glossary Agent/Run, FR-1 note, FR-19 note. | FR-19 note: "observed and registered Runs are metered on the wire when their traffic passes through Hekma" — an `observed` Run is "telemetry only" by the Control Depth definition, so it cannot be metered on the wire. (Verbatim from P6, so the defect is in the proposal text.) |
| 5 | AD-7 loopback-only listener | **Resolved** at capability level: FR-51 bullet 3. | — |
| 6 | AD-18 engine not in request path | Architecture-only. PRD contains no contradicting text. | — |
| 7 | NFR-4 25-instance sizing | **Resolved** (P8 amendment appended). | — |
| 8 | AD-21 run_id as metric attribute | Architecture-only, but leaves PRD gap G7. | — |
| 9 | `kt` first consumer; Control API "unstable 0.x" vs API + OTel as daily surface | **Still contradicted.** | FR-41: "The Control API carries its own version and is marked unstable until a second Host validates it." §13 Q6: "unstable `0.x` until a second Host validates it." §7 lists only three public contracts (Adapter Contract, Embedding Interface, `kt` CLI); the Control API and the OTLP exposition, now "the daily surface for teams" (§1), are absent, although FR-46 points to "the §7 deprecation path". |
| 10 | FR-43/NFR-6 cooperative fence | **Resolved.** FR-43 amendment note and §8 exception. | NFR-6 itself has no cross-reference to FR-53/FR-54 (cosmetic). |
| 11 | Any state-dir user or bearer can raise ceilings vs callers cannot override non-negotiables | **Still contradicted** (proposal accepts the full fix as theme D, but the PRD should say so). | FR-18: "Budgets are inspectable and changeable while `running`; changes apply immediately." FR-50: "`hekma budget` reads, sets and raises … over a Selector". §4 actor note: every Operator capability "is equally available to a Host". "Caller" (FR-53 bullet 4, §4.14 Description) is never defined, so nothing says *who* is barred. |
| 12 | "Ktesio"/"kt" | Ruled (D5a); not counted. | — |

## 3. P1–P10

| P | Status | Notes |
|---|---|---|
| P1 Title + §1 | **Deviated (benign).** | Title and frontmatter exact. New paragraph is placed *first* instead of added after paras 1–3; paras 1–3 kept and renamed. Arguably better. |
| P2 §2 + §2.2 | Faithful. | Framework teams added as a JTBD (§2 has no segment list); §2.2 two old bullets replaced by the NEW text. |
| P3 Glossary | Faithful (+ additive). | Agent, Run, Control Depth, Run Key, Breach Action as specified. Adds Lease, Selector, Scent-Check Mode, Shim, Node/Hub, and marks Task superseded (consistent with D2a). |
| P4 FR-21 | Faithful. | Adds "ships as a documented default change" (matches D1 note). |
| P5 FR-41 | **Deviated (substantive).** | FR-41 text is faithful, but FR-55 adds "Handoff mode is opt-in; without it the FR-41 drain-and-stop shutdown applies." The proposal never set handoff's default. With opt-in, a plain `hekma serve` restart still stops every Run, against "never the reason prod stops". Needs an explicit owner decision or a default-on-under-Shims rule. |
| P6 FR-1 / FR-19 notes | Faithful (verbatim). | Inherits the `observed` metering contradiction (§2 #4). |
| P7 FR-49..58 | **Deviated (benign).** | All ten present with full consequences. FR-49/FR-50/FR-55 retitled; `hekma budget` (21.4) folded into FR-50; A4 gateway identity (21.6) placed in FR-51. Not carried: the 19-4 AC "every record carries run id, control depth and run-key identity" — FR-43's per-call record field list still names "Task" and lacks those fields. |
| P8 NFR-4 | Faithful. | |
| P9 §8 | Faithful. | Q5 closed. |
| P10 §6 | Faithful. | Note cites "FR-46's successor" for traces; no such FR exists (ties to G7). |

## 4. Seven principles

| # | Principle | Where the PRD states or enforces it | Status |
|---|---|---|---|
| 1 | Signal, don't kill — make it loud | FR-21 (warn default + loud signal list: event, `ps` anomaly, OTLP counter, response header); §6 Safety; FR-43 note (`block` never default); FR-52 (silence never stops); FR-54 (detection only signals); §4.14 Description | Covered. Watchpoints: FR-42 liveness failure moves a Run to `failed` and applies the Restart Policy (a kill on a stale heartbeat); FR-55 handoff opt-in. |
| 2 | Run duration never changes behaviour | FR-49 bullet 5; §4.14 Description | Covered. |
| 3 | Callers cannot override non-negotiables | FR-53 bullet 4; §4.14 Description | **Partial.** "Caller" undefined; FR-18/FR-50 raises open to any bearer (§2 #11). |
| 4 | One consistent interface regardless of agent tech; installs anywhere | §1; FR-49 depth honesty; FR-50 Control API parity; FR-51 | **Partial.** Per-depth verb semantics (A1) and runtime-independent identity (A2) missing. |
| 5 | Scale to millions of Runs | NFR-4 amendment; FR-50 pagination; FR-58 rollups; §4.14 Description | Covered (1M measured vs intent 10M). |
| 6 | CLI for emergencies; API + OTel daily; OTel-first, UI-optional | §1 para 1; §2.2; §8 | **Partial.** Stated, not enforced: Control API/OTLP not in §7 contracts, Control API still "unstable" (§2 #9); G7 missing. |
| 7 | Scent check per policy off/shadow/enforce | Glossary; FR-53 bullet 3; FR-43 note; §13 Q9 | Covered. FR-43's "policy mode" is an undefined term (Glossary discipline). |

## 5. Qualitative ideas the FR structure may have dropped

- **"Make it loud"** — kept in FR-21's signal list. The intent's "the agent or framework corrects itself" (response header as the agent-facing signal) is only "where the wire allows"; acceptable until theme D.
- **"Never the reason prod stops"** — in §1 only. No NFR enforces it; FR-55 opt-in handoff and FR-42 kill-on-unresponsive cut against it. §13 Q10 lists "zero Hekma-caused stops" only as a candidate metric.
- **OTel-first, UI-optional** — in §8 and §2.2. Not backed by a cardinality rule (G7), a trace FR, or a stability promise for the exposition.
- **CLI for emergencies** — vision sentence only; fine as positioning, no FR needed.
- **Audiences / journeys** — no UJ for the framework team (attach by base URL, see CONTROL, get warned not stopped) or the solo multiplexer (`top` over many sessions, tailnet view). §10 Why Now not updated. Intent's "a committed first production host anchors the enterprise path" is absent.
- **Theme E** not acknowledged (D and F are).
- **Glossary hygiene introduced by the edit:** `registered` is both a Lifecycle State and a Control Depth; "policy mode" undefined; "caller" undefined; no Run status set (`held`, `silent`).
- Secondary, outside the PRD: `addendum.md` still says Hekma "acts on *lifecycle*, not per-request rejection" (Metering Sources section), now untrue under FR-43/FR-56.

## 6. Concrete fixes

1. **A1** — add to FR-49 or FR-50: at `wire`/`registered` depth, `pause`/`resume` = hold/release of the next call (FR-56); `stop` = reject further calls with the typed response and end the Lease; `logs` = the Run's per-call records (no content), with "no process output at this depth" stated; anything else fails fast naming the depth.
2. **A2** — add an FR-49 consequence: a Run's labels, not its Agent kind or runtime, key its telemetry, budgets, Selectors and policies, so moving a flow (same labels) to another framework keeps dashboards, alerts and runbooks.
3. **G7** — add to FR-46 (or a new FR-59 for traces): metrics carry Control Depth and flow/policy labels, never a run id; per-Run detail goes to exemplars and traces; the change ships as a new exposition version (`otlp-metrics v2`) under §7's deprecation path. Point §6's "FR-46's successor" at that FR.
4. **Observed depth** — state how a Run becomes `observed` and where its usage comes from (or drop the rung for v-next); reword the FR-19 note to "wire and registered Runs are metered on the wire; observed Runs carry only the usage their telemetry reports".
5. **Per-call records** — FR-43 bullet 4: replace "Task" with run id, Control Depth and Run Key identity (19-4 AC).
6. **#11 / principle 3** — define *caller* in §3 (a Run or agent process authenticating by Run Key or identity header) vs Operator/Host (state dir or Control API bearer); add to FR-53: a Run-Key-authenticated request can never raise a ceiling, lower a mode or change policy; note that Host-bearer raises stay until theme D.
7. **#9 / principle 6** — add the Control API and the OTLP exposition to §7 as public contracts and give FR-41's "unstable" marker a concrete exit criterion that does not wait only on a second Host.
8. **#1 residue** — rewrite the §4.5 Description ("loud by default; acts on lifecycle or the wire only when the Breach Action says so"), the §2.1 JTBD ("signalled, and enforced where I opt in") and SM-2 ("trigger" not "end in").
9. **#3 residue** — update §10 Why Now; add UJ-4 (framework team attaches a LangGraph flow) and UJ-5 (solo dev on `hekma top` / Hub) and cite them from FR-49..58; widen Glossary *Fleet* to Runs.
10. **P5 / "never the reason prod stops"** — owner decision: make handoff the default once Shims ship, or record opt-in as deliberate; add an NFR "Hekma stops a Run only on an explicit verb or an opt-in Breach Action or policy", and make FR-42's kill-on-unresponsive signal first (or opt-in).
11. **Theme E** — add "customer safety at the wire (theme E) is later" to §8 next to D/F.
12. **Glossary hygiene** — rename or qualify Control Depth `registered` vs Lifecycle State `registered`; define or remove "policy mode" (FR-43 note); add a Run status set (`active`, `held`, `silent`, `ended`, …).
13. **Minor** — FR-57 hotkeys add `attach`; record 10M as the long-run scale aim or say 1M is the v-next ceiling; state that FR-10's ≤5s loss bound applies to crash recovery under FR-55.
