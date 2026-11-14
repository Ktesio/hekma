# Changelog

All notable changes to Hekma (Ktesio through v0.7.0) are generated from git history when a version tag is published.

Release automation updates this file with a pull request after each `vMAJOR.MINOR.PATCH` tag.

> **Unreleased — after a crash, the next host does what the dead one asked (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Each Service Host records its Handoff Mode in the state directory** before it adopts anything: on, off, or not chosen.
> - **After a crash, the next host acts on that record.** With `--handoff` it reconnects to the dead host's agents. With `--no-handoff` it stops each one the ordinary way, and their Runs end with the new cause `engine-crash`, named in the started notice. With no mode chosen (the default), or no record, it adopts them as before. Detached starts are never stopped.
> - **A Shim that dies no longer makes its live agent look crashed.** The host holds the agent directly from then on (no stdin; it no longer outlives the host), notes it, and never starts a second copy. An agent that died with its Shim is recorded as an exit whose code is unavailable.
> - **Engine surface (breaking within the unreleased 0.12.0):** `TransitionCause` gains `EngineCrash` (`engine-crash`). Additive: `EngineOptions::host_posture` (`HandoffPosture`), `Engine::host_recovery` (`HostRecovery`, `PreviousHost`), and `Engine::clean_up_after_crash`. `state.db` migrates one way to schema 29.

> **Unreleased — Handoff Mode and in-place upgrades, a preview (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **`hekma serve --handoff`** (or `HEKMA_HANDOFF=on`): when the host stops, agents running in their own Shim are released instead of stopped. They keep running, and the next host reconnects to them. Agents held directly are still stopped. Off by default; `--no-handoff` keeps the drain.
> - **`hekma serve --upgrade`** (and `POST /v0/host/upgrade`, Operator only) hands the running host over in place to the `hekma` binary now installed, on Unix. The new host connects over an owner-only socket with a one-time token; until it does, nothing stops. The old host then releases its Shim-held agents and hands over the metering proxy's listening socket (never closed, so no connection is refused) and its host lock. The new host reconnects to every agent and serves, and only then does the old one exit.
> - **`hekma self-update` moves the running host onto the new binary in place** after a successful update (Unix), as `hekma serve --upgrade` does. With no host running it asks nothing; if the host cannot upgrade, the note says to restart it. `--no-restart` replaces the binaries only.
> - **The service-manager check.** When agents run in their own Shim, the started notice says whether the service manager keeps them running when the host stops. Under systemd it reads the unit's `KillMode`: `process` or `none` is `verified`, while `control-group` or `mixed` is `unsafe` and gets a warning. Elsewhere the answer is `unverified`, with how to check. `GET /v0/host` (Operator only) carries the answer with the host's mode. Hekma never refuses to start on it.
> - **Not yet:** rolling back once the hand-over has happened (if the new host fails then, the agents keep running and the next `hekma` command starts a host that reconnects), launchd and Windows checks, and Windows hand-over.
> - **Engine surface (additive):** `Engine::shutdown_for_handoff`, `Engine::shutdown_for_upgrade`, `HandoffShutdown`, `MeteringProxyOptions::inherited_listener` (`InheritedListener`), and the `handoff` module. No schema change.

> **Unreleased — bypass detection, a preview for agents in their own Shim (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Each Shim watches its agent's process tree** every 5 seconds for established TCP connections to a known provider host: the built-in provider hosts, every endpoint binding's upstream, every open Run's upstream and every origin pinned beside a held key. Hekma resolves the hosts (every 60 seconds, keeping the last good addresses) and hands the list to each Shim; the Shim compares address and port. Linux and macOS; Windows Runs stay `unreported` until agents run in a Shim there.
> - **On a Run with key custody on, a finding is the `bypass` Anomaly,** shown as `bypass[<host>]` in `hekma ps` and counted by `hekma.bypass.detected` (OTLP v2, by `hekma.provider.host`, depth and allow-listed labels, the Run as exemplar). With custody off, or no wire path, it is informational: a note naming the host and why direct calls are expected. Detection never stops, pauses or fences a Run.
> - **Never clean without checking.** Every supervised Run carries its detection state — `active`, `unavailable` with the reason, or `unreported` — in `hekma run show` (`BYPASS`) and `--json` (`bypass: {detection, reason?, hosts, informational?}`).
> - **A signal, not a guarantee:** another proxy or VPN, HTTP/3, a connection shorter than the interval, a process outside the tree and shared CDN addresses are the documented limits.
> - **Events:** `bypass-detected` (once per host per Run, at most 8 hosts) and `bypass-detection-changed` Run events.
> - **Engine surface (breaking within the unreleased 0.12.0):** `Anomaly` gains `Bypass`; `RunEventKind` gains `BypassDetected` and `BypassDetectionChanged`; `RunSummary` and `RunView` gain `bypass` (`BypassView`). The Shim protocol moves to 1.1 (additive; a 1.0 Shim keeps working and reports nothing). `state.db` migrates one way to schema 28.

> **Unreleased — the Shim, part two: agents in their own Shim, a preview (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Off by default.** With `EngineOptions::shim_exe` set (and `hekma serve` with `HEKMA_SHIM=1`), each supervised agent is launched through its own Shim. The agent then outlives the engine, and the next engine reconnects to it. Unix only; ACP agents stay held directly.
> - **Dropping an engine does not stop a Shim-held agent;** only an explicit stop or `shutdown` does.
> - **Engine surface (breaking within the unreleased 0.12.0):**
>   - `EngineOptions` gains `shim_exe`;
>   - `SpawnRecord` gains `shim` (`ShimRecord`);
>   - `state.db` migrates one way to schema 27.

> **Unreleased — reconciliation: confirmed usage beside the estimate (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **`hekma run reconcile <run-id> --input N --output N [--cached N] [--dollars D] [--source provider|host] [--ref ID]`** and **`POST`/`GET /v0/runs/{id}/reconciliation`** (Operator only) submit a Run's confirmed totals, from the provider's invoice or your gateway.
> - **Stored beside the estimate, never over it.** The newest submission is the Run's `reconciled` figure. `hekma run show` and the Run document (`reconciled: {current, submissions, drift}`) label each figure and report the drift.
> - **Retries are safe:** the same reference and figures change nothing, and the same reference with other figures is `409 reconciliation-conflict` (exit `4`).
> - **Unchanged:** budgets keep acting on estimated figures.
> - **Events:** one `reconciled` Run event per accepted submission.
> - **Engine surface:**
>   - additive: `Engine::reconcile_run`, `Engine::run_reconciliations`, the `Reconcile*` types, and `RunView.reconciled`;
>   - breaking within the unreleased 0.12.0: `RunEventKind` gains `Reconciled` and `RunError` gains `ReconciliationConflict`.
>
>   `state.db` migrates one way to schema 26.

> **Unreleased — retention and compaction (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Ended Runs are compacted.** Past the Run retention window (seven days by default) an ended Run with no open descendant is reduced to its run id, usage totals, labels, end cause, Control Depth and timestamps; its other detail is deleted. Its usage still counts on every ancestor, and its usage records, events and outbox rows stay. `hekma run show` answers a compacted Run's summary (`--json`: `compacted_run`). The Service Host compacts when it starts and then daily, in bounded batches, with one diagnostic line per pass.
> - **Per-call records rotate daily** (`calls/<UTC date>.db`, a call in flight moving with the head so its record lands in one file) and a file past the calls window (seven days) is deleted whole, never one holding a call in flight. Opening a store now closes the calls a dead host left open in older files too.
> - **`hekma run retention [--runs N] [--calls N]`** and **`GET`/`PUT /v0/retention`** (Operator only; `400 invalid-retention` outside 1 to 3650 days). The Run window is also `hekma ps`'s ended-Run window, which used to be a fixed seven days.
> - **Writes no longer slow down as Runs accumulate.** A Service Host used to re-read every open Run after each of its own commits, about 1 s per write at 100,000 Runs. It now reloads only when another process commits. Compaction no longer walks every open Run for each candidate. A new benchmark (`perf-budgets --run-scale <N>`) checks the first `hekma ps` page and the cost of a write at 100,000 Runs on every change, and a 1,000,000-Run rung is recorded in `docs/testing.md`.
> - **Lock timing for diagnosis.** `Engine::set_lock_timing` and `Engine::lock_hold_stats` record how long each call site holds the engine's two locks. This is additive, and off by default.
> - **Publishing Run changes no longer copies every open Run.** The data path's Run snapshot keeps its maps as a shared base plus small shared change layers, folded periodically, so a commit that changes a few Runs costs the same at a hundred open Runs or a million. Nothing about what readers see changes.
> - **Engine surface (additive):** `Engine::retention`, `Engine::set_retention`, `Engine::compacted_run`, and `RetentionSettings`, `RetentionChange`, `RetentionView`, `CompactionState`, `CompactedRun`, `RetentionError` with the `RETENTION_*` bounds. `state.db` migrates one way to schema 25 (24: retention; 25: two indexes).

> **Unreleased — the Shim, part one (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Internal, nothing changes yet:** a hidden `hekma shim` command and the `hekma-engine::shim` module add the Shim, the small supervisor that will own each supervised process so it survives the engine. The supervisor does not use it yet; agents start, stop and adopt exactly as before. A `shim-host-key` file (owner-only) is created in the state directory only when a Shim is used.
> - **Engine surface (additive):** the public `shim` module (`run`, `connect`, `socket_path`, the `proto`, `secret`, `server` and `client` submodules) and `backends::shim_listen`/`shim_connect` (Unix; Windows returns unsupported until the named-pipe transport lands). No schema change.

> **Unreleased — `otlp-metrics v2`: telemetry grouped by depth, Policy and labels (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **v2 is the default exposition.** `hekma.run_id` leaves every data point; series group by `hekma.control_depth`, `hekma.policy` (on breaches: the Policy that supplied the breached limit, `default`, or `explicit`), `hekma.instance` (the 256 most recently observed) and the label keys you allow-list with `--otlp-label-keys` (none by default; each key capped at `--otlp-label-cardinality` distinct values, default 200, past which values fold into `__hekma_overflow__`). Per-Run detail rides exemplars: the 8 most recent distinct Runs, `hekma.run_id` only.
> - **Runs Hekma did not start export too** (wire and observed Runs).
> - **New families:** `hekma.run.anomalies`, `hekma.calls.unscented`, `hekma.telemetry.received`, `hekma.otlp.folded`.
> - **`https://` collectors** are supported, with `--otlp-ca-file` for a root of your own.
> - **Deprecation:** `otlp-metrics v1` stays selectable with `--otlp-exposition v1` for at least one minor release and is removed only in a major release. Dashboards grouping by `hekma.run_id` move to depth, allow-listed labels and exemplars; see the migration notes in `docs/telemetry.md`.
> - **Engine surface (additive):** `UsageUpdateEvent` gains `control_depth`, `policies` and `labels`; `BudgetBreachEvent` gains `control_depth`, `policy` and `labels` (absent on older events); new `Engine::export_counters`, `Engine::note_telemetry_points` and `ExportCounters`.

> **Unreleased — the metering proxy beyond loopback (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **A wider bind:** `--metering-listen` takes an `IP:PORT` beside a bare port (an IP literal; a host name is refused). Beyond loopback it requires TLS through the new `--proxy-tls-cert` and `--proxy-tls-key` (exit `2` without them, before anything opens); `--proxy-public-host` names the address clients use. It stays one listener: a connection from this machine keeps every loopback rule and may speak plain HTTP, so supervised agents are unaffected.
> - **Every call from another machine authenticates** before its body is read: the binding's proxy-access credential (`hk_pxy_`) in the API-key slot, scoped to its binding's prefix, swapped for the binding's custodied key and never forwarded; or a Run Key of a Run Hekma did not start. A supervised agent's Run Key from another machine is unknown. Proxy-access calls are unscented under Scent-Check.
> - **Two new `proxy-wire v1` rows:** `401 hekma_authentication_required` and `403 hekma_credential_out_of_scope`.
> - **Behaviour change:** the TLS certificate and key files, for the proxy and for the Control API's `--tls-cert`/`--tls-key` alike, must be readable only by their owner (mode `0600` on Unix); a looser file now refuses the start.
> - **Started notice:** the proxy's address beyond loopback, a note while no proxy-access credential is active, and on every bind a line on whether other users could take its port. `GET /v0/proxy` gains `bind`, `tls` and `public_url`; `control.json` gains `metering_proxy_public_url`; `hekma binding show` prints the address for other machines.
> - **Engine surface (breaking within the unreleased 0.12.0):** `MeteringProxyOptions` gains `bind`, `tls` (`ProxyTls`) and `public_host`; `ProxyInfo` gains `bind`, `tls` and `public_url`; `ProxyStartError::NonLoopback` becomes `NonLoopbackWithoutTls`; new `check_owner_only_file`, `restrict_to_owner`, `interactive_users_hint` and `Engine::proxy_access_active`. No schema change.

> **Unreleased — the identity-header switch and Hekma in front of a gateway (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **A stored switch:** `hekma proxy set identity-headers on|off` (and `PATCH /v0/proxy`, Operator bearer only) turns the outbound `x-hekma-run-id` and `x-hekma-labels` headers on or off for every forwarded call. It is stored in the state directory, so an auto-started host reads it, and it applies to the very next call with no restart. One `proxy_setting` event per change. `hekma proxy show` reads it without starting anything; `GET /v0/proxy` gains `identity_headers`, `stored_identity_headers` and `forced_by_flag`.
> - **Per binding:** `hekma binding set <name> --identity-headers on|off|inherit` (the binding's `identity_headers` field) wins over the stored setting; `inherit` follows it again.
> - **Per run:** `hekma serve --identity-headers` joins `--no-identity-headers`; either overrides the stored setting until that host stops, is never stored, and the started notice says when it disagrees. Passing both is a usage error.
> - **Unchanged:** the header names, what they carry (the Run's id and labels, never content or a credential), the default (on) and the strip rule for a client's own `x-hekma-*` headers.
> - **New page:** `docs/gateways.md`, Hekma in front of LiteLLM, Portkey or a similar gateway, with two example recipes.
> - **Engine surface (breaking within the unreleased 0.12.0):** `MeteringProxyOptions.identity_headers` is now `Option<bool>` (`None` follows the stored setting); `EngineEvent` gains `ProxySetting`; `BindingSpec`, `Binding`, `BindingPatch` and `BindingEvent` gain `identity_headers`; new `Engine::proxy_settings` and `Engine::set_proxy_identity_headers`.
> - `state.db` migrates one way to schema 23.

> **Unreleased — hold, release, reroute and reject at the metering proxy (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Four new verbs:** `hekma hold`, `hekma release`, `hekma reroute --via <binding>/<route>|--off` and `hekma reject`, on one Run or a Selector (`--dry-run`, `--yes`, `--json`), and `POST /v0/runs/{id}/hold|release|reject|reroute` with set forms on the Control API (Operator or Host bearer only). Each sets or clears only its own reason; a reason applies to the Run and every Run beneath it.
> - **What a call gets:** reject first (a breach latched under `block` or `stop` answers `402 hekma_budget_block` as before; an Operator's reject answers the new `402 hekma_run_reject`; safe methods pass), then hold (parked in arrival order, re-checked against the current state when released — a revoked key, an ended Run, `enforce` or a reject refuses it), then reroute (forwarded to the route's upstream; a route's own custodied key is used, and a held key never leaves its pinned origin), else pass.
> - **New `proxy-wire v1` rows:** `402 hekma_run_reject` (non-retryable), `503 hekma_hold_full` (retryable; each Run holds at most 32 calls, `MeteringProxyOptions.max_held_per_run`), `503 hekma_hold_interrupted` (retryable, a graceful stop).
> - **Breach Actions at wire depth:** a latched `pause` now holds a wire Run's calls and a latched `stop` refuses them, until the ceiling is raised or reset; the `depth-fallback` Anomaly remains only with no wire path. A child Run with a wire path accepts its own `pause`, `stop` and `block`.
> - **Lifecycle verbs on wire Runs:** `pause`, `resume`, `stop` and `drain` on a Run that owns no process hold, release, or reject and end its calls, with a note.
> - **Visible:** a held Run shows `held` and, on `hekma ps`, its parked count and the oldest wait; Run documents and listing rows carry `dispositions`, `held_calls`, `oldest_held_at` / `oldest_held_age_secs` and `last_hold_end`; per-call records carry the disposition and `hold_end`. A new Run event, `disposition-changed`.
> - **Governance vectors stay 1.4.0;** the two wire-depth `pause`/`stop` cases now execute (`policy-wire-pause-holds-at-wire-depth`, `policy-wire-stop-rejects-at-announced-depth`; 79 cases, new checksum). The verb × depth fixture marks 12 more cells delivered.
> - **One-way upgrade.** The first open migrates `state.db` to schema 22 (`run_dispositions`, and the held-call columns on `runs`).
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump): `Engine::set_disposition` and its `Blocking` twin, `DispositionVerb`, `DispositionOutcome`, `disposition_for`, `RunEventKind::DispositionChanged`, `MeteringProxyOptions.max_held_per_run`, and the new `RunView`/`RunSummary` fields.

> **Unreleased — inbound OTLP: `observed` Runs from their own telemetry (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **`POST /v1/metrics` on the Control API listener**, OTLP/HTTP+JSON only, under a binding's announce credential (every other class `403`; none, unknown or revoked `401`). Always on; inert until an announce credential is issued.
> - **Exactly three inputs are read:** the resource attribute `hekma.run.external_id`, `hekma.label.*` Caller labels (a key the binding sets is skipped), and cumulative `gen_ai.client.token_usage` sums by `gen_ai.token.type` (`input`, `output`, `cached`; the point's `gen_ai.request.model` prices it and is never stored). Nothing else is stored.
> - **The third announce channel.** The identity is claimed beneath the credential's binding (one identity, one Run across the header, the endpoint and telemetry), created at depth `observed` or renewed, with a Lease and **no Run Key, ever**. Telemetry never moves a Run's depth.
> - **Counted once.** The last cumulative value per Run and token type is stored; only the increase becomes a usage row (`metering_source: "telemetry-reported"`, always estimated), in the same transaction, so a retried batch records nothing twice. A lower value re-bases.
> - **Budgets at `observed`:** telemetry usage counts; `warn` executes; a Policy's `block`, `pause` or `stop` acts as `warn` with the `depth-fallback` Anomaly. A Run whose calls pass through the metering proxy keeps per-call metering: its telemetry is skipped and reported in `partialSuccess`, never summed.
> - **Answers:** `200` (with `partialSuccess` and fixed messages for skipped points), `400`, `409 too-many-children`, `413` over 4 MiB, `415` for protobuf or gRPC (a Collector translates), `503` with `x-should-retry: true`. Bounds: 8 resources and 64 token-usage points per batch, 16 labels.
> - **Governance vectors stay 1.4.0** and gain three `policy-observed-*` cases appended after every earlier case (79 in all; new checksum).
> - **One-way upgrade.** The first open migrates `state.db` to schema 21 (`telemetry_counters`).
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump): `Engine::ingest_telemetry` and its `Blocking` twin; `TelemetryReport`, `TelemetryPoint`, `TelemetryTokenType`, `TelemetryOutcome`.

> **Unreleased — announce and Leases: Runs for clients Hekma did not start (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Two announce channels, one identity.** A proxied call carrying `x-hekma-announce: <id>` claims `<id>` beneath its binding (labelled or not), or a child beneath a Run Key or Run route; `POST /v0/runs/announce` claims it beneath the presented announce credential's binding (`{"external_id", "labels"?}`). Both use the namespaces `x-hekma-run` hints use, so a Run discovered on the wire and announced later stays one Run (depth `wire → announced`, one `depth-changed` event). An announce wins over a hint on the same call.
> - **Issued once per Run.** The announce that creates a Run returns its Run Key and refresh credential once: in the `201` of the endpoint, or in the new `proxy-wire v1` response headers `x-hekma-run-key` and `x-hekma-refresh` on the announcing call (whatever status it answers, a fenced `402` included), with `cache-control: no-store`. Announcing an open identity again renews it and updates its Caller labels (`200`, no headers) and issues nothing, so a revoked key stays revoked. The endpoint's key can draw key custody; the header's never does.
> - **Leases and `silent`.** An announced Run holds a Lease (60 s), renewed by every attributed call, a refused one included, and by the new `POST /v0/runs/{id}/heartbeat` (`204`; `409 run-ended`). The Service Host that owns the metering proxy sweeps every 5 s: a lapsed Lease, or 15 minutes without traffic for a Run with no Lease, marks the Run `silent` (one `status-changed` event); 60 minutes silent ends it with cause `silent-window`, its keys revoked at once. Silence never stops anything, and its next call or heartbeat brings it back. A binding's Run and the unattributed Run now end this way too and are recreated by the next call. Supervised Runs are never swept.
> - **The authority matrix opens.** The announce credential (`hk_ann_`) announces beneath its binding and heartbeats or closes only the Runs it created; a Run Key heartbeats its own subtree. Every other class on the announce route is `403 forbidden`.
> - **A per-root cap.** A Run's whole subtree holds at most 1,000 open Runs that Callers opened or announced (`409 too-many-children`; the header channel stays on the parent instead). The Operator and Hosts are not capped.
> - **Scent-Check:** an announcing call is served under every mode, `enforce` included; under `shadow` the announced Run gets the `unscented` Anomaly.
> - **Visible.** `hekma run show` prints LEASE and LAST SEEN; Run documents gain `lease_expires_at` and `last_activity_at`.
> - **One-way upgrade.** The first open migrates `state.db` to schema 20 (`runs.last_activity_at`, seeded from `updated_at` for open Runs, `runs.created_by_hash`, and two partial indexes for the sweeper). No Run's status or depth changes at upgrade.
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump): `Engine::announce_credential`, `announce`, `heartbeat` and `close_run_by` with their `Blocking` twins; `AnnounceCredential`, `AnnounceCredentialState`, `Announced`, `LeaseActor`, `LeaseOptions` (on `MeteringProxyOptions.leases`) and its default constants; `RunError::TooManyCallerRuns`; `MAX_ROOT_OPEN_DESCENDANTS`; `RunView.lease_expires_at` and `last_activity_at`.

> **Unreleased — Scent-Check Modes act at the metering proxy (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **`shadow` and `enforce` are accepted.** `hekma policy create|change --scent-check <off|shadow|enforce>` and `POST/PATCH /v0/policies` store any mode; `--clear scent-check` removes it. **Breaking:** `scent-unavailable` is gone (`RunError::ScentUnavailable` and its exit and HTTP mappings are removed).
> - **The proxy acts on the mode.** A call no valid Run Key identified (a binding's Run or split, the unattributed Run, a Run route reached without its key) is **unscented**. Under the strictest mode over its Run's ancestor chain: `off` serves it; `shadow` serves it and raises the new `unscented` Anomaly on the Run (once, one `anomalies-changed` event; cleared when a re-resolution moves the Run off `shadow`); `enforce` refuses it with the new `proxy-wire v1` row `401 hekma_unscented_call`, nothing forwarded, the per-call record's disposition `enforce`. Unknown and revoked Run Keys keep their own `401`s under every mode; a latched `block` still refuses first, and `enforce` has no safe-method exemption.
> - **A Caller may tighten.** `PATCH /v0/runs/{id}/limits` takes `scent_check` from the Run's own Run Key, applied only where tighter (`403 raise-forbidden` for a lower value); the Operator sets modes on Policies (`400 invalid-request` on that route).
> - **The default is `off`, everywhere.** The default Policy stays `off`; nothing is written at upgrade. There is no per-environment default.
> - **Governance vectors stay 1.4.0** and gain three Scent-Check `policy` cases appended after every earlier case (76 in all; new checksum).
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump): `Anomaly::UnscentedCall`, `CallDisposition::Enforce`, `RunLimits.scent_check`; `RunError::ScentUnavailable` removed. No schema step.

> **Unreleased — key custody, a preview (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it. Key custody is a **preview, not yet supported**: it becomes supported when the pending security rulings are recorded and its test criteria pass in CI.
>
> - **Hekma holds the provider key; the agent gets a Run Key.** A new unified key, `provider.api_key`, takes a `secret:NAME` reference only. With it set, custody is on by default (`provider.custody = "off"` opts out). The agent's mapped API-key variable receives the launch's Run Key and its base URL the proxy route; the real key stays in the Service Host's memory and is never in the agent's environment, arguments or Agent Home. An adapter may map `provider.api_key` to an `env` target only.
> - **Start refusals, before any state change** (`EngineError::Custody`, exit 1): the same key delivered a second way (another leaf naming the secret, an `agent.<VAR>` pass-through or a launch-environment entry naming a listed provider variable), a key that does not resolve, and an origin the key cannot be pinned to. **Breaking:** an `engine-observed` agent whose provider key Hekma does not hold is now refused at start; set `provider.api_key` (with `provider.custody = "off"` for an `http` gateway), or switch the adapter to self-reported.
> - **The swap.** Only a Run Key issued to a supervised agent or a credentialed open draws the custodied key, presented in the API-key slot (`authorization`, `x-api-key`, `x-goog-api-key`, `api-key`) or in `x-hekma-run-key`. The proxy removes the client's credentials, inserts the held key once in the provider's header, sends it only over `https` to the origin pinned for the key (frozen on the Run for its life) and only to paths on the key's list (`400 hekma_path_not_allowed`), and replaces the key in responses with `hekma_redacted_key`. Pins and paths come from a `[custody.NAME]` table in the secrets file, else a built-in table of well-known providers. A changed secrets file is picked up within 30 seconds. An endpoint binding's custodied key is used the same way, and a binding whose key cannot be pinned to its `https` upstream is refused.
> - **The Host/Origin guard.** The metering proxy refuses, with `403 hekma_bad_host` and before any dial, a `Host` that is not loopback or listed with `hekma serve --proxy-allowed-hosts`, and any `Origin` not listed with `--proxy-allowed-origins`.
> - **The process environment is cleared for a custodied agent** (its declared variables and the OS basics only), and `provider.isolate_home = "on"` points its home-directory variables inside its Agent Home.
> - **Reveals are recorded.** `hekma agent config get --reveal` of a custodied key first records a `secret_revealed` event (the secret's name, never its value) through the Service Host, via `POST /v0/instances/{name}/config/revealed`, and prints nothing if that fails.
> - **On Linux, `hekma serve` marks itself non-dumpable** with a core-dump limit of 0.
> - **Custody is visible:** `hekma ps` shows it in the CONTROL column, `hekma run show` on a CUSTODY line, and Run documents a `custody` field (`on`, or `off` with `no-held-key`, `opted-out` or `pre-custody`).
> - **One-way upgrade.** The first open migrates `state.db` to schema 19 (the Run custody columns); a Run live at the upgrade shows `pre-custody` until its next launch.
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump): `EngineError::Custody`, `EngineEvent::SecretRevealed` with `SecretRevealedEvent`, `Engine::note_secret_revealed` and its `Blocking` twin, `harden_host_memory`, `CustodyView` with `RunSummary.custody` and `RunView.custody`, and `MeteringProxyOptions.allowed_hosts`/`allowed_origins`.

> **Unreleased — per-call records for Hosts: `hekma calls` and `/v0/calls` (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Read the metering proxy's per-call records.** `hekma calls list` pages them newest first, filtered by Run (never its children), Run Key token id, outcome, disposition, status class, model, Control Depth, binding and a start-time window, with `--problems` for every call that did not pass cleanly; `hekma calls show <call-id>` prints one. The Control API serves the same document on `GET /v0/calls` and one record on `GET /v0/calls/{call_id}`, Operator bearer only, with typed refusals (`invalid-cursor`, `invalid-limit`, `invalid-run`, `invalid-time`, `invalid-view`, `invalid-filter`, `invalid-call-id`, `call-not-found`, `calls-unavailable`).
> - **What a record carries:** the call and Run ids, the presenting key's token id, the Control Depth, the binding, the disposition and outcome, the status and its class, the model, the token counts (`null` when unknown, never `0`), and the timings. Never a prompt, response, header, key or upstream address; a credential-shaped model name reads back as `[redacted]`.
> - **Bounded reads:** pages of at most 1,000 records with a cursor that never skips or repeats; a scan budget of 100,000 records a page, after which a page says `scan_limited` and how far back it searched; a calls file from a newer build, or one that cannot be read, is skipped, counted and named. Reads take no engine lock and work from a read-only open.
> - **The calls store moves to its schema 2** (two additive indexes), applied in one step by the engine that owns the proxy. A Run listing cursor and a calls cursor are refused on each other's listing (`invalid-cursor`).
> - **The perf harness gains `--calls-seed <N>`**, gating the unscoped, Run-scoped and problems first pages and the by-id probe at p99 < 1 s; CI runs it at 100,000 records.
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump, additive): `Engine::list_calls`, `Engine::call` and their `Blocking` twins; `CallsQuery`, `CallView`, `CallListing`, `CallListingDocument`, `CallFilters`, `CallRecord`, `CallError` with `code()`, the forward-compatible `CallDisposition`, `CallOutcome` and `StatusClass`, the calls cursor codec and the shared constants.

> **Unreleased — one shared metering proxy, endpoint bindings and `proxy-wire v1` (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **One proxy per state directory replaces every per-agent metering listener.** `hekma serve` runs one loopback metering proxy on a **sticky** port: `--metering-listen <port>` if given, else the port recorded for the state directory, else an OS pick that is then recorded. The URL is in the started notice, `control.json`, `GET /v0/proxy` and `hekma binding show`. A taken port never hops: the host serves on, names the port, and refuses `engine-observed` starts. Loopback only; no flag widens it.
> - **The injected base URL now carries a path.** A supervised agent's `metering.base_url` is `<proxy url>/_hekma/runs/<run id>` (was `http://127.0.0.1:<port>`). Manifests do not change. A client that appends paths works unchanged; one that drops the path gets `404 hekma_no_route`, never a wrong attribution.
> - **Attribution per call:** a credential in the URL is refused; then a presented Run Key (`x-hekma-run-key`, or an `hk_` bearer), then the Run route, then the longest-matching endpoint binding, else the new **unattributed** Run. `x-hekma-run` hints name child Runs (beneath a Run Key or Run route) or split a labelled binding into per-id Runs; beneath the unattributed Run they are ignored. A Run Key in the API-key slot is refused `400 hekma_custody_unavailable` until key custody ships, so it is never forwarded upstream.
> - **Endpoint bindings:** `hekma binding create|list|show|set|delete`, `hekma binding route add|remove` and `hekma binding credential issue|revoke|list`, and `GET/POST /v0/bindings`, `GET/PATCH/DELETE /v0/bindings/{name}`, `PUT/DELETE /v0/bindings/{name}/routes/{route}`, `POST /v0/bindings/{name}/credentials` (shown once), `POST /v0/bindings/{name}/credentials/revoke` and `GET /v0/bindings/{name}/credentials` (Operator bearer only; each change is one `binding` event). Refusals carry `error.code` (`binding-invalid`, `binding-not-found`, `binding-exists`, `binding-prefix-taken`, `binding-limit`, `binding-route-not-found`, `binding-legacy-route`), and the CLI maps them onto exit 2 and 3. Per-binding announce (`hk_ann_`) and proxy-access (`hk_pxy_`) credentials are issued now; nothing accepts them yet.
> - **Outbound identity headers are on by default.** Every forwarded call carries `x-hekma-run-id` and `x-hekma-labels` (the Run's labels) to the model provider; `hekma serve --no-identity-headers` turns them off. Every `x-hekma-*` request header is stripped before forwarding, and a response carries `x-hekma-breach` while a breach is latched.
> - **`proxy-wire v1`, complete:** every response the proxy makes itself is typed JSON (`hekma_credential_in_url`, `hekma_custody_unavailable`, `hekma_bad_target`, `hekma_unknown_credential`, `hekma_credential_revoked`, `hekma_budget_block`, `hekma_run_out_of_scope`, `hekma_no_route`, `hekma_body_too_large`, `hekma_upstream_error`, `hekma_proxy_busy`, `hekma_ledger_unavailable`, `hekma_run_unavailable`) with `x-should-retry`; the static-text 502 is gone. Documented in docs/metering-proxy.md.
> - **Metered once per call.** Each call gets a call id, and its usage row is keyed by (Run, call id); a per-call record lands in the new calls store (`<state dir>/calls/`). A call that cannot be recorded is refused `503` before anything is forwarded. Calls in flight at a crash are closed at the next start as `recovered` or `crash`.
> - **`block` at every wire depth.** `block` now executes for any Run with a wire path, over the Run and every Run above it, and a latched `block` holds across a restart until a raise or reset re-arms it. `pause` and `stop` below supervised still fall back to `warn`.
> - **Upgrades.** An agent adopted from a pinned-port host keeps working through a legacy root route when it is the only one; `hekma ps` notes it. Adopted agents' base URLs survive restarts because the port is sticky.
> - **Governance vectors stay 1.4.0** and gain four `policy-wire-*` cases appended after every earlier case (73 in all; new checksum).
> - **One-way upgrade.** The first open migrates `state.db` to schema 18 (bindings, routes, binding credentials, `runs.upstream_base_url`, `usage_rollup_run` keyed by Run, `proxy_state`). An older engine refuses the migrated state dir.
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump): `MeteringProxyOptions`, `ProxyInfo` and the binding types (`Binding`, `BindingSpec`, `BindingPatch`, `RouteSpec`, `BindingCredential`, `BindingCredentialClass`, `BindingError` with `code()`); `EngineEvent` gains `Binding` (exhaustive public enum); `UsageEvent.instance` becomes optional and gains `call_id`; the OTLP v1 export skips instance-less usage.

> **Unreleased — Run Keys: each Run's own credential (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **A Run Key per launch.** Every supervised start, and every Restart Policy relaunch, issues a fresh Run Key (`hk_run_` and 52 Crockford base32 characters, 32 bytes from the OS random source — no fallback: a failing source refuses the start before anything spawns) and hands it to the agent in `HEKMA_RUN_KEY`. An Adapter may also map the reserved `run.key` to an `env` target; a `flag` or `file` mapping of `run.key` or `run.refresh_key` is refused at registration, writing either key is refused at every layer, a hand-set value is stripped at start, and neither ever reaches `effective-config.json`. A launch's key is recognised once the launch is recorded and revoked when its process is confirmed exited; adoption changes nothing. A Run live at upgrade runs on without a key until its next launch.
> - **Child Run credentials and the Caller.** `POST /v0/runs` returns the child's Run Key and a refresh credential (`hk_ref_`) once, in the `201` only (`issue_credentials: false` opts out). A Run Key may open child Runs beneath its own Run (caller labels; at most 1000 open children per parent), close them, and tighten — never raise — limits on its subtree; anything else is `403 run-out-of-scope` or `403 forbidden`. Every Run Key request re-checks the key inside its own write.
> - **Refresh with reuse detection.** `POST /v0/runs/refresh` (refresh credential only) trades the credential once for a new pair and revokes the old Run Key at once. A used refresh credential presented again revokes the Run's current pair, raises the new `credential-reuse` Anomaly and answers `401 credential-revoked`; `revoke-keys` clears it.
> - **Revocation.** `hekma run revoke-keys <run-id> [--no-cascade]` (and `POST /v0/runs/{id}/revoke-keys`) revokes a Run's keys and, by default, every key issued under them; the next request presenting one is refused, and a hard kill of the host cannot undo it. A Run that ends revokes its own keys. `hekma run keys <run-id>` (and `GET /v0/runs/{id}/keys`) lists token ids, classes and states, read-only. `hekma run open --print-key` prints a child's pair once; without it the CLI issues none.
> - **Stored as hashes, shown once.** `state.db` keeps the SHA-256 of each credential and a 12-hex token id label, never a value; no read route, event, SSE frame, export row, `--json` document or log carries one.
> - **Never in a URL.** A credential in a Control API path or query is refused `400 credential-in-url` before authentication and revoked. The metering proxy refuses one with a typed `hekma_credential_in_url` `400` (`proxy-wire v1`, `x-should-retry: false`, nothing forwarded) without revoking it, since anyone on loopback could have sent it, and strips `x-hekma-run-key` before forwarding.
> - **The authority matrix per route family.** The operator bearer is refused on `POST /v0/runs/refresh`; a supervised Run's key is refused on a non-loopback `--listen` listener; `401` now carries `code` `unauthorized` or `credential-revoked`, and `403` `forbidden`. New codes `run-out-of-scope` (403), `too-many-children` (409), `credential-unavailable` (503), `credential-in-url` (400). Unrecognised presentations produce at most one diagnostic a minute, counts only.
> - **Events.** A new `credential` event kind (`issued`, `rotated`, `revoked`, `reuse-detected`), one per token per change, with a token id and never a value; the SSE Selector filter matches it by its Run; the OTLP v1 export ignores it. `limits-changed` gains `limits_layer` (`run` or `caller`).
> - **Caller limits.** A Caller action tighter than the effective one applies even where a Policy action fell back for depth; a Caller undercutting an unpriced Policy cap keeps the `unpriced-model` Anomaly; a Caller may clear its own row when that loosens nothing, and may not replace `pause` or `stop` with the cooperative `block`; an Operator clear also lifts a Caller's row; `hekma budget` and `hekma label` treat a registered agent named like a legacy run id as the agent. Governance vectors stay 1.4.0 and gain six Caller-layer `policy` cases appended after every earlier case (69 in all; new checksum).
> - **One-way upgrade.** The first open migrates `state.db` to schema 17 (`tokens` and three indexes). An older engine refuses the migrated state dir.
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump): `EngineEvent` gains `Credential`, `Anomaly` gains `CredentialReuse`, `RunError` gains `RunOutOfScope`, `CredentialRevoked`, `CredentialUnknown`, `CredentialUnavailable` and `TooManyChildren`, `EngineError` gains `CredentialUnavailable` (exhaustive public enums); `ChildRunRequest` gains `issue_credentials` and `RunEvent` gains `limits_layer` (struct literals need them). Additive: `Engine::authenticate`, `credential_state`, `open_child_run_as`, `close_run_as`, `set_run_limits_as`, `refresh_credentials`, `revoke_run_keys`, `revoke_exposed`, `run_keys` (async; blocking twins but `authenticate`, which is synchronous), `RunActor`, `OpenedRun`, `IssuedCredentials`, `CredentialMatch`, `CredentialSummary`, `RevokeOutcome`, `CredentialEvent`, `TokenClass`, `TokenHash`, `parse_presented`, `scan_url` and the reserved-key constants.

> **Unreleased — Selectors on the lifecycle verbs and `hekma budget` (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Selectors on the lifecycle verbs.** `hekma agent stop|pause|resume|drain` and `hekma run close` take an agent (unchanged), a run id, or `--selector key=value[,…]`, with `--dry-run`, `--yes` and `--json`. On the Run that owns an agent's process the verb is the agent's own; a child Run's lifecycle verbs act on its model calls, which Hekma cannot hold or reject yet, so a child is refused per Run naming its depth (a held child's `resume` names the parent holding it) and the others still act. A set of more than one Run is listed and confirmed on a terminal only (a pipe is refused, exit 2, unless `--yes`; stdin is never read); `--dry-run` plans each Run and starts no Service Host. Every verb prints one shared results document (`{schema_version, verb, dry_run, matched, results}`); the exit code is the first failure's class with the count. `start` and `send` take no Selector.
> - **`hekma budget show|set`.** `show` reads a Run's limits — each with its source (`run`, `instance`, `policy:<name>`, `default`), usage, headroom, the Breach Action and the latches — for one Run, or, with `--selector` or no target, for the Runs `hekma ps --all` lists, paged with `--limit`/`--cursor`. `set` writes limits on the Run itself (`--tokens-per-run`, `--tokens-cumulative`, `--dollars-per-run`, `--dollars-cumulative`, `--breach-action`, `--clear`): they outrank the agent's `budget.*` configuration and every Policy, and **belong to the Run** — they end with it, and a new `start` opens a new Run without them; every result says so (`run_scoped: true` and a note). For limits every future Run should carry, use `budget.*` or a Policy. Cumulative ceilings go on an agent's own Run only (`scope-unavailable` on a child); a Breach Action the Run cannot execute is refused naming the Metering Source.
> - **A raised ceiling now lifts on a live Run, from every path.** A ceiling raised above the totals — by `hekma budget set`, an agent `budget.*`/`cost.rate.*` configuration write, or a Policy change — re-arms its breach and lifts the matching `block` fence reason at once, reason by reason (a fence holding another latched `block` reason stays up); one lowered to or below the totals breaches at once and applies the Breach Action. Before, only a Policy change did this; a raised configuration ceiling waited for the Run to end.
> - **Control API.** `POST /v0/runs/select` resolves a Selector into its Runs, each as its `GET /v0/runs` listing row, and a set token (256 random bits, hashed at rest, bound to the selecting credential, single-use, 10-minute lifetime, at most 64 pending, forgotten by a host restart), carried in the BODY of the set routes `POST /v0/runs/stop|pause|resume|drain|close`, `PATCH /v0/runs/labels` and `POST /v0/runs/limits`, which act on exactly that set and answer per Run. Per-Run routes: `POST /v0/runs/{id}/stop|pause|resume|drain` and `GET`/`PATCH /v0/runs/{id}/limits`. New codes: `set-token-not-found` (404), `set-token-expired`/`set-token-used` (409), `set-token-foreign` (403), `verb-unavailable` (409), `scope-unavailable` (400), `raise-forbidden` (403). `hekma label --selector` now acts through one set call; its `--json` document gains `verb: "label"`, and a `--dry-run`'s refusals now print on stdout as `Run <id> would refuse (<code>): <reason>`, as on every set-taking verb (they were `Run <id> not labeled: <reason>` on stderr). The set routes' results document names the verb as the CLI does (`budget` for `POST /v0/runs/limits`) and carries `dry_run: false`.
> - **Events.** A new `limits-changed` Run event carries the Run's own limits before and after (`prior_limits`, `limits`) and the actor as its cause; additive, the Run event schema stays 1. The OTLP v1 export ignores it.
> - **Governance vectors 1.3.0 → 1.4.0 (additive):** a twelfth rule tag `re-arm` and a new `rearm` case kind (10 cases) appended after the unchanged 1.3.0 set, for 63 in all; `provenance.generated_from` gains `governance::rearm`; new checksum.
> - **No `state.db` step**: Run limits use the existing `run_limits` table; set tokens live in host memory.
> - **Engine surface within the unreleased `hekma-engine` 0.12.0** (no version bump): `RunEventKind` gains `LimitsChanged`, `RunError` gains `ScopeUnavailable`, `RaiseForbidden` and `VerbUnavailable`, `Case` gains `Rearm`, `CoverTag` gains `ReArm`, `LimitSource` gains `Caller` (a Caller's limit, applied only where tighter) (exhaustive public enums), and `RunLimits` gains `token_cumulative` and `dollar_cumulative_micros`, `RunEvent` gains `limits`/`prior_limits` (struct literals need the new fields). Additive: `Engine::stop_run`, `pause_run`, `resume_run`, `drain_run`, `plan_run_verb`, `select_run_summaries`, `set_run_limits`, `run_limits_view` (async and blocking), `LimitActor`, `RunLimitChange`, `RunLimitField`, `RunLimitsOutcome`, `RunLimitsView`, `LimitsDirection`, `RunLifecycleVerb`, `RunVerbError`, and the pure `governance::rearm` / `re_armed_keys`. The `run-ended` refusal's message now reads "has ended, so it cannot change".

> **Unreleased — `hekma ps`: the Runs that need you, newest first (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **`hekma ps`** lists the Runs that need attention: a Run is listed when it carries an Anomaly (an ended Run only within seven days of its end) or a `held`, `silent` or `stale` status; healthy open Runs are counted in the footer, not listed, and `--all` lists every Run inside that window. Rows are newest first and the footer prints the exact next-page command (the same view, Selector and `--limit` plus `--cursor`), which pages without duplicates; `--selector key=value,…` filters by label (bound or caller alike) in either view, and a listing pages past the 1000-Run cap the label verb keeps. Each row carries the CONTROL depth, the Run Status, the Anomalies with each latched Breach Action (`breach[block]` — a fenced agent never reads as merely healthy), the labels, the subtree usage and the headroom against the Run's **resolved** limits — naming the Policy that set them, with the cumulative token headroom on process-owning rows and no dollar figure where no Rate prices one. `--limit` defaults to 50, serves at most 1000, and `0` means the maximum. A read-only read: no host is started, nothing is created, an empty listing exits `0` with a note; `2` is an invalid selector, cursor or limit, `1` a store failure. `hekma agent list` keeps its own projection unchanged beside it.
> - **Control API.** `GET /v0/runs?selector=&view=exceptions|all&limit=&cursor=` (Operator bearer) serves the same document `hekma ps --json` prints — every filter, Selector and page parameter of the CLI has its equivalent — with typed `400`s (`invalid-selector`, `invalid-cursor`, `invalid-limit`, `invalid-view`); with no `limit` it serves the same 50-row first page as the CLI.
> - **Events.** `/v0/events` frames now carry `id: <seq>` (the event's position in the global outbox), accept `?selector=key=value[,…]` (only events whose subject Run currently carries the labels; events naming no Run are delivered only without a Selector; a filter read that fails is surfaced, never silent), and resume from `Last-Event-ID`: the server subscribes first, replays the outbox rows past the id through the active filter in bounded batches, then streams live with no duplicate and no gap at the seam (a replay batch that can no longer be read sends a `replay-error` frame and closes the stream). A seq below the retained floor answers `409 cursor-below-retention`, one past the tail `409 cursor-past-tail` — typed, never clamped.
> - **One-way upgrade.** The first open migrates `state.db` to schema 16 (three partial listing indexes). An older engine refuses the migrated state dir.
> - **Bench.** The perf harness gains `--ps-seed <N>`: seed N Runs (a documented status mix) into the state store and measure the listing's read-only open and first page plus counts; the `ps_first_page_p99_lt_1s` gate (strict) binds only on `--ps-seed` runs, and the default report stays byte-equivalent.
> - **Breaking engine surface within the unreleased `hekma-engine` 0.12.0:** `RunError` gains `InvalidCursor` (exhaustive public enums). Additive: `Engine::list_runs` and `subscribe_sequenced` (async and blocking), the listing types (`ListRunsQuery`, `RunListing`, `RunListingDocument`, `RunSummary`, `RunBudgetView`, `ListingCounts`, `ListingView`, `LatchedBreach`), the cursor codec (`encode_cursor`, `decode_cursor`), `ended_run_cutoff`, `Engine::event_subject_labels`, and `format_rfc3339`/`now_unix_secs` re-exports.

> **Unreleased — Policies and the `warn` default (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **The default Breach Action is now `warn`.** A breach with no Breach Action configured is recorded, raises the Run's `breach` Anomaly and is published, and acts on nothing. `pause`, `stop` and `block` stay opt-in.
> - **Upgrades keep your pause.** The first open of an upgraded state directory writes `budget.breach_action = "pause"` into the configuration of every agent that relied on the old implicit default, records one `breach_default_upgraded` event, and shows a "kept on pause by upgrade" note until a Breach Action is set explicitly. An agent whose configuration cannot be written still pauses. A malformed `breach_action` fails closed to `pause` with a diagnostic.
> - **Policies.** `hekma policy list|show|create|change|delete` (and `/v0/policies` on the Control API, Operator bearer) set Token Budgets, Cost Caps, a flat Rate and a Breach Action for every Run a `--selector` matches by **bound** label. Each limit resolves on its own: a malformed `breach_action` fails closed to `pause` before every other source (a Run's own action included, and without dropping the ceilings, the Rate or the model the agent set), then explicit limits on the Run or the agent, then the most specific matching Selector (a tie goes to the tightest value), then the stored `default` Policy. `--dry-run` reports the matched Runs and how many fall back to `warn` at their Control Depth. A named Policy arms a child Run's own level when its bound labels match; every Run resolves at creation, at every depth change and at every label change, so nothing waits for the first usage event. The Fleet's budget view names the Policies that supplied its limits (`policies`). An unknown `--clear` field, and a flat Rate on the `default` Policy (its model-keyed table is the only Rate it applies), are refused — exit 2 on the CLI, `422` for an unknown body field at every level of a PATCH and `400` for an unknown `clear` name.
> - **The `default` Policy** is stored and editable: no ceiling, `warn`, Scent-Check Mode `off`, and a model-keyed Rate table that starts empty (`hekma policy change default --rate-for <model> --rate-input … --rate-output …`). A Policy Cost Cap on a model with no Rate raises the `unpriced-model` Anomaly instead of staying silently inert.
> - **Changes apply at once.** A Policy change re-resolves every matched open Run: a ceiling lowered to or below usage breaches now, a raised one re-arms the latch and lifts the matching `block` fence reason on a live Run. Cumulative ceilings compare against the agent's lifetime totals; clearing a ceiling or deleting the Policy re-arms too, and a Selector change re-resolves the Runs both the old and the new Selector match.
> - **Fallback.** A Policy Breach Action a Run's Control Depth cannot execute acts as `warn` and raises the `depth-fallback` Anomaly.
> - **Events.** The bus, SSE and export carry two new event kinds, `policy` (created, changed, deleted — the payload names which under `change`, as the Run event does — with the matched and fallback counts) and `breach_default_upgraded`. The OTLP v1 export ignores them.
> - **One-way upgrade.** The first open migrates `state.db` to schema 15. An older engine refuses the migrated state dir.
> - **Breaking engine surface within the unreleased `hekma-engine` 0.12.0:** `EngineEvent` gains `Policy` and `BreachDefaultUpgraded`, `Anomaly` gains `UnpricedModel` and `RunError` gains `PolicyNotFound` and `PolicyExists` (exhaustive public enums). Additive: `Engine::policies`, `policy`, `create_policy`, `change_policy`, `delete_policy` (async and blocking), `PolicyChange`, `PolicyOutcome`, `PolicyRecord`, `PolicyLimits`, `PolicyEvent`, `ScentCheckMode` and the resolution types.

> **Unreleased — bound labels: `hekma label` (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **`hekma label set|remove`** binds and unbinds labels on one Run (by run id), on an agent's open Run (by name), or on every open Run a `--selector key=value[,key=value…]` matches, bound or caller labels alike. Several matches are listed and confirmed (`--yes` skips the question; without a terminal the command is refused unless `--yes` is given, so scripts never hang), `--dry-run` shows each Run's planned change and writes nothing, and `--json` prints one result per Run. A refusal on one Run does not stop the others; the exit code is the first failure's (`3`, `4` or `2`). No match exits `0` with a note; more than 1000 matches is refused.
> - **Precedence.** A label set with the verb is bound with origin `label-verb` and, per key, outranks the agent's `labels.<key>` configuration (origin `config`) and any caller label; a later configuration write, a crash relaunch or an adoption keeps it. Removing it restores the configured value in the same change. Removing a configured label (`label-from-config`, naming the key to change) or a caller label (`label-not-bound`) is refused, as is any change to an ended Run (`run-ended`). Labels belong to the Run: a new `start` opens a Run without them.
> - **A Caller cannot set a bound key.** A caller label on a key already bound on the Run is refused (`label-bound`), and the store never turns a bound label into a caller label.
> - **Control API.** `GET /v0/runs/{id}/labels` and `PATCH /v0/runs/{id}/labels` (`{"set": {…}, "remove": […], "dry_run": bool}` → the labels after, the labels before, `changed`), Operator bearer only; refusals carry the codes above as `409`, an unknown agent target is `404 instance-not-found`, a malformed change is `400 invalid_body`, and an unknown body field is refused so a misspelled `dry_run` never writes.
> - **Events.** A `labels-changed` Run event gains `prior_labels` (the bound labels before) and a cause (`label-verb` or `config`); every bound label on every surface gains `origin`. Additive; the Run event schema stays 1.
> - **One-way upgrade.** The first open migrates `state.db` to schema 14, recording `config` as the origin of every bound label.
> - **Engine surface (within the unreleased 0.12.0):** `Engine::label_run`, `select_runs` and `instance_run` (async and blocking), `LabelChange`, `LabelOutcome`, `LabelOrigin`, `Selector`, `SelectorScope` and `MAX_SELECTED_RUNS`; `Label` gains `origin`, `RunEvent` gains `prior_labels`, and `RunError` gains `RunEnded`, `LabelFromConfig`, `LabelNotBound`, `LabelBound` and `TooManyRuns`.

> **Unreleased — Runs as first-class entities (hekma-engine 0.12.0).**
> Announced ahead of the release that ships it:
>
> - **Per-run budgets no longer reset when a crashed agent is relaunched.** A Restart Policy relaunch now continues the same Run: the run id is kept, per-run Token Budgets and Cost Caps keep counting from before the crash, a breach already recorded for the Run does not fire again, a `pause` or `stop` it already fired is applied again to the relaunched agent, and a per-run `block` fence is up again before the relaunched agent's first call. Only `start` opens a new Run. If you relied on a crash resetting a per-run budget, raise the ceiling or stop and start the agent.
> - **Every supervised start is a Run.** Starting an agent creates a Run with a ULID run id (`run_01JZ6Q9G3S8V2X7K4M1N5P0R2T`); ids recorded before (`run-…`) stay valid. A Run carries a Control Depth (`supervised`, or `supervised (no wire)` for a `self-reported` agent), a Run Status (`active`, `held`, `ended`) that follows the agent's lifecycle, and Anomalies: `failed` while a crashed agent awaits its relaunch (kept if the Restart Policy gives up) and `breach` once a Token Budget or Cost Cap breaches, whatever the Breach Action.
> - **Child Runs.** `hekma run open --parent <agent|run-id>`, `hekma run close` and `hekma run show` (and `POST /v0/runs`, `GET /v0/runs/{id}`, `POST /v0/runs/{id}/close` on the Control API, `Engine::open_child_run`, `close_run` and `run` for embedders) let a Host open a child Run for one unit of its own work. A child's usage counts on every ancestor and every ancestor's ceilings apply; its own per-run ceilings take `warn` (`pause`, `stop` and `block` are refused for a child Run for now). Pausing or stopping the agent holds or ends its child Runs, each recording the cause. Runs nest at most 8 levels. Run refusals carry a stable `error.code` on the Control API and documented exit codes on the CLI.
> - **Labels from configuration.** `hekma agent config set <agent> labels.<key> <value>` binds a label on the agent's Runs (merged per key across layers, applied at once to a running agent, never delivered to the agent).
> - **Run events.** The event bus, `/v0/events` and the export carry a new `run` event kind (created, depth changed, status changed, Anomalies changed, labels changed, ended), published right after the event that caused it — a breach is followed by the Run's `breach` Anomaly, a transition by its Run's status change. OTLP metrics are unchanged.
> - **Governance vectors 1.1.0 → 1.2.0 (additive):** a tenth rule tag `ancestor-ceilings` and a new `chain` case kind (5 cases) appended after the unchanged 1.1.0 set, for 39 in all; `provenance.generated_from` gains `governance::decide_chain`; new checksum.
> - **One-way upgrade.** The first open migrates `state.db` to schema 13: the live Run of each running agent keeps its id, every other recorded run id becomes an ended Run, and breach latches now reference their Run. An older engine refuses the migrated state dir.
> - **Breaking engine surface — `hekma-engine` 0.11.0 → 0.12.0:** `EngineEvent` gains `Run`, the governance `Case` gains `Chain` and `CoverTag` gains `AncestorCeilings` (exhaustive public enums), and `RunId::mint` returns the ULID shape. Additive: the Run model (`RunRecord`, `RunView`, `RunEvent`, `RunError`, `ControlDepth`, `RunStatus`, `Anomaly`, `Label`, `ChildRunRequest`, the verb × Control Depth table `verb_cell`), `decide_chain`, `ChainCase` and `parse_dollars_to_micros`.

> **Unreleased — one data-path owner: the CLI routes through a local Service Host.**
> Announced ahead of the release that ships it:
>
> - **Every `hekma agent` write goes through the Service Host** that owns the state directory: `register`, `remove`, `start`, `stop`, `pause`, `resume`, `drain`, `send`, `config set` and `memory attach|detach` are each one Control API call. When no host serves the directory, the first write starts one in the background (`hekma serve --autostart`, output in `<state-dir>/serve.log`) and waits up to 10 s for it; concurrent commands share it. Reads (`list`, `show`, `usage`, `logs`, `config get`) open `state.db` read-only and never start a host — on a fresh machine `hekma agent list` creates nothing.
> - **Agents outlive the terminal.** A plain `hekma agent start` keeps the agent running — supervised, metered and within its budget — after the command and its terminal exit; the stderr note names the host's pid and `hekma agent stop`. `send` now reaches an agent a separate `start` command started. `--detach` still disarms the handle, so the agent also survives the host's exit; its note is reworded to match. An `engine-observed` detached start is still refused (exit 5).
> - **The background host leaves on its own:** it ignores SIGHUP, and drains and exits once no agent is live and no command reached it for 30 s, or when its state directory is deleted. It passes the starting command's environment to its agents, as the CLI's own engine did (restart it to pick up a changed environment); an explicit `hekma serve` keeps the child-environment allowlist and never idle-exits.
> - **Reads never write, and say so when the record is behind.** When `list` or `show` finds a `running` or `paused` instance whose process is gone (its host and the agent were killed, or the machine went down), it shows `failed`, with the cause naming the pid, and keeps the stored state in the additive `recorded_state` field, with a stderr note; the next write's host records the change.
> - **Same messages, same exit codes.** The Control API carries the engine's typed error (the additive `error.engine_error` object), which the CLI renders exactly as before; the engine's notes raised during a write ride back to the command's stderr. A host that cannot be found or started is exit 1, naming `serve.log`. `stop --timeout` is served within `1..=86400` s (`0` becomes a 1 s window, with a note).
> - **Every serve binds a loopback local endpoint** (127.0.0.1, an OS-chosen port, a fresh 256-bit token) recorded in an owner-only `<state-dir>/control.json`, removed at shutdown, with a new `local control endpoint on …` notice. `--listen` adds the operator's listener beside it.
> - **Breaking: a non-loopback `--listen` requires TLS.** `hekma serve --listen 0.0.0.0:8477 …` without `--tls-cert` and `--tls-key` (PEM) is refused with exit 2 before anything binds or opens (it used to serve plain HTTP with a warning). With both, the listener answers HTTPS only (the vendored rustls + ring stack). The `hekma-host` binary takes the same flags; the container recipe in the hosting docs is updated.
> - **Auth goes through one credential resolver:** SHA-256 hashes with a class and a class × surface authority matrix replace the single bearer compare. The local token and the `--bearer-secret` value are both operator bearers and work on both listeners; the reserved `403 forbidden` answers a credential class a surface does not allow.
> - **New Control API routes:** `POST /v0/instances/{name}/send`, `PUT`/`DELETE /v0/instances/{name}/memory`, `PUT /v0/instances/{name}/restart-policy`; `DELETE /v0/instances/{name}` takes `?disposition=retain|delete&force=true|false`; `POST …/start` takes an optional `{"detach": true}`.
> - **`hekma-engine` (additive):** `Engine::open_read_only` (a `query_only` connection, a closed writer, no adoption, no reaper; a missing `state.db` reads as an empty fleet), `Registry::open_read_only`, `Registry::resolve_state_paths`, and `StoreError::NeedsMigration` (a read-only open of an older schema). `StoreError` is an exhaustive public enum, so an exhaustive downstream `match` needs an arm.

> **Unreleased — one store writer and Run-scale storage (hekma-engine 0.11.0).**
> Announced ahead of the release that ships it:
>
> - **One writer per engine process.** Every `state.db` write now runs on one store writer thread with its own connection; the registry's connection is read-only. A write waits at most 1 s to be taken, then fails with the typed `Busy` error (`waited_ms: 1000`, was up to 5000) and is not applied, so a retry is safe; metering retries it without ever skipping usage. Two engine processes on one state dir still serialize through SQLite's write lock.
> - **One transaction per event.** A usage event, its per-instance and per-Run rollups, the budget evaluation, every breach it fires and their outbox entries commit together; a transition commits its state, event row and outbox entry together. Usage totals and the budget evaluation read the rollups instead of re-summing the ledger.
> - **Event order on a breach changes:** the bus and the export now show usage, then every breach it fired, then the `pause`/`stop` transition (was usage, token breach, pause, dollar breach). Bus order still equals commit order.
> - **Breaches re-fire once when the Breach Action changes mid-Run.** The latch that stops a breach repeating is durable and keyed by action, so changing `budget.breach_action` after a breach fires it once more under the new action — live, as adoption already did. Switching to `block` after a breach now records a `block` breach, so the fence survives an engine restart.
> - **The records move into `state.db`.** Transitions, breaches and the export outbox are tables; `instance.log` and `breaches.log` are still written after each commit as mirrors. `resync_events`, `transition_events`, `budget_breach_events` and the export read the database; `torn_tail_skipped` and `blank_lines_skipped` stay on the wire and are always `false`/`0`. An outbox write failure now fails the event's commit (it used to be best-effort). Removing an instance removes its event rows.
> - **Export seqs are global.** One sequence in the database orders every engine process sharing the state dir (was one counter per process).
> - **Adopted observed instances no longer lose usage.** Usage rows carry a launch generation bumped at every spawn, restart and adoption, so the metering listener's sequence restarting after an adoption is no longer dropped as a replay.
> - **One-way upgrade — stop older engines first.** The first open migrates `state.db` to schema 12 and imports `instance.log`, `breaches.log`, `outbox.jsonl` and `outbox-ack.json` once (seqs and the ack carry over; the outbox files are renamed `*.migrated`; unreadable log lines are named on the engine log). An older engine refuses the upgraded state dir as too new when it opens it, but one already running (a `hekma serve`) does not notice: stop it before upgrading. Usage it still records is counted, but its transitions, breaches and export entries after the upgrade are not.
> - **`hekma-engine` 0.10.0 → 0.11.0** for the schema step and the changed event order and outbox contract. No public types change.

> **Unreleased — `hekma agent show`, `usage <name>` and `config get` tables no longer clip a header, or a value the table has room for.**
> Announced ahead of the release that ships it:
>
> - The human tables capped their columns in code, not from the terminal: `hekma agent show` printed its Capabilities header as `Support (curren…` and capped every Runtime-status value at 48 columns however wide the terminal was — a Usage row carrying both scopes lost its current-run figures (`this run: in …`), and a tokens-plus-dollars Budget row lost its inline estimate label to `(estimat…`. A column's cap is now never below its header's width, so a header renders whole wherever the table fits, and the Value columns of `show`, `usage <name>` and `config get` grow with the terminal up to their longest cell, taking only the width the table's other columns leave (a long `config get` value never squeezes the Validated and Source columns). The width comes from `COLUMNS`, else the terminal's width, else 80 columns when stdout is not a terminal (79 on Windows), clamped to 40–132 columns. A row the table has no room for is still cut at the width with `…` — on 80 columns that can fall inside a dollar label — while `--json` carries every figure whole. `hekma agent list` keeps its compact 80-column design; `--json` output and exit codes are unchanged.

> **Unreleased — the wire fence: a `block` Breach Action (hekma-engine 0.10.0).**
> Announced ahead of the release that ships it:
>
> - **`budget.breach_action = block`** (never a default): on a budget or Cost Cap breach the engine-observed metering listener refuses every further model call. The agent process keeps running (no lifecycle transition); the breach event records `"action": "block"` and the engine log notes the fence. `GET`/`HEAD`/`OPTIONS` requests still pass.
> - **The refusal is the first slice of `proxy-wire v1`:** HTTP `402`, `content-type: application/json`, `x-should-retry: false`, no `retry-after`, and a body that parses as an OpenAI and an Anthropic error — `{"type":"error","error":{"type":"hekma_budget_block","code":"<tokens|dollars>_<per_run|cumulative>","message":…,"param":null,"dimension":…,"scope":…,"proxy_wire":"v1"}}`. No limit, total, instance name or request/response content. Later releases keep it unchanged.
> - **When it lifts:** with the Run. A new Run that starts at or over a `block` ceiling is fenced before its process spawns; an engine restart or adoption on the same `--metering-listen` port rebuilds the fence from the breach log. A Restart Policy relaunch starts a new Run with a fresh per-run budget (the engine log says so when the replaced Run was fenced). Un-fencing a live Run on a raised ceiling is planned for a later release.
> - **Refused where it cannot work:** `config set … budget.breach_action block` fails on an instance without the listener (a `self-reported` source), naming the source. If `block` reaches such an instance another way, the breach records, acts as `warn`, and the engine log says so.
> - **Cooperative:** the fence refuses calls routed through the listener; an agent calling its provider directly is not fenced.
> - **Governance vectors 1.0.0 → 1.1.0 (additive):** a ninth rule tag `block-fence`, three `block` decision cases and a new `fence` case kind (5 cases) appended after the unchanged 1.0.0 set (26 cases), for 34 in all; `provenance.generated_from` gains `governance::fence_refusal`; new checksum.
> - **Breaking engine surface — `hekma-engine` 0.9.0 → 0.10.0:** `BreachAction` (an exhaustive public enum) gains `Block`, so an exhaustive downstream `match` needs an arm. Additive alongside it: `fence_refusal`, `fence_code`, `FenceRefusal`, `FenceCase`, `Case::Fence`, `CoverTag::BlockFence`, and the `FENCE_*`/`PROXY_WIRE_VERSION` constants. The OTLP `hekma.budget.action` attribute gains the `block` value.

> **Unreleased — Hekma's identity: the control plane for agent workloads (docs + metadata).**
> Announced ahead of the release that ships it:
>
> - The README, the docs index, the `hekma --help` line, the `hekma` crate description and the Homebrew formula description now introduce Hekma as the control plane for agent workloads: one way to run, see and steer any agent. Budgets become one control among several rather than the headline.
> - Four docs pages describe what is being built, each tagged with its stage: [Runs](docs/runs.md) and [Policies and the warn default](docs/policies.md) (*in development*), [Run Keys](docs/run-keys.md) and [Attach and wire control](docs/attach.md) (*planned*). Nothing on them ships in this change; names, commands and defaults may change before they do.
> - No behaviour changes: the default breach action is still `pause`. Its coming change to `warn` will be announced in its own release notes, and agents that rely on today's default keep `pause`.

> **Unreleased — the agents-in-CI recipe (docs + dogfood lane).**
> Announced ahead of the release that ships it:
>
> - A recipe page (docs/agents-in-ci.md) ships the whole copy-paste path: a tag-pinned checksum-verified binary download, agent registration with a token budget and Breach Action, two-clock bounding (`timeout-minutes` + Hekma's bounded commands), a `$GITHUB_STEP_SUMMARY` renderer over `--json` (micro-dollars with their `estimate_label`, honest `—` where no rate or cached subset exists), exit-code branching with the no-breach-code honesty (a breach is proven from the Fleet row, spend from usage totals), and `secret:NAME` wiring for auth-gated agents. A new `agents-in-ci` CI job dogfoods the recipe on every PR with two deterministic legs against the bundled fake agent — a success leg that asserts the committed batch, the budget block, and the exact derived dollars (1650 µ$ estimated), and a breach leg that proves the 90-token pause from the public surface and asserts the honest absence of dollar fields when no rate is configured.

> **Unreleased — the adapter gallery (docs surface).**
> Announced ahead of the release that ships it:
>
> - A curated, CI-validated index of the agents that run under Hekma ships under `docs/adapters/`: ten entries — Copilot CLI, Hermes, opencode, Claude Code, Codex, Gemini CLI, goose, Crush, Aider, Cursor CLI — each with a per-capability honesty table in the Capability Declaration vocabulary, an explicit evidence class (`run-verified` / `contract-validated` / `characterized`), and the re-validation duty. The Aider entry ships the gallery's first `adapter.toml`, validated on every push by the conformance kit against a PATH-shadowed bundled fake (CI never launches real agents); drift guards fail the build when a manifest, its page, or any of the three nav surfaces diverge. The README and the docs site link the gallery; the deployed-site port is live (the docs probe pins the gallery URL).

> **Unreleased — opt-in OTLP telemetry export (the `otlp-metrics v1` exposition).**
> Announced ahead of the release that ships it:
>
> - `hekma serve` (and the `hekma-host` container binary) gains three flags — `--otlp-endpoint <url>`, `--otlp-headers <k=v,...>`, `--otlp-flush-interval <secs>` (default 10) — that turn on a host-owned metrics exporter: it subscribes read-only to the engine's committed-event stream and POSTs OTLP/HTTP+JSON metric batches to `<endpoint>/v1/metrics` on a bounded cadence. The privacy default is structural: with no endpoint configured the exporter does not exist — no task, no dial, nothing leaves the machine (test-proven). The exposition is versioned and documented in docs/telemetry.md: `gen_ai.client.token_usage` (histogram, `gen_ai.token.type` input/output/cached, fixed documented bounds; `gen_ai.client.operation_duration` is absent — no producer, never fabricated), `hekma.budget.breaches` (sum, dimension/scope/action attributes), `hekma.usage.cost` (cumulative USD sum from integer micro-dollars, MANDATORY `estimate_state` attribute — a reconciled figure is a new series, never a replacement; no code path exports an unlabeled dollar), `hekma.agent.state` (gauge by state), and `hekma.otlp.dropped` (the always-emitted self-metric for bus lag, queue overflow, and dropped batches — every loss is also a `[hekma] ` stderr diagnostic). Wire rules follow the opentelemetry-proto JSON mapping (camelCase, decimal-string 64-bit ints, numeric `aggregationTemporality`); resource attributes carry `service.name=hekma-host`, the version, and a state-dir-derived instance id. Delivery degrades visibly and never blocks supervision: bounded POST timeouts, bounded retries for 429/503/504 and timeouts only, drop-oldest under a collector outage, and a bounded final flush at shutdown whose undelivered in-flight points are named (cumulative sums reset at restart — the documented restart-reset; the outbox remains the only durable export surface). v1 is **http-only** — `https://` is a documented deferral (bridge with a local OTel Collector); header values may be `secret:NAME` references resolved at startup (unresolvable = a typed startup refusal; values live behind a redacting type and never enter a log). Two honest v1 limitations: `hekma.usage.cost` data points come only from cumulative-scope dollar breaches (an instance that never breaches exports no cost series; per-usage-event cost needs a named engine payload extension), and the exporter observes only its own lifetime (adoption-time events before exporter start and the shutdown-drain window are not exported — by design; the ledger stays lossless). **No engine API changes, no new event kinds, no new dependencies** — the host's dependency tree is unchanged; the exporter is unpublished-crate surface (`hekma-host`, `publish = false`).

> **Unreleased — the service-mode gateway reference manifest and the four TCK sections (conformance kit + docs).**
> Announced ahead of the release that ships it:
>
> - **The reference gateway manifest** (`hekma-conformance`, path-only crate — no crates.io bump): `gateway_manifest_text`/`write_gateway_manifest`/`free_loopback_port` plus the `GATEWAY_*` constants compose THE one manifest that declares all five epic-18 service-mode declarations — `[readiness]` http-get on the models endpoint, `[liveness.heartbeat_file]`, `[exit_codes] restart_requested = [75]`, the `[lifecycle.pause]`/`[lifecycle.resume]` sentinel pair, and `[drain]` (`timeout_ms` + `idle_file`) — alongside the two delivery pairs (`[config.model]`, the reserved `memory.dir`) and the `--dump` seam the config/memory proofs read, all carried by ONE composed start command (each fixture flag arms its own thread; composition over construction). `docs/manifest.md`'s Complete Example shows the same shape — the kit's reference IS the docs' example.
> - **Four new TCK sections — `drain`, `readiness`, `liveness`, `exit_classes`**: the conformance report grows from eight to twelve sections in a FIXED pipeline order (drain deliberately BEFORE pause: the drain's pause command is what first writes the subject's pause sentinel, and an idle signal anchored on its first appearance cannot predate the drain). Each section consults the subject's DECLARATION: declared → the behavior is proven end-to-end (every failing leg runs on a TWIN with its own declaring manifest — a subject is never deliberately broken or exited); undeclared → `not_applicable` naming the absence; a declaration that cannot be re-read → the section FAILS naming the cause (never a fabricated "declares nothing"). Native builtins declare none of the manifest service sections and read the honest absences.
> - **The pause section's dispatch awareness**: a declared pause/resume COMMAND pair is DEMONSTRATED (the `pause-cooperative`/`resume-cooperative` causes naming the rendered commands) on every OS and capability level — `unsupported` included — mirroring the engine's cooperative-first dispatch; a Guaranteed level still proves the real-suspension seam on the TCK's own signal-path probe.
> - **The hermes successor has landed** (the AD-19 wait is satisfied): the deprecation surfaces ([adapter contract](docs/adapter-contract.md#deprecation-policy), `docs/commands.md`, `docs/agents.md`) now record that a gateway-shaped agent registers as a manifest adapter declaring the five service-mode sections — the shape the kit proves end-to-end. The `hermes` kind keeps working unchanged; removal stays at a future MAJOR per the ratified policy. No hermes code changed.
> - **No version bumps, no engine behavior changes**: the Adapter Contract stays **1.1.0**, `hekma-adapter-api` stays **0.3.0**, `hekma-engine` stays **0.9.0**, and the report's `schema_version` stays `1` — the growth is additive surface on the conformance kit alone. Consumers pinning the report's section list need the new count (12) and order.

> **Unreleased — cooperative pause commands and the drain verb.**
> Announced ahead of the release that ships it:
>
> - **The optional `[lifecycle.pause]`/`[lifecycle.resume]` command pair and the `[drain]` section** (Adapter Contract stays **1.1.0**, additive same-major): a manifest may declare pause/resume as op templates the ENGINE runs — the agent suspends and resumes itself under its own program (exit 0 within the engine's 30 s standalone command cap), OS-independent by construction, and working even where the capability table says `unsupported`. The templates form a PAIR: exactly one of the two is refused at registration with both fields named (a cooperative pause the adapter cannot reverse would strand the instance paused forever). The optional `[drain]` section declares the drain verb's bounded wait: `timeout_ms` (`1..=600000`, required) and an optional Agent-Home-relative `idle_file` the engine polls for freshness (absent ≡ stale) — the stop-new-work → bounded-wait → stop composite. A contract-1.0.0 engine rejects `[drain]` at parse time, naming it (the older-same-major-engine rule, for free). Registration validation also refuses control characters in the pair's `exec`/`args` (they can only ever render into recorded causes — never executed blind) and a `Component::Prefix`-shaped `idle_file`.
> - **The cooperative-first dispatch**: when the command pair is declared, `pause`/`resume` RUN THE COMMANDS instead of dispatching the OS signal path, and the stop of a cooperatively-paused instance SKIPS the OS resume signal (there is no suspension to undo) with a visible note saying so. A command that exits nonzero or cannot spawn REJECTS the verb with the durable state untouched (`PauseCommandFailed` — never a fake pause; the command's captured stderr tail rides the failure detail), and a command stuck past the 30 s standalone cap is killed and rejected the same way — the kill's reap is BOUNDED, so an unkillable corpse is named honestly rather than hanging the verb. The RESUME direction is PAUSE-KIND-GATED: only a COOPERATIVE pause runs the declared resume command — a `resume` of a signal-frozen (operator- or breach-paused) instance stays on the SIGCONT path even with the pair declared (a command edge there would record `running` over a still-frozen process). The transitions record the NEW `pause-cooperative`/`resume-cooperative` causes naming the rendered command (the environment is never rendered — no-leak). One deliberate exception: a budget-breach pause keeps the REAL signal suspension (enforcement needs a process the engine actually froze), and manifests WITHOUT the pair behave byte-identically to before (pinned by test, signal path intact).
> - **The new `drain` verb** (`hekma agent drain <name> [--timeout-ms <ms>]`): runs the declared pause command, waits — BOUNDED — for the declared idle signal, then stops through the ordinary ladder. The bound is three-tier (`--timeout-ms` > `[drain].timeout_ms` > the 30 s engine default) and governs the pause command too. An OVERRUN inside a drain is the timed-out path, not a rejection: the killed command commits NO `pause-cooperative` edge (the row stays `running` — the bound, not the adapter, ended it) and the drain proceeds to its unconditional stop with the honest `completed: false` — unlike the standalone `pause`, which rejects on the same overrun. The wait watches the AGENT too: a child that ends on its own mid-wait (a crash or a declared exit) breaks the wait early with a diagnostic, and the outcome is honestly not-completed — never a fabricated completion. BOTH outcomes are command successes (exit 0): a drain that outlives its bound STILL stops the instance — the last step is unconditional — and reports the honest `completed: false` on the terminal `drain-stopped` cause (with the measured `waited_ms`), noted on stderr rather than errored. A manifest with neither commands nor a `[drain]` section has no declared wait: the drain stops directly, reports a zero wait, and says so with a diagnostic. Refusals: the instance must be `running` (`DrainRefused`, exit 4 for a non-running row), and a `--timeout-ms` outside `1..=600000` is a usage error (exit 2).
> - **New additive surfaces**: `hekma-adapter-api` rides **0.3.0** (`CONTRACT_VERSION` stays 1.1.0 — the `Drain`/`DrainSpec` types join the exit-code types this release; the pair rule lives in registration validation); `hekma-engine` unchanged at 0.9.0, BUT the public `TransitionCause` enum gains the `PauseCooperative { command }`, `ResumeCooperative { command }`, and `DrainStopped { completed, waited_ms }` arms — **hosts matching `TransitionCause` exhaustively need the new arms** — the public `EngineError` enum gains `PauseCommandFailed { name, direction, command, detail }`, `DrainRefused { name, state }`, and `DrainBoundInvalid { bound_ms }` — **hosts matching `EngineError` exhaustively need the new arms** — and `EngineOptions` gains one `#[doc(hidden)]` field (`pause_command_cap_override`, the command cap's test-scaling seam) — **hosts constructing `EngineOptions` exhaustively (without `..Default::default()`) need that field**. New public API: `Engine::drain`/`Blocking::drain` returning `DrainReport { instance, outcome }` with the `DrainOutcome { completed, waited_ms }` record. **Reader break, beyond the compile-time arms**: the `pause-cooperative`, `resume-cooperative`, and `drain-stopped` SERDE tags are new on the wire — an older binary deserializing a newer engine's event stream or `--json` output containing such a transition fails with an unknown-variant error. The Control API gains `POST /v0/instances/{name}/drain` (optional `{"timeout_ms": N}` body, `1..=600000`; both outcomes are `200` — the `DrainDocument` reports `completed: false` honestly rather than as an error), and a `PauseCommandFailed` maps to **409 Conflict** there and on the lifecycle routes (the adapter refused the verb and the state is untouched — a retryable conflict, never a 500), joining `DrainRefused` in the conflict class. No new lifecycle states, no new exit codes.

> **Unreleased — declared exit-code classes and the stable-window reset.**
> Announced ahead of the release that ships it:
>
> - **The optional `[exit_codes]` manifest section** (Adapter Contract stays **1.1.0** — a third additive section in the same minor as `[readiness]`/`[liveness]`): two optional lists giving the agent's OWN exit codes declared meaning — `restart_requested = [75]` (the conventional "please relaunch me" ask) and `clean = [0]` (finished on purpose). With no section, every unrequested exit remains a crash — byte-identical **for a young run** (pinned by test); the stable-window reset below is engine-owned and UNCONDITIONAL, so a no-section run that crashed after ≥ 30 s of stable running resets the counter exactly as a declaring one does. Validation enforces the portable exit-code domain (`0..=255` — unix exit status carries the low 8 bits, so a wider Windows code above 255 could never be observed there; declare the portable form), list disjointness (a code in BOTH lists is refused at registration naming both fields), and no duplicates within one list (the resolver's own validation check — `deny_unknown_fields` protects the section's field names throughout). A contract-1.0.0 engine rejects the section at parse time, naming it (the older-same-major-engine rule, for free).
> - **The two classes' semantics**: a declared `restart_requested` code takes the ordinary crash-path dance — `running → failed` under the NEW `restart-requested` cause naming the declared class, a restart plan on the standard backoff — but the consecutive-failure counter is **NOT charged**: the agent's own recycle proves neither stability nor failure. A restart-requested loop therefore NEVER trips the crash-loop terminal (the operator declared the code a relaunch ask — punishing it would punish exactly the blessed behavior) while the backoff still grows with the recycle streak to its 60 s cap, so the RATE is bounded even though the terminal is unreachable. Under `Restart Policy: never` the operator's policy outranks the adapter's ask: terminal `failed`, no restart, cause intact. A declared `clean` code lands `stopped` under the NEW `clean-exit` cause — settled exactly like a requested stop, no restart, no counter action; the operator's next fresh start is the counter reset, as it has always been. The classes classify RUNS, not launches: an exit during `starting` (survival window, readiness probe, ACP handshake) is a launch failure and is never classified, and a signal exit carries no code to classify.
> - **The stable-window reset (engine-owned, not adapter surface)**: the engine tracks how long a run has held `running` continuously; a crash after ≥ 30 s of stable running resets the consecutive-failure counter FIRST, so the crash charges as failure #1 (the reset is named in the cause's detail: "the previous run was stable for Ns — consecutive-failure count reset"). A pause interval never counts — the anchor restamps at every resume, so a 10-minute pause followed by a 1-second crash reads as exactly that (the process served nothing while paused). An adopted instance's anchor starts at ADOPTION: pre-adoption run time is unobservable, so its crash resets only after the adopting engine itself watched the whole window. The window is engine supervision policy at a fixed 30 s — deliberately not a manifest knob (the 18-1 engine-owned-cadence precedent).
> - **New additive surfaces**: `hekma-adapter-api` rides **0.3.0** (`CONTRACT_VERSION` stays 1.1.0 — the `ExitCodes` type joins the readiness/liveness types this release); `hekma-engine` unchanged at 0.9.0, BUT the public `TransitionCause` enum gains the `RestartRequested { code, detail }` and `CleanExit { code }` arms — **hosts matching `TransitionCause` exhaustively need the new arms** — and the `EngineOptions` struct gains one `#[doc(hidden)]` field (`stable_run_window_override`, the engine's own test-scaling seam for the window) — **hosts constructing `EngineOptions` exhaustively (without `..Default::default()`) need that field**; it is not an operator surface. **Reader break, beyond the compile-time arms**: the `restart-requested` and `clean-exit` SERDE tags are new on the wire — an older binary deserializing a newer engine's event stream or `--json` output containing such a transition fails with an unknown-variant error. No new CLI verbs, no new exit codes: `show`/`fleet` name the class and code through the existing failed-cause projection, and the stable-reset detail rides it too.

> **Unreleased — declared liveness: the unresponsive failure.**
> Announced ahead of the release that ships it:
>
> - **The optional `[liveness]` manifest section** (Adapter Contract **1.1.0**, additive same-major — the `[readiness]` section's sibling, same release): ONE declared check, `[liveness.heartbeat_file]` (an Agent-Home-relative file the agent rewrites to prove it is alive) or `[liveness.http_get]` (the same loopback-literal `http://` URL family the readiness probe enforces, plus the one expected status), each carrying `max_age_ms` (`250..=600000`; the floor keeps every declared window at least one reaper tick wide — nothing sub-observable). Validation enforces exactly-one-sub-table and the per-kind value rules, reusing the readiness URL validator so the two sections cannot drift (`deny_unknown_fields` typo protection throughout). A manifest without the section behaves exactly as before — process liveness only; the reaper's transition-EVENT sequence is byte-identical across passes and pinned by test (the pass additionally runs phase-4 snapshot bookkeeping, behaviorally invisible and covered by the same pin). A contract-1.0.0 engine rejects the section at parse time, naming it (the older-same-major-engine rule, for free).
> - **ONE unifying semantic**: the declared signal failing **continuously** for `max_age_ms` fails the instance (`running → failed`, cause `unresponsive`, naming the check and its failing streak). The window is enforced, never observed: a missing heartbeat file is failing from the start (absent ≡ stale), a blip that recovers inside the window never fails, and the engine surfaces the window edges ONCE each — a first-failure diagnostic when the window opens and a recovery diagnostic if the signal returns — never per-tick spam (a signal that flaps repeatedly gets three such pairs, then ONE suppression note, while the window keeps working). A **paused** instance is never probed (a suspended process cannot heartbeat; a pause can never fail the agent), and the window lives in supervision: a restart, an adoption, or a resume-after-pause re-derives it fresh (an adopted instance may honestly get up to one extra window of grace; between commands a detached instance is not evaluated at all — the command-scoped supervision honesty).
> - **The Restart Policy applies verbatim**: an unresponsive failure is a crash for the policy's purposes — restarts ride the normal backoff, and five consecutive unresponsive failures land the ordinary crash-loop terminal (the terminal verdict rides the reaper's ONE diagnostic AND the cause itself — whose optional `detail` carries any expiry honesty, so `show`'s failed-cause projection renders it too). This is the deliberate contrast with the readiness gate, which is terminal by design: a readiness timeout replays a broken deployment, while a liveness failure names an agent that demonstrably launched and then stopped proving it is alive.
> - **New additive surfaces**: `hekma-adapter-api` → **0.3.0** (`CONTRACT_VERSION` stays 1.1.0 — the `Liveness`/`HeartbeatFileCheck`/`HttpGetCheck`/`LivenessCheck` types join the readiness types this release); `hekma-engine` unchanged at 0.9.0, BUT the public `TransitionCause` enum gains the `Unresponsive { check, failing_for_ms, detail }` arm — **hosts matching `TransitionCause` exhaustively need the new arm** (the same rule the `ReadinessTimeout` arm announced). `detail` is OPTIONAL (`Option<String>`, `skip_serializing_if` — absent on the wire when there is nothing to say; populated today only by the liveness expiry's onset honesty, e.g. a heartbeat file that was ALREADY stale when the window first observed it). **Reader break, beyond the compile-time arm**: the `unresponsive` SERDE tag is new on the wire — an older binary deserializing a newer engine's event stream or `--json` output containing an unresponsive transition fails with an unknown-variant error. No new CLI verbs, no new exit codes: `show` names the check + the failing streak through the existing failed-cause projection, `fleet` carries it, and the window diagnostics ride the engine's stderr surface.

> **Unreleased — readiness probes: the adapter-declared gate on `starting → running`.**
> Announced ahead of the release that ships it:
>
> - **The optional `[readiness]` manifest section** (Adapter Contract **1.0.0 → 1.1.0**, additive same-major — joined in the same release by the `[liveness]` section, the block above): ONE declared probe — `http-get` (an `http://` URL + expected status), `file-fresh` (a file's maximum modification age, relative to the Agent Home), or `log-contains` (a substring in this Run's captured agent output) — with its own `timeout_ms` budget (`1..=600000`). Validation enforces the per-kind value rules and rejects another kind's fields (`deny_unknown_fields` typo protection). A manifest without the section behaves exactly as before — the pre-1.1.0 start sequence is byte-identical, pinned by test. An engine built at contract 1.0.0 rejects the section at parse time, naming it (the older-same-major-engine rule, for free).
> - **The gate**: the start waits — after spawn, after the startup watch, and after any ACP handshake — for the probe to pass on the ENGINE-owned poll cadence (~100 ms); the instance reaches `running` only once it does. The wait is surfaced (a stderr note names the probe as it begins, and a second names the pass with the wait actually taken) and bounded (`--detach` returns ready-or-failed). A probe that never passes lands `failed` with the `readiness-timeout` cause naming the probe + the honest wait (also visible via `show`'s failed cause) — and that failure is TERMINAL: the Restart Policy is never consulted, because an adapter that launched but never became ready is a broken deployment a restart would replay. An agent that EXITS while the probe waits follows the ordinary crash path verbatim (`crashed` cause, Restart Policy consulted; the restart is queued for the engine cadence with its normal backoff — a diagnostic names the queued plan and warns that a host exiting before the next cadence pass does not execute it). Under `log capture = "off"` a `log-contains` probe is unsatisfiable and times out honestly — never a fake pass.
> - **New additive surfaces**: `hekma-adapter-api` → **0.3.0** (`CONTRACT_VERSION` 1.1.0; the `Readiness`/`ReadinessGate`/`ReadinessProbe` + per-kind types, resolved by validation); `hekma-engine` unchanged at 0.9.0, BUT the public `TransitionCause` enum gains the `ReadinessTimeout { probe, waited_ms }` arm — **hosts matching `TransitionCause` exhaustively need the new arm** (the same rule the `DetachedResumed` arm announced). **Reader break, beyond the compile-time arm**: the `readiness-timeout` SERDE tag is new on the wire — an older binary (built against the pre-1.1.0 enum) that deserializes a newer engine's event stream or `--json` output containing a readiness-timeout transition fails with an unknown-variant error. Hosts that READ engine event data across versions must add the arm (or a `_` wildcard) before consuming 1.1.0 streams. **Conformance-report wire value**: the TCK report's `contract_version` field now carries `"1.1.0"` (it mirrors `CONTRACT_VERSION`); verification harnesses pinning the literal `"1.0.0"` need that literal updated. No new CLI verbs, no new exit codes.

> **Unreleased — governance conformance vectors: the portable decision artifact.**
> Announced ahead of the release that ships it:
>
> - **The governance vectors artifact** (`crates/hekma-engine/governance/vectors-v1.json` + its `.sha256` sidecar): a versioned, SHA-256-checksummed JSON document of 26 worked cases (15 decision, 11 derivation) generated FROM the engine's own evaluators and re-run against them by verifier tests on every run — byte-identical or red. It pins the eight budget/cost rules a Host must match (`threshold-ge`, `latch-per-scope`, `latch-re-arm`, `token-before-dollar`, `integer-micros`, `round-half-up`, `saturation`, `no-retro-repricing`), each covered by ≥1 case, with the full-cover assertion also enforced by the verifier.
> - **`enforce_budget` routes its decision half through the new pure `governance::decide`** (token dimension evaluated first, dollar second, each filtered by the `(dimension, scope)` latch) — one decision implementation shared with the artifact, so the two cannot drift by construction. Zero behavior change: the pre-existing budget/cost/breach test battery passes unchanged, and the single-evaluator guard tests were re-pointed to pin the new architecture (the evaluators live in the decision core; the choke point routes through `decide`). The record-first-then-act halves stay in the supervisor.
> - **New additive engine surface** — `hekma-engine` → **0.9.0**: `governance::decide` with the `GovernanceInput`/`GovernanceOutcome` types, `run_derivation`, `canonical_json`, `generate`, `checksum_file_content`, the `governance_vectors_checksum()`/`governance_vectors_json()` accessors, the `GOVERNANCE_VECTORS_*` constants, and the case/document schema types. Additive only; no exhaustive enum gained an arm.
> - **New CLI verb** `hekma agent governance-vectors [--json]`: the human path prints a summary (version, checksum, case mix, artifact path); `--json` prints the canonical bytes verbatim, so `hekma agent governance-vectors --json | sha256sum` reproduces the published checksum exactly.
> - **Release + docs**: the artifact and its sidecar ship as release assets (the sidecar inside the published checksums manifest); a new CI job verifies the sidecar and parses the artifact as a stand-in Host; `docs/governance.md` documents the schema, the pinning flow, and the versioning policy (additive cases = a minor bump; any expected-output/input/schema change = a major bump, announced).

> **Unreleased — graceful stops of paused agents (serve shutdown honesty).**
> Announced ahead of the release that ships it:
>
> - `hekma serve` killed PAUSED agents without a graceful stop at shutdown: the drain plan enumerated only `Running` rows, so a paused agent skipped every ladder pass and was then SIGKILLed by the engine-exit kill-on-drop teardown — no graceful window, no stop transition, and a phantom `paused` row over a dead process. Two fixes compose: the drain now takes `Paused` rows through the SAME bounded ladder (the host's signal notice, zero-agent note, and still-alive roll-up name the paused scope), and the engine's stop path RESUMES a real suspension before the ladder — POSIX holds a signal to a `SIGSTOP`ped process pending until `SIGCONT`, so the graceful `SIGTERM` could never be delivered to a suspended agent and a stop from `paused` ALWAYS escalated to a forced kill (a direct `hekma agent stop` of a paused instance hit the same wall). The resume is best-effort and surfaced; on Windows the best-effort pause never suspends, so the resume is the documented no-op there and the ladder is unchanged.

> **Unreleased — concurrent `config set` no longer silently loses keys.**
> Announced ahead of the release that ships it:
>
> - `hekma agent config set` (any engine) serialized its read→merge→write of the instance `config.toml` only per-write (the atomic rename prevents a torn file, not a lost update): two concurrent set processes — two operators, an operator + a script — could interleave as A-reads/B-reads/A-writes/B-writes, and B's document silently dropped A's key while BOTH commands exited 0 (reproduced live: 12 parallel CLI writers on distinct keys landed only 10, different keys lost per run; the regression pin loses a key on 6/6 runs on main). The load→set→write sequence is now serialized across processes by a per-instance advisory write lock (`config.toml.lock` beside the config, the `host.lock` pattern: the OS releases it when the holder dies, the residue file is re-acquired never refused, and a stuck holder times out within the store's 5s busy-wait discipline with a typed Store error instead of an unbounded hang).

> **Unreleased — hekma-engine 0.8.0: the host-mode service surface.**
> Announced ahead of the release that ships it:
>
> - **`SpawnSpec.inherit_env`** — new public field on the exhaustively-constructed `SpawnSpec` (exhaustive constructors need it): `true` keeps today's `Command::envs` semantics (the child sees the engine's whole environment plus the mapped keys); `false` clears the environment first and applies ONLY the declared mappings — the long-running Host hard-sets `false` so every hosted child gets a minimal, manifest-declared environment (story 17-2's `child_env_allowlist` seam).
> - **`RegistryError::ServeOptionsOpenTimeOnly`** — new variant on the exhaustive `RegistryError` enum (hosts matching exhaustively need the arm or `_`): the typed refusal for applying serve options to a supervisor whose lifecycle already began or twice on the same supervisor. Never reachable through the shipped open paths (`open`, `open_with_diagnostics`, `open_with_options` all apply once, on a fresh supervisor, before any lifecycle).
> - **`SpawnRecord.owner`** — new public field on the exhaustively-constructed `SpawnRecord` (exhaustive constructors need it): `Option<ProcessFingerprint>` naming the ENGINE PROCESS supervising the record (schema v10's `owner_pid`/`owner_start_time` columns; `None` for pre-v10 rows and degraded start-time reads). Read by orphan adoption as the LIVE-OWNER boundary — see the live-owner block below. Stop semantics are unchanged: a no-handle stop re-acquires the process on demand.
> - **`ProcessBackend::self_fingerprint` / `ProcessBackend::probe_alive`** — two new methods on the `ProcessBackend` port (implementors need them): this process's own fingerprint (the owner stamp the spawn path writes), and a no-handle live-identity probe (`adopt`'s exact pid + start-time check) for the owner-liveness gate.
> - `hekma-engine` → **0.8.0** — v0.11.0 published 0.7.0, so this additive surface takes the next minor (the version-slot move that keeps the release-to-release semver gate honest; the same pattern the 0.7.0 slot move took after v0.10.0).

> **Unreleased — the live-owner adoption boundary (cross-engine supervision honesty).**
> Announced ahead of the release that ships it:
>
> - Any `hekma` command that opened an engine over a state directory re-adopted EVERY live write-ahead spawn record — including ATTACHED agents a LIVE engine process was still supervising elsewhere (a running `hekma serve`, an embedding, a long-lived command) — re-arming kill-on-drop for them, so the first benign command (`hekma agent list`, `show`, …) STOPPED that agent at its own exit. The boundary: the spawn record now carries its supervising OWNER engine's process fingerprint (schema v10, additive nullable columns), and orphan adoption SKIPS a non-detached record whose owner is still alive elsewhere (surfaced as a diagnostic naming the owner pid). Benign commands leave a live owner's agent alone; an explicit `stop` keeps its exact previous meaning — a stop that finds no handle re-acquires the process on demand by its spawn-record fingerprint and runs the real ladder, so cross-engine stops still terminate the agent and the terminal state stays truthful. Dead-owner records (the crashed-engine case adoption exists for) adopt byte-for-byte as before, and detached records ignore the owner entirely (adoption stays disarmed).

> **Unreleased — transition-append compensation (durable-state honesty).**
> Announced ahead of the release that ships it:
>
> - When a lifecycle transition's durable event append fails AFTER the store commit (the store already settled the new state; `instance.log` could not record it), the engine now **compensates the row back to the prior state** — the registry never disagrees with the last durably-landed event — and the surfaced typed `Log` error **names the compensation**. If the compensating write fails too, the error **names both failures** and the row stays honestly at the new state (no retry loop, no fabricated event: a synthetic append would trust the medium that just failed). The success path is byte-identical — no new events, no ordering changes. Library surface: error detail text only; no new variants, no API change.

> **Unreleased — between-commands raw-log backlog: bounded follow prime.**
> Announced ahead of the release that ships it:
>
> - `logs --follow`'s raw fallback cursor no longer primes at the file's current end: it starts at the last N complete lines of `agent.log` (default 200, `HEKMA_LOGS_BACKLOG_LINES`), so output written between commands — while no engine session held a tailer — streams at follow start under the same once-per-session un-attributed qualifier. A gap larger than the window streams only its last N lines and the qualifier names the skipped-beyond count; a partial trailing line is never primed; a garbage env value falls back to the default with a stderr note naming it. The engine's live-capture guard keeps a fresh attributed capture byte-identical, and the one-shot `logs` is untouched. (Landed in #298; announced here per the release-notes discipline.)

> **Unreleased — conformance kit hardening (the TCK's third-party surface).**
> Announced ahead of the release that ships it:
>
> - **`hekma-conformance` additive surface** (path-only crate, no crates.io bump): `run_conformance_with` + `TckConfig` (per-section timeout, a CALLER-OWNED state root, typed section selection) — the default `run_conformance` stays byte-identical; the report shape is unchanged (schema_version 1, subsets yield fewer entries). Caller roots must be FRESH (empty) and are exclusively one run's — a non-empty or uncreatable root is a named stage failure, which also kills the stale-artifact false-pass and fixed-name-collision classes.
> - **Read routing + error honesty**: every read with an existing public seam routes through it (log polls via `read_agent_log`, metering source via `fleet_entry`, the `--dump` pair via `adapter::resolve` with documented resolve-from-source semantics); DB errors no longer masquerade as zero rows (named errors, propagated immediately); pre-section failures carry their own stage prefix (state root / engine open / registration / capabilities / snapshot read — no more blanket "registration failed").
> - **The `--dump` subject-delivery proof convention** (Adapter Contract docs): a manifest adapter that declares config delivery SHOULD provide a `--dump` seam; the TCK's memory section now proves the SUBJECT received the delivered values through it (dump-carrying adapters mapping `memory.dir` to a non-env target skip the leg honestly — the probe twin remains the proof).

> **Unreleased — ACP verified against real OpenCode + Hermes agents.**
> Announced ahead of the release that ships it:
>
> - **opencode is now a run-verified `acp`-kind agent** — opencode v2 ships a native ACP server (`opencode acp`); register it with `--kind acp`, `acp.command = opencode`, `acp.args = acp`. Verified end-to-end against `v2.0.28` (2026-10-16): handshake (its rich `initialize`/`session/new` shapes tolerated), a full prompt turn with the reply retained in the instance log, context-grain `usage_update` surfacing, clean stop. The pre-v2 manifest characterization remains documented for HTTP-native agents without an ACP mode.
> - **Hermes via `hermes-acp` is run-verified at `v0.21.7+500.g8f81791`** — register → start → full turn → clean stop plus context-grain surfacing; the migration path from the deprecated `hermes` kind now carries a real-run pin, not just the contract-twin proof.
> - **New opt-in turn-COMPLETION smokes** (`HEKMA_TEST_REAL_AGENT_TURNS=1`): drive a full prompt turn against the real `opencode`/`hermes-acp` binaries and assert the reply text lands in the retained log (reconstructed from the chunk stream — real agents split even a one-token reply across several updates). Default off with a visible skip note; binary presence alone never arms a possibly-costly model call. The existing register/start/send/stop smokes keep their skip-unless-present posture, and the real opencode dialect extras are pinned against the contract twin (`opencode-dialect` fake-agent mode) so tolerance is proven without the binary.
> - No engine behavior changed: the `acp` kind already tolerated everything the real agents sent; this change adds the PROOF and the docs (validation pins moved forward with fresh evidence in `docs/agents.md`).


> **Unreleased — detached continuation (run persistence, honest logs between commands, paused-stray handling, supervision indicator).**
> Announced ahead of the release that ships it:
>
> - **Detached instances are now ONE Run across commands.** The Run id is persisted with the write-ahead spawn record (state-schema v9, an additive nullable column) and every later command's adoption **resumes** it instead of minting a fresh one — per-run token/dollar totals no longer reset per command, and a budget breach that already fired for that Run is spent (the breach latch seeds from the instance's breach log, which keys events by run id: once per Run, not once per command). A genuine stop→start or Restart-Policy restart still mints a fresh Run and resets the latch. A pre-upgrade (pre-v9) spawn record is minted once, persisted back, and noted once on stderr.
> - **`FleetEntry` gains `supervision`** — an ADDITIVE, absent-when-none field (`skip_serializing_if`): `detached` or `session` exactly when a live spawn record exists, omitted for a stopped/never-started instance (never an implied supervised). The values are the new PUBLIC `Supervision` enum (`hekma_engine::Supervision` — `Detached`/`Session`, kebab-case on the wire), so library consumers can match on the indicator without string-parsing. Surfaces as a compact human `list` column and a `show` row beside the additive `--json` field. The Story 4-3 frozen key-set assertions were re-pinned in the same change; no always-present key moved and the Fleet schema version stays `2`.
> - **`TransitionCause` gains `DetachedResumed`** — a new closed-vocabulary cause tag (wire `detached-resumed`) on the `paused → running` edge the engine drives at release of a DETACHED **operator-paused** handle (an operator-paused detached child is never left SIGSTOP-frozen and unsupervised between commands; adoption of a stranded paused row keeps it paused — never auto-resumes — and says so on stderr). **This is a new arm on an exhaustive `enum`** — hosts matching `TransitionCause` exhaustively need the arm (or `_`). A **budget-breach** pause is deliberately NOT resumed at release: the freeze holds into the enforcement-free between-commands window (surfaced with a note naming why), so the breach-driven spend-stop is never un-done where no enforcement exists.
> - **`logs --follow` watches the raw `agent.log`** — between commands a detached instance's output lands only in the raw capture; new raw lines now stream in a later command's follow (human and NDJSON alike; stdout stays pure NDJSON) with an un-attributed passthrough qualifier on stderr, once per session. The one-shot `logs` still reads the attributed capture only.
> - **Engine/host surface**: `Engine::read_raw_output_since` / `Blocking::read_raw_output_since` (new public methods); `SpawnRecord` gains the public `run_id: Option<RunId>` field (exhaustive constructors need it); the public `StateStore` trait gains a DEFAULTED `set_spawn_run_id(name, run_id, fingerprint)` method (compile-compatible; the UPDATE compare-and-sets on the spawn record's fingerprint so a concurrent stop→start's fresh Run id is never stomped, and the default body fails loud with a named unsupported-operation error, so a custom store that has not implemented durable Run identity surfaces it instead of silently degrading — the engine then keeps the documented pre-v9 mint-per-adoption posture). `hekma-engine` → **0.7.0** — a one-hop jump on crates.io (0.4.0 → 0.7.0; the intermediate 0.5.0/0.6.0 bumps were in-tree version reservations only, never published — everything they announced ships here).
> - **Memory attach/detach × start cross-process atomicity (spec 15-6)** — the real CLI shape (one Engine per command, one state dir) raced `hekma agent start` against `hekma agent memory attach`/`detach` across the only shared state, the SQLite file; the windows are closed there with the crash-atomic `BEGIN IMMEDIATE` pattern. **Four new `StoreError` variants** — `MemoryBackingHotSwap`, `MemoryBackingChangedUnderStart`, `StateConflict`, `Busy` — are new arms on the exhaustive enum, so external implementors matching `StoreError` need the arms (or `_`). **`EngineError::MemoryBackingChangedUnderStart`** is likewise a new arm on the exhaustive `EngineError` (retryable, names both sides of the race; CLI exit-code row 4, no new number). Every connection now opens with `busy_timeout` = 5000ms installed before the WAL pragma, and `SQLITE_BUSY` classifies into the typed `StoreError::Busy` — a concurrent CLI pair degrades to the bounded wait and a typed retry error, never a raw busy string. The in-process two-mutex lock model is untouched (AD-17/AD-18 stand).

> **Unreleased — port-level memory-backing kind invariant (hardening batch).**
> Announced ahead of the release that ships it:
>
> - **`StoreError::MemoryBackingKindConflict`** — new variant on the exhaustive `StoreError` enum (hosts matching exhaustively need the arm or `_`). The state store now enforces the memory-backing kind invariant BELOW the registry: a re-attach that changes the attached kind on an existing row is refused with this typed error (detach first, then attach the new kind) instead of silently replacing it. Same-kind re-attaches are unchanged, and the CLI's behavior is unchanged (the registry always guarded this) — only a caller driving the `StateStore` port directly could ever observe the refusal. `hekma-engine` → **0.7.0** (ships in v0.11.0).

> **Unreleased — ACP backend support (the builtin `acp` kind).**
> Announced ahead of the release that ships it:
>
> - **New builtin `acp` kind** — any Agent Client Protocol (ACP) v1 agent runs under full supervision via `hekma agent register <name> --kind acp` + the `acp.command`/`acp.args` config keys. `send` = one `session/prompt` turn (returns immediately; chunks + stop reason stream to the log/events); `stop` sends `session/cancel` first; a concurrent second prompt is refused (typed, surfaced); permissions are denied with a surfaced diagnostic; detached acp starts are refused (the transport needs the pipes). Client capabilities advertised = the protocol default (no fs/terminal/elicitation in v1).
> - **`EngineError::AcpTurnInFlight`** — new variant on the exhaustive `EngineError` enum (hosts matching exhaustively need the arm or `_`). Maps to exit-code row 4; no new exit code.
> - **`SpawnSpec.pipe_stdout` (new public field) + `ProcessBackend::take_stdin`/`take_stdout` (defaulted trait methods)** — the ACP transport owns the child's stdio halves; every raw line still lands in the instance log. Defaulted methods keep existing backends compiling; exhaustive `SpawnSpec` constructors outside the crate need the new field. `hekma-engine` → **0.7.0** (ships in v0.11.0).
> - **ACP metering**: `usage_update` is context-grain and surfaced only (never billed); billing-grade tokens (input/output/**cached**) come from the epic's tiered paths (observed base-URL / stderr sentinel); no source → the honest not-available marker. Cached tokens become first-class across parse/ledger/rate/budgets/surfaces for every kind.
> - **Deprecation — the `hermes` kind is deprecated in favor of `--kind acp`.** Hermes Agent speaks the Agent Client Protocol natively (its `hermes-acp` entry point), so new registrations should use the `acp` kind (set `acp.command`/`acp.args` to the `hermes-acp` executable). The `hermes` kind keeps working **unchanged** — this is an announcement, not a removal — and per the CLI-surface deprecation policy it can be removed only at a future MAJOR release, announced ahead in these notes and the docs. Migration parity is shipped and tested with the `acp` kind: **metering** continues through the self-reported usage sentinel (under `acp` the sentinel channel is the agent's stderr, since its stdout is the ACP protocol stream; cached tokens included), and **memory delivery** continues through `HERMES_HOME` — the `acp` builtin maps the reserved `memory.dir` key to the same env var the `hermes` builtin does, so a `filesystem` Memory Backing attached to an `acp` instance reaches the agent's process environment identically (the `memory attach --json` `declared` fact reads `true` for both).

> **Compatibility notice — shipped in v0.9.0 (Adapter Contract v1 freeze; announced ahead per the deprecation policy).**
> Per the deprecation policy ratified by Islam on 2026-09-04 (*within a major, deprecations announced ≥1 minor ahead via CHANGELOG/RELEASE_NOTES + doc notices; removals only at next major; enforced by semver-checks CI*):
>
> - **The Adapter Contract is frozen at v1 (`1.0.0`)** (previously the `0.4.0` seed). The engine now **negotiates** at registration (FR-30): a manifest loads only when its `contract_version` **major** matches the engine's; a mismatch fails with both versions named and the rule quoted — *compatible iff the major versions match*. **Pre-v1 `0.x` manifests are not grandfathered** (the contract was never published under 0.x): set your manifest's `contract_version` to `"1.0.0"`. The parse is strict `X.Y.Z` (no `v` prefix, no `1`/`1.0` partials); prerelease/build suffixes parse and negotiate by major. See the new [Adapter Contract](docs/adapter-contract.md#versioning) page.
> - **New `--json` wire surface on the memory commands** — the ONE announced key-set edit (transferred from Story 5-1's DC-6 obligation, deferred by Story 5-2, and frozen with contract v1): `kt agent memory attach <name> --kind <kind> --json` emits a versioned document (`schema_version` `1`, `instance`, `kind`, `guarantee`, `dir`, `declared`) carrying the backing kind and guarantee level in their typed snake_case strings (`filesystem`/`native`, `managed_dir_byte_durable`/`home_persistence_only`); `kt agent memory detach <name> --json` emits the minimal versioned confirmation (`schema_version`, `instance`). Human output is unchanged. The Story 4-3 frozen key-set assertions were re-pinned to include these documents in the same change; every other frozen key-set is untouched.
> - **`[interaction].channel` gains the `"http"` value** (CP-6.5-a, additive vocabulary): an HTTP-native agent may name its real interaction transport. The engine does not branch on the channel in v1. Additive — nothing that parsed before stops parsing.
> - The CI `semver` job's toolchain cache is now keyed on the resolved `cargo-semver-checks` version, verified against the restored binary, and saved only after a real install (AI-3). The job now also runs an **armed in-repo baseline check** (`cargo-semver-checks --baseline-rev` against the contract-v1 freeze commit `4119db3`): any breaking change to the public `ktesio-adapter-api` surface vs the freeze baseline (removal, rename, signature change) fails CI during the pre-publish window — whether or not it was announced; announcement enforcement is review discipline until the crates.io baseline lets the gate diff release-to-release. The crates.io-published-baseline comparison stays dormant (notice-only) until the crates publish to crates.io (story 7-4).
> - **`ConformanceReport` (the conformance Test Kit's report) carries `contract_version`** — the Adapter Contract version the run was negotiated under. Additive and `#[serde(default)]` (empty = produced by a pre-field harness), so archived `schema_version: 1` reports still deserialize; the schema version stays `1` per the report family's documented bump policy. Third parties asserting on reports see one new key.
> - **`AgentAdapter` default lifecycle bodies return a new `Unavailable` reason string** (a frozen-v1 surface TEXT change, an announced, compatibility-neutral wording fix in place (the ratified deprecation policy's >=1-minor / remove-at-major clauses govern type and key-set removals, not correcting stale text)): the seed text `"lifecycle execution is not implemented until story 1-4"` — a stale development-story reference in a public error surface — is replaced by `"this adapter does not implement lifecycle ops (the trait's inert default body)"`. No types, variants, or key-sets change; only the human-readable `reason` text inside `AdapterError::Unavailable` produced by the trait's DEFAULT bodies (any adapter overriding the ops is unaffected). Code that string-matched the old seed text must match the new one.
>
> *Placement note (mechanics): this banner sits ABOVE the first `## ` heading on purpose — `scripts/generate_release_docs.py::upsert_release_section` inserts every generated release section directly above the first `## ` heading, so a `## Unreleased` heading would be pushed below the fold at the very next tag. The script never touches the header region, so the banner stays visible through the next release cut. At that cut, the release author moves this notice's content into the release section and deletes the banner.*

> **Embedding distribution — PUBLISHED (v0.7.0, 2026-09-09).**
> The epic's distribution capstone executed on Islam's explicit go (release issue #176): the three library crates are on crates.io. **Shipped in-repo:** the embedding quickstart — a complete host example at `crates/ktesio-engine/examples/embedding-quickstart.rs` that links only the engine's public facade (and nothing else — no test fixtures, no helper crates) and that CI compiles on all three OS legs and RUNS hermetically on ubuntu; the new [Embedding the engine](docs/embedding.md) documentation page (git-pinned dependency form, facade surface, event-bus contract, walkthrough); the publish runbook — every held action (publish-flag flip → `cargo publish -p ktesio-adapter-api` → `-p ktesio-adapters-hermes` → `-p ktesio-engine` → the `vX.Y.Z` tag → the Homebrew tap verification) written with exact commands, preconditions, and order in [the release process](docs/release-process.md#publishing-the-engine-crates); and the `ktesio-engine` in-repo semver baseline armed in CI alongside `ktesio-adapter-api`'s (each crate guarded against its own freeze commit — the engine's embedding surface froze with story 7-3, since its unpublished surface legitimately grew after the earlier contract freeze). Hosts depend on the published crates (`ktesio-engine = "0.1"`); the git-pin guidance remains as a fallback. The tag releases the `ktesio` CLI (0.7.0), the binaries, and the Homebrew tap update in the same sweep.

> **Epic 7 — the engine becomes an embeddable library: event bus, embed-clean guarantee, measured performance budgets.**
> Three headline surfaces of the embedding epic are new and shippable in-repo today (all three library crates now on crates.io):
>
> - **The engine event bus** (story 7-2): hosts and other Rust consumers can subscribe to the engine's committed events without polling — `Engine::subscribe()` (async, a raw broadcast receiver) and `Blocking::subscribe()` (sync, an `EventSubscription` whose `recv`/`try_recv` bridge through the engine runtime). Every payload is a versioned serde struct (`EngineEvent` wrapping lifecycle `TransitionEvent`, `BudgetBreachEvent`, and `UsageUpdateEvent` — each carrying its own `schema_version`, the exact wire shapes `kt --json` documents). The bus is bounded at `EVENT_BUS_CAPACITY` (1024): a subscriber that falls behind observes `Lagged(n)` naming the dropped prefix and resynchronizes at the tail — a slow subscriber can never stall supervision. Delivery is at-most-once in the crash window between a durable append and its publish; the durable record stays complete, the query APIs (`transition_events`, `budget_breach_events`, ledger reads) read the past directly, and `resync_events` (Epic 10, below) now heals that window in one call.
> - **The embed-clean guarantee** (story 7-3): the engine embeds into a host's process cleanly — it never reads stdin, never prints an interactive prompt, never detects or branches on a TTY, never mutates the host's process environment, and installs no process-global handlers or global state that could collide with a host runtime (several engines coexist in one process, each rooted at its own state directory). The full API is reachable behind the `blocking()` facade — the same surface `kt` itself drives — and CI audits that guarantee durably (a two-engine collision test, a source-level no-TTY/no-prompt/no-global-state audit with narrow named allowlist entries, and a whole-crate blocking-coverage inventory).
> - **The performance budgets are measured, not assumed** (story 7-5): the NFR-4 budgets — reads < 1 s on a 25-instance Fleet; supervision overhead ≤ 2% CPU and ≤ 50 MiB RSS per running instance — are benchmarked by a real harness (`crates/ktesio-engine/examples/perf-budgets.rs`) on a real running fixture, gated in CI by a dedicated ubuntu `perf-budgets` job whose JSON report is the measured record (reads p99 and RSS gate hard everywhere; CPU gates strict locally with the documented ×1.5 shared-runner tolerance over the median of three windows in CI). The measured figures and the gate policy are documented in [Testing](docs/testing.md#performance-budgets-nfr-4-story-75).

> **Epic 10 — embedder hardening: crash-window resync, host-owned diagnostics, ratified subscriber budgets.**
> Three new public surfaces of the embedding epic, additive throughout (the semver gate sees new items, never a breaking change):
>
> - **`resync_events` + `ResyncCursor`/`ResyncBatch`** (story 10-3): the one-call remedy for the event bus's documented at-most-once crash window — `Engine::resync_events(name, after)` / `Blocking::resync_events` read the instance's COMMITTED truth (transitions, breaches, ledger rows) past a per-family cursor and return it as the exact `EngineEvent` payloads the live bus delivers, plus the cursor to continue from (idempotent; serializes snake_case so a host can persist it). Crash-recovery reads are torn-tail tolerant — one unparseable trailing line carrying the torn-append signature (no final newline) is skipped, with the skip SURFACED on `ResyncBatch::torn_tail_skipped` (an additive `#[serde(default)]` field — archived batches still deserialize); every other malformed line is a typed error. A cursor past a family's committed count (a truncated/rotated log) is a typed error naming the family and both counts — never a silent re-delivery. Both combine orders are documented with their tradeoffs: subscribe-first is gap-free (dedup the overlap with your cursor), resync-first needs a quiescent agent to be gap-free.
> - **`DiagnosticSink` + `Engine::open_with_diagnostics` / `with_diagnostics` / `Blocking::with_diagnostics`** (story 10-2): route the engine's two stderr diagnostics (the DC-10 memory-delivery notice and the enforcement breadcrumb) into a host-provided `Arc<Mutex<Box<dyn Write + Send>>>` — same bytes, one line each. Install at open (in place before any supervision work) or rotate any time later (the outgoing writer is flushed on rotation); write errors AND write panics are best-effort swallowed (a host bug never unwinds through supervision); installing is one-way — it replaces, never removes, so returning to the stderr default means re-opening the engine. With no sink, the stderr behavior is byte-identical (pinned by a subprocess suite), and with a sink installed stderr stays silent.
> - **Ratified subscriber-active budgets** (story 10-3): the perf-budgets harness gains a subscriber-overhead addendum with three hard gates on the one-active-subscriber deltas vs the zero-subscriber baseline — CPU Δ ≤ 0.25 percentage points, RSS Δ ≤ 2 MiB, read-p99 Δ ≤ 100 ms per running instance (strict locally; the documented ×1.5 shared-runner tolerance in CI) — ratified from measured deltas recorded in [Testing](docs/testing.md#performance-budgets-nfr-4-story-75).
> - **Audit teeth**: the embed-clean audit pins the sink plumbing count==1 (the emission choke point, both routes into it, the single stderr default arm) and its stdio-reach scan covers fully-qualified, bare imported-path, and hand-written `_print` write forms — a stdio write via an imported path cannot evade the single-writer count.


> **ktesio-engine 0.3.0 — the epic-12 breaking release (published 2026-09-16, Islam's explicit go; announced ahead in the epic-12 PR).**
> The engine's crate version moves 0.2.0 → 0.3.0 to carry story 12-1's detached-start surface — the CI semver gate's crates.io loop flagged all three against the published 0.2.0 (its second real firing):
>
> - **`EngineError::DetachRefused` is a NEW variant on the exhaustive `EngineError` enum**: a `--detach` start of an engine-observed instance is refused with a named reason (its loopback metering listener dies with the CLI command — the same loud strand as adoption), never a silent fallback to attached. Host `match`es over `EngineError` need the new arm (or a `_` wildcard).
> - **`SpawnRecord` gains the pub field `detach`** — exhaustive struct literals over `SpawnRecord` in host code need the new field.
> - **`ProcessBackend::adopt` takes a second parameter (`detached: bool`)** — port-trait implementors must update the signature (the Windows backend's adopted shape is inherently drop-disarmed; the Unix backend uses the flag to re-hold a detached record's handle with its Drop disarmed).
> - Hosts on the crates.io pin move `ktesio-engine = "0.2"` → `"0.3"` (docs/embedding.md); the git-pin alternative is unchanged.

> **ktesio-engine 0.2.0 — the library's first breaking release (published 2026-09-15, Islam's explicit go; announced ahead in PR #181).**
> The engine's crate version moves 0.1.0 → 0.2.0 to carry the epic-11 surface extension that the CI semver gate flagged (its first real firing — first against the in-repo freeze baseline, then against the PUBLISHED 0.1.0 baseline once the crates.io release-to-release loop armed):
>
> - **`EngineError::ResumeUnsupported` is a NEW variant on the exhaustive `EngineError` enum** (AI-7, story 11-1): a `resume` on a PAUSED instance whose adapter declares pause `unsupported` for the current OS fails fast with a DEDICATED diagnostic — it names the state, the adapter's pause declaration, and the escape hatch (`stop` works without pause support) — instead of the bare pause-unsupported error that would strand the operator. Downstream `match`es over `EngineError` in host code need the new arm (or a `_` wildcard); NOTHING else on the engine surface moved (semver-checked release-to-release against published 0.1.0, and against the in-repo freeze baseline `bee7d48` on every push).
> - Hosts on the crates.io pin move `ktesio-engine = "0.1"` → `"0.2"`; the git-pin alternative is unchanged (see [Embedding the engine](docs/embedding.md)).
> - `ktesio-adapter-api` and `ktesio-adapters-hermes` are unchanged at 0.1.0 — no republish.

> **Hekma — the v0.8.0 rename (Ktesio → Hekma), shipping in this release.**
> The product formerly released as Ktesio ships as **Hekma**: the commands are `hekma` (primary) and `hkm` (short alias — same CLI, both standalone); the crate is `hekma` (the `ktesio` crate is frozen at 0.7.0 on crates.io, preserved, never yanked); the engine family continues its line as `hekma-engine` 0.4.0, `hekma-adapter-api` 0.2.0, `hekma-adapters-hermes` 0.2.0; the Homebrew formula renames in the same tap to `ktesio/tap/hekma`; docs live at `hekma.ktesio.dev`. **Ktesio remains the publisher** — Hekma is a ktesio.dev project; the license (Ktesio Noncommercial-Attribution License 1.0.0) is unchanged.
> **For `kt` users (deliberate, no compatibility window):** old `kt` binaries cannot self-update to 0.8.0 — release archives carry only the `hekma-*` family. Re-run the installer (`curl -fsSL https://cli.ktesio.dev/hekma/install.sh | sh`), or `cargo install hekma --force`, or `brew upgrade ktesio/tap/hekma`. **Your data is untouched** — the same state directory is read, nothing is moved or reset; `KTESIO_*` environment settings keep working (with new `HEKMA_*` aliases); JSON output, exit codes, the `KTESIO_USAGE` sentinel, `HERMES_HOME`, and the adapter contract are unchanged. Full details: the [migration guide](docs/migration.md).


## v0.11.0

Comparison: [v0.9.0...v0.11.0](https://github.com/Ktesio/hekma/compare/v0.9.0...v0.11.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [hekma-v0.11.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [hekma-v0.11.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [hekma-v0.11.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [hekma-v0.11.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [hekma-v0.11.0-checksums.txt](https://github.com/Ktesio/hekma/releases/download/v0.11.0/hekma-v0.11.0-checksums.txt) | - |

### Features

- perf-budgets baseline trend tracking — ratchet, committed baseline, documented updates (#285) ([186da69](https://github.com/Ktesio/hekma/commit/186da69))
- TCK hardening — public seams, error honesty, stage-differentiated failures, configurability, --dump convention (#282) ([65e7b45](https://github.com/Ktesio/hekma/commit/65e7b45))
- detached continuation — one Run across commands, breach-once, honest logs, supervision indicator (#281) ([9a2ceaa](https://github.com/Ktesio/hekma/commit/9a2ceaa))
- memory-backing invariant + adoption park-loss note + detached reaper (hardening batch 2) (#232) ([a5a763a](https://github.com/Ktesio/hekma/commit/a5a763a))
- ACP backend support — epic opened, spec stage complete, story 14-1 (the transport core) (#193) ([1e71389](https://github.com/Ktesio/hekma/commit/1e71389))

### Fixes

- memory attach/detach × start atomicity — durable-store transactions close the cross-process windows (#286) ([7c73d60](https://github.com/Ktesio/hekma/commit/7c73d60))
- Windows detached stdio write-end leak (root-caused) + x86-scoped CI skip (#283) ([800934c](https://github.com/Ktesio/hekma/commit/800934c))
- keep the previous-tag lower bound in pre-tag release-docs rehearsals (#278) ([9583b4a](https://github.com/Ktesio/hekma/commit/9583b4a))
- drop the direct postcss dev-dependency that conflicts with its override (#273) ([4bc791e](https://github.com/Ktesio/hekma/commit/4bc791e))
- record session/cancel delivery on the engine-attributed log (#266) ([4766352](https://github.com/Ktesio/hekma/commit/4766352))
- override transitive katex to >=0.18.2 (prototype-pollution gadget, GHSA-238p-pmpm-9mq7) (#256) ([ba3017a](https://github.com/Ktesio/hekma/commit/ba3017a))
- repin the conformance-kit git example — the pinned rev predates the crate rename (#252) ([68c1f42](https://github.com/Ktesio/hekma/commit/68c1f42))
- canonical /docs/ migration URL in generated release bodies (#251) ([597033a](https://github.com/Ktesio/hekma/commit/597033a))
- Apache-2.0 wording and the source-install path (#243) ([016c137](https://github.com/Ktesio/hekma/commit/016c137))
- docs probe follows the /docs/ mount — flat-URL reds, latent pipefail false-red (#249) ([7ab76f9](https://github.com/Ktesio/hekma/commit/7ab76f9))
- follow the narrowed OS-cfg allowlist in the in-repo CI-string pin (#235) ([5057851](https://github.com/Ktesio/hekma/commit/5057851))
- semver job script — close the both-graph case my shim insertion broke; add bash -n gate for every embedded workflow script ([0a02c2f](https://github.com/Ktesio/hekma/commit/0a02c2f))

### Documentation

- epic-15 closeout — retrospective, tracker repair, spec closeouts (#287) ([ea1a9f0](https://github.com/Ktesio/hekma/commit/ea1a9f0))
- refresh TRADEMARK, SPONSORS, SUPPORT to the post-rename identity (#276) ([a186a7a](https://github.com/Ktesio/hekma/commit/a186a7a))
- fix the source-install clone dir and state-path examples (#261) ([cae38b0](https://github.com/Ktesio/hekma/commit/cae38b0))
- update release notes for v0.9.0 (#191) ([6b55c81](https://github.com/Ktesio/hekma/commit/6b55c81))
- Ktesio attribution on the docs home page ([15fcafd](https://github.com/Ktesio/hekma/commit/15fcafd))
- clarify the ktesio.dev attribution line ([d78f3b5](https://github.com/Ktesio/hekma/commit/d78f3b5))

### Tests

- make the supervision-indicator and raw-follow tests Windows-honest (#284) ([3191699](https://github.com/Ktesio/hekma/commit/3191699))
- verify the acp kind against real opencode + hermes-acp agents (#280) ([e839265](https://github.com/Ktesio/hekma/commit/e839265))
- pin the non-UTF-8 stdout read-failure death (#277) ([9f58225](https://github.com/Ktesio/hekma/commit/9f58225))
- pin the resume result line + gap/context/malformed-layer absence arms (#274) ([c11840d](https://github.com/Ktesio/hekma/commit/c11840d))
- drop secret-tainted steering values from assert panics (#272) ([192f3b0](https://github.com/Ktesio/hekma/commit/192f3b0))
- pin the open-fault mapping, spawn-record pass-through, and steering degradation arms (#271) ([548844c](https://github.com/Ktesio/hekma/commit/548844c))
- pin the failed-stop breach breadcrumb + config-fault disarm (#267) ([1d71bdb](https://github.com/Ktesio/hekma/commit/1d71bdb))
- pin the resume divergence ladder + stuck-poll error map (#265) ([1205b2f](https://github.com/Ktesio/hekma/commit/1205b2f))
- pin the crash-cause vocabulary + row-gone + sibling-corroboration arms (#263) ([cf168cf](https://github.com/Ktesio/hekma/commit/cf168cf))
- serialize the state-dir env tests behind a shared lock (#260) ([1d743cd](https://github.com/Ktesio/hekma/commit/1d743cd))
- pin the not-draining send timeout and poisoned-channel refusal (#259) ([74fd289](https://github.com/Ktesio/hekma/commit/74fd289))
- pin the reader's tolerance + stream-death arms (#258) ([04b61ab](https://github.com/Ktesio/hekma/commit/04b61ab))
- pin the adapter-snapshot write-failure rollback arm (#257) ([4d4dd23](https://github.com/Ktesio/hekma/commit/4d4dd23))
- pin the --follow rotation notice (stderr-only, no NDJSON leak) (#255) ([8f30db1](https://github.com/Ktesio/hekma/commit/8f30db1))
- pin the TCK config section's swallowed pass-through failure arm (#253) ([8c51228](https://github.com/Ktesio/hekma/commit/8c51228))
- pin the self-reported metering fleet-equality failure arm (#254) ([f2a8632](https://github.com/Ktesio/hekma/commit/f2a8632))
- scrub inherited HERMES_HOME in the hermes e2e; visible opt-out for the real-agent smokes (#250) ([f219920](https://github.com/Ktesio/hekma/commit/f219920))
- full restoration — 95% gate (guard fix + 40-min budget + all restoration tests) (#238) ([763b632](https://github.com/Ktesio/hekma/commit/763b632))
- restoration batch 2 — start-time memory-dir guard arms + the AI-12 environmental-cap test ([c470764](https://github.com/Ktesio/hekma/commit/c470764))
- seven duplicate manifest-writer copies → the shared fixture builder (#233) ([d9a9b22](https://github.com/Ktesio/hekma/commit/d9a9b22))

### CI

- fix the duplicate env: key that startup-failed the workflow (#239) ([c22928f](https://github.com/Ktesio/hekma/commit/c22928f))
- raise the test-job budget 25 -> 40 minutes (the suite outgrew the wall) (#237) ([0b62590](https://github.com/Ktesio/hekma/commit/0b62590))

### Maintenance

- re-anchor the canonical baseline from the CI leg's own run (#289) ([3471f0e](https://github.com/Ktesio/hekma/commit/3471f0e))
- bump hekma to 0.11.0 — the backlog clear-out + engine 0.7.0 minor (#288) ([a074810](https://github.com/Ktesio/hekma/commit/a074810))
- bump hekma to 0.10.0 — the ACP + engine-hardening minor (#279) ([7a757c5](https://github.com/Ktesio/hekma/commit/7a757c5))
- bump rustls to 0.23.46 for RUSTSEC-2026-0346..0349 (#275) ([c1bbef1](https://github.com/Ktesio/hekma/commit/c1bbef1))
- bump the cargo-dependencies group with 4 updates (#268) ([2a98c21](https://github.com/Ktesio/hekma/commit/2a98c21))
- bump the docs-dependencies group in /docs with 6 updates (#270) ([825e757](https://github.com/Ktesio/hekma/commit/825e757))
- bump the github-actions group with 2 updates (#269) ([b37459d](https://github.com/Ktesio/hekma/commit/b37459d))
- raise engines floor to match katex 0.18 toolchain (#262) ([10bf3c1](https://github.com/Ktesio/hekma/commit/10bf3c1))
- bump sharp from 0.35.4 to 0.35.5 in /docs (#247) ([04d6aeb](https://github.com/Ktesio/hekma/commit/04d6aeb))
- bump lodash-es from 4.17.23 to 4.18.1 in /docs (#246) ([74bb929](https://github.com/Ktesio/hekma/commit/74bb929))
- bump source-map-js from 1.2.1 to 1.2.2 in /docs (#245) ([22ec402](https://github.com/Ktesio/hekma/commit/22ec402))
- bump the docs-dependencies group across 1 directory with 8 updates (#244) ([1a087ed](https://github.com/Ktesio/hekma/commit/1a087ed))
- bump dompurify from 3.4.15 to 3.4.16 in /docs (#241) ([ecdfa96](https://github.com/Ktesio/hekma/commit/ecdfa96))
- bump sharp from 0.35.3 to 0.35.4 in /docs (#178) ([b0897aa](https://github.com/Ktesio/hekma/commit/b0897aa))
- bump baseline-browser-mapping from 2.10.33 to 2.11.25 in /docs (#226) ([263eeb1](https://github.com/Ktesio/hekma/commit/263eeb1))
- bump the docs-dependencies group across 1 directory with 11 updates (#183) ([362220c](https://github.com/Ktesio/hekma/commit/362220c))
- bump the github-actions group across 1 directory with 2 updates (#134) ([fae5ef2](https://github.com/Ktesio/hekma/commit/fae5ef2))
- bump the cargo-dependencies group across 1 directory with 14 updates (#240) ([321270e](https://github.com/Ktesio/hekma/commit/321270e))
- record the Windows CI flake family (epic-12 item 95 extension) (#236) ([6489018](https://github.com/Ktesio/hekma/commit/6489018))
- CI + docs + test-hygiene batch (hardening backlog, part 1) (#231) ([d28c94e](https://github.com/Ktesio/hekma/commit/d28c94e))
- apply the confirmed action-item status batch (Islam, 2026-09-22) ([37482d7](https://github.com/Ktesio/hekma/commit/37482d7))
- move the semver baselines to the epic-14 squash (1e71389) — PR #193 merge checklist (#229) ([2014dea](https://github.com/Ktesio/hekma/commit/2014dea))
- retire the GitHub-issue mirror — project tracking migrated to Fibery (#228) ([efc4b8b](https://github.com/Ktesio/hekma/commit/efc4b8b))
- sprint-record closeout + epic-13 (supervisor split A+B implemented) (#192) ([7f4454e](https://github.com/Ktesio/hekma/commit/7f4454e))

### Other Changes

- the five product calls, drafted for ratification (PC-1..PC-5) (#234) ([b10eaeb](https://github.com/Ktesio/hekma/commit/b10eaeb))
- the owed retros — epics 10, 13, 14 (accepted-with-open-items) (#230) ([bf0c575](https://github.com/Ktesio/hekma/commit/bf0c575))

## v0.9.0

Comparison: [v0.8.1...v0.9.0](https://github.com/Ktesio/hekma/compare/v0.8.1...v0.9.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [hekma-v0.9.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [hekma-v0.9.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [hekma-v0.9.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [hekma-v0.9.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [hekma-v0.9.0-checksums.txt](https://github.com/Ktesio/hekma/releases/download/v0.9.0/hekma-v0.9.0-checksums.txt) | - |

### Features

- fully OSS — Apache-2.0, NOTICE added, CLA retained (v0.9.0) ([14fbf77](https://github.com/Ktesio/hekma/commit/14fbf77))

### Fixes

- sidebar brand + crates link still said Ktesio — the layout config escaped the rename ([249e300](https://github.com/Ktesio/hekma/commit/249e300))

### Documentation

- 'A Ktesio project ↗' -> ktesio.dev in the docs sidebar ([ee007b7](https://github.com/Ktesio/hekma/commit/ee007b7))
- update release notes for v0.8.1 (#190) ([1b9b048](https://github.com/Ktesio/hekma/commit/1b9b048))
- update release notes for v0.8.0 (#189) ([de0071e](https://github.com/Ktesio/hekma/commit/de0071e))

### Maintenance

- retrigger pages deploy (sidebar attribution) ([c49eaf3](https://github.com/Ktesio/hekma/commit/c49eaf3))

## v0.8.1

Comparison: [v0.8.0...v0.8.1](https://github.com/Ktesio/hekma/compare/v0.8.0...v0.8.1)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [hekma-v0.8.1-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [hekma-v0.8.1-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [hekma-v0.8.1-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [hekma-v0.8.1-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [hekma-v0.8.1-checksums.txt](https://github.com/Ktesio/hekma/releases/download/v0.8.1/hekma-v0.8.1-checksums.txt) | - |

### Fixes

- matrix REPO -> Ktesio/hekma (canonical post-rename) ([3377a2d](https://github.com/Ktesio/hekma/commit/3377a2d))
- drop v0.6.0 from the matrix floor — no GitHub release exists ([f2b7794](https://github.com/Ktesio/hekma/commit/f2b7794))
- matrix harness — assert empty-fleet JSON correctly on pre-fleet hops ([24900ec](https://github.com/Ktesio/hekma/commit/24900ec))

### Documentation

- brew trust-gate note (verified live during the v0.8.0 cutover) ([6b2f4ed](https://github.com/Ktesio/hekma/commit/6b2f4ed))

### Maintenance

- v0.8.1 — canonical URLs to Ktesio/hekma; docs probe drops the retired legacy host ([74fdcae](https://github.com/Ktesio/hekma/commit/74fdcae))

## v0.8.0

Comparison: [v0.7.0...v0.8.0](https://github.com/Ktesio/ktesio/compare/v0.7.0...v0.8.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [hekma-v0.8.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [hekma-v0.8.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [hekma-v0.8.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [hekma-v0.8.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [hekma-v0.8.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.8.0/hekma-v0.8.0-checksums.txt) | - |

### Features

- Hemaka — the v0.8.0 rename (Ktesio → Hemaka) (#187) ([ad0feaf](https://github.com/Ktesio/ktesio/commit/ad0feaf))
- durable detach + the production-usable observed channel (#184) ([49da96b](https://github.com/Ktesio/ktesio/commit/49da96b))
- bump ktesio-engine to 0.2.0 — the announced breaking release ([d087ed0](https://github.com/Ktesio/ktesio/commit/d087ed0))
- epic 11 — Technical Debt & Process Cleanup (all 7 stories) (#181) ([bee7d48](https://github.com/Ktesio/ktesio/commit/bee7d48))
- epic 10 — consolidate & harden the embedding surface (#180) ([b590dc8](https://github.com/Ktesio/ktesio/commit/b590dc8))

### Fixes

- correct the product name — Hemaka → Hekma, maka → hkm (#188) ([54dc79c](https://github.com/Ktesio/ktesio/commit/54dc79c))
- adopted Windows stop could never confirm death — add SYNCHRONIZE + exit-code probe ([c6ecadb](https://github.com/Ktesio/ktesio/commit/c6ecadb))

### Documentation

- v0.8.0 cutover — publishes recorded, embedding flip, fresh semver baselines @54dc79c ([ba7406d](https://github.com/Ktesio/ktesio/commit/ba7406d))
- 0.3.0 published — docs flip + decision log (Islam's go) ([4299bde](https://github.com/Ktesio/ktesio/commit/4299bde))
- step-5 flip — ktesio-engine 0.2.0 is LIVE; pins move to "0.2" ([fa76eaa](https://github.com/Ktesio/ktesio/commit/fa76eaa))
- announce engine 0.2.0 (the first breaking library release); record the go + the semver-baseline KEEP decision ([5152977](https://github.com/Ktesio/ktesio/commit/5152977))
- record the v0.7.0 manual tap push in the decision log ([5747d50](https://github.com/Ktesio/ktesio/commit/5747d50))

### Tests

- bounded reader grace (no join) in the detach harness — the hang is a PRODUCT finding ([56ee766](https://github.com/Ktesio/ktesio/commit/56ee766))
- in-harness progress markers (entry / post-spawn / 10s ticks) for the Windows detach hang ([66654e1](https://github.com/Ktesio/ktesio/commit/66654e1))
- incremental pipe readers + non-blocking timeout dump in the detach harness ([e5092ce](https://github.com/Ktesio/ktesio/commit/e5092ce))
- kill-before-join in the bounded harness — the join deadlocked the dump ([d773932](https://github.com/Ktesio/ktesio/commit/d773932))
- the detach-test forensic dump goes to stdout (nextest TMT shows stdout only) ([2e5546d](https://github.com/Ktesio/ktesio/commit/2e5546d))
- bounded legs + forensic dump in the 12-1 detach CLI test ([139f7c2](https://github.com/Ktesio/ktesio/commit/139f7c2))
- detach-leg markers on stdout (nextest TMT shows stdout only) ([40be515](https://github.com/Ktesio/ktesio/commit/40be515))
- leg markers in the 12-1 detach CLI test (Windows hang triage) ([08ae1d5](https://github.com/Ktesio/ktesio/commit/08ae1d5))

### Maintenance

- lockfiles for the one-shot ktesio-* deprecation shims (v0.8.0 release series) ([f8d6fb1](https://github.com/Ktesio/ktesio/commit/f8d6fb1))
- record the 12-1 Windows findings — SYNCHRONIZE stop fix landed; the stdio pipe-hold gap routed ([a7fcc1b](https://github.com/Ktesio/ktesio/commit/a7fcc1b))
- bump ktesio-engine to 0.3.0 — the epic-12 breaking release ([394e660](https://github.com/Ktesio/ktesio/commit/394e660))
- point github-sync-map project number at the Ktesio org copy (project 3) ([d32570d](https://github.com/Ktesio/ktesio/commit/d32570d))
- point project links at the Ktesio org copy (project 3) ([3ec8f86](https://github.com/Ktesio/ktesio/commit/3ec8f86))
- update repository references for Ktesio org migration ([e05bc93](https://github.com/Ktesio/ktesio/commit/e05bc93))
- post-merge epic-12 followups — semver baseline to 49da96b, tracker done ([8842b8b](https://github.com/Ktesio/ktesio/commit/8842b8b))
- untrack the local .video_agent artifact + ignore it ([9ee4df7](https://github.com/Ktesio/ktesio/commit/9ee4df7))
- RATIFIED — AI-20/AI-47 applied verbatim as epic-12 (Islam: recommended across the board) ([27ecf07](https://github.com/Ktesio/ktesio/commit/27ecf07))
- AI-58 draft — the AI-20 + AI-47 product-call proposals (awaiting ratification) ([de78001](https://github.com/Ktesio/ktesio/commit/de78001))
- epic-11 retrospective — accepted-with-open-items, 7 action items ([5d84e7f](https://github.com/Ktesio/ktesio/commit/5d84e7f))
- epic-11 complete — flip 11-1..11-7 + epic to done (merged as bee7d48) ([1ef8439](https://github.com/Ktesio/ktesio/commit/1ef8439))
- bump the ktesio-engine semver baseline to the epic-11 merge commit (bee7d48) ([4e12ef8](https://github.com/Ktesio/ktesio/commit/4e12ef8))
- epic 11 opened — Technical Debt & Process Cleanup ([f99e670](https://github.com/Ktesio/ktesio/commit/f99e670))
- epic 11 opened — Technical Debt & Process Cleanup (sprint change proposal approved) ([2f2b763](https://github.com/Ktesio/ktesio/commit/2f2b763))
- epic-5 + epic-7 retrospectives complete ([29bff1d](https://github.com/Ktesio/ktesio/commit/29bff1d))
- epic 10 -> done (PR #180 merged as b590dc8); 10-1..10-3 -> done ([ba524c5](https://github.com/Ktesio/ktesio/commit/ba524c5))
- epic 10 opened — consolidate & harden the embedding surface (sprint change proposal approved) ([999164c](https://github.com/Ktesio/ktesio/commit/999164c))

### Other Changes

- is_multiple_of — the newer stable-clippy lint the CI gate runs ([35a7525](https://github.com/Ktesio/ktesio/commit/35a7525))

## v0.6.0

Ktesio is repositioned as an agent runner: `kt` runs AI agents like services — supervising their lifecycle, metering real token usage, and enforcing dollar budgets.

### Removed

- The legacy skill-manager command surface is **removed** at 0.6.0. The commands `kt init`, `kt install`, `kt search`, `kt upgrade`, `kt publish`, `kt list`, `kt show`, `kt doctor`, `kt uninstall`, and the `kt remove` alias no longer exist; `kt` is no longer a skills package manager.

### Changed

- The single canonical way to operate the Fleet is the agent runner under `kt agent …` — `kt agent list` and `kt agent show <name>` replace the removed top-level `kt list`/`kt show`, alongside `kt agent register`/`start`/`stop`/`pause`/`resume` and `kt agent config …`. See the [command reference](docs/commands.md) for the full agent-runner surface.
- `kt --help` and the crate metadata now describe the agent runner rather than a skills package manager.
- Continuity is preserved: the `ktesio` crate name, the `kt` binary, the install channels, and `kt self-update` are unchanged.

## v0.5.0

Comparison: [v0.4.0...v0.5.0](https://github.com/Ktesio/ktesio/compare/v0.4.0...v0.5.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.5.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.5.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.5.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.5.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.5.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.5.0/ktesio-v0.5.0-checksums.txt) | - |

### Features

- add fumadocs documentation site ([ba48fe0](https://github.com/Ktesio/ktesio/commit/ba48fe0))

### Fixes

- migrate ureq usage for cargo dependency updates (#44) ([29de3e8](https://github.com/Ktesio/ktesio/commit/29de3e8))

### Documentation

- update release notes for v0.4.0 (#37) ([3fdf921](https://github.com/Ktesio/ktesio/commit/3fdf921))

### Maintenance

- bump version to 0.5.0 ([08c20ec](https://github.com/Ktesio/ktesio/commit/08c20ec))
- untrack BMAD artifacts and relicense under PolyForm Noncommercial 1.0.0 (#49) ([59c8d19](https://github.com/Ktesio/ktesio/commit/59c8d19))
- bump the docs-dependencies group in /docs with 4 updates (#42) ([d977de7](https://github.com/Ktesio/ktesio/commit/d977de7))
- bump actions/checkout from 6.0.2 to 6.0.3 in the github-actions group (#40) ([a90451b](https://github.com/Ktesio/ktesio/commit/a90451b))

### Other Changes

- ```text feat: add BMad Method v6.8.0 skills — agents, workflows, and core tools ([21a9ad1](https://github.com/Ktesio/ktesio/commit/21a9ad1))

## v0.4.0

Comparison: [v0.3.1...v0.4.0](https://github.com/Ktesio/ktesio/compare/v0.3.1...v0.4.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.4.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.4.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.4.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.4.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.4.0/ktesio-v0.4.0-checksums.txt) | - |

### Features

- add update notice and self-update (#34) ([5fcfb60](https://github.com/Ktesio/ktesio/commit/5fcfb60))
- add hosted installers (#36) ([2fe0f82](https://github.com/Ktesio/ktesio/commit/2fe0f82))

### Documentation

- update release notes for v0.3.1 (#32) ([6f50c2e](https://github.com/Ktesio/ktesio/commit/6f50c2e))

### Maintenance

- bump version to 0.4.0 ([580dfa1](https://github.com/Ktesio/ktesio/commit/580dfa1))
- add kt-release skill ([ff17e29](https://github.com/Ktesio/ktesio/commit/ff17e29))

## v0.3.1

Comparison: [v0.3.0...v0.3.1](https://github.com/Ktesio/ktesio/compare/v0.3.0...v0.3.1)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.3.1-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.3.1-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.3.1-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.3.1-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.3.1-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.3.1/ktesio-v0.3.1-checksums.txt) | - |

### Features

- discover fallback skills from agents directory (#31) ([634c99a](https://github.com/Ktesio/ktesio/commit/634c99a))

### Documentation

- merge install details into quickstart ([7913033](https://github.com/Ktesio/ktesio/commit/7913033))
- update quickstart install paths ([1a94d4f](https://github.com/Ktesio/ktesio/commit/1a94d4f))
- update release notes for v0.3.0 (#28) ([05651e6](https://github.com/Ktesio/ktesio/commit/05651e6))

### Maintenance

- bump version to 0.3.1 ([dc0965b](https://github.com/Ktesio/ktesio/commit/dc0965b))

## v0.3.0

Comparison: [v0.2.0...v0.3.0](https://github.com/Ktesio/ktesio/compare/v0.2.0...v0.3.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.3.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.3.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.3.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.3.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.3.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.3.0/ktesio-v0.3.0-checksums.txt) | - |

### Features

- show init adoption progress ([7ac8362](https://github.com/Ktesio/ktesio/commit/7ac8362))

### Fixes

- recognize publish docs examples ([8c1443c](https://github.com/Ktesio/ktesio/commit/8c1443c))

### Documentation

- update release notes for v0.2.0 (#27) ([12ce314](https://github.com/Ktesio/ktesio/commit/12ce314))

### Maintenance

- bump version to 0.3.0 ([dabb5a0](https://github.com/Ktesio/ktesio/commit/dabb5a0))

## v0.2.0

Comparison: [v0.1.1...v0.2.0](https://github.com/Ktesio/ktesio/compare/v0.1.1...v0.2.0)

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.2.0-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.2.0-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.2.0-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.2.0-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.2.0-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.2.0/ktesio-v0.2.0-checksums.txt) | - |

### Features

- add dependency publish manifest model ([784a02b](https://github.com/Ktesio/ktesio/commit/784a02b))
- polish CLI terminal output (#26) ([86e13be](https://github.com/Ktesio/ktesio/commit/86e13be))
- add skills search and shorthand installs (#23) ([17b4e4d](https://github.com/Ktesio/ktesio/commit/17b4e4d))

### Documentation

- update release notes for v0.1.1 (#11) ([883c7a3](https://github.com/Ktesio/ktesio/commit/883c7a3))

### CI

- remove OCI release packaging (#24) ([2e9cc1d](https://github.com/Ktesio/ktesio/commit/2e9cc1d))

### Other Changes

- [codex] Add adoption CLI workflows (#17) ([d66f73f](https://github.com/Ktesio/ktesio/commit/d66f73f))
- [codex] Add README banner (#12) ([c41c7ed](https://github.com/Ktesio/ktesio/commit/c41c7ed))

## v0.1.1

Comparison: Initial release history

| Platform | Target | Archive | Checksum |
|----------|--------|---------|----------|
| macOS Intel | `x86_64-apple-darwin` | [ktesio-v0.1.1-x86_64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-apple-darwin.tar.gz.sha256) |
| macOS Apple Silicon | `aarch64-apple-darwin` | [ktesio-v0.1.1-aarch64-apple-darwin.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-aarch64-apple-darwin.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-aarch64-apple-darwin.tar.gz.sha256) |
| Windows x64 | `x86_64-pc-windows-msvc` | [ktesio-v0.1.1-x86_64-pc-windows-msvc.zip](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-pc-windows-msvc.zip) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-pc-windows-msvc.zip.sha256) |
| Linux x64 | `x86_64-unknown-linux-gnu` | [ktesio-v0.1.1-x86_64-unknown-linux-gnu.tar.gz](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-unknown-linux-gnu.tar.gz) | [sha256](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| All | checksums | [ktesio-v0.1.1-checksums.txt](https://github.com/Ktesio/ktesio/releases/download/v0.1.1/ktesio-v0.1.1-checksums.txt) | - |

### Features

- improve cli visuals and help ([052ca93](https://github.com/Ktesio/ktesio/commit/052ca93))
- add release automation and open source polish ([f8ef392](https://github.com/Ktesio/ktesio/commit/f8ef392))
- Add GitHub CI pipeline for PR checks (#4) ([171de35](https://github.com/Ktesio/ktesio/commit/171de35))
- integrate GitHub issue tracking into task implementation and PR workflow ([4b4fdb7](https://github.com/Ktesio/ktesio/commit/4b4fdb7))
- add integration tests and improve unit test coverage ([2f6bf07](https://github.com/Ktesio/ktesio/commit/2f6bf07))
- add skill install fallback discovery ([7e6b43f](https://github.com/Ktesio/ktesio/commit/7e6b43f))
- add comprehensive documentation and test coverage ([b5fc1a5](https://github.com/Ktesio/ktesio/commit/b5fc1a5))
- implement agentic skills package manager CLI ([73e6ac3](https://github.com/Ktesio/ktesio/commit/73e6ac3))

### Fixes

- allow partial skill manifests ([a5f5dc4](https://github.com/Ktesio/ktesio/commit/a5f5dc4))
- install exported skill content safely ([2f525b2](https://github.com/Ktesio/ktesio/commit/2f525b2))

### Documentation

- mark dependabot updates merged ([a73ebeb](https://github.com/Ktesio/ktesio/commit/a73ebeb))
- clarify solo maintainer branch policy ([36a1b74](https://github.com/Ktesio/ktesio/commit/36a1b74))
- add repository audit checklist ([d978ca7](https://github.com/Ktesio/ktesio/commit/d978ca7))
- correct repository name and path in quick start instructions ([10da4ff](https://github.com/Ktesio/ktesio/commit/10da4ff))
- add test coverage and documentation currency principles (v1.1.0) ([c72c185](https://github.com/Ktesio/ktesio/commit/c72c185))

### Tests

- increase coverage for cli helpers (#7) ([7f29853](https://github.com/Ktesio/ktesio/commit/7f29853))

### CI

- publish only release asset files ([00e7fd3](https://github.com/Ktesio/ktesio/commit/00e7fd3))
- publish crate before release artifacts ([88438e5](https://github.com/Ktesio/ktesio/commit/88438e5))
- identify crates io release check ([dc2f96d](https://github.com/Ktesio/ktesio/commit/dc2f96d))
- use current intel macos release runner ([ad0e63d](https://github.com/Ktesio/ktesio/commit/ad0e63d))
- exempt dependabot prs from dco by author ([537742b](https://github.com/Ktesio/ktesio/commit/537742b))
- align dco checks with automation ([5682357](https://github.com/Ktesio/ktesio/commit/5682357))
- publish release artifacts to homebrew and crates (#6) ([d571bd5](https://github.com/Ktesio/ktesio/commit/d571bd5))

### Maintenance

- prepare 0.1.1 release ([285f059](https://github.com/Ktesio/ktesio/commit/285f059))
- rename project to ktesio (#10) ([d2cfa1f](https://github.com/Ktesio/ktesio/commit/d2cfa1f))
- bump cargo dependency group ([3ac0ab6](https://github.com/Ktesio/ktesio/commit/3ac0ab6))
- bump github actions group ([4a0ed6e](https://github.com/Ktesio/ktesio/commit/4a0ed6e))
- use canonical apache license text ([e5acc16](https://github.com/Ktesio/ktesio/commit/e5acc16))
- harden repository governance ([c1463f4](https://github.com/Ktesio/ktesio/commit/c1463f4))

### Other Changes

- Add license, homepage, repository, and readme to Cargo.toml ([d636953](https://github.com/Ktesio/ktesio/commit/d636953))
- apply code formatting and update Rust edition to 2024 ([32fbc59](https://github.com/Ktesio/ktesio/commit/32fbc59))
- speckit ([8d14960](https://github.com/Ktesio/ktesio/commit/8d14960))
- Initial commit from Specify template ([76d7354](https://github.com/Ktesio/ktesio/commit/76d7354))
