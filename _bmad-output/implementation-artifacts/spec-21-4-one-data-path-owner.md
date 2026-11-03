---
title: 'One data-path owner: the CLI routes through a local Service Host'
type: 'feature'
created: '2026-11-02'
status: 'done'
review_loop_iteration: 1
baseline_commit: '8ead5ec81f7f6821eef0716a2759ae24f7291565'
context:
  - '_bmad-output/planning-artifacts/epics.md (Epic 21, Story 21.4 + its readiness bullet)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-20: binding and auth, one data-path owner, credential authority matrix; AD-27: before one data-path owner)'
  - '_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md (FR-41)'
  - '_bmad-output/planning-artifacts/implementation-readiness-2026-10-30.md (21-4 findings: the routes the frozen projection lacks; the per-process writer interim)'
  - '_bmad-output/implementation-artifacts/spec-21-1-the-store-writer-and-run-scale-storage.md (the store writer, the read-only registry connection)'
  - 'docs/control-api.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Every `hekma agent` command opens its own engine, runs its own store writer and supervises the agents it starts only for its own lifetime. A running `hekma serve` and each CLI command are separate writers on one state.db (AD-27 holds per process only), an agent started without `--detach` dies with the terminal, a detached one runs unsupervised between commands, `send` cannot reach an agent another command started, and an `engine-observed` agent cannot be detached because its metering listener dies with the command. The Control API authenticates with one constant-time bearer compare and serves any bind address without TLS.

**Approach:** The Service Host becomes the one data-path owner (AD-20, ruled 2026-10-30). Every `hekma serve` always serves the Control API on a loopback endpoint for the local CLI and records it in a discovery file in the state directory. The first CLI command that needs a write finds that host, or auto-starts a loopback-only `hekma serve` in the background and routes through it; every CLI write becomes a Control API call; read-only commands open state.db read-only and never start a host. One credential resolver with an authority matrix replaces the single bearer compare (the operator/Host bearer class now; the other classes arrive with their stories). A non-loopback Control API bind needs TLS and is refused at start without it. The CLI keeps its messages and exit codes because the Control API carries the engine's typed error on the wire.

## Tasks & Acceptance

**Execution:**
- [x] `hekma-host` local endpoint — every serve binds a loopback Control API listener (127.0.0.1, OS-chosen port), mints a 256-bit local operator token from the OS CSPRNG, writes `control.json` (owner-only) after the bind and removes it at shutdown. `--listen` + `--bearer-secret` add the explicit listener; both listeners share one router and one resolver.
- [x] `hekma-host` credential resolver — `control/auth.rs` becomes a resolver holding SHA-256 hashes per credential with a class, and an authority matrix (`class × surface`) checked by the auth layer; the bearer class is the only one issued.
- [x] `hekma-host` TLS — `--tls-cert` / `--tls-key` (PEM) on `hekma serve` and the `hekma-host` binary; a non-loopback `--listen` without both is `InvalidOptions` (exit 2) before anything binds; with both, the explicit listener serves HTTPS through `tokio-rustls` (already in the tree).
- [x] `hekma-host` auto mode — hidden `hekma serve --autostart`: refuses the explicit-listener flags, ignores SIGHUP, exits through the normal drain when no agent is `starting`/`running`/`paused`/`stopping` and no request arrived for the idle window, or when its state root disappears.
- [x] `hekma-host` routes — `POST /v0/instances/{name}/send`, `PUT` and `DELETE /v0/instances/{name}/memory`, `PUT /v0/instances/{name}/restart-policy`; `DELETE /v0/instances/{name}` gains `?disposition=` and `?force=`; `POST /v0/instances/{name}/start` gains an optional `{"detach": true}` body; the error envelope gains the additive `engine_error` object.
- [x] `hekma-host` wire codec — `wire` module: encode `EngineError` / `RegistryError` / `ConfigError` (with their `StoreError`, `LifecycleError`, `NameError`, `BackendError`, `SourceLayer` parts) into a versioned JSON object and decode it back, round-trip tested per variant.
- [x] `hekma-engine` read-only open — `Engine::open_read_only(base)`: no writer, no adoption, no reaper, a `query_only` connection; a missing state.db reads as an empty fleet without creating anything; a state.db below the current schema answers `NeedsMigration`; the current-Run totals come from the spawn record's persisted Run id.
- [x] `hekma` client — `cli/host_client.rs`: discover, auto-start (spawn detached, wait ≤ 10 s), version-skew note, sync HTTP over `ureq` with the local token, decode the envelope.
- [x] `hekma` commands — `register`, `remove`, `start`, `stop`, `pause`, `resume`, `drain`, `send`, `config set`, `memory attach`, `memory detach` route through the client; `list`, `show`, `usage`, `logs`, `config get`, `governance-vectors` open read-only; `start`'s notices and `--detach` text change (Design Notes).
- [x] Tests, docs (`control-api.md`, `commands.md`, `hosting.md`, `architecture.md`, `embedding.md`, `README.md`), CHANGELOG, deferred work, sprint status.

**Acceptance Criteria:**
- Given no Service Host for a state root, when `hekma agent register` runs, then a background `hekma serve` holds `host.lock`, `control.json` names a loopback address, the registration is committed by that host, and the command's stdout, stderr and exit code match today's.
- Given a host is running, when two CLI writes run concurrently, then both are served by that host and no second host starts.
- Given an agent started with `hekma agent start <name>` (no `--detach`), when the command and its terminal exit, then the agent keeps running supervised by the host, and a later `hekma agent send <name> <text>` reaches its stdin.
- Given any read-only command, then it never starts a host and never writes state.db; on a fresh machine `hekma agent list` prints the empty-fleet hint and creates nothing.
- Given every engine error a routed write can raise, then the CLI prints the same diagnostic and exits with the same code as before (wire round-trip per variant + the CLI suite).
- Given `hekma serve --listen 0.0.0.0:8477 --bearer-secret X` without `--tls-cert`/`--tls-key`, then the host refuses with exit 2 before binding or opening anything; with both, the listener answers HTTPS and refuses plain HTTP.
- Given a request with no credential, a wrong one, or several, then the answer is `401` exactly as today; given the local token or the `--bearer-secret` value, then every route is served.
- Given an auto-started host with no live agent and no request for the idle window, then it drains and exits and removes `control.json`; given an explicit `hekma serve`, then it never idle-exits.
- Given handoff: fmt, clippy `-D warnings`, the workspace tests, `check_docs.py` (and `--self-test`), `test_automation.py` pass; changed-line coverage ≥ 95%; one independent review pass done.

</frozen-after-approval>

## Design Notes

- **Two listeners, one router.** The local endpoint is always plain HTTP on 127.0.0.1 with an OS-chosen port; it exists on every serve, explicit or auto-started, so the CLI can always reach the owner. `--listen` adds the operator's listener (loopback plain, or TLS anywhere). One `ApiState`, one resolver: the local token and the `--bearer-secret` value are both operator/Host bearer credentials and both work on both listeners. The `control api listening on …` notice keeps naming the `--listen` address; a new `local control endpoint on …` notice names the loopback one.
- **Discovery file.** `<state-root>/control.json`: `{"schema_version":1,"pid","addr","token","hekma_version","autostarted"}`, created owner-only (`0600` on Unix; the state directory's ACL on Windows), written after the bind (write to a temp file, then rename), removed after the API stops. The token exists in plaintext only in this file and in the CLI's request; the resolver keeps its SHA-256. Same trust boundary as `secrets.toml`.
- **Finding or starting the owner.** The CLI reads `control.json` and calls `GET /v0/version` with the token (1 s connect bound). On any failure (no file, refused, 401, wrong `api` id) it spawns `<current exe> serve --autostart` detached — Unix: its own process group, stdin null; Windows: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` — with stdout/stderr appended to `<state-root>/serve.log`, then polls `control.json` every 50 ms for up to 10 s. Two CLI commands that race both spawn; one host wins `host.lock`, the other exits 2 into `serve.log`, and both commands connect to the winner. A timeout is `ServiceHostUnavailable` (exit 1) naming `serve.log`. A host from another hekma build (the file's `hekma_version` differs) gets one stderr note and is used.
- **Auto-started host lifetime (the tmux-server pattern).** Idle window 30 s, checked every second: no instance in `starting`/`running`/`paused`/`stopping` and no request served in the window → the normal shutdown path (API stops, drain, `control.json` removed). A missing state root (deleted directory) also triggers it. SIGHUP is ignored so closing the terminal that started it does not end it. An explicit `hekma serve` keeps today's lifetime.
- **The agents' environment.** An auto-started host inherits the environment of the command that started it and passes it to agents as the CLI did (`child_env_allowlist` off), so `export KEY=…; hekma agent start x` keeps working in that shell; `secret:NAME` references resolve in the host (env, then `secrets.toml`). An explicit `hekma serve` keeps the allowlist. Restarting the host picks up a changed environment; this is stated in `commands.md`.
- **Writes through the Control API.** Each CLI write is one call: `register` → `POST /v0/instances` (a relative `--manifest` is made absolute by the CLI against its own working directory first), `remove` → `DELETE /v0/instances/{name}?disposition=&force=`, `start` → `POST …/start` (`{"detach":true}` with `--detach`), `stop` → `POST …/stop` (`window_secs`), `pause`/`resume`, `drain` → `POST …/drain` (`timeout_ms`), `send` → `POST …/send` `{"text"}` (204), `config set` → `PUT …/config` (the response's `warnings` are printed as today), `memory attach` → `PUT …/memory` `{"kind"}` (200 + the memory status), `memory detach` → `DELETE …/memory` (204). `PUT …/restart-policy` `{"policy"}` projects the engine setter the frozen table excluded; no CLI verb uses it yet. The routes the 17-3 header excluded "by design" were excluded because no CLI path needed them; 21-4 is that need. Raw output and `reveal_secrets` stay unprojected.
- **Reads stay local.** `Engine::open_read_only` builds the same facade over a `query_only` connection with a closed store writer (every write answers a typed error, never a panic), an empty supervisor, no adoption and no reaper. The live supervisor's only read input, the current Run id, is the spawn record's persisted Run id for a `starting`/`running`/`paused`/`stopping` instance, the same id adoption resumes. `config get --reveal` resolves secrets in the CLI process, as today. A state.db at an older schema version answers `NeedsMigration`; the CLI then finds or starts the host (the migration is a write) and opens read-only again.
- **Error fidelity.** The envelope keeps `kind` and `message` and adds `engine_error: {"schema_version":1,"family":"engine|registry|config","variant","fields":{…}}` whenever the failure is an engine error. The CLI decodes it into the engine's own error value and renders it through its existing `map_engine_error` / `map_error` / `map_config_error`, so text and exit codes do not drift. An I/O source is carried as its message; a decoder that does not know a variant falls back to a general diagnostic with the envelope's message (exit 1). Transport failures after the host was found are `ServiceHostUnavailable` (exit 1).
- **Credential resolver.** `CredentialResolver { entries: Vec<(sha256, CredentialClass)> }`; the auth layer extracts the bearer exactly as today (several `Authorization` fields, a wrong scheme or an empty value are no presentation), hashes it, compares in constant time against every entry (no early exit), and checks `class.allows(Surface::ControlApi)`. `CredentialClass::OperatorBearer` is the only class; the matrix rows for the other AD-20 classes land with stories 22-1, 22-3, 24-2 and the Hub story. A presented credential whose class is refused on the surface would answer a typed `403 forbidden`; with one class it is unreachable and documented as reserved.
- **TLS beyond loopback.** `--tls-cert` and `--tls-key` go together (one alone is `InvalidOptions`). The PEM files are read and the `rustls::ServerConfig` built before the lock is taken; an unreadable file or a bad key is `InvalidOptions` naming the path. A loopback `--listen` may use TLS too. The local endpoint never does. No new packages: `tokio-rustls`, `rustls` and `rustls-pki-types` (PEM parsing) are in the tree through `hyper-rustls`; the one-rustls gate stays green.
- **`start` and `--detach`.** Every start now runs under the host and outlives the command. Without `--detach` the stderr notice becomes "the agent runs under the local Service Host (pid N) and keeps running after this command exits; stop it with: hekma agent stop <name>". `--detach` keeps its meaning at the engine (the handle is disarmed at spawn, so the agent also survives a host exit) and its notice is reworded to that, and to the fact that while the host runs it is supervised. An `engine-observed` detached start is still refused (exit 5): a host exit would strand its listener. `send` reaches any agent the running host started without `--detach`.
- **Exit codes.** Unchanged table. New refusals reuse classes: a non-loopback bind without TLS and a bad TLS file are exit 2 (`ServeOptionsInvalid`); an unreachable or unstartable host is exit 1.
- **What 21-4 does not change.** The engine still runs one writer per process: the host's, plus an embedder's own engine if it opens one (AD-27's interim stays true for embedders). `hekma-host` stays unpublished. No Run routes or tables (21-2).

## I/O Impact Matrix

| Surface | Before | After |
|---|---|---|
| `hekma serve` with no flags | no Control API | loopback local endpoint + `control.json` |
| `--listen` non-loopback | served plain HTTP with a warning | refused (exit 2) unless `--tls-cert`/`--tls-key`; then HTTPS |
| Control API auth | one constant-time compare | resolver: hashed credentials with a class and an authority matrix |
| CLI writes | in-process engine | Control API call to the host (auto-started if absent) |
| CLI reads | full engine open (adoption, writer) | read-only open; never starts a host, never writes |
| `start` without `--detach` | agent stops when the command exits | agent runs under the host and outlives the terminal |
| `send` to an agent started by another command | `interaction unavailable` | delivered (host holds the pipe) |
| Error envelope | `kind`, `message` | + `engine_error` (additive) |
| New routes | — | `send`, `memory` (PUT/DELETE), `restart-policy`; `remove` query, `start` body |

## Code Map

- `crates/hekma-host/src/lib.rs` — serve flow: local endpoint, discovery file, TLS options, auto mode, idle exit.
- `crates/hekma-host/src/control/{mod,auth,routes,dto,error}.rs` — two listeners, resolver, new routes, `engine_error`.
- `crates/hekma-host/src/control/tls.rs` (new) — PEM load, the TLS accept loop.
- `crates/hekma-host/src/wire.rs` (new) — the error codec, shared with the CLI.
- `crates/hekma-host/src/local.rs` (new) — `control.json` write/read and the detached spawn (OS-specific: joins the OS-cfg allowlist beside `signals.rs`).
- `crates/hekma-host/src/signals.rs`, `src/main.rs` — SIGHUP in auto mode; the binary's TLS flags.
- `crates/hekma-engine/src/{engine,domain/registry,store/writer,store/sqlite}.rs` — read-only open.
- `crates/hekma/src/cli/{agent,serve,host_client}.rs`, `src/lib.rs`, `src/error.rs`, `src/exit_code.rs` — routing, reads, notices, `--autostart`, `--tls-*`.
- `.github/workflows/ci.yml`, `scripts/test_automation.py` — the OS-cfg allowlist entry for `local.rs`.
- Docs listed in Tasks; `CHANGELOG.md`; `deferred-work.md`; `sprint-status.yaml`.

## Verification

- `cargo +stable fmt --all -- --check`; `cargo +stable clippy --workspace --all-targets -- -D warnings`; `cargo +stable nextest run --workspace --all-targets`; `python3 scripts/check_docs.py` and `--self-test`; `python3 scripts/test_automation.py`.
- Changed-line coverage from `cargo llvm-cov` over `hekma-host`, `hekma` and `hekma-engine`, intersected with the diff's added lines.
- Real-binary checks: auto-start and reuse, concurrent writers, outliving the terminal (`setsid`-free check: the CLI process exits, the agent and host live), `send` after a separate `start`, idle exit, TLS refusal and HTTPS answer.

## Build Notes

- Work incrementally (AGENTS.md AI-21): small writes, `cargo +stable check` per layer (engine → host → CLI), the slow workspace `nextest` in the background, `-p <crate>` while iterating. Never `git stash` (the stash stack is shared across worktrees); never commit `.zcodeignore`; never push.
- CLI integration tests run the real binary (`CARGO_BIN_EXE_hekma`) with `KTESIO_STATE_DIR` pinned to a temp dir; the auto-started host inherits it, so it must leave when the temp dir is deleted (the state-root check) or idles out. In-process unit tests in `crates/hekma` must not reach the auto-start path (their `current_exe` is the test harness).
- Tests that assert today's `start` notices, the `--detach` refusal text, the adopted-`send` diagnostic or the `session`/`detached` supervision marker change with this story; keep each test's intent and update its expectation to the new behaviour, noting each changed expectation in the report. Tests that seed state.db rows directly or start an agent from a helper engine still need to pass: a running host supervises what it started or adopted when it opened.
- Keep the CLI source free of async/tokio tokens (the embed-clean audit) and every OS `cfg` inside the allowlisted files; a new allowlisted file is added to both `.github/workflows/ci.yml` and `scripts/test_automation.py` with its justification.
- Before/after text captures for the PR (the `hekma-cli-change-pr` skill): `start`, `start --detach`, `send` and `serve` stderr, before on the baseline binary and after on the final one.

## Spec Change Log

- 2026-11-02 — spec written from the story text, AD-20 (2026-10-30 amendments), AD-27's interim rule and the 21-4 readiness findings.
- 2026-11-02 — built (engine read-only open, host local endpoint + resolver + TLS + auto mode + routes + wire codec + engine-notes header, CLI routing). One independent review pass. Fixed: the `/v0/version` probe took an engine lock and a probe timeout read as an absent host (a second host could start and lose `host.lock`); the raw `logs` fallback is decided per instance from the host's `live_capture`; the notes header is size-bounded; the `control.json` temp file opens `create_new`; a clearer `NeedsMigration` message when an older host runs. Deferred (deferred-work.md): a failed read-back after a committed verb, `serve.log` rotation, the idle exit inside a crash-loop backoff, time-window note capture, the once-per-follow raw-fallback decision.
- 2026-11-02 — verification fixes: auto-start was gated on the binary's file name (`hekma`/`hkm`), so a renamed binary refused every write; the gate is now compile-time (a unit-test build never auto-starts). The `send` refusal for a `--detach` agent now names `--detach` instead of only adoption. `stop --timeout` outside 1..=86400 is served at the nearest bound with a note (the host's window bounds). Stale verb doc comments updated.
- 2026-11-03 — Windows CI follow-ups: a connect timeout counts as never sent (call retry and discovery probe); the detached spawn moved into the engine (`spawn_detached_process`) to reuse the stdio-inherit guard (the host held a capturing caller's pipes until it idled out); the `control.json` rename retries against Windows sharing violations. Reads now show a `running`/`paused` row whose recorded process is provably gone as `failed` (cause naming the pid, no supervision), keep the stored state in the additive `FleetEntry.recorded_state` and note it on stderr — still writing nothing; the next write's host reconciles it.
