# Spine Review — adversary lens, control-plane amendments (2026-10-30)

*Lens: build pairs of units one level down (stories 21.1 Run registry, 21.2 `hekma ps`, 21.3 Selectors on verbs, 21.4 `hekma budget`, 22.1 Run Keys, 23.1 wire discovery, 23.2 self-announce, 23.4 wire hold/reject, 24.1 Shim, 24.2 Handoff Mode, 24.3 registry completeness, 24.5 `otlp-metrics v2`, the Control API) that each obey every AD as written and still build things that do not fit together. Each pair that works is a hole, and each hole comes with the AD text that closes it. Inputs: the spine as amended 2026-10-30 (working-tree diff), `.memlog.md` (Islam's rulings of 2026-10-30), the PRD (§3, §4.14, FR-21/41/43, NFR-4/9, §6, §7), proposal §4.2, and the code under `crates/`. Ground rules: the `ktesio-*` names are not counted, rationale is not asked for, and an item in Deferred counts only if it lets two units diverge.*

**Verdict: 16 constructions work (1 critical, 7 high, 6 medium, 2 low). Most of them come from one gap. The amendment adds three new runtime owners: the published snapshot, the store writer and the Shim. It adds a new entity, the Run, which has no Agent Home. But it never says which owner holds the Run's mutable governance state (latches, holds, dispositions, Run limits, Status, Anomalies), what each owner may write, or how a commit reaches the snapshot. Each story can therefore make its own choice and still meet the letter of every AD. Closing F1–F6 removes most of the other constructions with them.**

---

## F1 (critical): several engine-owning processes share one state directory, but the new single owners are per process

**Pair:** the `hekma` CLI (21.2 `hekma ps`, 21.3 Selectors on verbs, 21.4 `hekma budget`, 22.1 revoke) running against the same state dir as a live `hekma serve` (AD-20, AD-26, AD-27).

**Evidence.**
- AD-26: "exactly **one** Metering Proxy listener per engine-owning process — the Service Host when it runs, else the CLI's engine for its lifetime — on a fixed port".
- AD-27: "One **store writer task** with its own connection".
- AD-25: "the core publishes an immutable snapshot behind an atomic pointer swap".
- AD-22: "Revocation deletes the row and republishes; the next call fails."
- AD-20: "one Service Host owns a state directory, enforced by a lock". That lock guards only `hekma serve`.
- Code: `crates/hekma-engine/src/domain/outbox.rs:32-35`: "Two engines on one state dir (the host lock only guards `hekma serve`) each hold their own counter and can interleave". `crates/hekma/src/cli/agent.rs:1290` (`open_engine()` followed by `engine.blocking().start(name)`): every CLI command opens its own engine. The CLI has no Control API client.

**Construction.** While the Service Host runs, an Operator runs `hekma budget raise --selector team=a`, which is 21.4 and goes through its own CLI engine. The CLI engine commits the new ceiling and the latch re-arm to state.db, then republishes *its own* snapshot. The Service Host's proxy keeps reading the old snapshot, so the Runs stay rejected. A `hekma run revoke` behaves the same way: the row is deleted, but the Service Host's in-memory hash index still accepts the key. FR-53 and AD-22's "the next call fails" are both broken, and the result is a security defect. Two CLI commands that run at once each try to bind the fixed proxy port. State.db ends up with two store writers, which is exactly the "more than one writer" AD-27 says it prevents. Events the CLI engine commits never reach the Service Host's SSE bus. Each story obeys every AD as written.

**Fix: add to AD-20 (Ownership) and restate in AD-26/AD-27:**
> **One engine owner per state directory.** While a Service Host holds the state-dir lock, it is the only engine-owning process for that directory: it alone binds the Metering Proxy, runs the store writer and publishes the snapshot. A `hekma` command that finds the lock held acts through the Service Host's Control API (loopback, operator bearer) and never opens an engine on that directory. Read-only commands may open the database read-only. Without a Service Host, the CLI engine takes the same lock for the duration of the command, so two engine-owning processes never share a state directory.

---

## F2 (high): the store writer and the AD-7 pipeline give two commit paths, two evaluation sites and two durability rules

**Pair:** 19-4 / 23.1 (the proxy sends per-call records to the store writer, AD-27) against the self-reported and supervised usage path (`Supervisor::enforce_budget` under `Mutex<Registry>`, AD-7). Story 24.4 adds a third variant when it moves rollups.

**Evidence.**
- AD-7: "`MeteringSource` → UsageEvent → ledger transaction → `BudgetEvaluator` (inside the same commit path) … No other code path may mutate the Usage Ledger".
- AD-6: "one transaction per usage event (durability loss bound ≤1s …)".
- AD-27: "One **store writer task** with its own connection batches transactions; the coarse mutexes no longer guard per-call writes."
- Code: `Registry { store: SqliteStore }` owns the single connection (`crates/hekma-engine/src/domain/registry.rs:255-258`). Budget evaluation runs in `domain/supervisor/usage.rs:950-1060` under the supervisor lock.

**Construction.** 19-4 sends the proxy's per-call usage to the writer, which batches it and runs no evaluator, because the evaluator lives in the supervisor. A wire Run therefore never breaches. Story 23.4 could instead have the writer call `governance::decide` itself. That makes two evaluation sites with different latch sources: the in-memory `Supervised.breached_scopes` and whatever the writer reads. Two connections now write `usage_events` and the rollups. "Batches transactions" contradicts AD-6's "one transaction per usage event", and no rule says which one wins.

**Fix: replace AD-27's writer sentence with:**
> One **store writer task** with its own connection is the only writer of state.db and the calls store. The `Registry` connection becomes read-only. Every mutation is a message to the writer, including lifecycle transitions, breach records and outbox appends. Usage from every Metering Source, whether proxy, self-reported or reconciliation, enters the AD-7 pipeline in the writer: the ledger insert, the rollup update, `BudgetEvaluator` with the Run's latches, the breach record and the outbox append run in **one** transaction. The writer may batch several usage events into one transaction; the durability bound stays ≤1s (this supersedes AD-6's one-transaction-per-event wording). A BreachDecision that needs a process action is posted to the supervisor after commit; a wire action is published in the snapshot (F3).

---

## F3 (high): more than one publisher writes the snapshot, and nothing says when a commit becomes visible

**Pair:** the core under `Mutex<Registry>` (21.1 Run create, 22.1 revoke) against the store writer (breach → `block` disposition, AD-7 amendment) and 23.4 (hold / release verbs).

**Evidence.**
- AD-25: "after every commit that changes what the data path needs … the core publishes an immutable snapshot behind an atomic pointer swap".
- AD-7 amendment: "`block` is a disposition in the proxy's snapshot (AD-26), published by the pipeline".
- AD-22: "Revocation deletes the row and republishes; the next call fails."

**Construction.** The pipeline (in the writer task) and a revoke (in the core) each load the current snapshot, apply their own change and swap the pointer. One change is lost: either the key comes back, or the block disappears. Story 22.1 might publish asynchronously after it returns. In that case "the next call fails" is false for a window of unstated length, and 23.4's `release` can return before the proxy stops holding calls.

**Fix: add to AD-25 (The published snapshot):**
> Exactly **one** snapshot publisher exists per engine owner. It is the store writer: it applies each committed change to the snapshot in commit order and swaps the pointer once per transaction, and the snapshot records the highest outbox seq it reflects. No other task constructs or swaps a snapshot. Any change the data path must honour on the next call (revocation, hold, release, reject, re-arm, Policy change) returns to its caller only after the snapshot that contains it has been swapped.

---

## F4 (high): latches, holds, dispositions and Run limits have no durable home and no single owner

**Pair:** 21.1 (the Run row) and 21.4 (`hekma budget` sets Run limits and re-arms latches) against 23.4 (holds and dispositions) and 24.3 ("leases, holds, dispositions journaled"), with the existing supervisor latch on the other side.

**Evidence.**
- AD-25's Run row is "{run id, Control Depth, Run Status, optional parent run id, optional instance id, Lease expiry, timestamps}", and its snapshot carries "dispositions, latches". AD-27's state.db list is "`runs`, `run_labels`, `policies`, usage rollups …, the **outbox**". None of these holds latches, holds, dispositions, Caller-set limits or explicit Run limits.
- AD-25: "explicit Operator/Host limits on the instance or Run first … Caller-set limits apply only while tighter". The source of a Run limit is never stored.
- PRD FR-55: "Run records, budgets, Leases and holds survive a crash … at every Control Depth". PRD FR-49: "a latched Breach Action stays latched across the change".
- Code: the latch is `Supervised.breached_scopes`, held in memory and "RESETS automatically when a new Run starts" (`domain/supervisor/mod.rs:656-675`). On adoption it is rebuilt from the instance's Agent Home `breaches.log` (`domain/supervisor/reaper.rs:1659-1666`). A wire or announced Run has no Agent Home.

**Construction.** Story 21.1 keeps the supervisor latch, which AD-25 permits: Supervisor "keeps only supervised process handles", and nothing says the latch moves. Story 23.4 keeps holds in proxy memory. Story 24.3 journals dispositions to a new file. Story 21.4 stores Run limits as JSON on `runs`, with no record of who set them. After a crash every wire latch is lost, so a `block` re-fires or quietly stops applying. A supervised Run's latch now has two owners. Policy resolution cannot tell an Operator's Run limit from a Caller's.

**Fix: add to AD-27's state.db list and AD-25's Rule:**
> state.db also holds `run_limits` {run id, limit key, value, source ∈ operator|host|caller}, `run_latches` {run id, level run id, dimension, scope, action, latched_at} and `run_dispositions` {run id, reason, kind ∈ hold|reject|reroute, route, set_by, set_at}. These, and the Lease, belong to the Run registry alone, which writes them through the store writer (F2). The supervisor keeps no latch. AD-7's evaluator reads latches from the Run registry, and re-arming deletes the latch row in the same transaction as the ceiling change.

---

## F5 (high): a Restart Policy relaunch keeps the Run, but self-reported dedup still keys on the agent's per-process sequence

**Pair:** 21.1 ("a Restart Policy relaunch keeps the Run", AD-25) against the self-reported ledger dedup (story 3-1, which still applies to `supervised (no wire)` Runs such as Hermes).

**Evidence.**
- AD-25: "a Restart Policy relaunch keeps the Run".
- AD-7: "dedup keys on the call id". That covers proxy calls only; self-reported usage has no call id.
- Code: `CREATE UNIQUE INDEX … ON usage_events(instance_id, run_id, sequence)` (`store/sqlite.rs:151`). The agent stamps its own per-Run `sequence` (`ports/usage_source.rs:205`), and a new agent process starts that sequence again at 0. Today `RunId::mint()` runs on every respawn, relaunches included (`domain/supervisor/spawn.rs:690`), and that is the only thing keeping the key unique.

**Construction.** Story 21.1 stops minting a new id at relaunch, as AD-25 requires. After a crash the relaunched Hermes reports `sequence` 0, 1, 2 …. Those rows collide with the earlier process's rows under the same `(instance, run)` and are dropped as replays. Usage is undercounted without any warning: the same class of defect as the memlog's adoption sequence reset, now on every relaunch.

**Fix: add to AD-7 (after "dedup keys on the call id"):**
> Self-reported usage dedups on (run id, process generation, sequence). The process generation increments with every spawn under a Run, relaunches included, is committed with the spawn record (AD-5) before launch, and is stamped on each self-reported UsageEvent. No dedup key may rely on a value that restarts when the process restarts.

---

## F6 (high): how the proxy attributes a call to a Run conflicts with engine-minted run ids, with FR-51's split, and with itself

**Pair:** 23.1 (wire discovery) and 23.2 (announce header or OTel attribute) against 21.1 (the run id format) and 22.2 (virtual provider keys).

**Evidence.**
- AD-25: "**Run ids are globally unique random IDs (ULID-style …)**".
- AD-26: "identify the Run (Run Key, virtual provider key, identity header, endpoint binding — in that order)".
- PRD FR-51: "their traffic forms one Run per bound identity until a per-run identity (a per-call identity header or an announcement …) splits it". PRD FR-51 also says "A discovered Run appears … under the identity it presented". PRD FR-52 adds "discovered … and later announced under the same identity stays one Run".
- AD-25 and AD-26 also forbid the proxy from writing: "The Metering Proxy reads only the snapshot".

**Construction.**
1. A LangGraph helper puts its own thread id in the identity header. Under one reading, story 23.1 uses that string as the run id; under another, it mints a ULID and throws the client's id away. In the first case the ids are neither ULIDs nor globally unique. In the second, the same identity can never be found again, so FR-52's "stays one Run" cannot hold.
2. A client sends a virtual provider key together with a per-call identity header. AD-26 checks the vk first, so the header is never read and FR-51's split cannot happen.
3. The first call under a new identity has no Run in the snapshot. The proxy cannot create one (it has no write path), and the next call arrives before the snapshot is republished. Two Runs get created for one identity, or the per-call record has no run id at all.

**Fix: replace AD-26's identification clause and add to AD-25:**
> Attribution has two parts. The **credential**, which Scent-Check reads, is a Run Key or a virtual provider key. The **Run** is chosen by: the Run Key's Run; else the identity header's id *scoped under* the binding it arrived with (vk or endpoint), which yields its own Run that inherits that binding's bound labels; else the binding's Run. A client-presented id is an **external identity key** and is never a run id. state.db holds `run_identities` {scope, external key → run id}, with UNIQUE(scope, key). The proxy never mints run ids. It sends the per-call record carrying the external key, and the writer creates the Run if it is absent (upsert on `run_identities`) in the same transaction as the record. Calls that arrive before the snapshot is republished resolve to the same row.

---

## F7 (high): a supervised Run without key custody has nothing that identifies it on the shared proxy

**Pair:** AD-26 (the shared proxy replaces `ObservedListener`, and supervised instances receive "its address through the existing `metering.base_url` override") against 22.1 (issuing a Run Key at a supervised start) and 22.3 (Scent-Check `enforce`).

**Evidence.**
- AD-26: "It replaces the per-instance `ObservedListener`; supervised instances get its address through the existing `metering.base_url` override."
- AD-22: "Scent-Check … a valid Run Key or virtual provider key is an identity; an endpoint-path label binding is not."
- Code: the override is the bare `http://127.0.0.1:<port>` (`metering/listener.rs:374`). Calls are attributed by which socket they arrive on, one listener per instance (`domain/supervisor/mod.rs:677-682`). The built-in mapping sends `metering.base_url` → `OPENAI_BASE_URL` (`adapter/builtin.rs:222`). An unmodified agent sends only `Authorization: Bearer <its real key>`.

**Construction.** Story 22.1 issues the supervised Run's Run Key into an environment variable that the agent never reads, so its calls carry no Run Key, no vk and no header. Through the bare shared address the proxy sees them as unattributed. Under `enforce` they are refused, and NFR-9 forbids that stop. Otherwise they land on the default Policy instead of the instance's Run. Another story could put a per-instance path into `base_url`. But AD-22 says a path binding is not an identity, so `enforce` would still refuse it.

**Fix: add to AD-26:**
> For a supervised Run, the injected `metering.base_url` carries the Run's credential in its path (`http://<proxy>/r/<run-key>/…`). The proxy accepts a path-carried Run Key as a Run Key identity, so Scent-Check counts it, and strips it before forwarding. A relaunch receives a fresh key in the path (AD-22). Endpoint-path *label* bindings stay non-identities.

---

## F8 (high): the spine contradicts FR-55 on held calls during a Handoff Mode restart

**Pair:** 24.2 (Handoff Mode restart, tested against PRD FR-55) against 23.4 (holds, built to AD-26).

**Evidence.**
- AD-26 (Restart): "held calls in the outgoing process end with a retryable error and are recorded".
- PRD FR-55: "During a Handoff Mode restart the Metering Proxy refuses no connection: **held calls stay held**, a call in flight either completes or its client gets a retryable error … each Run sees at most one such error per restart".

**Construction.** Story 23.4 keeps held calls as accepted connections inside the outgoing process, and AD-26 then fails them. Story 24.2's acceptance test is written from FR-55 and asserts that they stay held. Each story is correct against the document it follows, and the two cannot both pass. Islam's ruling in the memlog covers the listening socket only, so the conflict needs an explicit decision rather than an editorial fix.

**Fix: pick one and record it.**
- (a) **Amend the spine** so it matches the PRD: "On a graceful restart the outgoing process also passes every held call's accepted socket and its buffered request bytes to the successor (`SCM_RIGHTS` / `WSADuplicateSocket`). The successor re-queues them in arrival order under the persisted hold."
- (b) **Amend PRD FR-55** to "the hold stays in place; each held call ends with one retryable error, and the client's retry is held again by the successor".

Until one of these is ratified, 24.2 cannot be specified.

---

## F9 (high): Run Status and Anomaly flags have several writers, none named, and nothing stores the flags

**Pair:** 21.2 (an exception-first `hekma ps` over 1M Runs, which needs indexes on status and anomaly) against the AD-15 projection (supervisor transitions), 23.3 (Lease expiry → `silent`), 22.3 (the proxy's unscented Anomaly), 22.4 (bypass) and the policy fallback (FR-60).

**Evidence.**
- AD-25: the Run row holds "Run Status".
- AD-15 amendment: "a supervised Run's Run Status is projected from Lifecycle State".
- Nothing in the spine stores Anomaly flags, and the Hot path convention says the proxy uses "no SQLite".
- PRD FR-50: "The default view is exception-first: Runs with an Anomaly or a `held`, `silent` or `stale` status are listed". The page must come back in under 1s at 1M Runs (NFR-4).

**Construction.** Story 21.2 computes Status when it reads. That cannot be indexed, so the exception-first page scans everything. Story 23.3 adds a stored `status` column that a sweeper sets to `silent`. Supervised Runs go on being projected live from `agent_instances.state`, so `hekma ps` and `/v0/runs` disagree about one Run. The proxy has no route by which to set an "unscented" flag.

**Fix: add to AD-25:**
> Run Status and Anomaly flags are **stored** on the `runs` row and indexed, and the Run registry is their only writer, through the store writer. Supervised projection (AD-15) is applied at each transition's commit, never at read time. A bounded Lease sweeper task sets `silent` and `ended` on time. The proxy and the bypass detector report Anomalies as records the writer applies. Every surface reads the stored values.

---

## F10 (medium): the Run registry has no lock and no lock order

**Pair:** 21.1 (Run registry "separate from `Supervisor`") against the AD-15 projection (a supervisor transition updates the Run) and 21.3 (`hekma stop <run>` on a supervised Run calls the supervisor).

**Evidence.**
- AD-25: "the **Run registry** lives in the engine core, separate from `Supervisor`".
- AD-17: "acquired registry-first" names only `Mutex<Registry>` and `Mutex<Supervisor>`.
- AD-18 (2026-10-30): "Run records sit in their own registry".

**Construction.** Story 21.1 gives the Run registry a `Mutex<Runs>`. A transition holds Supervisor and takes Runs to project Status. A Run verb holds Runs and takes Supervisor to stop the process. The two acquire in opposite orders and deadlock.

**Fix: add to AD-18's 2026-10-30 amendment:**
> The Run registry is the third lock and is always taken last: `Registry` → `Supervisor` → `Runs`. Code that holds `Runs` never acquires the other two. A Run verb that needs a process action releases `Runs` and calls the supervisor. Writes leave through the store writer (F2), so the lock covers in-memory bookkeeping only.

---

## F11 (medium): the verb × Control Depth table and set tokens have no owner in the core

**Pair:** 21.3 (Selectors on CLI verbs) against the Control API `/v0/runs/{id}/{verb}` handlers and 21.5 `hekma top` hotkeys.

**Evidence.**
- AD-20: "Multi-Run verbs resolve the matched set first and act on a set token … Handlers stay thin projections; Selector and Policy evaluation live in the engine core". The FR-49 verb × depth table is not assigned anywhere.
- Lists & identity convention: "Every list read and endpoint is keyset-paginated with a bounded page". PRD FR-50 says the Control API "returns [the matched set] with a token".

**Construction.** The Control API implements the FR-49 table in its handlers (for example, `stop` on a wire Run means reject). The CLI implements it again for the case without a Service Host, and the two copies drift on qualifiers. A set token minted in host memory does not survive a Handoff Mode restart. A matched set of 1M Runs cannot be "returned" in one response without breaking the pagination convention.

**Fix: add to AD-25:**
> The FR-49 verb × Control Depth table is one data table in the core, like AD-15, and every Run verb from every surface is dispatched through it. A **set token** names a frozen matched set that the core persists in state.db with a TTL. It is listed through keyset pages and redeemed only through the core, which reports a result for each Run.

---

## F12 (medium): who executes the Restart Policy, and where an exit goes, while no engine is connected to the Shim

**Pair:** 24.1 (Shim) against AD-15's Restart Policy executor and 24.3 (registry completeness).

**Evidence.**
- AD-23: "exit is pushed by the Shim instead of polled". Nothing says what happens when no engine is listening, which is the case during a handoff or in CLI-only mode.
- AD-22: "a Restart Policy relaunch gets a fresh one", meaning a refresh credential. Minting it needs state.db.

**Construction.** Story 24.1 lets the Shim relaunch the child on its own, so a crash during a restart does not wait. That Shim can mint neither a Run Key nor a refresh credential, and cannot commit the AD-5 record first. A competing design has the Shim write the exit to state.db directly, which adds a writer (F2). A third has the Shim exit along with its child, which loses the exit cause. Each meets AD-23 as written.

**Fix: add to AD-23:**
> A Shim never opens state.db and never relaunches. It holds the child's exit status and any unflushed log tail until an engine acknowledges them, and only then exits. The Restart Policy stays in the engine: an exit observed at reattach is relaunched there with backoff measured from the exit time, and every relaunch spawns a new Shim with its record committed first (AD-5).

---

## F13 (medium): "`failed` → `ended` unless relaunched" can end a Run and then restart it

**Pair:** the AD-15 projection against the AD-14 "Run ended" event and FR-49 compaction of ended Runs.

**Evidence.**
- AD-15 (2026-10-30): "`failed` → `ended` with an Anomaly unless relaunched".
- AD-14 (2026-10-30): "new event families for Runs (created, depth changed, status changed, ended)".
- AD-15's backoff waits up to 60s before relaunching.

**Construction.** Story 21.1 projects `failed` as `ended` and emits `ended`. 60s later the Restart Policy relaunches the process, and the "ended" Run becomes `active` again. Retention may already have compacted it in the meantime, or a consumer has closed out its books on it.

**Fix: replace the AD-15 projection clause with:**
> `failed` with a relaunch pending under the Restart Policy → `active`, with an Anomaly `restarting`. The Run becomes `ended` only when no relaunch will follow (Restart Policy `never`, crash-loop stop, or an operator `stop`), and a Run that has ended is never reopened.

---

## F14 (medium): child Runs and instance totals have no attribution rule

**Pair:** 21.1 (rollups "per Run and per instance") against 21.4 (`hekma budget` headroom) and FR-22 ("totals equal the Usage Ledger exactly").

**Evidence.**
- AD-7: "{run id, instance id when supervised, …}".
- AD-27: "usage rollups per Run and per instance".
- PRD FR-49: a child Run's usage "counts on itself and rolls up to every ancestor".

**Construction.** A child Run (depth announced) inside a supervised process writes rows with `instance_id` NULL, reading "when supervised" as the *Run's* depth. The per-instance cumulative then misses all child spend, while the parent Run's rollup includes it. Another story stamps the instance id on child rows, and the instance total double-counts against a rollup that sums each Run's subtree.

**Fix: add to AD-27:**
> A usage row carries the run id it is attributed to and the instance id of the nearest supervised ancestor, if there is one. The per-Run rollup holds the Run's own usage and its subtree usage separately. The per-instance rollup is the sum of the rows carrying that instance id. Every total a surface shows names which of these it is.

---

## F15 (medium): a Run without an Agent Instance has no Rate source, and OTLP v2 attributes have no source

**Pairs:** (a) the writer stamping a Rate on proxy usage (F2) against 21.4 Cost Caps on wire Runs. (b) 24.5 `otlp-metrics v2` against the AD-14 payloads.

**Evidence.**
- (a) Code: the Rate is stamped per row from the instance's resolved config (`store/sqlite.rs` SCHEMA_V4 doc, lines 157-170). PRD FR-20 also allows a rate "per model key". AD-25's resolution covers "Token Budgets, Cost Caps, Breach Actions and the Scent-Check Mode", but not Rates.
- (b) AD-21: the exporter "derives every metric from the existing versioned AD-14 payloads". The v2 amendment then requires "`hekma.control_depth`, `hekma.policy` … and only Operator-allow-listed label keys".

**Construction.** (a) A Cost Cap set by Policy on a wire Run is either inert (no Rate) or priced from a table each story invents for itself. (b) The usage and breach events carry neither depth nor Policy nor labels, so story 24.5 has the exporter resolve the Policy itself. That is a second evaluation site, which AD-25 forbids ("none re-implements it").

**Fix:**
> (a) AD-9: the engine-defaults layer gains `rates.<model>`. The Rate for a Run is its instance's Rate when it has one, else the model's Rate. The writer stamps it at commit, as today.
> (b) AD-14 / AD-21: at commit the core stamps usage and breach events with `control_depth`, the supplying Policy name for each limit, and the Run's bound labels. The exporter only filters these through the allow-list and never resolves Policy or reads labels.

---

## F16 (low): Run ids, "supervised (no wire)" and unattributed traffic have no wire form

**Pair:** 21.2 `hekma ps --json` against the Control API, 24.5 OTLP and 19-4 per-call records.

**Evidence.**
- AD-25: "ULID-style", with no canonical string form given. The "IDs & time" convention row is unchanged, and legacy `run-<nanos>-<nonce>` ids persist (`domain/usage.rs:66-73`).
- Control Depth has four values, but PRD FR-49 displays `supervised (no wire)`.
- PRD FR-51 lists unattributed traffic, while PRD FR-43 requires every per-call record to carry a run id.

**Construction.** The CLI uses lowercase ULIDs and the Control API path parser expects uppercase. One surface emits `control_depth: "supervised-no-wire"` and another emits `supervised`. Story 19-4 writes per-call records with a NULL run id for unattributed calls, while 23.1 creates a synthetic Run for them.

**Fix: add to the IDs & time convention row:**
> A run id is `run_` followed by a 26-character lowercase Crockford ULID. Legacy ids are accepted verbatim and never reissued. Control Depth serializes as one of four values, plus a separate boolean `wire_path`; `supervised (no wire)` is display text only. Unattributed calls are attributed to one reserved per-Node Run (`run_unattributed`, depth `wire`, never ended).

---

## Constructions tried that the spine already blocks

- **Two Policy resolvers** (the CLI resolving limits locally): blocked by AD-25, "one pure function … Every surface calls this function". The OTLP route around it is F15(b).
- **The proxy taking engine locks** to read a Run: blocked by the Hot path convention and by AD-25.
- **Signed self-contained Run Keys** in one story and opaque keys in another: blocked by AD-22's typed-prefix opaque tokens.
- **A second Run concept in the host:** blocked by AD-20, "Handlers stay thin projections".
- **The outbox order breaking across connections:** solved by the autoincrement in state.db once F1 and F2 leave a single writer.
- **Adoption by pid against the Shim socket:** blocked by AD-23, "adopt reconnects to the socket", together with the renamed ownership claim.
- **Policy matching Caller labels:** blocked by AD-25, "Policy Selectors match bound labels only".
