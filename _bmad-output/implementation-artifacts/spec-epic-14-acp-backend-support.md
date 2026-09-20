---
title: 'Epic 14: ACP Backend Support (14-1..14-6, batch epic)'
type: 'feature'
created: '2026-09-19'
status: 'in-progress'
review_loop_iteration: 0
baseline_commit: 702571ed4a98b5ce9c6c191aa1c6d42e0da0aafb
context: ['_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-19-acp.md']
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Integrating any new backend agent into Hekma costs bespoke adapter work, while the industry has converged on the Agent Client Protocol (ACP) — Zed's JSON-RPC-over-stdio standard, spoken natively by Gemini CLI and Hermes Agent (`hermes-acp`) and via adapters by Claude Code. Hekma cannot currently front any of them without per-agent work, and its metering pipeline silently drops cached tokens even for the agents it does support.

**Approach:** A new builtin `acp` kind makes any ACP-speaking agent a Hekma backend: the engine gains a JSON-RPC **client** role over the child's stdio (ndJSON framing), driving `initialize` → `session/new` → `session/prompt` turns, routing `session/update` notifications into the existing output-log/event machinery, and stopping via `session/cancel` + the normal termination ladder. Usage metering is CRITICAL (Islam's directive): the engine acquires REAL token usage tiered — (T1) ACP `usage_update` notifications (context-grain), (T2) the engine-observed loopback channel where the agent honors a base-URL override (billing-grain), (T3) the optional stderr sentinel (billing-grain) — with the honest `—` only as the surfaced last resort. Cached tokens become first-class across the WHOLE metering pipeline (parse → ledger → rate → budgets → surfaces), for every kind.

## Boundaries & Constraints

**Always:**
- Execution order 14-1 → **14-6** (foundation) → 14-3 → 14-2 → 14-5 → 14-4; each story ships with its own tests and stays independently verifiable.
- Gates in the same change: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, `python3 scripts/check_docs.py`; coverage ≥ 95% stays green.
- Docs updated in the same change as behavior: `docs/commands.md` (the `acp` kind), `docs/agents.md` (supported agents), `docs/architecture.md` (AD-19), `docs/adapter-contract.md` (builtins-don't-negotiate scope sentence), `docs/troubleshooting.md` (ACP failure modes).
- Protocol reference: ACP v1 docs at agentclientprotocol.com (`/protocol/v1/*`), pinned at this spec's writing: ndJSON framing (UTF-8, one JSON-RPC message per `\n`, no embedded newlines, stdout purity), `initialize` first (client speaks first; single-integer protocolVersion; agent echoes or counters; mismatch → close + surfaced error), `session/new` (cwd, mcpServers), `session/prompt` (ContentBlock[]; response = stopReason only: `end_turn|max_tokens|max_turn_requests|refusal|cancelled`), `session/update` (agent_message_chunk, agent_thought_chunk, tool_call, tool_call_update, plan, usage_update{used,size,cost?}, available_commands_update, current_mode_update, …), `session/request_permission`, `session/cancel`, optional `session/load` (loadSession capability).
- **JSON-RPC codec: hand-rolled over `serde_json` (already a dependency) — NO new crates.** Tolerant parsing (unknown update variants → a counted `Unhandled{discriminator}` surfaced honestly); the official `agent-client-protocol` Rust SDK was evaluated and rejected for v1 (fast SDK churn vs. the protocol integer; lean-dependency policy NFR-8). Revisit if the RFD surface grows.
- Client capabilities advertised at `initialize`: the PROTOCOL DEFAULT (no fs, no terminal, no elicitation — D3). An incoming `session/request_permission` is answered with the denial option (or `cancelled` if none fits) and a surfaced diagnostic per request.
- Surfaced-not-silent: every degraded/skipped/unhandled ACP message emits a diagnostic; capability refusals name the capability and the reason; the `—` metering cell, when shown, names the tiers attempted (AI-18).
- No-leak discipline: ACP diagnostics never echo prompt content, file contents, or auth material.
- `usage_update` is CONTEXT-grain (`used`/`size` = session context window tokens, optional cost): it is surfaced (log + `show`/`--json` additive field) but is **never minted into the billing ledger** — the input/output/cached billing shape comes only from T2/T3. Billing-cell honesty distinguishes "context usage reported; billing-grade unavailable" from "no usage signal".
- Schema migrations are additive nullable columns per precedent: 14-6 `cached_tokens` (v7), 14-2 `acp_session_id` (v8).
- Keep AD-17/AD-18 lock discipline: no unbounded work under the coarse locks; ACP stream reading lives on dedicated tasks (the metering-listener precedent), never on the engine mutexes.

**Ask First:**
- Any new dependency beyond `serde_json` (including the official ACP SDK).
- Advertising ANY client capability beyond the protocol default (fs/terminal/elicitation — D3 forbids in v1).
- Supporting a second ACP transport (Streamable HTTP is draft-only upstream).
- A concurrent-prompt policy other than "refuse the second prompt with a surfaced in-flight error" (v1 serializes turns per session).
- Any change to the frozen exit-code table beyond what a new error maps to within the existing rows.

**Never:**
- No fabricated usage counts, no fabricated zeros for unknown source fields, no cost derivation from `usage_update`'s optional cost alone (context-grain) — AD-8.
- No writes to the agent's stdin except well-formed ACP messages (framing purity); no reading of agent stdout as anything but ACP.
- No client capability advertisement beyond the default; no fs/terminal/elicitation handling in v1 (NFR-6: no sandbox claims).
- No per-OS cfg outside `backends/` (AD-4) — ndJSON/JSON-RPC framing is OS-uniform.
- No `contract_version` negotiation for the `acp` kind (builtin; epic-6 B3 precedent) — no adapter-contract v1 engagement (D5).
- No daemon/persistent engine session (AD-18/AI-20 unchanged); no telemetry home.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Register an ACP agent | `agent register <name> --kind acp` + launch command (via `acp.command`/`acp.args` config keys) | Registers like any builtin; `start` refuses honestly until `acp.command` is set | missing command → surfaced refusal naming the config keys |
| Start + handshake | `start` on an acp instance | spawn via backend → `initialize` (version 1, default caps, clientInfo) → agent's caps read (`loadSession` recorded) → `session/new` (cwd = Agent Home) → state `running` | garbage/non-JSON on stdout → surfaced parse failure + reconcile; version counter outside tolerated set → close + surfaced refusal |
| Send a prompt | `agent send <name> "…"` | one `session/prompt` (text ContentBlock) written (bounded by the stdin-write bound); send returns immediately; turn streams to log/events; stopReason lands in log + events | second prompt while a turn is in flight → surfaced in-flight refusal; stdin write timeout → the AI-59 bounded failure shape |
| Updates stream | agent emits `session/update` | chunks/tool-calls/plan appended to the per-instance output log + events (existing machinery); `usage_update` → context-usage surfaced (log + additive `show --json` field); unknown variant → counted, surfaced `Unhandled` diagnostic | malformed line → surfaced, skipped, counted (never fatal to the stream) |
| Permission request | agent sends `session/request_permission` | answered denied (or `cancelled` when no denial option exists) + one surfaced diagnostic | agent aborts turn on denial → normal stopReason path |
| Stop | `agent stop` | `session/cancel` (if a turn is in flight) then the engine's normal termination ladder; record settles normally | agent ignores cancel → SIGTERM ladder as today |
| Pause / resume | SIGSTOP/SIGCONT parity | unchanged from any process kind (guaranteed on unix; documented per-OS) | unchanged |
| Stop engine mid-turn | CLI exits / crash | process handled by existing drop/adoption machinery; on re-adoption the ACP connection is re-established (14-2) | enforcement window as with detach — documented honestly |
| Re-adoption (14-2) | engine restart, agent alive, session id persisted | `session/load` when the agent advertised `loadSession` (turns resume); else `session/new` + honest stderr note | load failure → fall back to `session/new` + surfaced note (never fatal to adoption) |
| Usage via observed (14-3 T2) | agent honors a base-URL override pointed at the loopback listener | provider responses parsed by the existing observed pipeline — billing-grade input/output/**cached** into the ledger; budgets enforce | as the observed channel behaves today |
| Usage via sentinel (14-3 T3) | cooperative agent writes KTESIO_USAGE to stderr | existing self-reported ingest path (14-5 enables the mode for the acp kind) | unchanged AI-41 durability |
| No usage anywhere | none of T1-billing/T2/T3 yields billing-grain usage | Fleet token/cost cells: honest `—` + gap notice naming tiers attempted and the context-usage state | never fabricated, never silent |
| Cached tokens (14-6) | any kind's usage source reports `prompt_tokens_details.cached_tokens` / Anthropic cache fields / Gemini `cachedContentTokenCount` | `cached_tokens` lands in the ledger; cost derives at the cached rate (input rate when unset — conservative); budgets include cached; surfaces distinguish | unknown fields stay uninterpreted (honest lower bound) |
| Hermes over ACP (14-5) | `hermes-acp` under the acp kind + sentinel mode | hermes' existing metering continuity (T3) + HERMES_HOME delivery proof under the acp kind; docs announce the `hermes` kind's deprecation | none removed in v1 — the legacy kind keeps working |

## Code Map

- **New engine module family** `crates/hekma-engine/src/acp/` — `codec.rs` (ndJSON + JSON-RPC 2.0 envelopes over the child's stdio; serde_json only), `client.rs` (initialize/session/prompt/cancel request-response correlation; request ids), `updates.rs` (SessionUpdate tolerant parser incl. `usage_update`), `connection.rs` (reader/writer tasks, in-flight turn state, permission answering). OS-uniform (AD-4).
- `crates/hekma-engine/src/domain/supervisor/spawn.rs` — acp-kind spawn wiring (launch from `acp.command`/`acp.args` config; refusal when unset); `interaction.rs` — `send_input`'s acp arm (session/prompt dispatch; in-flight refusal); `reaper.rs`/`mod.rs` — connection teardown on drop/stop; adoption hook for 14-2.
- `crates/hekma-engine/src/ports/process_backend.rs` — SpawnSpec/session-id plumbing only as needed (schema rides spawn records like `detach` did, schema v8).
- `crates/hekma-engine/src/metering/parse.rs` — 14-6: cached-token parsing (today line ~53 explicitly ignores `prompt_tokens_details`); `domain/cost.rs` — Rate cached rate (conservative default); `store/sqlite.rs` — v7/v8 additive migrations; `ports/usage_source.rs` + `domain/supervisor/usage.rs` — ledger/enforcement/surface wiring.
- `crates/kt/src/cli/agent.rs` + `main.rs` — `--kind acp` registration, `show --json` additive usage fields, honest cells; `crates/kt/src/exit_code.rs` — only if a new row is forced (avoid).
- `crates/hekma-conformance/src/bin/` — fake ACP agent (`fake_acp_agent`): scriptable ndJSON agent (handshake, chunked updates incl. `usage_update`, tool-call+permission, load/no-load, version counter, malformed-line mode); tests: `crates/hekma-engine/tests/acp_lifecycle.rs` (new), extensions to `observed_metering.rs` (cached tokens), `crates/kt/tests/agent_cli.rs` (register/start/send/stop e2e).
- Docs: `docs/commands.md`, `docs/agents.md`, `docs/architecture.md` (AD-19), `docs/adapter-contract.md`, `docs/troubleshooting.md`, `docs/design/metering-agents-you-dont-control.md` (tier design).

## Tasks & Acceptance

**Execution order: 14-1 → 14-6 → 14-3 → 14-2 → 14-5 → 14-4.**

- [x] 14-1 `src/acp/` codec+client+connection, spawn wiring, `send`→`session/prompt`, updates→log/events, stop ladder with cancel, permission auto-deny + diagnostic, in-flight refusal, version negotiation (tolerated set {1}) — the transport core
- [x] 14-6 `parse.rs` three cached shapes + `Rate` optional cached rate (conservative default) + ledger v7 `cached_tokens` + budget counting + fleet/config surface distinction + tests per shape and per absence — cached tokens, every kind
- [x] 14-3 T2 observed base-URL path for acp instances + T3 sentinel mode flag + `usage_update` surfacing (log + `show --json`) + the honest `—` gap notice naming tiers — metering acquisition for ACP
- [x] 14-2 session-id persistence (v8) + `session/load` on adoption when advertised, `session/new` + note otherwise; detach composes — sessions across lifetimes
- [x] 14-5 sentinel mode for the acp kind proven against `hermes-acp` + HERMES_HOME delivery proof under acp + deprecation docs for the `hermes` kind — the retirement path
- [x] 14-4 `fake_acp_agent` + `acp_lifecycle.rs` matrix + kt e2e + real-agent smoke (`gemini --acp` when present, skip otherwise, surfaced) + full docs sweep + AI-55 release-surface announcement — verification & docs

**Acceptance Criteria:**

- Given an ACP v1 agent binary, when registered with `--kind acp` and a launch command and started, then the handshake completes, state reaches `running`, a sent prompt streams the agent's chunks into the output log and events, and stop terminates cleanly after `session/cancel`.
- Given a turn in flight, when a second `send` arrives, then it is refused with a surfaced in-flight error and the first turn is unaffected.
- Given the agent emits `usage_update`, when updates arrive, then the context-usage figure is surfaced (log + `show --json`) and is NOT written into the billing ledger.
- Given usage from ANY tier of the billing path (observed base-URL or sentinel), when a turn completes, then input/output/**cached** token counts land in the ledger exactly once (dedup keys stable) and token budgets enforce on the total including cached.
- Given a source reporting a known cached shape, when parsed, then `cached_tokens` persists and cost derives at the configured cached rate — or at the input rate when none is configured, surfacing the conservative default in docs.
- Given no billing-grade usage source, when the fleet is listed, then the token/cost cells show the honest `—` with a notice naming the tiers attempted (and the context-usage state), and nothing is fabricated.
- Given an agent advertising `loadSession`, when the engine exits and the next command re-adopts, then the session resumes via `session/load`; otherwise a new session opens with an honest note.
- Given all stories merged, the four house gates pass, coverage ≥ 95% holds, the adapter contract's public surface shows no diff (semver clean), and no new dependency enters the tree.

## Spec Change Log

- **Loop 0 (spec stage, 2026-09-19).** From the approved Sprint Change Proposal + two ratified amendments: D2 rewritten to tiered real-usage acquisition (metering is critical — Islam); D8 added (cached tokens across the pipeline; the local gap is pinned: `parse.rs` ignores `prompt_tokens_details`, `Rate` lacks a cached price, the ledger lacks the column); D7 added (hermes kind deprecates via 14-5); D6 refined to a version-range/counter posture (SDK churn ≠ protocol integer). Protocol research pinned: ndJSON framing; default clientCapabilities = advertise-nothing (D3 is the protocol default); `usage_update` is STANDARD v1 (used/size/cost) but CONTEXT-grain — the billing ledger is fed only by T2/T3; StopReason enum; SessionUpdate discriminator set. Codec decision: hand-rolled serde_json codec, official SDK rejected for v1 (NFR-8, churn), revisit on RFD growth.
- **Closeout (14-4, 2026-09-19) — epic verification complete.** All six story task rows ticked; the epic landed in execution order 14-1 → 14-6 → 14-3 → 14-2 → 14-5 → 14-4 with every gate green at closeout (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, `python3 scripts/check_docs.py`), the dependency proof held (`cargo tree -p hekma-engine -e normal` shows NO new dependency — the codec is serde_json only, NFR-8 held), and the adapter-contract surface claim held at the API level — with ONE finding to record: the raw `git diff --stat 54dc79c46f2c538762a4743898c3014e34ffcfe8 -- crates/hekma-adapter-api` is NOT empty, but its provenance is (a) story 14-6's a94c9b1 touching `src/metering.rs` DOC COMMENTS ONLY (8+/1-, every added line a `//!` documentation of the input-inclusive `cached_tokens` convention on the existing documentary usage-line contract — no type, trait, signature, or key-set change, so semver-clean in the cargo-semver-checks sense), and (b) the pre-epic v0.9.0 relicense 14fbf77 (Cargo.toml `license-file` → `license` metadata only, ancestor of 14-1's 3833c28). The engine's own surface grew only additively (`EngineError::AcpTurnInFlight`, `SpawnSpec.pipe_stdout`, the defaulted `take_stdin`/`take_stdout`) with the semver baseline re-pinned at the 14-1 merge as recorded in that story's checklist.. **Story-recorded deviations, ratified in-flight:** (1) 14-3's discovery that an acp instance's self-reported sentinel channel is the agent's **stderr** (its stdout is the protocol stream — the spec's T3 row said "stderr sentinel" for acp and that is what shipped); (2) 14-2's ruling that an adopted acp process is NOT re-piped, so the session resume rides the **next start**'s handshake (`session/load` when advertised, `session/new` + surfaced note otherwise) — adoption surfaces the recorded-session state honestly; (3) the **input-inclusive cached invariant** (14-6/14-5): `cached_tokens` is a subset of `input_tokens` (`0 ≤ cached ≤ input`, the OpenAI convention, normalized at the parse seam), and the hermes-shaped stderr-sentinel line carries the full in/out/cached billing vocabulary under the acp kind. **14-4 verification delivered:** the kt-level e2e now lives in `crates/hekma/tests/agent_cli.rs` (three tests: the register → missing-command-refusal → full-handshake `start` → honest-gap journey cross-OS; a sentinel turn through the re-exec helper landing 40/20/25 in the ledger with the gap GONE cross-OS; the adopted-survivor resume-at-next-start note `_unix`); the `gemini` real-agent smoke was added next to 14-5's `hermes-acp` smoke (both known flag spellings tried; a both-flags failure is a REAL failure, never a skip). **Smoke outcomes on the closeout machine:** neither `gemini` nor `hermes-acp` is present on PATH — both smokes pass as honest skips (the skip-unless-present posture the spec pins); run `cargo test -p hekma-engine --test acp_lifecycle real_` on a machine with the agents. **Honest reach notes:** the `acp_context_usage` FIGURE is live-supervision state (the connection's in-memory record), so no cross-invocation binary test can observe it — the turn+figure proof lives in the engine-level `usage_update_is_surfaced_as_context_and_never_billed` (one in-process lifetime) plus the CLI cell-renderer unit tests; and the in-flight refusal's exit code 4 is pinned at the kt layer by the `exit_code.rs` classifier + `cli::agent` mapper tests (both in the `hekma` crate), because a live ACP turn exists only inside ONE engine lifetime and is unreachable through separate binary invocations (AD-18/AI-20). **Docs sweep (the deferred 14-1/14-2/14-3 items, verified + completed):** `commands.md` (acp lifecycle semantics: handshake, the in-flight refusal, resume-at-next-start, detached refusal, the two usage grains; the additive show --json fields; exit-code row 4's acp cause), `agents.md` (the ACP agents section: Gemini CLI, Claude Code via adapter, Hermes via `hermes-acp`, the metering tiers, the contract-validated-not-agent-pinned honesty), `troubleshooting.md` (the ACP failure-modes section: version counter, launch failures, malformed lines, in-flight refusal, re-adopted send, the `—` gap meaning; the usage-zeros bullet now names the acp stderr channel), `architecture.md` (the `acp/` module in the engine tree + the AD-19 backend/tiers section), `testing.md` (the ACP test matrix + the real-agent smoke run command), `embedding.md` (the 0.5.0 additive-surface note + the `acp` kind in the facade table). The AI-55 release-surface announcement (CHANGELOG banner) landed with 14-1 and was re-verified at closeout.

## Design Notes

- **The two usage grains must never mix.** `usage_update.used` counts session-context tokens (the agent's window), not the billed input/output split. Minting it into `usage_events` would fabricate a billing record — forbidden. The fleet's billing cells stay `—` without T2/T3 even when context usage is live; the gap notice says exactly that. If the End-Turn Token Usage RFD lands billing-grain fields, adopting it is a spec amendment.
- **Correlation, not frameworks.** The client is a small state machine: outgoing request id → oneshot reply channel; notifications fan into the log/event path; one in-flight-turn flag. No actor framework, no new runtime — tokio tasks on the engine's existing runtime handle (AD-13), read loop mirrors the metering listener's task shape.
- **Bounded holds.** The stdin write keeps the AI-59 bound; the reader task never takes the engine mutexes except for short, bounded state swaps (the usage-drain shape). Turn completion is asynchronous — `send`'s latency contract stays constant-time.
- **Fake agent = contract twin.** `fake_acp_agent`'s scriptable modes double as the protocol documentation in tests: every matrix row above maps to one mode; the malformed-line and version-counter modes pin the surfaced-failure diagnostics.
- **Deprecation is an announcement, not a removal.** 14-5 changes docs and adds the acp-side parity proofs; the `hermes` kind's removal is a future MAJOR-release decision under the CLI-surface policy (FR-38's shape), never this epic.

## Verification

**Commands:**
- `cargo fmt --all --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — zero warnings
- `cargo test --workspace --all-targets` — all pass, incl. the new `acp_lifecycle` matrix, cached-token suites, kt e2e
- `python3 scripts/check_docs.py` — passes (docs swept in the same changes)
- Semver: adapter-api/engine public surface shows no diff against the pinned baseline (builtin additions only)
- Real-agent smoke (14-4, skipped honestly when the binary is absent): `gemini --acp` and `hermes-acp` register → start → send → stop; usage surfaces per the matrix

## Suggested Review Order

- The codec + connection state machine — framing purity, bounded holds, tolerant parsing
  [`src/acp/codec.rs`, `src/acp/connection.rs` (new)]
- Spawn/handshake wiring + in-flight refusal
  [`domain/supervisor/spawn.rs`, `interaction.rs`]
- Cached tokens: parse → ledger → rate → budgets → surfaces (the D8 chain)
  [`metering/parse.rs`, `domain/cost.rs`, `store/sqlite.rs`, `domain/supervisor/usage.rs`]
- Metering tiers + the honest gap notice
  [`ports/usage_source.rs`, `domain/supervisor/usage.rs`]
- Session persistence + load-on-adoption
  [`store/sqlite.rs` (v8), `domain/supervisor/reaper.rs`]
- The fake agent + the matrix it pins
  [`hekma-conformance/src/bin/fake_acp_agent.rs`, `tests/acp_lifecycle.rs`]
- Docs + the deprecation announcement
  [`docs/*`]
