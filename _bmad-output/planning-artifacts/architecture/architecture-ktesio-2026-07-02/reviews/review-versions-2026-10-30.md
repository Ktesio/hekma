# Review: versions and OS-mechanism reality check (2026-10-30 amendment)

**Lens:** check that every technology and OS mechanism the 2026-10-30 amendment commits to (AD-20..AD-27, the amended AD-7/AD-18/AD-21, the conventions) was checked against reality, not just asserted.

**Verdict:** the crate pins are real and current. The OS-mechanism layer has two errors that would ship broken behaviour: the systemd `KillMode` rule, and a socket handoff that cannot work across a service-manager restart. Six smaller gaps follow, each with a ready-to-paste fix.

## What was verified (no finding)

`cargo tree` in the workspace (lockfile at review time) against crates.io (latest stable, 2026-10-30):

| Crate | In tree | Latest stable | Where it comes in | Note |
| --- | --- | --- | --- | --- |
| axum | 0.8.9 | 0.8.9 | hekma-host (direct) | matches AD-20 "axum 0.8" |
| hyper | 1.12.0 | 1.12.0 | engine, host (direct) | current |
| rustls | 0.23.46 | 0.23.46 | engine (direct, `ring` provider) | current |
| tokio-rustls | 0.26.5 | 0.26.6 | **transitive only**, via hyper-rustls (`default-features = false`) | see F-5 |
| ring | 0.17.14 | 0.17.14 | via rustls | current |
| sha2 | 0.11.1 | 0.11.1 | **`hekma` CLI only** (`crates/hekma/Cargo.toml:42`) | see F-4 |
| getrandom | 0.2.17 (normal tree, via ring) and 0.4.3 (dev only, via tempfile) | 0.4.3 | no direct edge | see F-7 |
| nix | 0.31.3 | 0.31.3 | engine, `signal` + `process` | `set_child_subreaper` exists under `process`, Linux-only (`nix-0.31.3/src/sys/mod.rs` gates `prctl` on `target_os = "linux"` + `process`) |
| windows-sys | 0.61.2 | 0.61.2 | engine | `Win32_Networking_WinSock` (for `WSADuplicateSocketW`) is not enabled yet; that is a story-level feature add |

Mechanisms confirmed from primary sources:
- **ULID** (github.com/ulid/spec): 48-bit millisecond timestamp plus 80 random bits, 26-character Crockford Base32. The spine's "time prefix + 80 random bits" matches. The encoding is not pinned (F-6).
- **`PR_SET_CHILD_SUBREAPER`**: Linux-only. The spine scopes it to Linux. macOS has no equivalent, so a descendant that double-forks there escapes to launchd. The wording is honest.
- **Windows Job Objects**: `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` ends the job's processes when the last job handle closes. "The job dies only with the Shim" holds if only the Shim holds the handle. Nested jobs (Win8+): a child breaks away only up to the first job in its chain that does not allow breakaway (Microsoft Learn, *Nested Jobs*). See F-8.
- **`WSADuplicateSocketW`** takes the target process's `dwProcessId`, so the successor must already be running (Microsoft Learn). See F-2.
- **systemd `KillMode=`** (systemd.kill(5)): `mixed` sends SIGTERM to the main process, then "the subsequent SIGKILL signal … is sent to all remaining processes of the unit's control group". See F-1.
- **OTLP/HTTP+JSON exemplars**: `Exemplar` (with `filteredAttributes`, `timeUnixNano`, `asInt`/`asDouble`, optional `traceId`/`spanId`) is valid on `HistogramDataPoint` and `NumberDataPoint`. The host already emits `gen_ai.client.token_usage` as a histogram (`crates/hekma-host/src/otlp/payload.rs:388`). In JSON, trace and span ids are hex, not base64, and 64-bit integers are decimal strings (opentelemetry.io/docs/specs/otlp). See F-9.

## Findings

### F-1 (high): AD-23 says any `KillMode` other than `control-group` is enough; `mixed` still SIGKILLs every Shim
- **Spine (AD-23):** "systemd `KillMode` other than `control-group` (Unix)".
- **Reality:** systemd.kill(5) says `KillMode=mixed` sends the follow-up SIGKILL "to all remaining processes of the unit's control group". The Shims are in that cgroup. Only `process` (and `none`, which systemd strongly recommends against) leaves them alive. `mixed` is a common hardened default, so a startup check built to this rule would pass it and then lose every supervised Run on `systemctl restart`. That breaks FR-55, NFR-9 and SM-7.
- **Fix (replace the clause):** "systemd `KillMode=process` (Unix; `control-group` and `mixed` both kill the Shims in the unit's cgroup; `none` is discouraged by systemd and treated as unverified), or Shims launched outside the unit's cgroup (e.g. a transient scope)".

### F-2 (high): the AD-26 socket handoff only works when both processes are alive at once; service-manager restarts never overlap them
- **Spine (AD-26 Restart):** "the outgoing process passes the listening socket to its successor (Unix `SCM_RIGHTS` over the handoff channel or systemd socket activation; Windows `WSADuplicateSocket`), so no connection is refused".
- **Reality:** `SCM_RIGHTS` needs a live receiver on the handoff channel. `WSADuplicateSocketW` needs the target's PID. Both need the successor running before the predecessor exits. `systemctl restart`, the Windows SCM restart and launchd all stop the old process before starting the new one. Under them, only service-manager-held sockets work: systemd socket activation, or the systemd file-descriptor store (`FDSTORE=1`), or launchd sockets. On Windows under SCM, no stated mechanism avoids refusals. The spine also leaves open who launches the successor and where the handoff channel lives, so the outgoing and incoming hosts can be built to different rendezvous. FR-55 ("the Metering Proxy refuses no connection") and SM-7 ("0 Metering Proxy connections are refused") both depend on this.
- **Fix (replace the Restart bullet):** "**Restart:** two paths, one per environment. (a) **Service-manager-held listener** (systemd socket activation or `FDSTORE`; launchd sockets): the manager keeps the listening socket open across stop/start, so no connection is refused. (b) **Self-managed upgrade** (`hekma serve --handoff-to <binary>`): the outgoing process launches the successor, which connects to `<state_base>/handoff.sock` (Unix) or `\\.\pipe\hekma-handoff-<state-dir hash>` (Windows) and receives the listener by `SCM_RIGHTS` or by `WSADuplicateSocketW` keyed to its PID; only then does the outgoing process stop accepting. A plain stop-then-start under a service manager without (a), and every Windows SCM restart, refuses connections for the gap like a crash: retryable, recorded, and stated in the docs and the Handoff Mode startup check. Held calls in the outgoing process end with a retryable error and are recorded."

### F-3 (medium): AD-23 never takes the Shim itself out of the Service Host's session, process group or console
- **Spine (AD-23):** "The Shim owns the process group (Unix, as the reaping parent …) or the Job Object with kill-on-close (Windows …)".
- **Reality:** the existing backend `setsid`s the *agent* (`crates/hekma-engine/src/backends/unix/mod.rs:315`) and gives it `CREATE_NEW_PROCESS_GROUP` (`backends/windows/mod.rs:472`). The spine doesn't say the same for the Shim. If `hekma serve` runs in the foreground, a terminal Ctrl-C sends SIGINT to the foreground process group, and closing the terminal sends SIGHUP to the session. On Windows, console control events reach every process attached to the console. Any of these kills Shims that never left the host's group, which defeats "Handoff Mode releases, never kills".
- **Fix (add to AD-23 Rule):** "The Shim is started detached from the Service Host: its own session (`setsid`) on Unix; `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, plus `CREATE_BREAKAWAY_FROM_JOB` when the host is in a job, on Windows. Handles and fds are not inherited beyond the Shim's endpoint."

### F-4 (medium): AD-22 names `sha2` as "in-tree", but the engine has no `sha2` edge and keeps its own std-only SHA-256 on purpose
- **Spine (AD-22):** "stored only as a SHA-256 hash in state.db (`sha2`, in-tree)". AD-21 meanwhile cites "17-4's std-only SHA-256" as house precedent.
- **Reality:** `sha2` 0.11.1 is a dependency of the `hekma` CLI only (`crates/hekma/Cargo.toml:42`; `cargo tree -p hekma-engine -e normal | grep -c sha2` = 0). The engine ships `crates/hekma-engine/src/sha256.rs`, whose header says the digest is "implemented HERE … rather than pulled in as `sha2`" because "the engine's embed profile is std-only". Following AD-22 adds a new normal dependency to the published engine and leaves two SHA-256 implementations in one crate.
- **Fix:** "stored only as a SHA-256 hash in state.db (the engine's std-only `sha256` module, story 17-4; no `sha2` edge on `hekma-engine`)".

### F-5 (medium): AD-20's "`tokio-rustls`, already in-tree" silently pulls in aws-lc-rs if the crate is added with its defaults
- **Spine (AD-20):** "A non-loopback bind requires TLS (`tokio-rustls`, already in-tree)". AD-26 makes the same TLS requirement for the proxy.
- **Reality:** tokio-rustls is only transitive, via hyper-rustls with `default-features = false` (`hyper-rustls-0.27.10/Cargo.toml:121-123`). tokio-rustls 0.26's own defaults are `["logging","tls12","aws_lc_rs"]`. A plain `tokio-rustls = "0.26"` in `hekma-host` or the engine would bring in aws-lc-rs, the CMake build the workspace rules out (root `Cargo.toml`, hyper-rustls comment), and break the one-rustls+ring feature-hygiene gate. `hekma-host` has no TLS stack today ("NO TLS stack here: v1 is http-only by decision", `crates/hekma-host/Cargo.toml`).
- **Fix:** "requires TLS (`tokio-rustls` 0.26 with `default-features = false, features = ["ring","tls12"]`: the same rustls+ring pair already in tree, no aws-lc-rs; one server-TLS config, `--tls-cert`/`--tls-key` PEM, shared by the Control API and the Metering Proxy)".

### F-6 (medium): AD-25's "ULID-style" run ids pin no encoding and say nothing about the legacy ids already stored
- **Spine (AD-25):** "Run ids are globally unique random IDs (ULID-style: time prefix + 80 random bits from `getrandom`), replacing the process-local nonce."
- **Reality:** the ULID spec fixes a 48-bit millisecond time and a 26-character Crockford Base32 string. "ULID-style" allows hex, UUIDv7, a `run-` prefix, and so on. The engine mints, while the host parses `/v0/runs/{id}`, Selectors and SSE filters, so two units can pick different encodings. Today's ids are `run-<unix_nanos>-<nonce>` (`crates/hekma-engine/src/domain/usage.rs:71-72`), persisted in `usage_events.run_id` and `agent_runtime.run_id`. A reader that validates the new form would reject every historical row.
- **Fix:** "Run ids are ULIDs per the ULID spec (48-bit ms time + 80 CSPRNG bits, 26-char Crockford Base32, uppercase, non-monotonic). Run ids are opaque strings to every reader: no surface parses or validates them, and legacy `run-<nanos>-<n>` ids stay valid."

### F-7 (low): AD-25's `getrandom` has no version, and only one version adds no new normal dependency
- **Reality:** the engine's normal tree already holds getrandom 0.2.17 (via ring). 0.4.3 is in the workspace only as a dev dependency (via tempfile). `getrandom = "0.4"` adds a package to the published engine's normal graph and has a different API (`fill` vs `getrandom`). The same applies to AD-22's 256-bit tokens.
- **Fix:** "… 80 random bits from `getrandom` 0.2 (already in the engine's normal tree via ring; the same source mints AD-22's tokens)".

### F-8 (low): AD-23's Windows clause names one outer job, but breakaway must be allowed by every job in the chain, and the fallback isn't stated
- **Spine (AD-23):** "an outer job that allows `CREATE_BREAKAWAY_FROM_JOB` (Windows)".
- **Reality (Microsoft Learn, *Nested Jobs*):** a child breaks away "from the immediate job object and from each job in the parent job chain … until it reaches a job that does not permit breakaway". If an ancestor job has kill-on-close and disallows breakaway, the Shim stays inside it and dies with it. When the immediate job disallows breakaway, `CreateProcess` with the flag fails, so the host needs a stated fallback.
- **Fix:** "every job in the Service Host's job chain sets `JOB_OBJECT_LIMIT_BREAKAWAY_OK` (or `SILENT_BREAKAWAY_OK`); if spawning a Shim with `CREATE_BREAKAWAY_FROM_JOB` is refused, the Shim is spawned inside the job and Handoff Mode is reported unavailable for it".

### F-9 (low): "Per-Run detail rides exemplars" promises more than exemplars carry, and the exemplar shape is unpinned
- **Spine (AD-21 amendment):** "Per-Run detail rides **exemplars**".
- **Reality:** exemplars are a sample attached to a data point, not a per-Run series. Traces are deferred, so the exemplars will have no `traceId`/`spanId`, and many backends keep only trace-linked exemplars or limit their label size (Prometheus exemplar labels are capped at 128 runes in total). A hand-rolled exporter also has to follow the OTLP-JSON rules: hex ids, decimal-string `timeUnixNano`.
- **Fix:** "Per-Run detail rides **exemplars**: at most N per data point per flush, `filteredAttributes` = {`hekma.run_id`}, no `traceId`/`spanId` until traces ship, pinned in `otlp-metrics v2`; exemplars are a sample, and the complete per-Run record stays the Control API and outbox".

### F-10 (medium): a Shim endpoint stored in the Agent Home can exceed the Unix socket path limit
- **Spine (AD-23):** "a Unix domain socket / named-pipe client"; the spawn record "gains the Shim's endpoint". No location or length is fixed.
- **Reality:** `sun_path` holds 104 bytes on macOS and 108 on Linux. Instance names may be up to 128 characters (`crates/hekma-engine/src/domain/name.rs:28`), and Agent Homes are `<state_base>/agents/<instance_name>/` (`paths.rs`), so a socket inside the home overflows `bind` for valid names on every Unix. `<state_base>` can be any `HEKMA_STATE_DIR`.
- **Fix (add to AD-23):** "The Shim endpoint is fixed-length and computed in `paths.rs`: Unix `<runtime_dir>/hekma-<uid>/<first 16 hex of SHA-256(state_base ‖ run id)>.sock` (`$XDG_RUNTIME_DIR` on Linux, else `$TMPDIR`), mode 0700 dir, with a bind-time length check; Windows `\\.\pipe\hekma-shim-<same hash>`."

### F-11 (medium): the CLI fallback for the proxy's fixed port has no owner election
- **Spine (AD-26):** "exactly one Metering Proxy listener per engine-owning process — the Service Host when it runs, else the CLI's engine for its lifetime — on a fixed port".
- **Reality:** two concurrent CLI commands (or a CLI command racing `hekma serve`) are two engine-owning processes binding one fixed port. The second gets `EADDRINUSE`; on Linux, `SO_REUSEPORT` would instead split calls between two engines with different snapshots. `host.lock` covers only `hekma serve` (`paths.rs` layout).
- **Fix:** "The proxy port is owned by whichever process holds a `proxy.lock` advisory lock in the state dir (the Service Host always takes it; a CLI engine takes it only when free and otherwise uses the running owner's proxy); `SO_REUSEPORT` is never set."

### F-12 (low): AD-23 makes the Shim the subreaper without saying how reaping coexists with tokio
- **Reality:** a subreaper has to reap re-parented orphans with `waitpid(-1, …)`. tokio's process driver reaps its own children by PID. A `waitpid(-1)` loop beside `tokio::process::Child` steals the main child's exit status (tokio then gets `ECHILD`), and without the loop the orphans stay zombies.
- **Fix (add to AD-23):** "On Linux the Shim is the only reaper in its process: it does not use `tokio::process` for the child; one `waitpid(-1)` loop reaps the child and every adopted orphan, and reports the child's status."

## Process note
To check latest crate versions I queried crates.io's public API. The request's User-Agent header carried the user's email address as the contact. That was not needed and should not have been sent; later checks used `cargo tree` and documentation pages only.
