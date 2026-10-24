# Epic 20 Context: Adoptability

<!-- Compiled from planning artifacts. Edit freely. Regenerate with compile-epic-context if planning docs change. -->

## Goal

Hekma runs agents well, but a newcomer's first hour lacks the artifacts adoption runs on: governance metrics do not feed the Grafana/Datadog stacks enterprises already live inside, there is no recipe for running a governed bot in CI, and there is no validated index of which agents run under Hekma today. This epic delivers those three adoption artifacts — opt-in OTLP metrics export into existing observability stacks, a budget-capped agents-in-CI recipe dogfooded in this repository's own CI, and a conformance-validated adapter gallery with per-capability honesty tables (FR-46..FR-48; approved 2026-10-23). It is purely additive and must not disturb Epics 18/19 (in flight elsewhere): no story touches the lifecycle declarations, the wire fence, Task scope, the governance vectors, or reconciliation beyond reading their outputs. Expectation honesty bounds the epic: it promises metering honesty, not metering coverage — billing-grade token coverage across the ten seed agents is currently 1 of 10, so the gallery carries upstream stability status and re-validation duties, and the exporter faithfully exports honest dashes where no tier yields usage.

## Stories

- Story 20.1: OTLP telemetry export
- Story 20.2: Agents in CI — the GitHub Actions recipe
- Story 20.3: The adapter gallery

## Requirements & Constraints

**OTLP export (FR-46):**

- Opt-in, off by default: with no collector endpoint configured there is no export path — no dial, no payload, nothing leaves the machine — asserted by test. This is the privacy default's one sanctioned, metrics-only, operator-endpointed exception.
- Payloads carry counters and gauges derived from the Usage Ledger and supervision state; never request/response/agent content, never secrets — the secrets no-leak matrix extends to the export path.
- Every exported dollar figure carries its `estimated|reconciled` label as an attribute; no code path exports an unlabeled dollar.
- The metric exposition (names, types, attributes, units) is a versioned, documented, semver'd surface; renames/drops are announced release-surface changes.
- A failed or undeliverable export degrades visibly (diagnostic and counter) and never delays supervision or enforcement.
- Grafana/Datadog ingest is proven in CI against a collector fixture, not asserted from spec compliance.

**Agents-in-CI recipe (FR-47):**

- Spend is capped per run: a breach executes the configured Breach Action within the enforcement latency bound, and no path masks a breach as success.
- Every lane is wall-clock bounded; a lane that ends on a bound reports which one ended it — budget or clock — not a generic failure.
- Usage lands in `$GITHUB_STEP_SUMMARY` rendered from `--json` output: tokens and derived dollars, labeled estimated.
- Exit codes keep success, breach, and agent failure distinguishable by a workflow consumer.
- Secret plumbing is first-class: the recipe shows `secret:` wiring for auth-gated agents, not a footnote.
- Dogfood proof: a bot lane runs under the recipe in this repository's own CI — the lane is the recipe's test.

**Adapter gallery (FR-48):**

- Ten seed entries — Copilot CLI, Hermes, OpenCode, Claude Code, Codex, Gemini CLI, goose, Crush, Aider, Cursor CLI — each a validated manifest or an honestly named gap.
- Per-entry honesty tables in the Capability Declaration vocabulary (guaranteed / best-effort / unsupported) with explicit absence tokens — never a fabricated pass or implied zero; tables carry upstream stability status and the re-validation duty.
- Every manifest is validated in CI by the conformance kit against bundled fake/shim execs — CI never launches real agents; a failing manifest or an entry claiming more than its validation proves fails the build.
- Where no metering tier yields usage, the entry shows the honest not-available marker naming the tier — never a blank.
- Public pages carry no planning-provenance wording (docs gate enforced); an entry ships only when its controls behave uniformly across the fleet.

Cross-cutting: all house gates per story (≥95% coverage, docs updated in the same change); no new exit codes — new diagnostics join existing classes; AI-55 two-pass review applies to 20.1 (public exposition + privacy surface) and to the gallery's public claims.

## Technical Decisions

- **The host owns the exporter; the engine stays telemetry-free (AD-21).** A module inside the host crate (`src/otlp/`) — not a new crate, not in the engine — subscribes read-only to the public event bus and derives every metric from the existing versioned event payloads. No new event kinds, no engine API change, no engine instrumentation; the exporter is a third consumer of the one event schema, and the outbox remains the only durable/acknowledged export surface.
- **Hand-rolled OTLP/HTTP+JSON, zero new runtime deps.** A POST of a JSON metrics batch to `<endpoint>/v1/metrics`, serialized with in-tree serde_json over the hyper+rustls client the metering channel already proved. Never gRPC (tonic/prost is the largest dependency delta available); binary protobuf encoding deferred (volume-triggered). The otel-SDK path is the recorded standing alternative with named reopen triggers (a real traces consumer, or a vendor must-have); a Prometheus scrape endpoint was rejected — an OTel Collector in front of Hekma can bridge.
- **Never blocks supervision.** Publish-after-commit comes free from the bus contract; lag becomes a surfaced drop count (a `hekma.otlp.dropped` self-metric plus a restart note), never an error into supervision; each POST carries an explicit timeout; queue overflow drops oldest with a surfaced note; in-flight points lost on crash are surfaced once at restart. Named trade-off: the live bus is at-most-once in the crash window, so v1 counters can undercount across a lag/crash — surfaced-drop honesty, with ledger-sourced lossless re-derivation deferred behind a trigger.
- **Naming: semconv first.** OTel GenAI semantic conventions first (`gen_ai.client.token_usage` with token type input/output/cached; `gen_ai.client.operation_duration` only once per-call latency exists — absent, never fabricated); `hekma.*` only where no semconv exists (`hekma.budget.breaches`, `hekma.agent.state`, `hekma.usage.cost`, `hekma.otlp.dropped`). A reconciled figure is a new series, never a replacement. Resource attributes stay minimal (`service.name`, `service.version`, `service.instance.id`).
- **Config on one surface** (both host entry points): `--otlp-endpoint` (default: off — the exporter exists only when configured), `--otlp-headers` with `secret:NAME` values resolved at startup through the secret resolver (unresolvable = typed startup refusal), and a bounded flush interval (default 10s). Cumulative temporality with an honest restart-reset, stated in docs.
- **Versioned exposition** pinned in the telemetry docs page under an `otlp-metrics v1` marker, additive-only-until-frozen.
- **Gallery/CI validation model:** CI validates manifests against bundled fakes/shims (the existing shim precedent); real-agent verification rides the existing run-verified-pins docs discipline. The CI recipe ships in-repo (docs page + dogfood lane); the composite marketplace Action is deferred (trigger: a third-party user asks).

## UX & Interaction Patterns

Terminal product, no GUI — the new operator-facing surfaces are docs pages (telemetry, the CI recipe, the gallery under `docs/adapters/`) plus the OTLP flags on the host. Public-content honesty is the interaction constraint: capability vocabulary with absence tokens, honest dashes where no tier yields usage, no planning-provenance wording on public pages.

## Cross-Story Dependencies

- **Execution order:** 20.3 (gallery) and 20.2 (CI recipe) start immediately and in parallel with Epics 18/19 — docs + CI-lane work on shipped surfaces, depending on nothing in flight. 20.1 (OTLP export) sequences after the governance-vectors story (19.1) merges, branching from main — never stacked on the epic-19 branch.
- **Graceful degradation on 18/19 outputs:** 20.1's operation-duration metric has no producer until per-call records (19.4) land, and no `reconciled` series until reconciliation ingestion (19.5) — both stay absent, never fabricated, with small follow-up derivations inside `otlp/` when those stories merge. The spec stage picks whether 20.1 v0 waits for Task-scoped attribution or ships per-Run/cumulative first; either order satisfies FR-46.
- **Parallel-safety with 18/19 is by construction:** the only shared files are the host's flag plumbing, landed in one commit before parallel work resumes; zero merge conflicts from Epic 20 work is an epic success criterion.
- Batch-epic shape: full acceptance criteria are derived at each story's spec stage.
