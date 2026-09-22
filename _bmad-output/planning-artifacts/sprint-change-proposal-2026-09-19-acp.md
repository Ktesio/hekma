# Sprint Change Proposal — ACP Backend Support (any ACP-capable agent as a Hekma backend)

**Date:** 2026-09-19 · **Trigger owner:** Islam (product directive) · **Mode:** Batch
**Route:** Correct Course → new Epic 14 · **Scope classification:** Moderate (backlog reorganization; no replan of existing work)

---

## Section 1 — Issue Summary

**Problem statement.** Hekma's promise is "runs third-party AI agents like services," but today every new agent requires bespoke integration work: a manifest (or a bespoke native builtin), per-agent config mapping, and metering wiring. Meanwhile the industry has converged on an open integration standard — the **Agent Client Protocol (ACP)**, Zed's JSON-RPC-over-stdio "LSP for AI agents" — with Gemini CLI speaking it natively (`--acp`) and Claude Code via the `claude-code-acp` adapter, and client adoption spreading through 2026. Islam's directive: make any ACP-capable agent a Hekma backend with minimal work.

**Issue type:** new strategic requirement / market alignment (not a defect; nothing failed).

**Evidence.**
- ACP is an open standard at v1 ([agentclientprotocol.com](https://agentclientprotocol.com)): JSON-RPC 2.0 over stdio, agents run as the client's subprocesses — exactly Hekma's supervision shape. Handshake = `initialize` (version + capability negotiation) + optional `authenticate`; sessions via `session/new` / `session/prompt` (a turn ends with a stop reason) / `session/cancel` / optional `session/load` (resume, capability-gated); streaming via `session/update` notifications (message chunks, thoughts, tool calls, plans); optional client capabilities: `fs.readTextFile`/`fs.writeTextFile`, a terminal suite, `elicitation`.
- **The protocol exposes NO token-usage or cost data** — verified against the spec overview. This forces the metering decision below (D2).
- PRD alignment is direct: **SM-1** (ratified north star) demands identical core controls across structurally different agents, "fully measured at v1.x when the opencode Adapter ships"; **SM-5** budgets ≤1 person-day to author an adapter for a new agent. A builtin `acp` kind delivers SM-1's intent for every ACP agent at ~zero per-agent cost — no adapter authoring at all.
- Hekma already owns the hard part: process supervision, fingerprints, adoption, Agent Homes, config mapping, and (from epic-12) durable detach — an ACP agent is a supervised process whose stdio speaks a known dialect.

## Section 2 — Impact Analysis

**Epic impact.**
- No epic is in flight (13 closed 2026-09-19); the pipeline was empty. **Impact = one new epic (14).** No existing epic is modified, deferred, or invalidated; epic ordering is unchanged (14 is simply next). The twice-carried supervisor concern is retired early by 13-3's split — the new ACP work lands against a six-file, family-organized supervisor.

**Story impact.** New stories (draft shape; full derivation at the epic's spec stage):

| Story | Shape |
|---|---|
| 14-1 | **ACP transport core** — builtin `acp` kind: spawn under existing supervision; `initialize` handshake (protocol-version pin + capability advertisement per D3); `session/new` at start; `send` = `session/prompt` turn; `session/update` stream → the existing per-instance output log + events; stop = `session/cancel` then the engine's normal termination ladder; pause/resume via existing process semantics (SIGSTOP parity unchanged). |
| 14-2 | **Sessions across lifetimes** — persist the last ACP session id with the spawn record; on re-adoption, `session/load` when the agent advertises `loadSession`, else `session/new` with an honest stderr note (D4). Detach (epic-12) composes: the process survives, the session resumes on reattach. |
| 14-3 | **ACP usage acquisition (D2, tiered)** — parse de-facto usage channels on the ACP stream (T1); wire the engine-observed base-URL path for ACP agents (T2); the `—` cell survives ONLY as the surfaced last resort naming the gap and tiers attempted; config mapping (env/args) and Memory Backing delivery unchanged; `--help`/notice honesty per the house pattern. |
| 14-6 | **Cached tokens across the metering pipeline (D8, ACP-independent)** — parse the three provider cached-token shapes; additive nullable `cached_tokens` ledger column; optional `Rate` cached rate with the conservative input-rate default; budgets count cached tokens; surfaces distinguish the cached contribution. |
| 14-5 | **Hermes-kind deprecation path (D7)** — acp-kind metering parity (optional self-reported sentinel over stderr) + HERMES_HOME proof under acp; deprecation announcement; real-hermes-over-acp verification. Actual `hermes` removal at a later major. |
| 14-4 | **Verification & docs** — a fake ACP agent binary in `hekma-conformance` (scriptable JSON-RPC over stdio) drives the test matrix (handshake, prompt turn, update capture, cancel/stop, load/no-load adoption, refusal of unadvertised capabilities); real-agent smoke against `gemini --acp` when present (skip otherwise); docs: commands.md, supported-agents, architecture AD-19, adapter-contract scope note. |

**Artifact conflicts.**
- **PRD:** no conflict — this *is* SM-1's measurement path. Addition: one new FR (FR-40) + SM-1/SM-5 notes. MVP scope untouched (post-v1 expansion, consistent with the product vision).
- **Architecture:** new **AD-19** (the engine gains a JSON-RPC *client* role for the `acp` builtin kind — transport, session lifecycle, capability advertisement); **AD-7** note: the `acp` kind is unmetered-by-protocol (honest lower bound); **adapter-contract scope note**: builtin kinds don't negotiate contract_version (epic-6 retro B3 precedent — mock/hermes already ride it), so the frozen v1 contract is untouched (D5). Per-OS code stays in `backends/` (AD-4): ACP framing is OS-uniform stdio, like the metering listener.
- **UI/UX:** none (terminal product; the Fleet table's honest `—` cells are the established pattern).
- **Secondary artifacts:** conformance crate gains the fake ACP agent (test-only); docs pages listed in 14-4; no CI-pipeline change beyond the new tests; no new heavyweight dependency (a minimal JSON-RPC codec is std+serde — NFR-8 lean-policy review at the spec stage; if a crate is warranted it must be lean, `no default features`).

**Technical impact.** Engine-internal plus one new builtin module family; no public embedding-API break (new `AdapterRef::Acp`-shaped additions are additive); the supervisor's new family layout (13-3) gives the ACP work a clean landing place. Lock model untouched (AD-18).

## Section 3 — Recommended Approach

**Option 1: Direct Adjustment — add Epic 14 to the plan.** (Options 2/3 evaluated and rejected: there is nothing to roll back — no in-flight work conflicts; the MVP is unaffected, so no scope reduction is warranted.)

- **Effort: Medium-High.** The transport core (14-1) is the bulk: JSON-RPC framing, handshake, session state machine, and the interaction mapping. 14-2/14-3 are thin riders on existing machinery (spawn records, adoption, honest cells). 14-4 is test-weight.
- **Risk: Medium.** Third-party ACP-agent behavior variance (capability advertisement is heterogeneous); protocol version drift (pinned + negotiated at `initialize` per D3/D6); the metering gap is a product-visibility risk, not a correctness one (D2 makes it honest).
- **Timeline:** next epic after 13 — nothing displaced.
- **Why this path:** it converts an industry standard into Hekma's integration surface at the exact moment of its adoption inflection, serves the ratified north star, and costs the frozen v1 contracts nothing (builtin-kind precedent).

## Section 4 — Detailed Change Proposals

**D1 — Integration shape (ratified by Islam 2026-09-19, this session):** a new **builtin `acp` kind** (sibling of `mock`/`hermes`). Hekma owns the ACP client role; per-agent launch command/args ride the manifest-like registration surface already used for builtins. The manifest-level ACP-transport alternative is recorded as a contract-v2 candidate, not built.

**D2 — Metering: TIERED REAL-USAGE ACQUISITION; `—` is the surfaced last resort, never the stance.** [AMENDED 2026-09-19 — Islam: "Tokens and cached tokens usage/metering is critical and must be included."] ACP's core `session/prompt` response carries only a stopReason today (the "End-Turn Token Usage" RFD is in draft on agentclientprotocol.com), but real agents already surface usage de-facto: Gemini/ADK-based agents emit UsageUpdate notifications from `usageMetadata`; Claude-based adapters carry Anthropic usage fields in `session/update` chunks; some agents (Copilot, cursor-agent) expose nothing. The `acp` kind therefore acquires REAL usage tiered: (T1) parse the de-facto usage channels on the ACP stream (usage-bearing `session/update` chunks and usage-update notifications, tolerant of absence and of `_meta` extension shapes); (T2) the existing engine-observed loopback channel (3-4) where the agent honors a provider base-URL override; (T3) the optional stderr KTESIO_USAGE sentinel (cooperative agents — hermes, via 14-5's mode). Only when NO tier yields usage does the instance show the honest `—` (METERING_SEED_CELL pattern) with a surfaced notice naming the gap and the tiers attempted — never fabricated counts, never silent absence (AD-8, AI-18).

**D3 — Client capabilities: minimal advertisement in v1.** Hekma advertises only the baseline (`session/request_permission` → v1 auto-denies with a surfaced diagnostic, or the agent's own confirmation flow is relied on — pinned at 14-1's spec stage); `fs.*`, `terminal`, `elicitation` are NOT advertised; if an agent requests them anyway it gets a typed, surfaced refusal. Rationale: NFR-6 promises no sandbox; honoring client-side fs/terminal calls is a v2 design with real security review (the "fs calls honored only inside the Agent Home" sandbox-flavored idea is explicitly a v2 candidate, not v1 behavior).

**D4 — Session persistence across lifetimes.** The last ACP session id is persisted alongside the spawn record (additive schema extension); adoption re-attaches via `session/load` when advertised, else `session/new` + honest note. This composes with epic-12 detach rather than fighting it.

**D7 — The bespoke `hermes` kind deprecates in favor of `acp` (ratified by Islam 2026-09-19, mid-review amendment).**
Hermes Agent now has first-class ACP support (`hermes-acp` stdio entry point; listed on agentclientprotocol.com's
agents page) — the bespoke `hermes` builtin (epic-6) is a special case of the `acp` kind. Epic 14 gains **story 14-5**:
(a) make the `acp` kind cover hermes' needs — an OPTIONAL self-reported metering mode for the `acp` kind that reads
the KTESIO_USAGE sentinel from the shared stderr agent.log (hermes cooperates; D2's default stays unmetered for
agents that don't), plus the HERMES_HOME memory-delivery proof under the `acp` kind; (b) land the deprecation
announcement (docs steer new registrations to `--kind acp`; the `hermes` kind keeps working; actual removal at a
later MAJOR release per the CLI-surface deprecation policy — no stranded instances, no silent metering loss);
(c) real-hermes-over-acp verification (14-4's smoke matrix gains the hermes entry point).

**D8 — Cached tokens across the whole metering pipeline (Islam's directive, same session).** The gap predates ACP: `metering/parse.rs` explicitly ignores `prompt_tokens_details` (OpenAI's `cached_tokens` home) and `total_tokens`, the `Rate` cost model carries only input/output prices, and the ledger has no cached column. Story 14-6 (ACP-independent — benefits every kind): ParsedUsage gains cached-token parsing for the three provider shapes (OpenAI `prompt_tokens_details.cached_tokens`; Anthropic `cache_read_input_tokens` + `cache_creation_input_tokens`; Gemini `cachedContentTokenCount`); the ledger gains an additive nullable `cached_tokens` column (next migration after v6 — the additive-nullable precedent); `Rate` gains an optional cached rate — **when unset, cached tokens price AT THE INPUT RATE (documented conservative default: overstates cost, never understates it); when set, at the cached rate**; token budgets count cached tokens (they are real billed tokens); fleet/config surfaces distinguish the cached contribution. Cached-token absence in a source parses as known-zero (not a fabricated zero for unknown fields — the honest-lower-bound rule).

**D5 — Contract v1 untouched.** `negotiate_contract_version` stays manifest-only (B3 precedent); `acp` is a builtin kind with its own protocol pin (ACP v1 at the spec stage, negotiated at `initialize`). Contract docs get a scope sentence, not a version bump.

**D6 — Protocol pin & drift.** Pin the ACP version RANGE at 14-1's spec stage (the docs site currently claims v1; Hermes tracks `agent-client-protocol >=0.8.1,<1.0` per its issue #3150 — the ecosystem is still churning); version negotiation at `initialize` refuses unsupported versions with a surfaced, traffic-free error (the 6-6 negotiation shape, transposed).

**PRD edit (old → new).**

```
Section: §4.x Functional Requirements (after FR-39)
NEW:
#### FR-40: ACP backend support
Any agent that speaks the Agent Client Protocol (ACP) v1 can be registered,
started, and supervised as a Hekma backend via the builtin `acp` kind, with
core controls (start/stop/pause/resume/send/stop-escalation, config, memory
backing, fleet surfaces, detach+adoption) behaving identically to other
kinds. ACP agents are UNMETERED in v1 (the protocol carries no usage data):
fleet/config surfaces show an honest not-available marker and no usage or
cost is ever fabricated for them. Client capabilities beyond the ACP
baseline are not advertised in v1; capability requests Hekma did not
advertise are refused with a surfaced diagnostic. [ADDED 2026-09-19 —
Sprint Change Proposal, Islam's directive; serves SM-1/SM-5.]

Section: Success Metrics — SM-1 note
OLD: ...fully measured at v1.x when the opencode Adapter ships.
NEW: ...fully measured at v1.x when the opencode Adapter ships. The `acp`
kind (FR-40) extends the same guarantee to every ACP-capable agent without
per-agent adapter authoring. [AMENDED 2026-09-19.]
```

**epics.md edit.** Append **Epic 14: ACP Backend Support** to the epic list + a full section with the four stories from Section 2 (declared ACs per story: e.g. 14-1 — a fake-ACP-agent e2e proves start→prompt→captured update→stop and that `send` produces a `session/prompt` turn on the wire; 14-2 — adoption with `session/load` vs `session/new` both pinned; 14-3 — the `—` metering cell and the refusal diagnostic pinned; 14-4 — docs + real-agent smoke). Frozen-boundary notes: AD-4 (no per-OS cfg outside backends), NFR-6 (no sandbox claim), AD-8 (no fabricated estimates), surfaced-not-silent throughout.

**Architecture edit.** New **AD-19 — The engine speaks ACP as a client for the builtin `acp` kind** (Binds/Prevents/Rule in spine format: JSON-RPC client role in a new engine module family; capability advertisement minimal per D3; protocol version negotiated and pinned per D6; metering honesty per D2 referencing AD-7; contract non-engagement per D5 referencing the B3 precedent; OS-uniform framing, AD-4 respected). Plus a one-line **AD-7** metering-source note (`acp` → unmetered-by-protocol) and the adapter-contract scope sentence.

**Tracker edit (applied after approval, per checklist 6.4).** `epic-14: backlog` + `14-1..14-4: backlog` rows with provenance comments pointing at this proposal.

## Section 5 — Implementation Handoff

**Scope: Moderate** — backlog reorganization; no PM/Architect replan required (the architecture decisions are drafted above for ratification-in-place; no existing artifacts are invalidated).

| Recipient | Responsibility |
|---|---|
| Product Owner (Islam) | Ratify the proposal + the six D-decisions; pick 14's start date. |
| Developer agent (next epic cycle) | Spec stage (derive the four stories' full ACs, pin the ACP v1 subset, settle the JSON-RPC dependency question under NFR-8) → implementation per the batch-epic shape (10–13 precedent), gates as usual. |
| Architect (light touch) | AD-19 text lands with the spine at 14-1's spec stage (draft above is the seed). |

**Success criteria.** `hekma agent register <name> --kind acp` + a launch command starts any ACP v1 agent under full supervision; lifecycle/config/memory/fleet behave identically (SM-1's bar) ; **real usage — tokens INCLUDING cached tokens — lands in the ledger and budgets enforce on it for every acquisition tier available (ACP stream channels, observed base-URL, stderr sentinel), with the honest `—` + surfaced gap notice only when no tier yields usage**; detach→adopt resumes the ACP session when the agent supports it; all four house gates green; docs current (NFR-7); the frozen v1 contracts show no diff (semver clean).

---

## Approval

**APPROVED by Islam 2026-09-19** (batch mode): the proposal as a whole and decisions D1–D6 ratified explicitly via the correct-course approval gate. **Amendment ratified same session:** D7 added (Hermes speaks ACP — the bespoke `hermes` kind deprecates via story 14-5, removal at a later major); D6 refined to a version RANGE (Hermes tracks 0.8.1–<1.0). Epic 14 = stories 14-1…14-6. **Second amendment ratified same session (Islam: metering critical):** D2 rewritten to tiered real-usage acquisition (`—` is the surfaced last resort, never the stance), D8 added (cached tokens across the whole pipeline — parse/ledger/rate/budgets/surfaces), story 14-6 added, execution order re-prioritized: 14-6 is FOUNDATION and runs right after 14-1. Consequence: Epic 14 + stories 14-1…14-4 added to `epics.md` and sprint-status (backlog); D2–D6 carry into the epic's spec stage as ratified constraints; AD-19's spine text lands at 14-1's spec stage from the seed in Section 4.
