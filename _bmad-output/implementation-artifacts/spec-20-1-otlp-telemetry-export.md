---
title: 'OTLP telemetry export (AD-21) + restore the 95% coverage gate'
type: 'feature'
created: '2026-10-25'
status: 'in-progress'
review_loop_iteration: 0
baseline_commit: 'f7ba049a08f2d960c3a390da1224ec7defab8bf8'
context:
  - '_bmad-output/implementation-artifacts/epic-20-context.md'
  - 'docs/commands.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** FR-46/AD-21: operators cannot feed Hekma's governance metrics into the Grafana/Datadog stacks they already run — the host has no telemetry export. Separately binding: main's workspace coverage sits at 94.0183% (12810/13625) below the enforced 95% gate (Islam's 2026-10-25 directive: coming PRs take care of coverage), and the gate is workspace-wide, so this PR must restore it or ship red.

**Approach:** A hand-rolled OTLP/HTTP+JSON metrics exporter module in `hekma-host` (`src/otlp/`) — opt-in via `--otlp-endpoint`, off by default, zero new runtime deps (hyper/hyper-util already in the host's normal closure via axum+engine) — subscribing read-only to the engine event bus, deriving semconv-first metrics, flushing on the host's runtime with bounded POSTs; plus a coverage-repair task targeting ~180 newly-covered lines from the merged-tracefile's cheap pool so the PR tree clears 95% with headroom for the new module's denominator.

## Boundaries & Constraints

**Always:**
- AD-21 verbatim: host-owned module (never the engine, never a new crate); read-only bus subscription; publish-after-commit honored; `Lagged(n)` → surfaced drop count + `hekma.otlp.dropped` self-metric + restart note (AI-18), never an error into supervision; each POST bounded (timeout); queue overflow drops oldest with a surfaced note; NO durable telemetry persistence (the outbox stays the only durable surface); metric-only v1.
- Opt-in by configuration only: no `--otlp-endpoint` → the exporter does not exist — no task, no dial (test-proven, the §6-amendment default).
- Naming per AD-21: `gen_ai.client.token_usage` (histogram, attribute `gen_ai.token.type` ∈ input/output/cached, plus instance/run attributes), `gen_ai.client.operation_duration` NOT emitted (no producer until per-call records), `hekma.budget.breaches` (sum, dimension/scope/action attributes), `hekma.agent.state` (gauge by state), `hekma.usage.cost` (cumulative sum, unit USD, asDouble = micros/1e6, MANDATORY `estimate_state` attribute — estimated|reconciled; a reconciled figure is a new data point, never a replacement), `hekma.otlp.dropped`. Resource attributes: `service.name=hekma-host`, `service.version`, `service.instance.id` (state-dir lock identity). Exposition versioned `otlp-metrics v1` in `docs/telemetry.md`, additive-only-until-frozen (AI-55 applies — public exposition).
- OTLP/HTTP+JSON wire rules: lowerCamelCase keys; 64-bit ints as decimal strings (`timeUnixNano`, `asInt`); `aggregationTemporality` numeric (1=CUMULATIVE for sums; histograms default); POST `<endpoint>/v1/metrics`, `Content-Type: application/json`; 200 = success, 429/503/504 = retryable (bounded retries), others = terminal for the batch with a surfaced diagnostic.
- Config on ONE surface, parsed BOTH places (house pattern): `--otlp-endpoint <url>` (http:// only in v1 — https:// is a deferred entry with a named trigger), `--otlp-headers <k>=<v>,...` where any value may be `secret:NAME` resolved at startup via `CompositeSecretResolver` (unresolvable = typed startup refusal; values live in a redacting newtype, never in Debug/logs), `--otlp-flush-interval <secs>` (bounded, default 10). Add to `crates/hekma-host/src/main.rs`'s hand-rolled loop AND the clap `Serve` variant in `crates/hekma/src/lib.rs` + `cli/serve.rs`.
- Shutdown handshake: the exporter task stops before/with the Control API in `serve_with_signal`'s shutdown sequence; in-flight flush bounded; a final flush is attempted but never blocks exit past the bound.
- Coverage repair is a TASK, not a hope: the PR tree's merged coverage must be ≥95% — target ~180 newly-covered lines from the cheap pool (below), each via real unit tests in the owning crates, no fault-injection restructuring.

**Ask First:** (standing autonomous instruction: collect to end — pre-decided in the Spec Change Log) http-only v1 with https deferred; dollars as asDouble USD from integer micros; token_usage as histogram with documented fixed bounds; engine-crate test additions ARE in scope (the gate is workspace-wide; the repair is explicitly this PR's job per Islam's directive); the new test file is OS-neutral (data-driven, no cfg(unix/windows) outside allowlists).

**Never:** No engine API changes, no new event kinds, no otel SDK crates (AD-21's standing alternative), no gRPC/protobuf, no content or secrets in payloads (the no-leak matrix extends to the export path), no durable telemetry persistence, no new exit codes, no `gen_ai.client.operation_duration` fabrication.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| No endpoint configured | default flags | exporter absent: no task spawned, no dial — asserted by test | N/A |
| Happy flush | events committed; interval elapses | OTLP/HTTP+JSON POST to `<endpoint>/v1/metrics`; in-test axum sink asserts method/path/content-type + payload shape vs the versioned exposition | malformed server response → surfaced diagnostic, batch dropped |
| Unreachable collector | sink refused/timing out | bounded retries on 429/503/504/timeouts; terminal errors surface a diagnostic + `hekma.otlp.dropped` grows; supervision unaffected | never blocks, never panics |
| Slow subscriber | bus Lagged(n) | drop count surfaced (self-metric + note); exporter resyncs at tail | N/A |
| Header secret unresolvable | `--otlp-headers auth=secret:NOPE` | typed startup refusal naming the secret (the --bearer-secret rule) | exit 2 class |
| Restart mid-stream | host killed with in-flight points | restart note names lost in-flight count; cumulative sums reset (documented restart-reset) | N/A |
| Dollar metric | breach/usage carrying Micros + label | `hekma.usage.cost` data point with `estimate_state` attribute; reconciled never overwrites estimated | unlabeled dollar = test failure |

</frozen-after-approval>

## Code Map

- `crates/hekma-host/src/lib.rs:36-39,57-96,304-683` — module decls, ServeOptions (+manual Debug :108-125 — new fields redact header values), serve_with_signal; exporter attaches AFTER engine open (:537-563) BEFORE signal wait, exactly like `control/mod.rs:168-224` (ControlApi::bind); shutdown slot beside api.shutdown() (:658-668)
- `crates/hekma-host/src/control/auth.rs:30-40` + `lib.rs:435-476` — the BearerToken/secret-NAME resolution pattern to copy for `--otlp-headers` (CompositeSecretResolver::env_then_file, `ports/secret_resolver.rs:204-233`)
- `crates/hekma-engine/src/domain/bus.rs:84-157` + `engine.rs:652-675` — `Engine::subscribe()` (lock-free), EngineEvent kinds, Lagged semantics; consumption precedent `control/sse.rs:32-47` (cancel-safe unfold + shutdown watch), test precedent `crates/hekma-engine/tests/resync_events.rs`
- `crates/hekma-engine/src/metering/listener.rs:117-175` — the ForwardClient hyper_util/rustls construction (engine-private; the host rebuilds the ~20-line plain-HTTP variant: `hyper_util::client::legacy::Client` + `HttpConnector`, all types already in the host's normal closure — `cargo tree -p hekma-host -e normal` shows hyper 1.12/hyper-util 0.1.21/http 1.4.2/http-body-util; workspace pins root Cargo.toml:67-70)
- Payload field sources: `TransitionEvent` (domain/event.rs:703-715), `BudgetBreachEvent` (:767-805 — dimension/scope/limit/observed/Micros/EstimateLabel::as_str 'estimated'/'reconciled' cost.rs:372-384, action), `UsageUpdateEvent` (domain/usage.rs:239-248 wrapping :117-143 — tokens, cached Option, run_id, metering_source)
- OTLP JSON mapping (verified against opentelemetry-proto spec/OTEP-0122): ExportMetricsServiceRequest {resourceMetrics[{resource{attributes[{key,value:{stringValue}}]}, scopeMetrics[{scope{name,version}, metrics[{name,description,unit, sum|gauge|histogram {dataPoints[{attributes, startTimeUnixNano:"…", timeUnixNano:"…", asInt:"N"|asDouble}]}}]}]}]}; camelCase; int64/fixed64 as strings; aggregationTemporality numeric
- Host tests today: `crates/hekma-host/tests/{serve,control_api,drain_subprocess,binary_flags}.rs` (spawn_serve + signal factory; ureq over real 127.0.0.1 HTTP; the in-test axum sink pattern is `control/mod.rs:168-224` + dev-dep axum/tokio already present)
- Coverage pool (from `~/.hermes/cache/scratch/merged.info`, MERGED view — the per-crate CI log lists are slice artifacts, do not re-trust them): cheap targets — engine `ports/probe_kinds.rs` 13 (malformed-url early returns :70-131, pure fns), `ports/state_store.rs` 13 (:176,182-187,346,352-356 error mapping), `ports/process_backend.rs` ~8, `backends/unix/mod.rs` ~15 (exec-a-nonexistent-path + spawn error arms), `supervisor/drain.rs` KillOnDrop::drop (:111-114) + bound arm (~10), `shutdown.rs` 33 (Engine::shutdown facade arms — unit-testable), `hekma/src/cli/agent.rs` ~25 (unknown memory-backing kind :2312-2340 via existing CLI harness; memory_status-None :3087-3092), host `lib.rs` :394 (state_base under a file path) + :515-517 (wait-task JoinError via a panicking signal factory); structural/do-not-touch: reaper/spawn/lifecycle fault-injection (~200 lines — NOT this PR)
- Docs registration: `docs/meta.json` pages list + `docs/source.config.ts` files list (both hand-maintained; add `telemetry`); check_docs auto-globs docs/*.md; test_automation pins: new host test file must be OS-NEUTRAL (ci.yml:800 per-file cfg allowlist — data-driven OsId pattern) or join the allowlist with justification + the exact-string pin update in scripts/test_automation.py
- `docs/RELEASE_NOTES.md` — Unreleased bullet (additive; hekma-host is unpublished so no version-slot move)

## Tasks & Acceptance

**Execution:**
- [ ] `crates/hekma-host/src/otlp/` — NEW module: `mod.rs` (OtlpOptions + redacting Debug + wiring into serve_with_signal + shutdown handshake), `payload.rs` (versioned metric derivation: event → metric families, serde payload types, camelCase/string-int rules), `client.rs` (plain-HTTP POST, bounded timeout, retry classes), `task.rs` (bus-drain loop: try_recv into bounded queue, Lagged accounting, flush interval, drop-oldest)
- [ ] `crates/hekma-host/src/main.rs` + `crates/hekma/src/lib.rs` (clap Serve variant) + `crates/hekma/src/cli/serve.rs` — the three `--otlp-*` flags on both surfaces, parsed once into ServeOptions
- [ ] `crates/hekma-host/tests/otlp_export.rs` — NEW, OS-neutral: (1) no-endpoint → no dial/task (assert via diagnostics + a sink that would fail the test if contacted); (2) end-to-end: in-test axum sink on 127.0.0.1:0, serve_with_signal with a fake_agent instance driving usage + a breach → assert POST path/content-type + payload decoded against the versioned exposition (names, attributes incl. estimate_state, camelCase, string ints, cumulative temporality, histogram bounds); (3) collector-refused → surfaced diagnostic + dropped counter; (4) unresolvable secret:NAME header → typed startup refusal; (5) restart note honesty (kill path or simulated queue loss)
- [ ] `crates/hekma-host/src/otlp/payload.rs` unit tests — every metric family + edge attributes (the I/O matrix rows) pinned without I/O
- [ ] Coverage repair (target ≥180 newly-covered lines, each a real test in the owning crate): engine `ports/probe_kinds.rs`, `ports/state_store.rs`, `ports/process_backend.rs` error arms; `backends/unix` spawn/exec-failure arms (OS-neutral guards); `supervisor/drain.rs` KillOnDrop; `shutdown.rs` facade arms; `hekma/src/cli/agent.rs` unknown-kind + memory_status-None arms via the existing CLI test harness; host `lib.rs` :394 + :515-517 arms
- [ ] `docs/telemetry.md` — NEW: `otlp-metrics v1` exposition (names, types, units, attributes, bounds, temporality, restart-reset semantics, the estimated|reconciled rule, the drop/diagnostic behavior, config reference, http-only note + https deferral); registered in meta.json + source.config.ts; cross-linked from commands.md serve section + hosting.md
- [ ] `_bmad-output/implementation-artifacts/deferred-work.md` — https:// endpoints entry (trigger: a user names an https collector); ledger-sourced lossless re-derivation already deferred (2026-10-23 entry)
- [ ] `docs/RELEASE_NOTES.md` — Unreleased bullet
- [ ] `_bmad-output/implementation-artifacts/sprint-status.yaml` — tick 20-1 per house convention on PR open

**Acceptance Criteria:**
- Given no `--otlp-endpoint`, when the host serves normally, then nothing dials out and no exporter task exists (test-proven).
- Given events and a configured sink, the POSTed payload matches `otlp-metrics v1` verbatim (in-test sink decodes and asserts); dollars always carry `estimate_state`; no content/secrets anywhere (sink asserts absence of known secret material).
- Given a refused/flaky collector, supervision and the Control API continue unaffected; drops are visible (diagnostic + counter); bounded retries only for the retryable classes.
- Given the PR tree, the merged workspace coverage is ≥95% (local proof: the added tests + a documented count of the targeted uncovered lines; CI's tarpaulin lane is authoritative) — record the before/after numbers in the PR body.
- Given `python3 scripts/check_docs.py`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets --no-fail-fast` — all green.

## Spec Change Log

- 2026-10-25 (pre-approval, standing autonomous instruction): gates pre-decided: (1) http-only v1, https:// deferred (the tested surface must equal the shipped surface; TLS handshakes aren't provable network-free); (2) dollars export as asDouble USD derived from integer micros (display-truthful; precision beyond 1e-6 USD is not a display concern); (3) token_usage ships as a histogram with documented fixed bounds (per-AD-21; each UsageUpdateEvent is one observation); (4) engine-crate coverage tests are IN scope — the gate is workspace-wide and Islam's directive assigns the repair to this PR; (5) the investigation's two corrections adopted: the host-lib 180-line "gap" was a per-crate-slice artifact (merged view: 6 lines) — the repair pool is the cheap fault arms enumerated in the Code Map, structural fault-injection arms (~200 lines) stay untouched; (6) hekma-host is unpublished → ServeOptions additions take no version-slot move (the semver job gates published crates).
- 2026-10-25 (step-04 review, loop 0 — patches only, no loopback): 3 reviewers at max effort; triage: ~20 findings PATCHED (cost-series honesty: Cumulative-scope-only + skip dollar_observed-None points + the v1 limitation documented + deferred per-usage-event cost; run-series bound 64/instance with surfaced eviction; final-flush loss-count corrected; body-drain made real; partialSuccess surfaced + counted; endpoint/header validation hardened at parse time (?/#, port, CRLF, dups); FNV-1a replaces DefaultHasher for service.instance.id; retry leg test (503→200); flush-interval requires endpoint on both surfaces; rate-limited drop notes; stop-ladder resume seam-fronting + failure-arm pin; README/telemetry doc-honesty fixes incl. the observation-window statement; 2 deferred entries; release-notes residue cleaned); REJECTED: the "leftover `<<<<<<<` opener" claim (verify on inspection — pass 2 verified zero markers); the cost-limitation finding did NOT loopback (the spec's engine-change Never boundary held — the honest fix is docs + deferred entry, recorded here). Also adopted from the build reports: coverage second pass +106 verified lines (~166 total vs ~134 needed) and the otlp_export flake fix (process-global bearer env race; 6/6 proof runs). *(Provenance note: this entry briefly replaced the pre-approval entry — an append-only violation, second occurrence of the same class — restored before commit; the workflow's change-log discipline demands APPEND, always.)*

## Design Notes

- The exporter's queue is bounded (e.g. 1024, the bus's own capacity); drop-oldest keeps memory flat under collector outage; the drop count is itself exported (`hekma.otlp.dropped`) — self-observability, AI-18.
- Flush task shape: `tokio::spawn` on the host's existing rt; `futures_util` not required — a plain `rx.recv()` loop with `interval()` tick + shutdown `watch` (the sse.rs cancel-safe race pattern) is sufficient; NEVER hold the engine lock (subscribe() is lock-free by construction).
- Payload types: hand-written `#[derive(Serialize)]` structs in `payload.rs` mirroring the OTLP JSON mapping (attribute as enum with stringValue/intValue variants; one sum/gauge/histogram family struct each) — kept versioned behind `otlp_metrics_v1` so a future v2 is additive.
- Histogram bounds (documented in telemetry.md, fixed): [1k, 10k, 100k, 1M, 10M] tokens per observation — coarse, stable, and enough to see runaway batches.

## Verification

**Commands:**
- `cargo test -p hekma-host --test otlp_export -- --nocapture` — expected: all legs green against the in-test sink
- `cargo test --workspace --all-targets --no-fail-fast` — expected: green (incl. the new coverage-repair tests; pre-existing environmental reds documented if any)
- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings` — green
- `python3 scripts/check_docs.py && python3 scripts/check_docs.py --self-test` — green with telemetry.md
- Coverage estimate for the PR body: `grep -c` the targeted lines covered by the new tests vs the merged.info baseline (CI's tarpaulin run is the authoritative gate)
