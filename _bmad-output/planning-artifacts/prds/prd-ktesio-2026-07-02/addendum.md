---
title: "PRD Addendum — Ktesio Unified Personal-Agent Runtime Engine"
status: draft
created: 2026-07-02
updated: 2026-07-02
---

# PRD Addendum

Technical-how, options-considered, and carried context that the PRD deliberately keeps out of its main narrative. The brief's addendum (`../../briefs/brief-ktesio-2026-07-02/addendum.md`) remains the product-level research record (landscape §A, cost-governance analysis §B, Hermes analysis §C, memory patterns §D, v0.5.0 teardown §E, durable gates §F, adapter-contract considerations §G, dual-delivery context §I) — referenced here, not duplicated.

## 1. Metering mechanisms (feeds FR-19/FR-21 architecture)

Two declared Metering Sources, different fidelity/effort trade-offs:

- **`self-reported`** — the Adapter forwards the Agent's own usage accounting (Hermes Agent exposes `/usage` + analytics; brief addendum §C). Cheap to build, fidelity depends on the Agent, arrives in batches → enforcement latency is bounded by report cadence; reconciliation must be idempotent (no double-count on replays).
- **`engine-observed`** — the Adapter routes the Agent's model traffic through an Engine-provided interception point (env-injected base-URL/proxy is the common mechanism; cf. LiteLLM-style proxies, brief addendum §B — but Ktesio acts on *lifecycle*, not per-request rejection) *(Superseded 2026-10-30: Hekma also acts per request — `block`, wire hold and reroute, PRD FR-43 and FR-56.)*. Higher fidelity and real-time; requires the Agent's model client to honor endpoint override — true for most, not all.
- Rejected for v1: parsing provider dashboards/billing APIs for actuals (per-provider sprawl, auth burden). Reconciliation-with-actuals stays an interface concept (FR-23 labels) without a v1 implementation.

## 2. Process supervision per OS (feeds NFR-2, FR-5–FR-10)

- **Unix (Linux/macOS):** process groups + signals (SIGTERM→SIGKILL escalation for FR-6; SIGSTOP/SIGCONT as the *mechanism candidate* for guaranteed pause where the Agent tolerates it).
- **Windows:** Job Objects for group termination; no SIGSTOP analogue → pause on Windows is likely `best-effort` via Adapter cooperation (agent-native pause, or suspend threads with caveats). This asymmetry is why FR-7 makes honesty about pause semantics a first-class requirement rather than assuming parity.
- Orphan adoption after Engine crash (NFR-1): PID + start-time fingerprints persisted in the state store; reconcile on Engine start.

## 3. State store options (feeds FR-10)

Candidates: (a) JSON/TOML files per Agent Home + a small index (matches current codebase idioms, human-inspectable, weakest concurrent-write story), (b) embedded SQLite (transactional, concurrent-safe, one new dependency), (c) append-only event log + snapshots (best audit trail, most build). Usage Ledger durability bound (≤5s flush assumption) likely decides this toward (b) or (c). Architecture decides; PRD only fixes durability consequences.

## 4. Secret storage options (feeds FR-14)

(a) OS keychain (macOS Keychain / Windows Credential Manager / Secret Service) — best at-rest story, platform-API sprawl; (b) env-var passthrough only, never persisted — simplest, pushes burden to Operator; (c) file-based with 0600 perms + explicit warning — portable, weakest. Likely v1: (b) + (c) with (a) as fast-follow. `[Carries the FR-14 assumption; architecture decides.]`

## 5. Engine/CLI crate shape (feeds FR-31/FR-32, §7)

Workspace split: `ktesio-engine` (library crate, the Embedding Interface = its public API) + `kt` (bin crate depending only on the engine's public surface). FR-32's CI enforcement = the bin crate simply cannot reach non-public items; plus a semver-check job (e.g. cargo-semver-checks) on the engine crate. Event subscription shape: callback/channel-based subscriber registration in v1 (no network transport — that's the deferred service/IPC question, PRD Q6).

## 6. Migration mechanics options (feeds §4.10)

Legacy `skills.json` (project-scoped) → Agent-scoped Skill Set: (a) `kt migrate` assistant mapping a project manifest into a chosen Agent Instance's Skill Set; (b) docs-only manual path. Lean: (b) at pivot release, (a) if legacy usage warrants. Existing install/lock/git modules (`manifest.rs`, `lockfile.rs`, `git.rs`, `install_target.rs` — brief addendum §E) are the reuse surface; `skills_sh.rs` (search) has no home in the refocus (PRD Q7).

## 7. Conformance kit sketch (feeds FR-27, SM-1 interim measurement)

A test harness + a `mock` Adapter driving a scripted fake Agent: exercises every contract section (lifecycle transitions incl. crash, config mapping, both Metering Sources, memory attach, interaction, Capability Declaration edge cases like `pause: unsupported`). Doubles as (a) the Adapter author's TCK and (b) SM-1's interim measurement basis until Adapter #2 ships — which is exactly the §9.2 tension: the mock proves *contract* uniformity, not *real-world* uniformity. Candidate profiles for the real second Agent (PRD Q1, brief addendum §G) were: a single-shot LangGraph/OpenAI-SDK-style script agent, Aider, or a minimal custom agent. **Islam selected opencode (opencode.ai) on 2026-07-02.** First conformance-mapping step: characterize opencode's actual structure (session/TUI model, client/server split, provider config, usage surface) against Hermes to confirm the structural-distance axes it does and does not cover — supplement with the mock Adapter for any axis opencode shares with Hermes.

## 8. Licensing note (PRD Q2, decision material)

PolyForm Noncommercial today blocks commercial Hosts from embedding. Options when Islam decides: dual-license (noncommercial default + commercial grants), switch to a permissive/copyleft OSS license for the engine and keep something else for premium surfaces, or keep PolyForm and treat embedding as a licensed business motion. The PRD takes no position; GTM for the Host audience is gated on this, the build is not.
