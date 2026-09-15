---
title: "Addendum — Ktesio Unified Agent Runner"
status: updated
created: 2026-07-02
updated: 2026-07-02
---

# Addendum: Ktesio Unified Agent Runner

Supporting depth for the product brief. This material grounds the brief's claims and carries context forward to the PRD / architecture phases. It is not part of the 1–2 page brief itself. Research was gathered via web-search subagent on 2026-07-02; note the verification caveat at the end.

## A. Landscape scan (grounding the differentiation)

The current agent-tooling market splits into distinct categories, none of which is an operator-facing CLI unifying lifecycle + config + memory + dollar-cost governance across heterogeneous agents:

**1. Single-stack agent frameworks (run only their own agents).**
- LangGraph — durable/resumable stateful execution, but only for its own graphs. (https://www.langchain.com/resources/ai-agent-frameworks)
- CrewAI, AG2/AutoGen (AutoGen now in maintenance; succeeded by **Microsoft Agent Framework**, Oct 2025), OpenAI Agents SDK, Google ADK — each single-stack. (https://alicelabs.ai/en/insights/best-ai-agent-frameworks-2026)

**2. Single-agent CLI harnesses (drive one agent loop; no cross-framework supervision or cost caps).**
- Goose (Block; now under Linux Foundation), Aider, OpenHands (~79k★), Letta Code. (https://pinggy.io/blog/best_open_source_cli_coding_agents/, https://github.com/bradAGI/awesome-cli-coding-agents)

**3. Multi-agent parallel launchers (orchestrate many agents, but no unified token/cost governance).**
- Emdash (Electron) runs ~22 CLI providers — Claude Code, Codex, Goose, Gemini, **Hermes Agent**, etc. — in parallel via git worktrees + per-task setup/teardown/port scripts. Process orchestration, not governed running. (https://www.augmentcode.com/tools/open-source-agent-orchestrators, https://emdash.sh/docs/providers)

**4. Governance SDKs (embedded in code, not a CLI you operate).**
- **Microsoft Agent Governance Toolkit** (Apr 2026) — strongest prior art for "container-runtime/process-supervisor for agents." Wraps LangChain/AutoGen/CrewAI/ADK/OpenAI/LlamaIndex via `BaseIntegration` adapters exposing pre/post-exec hooks, policy, a kill switch, and "Agent OS / Runtime / SRE" layers. But it is a library, not an operator CLI. (https://opensource.microsoft.com/blog/2026/04/02/introducing-the-agent-governance-toolkit-open-source-runtime-security-for-ai-agents/, https://deepwiki.com/microsoft/agent-governance-toolkit/7.1-execution-rings)

**5. Isolation / packaging runtimes (supervise agents as workloads; focus on isolation, not $/token caps).**
- Docker `docker agent` plugin, Northflank, **Microsoft Execution Containers (MXC)** (Build 2026, kernel-enforced). (https://northflank.com/blog/top-ai-agent-runtime-tools, https://cloudnativenow.com/features/microsoft-introduces-execution-containers-to-keep-ai-agents-in-check/)

**6. Interop protocols (agents discover/talk to each other; not a runner).**
- MCP + A2A, both now Linux Foundation. Cross-framework interop in 2026 is being solved at the protocol layer, not by a supervising runner. (https://openagents.org/blog/posts/2026-02-23-open-source-ai-agent-frameworks-compared)

**Read:** the specific niche — a `pm2`-like CLI unifying lifecycle *and* dollar-denominated cost governance across heterogeneous agents — appears genuinely underserved. Treat as a validated hypothesis, not a proof.

## B. Cost / token governance today (why runner-level enforcement is different)

Existing hard-cap and tracking tools are **API-interception layers on the request path**, architecturally distinct from controlling an agent's lifecycle:

- **LiteLLM proxy** — strongest hard caps: per-key/user/team/tag budgets, multi-window resets, automatic request rejection at limit (Redis-backed). (https://docs.litellm.ai/docs/proxy/users, https://docs.litellm.ai/docs/proxy/tag_budgets)
- **Helicone** — proxy observability + cost-optimizing routing. (https://docs.helicone.ai/guides/cookbooks/cost-tracking)
- **Langfuse** — tracing/spend visibility, not hard caps. **Cloudflare AI Gateway** — analytics + caching, lighter enforcement. **OpenMeter** — metering/billing. (https://techsy.io/en/blog/best-llm-gateway-tools)

**Design implication for Ktesio.** Deriving a dollar cap from a $/1M-token rate and enforcing it *at the runner level* (pausing/stopping the agent process) is different from a proxy rejecting individual API calls. Ktesio could optionally *wrap* a proxy like LiteLLM for request-level enforcement, but its distinctive control point is the agent lifecycle. Caveat to preserve: rate-derived dollar figures are **estimates** unless the agent/provider reports actuals — enforcement fidelity and the estimate/actual boundary must be explicit in the PRD.

## C. "Hermes" analysis (reference adapter + de-confliction)

**Confirmed identity (Islam, 2026-07-02): NousResearch Hermes Agent** (open-source, released ~Feb 2026) — "the agent that grows with you": a self-improving agent living on a server, with persistent memory, auto-created skills, and multi-channel connectivity (Telegram/Discord/Slack/WhatsApp/Signal/CLI) from one **gateway** process. (https://github.com/nousresearch/hermes-agent, https://hermes-agent.org/)

- It is a **single self-contained agent, not a framework-agnostic runner** — but it *is* model-agnostic (OpenRouter/OpenAI/Anthropic/Gemini/Ollama/Bedrock via `hermes model`).
- Partial lifecycle surfaces already exist: `hermes gateway run/start/setup/enroll`, s6-overlay/Docker supervision, `/usage` + `analytics` for token/cost breakdown, loop/iteration guardrails (hard-stop in unattended mode). No native `stop`/`status` or $-denominated cost cap was found. (https://hermes-agent.nousresearch.com/docs/reference/cli-commands, https://hermes-agent.nousresearch.com/docs/user-guide/configuration)
- **Distinct from the fine-tuned models.** NousResearch **Hermes 2/3** are generalist fine-tunes of Llama 3.1 (8B/70B/405B) and Mistral/Mixtral (ChatML), 33M+ downloads — a different thing from Hermes *Agent*. (https://nousresearch.com/hermes3, https://huggingface.co/NousResearch/Hermes-3-Llama-3.1-8B)

**De-confliction consequence.** If Ktesio's reference agent is Hermes Agent, Hermes' own gateway/supervision/memory/analytics **overlap** Ktesio's proposed scope. Hermes becomes an ideal reference adapter *and* a partial competitor. Positioning must make the **cross-agent / heterogeneity** value primary; single-agent operation is table stakes. This is why the adapter contract must be paper-checked against a *second, structurally different* agent — to prove the abstraction is not Hermes-shaped.

**Resolved:** Islam confirmed on 2026-07-02 that "Hermes" means this public project. The remaining Hermes-adjacent open question is only which *second, structurally different* agent the contract is checked against.

## D. Agent memory patterns (informs "memory wiring")

Two dominant patterns the memory-wiring interface will likely need to accommodate:
- **Memory layer bolted onto any framework** — **Mem0** extracts/dedupes facts into a vector store (ADD/UPDATE/DELETE/NOOP), framework-agnostic. (https://vectorize.io/articles/mem0-vs-letta)
- **Agent runtime with OS-style tiers** — **Letta/MemGPT**: core (RAM) / recall (history) / archival (vector disk), self-editing via tool calls.
- Plus vector stores, conversation buffers, and temporal knowledge graphs (Zep/Graphiti). (https://codepointer.substack.com/p/agent-memory-systems-and-knowledge)

Implication: "memory wiring" is a spectrum from "attach a vector store" to "delegate to an agent's own tiered memory runtime." The v1 interface scope (which backings, how much Ktesio owns vs delegates) is open question #8.

## E. Current Ktesio (v0.5.0) — what is being repositioned away from

For migration/deprecation planning in the PRD. Source: repo `README.md`, `docs/architecture.md`, `Cargo.toml`, `AGENTS.md` (all read 2026-07-02).

- **What it is today:** a Rust single-binary CLI (`kt`) — an *agent-skills package manager*. Installs/shares reusable skill directories from git into `.agents/skills/`, tracked by a small `skills.json` manifest and reproducible `skills.lock`; searches skills.sh listings; publish/upgrade/doctor/uninstall commands. ~12.5k LOC.
- **Distribution:** crates.io (`cargo install ktesio`), Homebrew (`ktesio/tap/ktesio`), install script at cli.ktesio.dev, GitHub Releases. License: PolyForm Noncommercial 1.0.0.
- **Existing modules (src/):** `main.rs` (clap dispatch), `cli/` (handlers), `manifest.rs`, `lockfile.rs`, `git.rs`, `install_target.rs`, `install_channel.rs`, `skills_sh.rs`, `skill.rs`, `discovery.rs`, `ui.rs`, `error.rs` (miette), `update_check.rs`. Stack: clap, miette, indicatif, serde, ureq, dialoguer.
- **Reusable for the pivot (likely):** the CLI scaffold (clap dispatch, `--help`/`--version`), terminal UX layer (`ui.rs`), miette diagnostics, git wrapper, cross-platform packaging/distribution, the `kt` binary name and release/CI machinery. The git-native distribution model may also inform how *adapters* or *agents* are fetched.
- **Being demoted/retired:** the skills manifest/lockfile/publish/search domain — unless skills map onto agent provisioning (open question #6).
- **`AGENTS.md` already documents the pivot** and instructs contributors to keep shipping-behavior docs (README/docs) describing the skills CLI until runner features land via a BMAD story. It re-ratifies the durable gates below.

## F. Durable engineering gates (verbatim, carried from the retired constitution)

From `AGENTS.md` — hold across the pivot, re-ratified as the BMAD PRD/architecture lands:

- **CLI-first** — every feature reachable via `kt`; stdout for output, stderr for diagnostics; `--help`/`--version` on all commands.
- **Test coverage MUST stay ≥ 95%** — enforced in CI via `cargo tarpaulin --fail-under 95`; new code ships with tests.
- **Documentation currency** — update `docs/` and `README.md` in the same change as the code; stale docs are a bug.
- **Cross-platform** — Linux, macOS, Windows; path-agnostic std APIs.
- **Graceful degradation** — partial failures report a clear reason + remediation and do not abort the whole operation.

Pre-handoff checks in the repo: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`, `python3 scripts/check_docs.py`.

## G. Adapter-contract considerations to carry into architecture

Product-level notes (the brief keeps these out; the architecture phase should address them):
- What the contract must abstract: process lifecycle (start/pause/resume/stop + state), config injection, memory attachment, token/cost metering hooks, and an interaction channel.
- **Pause/resume is the hardest guarantee** — in-flight tool calls, external side effects, and persistent gateway connections mean some agents can only be *best-effort* paused. The contract should distinguish guaranteed vs best-effort semantics.
- **Metering source of truth** — does Ktesio count tokens itself (wrapping the model call / proxy), or consume the agent's self-reported usage (as Hermes' `analytics` provides)? Mixed sources complicate the dollar cap.
- **The second-agent check** should be chosen to be structurally *unlike* Hermes Agent (e.g. a single-shot framework agent with no gateway and no native memory) to maximally stress the abstraction.
- **Upward embedding interface** (added 2026-07-02 with the dual-delivery decision): how a host platform drives the engine — lifecycle commands in, budget/config injection in, state/events/telemetry out. Keep it distinct from the downward adapter contract; the CLI consumes the same upward surface (`kt` as the reference frontend), which is itself the embeddability proof.

## H. Research verification caveat

The web-research subagent reported that direct page-fetch (WebFetch) was unavailable during the run; all Hermes Agent and Emdash specifics rest on **search-result excerpts of primary sources** (NousResearch GitHub/docs, Emdash changelog) rather than directly fetched pages — high-confidence but not independently page-verified. The market-gap conclusion is a grounded hypothesis, not an exhaustive competitive proof. Recommend a light re-verification pass (or a `bmad-market-research` run) before treating any competitive claim as settled.

## I. Dual delivery (engine + CLI) — carried context for PRD/architecture

Confirmed by Islam 2026-07-02. Ktesio's core identity is a personal-agent **runtime engine** with two delivery surfaces: the `kt` CLI (solo/technical operators) and an embeddable engine core (agent-hosting platforms/apps). Carry forward:

- **Engine core decoupled from the CLI.** `kt` is the engine's first and complete consumer — this preserves the CLI-first gate and doubles as the embeddability proof.
- **Two contracts, kept distinct:** the *downward* agent-adapter contract (agent side) and the *upward* embedding interface (host side). Architecture must define both; the brief fixes only their existence and separation.
- **Hosts get** resilience, consistency, predictability for the personal agents they run; **hosts keep** user management, billing, policy, and any hosted UI (all out of Ktesio scope).
- **Licensing tension:** PolyForm Noncommercial constrains commercial embedders — resolve licensing/positioning before courting host integrations (brief open question #2).
- **PRD to define:** engine-vs-CLI feature-parity expectations; what "embeddable" means for v1 (Rust library API first; service/IPC later is an open architecture question); and how agent-scoped skills provisioning (reusing v0.5.0 install/lock machinery) surfaces in both deliveries.
