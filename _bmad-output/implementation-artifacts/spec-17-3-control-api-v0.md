---
title: 'Control API v0 — the host''s local HTTP/JSON surface'
type: 'feature'
created: '2026-10-19'
status: 'done'
review_loop_iteration: 0
baseline_commit: 'f5118bcfa2e7aa289956898e3b852af879ca0280'
context:
  - '_bmad-output/implementation-artifacts/epic-17-context.md'
  - '_bmad-output/implementation-artifacts/spec-17-2-hekma-serve-the-long-running-host.md'
  - '_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md (§4.4 Story 15.3 → renumbered 17.3)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-20 + Amendments 2026-09-30/2026-10-19; AD-2; AD-13; AD-14)'
  - '_bmad-output/planning-artifacts/host-mode-decisions-2026-10-19.md (D2 + the §5 erratum)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The 17-2 host supervises continuously but answers only to OS signals — a Host that cannot link Rust (FR-41's premise) has no local surface to drive the engine it runs: lifecycle, config and budgets, fleet and status, usage, events (proposal :361-368).

**Approach:** Serve an axum 0.8 Control API (D2, ratified) from inside `hekma-host` — its own Control-API types per the AD-2 amendment (spine :232-234) — mounted on the host's existing runtime after the engine opens, projecting ONLY `hekma-engine`'s public async facade (AD-20). Explicit bind (the API is off unless configured); bearer auth as one tower-style layer over `SecretResolver`; no secret reveal (`reveal_secrets` is never projected) and no raw agent output; events live over SSE and by per-instance cursor pull; versioned `/v0`, marked unstable until a second Host validates it. The drain composite and the Task verbs are NOT v0 — they arrive with today's Epics 18 and 19.

## Tasks & Acceptance

**Execution:**
- [x] `crates/hekma-host/src/control/` — NEW module (router + the AD-2 amendment's own Control-API types): handlers projecting the facade; snake_case DTOs stamped `schema_version` on every payload (the spine's AD-14 wire rule); an error mapper `EngineError`/`RegistryError` → typed statuses (400 invalid name/body, 401 auth, 404 unknown instance, 409 invalid transition or cursor-past-family, 500 surfaced reason — never a panic). `GET /v0/version` self-describes `{api, stability: "unstable", engine versions}`. Unit tests: mapper pins + DTO round-trips.
- [x] Routes (read-mostly, v0-minimal): `GET /v0/fleet` (`Engine::fleet`); per-instance `GET` (`fleet_entry`), `/status` (`instance_status`), `/capabilities` (`effective_capabilities`), `GET|PUT /config` (`effective_config`/`set_config`), `/events?cursor=` (below); `POST /v0/instances` (register), `POST .../start|stop|pause|resume` — `stop`'s body carries optional `window_secs` (the bounded ladder) — and `DELETE` (remove). Usage and budgets ride the fleet/status projections (`FleetEntry.budget`/`.usage`) — no separate routes. Tests per route: happy path + error mapping against a fake agent.
- [x] Auth — one tower layer over `SecretResolver` (:54): the host resolves the operator-named secret at STARTUP through the composite (`env_then_file` over the engine's `EnginePaths::secrets_file()` seam — the host never joins layout paths itself); an unresolvable reference is a typed `ServeError::Startup` refusal — the API never serves unauthenticated. Each request checks `Authorization: Bearer` (constant-time compare) → 401 otherwise. No secret VALUE ever enters a response, log, or SSE frame. Tests: absent/wrong bearer 401 on every route; startup refusal on unresolvable reference; no-leak pins.
- [x] Events — `GET /v0/events` (SSE): one `Engine::subscribe()` per client; `RecvError::Lagged(n)` surfaced as an in-stream event naming the gap (AI-18), the stream continues, never hangs. Pull: `resync_events` verbatim — the serde `ResyncCursor`/`ResyncBatch` shapes; `torn_tail_skipped` surfaces in the payload. Tests: SSE delivery from a live fake agent; cursor roundtrip idempotence; lag + torn-tail pins.
- [x] Mount + lifecycle — `ServeOptions.control_listen: Option<SocketAddr>` + the bearer secret name: `None` = API off, serve byte-identical to 17-2. On Some: bind on the host runtime right after the started notice, before the signal wait; bind failure is a surfaced `Startup` error naming the address (AI-18). On shutdown the API stops accepting first, then the drain runs. CLI: `--listen`/`--bearer-secret` flags on `hekma serve` (CLI-first gate). Tests: off-by-default byte-identity; bind-conflict surface; signal → API down → drain order.
- [x] Conformance-style suite — `crates/hekma-host/tests/control_api.rs`: drives the WHOLE API over real HTTP against the conformance fake agents: every route's happy/error/auth matrix; an end-to-end lifecycle (register → start → fleet shows running → stop honoring `window_secs`); SSE + cursor continuity across the run; the no-secret/no-raw-output leak matrix.
- [x] Tree diff + the NFR-8 gate — record the ACTUAL `cargo tree` diff (before/after axum) in THIS story's PR (the 17-2 erratum's ruling). Pre-check 2026-10-19 (scratch manifest, latest-compatible axum 0.8.9, diffed against the engine's tree): the D2-estimated tower family is exact — `tower`, `tower-layer`, `sync_wrapper`, `matchit` new, `tower-service` ALREADY in-tree — but the closure adds ~8 leaf crates the estimate did not enumerate (`form_urlencoded`, `percent-encoding`, `mime`, `serde_urlencoded`, `serde_path_to_error`, `slab`, `log`, `ryu`); the PR diff must name them. GATE: if the real diff materially exceeds the D2 estimate (tower-family growth or a heavy new subtree), STOP and Ask First — hyper-direct (D2's stated fallback) is the honest escape. Lean posture: `default-features = false`, features `["http1","json","tokio","sse","query"]`.
- [x] CI + docs + coverage in the same change — the AD-2 boundary gate is unaffected (axum is not a workspace-crate edge; `hekma → hekma-host` already allowlisted, `ci.yml:658`) — verify green. Docs: NEW `docs/control-api.md` (routes, auth, the SSE/cursor contract, the unstable-0.x marking, the cooperative NFR-6 caveat) + a README hosting mention + a pointer from the `docs/commands.md` serve section. Coverage ≥ 95% (tarpaulin aggregate). — *Local evidence 2026-10-19: tarpaulin 0.37.5 `--engine llvm` (CI's own recipe, per-crate) over `hekma-host` — the NEW `control/` module at 275/285 lines = 96.5% (dto 5/5, error 60/61, auth 27/29, mod 46/49, routes 118/120, sse 19/21 — the 2 open lines are the documented-unreachable serialize-failure fallback); the workspace ≥95% figure is CI's five-run tracefile union, not a per-crate number. Also from this run: the OS-cfg CI gate needed `control_api.rs` allowlisted (the secrets-file 0600 write — AI-35 convention, done with justification) and the SSE lag-frame pin added a stream-arm unit test.*

**Acceptance Criteria:**
- Given serve with `--listen` + `--bearer-secret`, every route answers a valid bearer and returns 401 without one; no route ever returns secret material or raw agent output.
- Given an unresolvable bearer reference, the host refuses to start (typed, surfaced) rather than serving unauthenticated.
- Given a running fake agent, `/v0/fleet` shows it and the lifecycle verbs drive it through the AD-15 states; stop honors `window_secs`.
- Given a live run, SSE delivers events and a cursor pull resumes losslessly and idempotently; lag and torn tails are surfaced, never silent.
- Given no `--listen` (the default), serve is byte-identical to 17-2; a bind conflict surfaces the address and exits nonzero.
- Given handoff: the tree diff is recorded in the PR, the four house gates pass, coverage ≥ 95%.

</frozen-after-approval>

## Design Notes

- **Route table (v0):** as frozen above. Excluded, and why: `send_input`, restart-policy, and the memory verbs (interaction/memory are not among the story's named groups — additive later under 0.x); `read_agent_log`/raw-output reads (FR-41's "no raw agent output unless explicitly enabled" — v0 defines no enable flag, so excluded outright; a future explicit flag is the 0.x additive path); `reveal_secrets` (the one facade read FR-41 forbids — never projected). Forward-compatibility under 0.x instability: additive routes/fields only; `/v0` + `GET /v0/version` carry the marker (the Adapter-Contract discipline — frozen only after a second Host validates).
- **Auth model:** the flag names a secret in the AD-10 resolver namespace; it resolves ONCE at startup into a `SecretString`; the layer compares per request. Reusing the engine's `SecretResolver` port keeps one secret store for the Host; no new resolution mechanism. HTTP only, no TLS — a local bind; TLS is a deployment concern (D2), never a crate feature.
- **SSE + cursor:** v0's pull is per-instance/per-family exactly as `resync_events` defines (family-major, cursor-lossless) — the AD-14 GLOBAL cursor is 17-4's outbox; the route shape (`?cursor=` + batch payload) is chosen so the global export lands beside it, not over it. The resync contract's recommended order (subscribe FIRST, then resync) composes SSE + pull naturally; v0 leaves the per-family dedup to the Host, per the engine's documented contract.
- **"drain" in v0:** absent by the story's own sentence — "plus `drain` and the Task verbs once Epics 16 and 17 land" (proposal :363) means today's Epic 18 (18-4's AD-15 drain composite) and Epic 19. 17-2's SIGTERM bounded stop ladder is projected as `stop {window_secs}` — the SAME `Engine::stop` call the drain uses — never renamed `drain`. No premature composite.
- **Default-off bind:** FR-41's "binds only where explicitly configured" reads: no flag, no API. The host loop is unchanged without it; 17-5's container recipe documents turning it on.

## I/O Impact Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Authenticated read | valid bearer | facade JSON, `schema_version`-stamped | unknown name → 404; invalid transition → 409 |
| Missing/wrong bearer | any route | 401; no handler runs | same for SSE (refused at HTTP level) |
| Unresolvable bearer ref | startup | typed `Startup` refusal, surfaced; nothing binds | no engine work done |
| SSE live events | running agents | `EngineEvent` frames; `Lagged` surfaced in-stream | client hang: connection closes; engine unaffected |
| Cursor pull | `?cursor=` | committed events + next cursor + `torn_tail_skipped` | cursor past a family's count → typed error (never clamped) |
| API off (default) | no `--listen` | byte-identical 17-2 serve | — |
| Bind failure | addr/port taken | surfaced `Startup` error naming the address | exit nonzero (system class per the house table) |

## Code Map

- `crates/hekma-host/src/lib.rs` — `ServeOptions` (:56); `serve_with_signal` (:267): the mount sits between the started notice (:431) and the signal wait (:441); the host runtime (:359); the engine open (:415-430); the drain (:467). `src/control/` NEW (mod/auth/sse/error).
- `crates/hekma/src/cli/serve.rs` — the `--listen`/`--bearer-secret` flags (parse/dispatch pins; the sync-only embed-clean audit holds).
- `crates/hekma-host/Cargo.toml` — the axum edge: the story's ONE new dependency.
- `crates/hekma-engine/src/engine.rs` — `Engine` (:139); `subscribe` (:492); `fleet` (:667); `fleet_entry` (:713); `effective_capabilities` (:963); `start` (:986); `stop` (:1023 — the window param); `pause` (:1045); `resume` (:1059); `instance_status` (:1108); `effective_config` (:1188); `reveal_secrets` (:1217 — deliberately NOT projected); `set_config` (:1254); `resync_events` (:1439).
- `crates/hekma-engine/src/domain/bus.rs:113` — `EngineEvent` (the SSE payload).
- `crates/hekma-engine/src/domain/resync.rs` — `ResyncCursor` (:127); `ResyncBatch` (:166); `torn_tail_skipped` (:179); the ordering contract (module docs).
- `crates/hekma-engine/src/ports/secret_resolver.rs` — `SecretResolver` (:54); `CompositeSecretResolver::env_then_file` (:211).
- `crates/hekma-engine/src/paths.rs:383` — `secrets_file()` (the resolver's file leg).
- `crates/hekma-engine/src/domain/supervisor/mod.rs:682` — `DiagnosticSink` (API-start/bind notices join the host's one stream).
- `crates/hekma-engine/src/domain/error.rs:279` — `EngineError` (the mapper's input).
- `crates/hekma-engine/src/domain/fleet.rs:430` — `FleetEntry` (`budget`/`usage`/`metering_source` — the usage/budget projection).
- `.github/workflows/ci.yml` — boundary allowlist (:658); tarpaulin ≥95 (:1246, :1307-1308).
- `docs/commands.md:421` — the serve section the flags extend; `docs/control-api.md` NEW.

## Verification

**Commands:**
- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace --all-targets` — pass, including the control-API conformance suite.
- `python3 scripts/check_docs.py` — pass.
- `cargo tree -p hekma-host -e normal` — engine + tokio + the recorded axum set; the before/after diff pasted into the PR; `cargo tree -p hekma -e normal | grep axum` — empty (the CLI never grows the edge).
- A curl-level smoke (two terminals): `hekma serve --listen 127.0.0.1:8477 --bearer-secret control_bearer` with the secret set; `curl` `/v0/version` and `/v0/fleet` with and without the bearer (200 vs 401); drive a fake agent via `POST /v0/instances` + `/start`, watch `GET /v0/events` (SSE) and one cursor pull; `/stop`.

## Ask First

None at approval time — the three candidate questions each resolve from settled text: the drain composite is deferred by the story's own sentence (proposal :363); the global cursor is 17-4's (AD-14 amendment); the axum gate is a PR-time conditional living in the frozen tree-diff task (the 2026-10-19 scratch pre-check: within the estimate's tower family, ~8 leaf extras to record — fallback not triggered).

## Spec Change Log

- 2026-10-19 (supervisor approval): approved as drafted — anchors spot-verified, all three
  ambiguity resolutions sound (drain deferral from the story's own sentence; per-family
  cursor pull with the global export shaped to land beside it; default-off bind from
  FR-41's explicit-configuration rule), the axum tree pre-check recorded in the frozen
  tree-diff task (tower family within the D2 estimate, ~8 leaf extras to name; the
  hyper-direct gate stays live at PR time). Spec → in-progress.

## Spec Change Log

- 2026-10-19 (supervisor approval): approved as drafted — anchors spot-verified, ambiguity
  resolutions sound, axum tree pre-check recorded. Spec → in-progress.
- 2026-10-19 (review round, 29 findings — all applied): load-bearing — bind-failure-after-
  adoption now drains before the surfaced Startup error (the false "host still starts"
  comment is gone; the frozen matrix's abort semantics kept, no agent dies without the
  ladder); every error class is now the versioned envelope (extractor rejections, fallback
  404, 405 — inside the auth layer); malformed stop bodies answer 400, only a truly absent
  body means the default window; empty resolved secret refuses at startup; RFC 9110 scheme
  case-insensitivity + duplicate-header 401; relative manifest paths refused; config writes
  serialized per host; steering warnings pinned end-to-end over the wire; the serve flags
  tested at BOTH binary surfaces (parse pins + real-binary leg); the shutdown abort
  backstop observed by test (stalled client, bounded return); a REAL torn tail drives the
  surfaced-skip path; default-off byte-identity pinned as the exact 17-2 notice sequence;
  StatusDetail mirror made exhaustive (no rest pattern — a new engine field breaks the
  compile); one schema-version constant; host_version top-level; register answers
  committed-truth 201 + Location; non-loopback binds warn loudly (never refused — 17-5's
  container recipe); manifest-path execution + CORS posture documented. Known cosmetic
  frozen slips left as-is (human renegotiation): the tree-diff task lists an axum `sse`
  feature that 0.8.9 does not have (unconditional since 0.7; shipped features
  http1/json/tokio/query, erratum in Cargo.toml). The early-API-death notice is a
  documented seam (accept-loop failure is not portable to force in a test). Gates green:
  fmt / clippy -D warnings / workspace tests (known env smoke only) / check_docs 28 /
  test_automation 34/34. Spec → done.

## Suggested Review Order

**The mount + lifecycle (entry point)**

- The API mounts inside the host's serve flow: bind after the engine opens (a bind failure DRAINS before failing), stop-accepting first on shutdown, the 1 s abort backstop.
  [`control/mod.rs`](../../crates/hekma-host/src/control/mod.rs) · mount step in [`lib.rs`](../../crates/hekma-host/src/lib.rs)

**Auth**

- Bearer middleware: startup-resolved secret (empty or unresolvable refuses), RFC 9110 scheme matching, constant-time compare, duplicate-header 401.
  [`auth.rs:46`](../../crates/hekma-host/src/control/auth.rs#L46)

**Routes + the error envelope**

- The handlers projecting the facade; the ApiJson extractor + fallbacks that make EVERY error the versioned envelope.
  [`routes.rs:169`](../../crates/hekma-host/src/control/routes.rs#L169)

**SSE + cursor**

- Live events with Lagged surfaced in-stream; cooperative close; pull via resync_events verbatim.
  [`sse.rs:41`](../../crates/hekma-host/src/control/sse.rs#L41)

**Proof**

- The real-HTTP conformance suite (auth matrix, lifecycle vs fake agents, torn tail, stalled-client backstop, warning surfacing).
  [`control_api.rs`](../../crates/hekma-host/tests/control_api.rs) · the CLI-flag legs in [`serve_cli.rs`](../../crates/hekma/tests/serve_cli.rs)

**Docs + the tree record**

- The v0 contract, security notes (manifest execution, non-loopback), and the unstable-0.x marking.
  [`control-api.md`](../../docs/control-api.md) — the NFR-8 before/after tree diff rides the PR body.
