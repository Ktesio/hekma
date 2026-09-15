# Epic 12 Context: Durable Detach & the Production-Usable Observed Channel

<!-- Compiled from planning artifacts. Edit freely. Regenerate with compile-epic-context if planning docs change. -->

## Goal

An Operator can detach a start from the CLI's lifetime — the agent survives the CLI exit and the next command adopts it through the hardened adoption path — and the engine-observed metering channel works against real providers: streamed completions are metered, HTTPS upstreams dial directly with no local TLS hop, and a store outage degrades the observed drain loudly (bounded skip) instead of wedging or silently losing usage. This closes the widest gap between the product's "runs agents like services" claim and shipped behavior, and lands the ratified follow-ups that make the observed channel production-usable. The epic was opened 2026-09-15 by Islam's ratification ("recommended across the board") of `_bmad-output/planning-artifacts/ai-20-ai-47-product-calls-2026-09-15.md` — the epic's primary planning source. Stated intent: all four stories ship together in a single PR.

## Stories

- Story 12-1: Detached start survives the CLI exit (AI-20, ratified option b)
- Story 12-2: Streaming usage parse — the `include_usage` terminal SSE frame (AI-47b)
- Story 12-3: HTTPS upstream via vendored rustls+ring (AI-47a)
- Story 12-4: Observed-drain durability — park + bounded retry + loud skip (epic-11 retro F3 fold)

## Requirements & Constraints

**Detach (12-1).** `kt agent start --detach` persists `running` and drops the supervisor handle WITHOUT the process-group kill, so the agent survives the CLI exit; the next command adopts it via the existing spawn-fingerprint adoption path. Detach v1 REFUSES engine-observed instances with an explanatory error (their loopback listener dies with the command — the same loud strand as adopted observed instances). The command-scoped supervision cost is accepted and must be documented honestly in `--help` AND the stderr notice: between commands there is NO crash detection, NO budget enforcement, NO event delivery.

**Streaming usage parse (12-2).** The forward listener requests `stream_options.include_usage` on forwarded stream requests and parses the terminal SSE usage frame into the SAME `ParsedUsage` choke point the non-streaming path uses, via an O(1)-memory line-scanner. The injected `stream_options` is an upstream-visible modification of the agent's request — document it. The std-only upstream stub in the observed-metering integration tests gains SSE cases. Non-OpenAI provider usage schemas stay deferred behind a named parse seam (the OpenAI parse is the v1 shape; a provider-parse seam is the later extension point) — respect the seam, do not build other schemas.

**HTTPS upstream (12-3).** `metering.upstream_base_url` accepts `https://`, dialed with vendored rustls+ring — no system TLS library dependency. The start-time `https://` refusal error is replaced by a working forward. The new dependency tree is paid for with updated supply-chain teeth (cargo audit + the CI boundary review).

**Observed-drain durability (12-4).** The observed drain gets the same durability treatment the self-reported channel has: park on ledger-write failure, bounded retry, and a loud SKIPPED diagnostic when an event is abandoned — never silent best-effort loss under store failure. This closes the stranded deferred-work entry (observed channel lossy under store failure) that was routed to 11-6 but closed without it.

**Standing gates.** Test coverage stays ≥95% (CI-enforced); docs (`docs/architecture.md` AD-7 sections, the metering design doc, `--help`) update in the same change as the behavior they describe; cross-platform (Linux/macOS/Windows) with per-OS code confined to the process backends. The epic lands under FR-19's amendment (observed-channel production usability) — no new FR numbers.

## Technical Decisions

- **Detach over daemon (AI-20 option b).** Detach reuses the machinery epics 11 recently hardened (fail-closed spawn fingerprints, Windows-positive adoption tests) and closes the user-facing surprise; a daemon remains a deliberate future epic if embedder demand or enforcement-window pain materializes. Ratifying detach means accepting its documented enforcement windows.
- **Per-OS teardown inversion.** Detach inverts the single-lifetime net per backend: Unix skips the group SIGKILL on handle drop for a detached start; Windows must undo kill-on-close (breakaway from the Job Object + detached-process creation flags) — the fracture-prone path the detach research flagged; lean on the backend module's existing conventions. After detach, the no-orphan guarantee rests entirely on adoption + reconciliation covering the clean-exit path too (until now they covered only crashes) — the no-orphan gate must be re-argued for intentional detach.
- **TLS: rustls+ring, vendored (AI-47a option i).** The dependency philosophy is "no SYSTEM library dependencies; vendored builds OK" — the bundled-SQLite C build already established that precedent, so ring's C+asm adds degree, not kind. Vendored rustls gives identical behavior across the three OSes; native-tls was rejected (Linux OpenSSL variance violates the philosophy).
- **Streaming parse is bounded, not a state machine (AI-47b yes).** The OpenAI contract is ONE well-defined shape: a terminal SSE frame carrying `usage` when `include_usage` is requested. An SSE line-scanner is O(1) memory and exhaustively unit-testable — the shape of small machine this repo tests exhaustively. Parsed usage funnels into the same ingestion choke point (engine-minted per-Run ordinals, dedup invariant preserved), so budgets and caps enforce on streamed usage with zero re-plumbing.
- **Engine-core placement.** The listener/TLS/parse work lives in the engine core metering module (never `backends/`) — loopback TCP + HTTP is OS-uniform.
- **Drain durability mirrors the self-reported design.** Cursor discipline: advance only past bytes/events whose ledger writes are DURABLE; park on failure and retry next pass; already-committed neighbors re-drift safely into the dedup key; an unfixable event is a loud diagnostic skip (surfaced-not-silent), never a silent drop and never a wedged drain.

## UX & Interaction Patterns

CLI product; no UX design contract — terminal conventions apply (stdout = output, stderr = diagnostics, miette-style errors). Relevant patterns:

- **Honest `--detach` surface:** the stderr notice on a detached start states what is NOT supervised between commands (crash detection, budget enforcement, event delivery) and how the next command reattaches; `--help` carries the same honesty. The refusal of engine-observed instances names why (the listener dies with the command) and the remediation.
- **Loud degradation:** any dropped/skipped observed-drain event emits a visible diagnostic with reason + remediation — an honest lower-bound stance for observed totals, never fabricated zeros.

## Cross-Story Dependencies

- **Execution order: 12-1 → 12-2 → 12-3 → 12-4.** 12-1 is independent and carries the highest user-facing value. Among the observed-channel stories, the streaming parse (12-2) lands FIRST because it delivers working metering to streaming agents on today's HTTP path (including through a local TLS hop); rustls (12-3) then removes the local-hop requirement; drain durability (12-4) closes the epic.
- **12-1 builds on Epic 11's adoption machinery** (spawn fingerprints, orphan adoption, PID-reuse guard) — it must not re-litigate or weaken that work.
- **12-4 closes the stranded deferred-work entry** routed "→ 11-6" (observed drain lossiness under store failure) and applies the self-reported channel's established durability pattern.
- **12-2 depends on the existing observed-listener pipeline** (`ParsedUsage` choke point, engine-minted sequences, the upstream stub in the observed-metering tests); **12-3 swaps the upstream dial behind the same listener** — the two must compose (HTTPS upstream + streamed completion metered end-to-end).
- All four stories land in one PR: keep each story's changes reviewable and independently verifiable (tests per story) even though they merge together.
