---
title: 'Epic 12: Durable Detach & the Production-Usable Observed Channel (12-1..12-4, one PR)'
type: 'feature'
created: '2026-09-15'
status: 'done'
review_loop_iteration: 1
baseline_commit: 9ee4df73aec2cf5fbe7125576656bb9a75a671f7
context: ['_bmad-output/implementation-artifacts/epic-12-context.md']
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** An agent started with `kt agent start` dies when the CLI exits, and the engine-observed metering channel is not production-usable: streamed completions go unmetered, `https://` upstreams are refused, and a store failure silently loses observed usage.

**Approach:** Four ratified stories land together in ONE PR, in order: (12-1) `kt agent start --detach` — detached spawn whose handle is disarmed so the child survives CLI exit and the next command re-adopts it via the existing fingerprint path; engine-observed instances are refused with an explanatory error; enforcement windows documented honestly in `--help` + stderr notice. (12-2) the forward listener injects `stream_options.include_usage` on forwarded streaming requests and parses the terminal SSE usage frame into the same `ParsedUsage` choke point via an O(1)-memory line scanner; the injection is documented as upstream-visible. (12-3) `metering.upstream_base_url` accepts `https://` via vendored rustls+ring (no system TLS); the refusal is replaced by a working forward; a new `cargo audit` CI job supplies the supply-chain teeth. (12-4) the observed drain gets the self-reported channel's AI-41 treatment: pending-buffer park, bounded retry, loud SKIPPED — never silent loss.

## Boundaries & Constraints

**Always:**
- Execution order 12-1 → 12-2 → 12-3 → 12-4; each story ships with its own tests and stays independently verifiable in the PR.
- Gates in the same change: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, `python3 scripts/check_docs.py`; coverage ≥ 95% stays green.
- Docs updated in the same change as behavior: `docs/commands.md`, `docs/get-started.md`, `docs/troubleshooting.md` (single-lifetime wording), `docs/architecture.md` (AD-7), `docs/design/metering-agents-you-dont-control.md`, root `Cargo.toml` NFR-8 comment ("NO TLS stack" claim must be rewritten).
- No-leak discipline: only integer token counts leave the proxy; error messages stay traffic-free (never echo URLs, auth, bodies).
- Surfaced-not-silent: every skip/loss/degradation emits a visible diagnostic with reason + remediation.
- Keep Epic-11 adoption machinery intact: detach must NOT clear the spawn record, NOT weaken fingerprints, NOT re-litigate AD-5 ordering.
- 12-2 must not touch the `https://` refusal (12-3's surface); 12-4 must not change the non-observed drain.
- Feature hygiene: rustls/tokio-rustls with `default-features = false` + `ring` provider so ONE rustls/ring unifies with kt→ureq; verify via `cargo tree`.

**Ask First:**
- Any cargo-audit finding requiring an `audit.toml` waiver, or a dependency whose MSRV exceeds the 1.96.1 pin.
- A new non-dev dependency beyond the TLS set (tokio-rustls/hyper-rustls/rustls/webpki-roots) or a dev-dep beyond rcgen.
- If Windows detach cannot be done by skipping job assignment at spawn (i.e. it needs a Job-Object redesign or breakaway flags mid-flight).
- Any change to the frozen exit-code table beyond the new detach-refusal entry.

**Never:**
- No daemon mode, no persistent engine session (AI-20 chose story-shaped detach).
- No non-OpenAI provider usage schemas (deferred behind the parse seam — build the seam, not the schemas).
- No system TLS: no openssl/native-tls/security-framework; TLS code only in engine-core `src/metering/` (never `backends/`, no per-OS cfg there).
- No silent drops, no fabricated zeros, no clearing of spawn records on detach.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Detached start survives exit | `kt agent start --detach` (self-reported metering, unix) | stdout state line + detach stderr notice naming the enforcement windows; child alive after CLI exit; record left; next command adopts | spawn/record failure = today's start failure path |
| Detach refused (engine-observed) | `start --detach` on `.metering("engine-observed")` manifest | Refusal BEFORE any side effect (no transition, no listener, no spawn); error names why (listener dies with the command) + remediation | new engine error → mapped diagnostic, documented exit code |
| Detached start cannot send stdin | `--detach` with pipe_stdin configured | Spawned with stdin null (no EPIPE strand); `send` after re-adoption fails with today's adopted-instance error | unchanged |
| Streaming completion metered | agent POSTs `"stream": true` through observed listener | forwarded body carries `stream_options.include_usage`; SSE response's terminal usage frame lands in ledger; intermediate `"usage": null` frames ignored; non-stream requests NOT modified | malformed body/no usage frame → silent skip (counts stay lower-bound-honest) |
| HTTPS upstream | `metering.upstream_base_url = "https://…"` | listener dials TLS (rustls+ring, vendored roots); forward works end-to-end | cert/DNS failure → static 502, honest, traffic-free |
| Unsupported scheme | `ftp://…` | start-time refusal (BadUpstream), URL never echoed | EngineError::ObservedMetering → exit 1 |
| Observed drain parks | ledger insert fails MidRun (store outage) | minted events parked as-is (same sequence on retry); no leapfrog; retry commits once dedup-safe | bounded: after 3 same-front failures → loud SKIPPED diagnostic, cursor-equivalent advances past the poisoned event |
| Terminal observed drain fails | store dead at stop/crash-reap | loss notice to diagnostic sink ("lost, cannot be retried"), no retry claim | parked buffer dropped with instance |
| Self-reported drain | unchanged | today's AI-41 behavior + its park test keeps passing | unchanged |

</frozen-after-approval>

## Code Map

**12-1 detach**
- `crates/ktesio-engine/src/ports/process_backend.rs` — `SpawnSpec` (add detach flag), `ProcessBackend` trait (L1143-1240; no disown op today); `ProcessFingerprint::matches` L189-209.
- `crates/ktesio-engine/src/backends/unix/mod.rs` — spawn L144-365 (setsid L252-257, pgid==pid L265); `UnixProcess` L78-125; **Drop kill-on-drop L648-684** (must learn "detached" disarm); stop L367-429; adopt L472-521.
- `crates/ktesio-engine/src/backends/windows/mod.rs` — spawn L220-457 (job L223-242, assign L353); **detach = skip job creation/assignment at spawn** (no breakaway exists); `WindowsProcess` Drop L178-200; adopt L553-610.
- `crates/ktesio-engine/src/domain/supervisor.rs` — `start_inner` L908-1375 (metering_source read L948-950 = refusal point; listener L1071; record commit L1293-1311; `Supervised` insert L1351-1372; `adopted:false` L3000, cursor init L1358/2965); `adopt_orphans` L2907-3064 (works unchanged — record survives + process alive); stranded-listener diagnostic L3021-3035.
- `crates/kt/src/main.rs` L143-147/L347 — clap `AgentCommands::Start` (+ `--detach` flag); `crates/kt/src/cli/agent.rs` L1126-1148 start cmd; single-lifetime notice L1139-1143 (make conditional; test `start_prints_single_lifetime_notice_to_stderr_only` L611 flips); `map_engine_error` L2278+, exit codes `crates/kt/src/exit_code.rs` L25-33, diagnostics `crates/kt/src/error.rs`.
- Tests: `crates/ktesio-engine/tests/adoption.rs` (harness L227-574, helpers), `crates/kt/tests/agent_cli.rs` (`start_via_surviving_engine` L75, StopOrphanOnDrop L2280-2306, `_unix` + Windows-positive convention L44-74), `ManifestFixture` in `crates/ktesio-conformance/src/test_support.rs` L106-284.

**12-2 streaming parse**
- `crates/ktesio-engine/src/metering/listener.rs` — forward L357-440 (request buffered L374-380 → **inject here**; response buffered L412-418 → **SSE route by content-type in res_parts**; skim L424-428); client L308-309; `HOP_BY_HOP_HEADERS` incl. CONTENT_LENGTH L95-104 (length fixup unnecessary — keep strip); module doc L6-11/L49 must flip; validate_upstream L271-297 (leave for 12-3).
- `crates/ktesio-engine/src/metering/parse.rs` — `parse_openai_usage` L50-59, `usage_field` L66-72; **add `parse_openai_sse_usage(&[u8]) -> Option<(u64,u64)>` (terminal/last usage-bearing `data:` frame) = the named provider-parse seam**; flip module doc L26-34 + SSE-skip test L139-150; crate-private (`metering/mod.rs:32`).
- Choke point (unchanged): queue `listener.rs:116` → `drain_observed_for` supervisor.rs L3547 → `ObservedUsageSource::mint` `ports/usage_source.rs:222` → `ingest_usage` L3633.
- Tests: in-crate listener tests L689-1012 (pattern); `crates/ktesio-engine/tests/observed_metering.rs` — `UpstreamStub`/`serve_one` L54-157 (**extend to read request body per Content-Length; respond SSE when body has stream:true**); `fake_agent.rs` L506-533/L651-699 (**add `--observed-stream-calls`**); `docs/testing.md:84` flip.

**12-3 rustls**
- `Cargo.toml` (root) — hyper family L56-59, NFR-8 comment L44-59 (**rewrite**); add `hyper-rustls` (or tokio-rustls) `default-features=false, ring`, `webpki-roots`. rustls+ring already in lock via ureq (`Cargo.lock:1132-1145, 1074-1086`) — keep one copy.
- `crates/ktesio-engine/src/metering/listener.rs` — L271-297 validate_upstream (accept https; keep empty/unsupported-scheme arms, no-leak tests L557-635); L308-309 client construction → TLS-capable connector handling both schemes (test seam: build client from an injectable rustls `ClientConfig` defaulting to webpki-roots).
- Supply-chain teeth (greenfield): new `cargo audit` job in `.github/workflows/ci.yml` (SHA-pinned actions, `cargo install --locked` convention L125-134); findings → fix or documented waiver.
- Docs: `docs/architecture.md` AD-7, `docs/design/metering-agents-you-dont-control.md:84,88`, `docs/embedding.md:287-297` AI-48 re-audit note (new TLS crates keep tracing silent).

**12-4 drain durability**
- `crates/ktesio-engine/src/domain/supervisor.rs` — model: drain_usage_for L3385 (ingest-break loop L3439-3452; advance-only-on-durable L3456-3460; terminal loss L3461-3475; bounded park L3476-3506, `USAGE_PARK_MAX_ATTEMPTS=3` L129); target: drain_observed_for L3547-3593 (**lossy `let _ =` L3590-3592**); `Supervised` cursor fields L494/L501 → add observed pending-buffer + attempt counter (init L1358, reset L2965); `emit_diagnostic` L851; shared ingest-failure text L3689-3696 (make honest for a retrying caller).
- Store: `ports/state_store.rs:116-120` `record_usage_event` → `RecordOutcome`; `store/sqlite.rs:675-740` (UNIQUE dedup L124, classify L440-450); BUSY is plain `Backend(String)` — don't special-case.
- Tests: mirror `a_failed_ledger_insert_parks_the_cursor_and_retries_exactly_once` L7625-7770 (DROP TABLE + restore incl. UNIQUE index + RAISE(ABORT) trigger technique; helpers `append_usage_lines` L7598, `install_capture_sink` L7246); integration in `tests/observed_metering.rs`.
- `_bmad-output/implementation-artifacts/deferred-work.md` L123-126 — replace `routed:` with `resolved:` (style of L118/122).

## Tasks & Acceptance

**Execution:**
- [x] `crates/ktesio-engine/src/ports/process_backend.rs` + both backends + `domain/supervisor.rs` -- 12-1: `detach` on SpawnSpec; unix Drop disarm; windows skip-job spawn; stdin null; refusal in start_inner before side effects (new EngineError variant + map_engine_error arm + error.rs diagnostic + exit-code test) -- survival via existing adoption
- [x] `crates/ktesio-engine/src/ports/state_store.rs` + both backends' `adopt` + `domain/supervisor.rs` -- 12-1 AMENDMENT (review loop 1): detached-ness persists — the spawn record carries the detach flag, `adopt()` re-holds the handle DISARMED (unix Drop skips the group kill; Windows equivalent), so a benign intervening command (`kt agent list`/`show`/…) does NOT kill the detached agent on its engine exit; stop must still work on the adopted detached handle -- the epic's durable-detach promise across N commands
- [x] `crates/ktesio-engine/src/backends/windows/mod.rs` -- 12-1 AMENDMENT (review loop 1): a detached Windows spawn creates a Job Object WITHOUT kill-on-close (still no assignment kill at drop), so stop escalation's `TerminateJobObject` kills descendants exactly like unix `killpg` -- platform-asymmetric descendant leak closed
- [x] `crates/kt/src/main.rs` + `crates/kt/src/cli/agent.rs` -- 12-1: `--detach` flag; conditional single-lifetime/detach stderr notice (+ `--help` honesty); flip pinned notice test -- honest surface
- [x] `crates/ktesio-engine/tests/adoption.rs`, `crates/kt/tests/agent_cli.rs` -- 12-1: detach-survival e2e (`_unix` + Windows-positive counterpart), refusal test both layers, stop/pause works on detached-in-process, record-left-for-adoption pin -- per-story verifiability
- [x] `crates/ktesio-engine/src/metering/parse.rs` -- 12-2: `parse_openai_sse_usage` O(1) scanner + unit tests (terminal frame, null intermediates, multi-frame, no usage, malformed); flip deferral doc + old SSE-skip test -- the parse seam
- [x] `crates/ktesio-engine/src/metering/listener.rs` -- 12-2: request injection (POST+JSON+stream:true only, respect existing stream_options) + content-type SSE routing into same queue; doc flip (faithful-relay caveat); in-crate SSE relay tests -- metered streams
- [x] `crates/ktesio-engine/src/metering/listener.rs` -- 12-2 AMENDMENT (review loop 1): injection additionally gated to chat-completions paths (request path ends `/chat/completions` — the same surface `parse_openai_usage` reads responses from); any other streaming endpoint is forwarded UNMODIFIED (no `stream_options` → no provider 400); module docs + tests pin the path gate -- metering must never break the call it meters
- [x] `crates/ktesio-engine/tests/observed_metering.rs` + `fake_agent.rs` -- 12-2: stub reads request body (assert include_usage) + SSE responses; `--observed-stream-calls` e2e ledger case -- end-to-end proof
- [x] root + engine `Cargo.toml`, `Cargo.lock` -- 12-3: rustls deps (default-features=false, ring, webpki-roots); single rustls/ring in `cargo tree`; NFR-8 comment rewrite -- vendored-TLS philosophy
- [x] `crates/ktesio-engine/src/metering/listener.rs` -- 12-3: https accepted, connector per scheme via injectable ClientConfig (webpki-roots default); keep no-leak tests; unsupported-scheme refusal stays; in-crate TLS test with self-signed root (rcgen dev-dep) proving https+SSE compose -- production-usable channel
- [x] `.github/workflows/ci.yml` -- 12-3: `cargo audit` supply-chain job (resolve or waive findings explicitly) -- supply-chain teeth
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- 12-4: observed pending-buffer park + break-on-first-error + bounded-attempt SKIP + terminal loss arm; channel-honest diagnostic text; init/reset lifecycle -- durability
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- 12-4 AMENDMENTS (review loop 1): (a) the pending park buffer is CAPPED (named const) with a loud overflow diagnostic when the cap forces a drop — never unbounded memory during a store outage, never a silent loss; (b) the shared ingest-failure diagnostic is MODE-aware — a Terminal drain must never claim "parked for retry on the next drain" (contradicts the adjacent LOST notice); (c) the terminal test asserts the ABSENCE of the retry claim -- honest diagnostics on both modes
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` (tests) + `tests/observed_metering.rs` -- 12-4: park/retry-once (DROP+restore incl. index), partial-failure park (trigger), SKIP-bound, terminal loss; self-reported AI-41 tests keep passing -- pinned semantics
- [x] `docs/*` + `_bmad-output/implementation-artifacts/deferred-work.md` -- docs currency sweep (single-lifetime wording, AD-7, metering design doc, testing.md) + resolve the stranded entry -- documentation currency gate
- [x] PATCH BUNDLE (review loop 1, applied on the re-derived code): (a) `ci.yml` -- one-rustls/ring invariant gets CI teeth (a `cargo tree -p ktesio-engine` grep-assert step; fails on a second rustls/ring or aws-lc/openssl); audit job's cargo-audit cache key is version/date-stamped so the scanner binary can actually refresh; (b) `crates/kt/tests/agent_cli.rs` -- assert the `--help`/long-help text carries the enforcement-window statement; (c) docs -- architecture.md survival/adoption section mentions detached spawns + the enforcement window; embedding.md documents `start_detached`/`DetachRefused` + the host's enforcement-window duty; a sentence names the observed park bound (3 attempts ~250ms cadence) and the store-and-forward relay latency/64MiB cap; (d) merge checklist recorded in the tracker comment + PR body: post-merge baseline SHA move in BOTH pinned sites (ci.yml + scripts/test_automation.py), per the #181 precedent

**Acceptance Criteria:**
- Given a self-reported agent started with `--detach`, when the CLI exits, then the child is alive, the spawn record remains, and the next `kt` command adopts it with usage continuity (unix proven e2e; windows-positive counterpart per convention).
- Given an engine-observed manifest, when `start --detach`, then refusal fires before any state transition with reason + remediation, exit code documented.
- Given a streaming completion through the observed listener, when the upstream returns an SSE terminal usage frame, then exactly one ledger event lands with the streamed counts and budgets enforce on it.
- Given `https://` upstream, when an agent makes a streamed call, then the request is forwarded over TLS and usage lands (self-signed-root test proves the composition).
- Given a store failure MidRun, when observed usage drains, then events park and retry with identical dedup keys; after 3 same-front failures the poisoned event is SKIPPED loudly and the rest keep counting; terminal failure announces loss.
- Given all four stories merged, `cargo fmt/clippy/test`, `check_docs`, and the ≥95% coverage gate pass; `cargo tree` shows one rustls+ring.

## Spec Change Log

- **Loop 1 (2026-09-15, from the step-04 three-layer review).** Triggering findings (all proven in source): (1) post-adoption survival was never designed — `adopt()` hardcodes `detached: false`, so the first benign intervening command's engine exit kills the detached agent (contradicts the epic's durable-detach promise); (2) `inject_include_usage` is path-blind — non-chat-completions streaming endpoints reject `stream_options` with a 400, so metering breaks the call it meters; (3) Windows detached stop escalation terminates only the direct child — descendants survive, asymmetric with unix `killpg`. Patches folded into re-derivation: terminal-drain false "parked for retry" claim + absence assertion; capped park buffer with loud overflow; one-rustls CI teeth; audit cache-key staleness; `--help` honesty test; architecture/embedding/park-bound/relay-latency docs; baseline-bump merge checklist. Amended: the three AMENDMENT tasks above + the patch bundle task. **Known-bad state avoided:** a detached agent that dies on its first `kt agent list`; a metered agent whose provider 400s on injected `stream_options`; orphaned Windows descendants after a detached force-stop; a diagnostic that promises a retry a terminal drain will never do. **KEEP instructions (must survive re-derivation):** the spawn-time detach shape (SpawnSpec.detach, unix Drop disarm + try_wait reap, stdin null, refusal BEFORE any side effect, `EngineError::DetachRefused` → exit 5, both honesty notices); the parse seam + O(1) SSE scanner + its 8-test suite; injection mechanics (method/JSON/stream-bool/existing-stream_options gates) with only the path gate added; the rustls connector + injectable ClientConfig seam + webpki-roots default + the self-signed-root composition test; the `cargo audit` CI job + the RUSTSEC-2026-0285 rustls 0.23.45 bump; the drain park mechanics (same-`ParsedUsage` retry, 3-attempt bound, SKIP + terminal arms) and their tests; all doc sweeps already made; the record-left-for-adoption ordering. The e2e survival test must extend to: start --detach → benign command (list) → exit → agent STILL alive → stop works.

## Design Notes

- **Loop-1 design corrections.** Detached-ness is a CROSS-LIFETIME property: it rides the spawn record (`SpawnRecord.detach`) so every later `adopt()` re-holds the handle disarmed — detach is durable across N commands, not just the first. Windows detached spawns keep a Job Object but WITHOUT `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — the job exists purely so stop escalation kills descendants (`TerminateJobObject`); nothing kills on engine exit. Injection scope mirrors parse scope: only `/chat/completions` requests are modified (the only surface whose response shape `parse_openai_usage`/`parse_openai_sse_usage` understand). The observed park buffer is bounded by a named const; overflow drops oldest with a loud diagnostic.

- **Detach = spawn-time mode, not a post-hoc disown.** Unix: child already has its own session (setsid, pgid==pid) — a detached `UnixProcess` just skips the Drop killpg; stop/pause still work in-process via pgid. Windows: skip Job-Object creation/assignment when detach is requested (kill-on-close is the only thing that would kill the child); adoption's `OpenProcess`+`GetProcessTimes` fingerprint path works for non-job processes unchanged.
- **Enforcement-window honesty is a hard AC**, not a nicety: `--help` and the stderr notice must state that between commands there is no crash detection, no budget enforcement, no event delivery (AI-18 surfaced-not-silent).
- **SSE scanner contract:** iterate `data:` lines of the buffered body; parse each frame's JSON lazily; keep the LAST frame bearing a usable `usage` object (`"usage": null` intermediates are normal). O(1) auxiliary memory, no state machine.
- **Injection is best-effort and body-shape-gated:** only `POST` + JSON body already setting `"stream": true`; if `stream_options` exists, leave it; malformed body → forward unmodified. Value round-trip is acceptable (house lenient-parse style); Content-Length is reframed by hyper (hop-by-hop strip stays).
- **TLS test seam:** factor client construction so tests inject a rustls `ClientConfig` with a custom root; production default = webpki-roots (vendored, matches no-system-dependency philosophy and ureq's existing choice).
- **12-4 mirrors, not reuses, the self-reported park:** identity for attempt-counting is the front parked event's minted `sequence` (analog of same-offset cursor); retry the SAME `ParsedUsage` (never re-mint — dedup key stability).

## Verification

**Commands:**
- `cargo fmt --all --check` -- clean
- `cargo clippy --workspace --all-targets -- -D warnings` -- zero warnings
- `cargo test --workspace --all-targets` -- all pass, incl. new detach/SSE/TLS/drain suites
- `python3 scripts/check_docs.py` -- passes (docs swept in same change)
- `cargo tree -p ktesio-engine -e normal | grep -E "rustls|ring"` -- exactly one rustls, one ring; no aws-lc/openssl
- `cargo tarpaulin` (or CI coverage split) -- ≥ 95% per the 11-5 gate

## Suggested Review Order

**Durable detach — the cross-lifetime core (12-1)**

- Entry point: detached start wraps `start_inner` — refusal before side effects lives here
  [`supervisor.rs:955`](../../crates/ktesio-engine/src/domain/supervisor.rs#L955)

- Detach rides the spawn record — the durable-detach promise across N commands
  [`state_store.rs:58`](../../crates/ktesio-engine/src/ports/state_store.rs#L58)

- SpawnSpec gains `detach`; stdin force-clear documented at the port boundary
  [`process_backend.rs:159`](../../crates/ktesio-engine/src/ports/process_backend.rs#L159)

- Unix Drop disarm: no group kill, one-shot reap — survival is the point
  [`unix/mod.rs:713`](../../crates/ktesio-engine/src/backends/unix/mod.rs#L713)

- Adoption re-holds DISARMED via the record's flag (loop-1 fix: benign commands don't kill)
  [`supervisor.rs:3090`](../../crates/ktesio-engine/src/domain/supervisor.rs#L3090)

- Restart preserves detach (loop-2 fix: crash-restart no longer downgrades to attached)
  [`supervisor.rs:1556`](../../crates/ktesio-engine/src/domain/supervisor.rs#L1556)

- Windows: job WITHOUT kill-on-close — escalation reaches descendants, engine exit kills nothing
  [`windows/mod.rs:137`](../../crates/ktesio-engine/src/backends/windows/mod.rs#L137)

- Record-commit failure explicitly kills the detached child (loop-2 fix: no unrecorded orphan)
  [`supervisor.rs:1419`](../../crates/ktesio-engine/src/domain/supervisor.rs#L1419)

**CLI honesty surface (12-1)**

- `--detach` flag + enforcement-window stderr notice; refusal maps to exit 5
  [`agent.rs:1131`](../../crates/kt/src/cli/agent.rs#L1131)

- The mapped diagnostic + frozen exit-code table entry
  [`exit_code.rs:32`](../../crates/kt/src/exit_code.rs#L32)

**Streaming metering (12-2)**

- The O(1) SSE parse seam — last usage-bearing `data:` frame wins
  [`parse.rs:112`](../../crates/ktesio-engine/src/metering/parse.rs#L112)

- Injection: method/JSON/stream-bool gates + the `/chat/completions` path gate (loop-1 fix)
  [`listener.rs:610`](../../crates/ktesio-engine/src/metering/listener.rs#L610)

- Response routing by content-type into the same queue
  [`listener.rs:573`](../../crates/ktesio-engine/src/metering/listener.rs#L573)

**HTTPS upstream (12-3)**

- Vendored trust: `default_root_store` (webpki-roots) + non-empty pin (loop-2 fix)
  [`listener.rs:118`](../../crates/ktesio-engine/src/metering/listener.rs#L118)

- NFR-8 rewritten: one vendored rustls+ring, features pinned, tracing off
  [`Cargo.toml:65`](../../Cargo.toml#L65)

- CI teeth: TLS invariant over the whole workspace + forbidden system-TLS list
  [`ci.yml:428`](../../.github/workflows/ci.yml#L428)

- The new `audit` supply-chain job (date-stamped cache, self-healing install)
  [`ci.yml:712`](../../.github/workflows/ci.yml#L712)

**Drain durability (12-4)**

- Park + same-event retry + 3-attempt SKIP + terminal loss — the AI-41 mirror
  [`supervisor.rs:3732`](../../crates/ktesio-engine/src/domain/supervisor.rs#L3732)

- The 1024-event park cap with loud overflow (loop-1 fix: bounded memory, no silent loss)
  [`supervisor.rs:3878`](../../crates/ktesio-engine/src/domain/supervisor.rs#L3878)

**Peripherals — tests, schema, release notes**

- E2e: start --detach → benign `kt agent list` → STILL alive → stop (both layers, cross-OS)
  [`agent_cli.rs:732`](../../crates/kt/tests/agent_cli.rs#L732)

- Crash-restart keeps the flag + stdin force-clear pins (loop-2 verification gaps)
  [`supervisor.rs:7400`](../../crates/ktesio-engine/src/domain/supervisor.rs#L7400)

- Schema v6: additive `detached` column, crash-atomic step
  [`sqlite.rs:59`](../../crates/ktesio-engine/src/store/sqlite.rs#L59)

- Release-surface announcement (AI-55): --detach, exit 5, DetachRefused, schema v6, dep family
  [`RELEASE_NOTES.md:31`](../../docs/RELEASE_NOTES.md#L31)
