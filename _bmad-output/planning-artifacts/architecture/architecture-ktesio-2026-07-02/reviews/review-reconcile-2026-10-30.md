# Review — input reconciliation (2026-10-30 control-plane amendment)

- **Artifact:** `ARCHITECTURE-SPINE.md` (working tree, amended 2026-10-30)
- **Lens:** input reconciliation. Each PRD requirement in §4.14 (FR-49..FR-60), amended FR-21/FR-41/FR-43, NFR-4, NFR-9, §7 items 4–5, and each row of proposal §4.2 was checked for a governing AD/amendment and for agreement with the PRD.
- **Ground rules applied:** `ktesio-*` names are ruled (not findings); rationale absence is not a finding; Deferred items are accepted unless they let two units diverge. Memlog rulings (proxy in the engine-owning process with socket handoff; one Shim per supervised process; opaque hashed tokens; state.db + rotating calls store) are treated as decided — including the proposal's "Shim per Run" being superseded by "Shim per supervised process".

## Verdict

The pivot landed in outline: Run registry, Policy resolution, the published snapshot, the shared proxy, the Shim, token custody, storage, OTLP v2 and the proposal §4.2 rows are all present and mostly faithful. But several PRD requirements did not land, and four landed contradicting the PRD or contradicting each other:

1. The new engine-defaults entry `budget.breach_action = "warn"` makes the FR-21 upgrade write a no-op as written, and as an explicit FR-11 layer it outranks every Policy (FR-60).
2. The graceful-restart rule ends held calls with retryable errors. FR-55 says held calls stay held, with at most one error per Run per restart.
3. Proxy-path Run creation (discovered `wire` Runs, and the announce header that returns a Run Key) has no governing rule, and the hot-path rule forbids the write it needs.
4. Inbound OTLP (`observed` Runs, OTel-attribute announce, telemetry-sourced usage) has no AD at all.

Lease/Run Status for unsupervised Runs, child-Run hierarchy semantics, the verb × Control Depth matrix (including `supervised (no wire)`), the default Policy, retention/compaction and bypass detection are also missing. **Not ready to drive stories without amendment.**

## Coverage table

| Input | Governing AD | Status |
|---|---|---|
| FR-49 Runs | AD-25, AD-15 amend | Partial: Run row and projection landed. Child semantics, verb × depth matrix, `supervised (no wire)`, retention and latch-across-depth are missing (F6, F7, F15) |
| FR-50 listing/verbs | AD-20, AD-25, AD-27 | Landed (set token, keyset, Selector in core) |
| FR-51 discovered | AD-26 | Partial: Run creation on the proxy path, unattributed bucket, outbound identity headers and inbound OTLP are missing (F3, F4, F10) |
| FR-52 announce/Lease | AD-20 (routes only) | Partial: proxy announce-header channel, OTLP-attribute channel, Lease/silent/ended are missing (F3, F4, F5) |
| FR-53 Run Keys/custody | AD-22 | Landed; expiry check location unstated (F3) |
| FR-54 bypass | none (map row cites AD-22/26) | Missing (F14) |
| FR-55 continuity | AD-23, AD-26, AD-5 amend | Contradicted on held calls; crash-survival of holds incomplete (F2) |
| FR-56 hold/reroute | AD-26 | Partial: record fields, held count/age, end causes and a Hekma-imposed bound (F11) |
| FR-57 top | AD-20 | Landed |
| FR-58 Hub | AD-24 reserved | Accepted (reserved) |
| FR-59 OTLP v2 | AD-21 amend | Landed; default-Policy attribute and §7 deprecation window missing (F17) |
| FR-60 Policies | AD-25, AD-20 | Partial: default Policy content, refused-at-config cases, fallback count; contradicted by engine-default layer (F1, F7, F8) |
| FR-21 amended | AD-7 | Contradicted (F1); upgrade event and registration field missing (F13) |
| FR-41 amended | AD-20 | Landed |
| FR-43 amended | AD-7 amend, AD-26 | Partial: per-call record shape, child rollup (F6, F11) |
| NFR-4 | AD-18 amend, AD-27 | Partial: proxy ≤10 ms p99 not gated (F16); batching vs flush bound (F9) |
| NFR-9 | AD-23, AD-5 amend | Mostly landed; malformed-config fail-closed pause is off-list (F19) |
| §7 items 4–5 | AD-20, AD-21 | Partial (F17) |
| Proposal §4.2 AD-7/18/20/21/22/23/24 rows | as named | Landed (AD-23 per process by ruling) |
| Proposal §4.2 Deferred row | Deferred | Partial: traces/spans listed twice with conflicting triggers (F18) |

## Findings

### F1 — HIGH — The engine-defaults `breach_action = "warn"` entry defeats the FR-21 upgrade write and outranks every Policy

**Evidence.**

- AD-7, lines 88–89: "an engine-defaults entry `budget.breach_action = "warn"` makes the source visible. On first open after the upgrade, every instance whose four config layers carry no `budget.breach_action` gets `pause` written at the instance layer".
- AD-9 amend, line 412: "the engine-defaults layer gains `budget.breach_action = "warn"`".
- AD-9, line 101: the four layers are "engine-defaults < adapter (kind) defaults < Agent Home instance config < invocation overrides".
- Code: `crates/hekma-engine/src/domain/registry.rs:1086-1089` resolves `[engine, kind, instance_layer, overrides]`. The engine layer is one of the four (it is empty today, `registry.rs:56-61`).

Once the engine-defaults layer carries the key, "four config layers carry no `budget.breach_action`" is never true, so no pre-upgrade instance gets `pause`. That breaks FR-21's "every Agent Instance registered before the change that relied on the implicit default keeps `pause`".

Separately, FR-60 says "A limit set explicitly on an Agent Instance (any FR-11 layer …) outranks every Policy", and AD-25 says "explicit Operator/Host limits on the instance or Run first". A resolved `EffectiveConfig` always shows `breach_action = warn` with an engine-default source, so every instance has a "set" Breach Action and no Policy (nor the default Policy) can ever supply one. The CLI resolver and the proxy-snapshot resolver will diverge depending on whether the implementer treats the engine-default value as explicit.

**Fix.** Replace the AD-7 "Breach default and upgrade" bullet with:

> The code's `#[default]` moves to `warn`, but no config layer carries `budget.breach_action` by default: the ratified default comes from the **default Policy** (AD-25), which is the visible source (`source: policy:default`). On first open after the upgrade, every instance whose instance and invocation layers (and kind defaults) carry no `budget.breach_action` gets `pause` written at the instance layer, with a store marker … Only values in the kind-defaults, instance and invocation layers count as "set explicitly" for AD-25; the engine-defaults layer never carries a Policy-governed limit.

Then strike the AD-9 (2026-10-30) amendment sentence about the engine-defaults entry.

### F2 — HIGH — Graceful restart ends held calls (contradicts FR-55); crash-time hold and in-flight recording is unbuilt

**Evidence.**

- AD-26, line 307: "on a graceful restart or upgrade, the outgoing process passes the listening socket to its successor …; held calls in the outgoing process end with a retryable error and are recorded."
- PRD FR-55 says: "During a Handoff Mode restart the Metering Proxy refuses no connection: held calls stay held, a call in flight either completes or its client gets a retryable error …, each Run sees at most one such error per restart". A Run with N held calls gets N errors under the spine.
- Crash: FR-55 says "After a `kill -9` … the record of every hold survives; every held or in-flight call … ends with a retryable error …, recorded on its Run as ended by the crash once the Service Host restarts", and "Run records, budgets, Leases and holds survive a crash … within FR-10's flush bound".
- The spine persists holds only on SIGTERM. AD-23, line 288: "on SIGTERM the Service Host releases its Shim connections without stopping, persists holds and Leases, and exits". AD-26 emits a per-call record only after the call completes (line 306), so a call that was held or in flight at a crash has no record to mark "ended by the crash".

**Fix.** Replace the AD-26 Restart bullet's held-call clause with:

> The outgoing process stops accepting and keeps its accepted connections: held calls stay held and follow the successor's dispositions (release / reroute / reject) for them over the handoff channel until each completes. Only if the outgoing process cannot finish within the shutdown bound does each Run receive at most one retryable error, recorded.

Add:

> A hold (Run, start time, reason) is committed through the store writer when it is placed, not at shutdown, and each held or forwarded call writes a call-start entry before forwarding. After a crash, the restarted engine closes every open call-start entry as `ended: crash` on its Run.

In AD-23, change "persists holds and Leases, and exits" to "(holds and Leases are already durable, AD-27), and exits".

### F3 — HIGH — Run creation on the proxy path (discovered `wire` Runs, announce header returning a Run Key) is ungoverned and conflicts with the hot-path rule

**Evidence.**

- PRD FR-52 channel 1: "an announce header on the Run's first call through the Metering Proxy returns the Run Key and refresh credential in response headers". FR-51: "A discovered Run appears after its first call … under the identity it presented."
- AD-26's identification order, line 306 ("Run Key, virtual provider key, identity header, endpoint binding"), has no announce-header case and no "unknown identity → create Run" case.
- The Hot path convention, line 436, says code on the per-call path "reads only the published snapshot …: no engine mutex, no SQLite". AD-25 says the snapshot is republished only "after every commit".

Creating a Run and minting a Run Key requires a write. Then the Key must validate on the very next call, but the hash index lives only in the snapshot, which lags the writer's batch. Under `enforce` that next call would be refused. Run Key expiry (FR-53 "issues a Run Key that expires") also has no stated check location. A proxy story and a core story will each invent a mechanism.

**Fix.** Add to AD-26:

> **Run creation on the wire:** for an unknown identity, an identity header or an announce header, the proxy mints the run id and (for an announce) the Run Key and refresh credential locally (AD-22 format), records them in a proxy-local pending overlay that it consults after the snapshot, and sends a create record to the store writer. The overlay entry drops once a snapshot containing it is published. The announce header's response carries `hk_run_` / `hk_ref_` in response headers. Run Key expiry is a field of the snapshot's hash index and is checked in the proxy.

Add "announce header" to the identification order (first) and the capability-map row for FR-52.

### F4 — HIGH — Inbound OTLP (`observed` Runs, OTel-attribute announce, telemetry usage) has no AD

**Evidence.**

- PRD FR-51: "Hekma accepts inbound OTLP from clients; telemetry carrying a run identity creates or updates an `observed` Run".
- FR-52 channel 3: "an OTel resource attribute creates or updates the Run's identity and Lease but issues no Run Key".
- FR-19 amend: "`observed` Runs carry only the usage their own telemetry reports, labeled estimated".
- FR-49 table: "Budgets, `warn` | from its telemetry".
- The spine's only trace is the diagram edge `OBS -- OTLP --> API` (line 397). AD-21 is exporter-only ("subscribes read-only to the public event bus", line 231). AD-7 says "No other code path may mutate the Usage Ledger" except `MeteringSource` → pipeline. No AD names the receiver, its auth class, its mapping to a Run, or the `MeteringSource` it feeds.

**Fix.** Add an AD (or an AD-21/AD-25 amendment):

> The Service Host accepts OTLP/HTTP+JSON metrics on the Control API listener under the announce-scoped or bearer credential (AD-20 resolver). A resource attribute carrying a run identity creates or updates an `observed` Run and renews its Lease, and never issues a Run Key. Token counts from `gen_ai.client.token_usage` enter the ledger only through a `telemetry` `MeteringSource` → AD-7 pipeline, labeled `estimated`. Content-bearing signals are refused (PRD §6). Without a Service Host there is no OTLP intake.

Add it to the capability map for FR-51/FR-52.

### F5 — MEDIUM — Lease and Run Status for unsupervised Runs (`silent`, `held`, ended-by-silence, wire inactivity) are ungoverned

**Evidence.**

- The PRD Glossary Lease: "renewed by heartbeat or traffic; an expired Lease marks the Run `silent`, not gone. A `wire` Run has no Lease and goes `silent` after an inactivity window". Run Status: "A Run ends … when it has been `silent` longer than the silent window; ending is bookkeeping and never acts on a process". FR-52 restates these.
- The spine only stores "Lease expiry" in the Run row (AD-25, line 299) and projects status for supervised Runs (AD-15 amend, line 416).
- Nothing says whether `silent` is computed at read time or written by a sweeper. The status-changed event (AD-14 amend, line 415) implies a writer, but it is unstated who runs it under the CLI's short-lived engine. Nothing says traffic renews the Lease via the store writer, or that ending a Run by silence is bookkeeping (no disposition change).
- The `ps` reader and the event emitter can disagree on status.

**Fix.** Add to AD-25:

> **Run Status for unsupervised Runs is derived in the core:** `held` while a hold disposition is set; `silent` when `now > lease_expiry` (announced/observed) or `now > last_call + inactivity window` (`wire`); `ended` on an announced end or after the silent window. Reads compute it from the row; the Service Host's core runs one timer task that writes the `silent` / `ended` transitions (and their AD-14 events) through the store writer. A CLI engine computes on read and writes nothing. Traffic renews the Lease by a coalesced per-call update in the store writer. Ending is bookkeeping: it changes no disposition and never acts on a process.

### F6 — MEDIUM — Child-Run hierarchy semantics (FR-49, FR-43) did not land beyond the parent column

**Evidence.**

- AD-25, line 299, records only "optional parent run id (same Node, ≤ 8 levels, no cycles)". AD-7 amend, line 411: "Task is replaced by a child Run (AD-25)". AD-27 rollups are "per Run and per instance" (line 313).
- Missing PRD rules:
  - usage "rolls up to every ancestor, every ancestor's ceilings apply, and a breach executes the Breach Action of the level whose ceiling broke over the Runs beneath it";
  - `stop`/`pause` on the owning Run cascades to in-process children "each recording the cause (`parent-stopped`, `parent-paused`)";
  - a parent's end doesn't end children outside its process;
  - "A child Run opened inside a supervised process … is at most `announced`";
  - FR-43: "a call with no child's identity stays on the parent Run and is never split across open children".
- The AD-7 evaluator and the snapshot dispositions must agree on ancestor ceilings and descendant fan-out; unspecified, they will diverge.

**Fix.** Add to AD-25:

> **Hierarchy:** each UsageEvent is applied to its Run and every ancestor inside the same AD-7 commit (ancestor rollups in AD-27). The evaluator checks every ancestor's ceilings. A breach at level L publishes the disposition for L and every descendant. A verb cascades only through an explicit Selector/flag, except that stop/pause of a Run owning a supervised process applies to the child Runs in that process with cause `parent-stopped` / `parent-paused`. A child Run in a supervised process has depth ≤ `announced`. A call carrying no child identity is attributed to the process's own Run.

### F7 — MEDIUM — The FR-49 verb × Control Depth matrix and `supervised (no wire)` are absent; AD-25's blanket fallback contradicts "refused at configuration"

**Evidence.**

- FR-49 defines a verb × depth table ("Every cell is covered by a conformance test"). It also defines `supervised (no wire)`: "`block` set directly on it is refused at configuration (one inherited from a Policy follows FR-60's fallback)". For `observed`, a Breach Action "set directly: refused at configuration; from a Policy: FR-60 fallback".
- AD-25, line 300: "A Breach Action the Run's depth cannot execute resolves to `warn` plus an Anomaly". That applies to everything, so directly set values are silently degraded instead of refused.
- `grep "no wire"` on the spine returns nothing. There is also no statement that the matrix lives in one place (the AD-15 "is data" pattern), so the CLI, Control API and `hekma top` hotkeys can each encode it.
- FR-49's "a latched Breach Action stays latched across the change" is also unstated.

**Fix.** Add to AD-25:

> **The verb × Control Depth table is data in the domain core** (as AD-15's transition table), exhaustively tested and called by every surface. `supervised (no wire)` is a derived depth qualifier: supervised, Metering Source `self-reported`, no key custody. A Breach Action **set directly** that the Run's depth cannot execute (`observed`; `block` on `supervised (no wire)`) is refused at configuration with a typed error naming the depth. Only a Policy-supplied one falls back to `warn` + Anomaly. Latches survive depth changes.

### F8 — MEDIUM — The default Policy is referenced but never defined

**Evidence.** AD-25, line 300: "then the default Policy". The PRD (FR-60) says: "The default Policy sets no ceiling, Breach Action `warn`, and the environment's default Scent-Check Mode (§13 Q9); the Operator can change it." FR-60 also says "Creating or changing a Policy reports how many matched Runs fall back this way." The spine names neither the default Policy's content nor its storage (a row in `policies`? config?). It also does not say that the env-dependent Scent-Check default (D4, open) is a property of it, or that Policy writes return the fallback count. Combined with F1, the default Breach Action has two candidate sources.

**Fix.** Add to AD-25:

> The **default Policy** is a reserved row in `policies` (empty Selector, name `default`), seeded with no ceilings, Breach Action `warn` and the environment's Scent-Check default (decided in the FR-53 story; D4), and editable but never deletable. Policy create/change returns the count of matched Runs that fall back to `warn`.

### F9 — MEDIUM — The batched store writer silently replaces AD-6's per-event transaction and AD-7's commit-path evaluation

**Evidence.**

- AD-6, line 83: "one transaction per usage event (durability loss bound ≤1s …)". AD-7, line 88: "ledger transaction → `BudgetEvaluator` (inside the same commit path)".
- AD-27, line 313: "One **store writer task** with its own connection batches transactions; the coarse mutexes no longer guard per-call writes."
- The AD-6 amendment (line 410) only says "extended by AD-27". The batch bound is never tied to FR-10's flush bound, which FR-55 depends on ("within FR-10's flush bound"). Nothing says where the evaluator runs relative to the batch commit, which bounds FR-21's "≤ the metering interval + 1s". The pre-existing pipeline (under the coarse mutex) and the new writer can produce two evaluation orders.

**Fix.** Add to AD-27:

> The store writer commits at most every 1 s (FR-10 bound, superseding AD-6's per-event transaction). Each usage event in a batch is applied and evaluated (AD-7) inside that batch's transaction, in arrival order, and the resulting dispositions are republished (AD-25) before the next batch starts. All ledger writes, CLI and Service Host alike, go through this writer.

### F10 — MEDIUM — The proxy's outbound identity headers, breach response header and unattributed bucket are missing (AD-26 says only "strip")

**Evidence.**

- FR-51: "Identity headers (run id and labels, never content) are added to forwarded calls so a gateway in front of the provider … can attribute them; the Operator can turn them off". Also: "Under Scent-Check `off` or `shadow`, traffic with no identity is listed as unattributed".
- FR-21: a breach always emits "a response header where the call passes through the Metering Proxy".
- AD-26, line 306, says only "strip identity and credential headers". Read literally, nothing is added outbound and no breach header exists. An unidentified call with no endpoint binding has no defined Run.

**Fix.** Append to AD-26's per-call sequence:

> After stripping, add Hekma's own identity headers (run id + bound labels, never content; Operator-switchable, default on) to the forwarded request. On any response for a Run with a breached ceiling, add a breach header naming the ceiling and action. A call with no identity and no endpoint binding is attributed to the per-endpoint **unattributed** Run under `off` / `shadow` and refused (Anomaly) under `enforce`.

### F11 — MEDIUM — The per-call record and hold model miss PRD fields and add a Hekma-imposed hold bound the PRD does not list

**Evidence.**

- AD-26, line 306: per-call record "{call id, run id, Control Depth, model, status class, latency, tokens}".
- FR-43 also requires "Run Key identity once FR-53 lands, sub-second time". FR-56 says "Hold, release, reroute and reject … appear in per-call records". FR-56 also requires "`hekma ps` shows the Run as `held` with the held-call count and the oldest held call's age", and that a held call ending without release records "the client's own timeout, the connection dropping or the process exiting".
- AD-26 says "hold (an async wait with a bound …)". A Hekma-side bound ending a held call is a fourth end cause the PRD does not list, and it acts on the Run outside NFR-9's list.
- The held count and age live in proxy memory, and no path to `ps` is stated.

**Fix.** Replace the record with:

> {call id, run id, Control Depth, credential identity (token class + hash prefix), model, status class, latency, tokens, start time (sub-second), disposition applied (pass / held → released / rerouted / rejected), hold end cause (`client-timeout` / `disconnect` / `process-exit` / `crash`)}

Replace "an async wait with a bound" with "an async wait with no Hekma-imposed deadline (the client's own timeout ends it)". Add: "the proxy publishes per-Run held count and oldest-held age to the core on change (coalesced), which `ps` reads."

### F12 — LOW — Label rules: "Caller cannot set a bound key" and the bound-label sources are not governed

**Evidence.**

- FR-49 lists the bound sources: "Agent Instance configuration, which supervised Runs inherit, a virtual provider key, a Metering Proxy endpoint, or an Operator-only label verb and Control API call". FR-49 also says "A Caller cannot set a key that is already bound".
- AD-25, line 299, has only "source ∈ bound|caller" and "Policy Selectors match bound labels only".
- No rule rejects a Caller label that collides with a bound key; the announce endpoint and identity-header paths can differ.
- There is no home for endpoint-path and virtual-key bindings: AD-27's table list (line 313) has no bindings or token-hash table, though AD-22 says hashes live "in state.db".
- No config key for instance labels exists (AD-9 rejects unknown keys).

**Fix.** Add to AD-25:

> A Caller label whose key is already bound on the Run is rejected (typed) by the core on every path (announce, identity header, OTLP attribute).

Add to AD-27's state.db list: `credentials` (token hash, class, run id, expiry) and `label_bindings` (endpoint path or virtual key → bound labels). Add to AD-9 a reserved `labels.*` instance key that supervised Runs inherit as bound.

### F13 — MEDIUM — Parts of the FR-21 upgrade contract did not land (event, registration response, per-Run note, release class)

**Evidence.**

- FR-21 asks for a note "on each of its Runs wherever `hekma ps` lists them (human-readable and `--json`) …, and one event is emitted at upgrade".
- FR-21 also says "every registration response and registration event names the effective Breach Action". And: "The default changes for programmatic registrations only in a semver-breaking Embedding Interface release, or at a Control API minor version under §7 item 4".
- AD-7, line 89, has only the instance-layer write and a store marker that "renders the 'kept on pause by upgrade' note". The upgrade event, the registration payload field, the per-Run note and the semver-breaking release obligation are absent. AD-2's semver-check job will not catch a changed `#[default]`.

**Fix.** Append to the AD-7 bullet:

> The upgrade emits one `breach_default_upgraded` event listing the instances written. The note renders on the instance and on each of its Runs in `ps` (human and `--json`). Registration responses and `registered` events carry `effective_breach_action` with its source. The release carrying the `#[default]` change is a semver-breaking `hekma-engine` release and a Control API minor, each with a release note.

### F14 — MEDIUM — FR-54 bypass detection has no governing AD or owner

**Evidence.**

- The capability map row (line 524), "Run Keys, custody, scent, bypass (FR-53, FR-54) | … | AD-22, AD-26". But AD-22 and AD-26 say nothing about detection, and the proxy cannot see traffic that bypasses it.
- FR-54 needs:
  - a per-OS signal for a supervised Run's direct provider connections;
  - "Where an OS gives no reliable signal, the CLI and docs say detection is unavailable on that OS; it never reports a Run as clean without having checked";
  - an informational note (not an Anomaly) for `supervised (no wire)`;
  - an OTLP counter by provider host.
- The mechanism is OS-specific (Platform code convention), so without an owner it can land anywhere.

**Fix.** Add to AD-23:

> **Bypass detection** runs in the Shim (OS parts under `backends/`): it samples the supervised process tree's outbound connections against the known provider host list and reports matches to the engine as Anomalies (informational note for `supervised (no wire)`). An OS or backend without a reliable signal reports `bypass: unavailable`, never `clean`.

Fix the map row to cite AD-23.

### F15 — MEDIUM — Ended-Run retention and compaction (FR-49) did not land

**Evidence.** FR-49 says "Ended Runs are kept for a configurable retention window, then compacted into rollups that keep usage totals and labels". FR-50 says anomalous ended Runs "stay listed until the ended-Run retention window compacts them". AD-27, line 313, covers retention only for the calls store ("dropped whole at retention"). At 1M+ Runs, the `runs` and `run_labels` tables grow unbounded, and compaction semantics decide what `ps --all` and the rollups return.

**Fix.** Add to AD-27:

> Ended Runs older than the retention window are compacted by the store writer: the Run row and labels fold into a compacted-Run rollup (usage totals, labels, end cause), and per-Run detail rows are deleted. Rollups of ancestors and instances are unaffected.

### F16 — LOW — The NFR-4 placeholders are not named in the measurement gate (proxy ≤10 ms p99; Shim overhead)

**Evidence.**

- NFR-4 sets placeholders: "the first page of `hekma ps` … in <1s at 1M Runs, and the Metering Proxy adding ≤10ms p99 to a model call", plus "≤ 2% CPU … ≤ 50MB RSS" per instance.
- AD-18 amend, line 417: "A `--serve-shape`-style measurement at 10k / 100k / 1M Runs gates any claim against NFR-4". The proxy-latency term is not named, and the Shim (a full `hekma` process per supervised process, AD-23) is not counted in per-instance supervision overhead.

**Fix.** Amend line 417:

> … gates any claim against NFR-4: first-page `ps` / `/v0/runs` latency, Metering Proxy added latency p99 (target ≤10 ms), and per-instance CPU/RSS including its Shim.

### F17 — LOW — The §7 item 5 deprecation path and FR-59's default-Policy attribute are missing from the OTLP v2 amendment

**Evidence.**

- AD-21 amend, line 419: "`hekma.run_id` leaves metric attributes … The change is a versioned exposition change under AI-55's two-pass review." PRD §7 deprecation policy: "minimum one minor-version notice window, removal only at a major". FR-59: "ships … under the §7 deprecation path".
- FR-59's policy attribute takes "the default Policy's name when none did; metrics not tied to one limit carry no Policy attribute". The spine says only "the Policy that supplied the limit".
- AD-21, line 271, binds the Control API to "additive-only-until-frozen discipline". PRD §7 item 4 allows breaking changes "only at a minor version with release notes", which FR-21's default change uses.

**Fix.** Append to the AD-21 amendment:

> `hekma.run_id` is marked deprecated in `otlp-metrics v1` for at least one minor release before v2 removes it. `hekma.policy` carries the default Policy's name when no Policy or explicit limit supplied the value, and is absent on metrics not tied to one limit.

In AD-21, line 271, replace "additive-only-until-frozen" with "§7 item 4 (breaking only at a `0.x` minor, with release notes)".

### F18 — LOW — Deferred lists traces/spans twice with different triggers

**Evidence.** Line 543: "Newly deferred: … **traces/spans** (until the dated PRD §6 amendment)". Line 545 (2026-10-23) still has "**traces/spans + a spans view** (waits on Story 19.4 plus a named consumer)". Proposal §4.2 said to reactivate traces/spans, and the PRD FR-59 gate is the §6 amendment. Two triggers let two stories disagree on when traces may ship.

**Fix.** In line 545, replace "**traces/spans + a spans view** (waits on Story 19.4 plus a named consumer)" with "**a spans view** (waits on Story 19.4 plus a named consumer; trace/span export itself is gated by the PRD §6 amendment, line above)".

### F19 — LOW — Failing a malformed `breach_action` closed to `pause` is a Hekma-chosen stop outside NFR-9's list

**Evidence.** AD-7, line 89: "A malformed `breach_action` fails closed to `pause` with a surfaced diagnostic". NFR-9 allows a pause only for "a non-`warn` Breach Action … configured at any layer", plus verbs, failed starts or probes, and shutdown or cleanup with Handoff Mode off. A malformed value is not a configured `pause`. The code already rejects unknown values at write time (`crates/hekma-engine/src/domain/budget.rs:348`, `config.rs:1142`). So the only malformed case is a hand-edited file, where refusing the start is the existing honest behaviour.

**Fix.** Replace the sentence with:

> A malformed `breach_action` refuses the start (or the config read) with a typed diagnostic naming the value, as today. It never silently falls to either default.

Alternatively, add "a malformed Breach Action, failing closed" to NFR-9's list in the PRD.
