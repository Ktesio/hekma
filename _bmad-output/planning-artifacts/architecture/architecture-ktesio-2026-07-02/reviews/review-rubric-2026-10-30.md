# Spine Review: good-spine rubric, 2026-10-30 control-plane amendment

Scope: `ARCHITECTURE-SPINE.md` as amended 2026-10-30 (AD-22..AD-27; AD-7/AD-18/AD-20/AD-21 amended; the *Amendments (2026-10-30)* block; conventions; capability map; Deferred). Checked against PRD §3, §4.14 (FR-49..FR-60), FR-21, FR-41, FR-43, NFR-4, NFR-9, §6, §7; the approved proposal §4.2; the memlog rulings of 2026-10-30; and the code under `crates/`. Ground rules respected: `ktesio-*` names are a ruled decision; rationale is not asked for; Deferred items are judged only on whether they let two units diverge.

## Verdict

**Not yet ready to bind Epics 21-25.** The amendment picks the right shapes, and they ratify Islam's rulings faithfully: a Run registry in the core, a lock-free snapshot for the data path, one proxy, one Shim per process, opaque hashed tokens, and state.db plus a calls store. It fixes most of the divergence points it names. Several load-bearing seams are still open, though, and different stories will fill each one differently:

- The spine assumes one engine process per state directory. The brownfield runs several: CLI engines next to `hekma serve`.
- The shared proxy has no endpoint or route model. Nothing says how a supervised Run is attributed, or where a wire Run's call goes upstream.
- The upgrade write in AD-7 can never fire once the engine-defaults layer carries `warn`.
- The handoff sequence is not decided: the host lock, how the successor starts, schema migration, and Shim protocol versions.
- Two paths are undefined: proxy to store writer to budget evaluator, and Run creation or announce from the request path.
- The proxy's wire contract (header names, response headers, the reject body) is a public surface with no owner.
- Inbound OTLP (FR-51/FR-52) has no architecture.

Each finding below has evidence and replacement text. The tally is 1 critical, 7 high, 8 medium and 2 low.

## Findings

### 1. CRITICAL: one-process assumption. A CLI engine beside `hekma serve` bypasses the snapshot, the proxy and revocation (AD-25, AD-26, AD-22, AD-27)

- **Problem.** AD-25 publishes the snapshot "after every commit that changes what the data path needs", and AD-22 says "Revocation deletes the row and republishes; the next call fails". Both describe an in-process mechanism. AD-26 gives "exactly **one** Metering Proxy listener per engine-owning process — the Service Host when it runs, else the CLI's engine for its lifetime — on a fixed port", and AD-27 gives "One **store writer task**".
- **What the code does.** The brownfield deliberately runs several engine processes on one state directory:
  - `host.lock` only stops a second *Service Host*: `crates/hekma-host/src/lock.rs:1-3`, "one `hekma serve` process per state directory".
  - The live-owner columns exist because "a second CLI Engine on the same state root" is normal: `crates/hekma-engine/src/ports/state_store.rs:78-95`, `crates/hekma-engine/src/domain/error.rs:600-604`, schema v10/v11 `owner_pid`/`lease_pid` at `store/sqlite.rs:314-350`.
- **What breaks.** Suppose a CLI `hekma budget raise`, a Policy edit, a key revocation or a `hold` commits from a CLI engine while `hekma serve` runs:
  - The Service Host's proxy keeps serving its stale snapshot. The revoked key still works, and the raised ceiling never re-arms.
  - AD-25's "every surface calls this function" holds per process, but the processes disagree.
  - Two CLI engines without a host each try to bind the fixed port, so one of them fails.
  - A supervised Run whose engine-owning CLI process exits loses its proxy. The Shim keeps the process alive, but its `base_url` is dead. Today's code refuses `--detach` for `engine-observed` for this exact reason (`docs/commands.md:209`), and AD-22 custody makes every custodied Run depend on the proxy.
  - Stories 21-3/21-4 (CLI verbs) and 22-x/23-x (proxy) will each pick a different answer.
- **Fix.** Add to AD-25 (or as AD-25 rule text):
  > **One data-path owner per state directory.** While a Service Host holds `host.lock`, it is the only process that runs the Metering Proxy, the store writer and the published snapshot. A CLI engine that finds `host.lock` held sends every Run, Policy, budget, hold and credential mutation through the Control API (the local socket and bearer the host publishes in the state dir), and never commits them to state.db itself. Without a Service Host, a CLI engine runs the proxy only for Runs it supervises in that command, and refuses (exit 5) any mode that needs the proxy to outlive the command: `--detach` with `engine-observed`, or key custody. The Service Host also polls `PRAGMA data_version` and republishes on any foreign commit, as a backstop for writes from older binaries.

  If Islam prefers "the CLI writes and the host polls `data_version` with a bound" instead of routing through the Control API, state that, and state the staleness bound that revocation then carries.

### 2. HIGH: the shared proxy has no endpoint or route model; supervised attribution and per-call upstream are undecided (AD-26, AD-22)

- **What AD-26 says.** It "replaces the per-instance `ObservedListener`; supervised instances get its address through the existing `metering.base_url` override". It identifies Runs by "Run Key, virtual provider key, identity header, endpoint binding — in that order", and reroute uses "a per-call upstream from configured routes".
- **What it leaves out.**
  - Today each listener is bound to one upstream and one instance by socket: `crates/hekma-engine/src/metering/listener.rs:4-11` and `:301` (`start(handle, upstream)`), with `metering.upstream_base_url` per instance at `domain/config.rs:121`.
  - On a shared fixed port, a supervised agent without custody sends only its own provider key. It carries no Run Key and no identity header, and every instance gets the same address. No rule says how the proxy attributes the call or which upstream it forwards to.
  - FR-53 says an endpoint-path binding "is not a Scent-Check identity". Under `shadow`, every supervised non-custody call would raise an unscented Anomaly, and under `enforce` it would be refused, unless the spine says how a supervised Run carries its identity.
  - A base-URL-only `wire` client (FR-51) has no upstream at all.
- **Where it diverges.** 22-2 (virtual keys), 23-1 (wire discovery), 23-4 (reroute) and 21-6 (gateway recipes) will each invent an endpoint table.
- **Fix.** Add to AD-26:
  > **Endpoints and routes.** The proxy serves configured *endpoints*, rows in state.db: {path prefix, upstream base URL, bound labels, optional custodied key name, optional route set}. A supervised Run's injected `metering.base_url` is `<proxy>/r/<run id>/` plus the instance's upstream path, which the engine computes. That segment attributes the call, and the instance's `metering.upstream_base_url` is its upstream. The engine also injects the Run Key through a reserved invocation-override key, which is the supervised Run's Scent-Check identity, so a supervised Run is never unscented. A virtual provider key maps to {Run or bound labels, custodied key name, endpoint}. A call that matches no endpoint is refused with the typed `no-route` response, so it is never forwarded to a guessed upstream.

### 3. HIGH: the breach-default upgrade write cannot fire, and engine-defaults would outrank every Policy (AD-7, AD-9 amendment, AD-25)

- **Evidence.**
  - AD-7: "an engine-defaults entry `budget.breach_action = "warn"` makes the source visible. On first open after the upgrade, every instance whose four config layers carry no `budget.breach_action` gets `pause` written". The engine-defaults layer is one of those four layers: `domain/config.rs:224-258` (`SourceLayer::ORDER = [EngineDefault, KindDefault, Instance, InvocationOverride]`), loaded from `ENGINE_DEFAULTS_TOML` by `domain/registry.rs:1754-1763`. Once that TOML carries `warn`, no instance matches "carry no `budget.breach_action`", so nothing is kept on `pause`. That contradicts FR-21's upgrade consequence.
  - The invocation layer is per start and unknown at open time, so it cannot be part of an open-time check.
  - AD-25 ranks "explicit Operator/Host limits on the instance or Run first", and FR-60 says "any FR-11 layer". If the engine-defaults `warn` counts as explicit, it outranks every Policy, so a Policy can never set a Breach Action for a supervised instance.
  - The default Policy and the engine-defaults layer now both claim to be the default. The code also has no `Block` variant yet (`domain/budget.rs:86-97`).
- **Fix.** Replace the AD-7 sentence and add one to AD-25:
  > On first open after the upgrade, every instance whose **kind-defaults and instance layers** carry no `budget.breach_action` gets `pause` written at the instance layer, with the store marker. The check runs once, keyed by the migration step, so it does not depend on the engine-defaults content.

  > (AD-25) Only the **instance and invocation-override layers**, and limits set on a Run, are *explicit*. Engine-defaults and kind-defaults values for budget keys are not explicit: they rank below every matching Policy, and the default Policy's value replaces them. `hekma agent config show` renders the source as `default-policy`.

### 4. HIGH: the handoff and upgrade sequence is undecided: host lock, successor launch, schema migration, Shim protocol version (AD-20, AD-23, AD-26)

- **Evidence.**
  - AD-26: "the outgoing process passes the listening socket to its successor (Unix `SCM_RIGHTS` over the handoff channel or systemd socket activation; Windows `WSADuplicateSocket`)". AD-20 says "one Service Host owns a state directory, enforced by a lock". AD-23 says "the next Service Host reattaches to every live Shim".
  - `SCM_RIGHTS` and `WSADuplicateSocket` both need the successor to be running before the predecessor exits. But the successor cannot take `host.lock` while the predecessor holds it (`hekma-host/src/lock.rs`), so it is refused under AD-20's own rule.
  - Nothing says who starts the successor (a signal, `hekma serve --takeover`, or the service manager), where the handoff channel lives, or when the successor may migrate state.db while the predecessor's store writer is still open.
  - After an upgrade, a new-version host must speak to Shims still running the *old* binary. No version or compatibility rule exists for the Shim RPC or the handoff channel.
  - Rollback is not addressed either. The forward-compat guard refuses an older binary on a newer `user_version` (`store/sqlite.rs:539-545`), so an old host cannot even read state after a failed upgrade.
- **Where it diverges.** 24-1 (Shim protocol) and 24-2 (handoff) are separate stories, and an upgrade is FR-55's headline case.
- **Fix.** Add to AD-23:
  > **Handoff sequence.**
  > 1. The successor starts with `hekma serve --takeover` and connects to the predecessor's handoff endpoint, a UDS or named pipe at a path the engine's path authority names.
  > 2. The predecessor stops accepting, flushes and closes its store writer, persists holds and Leases, passes the listening socket and the `host.lock` handle (an fd on Unix, a duplicated handle on Windows), releases its Shims, and exits.
  > 3. Only then does the successor migrate state.db and reattach.
  >
  > Under systemd, socket activation replaces the socket pass, and `KillMode=process` with `ExecReload=` drives the takeover.
  >
  > **Versioning.** The Shim RPC and the handoff channel each carry a protocol version in their first message. A host accepts Shims from its own and the previous minor protocol. A Shim it cannot speak to is reported, and is never killed. Downgrade across a schema migration is unsupported and documented.

### 5. HIGH: the proxy to store writer to BudgetEvaluator path is undefined; batching contradicts AD-6 (AD-7, AD-26, AD-27, AD-6)

- **Evidence.**
  - AD-6: "one transaction per usage event (durability loss bound ≤1s...)". AD-27: "One **store writer task** with its own connection batches transactions". AD-7: "ledger transaction → `BudgetEvaluator` (inside the same commit path)". AD-26: the proxy will "emit a **per-call record** ... to the store writer". The *Hot path* convention allows "no unbounded wait".
- **What is left open.**
  - Whether the BudgetEvaluator runs in the writer task.
  - How a BreachDecision for a supervised Run reaches the Supervisor. If the writer takes `Mutex<Supervisor>`, it can stall for the accepted ≤35s `KILL_CONFIRM_TIMEOUT` (AD-18).
  - The bound and overflow policy of the proxy→writer channel. If the proxy blocks, it violates the hot-path rule. If it drops, the ledger undercounts, which breaks FR-22 "totals equal the Usage Ledger exactly".
  - The batch bound that keeps FR-21's "≤ metering interval + 1s" and AD-6's ≤1s.
  - Whether `Mutex<Registry>`'s rusqlite connection still writes state.db concurrently with the writer. If it does, SQLITE_BUSY waits under the global lock are another AD-17 hazard.
- **Fix.** Replace AD-27's writer sentence:
  > One **store writer task** owns every per-call and Run write to state.db and the calls store. It commits a batch at least every 250 ms, or every 512 records, and the per-event durability bound stays ≤1s, superseding AD-6's per-event transaction. The BudgetEvaluator runs inside the writer's commit, per event, in arrival order. A BreachDecision for a wire action republishes the snapshot. One for a process action is sent to the supervisor over a channel, and the writer never takes `Mutex<Supervisor>`. The proxy→writer channel is bounded. When it is full, the proxy keeps forwarding and counts each unrecorded call into a durable per-Run `unrecorded_calls` counter, flushed with the next batch and surfaced as an Anomaly. It never blocks the request and never drops silently. Lifecycle writes under `Mutex<Registry>` keep their own connection with `busy_timeout` ≤ 1 s, as AD-17's bound.

### 6. HIGH: the proxy's wire contract is a public surface with no owner and no version (AD-26, AD-22, PRD §7)

- **Evidence.** The spine and PRD rely on these wire elements:
  - a Run Key header, an "identity header", and an "announce header" whose response carries "the Run Key and refresh credential in response headers" (FR-52);
  - a breach "response header where the call passes through the Metering Proxy" (FR-21);
  - identity headers forwarded to gateways (FR-51);
  - "a typed non-retryable response" for reject/`block`, a retryable error on restart, and the `hk_*` token prefixes.

  No header names exist anywhere: `grep -ri 'x-hekma'` over `crates/` and `docs/` finds nothing. PRD §7 lists five versioned surfaces, and this wire contract is not among them.
- **Where it diverges.** The CrewAI/LangGraph helpers (23-2), the proxy (22-1, 23-1, 23-4), gateway recipes (21-6) and the typed-reject consumers (19-2 Hosts) will each name headers and status codes on their own, and external helpers will break on any rename.
- **Fix.** Add a convention row:
  > **Proxy wire contract.** All request and response header names, the announce exchange, the typed reject/retryable error bodies and status codes, and the token prefixes are defined once in `docs/metering-proxy.md` under a `proxy-wire v1` marker, and in one engine module that the proxy and the helpers' test vectors both read. It is versioned like the Control API (additive-only within `0.x`). Header names use the `hekma-` prefix.

  Raise the sixth surface to the PRD as a §7 open item.

### 7. HIGH: inbound OTLP for `observed` Runs and the OTel announce channel has no architecture (FR-51, FR-52)

- **Evidence.** FR-51: "Hekma accepts inbound OTLP from clients; telemetry carrying a run identity creates or updates an `observed` Run". FR-52 adds "an OTel resource attribute creates or updates the Run's identity and Lease". The FR-49 table lists `observed` usage as "from its telemetry, estimated". In the spine, the only trace is a diagram edge (`OBS -- OTLP --> API`).
- **What is undecided.**
  - The receiver: whether it is in the host or the engine, its route, its auth class, and which encodings it accepts. Most OTel SDKs default to OTLP/gRPC or http/protobuf, while AD-21 bans gRPC and protobuf for the exporter.
  - Which attribute names carry run identity and labels.
  - Whether usage is taken from metrics (`gen_ai.client.token_usage`) or from spans.
  - How content-bearing span attributes or events are dropped, as §6 requires ("never request or response content").
  - The capability map routes FR-51/FR-52 only to "Metering Proxy + Control API announce routes".
- **Where it diverges.** 23-1 and 23-2 have no rule to follow, and the AD-21 "no protobuf" stance will be read as covering intake.
- **Fix.** Add a rule to AD-20 (or a new AD-28):
  > **OTLP intake.** The Service Host serves `POST /v1/metrics` and `/v1/traces` (OTLP/HTTP, JSON and protobuf. Intake may accept protobuf, the exporter stays JSON) behind the announce-scoped or bearer credential. The resource attributes `hekma.run.id` and `hekma.run.label.<key>` identify the Run. Usage comes only from `gen_ai.client.token_usage` data points, or from `gen_ai.usage.*` span attributes. Every other span attribute and every span event is discarded before anything is stored. Intake writes through the store writer as `observed`-depth records labeled `estimated`.

  If protobuf intake is a new dependency, record the NFR-8 sign-off, or decide JSON-only and say that SDKs must be configured for it.

### 8. HIGH: creating a Run, and announcing one, from the request path is undefined (AD-25, AD-26, AD-22, AD-27)

- **Evidence.**
  - AD-26: the proxy "reads only the published snapshot" and only "emit[s] a per-call record ... to the store writer".
  - FR-51: "A discovered Run appears after its first call". FR-52: the announce header "returns the Run Key and refresh credential in response headers". AD-22: tokens are "stored only as a SHA-256 hash in state.db".
  - On a first call from an unknown identity, the Run is in no snapshot. Nothing says who mints its run id: the proxy (no lock) or the core (a round trip on the request path).
  - On an announce, the proxy hands out a token whose hash may not be durable yet. After a crash, the client holds a key that state.db never stored.
- **Where it diverges.** 23-1, 23-2 and 22-1 each meet this first, and they could choose three different mechanisms.
- **Fix.** Add to AD-26:
  > **Run creation on the wire.** The proxy mints the run id (AD-25 format) for an unknown identity and forwards immediately. The Run row is created by the store writer from the first per-call record, and the snapshot picks it up on the next publish. An **announce** waits, bounded at 1 s, for the writer to acknowledge that the Run row and token hashes are committed, before the proxy returns the credentials. On timeout it returns the typed retryable error and issues nothing.

### 9. MEDIUM: snapshot scale, Run-registry residency and lock order are unstated (AD-25, AD-17, AD-18)

- **Evidence.** AD-25 publishes "an immutable snapshot behind an atomic pointer swap" containing the "Run index, token hashes, resolved Policies, dispositions, latches", republished "after every commit that changes what the data path needs". Wire discovery creates Runs per first call, and NFR-4 targets 1M Runs (10M per Hub).
- **What is missing.**
  - A full copy-on-write rebuild is O(Runs) per commit, and nothing bounds it.
  - Nothing says which Runs are resident. Ended Runs, for example, need not be in the snapshot.
  - "The **Run registry** lives in the engine core, separate from `Supervisor`" adds state without saying what guards it. AD-17 fixes exactly "two coarse mutexes ... acquired registry-first" (`engine.rs:243-250`), so a third lock with no stated order invites deadlocks between 21-1 (Run rows at supervised start, under `Mutex<Supervisor>`) and 24-4.
- **Fix.** Add to AD-25:
  > The snapshot holds only non-ended Runs, and their tokens, dispositions and latches, in sharded persistent maps. A publish copies only the shards it changes, so its cost is O(changed entries), and 24-4's benchmark measures it. The Run registry is a cache over state.db. Its lock (or the writer task that owns it) is acquired **after** `Mutex<Registry>` and `Mutex<Supervisor>` and is never held across them. List reads go to state.db through the new port, never to the in-memory registry.

### 10. MEDIUM: hold semantics contradict FR-55, and the hold "bound" is undefined (AD-26)

- **Evidence.**
  - AD-26: "held calls in the outgoing process end with a retryable error and are recorded". FR-55: "During a Handoff Mode restart ... held calls stay held, ... each Run sees at most one such error per restart". A Run with N held calls gets N errors, and holds that "stay held" are lost.
  - AD-26 also defines hold as "an async wait with a bound" without naming the bound or what happens when it expires.
  - FR-56 says a held call waits until release and that Hekma records the *client's* timeout. A Hekma-side time bound that fails held calls acts on a Run without a verb, which NFR-9 rules out.
- **Fix.** Pick one option and write it into AD-26:
  - (a) The outgoing process passes held connections (fd plus the buffered request) over the handoff channel, and the successor re-queues them in arrival order.
  - (b) An explicit PRD amendment: FR-55 says held calls get one retryable error per Run.

  And replace "an async wait with a bound" with:
  > a wait with no time bound (only release, reroute, reject, or the client's own timeout or disconnect ends it), bounded in *count and bytes* per Run and per proxy. A held call beyond the count bound is refused with the typed retryable `hold-full` response and recorded as an Anomaly.

### 11. MEDIUM: no persisted home for holds, latches, per-Run and Caller limits, bindings, routes and virtual keys (AD-27, AD-25)

- **Evidence.**
  - AD-27's state.db list is "`runs` ..., `run_labels` ..., `policies`, usage rollups ... and the **outbox**". The Run row holds "{run id, Control Depth, Run Status, ... Lease expiry, timestamps}".
  - Latches live in memory today, on `Supervised.breached_scopes` (`domain/supervisor/mod.rs:656-675`).
  - FR-55 requires "Run records, budgets, Leases and holds survive a crash ... at every Control Depth". FR-49 requires that "a latched Breach Action stays latched across the change" of depth. FR-50 has `hekma budget` set limits "on the Runs a Selector matches".
  - Nothing says where per-Run explicit limits, Caller-set limits, latches, holds and dispositions, endpoint and virtual-key bindings, or routes are stored. Nor does it say whether `hekma budget` on a supervised Run writes the instance `config.toml` or a Run row.
- **Fix.** Extend AD-27's list:
  > ... `run_limits` (run id, limit key, value, source ∈ operator|host|caller), `run_latches`, `holds` (with the per-call queue metadata, never bodies), `dispositions`, `endpoints`, `credentials` (hash, class, prefix, run id, expiry, key id), and `routes`. Every row the snapshot carries is committed before the snapshot that carries it is published. `hekma budget` on any Run writes `run_limits`, and never an instance `config.toml`.

### 12. MEDIUM: the `usage_events` change is not additive; retention contradicts append-only; cross-file atomicity is unstated (AD-27, AD-6)

- **Evidence.**
  - AD-27: "`usage_events` gains a nullable instance id and a per-call id dedup key". The column is `instance_id INTEGER NOT NULL REFERENCES agent_instances(id) ON DELETE CASCADE` (`store/sqlite.rs:105-113`), with `UNIQUE(instance_id, run_id, sequence)` (`:150-152`). SQLite cannot relax NOT NULL by `ALTER`, so this is a table rebuild. With a nullable column, NULLs never collide in that index, so instance-less replays escape dedup. The cascade deletes Run history when an instance is unregistered.
  - FR-49 compacts ended Runs into rollups after retention, but AD-6 calls the ledger "append-only". AD-27 does not say whether compaction deletes `usage_events`, or how the outbox in state.db is bounded. Today it is 10MB × 3 with surfaced eviction and a single ack cursor in `outbox-ack.json` (`paths.rs:46-58`).
  - A per-call record in the calls store and its usage event in state.db are two SQLite files, and WAL gives no atomic commit across attached databases.
- **Fix.** Add to AD-27:
  > `usage_events` is rebuilt (one migration step) with `instance_id` nullable, `ON DELETE SET NULL`, and the dedup key `UNIQUE(run_id, call_id)` for proxy rows, keeping `(instance_id, run_id, sequence)` for self-reported rows. Unregistering an instance never deletes Run history. At ended-Run retention, a Run's `usage_events` are folded into its rollup and deleted: the ledger is append-only *within* retention. The outbox table carries its ack cursor as a row, and evicts acknowledged rows past a bound (surfaced, as today). The state.db usage event is authoritative. A calls-store record is written after it, keyed by call id, and is best-effort detail.

### 13. MEDIUM: bypass detection (FR-54) has no architecture

- **Evidence.** The capability map's FR-54 row is "credential resolver + proxy + `SecretResolver` engine-held class | AD-22, AD-26". The proxy cannot see traffic that bypasses it. FR-54 requires per-OS honesty: "Where an OS gives no reliable signal ... detection is unavailable on that OS". The Platform-code convention confines OS code to `backends/`. No AD decides:
  - where detection runs (the Shim, which already owns the process tree, is the natural place);
  - the mechanism per OS (socket-table polling of the process tree on Linux and Windows, macOS availability);
  - where the known-provider host list comes from, and how hostnames resolve to the addresses observed;
  - how a finding reaches the core (a Shim RPC event into the pipeline as an Anomaly).
- **Where it diverges.** 22-4 per OS. A Linux design and a Windows design will produce incompatible Anomaly shapes and different "unavailable" honesty.
- **Fix.** Add a bullet to AD-23:
  > **Bypass detection** runs in the Shim's OS backend. It periodically samples the outbound connections of the process tree and matches them against a provider-host list shipped in the engine, which the Operator can extend. A match is pushed to the engine as a `bypass` event, which the pipeline records as an Anomaly or as the `supervised (no wire)` informational note. Each backend declares `bypass_detection: available|unavailable` per OS in the effective capability view (AD-4). Update the capability-map row to AD-23.

### 14. MEDIUM: the streaming-buffering deferral conflicts with FR-51's "only integration needed" for streaming clients (AD-26, Deferred)

- **Evidence.** AD-26 says "responses are still buffered whole before relay ... Incremental SSE relay stays Deferred with a named trigger (mid-stream hold, time-to-first-token)".
- **What breaks.** FR-51 makes the shared proxy the zero-code path for arbitrary framework clients. A streaming client then gets its whole stream at the end, and clients with first-byte or idle read timeouts fail on long generations. That is Hekma being the reason a flow fails, which goes against §1 and NFR-9.
- **Why the deferral is unnecessary.** The spine itself notes hold and reroute "act before forwarding, so they do not need incremental relay". Tee-parsing an SSE stream to extract usage is what `parse_openai_sse_usage` already does over a buffer.
- **Fix.** Move incremental relay out of Deferred:
  > Responses are relayed incrementally. The proxy tees the stream through the usage parser and records the call when the stream ends. Whole-body buffering remains only for non-streaming responses up to the existing size cap.

  If this is kept deferred, the docs and FR-51 must say streaming clients see delayed delivery. That would need a PRD change.

### 15. MEDIUM: the operational envelope is incomplete, and there is no Open Questions section

- **Evidence.**
  - The spine has no Open Questions section at all, so items that are open by ruling have no home:
    - D4: per-environment Scent-Check defaults, "Open; decide in 22.3's spec";
    - the FR-58 topology;
    - the defaults for ended-Run retention, Lease and silent windows, Run Key lifetime, and the label-cardinality cap, which PRD assumptions delegate to story specs.
  - Undecided operations:
    - backing up state.db and the calls store while a host is live;
    - how the store writer behaves on a full disk (stop recording vs refuse calls), which is an NFR-9 question;
    - the TLS certificate and key configuration shared by the Control API and the proxy (one flag set or two, reload on rotation);
    - how the custodied-key cache (AD-10 amendment) learns that `secrets.toml` rotated;
    - the deployment shape: the systemd unit and Windows service that Handoff Mode's prerequisites assume.
- **Fix.** Add a section:
  > **Open Questions (owner, decide-by).**
  > - D4 Scent-Check defaults per environment (22-3 spec).
  > - Hub topology (Epic 25 / AD-24).
  > - Defaults for retention, Lease, silent window, Run Key lifetime and cardinality cap (each in its story spec, recorded back here).
  >
  > **Operations (decided):**
  > - Backup uses SQLite's online backup API through `hekma state backup`, never a file copy.
  > - On disk-full, the writer keeps forwarding, counts unrecorded calls (finding 5) and raises an Anomaly. It never refuses a call.
  > - One TLS config (`--tls-cert`, `--tls-key`, reloaded on SIGHUP) serves both listeners.
  > - The custody cache revalidates the secrets file's mtime with a bound of at most 5 s.
  > - The docs ship a reference systemd unit (`KillMode=process`, socket activation) and a Windows service definition.

### 16. MEDIUM: the CLI-surface convention does not cover Run verbs or the `hekma shim` mode (Conventions, AD-23, PRD §7)

- **Evidence.**
  - The Naming row says new operator capabilities are "a NOUN GROUP under `kt agent` ... never a new top-level `kt <noun>`". The pivot adds top-level `hekma ps`, `top`, `budget`, policy verbs, and hold, release, reroute and reject.
  - AD-23 adds "`hekma shim`, a subcommand mode of the same binary". Under PRD §7 item 3, every command name and flag becomes a compatibility surface. Without a carve-out, the Shim's internal arguments are frozen public API, and Hosts may script it.
  - New fail-fast outcomes need exit codes: "fails fast naming the depth", set-token mismatch, `no-route`.
- **Fix.** Add a convention row:
  > Run-scoped verbs are top-level (`hekma ps|top|budget|policy|hold|release|reroute|reject`), take `--selector`/`--limit`/`--cursor`/`--dry-run`/`--yes` uniformly, and map onto the frozen 0-6 exit codes: depth fail-fast and `no-route` → 5; stale set token → 4. `hekma shim` is hidden and internal: its arguments and RPC are versioned by the Shim protocol (finding 4), not by the CLI surface.

### 17. LOW: token details. `sha2` is not in-tree for the engine; hash input, encoding and a non-secret key id are unset (AD-22)

- **Evidence.**
  - AD-22: "stored only as a SHA-256 hash in state.db (`sha2`, in-tree)". `sha2` is a dependency of the `hekma` CLI crate only (`crates/hekma/Cargo.toml:42`, `cargo tree -i sha2`). The engine deliberately carries a std-only SHA-256 instead (`crates/hekma-engine/src/sha256.rs:1-9`, "The spec forbids new dependencies ... rather than pulled in as `sha2`").
  - FR-43 per-call records carry "Run Key identity". Nothing says what identity is recorded (it must not be the token or its full hash), whether the hash covers the prefix, or how it is encoded.
- **Fix.** Replace the parenthetical with:
  > hashed with the engine's `sha256` module over the full token string including its prefix, stored as lowercase hex. Records, logs and events carry only the **key id**, the first 12 hex characters of that hash.

  Name the randomness source too ("from `getrandom`, already in the engine tree via `ring`").

### 18. LOW: stale or contradicted passages left by the amendment

- Structural Seed (lines 459-477): no `hekma-host`, no `shim`, `src/metering/ # pipeline + loopback listener (AD-7)`.
- The second component diagram still shows `AG2 -.base_url.-> MET` per instance.
- The Stack table omits axum, hyper and rustls, all of which are in-tree and ratified.
- AD-20: "the Metering Proxy listener, AD-26, is the one engine-owned socket". AD-23/AD-26 now add engine-owned Shim client sockets and the handoff channel.
- Capability-map row "Engine concurrency / lock model ... AD-17" omits AD-18/AD-25/AD-27.
- **Fix.** Update the seed tree (`src/metering/ # shared Metering Proxy (AD-26)`, `src/shim/`, `crates/hekma-host/`), the diagram edge, the Stack rows, the AD-20 parenthetical ("the Metering Proxy listener (AD-26) and the Shim and handoff channels (AD-23) are the engine-owned sockets"), and the map row ("AD-17, AD-18, AD-25, AD-27").

## Rubric sweep

| Criterion | Result |
| --- | --- |
| Fixes the real divergence points for Epics 21-25 | Partly. Fixed: Run identity and format, the separate Run registry, the Policy resolution function, depth projection, outbox ordering, keyset reads, the token format, custody delivery, the Shim ownership split, and OTLP v2. Missed: findings 1, 2, 5, 6, 7, 8, 11 and 13. |
| Every AD Rule enforceable and prevents its divergence | AD-7's upgrade rule cannot fire (3). AD-26's "bound" and hold behaviour are unspecified (10). AD-27's "nullable" is not additive (12). The rest are enforceable. |
| Deferred cannot let units diverge | The external DB, Hub (reserved AD-24), traces and OS attestation are fine. Incremental SSE relay is a deferral with user-visible breakage (14). |
| Ratifies brownfield | Mostly. Recorded drift (AD-4/5/7/12) is honest. Contradictions: multi-process engines (1), the `sha2` claim (17), and the engine-defaults layer (3). |
| Covers FR-49..FR-60 | FR-49/50/53/55/56/57/59/60 covered. FR-51/52 lack OTLP intake (7) and Run creation on the wire (8). FR-54 has no mechanism (13). FR-58 is reserved by ruling. |
| Operational envelope decided, deferred or open | Not met: upgrades and handoff (4), backup, disk-full, TLS config, deployment unit, and no Open Questions section (15). |
