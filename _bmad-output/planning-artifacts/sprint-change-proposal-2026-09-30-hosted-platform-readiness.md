# Sprint Change Proposal — Hosted-Platform Readiness (the first production Host)

> **RENUMBERED 2026-10-19 (before landing):** approved 2026-09-30 as Epics 15–17, but while this
> approved output sat uncommitted on a local branch, remote main's autonomous Fibery-backlog
> tranches consumed the epic numbers 15 (done 2026-10-17) and 16 (done 2026-10-19). The hosted-
> platform epics land RENUMBERED: Service Host 15→**17**, Service-Grade Lifecycle 16→**18**,
> Wire Governance 17→**19**; stories shift with their epics (15.x→17.x, 16.x→18.x, 17.x→19.x).
> The body below retains the original 2026-09-30 numbering as the historical record of what was
> ratified; the live tracker (`sprint-status.yaml`), `epics.md`, the PRD FR map, and the
> architecture spine carry the renumbered numbers.

**Date:** 2026-09-30 · **Trigger owner:** Islam (product directive) · **Mode:** Batch
**Route:** Correct Course → new Epics 15, 16 and 17 (renumbered to 17, 18, 19 at landing) · **Scope classification:** Moderate (new epics +
PRD/spine additions; no replan of shipped work)
**Status:** APPROVED 2026-09-30 (Islam, Batch mode) — applied the same day; landed 2026-10-19

> **Naming.** This proposal describes its first production Host generically — a hosted container
> platform (the PRD's UJ-3 "Lena" archetype). Whether to name it publicly is Islam's decision after
> the integration lands; until then no Host identifier, hostname or infrastructure detail enters this
> repository. Every Host-side fact below is stated as a requirement, and every Hekma-side fact is cited
> against this repository.

---

## Section 1 — Issue Summary

**Problem statement.** Hekma has closed all fourteen planned epics. Its production-grade items were
deliberately deferred until a real Host needed them: a long-running host mode (AI-20's option (a)),
the service/IPC delivery surface (PRD §13 Q6), readiness versus liveness (Adapter Contract post-v1),
reconciliation with provider actuals (AD-8's unused `reconciled` label), and per-window budgets. A
Host now needs them. It runs one third-party agent per single-tenant container, as a service
(an HTTP API server, scheduled work, messaging), and wants Hekma to supervise that agent there. Its
money enforcement lives at a choke point outside the container — the container's only egress — and
it wants that enforcement to share Hekma's decision semantics exactly.

**Issue type:** new strategic requirement (UJ-3 made real); nothing failed.

**Evidence — what the Host needs that Hekma does not have (cited in this repository):**

| Need | Today in Hekma | Evidence |
| --- | --- | --- |
| Supervision that outlives a command, driven by the Host over a local API | No daemon and no network or IPC API; a plain `start` supervises only for the command's lifetime; `--detach` leaves no crash detection, enforcement or events between commands | `README.md:163`; `docs/embedding.md:98-112`; spine Deferred "Service/IPC embedding transport" (`ARCHITECTURE-SPINE.md:273`) |
| A host process that owns the engine safely | The CLI crate is audited sync-only (no tokio, no `async`); dropping an `Engine` SIGKILLs attached agents; one engine per state dir with no lock | `crates/hekma-engine/tests/embed_clean.rs:1421+`; `docs/testing.md:153` |
| `running` only when the agent can serve | `starting → running` after a 300 ms liveness window; readiness vs liveness is contract post-v1 | `crates/hekma-engine/src/domain/supervisor/mod.rs:92`; `docs/adapter-contract.md:43` |
| A service's restart hand-off is not a crash | Every unrequested exit, including 0 and 75, is recorded as a crash; the crash-loop counter resets only on an operator start | `domain/supervisor/reaper.rs:425-427`; `spawn.rs:557-561` |
| Drain before the Host stops the container | No drain verb; pause on a cooperative agent is best-effort and signals nothing | `domain/supervisor/lifecycle.rs:637-653` |
| A service-mode agent expressible without a bespoke kind | The `hermes` builtin is deprecated toward ACP stdio, which does not run a gateway's API server, scheduler or messaging | `docs/commands.md:44`; `_bmad-output/planning-artifacts/hermes-agent-verification-2026-08-25.md` §2, §6 |
| The Host's own enforcement matches Hekma's semantics | The evaluator is Rust-internal; no portable statement of its decisions exists | `domain/budget.rs`, `domain/cost.rs` |
| Refuse at the wire when pause cannot stop in-flight work | `forward()` has no budget gate; the breach latch is per process-lifetime Run | `metering/listener.rs:462-568`; `domain/supervisor/mod.rs:573-592` |
| Attribute usage to the Host's own unit of work | A Run is one process lifetime; budgets are per-Run or cumulative only | `docs/architecture.md:61`; AD-7 |
| Per-call facts, and actuals the Host confirms | The ledger holds counts only (no model, latency or status); `EstimateLabel::Reconciled` has no producer | `store/sqlite.rs:96-106`; AD-8 |
| A container-friendly binary, and state that survives an ephemeral disk | Linux releases are glibc x86_64 only; the durable record is local SQLite plus per-family logs; `agent.log` is never rotated | `.github/workflows/release.yml:21-33`; `docs/embedding.md:136-209`; `paths.rs:45-53` |

**A finding that shapes the design (the Host's threat model).** Inside the container the agent runs
model-directed tools, including a shell. Hekma's observed meter there is therefore **cooperative**: an
agent that ignores the injected `base_url` walks around it — the design essay already says the
loopback bind "is not a sandbox" (`docs/design/metering-agents-you-dont-control.md:94`). The Host
keeps enforcement of record at its external choke point and wants Hekma's *semantics* there, proven
by shared test vectors. Hekma supervises the agent, meters cooperatively, and reconciles against the
Host's figures. Privilege separation and attested egress, which would make an in-container fence
non-bypassable, stay deferred until a topology without an external choke point needs them.

---

## Section 2 — Impact Analysis (checklist 2.1–3.4)

**Epic impact.** No epic is in flight — 14 closed 2026-09-22, and the hardening backlog and PC-1..PC-5
are unaffected. **Impact = three new epics (15, 16, 17).** No shipped epic is reopened, invalidated or
re-sequenced.

**Story impact.** Fifteen new stories in the batch-epic shape (the 10–14 precedent): each carries a
scope paragraph here, and full acceptance criteria are derived at the epic's spec stage.

**Artifact conflicts.**

- **PRD** — additive. The approved-but-missing **FR-40** (ACP, 2026-09-19) lands. New **FR-41..FR-45**
  cover host mode, service-grade lifecycle, wire governance, governance vectors and reconciliation.
  §13 Q6 and Q8 get proposed answers; FR-9's `always` Restart Policy is re-expressed as exit-code
  classes. The non-goals **hold**: the control API serves the Host that runs the engine — it is not a
  hosted control plane (§8), not team governance (§8), and not remote telemetry (§6 Privacy). The wire
  fence refuses the agent's own calls at the engine's local proxy — engine-side, not provider-side
  billing enforcement (§8).
- **Architecture spine** — additive:
  - **AD-20 (new)** — the service host.
  - **AD-18** — reopen trigger (1) fires. The re-ratification is owed at Epic 15's spec stage and is
    not assumed here.
  - **AD-7 amendments** — the fence as a Breach Action; task scope; the latch re-arms when a ceiling
    changes.
  - **AD-12 amendment** — log capture modes and `agent.log` rotation.
  - **AD-14 amendment** — a global outbox cursor.
  - **AD-15 amendment** — readiness and liveness gating, exit-code classes, drain.
  - **AD-19 amendment** — any `hermes`-kind removal waits for the service-mode successor.
  - **AD-8** — the `reconciled` label gains its first producer.
  - The Deferred list sheds "Service/IPC transport" and "Reconciliation", and gains the items in §4.5.
- **Adapter Contract** — **v1.1.0, an additive minor** for readiness, liveness, exit-code classes and
  cooperative pause commands. Per the contract's own rule (`docs/adapter-contract.md:89-103`), an
  older engine still rejects a newer same-major manifest that uses a post-1.0.0 section. The Epic 16
  spec states this on the release surface.
- **UI/UX** — none (terminal product).
- **Secondary artifacts:**
  - CI: an AD-2 boundary-gate entry for the host crate, and release-matrix targets for static Linux.
  - Docs: `embedding.md`, `architecture.md`, `adapter-contract.md` and `commands.md`, plus a new
    "running Hekma as a container supervisor" page.
  - The conformance kit gains a service-mode gateway fixture.
  - `deferred-work.md` gains six entries (§4.5).
  - Fibery mirrors the three epics and fifteen stories.

**Technical impact.**
- **New public surfaces, each versioned:** the host binary, the control API (unstable `0.x` until a
  second Host validates it — the Adapter Contract froze only after a second agent validated it,
  `docs/adapter-contract.md:8`), and governance-vector files.
- **Engine surface:** additive. Every new `EngineError` arm or struct field is an announced minor,
  under the semver gate.
- **Money code gets two-pass review:** the fence, task scope and vectors are money code, so the
  AI-55 two-pass review applies.

---

## Section 3 — Recommended Approach (checklist 4.1–4.4)

**Option 1 — Direct adjustment: add Epics 15–17. SELECTED.**
- Option 2 (rollback) — nothing to roll back.
- Option 3 (MVP review) — the v1 MVP is shipped and unaffected. These are v1.x additions that the
  PRD's own deferrals anticipated.

- **Effort: High, but split cleanly.** 17.1 (vectors) is small and unblocks the Host at once. Epics 15
  and 16 carry the bulk and can run in parallel after 15.1's spec. 17.2–17.5 follow.
- **Risk: Medium.**
  - The lock model under a long-lived host (AD-18 trigger 1): measured, not argued, at 15.1.
  - The contract minor's older-engine rejection.
  - Scope creep toward a hosted control plane, which stays a non-goal.
- **Why this path:** it realizes UJ-3 against a real Host. It discharges four long-standing deferrals
  on evidence instead of speculation. And it keeps Hekma generic: every story ships its own fixtures,
  so no guarantee rests on private Host evidence.

**Execution order:**
1. **17.1** — governance vectors. First, because the Host's guard is waiting on them.
2. **15.1** — the host-mode spec.
3. **15.2–15.5** in parallel with **16.1–16.5**.
4. **17.2 → 17.3 → 17.4 → 17.5.**

---

## Section 4 — Detailed Change Proposals

### 4.1 Architecture decisions to ratify with this proposal (AI-58)

**D1 — A service host exists, as a shim crate over the public engine API.** The spine already
anticipated it: "the Embedding Interface stays transport-agnostic (plain async API + serde events) so
a JSON-RPC/gRPC shim can wrap it at v1.x without reshaping the engine" (`ARCHITECTURE-SPINE.md:273`).
The host is a **new crate** wrapping `hekma-engine`'s public API and serving a local HTTP/JSON control
API.
- The `hekma` CLI stays synchronous (the embed-clean audit is untouched). The AD-2 boundary gate gains
  one allowlist entry for the new crate.
- *Alternatives considered:* a synchronous server inside the CLI crate (needs an NFR-8 dependency
  sign-off, and serves SSE poorly); a listener owned by the engine (contradicts the transport-agnostic
  rule). The final crate name and HTTP stack are settled at 15.1's spec stage.

**D2 — The lock model re-opens by trigger (1), decided on measurement at 15.1.**
- Ratifying D1 fires AD-18's reopen trigger (1). The decision is not made here.
- 15.1 measures the serve shape against NFR-4 (the 7-5 method), then either re-ratifies the coarse
  two-mutex model with that evidence or activates the archived per-instance plan
  (`ai-63b-lock-model-decision-2026-08-27.md`).

**D3 — The control API is versioned and unstable until a second Host validates it.** Its rules:
- It binds only where the Host says, and requires a bearer resolved through `SecretResolver`.
- It never exposes `reveal_secrets` or raw agent output unless explicitly enabled.
- Events come over SSE and by cursor pull.
- It is a surface for the Host that runs the engine, not a hosted control plane (PRD §8).

**D4 — The fence is a Breach Action; the Host's choke point stays the enforcement of record.**
- A new Breach Action (e.g. `block`) flips a lock-free per-instance flag, which the loopback
  `forward()` checks, answering with a typed non-retryable refusal (not 429 — SDKs retry it).
- This keeps AD-7 — only the pipeline triggers Breach Actions — and AD-18's premise that the engine is
  not in the agent's request path under a lock.
- It is cooperative, like the meter it guards. It does not make Hekma a sandbox (NFR-6).

**D5 — Governance semantics become a portable, versioned artifact.** Decision vectors (ledger state
and ceilings in, decisions out) cover:
- the `≥` threshold;
- the per-scope latch and its re-arm;
- token ceilings before dollar ceilings;
- integer micro-dollar arithmetic;
- round-half-up;
- saturation;
- no retroactive repricing.

They are generated from, and verified against, the evaluator's own tests. They ship as versioned files
that non-Rust Hosts pin by version and checksum.

### 4.2 PRD — `_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md`

**(a) Frontmatter.** `updated: 2026-09-30`.

**(b) §3 Glossary — add:**
- **Service Host** — a long-running process that owns an Engine continuously and serves a local Control API to the Host that runs it (FR-41).
- **Control API** — the Service Host's versioned local HTTP/JSON surface: lifecycle, configuration, usage, and events by stream or cursor.
- **Task** — a Host-defined unit of work inside a Run (e.g. one request the Host sent the Agent). Budgets and latches can be scoped to it (FR-43).
- **Drain** — stop new work, wait for in-flight work up to a bounded window, then stop (FR-42).
- **Readiness / Liveness Probe** — Adapter-declared checks that an Agent can serve, and is still alive, beyond its process existing (FR-42).
- **Governance Vectors** — the versioned, portable test cases that state the Engine's budget and cost decision rules (FR-44).

**(c) FR-9 amendment (Restart Policy).** Append:

> *(Amended 2026-09-30.)* `always` is subsumed. Every exit the Engine did not request already
> restarts under `on-failure`. What services actually need is exit-code classes (restart-requested,
> clean) and a stable-window reset of the crash-loop counter — FR-42.

**(d) New §4.11 — ACP Backend** (lands the FR approved 2026-09-19, corrected to D2's same-session
amendment and the shipped detach behaviour):

> #### FR-40: ACP backend support
> Any agent that speaks the Agent Client Protocol (ACP) v1 can be registered, started and supervised
> as a Hekma backend via the builtin `acp` kind. Core controls — start, stop, pause, resume, send,
> stop escalation, config, memory backing, fleet surfaces and adoption — behave as they do for other
> kinds.
> **Consequences (testable):**
> - Usage is acquired in tiers — engine-observed, then the stderr sentinel — and ACP context usage is
>   surfaced but never billed. When no tier yields usage, the surfaces show an honest not-available
>   marker naming the tiers attempted. Nothing is fabricated.
> - Client capabilities beyond the ACP baseline are not advertised; requests for them are refused with
>   a surfaced diagnostic.
> - A detached start is refused (the protocol's pipes die with the spawner); the session resumes at
>   the next start's handshake when the agent supports it.

**(e) New §4.12 — Hosted-Platform Readiness (v1.x):**

> #### FR-41: Service host mode
> A Host can run the Engine as a long-running Service Host that owns supervision, metering listeners
> and enforcement continuously, and drive it through a local, versioned Control API instead of
> linking the library.
> **Consequences (testable):**
> - Supervision, crash detection, budget enforcement and event delivery run continuously while the
>   host runs. On SIGTERM the host drains and stops every Agent Instance within a bounded window, and
>   it never SIGKILLs an agent on its own exit before that window ends.
> - Exactly one Service Host owns a state directory at a time; a second owner is refused with a clear
>   diagnostic.
> - The Control API binds only where configured, requires a bearer resolved through the secret
>   mechanism, and returns no secrets and no raw agent output unless explicitly enabled.
> - Events are available live (stream) and by cursor-based pull with one global order across event
>   families, so a Host whose disk is ephemeral can export what it needs before the host exits.
> - The Control API carries its own version and is marked unstable until a second Host validates it.
>
> #### FR-42: Service-grade lifecycle
> An Adapter can declare how its Agent proves it is ready, how it proves it is alive, which exit codes
> are not failures, and how it pauses cooperatively; the Operator or Host can drain an Agent Instance
> before stopping it.
> **Consequences (testable):**
> - `starting → running` happens only when the declared readiness probe passes within its timeout;
>   otherwise the instance lands in `failed` with the probe's reason.
> - A declared liveness failure (e.g. a stale heartbeat) moves a `running` instance to `failed` with
>   cause `unresponsive`, and the Restart Policy applies.
> - A declared restart-requested exit relaunches without counting toward the crash loop, and a
>   declared clean exit is not a crash. The crash-loop counter resets after a stable window.
> - A declared pause command drives pause and resume, surfaced as best-effort when it only stops new
>   work.
> - Drain stops new work, waits for in-flight work up to a bounded window, then stops, recording
>   whether it completed or timed out.
> - These declarations are an additive Adapter Contract minor (v1.1.0), so a gateway-style service
>   agent is expressible as a manifest adapter.
>
> #### FR-43: Governance at the wire and per unit of work
> A Host can scope budgets to its own units of work and have a breach refuse further model calls at
> the Engine's metering proxy.
> **Consequences (testable):**
> - A Host can begin and end a Task on a running Agent Instance. Usage in between is attributed to the
>   Task, and per-Task budgets and latches apply alongside the per-Run and cumulative ones.
> - A `block` Breach Action refuses further model calls through the proxy with a typed, non-retryable
>   response until the ceiling is raised or reset. The breach is recorded like any other.
> - Raising a ceiling re-arms its latch within the same Run.
> - Per-call records are available to the Host — status class, latency, model when present, tokens,
>   Task, sub-second time — and never contain request or response content.
> - The docs state that the fence is as cooperative as the meter: an Agent that bypasses the injected
>   endpoint bypasses both (NFR-6).
>
> #### FR-44: Portable governance semantics
> The Engine's budget and cost decision rules are published as versioned Governance Vectors that a
> Host implementation in another language can run.
> **Consequences (testable):**
> - The vectors cover thresholds, latches and re-arm, evaluation order, integer arithmetic, rounding,
>   saturation and repricing. CI verifies each vector against the Engine's own evaluator.
> - Vectors are versioned and checksummed; changing a rule changes the vectors and the version.
>
> #### FR-45: Reconciliation with confirmed actuals
> A Host can submit provider- or Host-confirmed usage for a Run or Task, and the Engine records it as
> `reconciled` alongside its own `estimated` figures.
> **Consequences (testable):**
> - Reconciled figures never overwrite estimated rows. Both stay readable, and the drift between them
>   is reported.
> - Surfaces label each figure `estimated` or `reconciled` (AD-8), and totals state which label they
>   carry.

**(f) §9 — add §9.3 "v1.x additions":** FR-40 (shipped in Epic 14) and FR-41..FR-45 (Epics 15–17) are
v1.x, not the v1 MVP.

**(g) §13 — proposed answers:**
- **Q6 (proposed 2026-09-30):** a Service Host with a local HTTP/JSON Control API (FR-41, AD-20),
  unstable `0.x` until a second Host validates it. The library remains the primary embedding surface.
- **Q8 (proposed 2026-09-30):** metering stays mandatory at registration for manifest adapters
  (FR-19). The `acp` kind's surfaced gap (FR-40) is the one sanctioned exception. A Host that meters
  at its own choke point may feed Hekma through reconciliation (FR-45), which satisfies FR-19's
  intent without faking a source.

### 4.3 Epics — `_bmad-output/planning-artifacts/epics.md`

**(a) Requirements Inventory:** after FR-39, add one-line entries for FR-40..FR-45 (the §4.2 titles).

**(b) FR Coverage Map:** FR-40: Epic 14 · FR-41: Epic 15 · FR-42: Epic 16 · FR-43: Epic 17 (17.2–17.4) ·
FR-44: Epic 17 (17.1) · FR-45: Epic 17 (17.5).

**(c) Epic List:** add, newest first (the 12–14 pattern):

> ### Epic 17: Wire Governance for Hosts *(opened 2026-09-30 via correct-course)*
> A Host shares the Engine's decision rules as portable vectors, scopes budgets to its own units of
> work, fences further model calls when a ceiling is reached, reads per-call facts, and feeds back
> confirmed actuals. **FRs covered:** FR-43, FR-44, FR-45.
>
> ### Epic 16: Service-Grade Lifecycle *(opened 2026-09-30 via correct-course)*
> Readiness and liveness probes, exit-code classes with a stable-window reset, cooperative pause
> commands and a drain verb — an additive Adapter Contract minor (v1.1.0) under which a gateway-style
> service agent is a plain manifest adapter. **FRs covered:** FR-42 (+ FR-9 amendment).
>
> ### Epic 15: Service Host *(opened 2026-09-30 via correct-course)*
> A long-running host that owns supervision continuously and serves a local, versioned Control API;
> state export for ephemeral disks; static Linux releases and a container recipe. **FRs covered:**
> FR-41.

**(d) Epic sections:** append §4.4's text at the end of the document.

### 4.4 The epic sections (appended to `epics.md`)

````markdown
## Epic 15: Service Host

> **(2026-09-30 — opened via correct-course: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md`.)**
> The first production Host — a hosted container platform, the UJ-3 archetype — needs supervision
> that outlives a command and a local surface to drive it. AI-20 deferred exactly this "as a deliberate
> future epic when embedder demand … materializes"; PRD §13 Q6 asked for its delivery path. Realizes
> FR-41. Batch-epic shape (the 10–14 precedent): full ACs are derived at the spec stage.

### Story 15.1: Host-mode spec — the shim crate and the lock model (D1, D2; spec stage, AI-58)
Settle the two decisions this proposal leaves to measurement:
1. **The host crate.** Its name, its HTTP stack under an NFR-8 review, and its AD-2 boundary entry.
   The CLI stays synchronous and the engine transport-agnostic.
2. **The lock model.** AD-18's reopen trigger (1) fires here. Measure the serve shape against NFR-4 by
   the 7-5 method, then either re-ratify the coarse two-mutex model with that evidence or activate the
   archived per-instance plan.

Deliverables: the AD-20 spine text, and a bounded-options proposal for Islam.

### Story 15.2: `hekma serve` — the long-running host
- A foreground host that owns one Engine for its whole life, with a state-directory lock so exactly
  one owner exists.
- A fixed `metering.listen` port option, which also closes the adoption strand.
- A child environment allowlist: agents inherit only what is declared, not the host's whole
  environment.
- SIGTERM drains then stops every instance within a bounded window, and the host never SIGKILLs an
  agent on its own exit before that window ends.
- Operator-visible notices through the diagnostic sink.

### Story 15.3: Control API v0 (D3)
- An HTTP/JSON projection of the public facade — lifecycle, config and budgets, fleet and status,
  usage, events — plus `drain` and the Task verbs once Epics 16 and 17 land.
- Explicit bind; a bearer resolved through `SecretResolver`; no secret reveal and no raw agent output
  unless explicitly enabled.
- Events over SSE, and pull by cursor.
- Versioned and marked unstable (`0.x`) until a second Host validates it.
- A conformance-style suite drives the whole API against the fake agents.

### Story 15.4: Export for ephemeral hosts — the outbox and log capture modes
- A global sequence across the transition, breach and usage families, pulled by cursor and
  acknowledged by the Host (AD-14 amendment); the per-family `resync_events` stays.
- The state directory can be placed on a volume the Host chooses.
- Log capture modes `full` (today's behaviour, the default), `metadata` and `off`, with `agent.log`
  rotation bounded like `output.log` (AD-12 amendment). A Host whose agent output may carry user
  content can keep it off disk.

### Story 15.5: Static Linux release and the container recipe
- Static musl `x86_64` release artifacts, checksummed, in the release matrix; `aarch64` is deferred.
- A docs page, "running Hekma as a container supervisor", covering:
  - the host as the container command or as an init-supervised service;
  - how drain meets the container's stop grace;
  - state-directory placement;
  - the cooperative-meter caveat (NFR-6).

## Epic 16: Service-Grade Lifecycle

> **(2026-09-30 — opened via correct-course: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md`.)**
> A service agent is ready later than its process exists, can hang while alive, asks its supervisor to
> restart it with an exit code, and must drain before a stop. None of that is expressible today.
> Realizes FR-42 as an additive Adapter Contract minor (v1.1.0). Per the contract's own rule, an
> older engine still rejects a newer same-major manifest that uses a new section — this is stated on
> the release surface (AI-55).

### Story 16.1: Readiness probes
- Declared probes: an HTTP GET with an expected status, a file's age, or a log-line match — each with a
  timeout.
- `starting → running` waits for the probe; a timeout lands the instance in `failed` with the probe
  named (AD-15 amendment).
- Honest per-OS semantics where a probe kind needs them.

### Story 16.2: Liveness and the unresponsive failure
- Declared liveness: a heartbeat file's maximum age, or an HTTP check.
- A failure moves `running → failed` with cause `unresponsive`, and the Restart Policy applies.
- Liveness never probes a paused instance.

### Story 16.3: Exit-code classes and the stable-window reset
- Declared restart-requested codes (e.g. 75) relaunch without charging the crash loop.
- Declared clean codes are not crashes.
- The consecutive-failure counter resets after a stable running window.
- FR-9's `always` is recorded as subsumed.

### Story 16.4: Cooperative pause commands and the drain verb
- A declared pause/resume command pair (e.g. a sentinel-writing CLI verb) drives pause and resume.
  It is still surfaced as best-effort when it only stops new work.
- `drain` = stop new work (the pause command, if declared) → wait for idle, bounded → stop. The
  outcome records completed versus timed out, and no new lifecycle state is added.

### Story 16.5: A service-mode gateway as a manifest adapter
- A reference manifest for a gateway-style service agent: an API server, readiness on its models
  endpoint, heartbeat liveness, exit-75 restart, and a cooperative pause.
- A conformance-kit fake that behaves like one, with TCK sections for readiness, liveness, exit
  classes and drain.
- The `hermes`-kind deprecation (14-5) gains its service-mode successor here: removal waits for this
  story (AD-19 amendment).

## Epic 17: Wire Governance for Hosts

> **(2026-09-30 — opened via correct-course: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md`.)**
> The Host enforces money at its own external choke point and wants Hekma's rules there, provably. It
> also wants Hekma's cooperative meter to fence, to attribute usage to its own units of work, and to
> reconcile against the Host's figures. Realizes FR-43, FR-44 and FR-45. This is money code: every
> story gets the AI-55 two-pass review.

### Story 17.1: Governance conformance vectors — FOUNDATION, runs first (D5, FR-44)
- Versioned, checksummed JSON vectors — ledger state and ceilings in, decisions out — covering:
  - the `≥` threshold;
  - per-scope latches and re-arm;
  - token ceilings before dollar ceilings;
  - integer micro-dollar arithmetic;
  - round-half-up;
  - saturation;
  - no retroactive repricing.
- They are generated from, and verified against, the evaluator in CI, and published so a non-Rust Host
  can pin them.
- Small, and first: the Host's guard waits on them.

### Story 17.2: The wire fence — a `block` Breach Action (D4, AD-7 amendment)
- On breach, the pipeline flips a lock-free per-instance flag.
- `forward()` refuses further model calls with a typed, non-retryable response (not 429), until the
  ceiling is raised or reset.
- The breach event and log record the fence like any action.
- Docs state its cooperative scope (NFR-6).

### Story 17.3: Task scope — Host units of work (AD-7 amendment)
- Begin and end a Task on a running instance through the facade and the Control API.
- Ledger rows and events carry the Task.
- Per-Task budgets and latches sit beside per-Run and cumulative ones.
- Raising a ceiling re-arms its latch within the Run.
- Usage outside any Task is recorded as untasked.

### Story 17.4: Per-call records for Hosts
- For each observed completion: status class, latency, model when present, tokens, Task, sub-second
  timestamp.
- Never request or response content — the sentinel no-leak matrix is extended.
- Readable through the facade and the Control API.

### Story 17.5: Reconciliation ingestion — the `reconciled` label's first producer (FR-45, AD-8)
- A Host submits confirmed usage for a Run or Task.
- Reconciled figures sit beside estimated rows and never overwrite them.
- Drift is reported, and every surface labels which figure it shows.
````

### 4.5 Architecture spine — `_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md`

**(a) Frontmatter:** `updated: '2026-09-30'`; `binds` gains FR-40..FR-45.

**(b) New AD** (after AD-19):

> ### AD-20 — The Service Host is a shim over the public engine API
> - **Binds:** FR-41; AD-2, AD-13, AD-14, AD-17/AD-18; PRD §8 (no hosted control plane), §6 Privacy, NFR-6, NFR-8.
> - **Prevents:** supervision gaps between commands for Hosts that cannot link Rust; a second
>   business-logic implementation growing inside a transport layer; a network surface that leaks
>   secrets or content.
> - **Rule:** a new host crate depends only on `hekma-engine`'s public API — one AD-2 boundary-gate
>   entry — and serves a local HTTP/JSON Control API. The `hekma` CLI stays synchronous; the engine
>   library stays transport-agnostic. Around that:
>   - **Ownership:** one Service Host owns a state directory, enforced by a lock.
>   - **Binding and auth:** the bind address is explicit, and auth is a bearer resolved through
>     `SecretResolver`.
>   - **What it never returns:** no secret reveal and no raw agent output unless explicitly enabled.
>   - **Events:** live over SSE, and by pull on a global cursor (AD-14 amendment).
>   - **Shutdown:** SIGTERM drains, then stops within a bound. The host may own process signals; the
>     engine library still may not (the embed-clean audit is unchanged).
>   - **Versioning:** the Control API carries its own version, unstable `0.x` until a second Host
>     validates it.
>   - **Settled at 15.1's spec stage:** the crate name, the HTTP stack (NFR-8 review) and AD-18's
>     outcome.

**(c) Amendments** (appended as dated notes, the AD-16 correction precedent):

- **AD-7 (2026-09-30):**
  - A `block` Breach Action fences further model calls: the pipeline flips a lock-free per-instance
    flag that `forward()` checks, refusing with a typed non-retryable response. No lock is taken in
    the request path, so AD-18's premise holds.
  - A Host-defined **Task** is a budget scope beside per-Run and cumulative.
  - Raising a ceiling re-arms its latch within the Run.
  - Still true: no code path other than the pipeline triggers a Breach Action.
- **AD-8 (2026-09-30):** reconciliation ingestion (FR-45) is the `reconciled` label's first producer.
  Reconciled figures never overwrite estimated rows.
- **AD-12 (2026-09-30):** capture modes `full` (the default, today's behaviour), `metadata` and `off`;
  `agent.log` rotation joins `output.log`'s bound.
- **AD-14 (2026-09-30):** an export view with one global sequence across the event families, pulled by
  cursor. The per-family `resync_events` is unchanged.
- **AD-15 (2026-09-30):**
  - Declared readiness gates `starting → running`.
  - Declared liveness failure is `running → failed` (cause `unresponsive`).
  - Declared exit-code classes: restart-requested exits do not charge the crash loop; clean exits
    are not crashes.
  - A stable window resets the counter.
  - `drain` is a composite (stop new work → bounded wait → stop). It adds no state.
- **AD-18 (2026-09-30):** reopen trigger (1) fired with AD-20's ratification. The decision is
  15.1's, on measurement.
- **AD-19 (2026-09-30):** removal of the `hermes` kind waits for the service-mode successor (16.5);
  ACP stdio does not run a gateway's API server, scheduler or messaging.

**(d) Deferred list:**
- Remove "Service/IPC embedding transport" (now AD-20) and "Reconciliation with provider actuals"
  (now 17.5).
- Add: privilege separation + attested egress; windowed (daily/monthly) and runtime-hour budgets;
  pre-start/pre-stop lifecycle hooks; a declarative fleet file; static `aarch64` Linux; incremental
  SSE relay; multi-route upstreams; approval routing (ACP `request_permission` to the Host); a
  `hekma conformance` CLI for non-Rust adapter authors; a WASM build of the governance core.

### 4.6 `_bmad-output/implementation-artifacts/deferred-work.md`

New section, `## Deferred from: sprint-change-proposal-2026-09-30-hosted-platform-readiness (correct-course, Batch)`:
1. **Privilege separation and attested egress.** Would make an in-container fence non-bypassable.
   Trigger: a Host topology with no external choke point.
2. **Windowed and runtime-hour budgets** (daily/monthly; compute time). Trigger: a Host asks for
   either against Hekma's own evaluator.
3. **Pre-start / pre-stop lifecycle hooks** (e.g. state hydrate / dehydrate). Trigger: a Host shows
   its own hook machinery failing where engine hooks would not.
4. **A declarative fleet file.** Trigger: a Host runs more than one agent per Service Host.
5. **Incremental SSE relay and multi-route upstreams.** Trigger: a Host whose model path streams to
   the agent, or reaches more than one upstream.
6. **Approval routing** (ACP `request_permission` to the Host) **and a `hekma conformance` CLI.**
   Trigger: the first Host that surfaces approvals to its users, and the first non-Rust adapter
   author, respectively.

### 4.7 `_bmad-output/implementation-artifacts/sprint-status.yaml`

In `development_status`, add the three epics — `backlog`, with provenance comments — their stories,
and `optional` retrospective keys:

```yaml
  epic-15: backlog  # opened 2026-09-30 by sprint-change-proposal-2026-09-30-hosted-platform-readiness.md (Service Host; FR-41; AD-20; AD-18 trigger 1 → 15-1)
  15-1-host-mode-spec-the-shim-crate-and-the-lock-model: backlog
  15-2-hekma-serve-the-long-running-host: backlog
  15-3-control-api-v0: backlog
  15-4-export-for-ephemeral-hosts-outbox-and-log-capture-modes: backlog
  15-5-static-linux-release-and-the-container-recipe: backlog
  epic-15-retrospective: optional
  epic-16: backlog  # opened 2026-09-30 by the same proposal (Service-Grade Lifecycle; FR-42; Adapter Contract v1.1.0)
  16-1-readiness-probes: backlog
  16-2-liveness-and-the-unresponsive-failure: backlog
  16-3-exit-code-classes-and-the-stable-window-reset: backlog
  16-4-cooperative-pause-commands-and-the-drain-verb: backlog
  16-5-a-service-mode-gateway-as-a-manifest-adapter: backlog
  epic-16-retrospective: optional
  epic-17: backlog  # opened 2026-09-30 by the same proposal (Wire Governance for Hosts; FR-43..FR-45; money code → AI-55 two-pass)
  17-1-governance-conformance-vectors: backlog  # FOUNDATION — runs first; an external Host consumer waits on it
  17-2-the-wire-fence-block-breach-action: backlog
  17-3-task-scope-host-units-of-work: backlog
  17-4-per-call-records-for-hosts: backlog
  17-5-reconciliation-ingestion: backlog
  epic-17-retrospective: optional
```

The header's `last_updated` timestamp is refreshed.

### 4.8 Fibery mirror (per `AGENTS.md`, in the same change)

In Ktesio → Product Development → the **Hekma** Product:
- three Features — Epic 15, Epic 16, Epic 17 — and fifteen Tasks (Story 15.1 … 17.5), keeping the
  BMAD numbering and linking this proposal;
- the six §4.6 deferrals under "Hardening & Housekeeping Backlog".

Wording stays generic ("a hosted container platform").

---

## Section 5 — Implementation Handoff

**Scope: Moderate** — three new epics and additive PRD/spine/contract changes. No shipped work is
replanned. The architect touch (AD-20 and the amendments) is drafted here for ratification in place
(AI-58).

| Recipient | Responsibility |
| --- | --- |
| **Product Owner (Islam)** | Ratify this proposal and decisions D1–D5, and the proposed Q6/Q8 answers. Pick the start of 17.1. Decide later whether to name the first Host publicly. |
| **Architect** | At 15.1: the host crate and HTTP stack (NFR-8 review) and AD-18's measured outcome, as bounded options. At Epic 16's spec: the contract v1.1.0 schema and its release-surface statement. |
| **Developer** (batch-epic flow) | 17.1 first; then Epics 15 and 16 in parallel after 15.1; then 17.2 → 17.5. All four house gates on every story, ≥95% coverage, docs in the same change, and AI-55 two-pass review on the money code (Epic 17) and the release surface. |

**Success criteria:**
1. A Host can run the Service Host as a container command, drive it only through the Control API, and
   stop it with a bounded drain. No agent is SIGKILLed early, and no event is lost across an ephemeral
   disk.
2. A gateway-style service agent registers as a plain manifest adapter. It reaches `running` only when
   ready, recovers from a hang, and restarts on its hand-off code without a crash-loop charge — proven
   by the TCK fixture.
3. The Host's own guard implementation passes the published Governance Vectors, and Hekma's evaluator
   passes the same vectors in CI.
4. A `block` breach refuses further model calls through the proxy. Per-Task budgets and per-call
   records work end to end, and reconciled figures appear beside estimates with drift reported.
5. The frozen v1 contracts show only additive, announced changes, and the semver gate stays green.

---

## Approval

**APPROVED by Islam 2026-09-30** (Batch mode): the proposal as a whole, decisions D1–D5, and the
proposed Q6/Q8 answers, ratified as written. Applied the same day on branch
`docs/hosted-platform-readiness`: PRD (§4.2), epics (§4.3–§4.4), spine (§4.5), deferred work (§4.6),
tracker (§4.7), and the Fibery mirror (§4.8).
