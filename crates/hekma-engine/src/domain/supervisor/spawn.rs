//! spawn family — split out of the supervisor monolith by story 13-3
//! (epic-13; boundary ratified 2026-09-19, study: Option B). Pure move:
//! additional `impl Supervisor` block, no signature or behavior change.

use super::*;

impl Supervisor {
    /// Start a registered / previously stopped / FAILED Agent Instance
    /// (AC1/AC2; AC3 restart-from-failed via the 1-6 transition row).
    ///
    /// Thin wrapper over [`Supervisor::start_inner`] with no restart context (a
    /// fresh operator `start`): the `starting → running` transition records a
    /// plain [`TransitionCause::AdapterReady`], and the write-ahead spawn record's
    /// restart count is RESET to 0 (a clean run resets the count, AC4).
    pub fn start(&mut self, registry: &Registry, name: &str) -> Result<AgentInstance, EngineError> {
        self.start_inner(registry, name, None, false)
    }

    /// Start a registered Agent Instance DETACHED (story 12-1): the spawned
    /// child's handle is DISARMED at spawn so the child survives THIS engine
    /// session's exit, and the next engine open re-adopts it through the
    /// EXISTING write-ahead fingerprint path (AD-5 — nothing about the record,
    /// the fingerprint, or the adoption machinery changes).
    ///
    /// Ratified v1 REFUSALS + HONEST WINDOW (AI-20 option b; the epic's
    /// enforcement-window honesty is a hard AC, not a nicety):
    /// * An `engine-observed` instance is refused BEFORE any side effect (no
    ///   transition, no listener, no spawn): the loopback forward listener
    ///   would die with this command, stranding the agent's model traffic on a
    ///   dead port — the same loud strand adoption already surfaces. The error
    ///   names why + the remediation (start without `--detach`).
    /// * A detached self-reported instance is supervised ONLY while THIS
    ///   command runs: between commands there is NO crash detection, NO budget
    ///   enforcement, and NO event delivery. The caller (the CLI) must state
    ///   that window on `--help` and in its stderr notice.
    /// * The child is spawned with stdin NULL regardless of the declared
    ///   interaction level (a pipe's write end would die with this CLI — an
    ///   EPIPE strand); after re-adoption `send` fails with the ordinary
    ///   adopted-instance interaction error.
    pub fn start_detached(
        &mut self,
        registry: &Registry,
        name: &str,
    ) -> Result<AgentInstance, EngineError> {
        self.start_inner(registry, name, None, true)
    }

    /// The shared start path (AC1/AC2 + the 1-6 write-ahead record commit).
    ///
    /// `restart`:
    /// * `None` — a fresh `start` (operator or first launch). The
    ///   `starting → running` cause is [`TransitionCause::AdapterReady`]; the
    ///   spawn record's restart count is RESET to 0.
    /// * `Some((attempt, waited))` — a Restart Policy restart (from
    ///   [`Supervisor::restart`]). The `starting → running` cause is
    ///   [`TransitionCause::Restarted`] recording the consecutive `attempt` +
    ///   the backoff `waited`; the record keeps that count.
    ///
    /// Order (so a rejection leaves NO spurious state change):
    /// 1. look up + validate `Start` against the transition table (AC4),
    /// 2. resolve the launch spec (a bad/native-only adapter rejects here),
    /// 3. persist `registered/stopped/failed → starting` + emit,
    /// 4. spawn; a spawn failure → `starting → failed` (diagnostic preserved),
    /// 5. readiness watch: an immediate death → `failed` (AC2),
    /// 6. **commit the write-ahead spawn record** (AD-5: `{pid, fingerprint}` +
    ///    policy + count) BEFORE the instance is treated as supervised — "no
    ///    spawn without its record committed first",
    /// 7. persist `starting → running` + emit, store the handle, return.
    pub(super) fn start_inner(
        &mut self,
        registry: &Registry,
        name: &str,
        restart: Option<(u32, Duration)>,
        detach: bool,
    ) -> Result<AgentInstance, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // (1) Transition gate (AC4). Rejects (e.g. start on running) before any
        // side effect and BEFORE we touch the backend.
        let starting = next_state(instance.state, LifecycleCommand::Start)?;

        // (2) Resolve the launch spec (may reject: native-only / bad manifest),
        // still before any persisted state change.
        let (kind, manifest_path, persisted_launch) = registry
            .adapter_launch_facts(&name)
            .map_err(registry_to_engine)?;
        // Story 14-1 (spine AD-19): the acp kind's start behavior keys off the
        // kind ONCE — its launch resolves from the instance's unified config
        // keys (`acp.command`/`acp.args`, the resolver refuses honestly naming
        // both keys when the command is unset), its spawn pipes BOTH stdio
        // directions (the transport IS the pipe pair), and its stdout is the
        // protocol stream the connection's reader consumes.
        let is_acp = crate::acp::is_acp_kind(&kind);
        // Story 14-2 (D4, spine AD-19): the session id PERSISTED by a previous
        // Run — the resume offer for this start's handshake. Read BEFORE any
        // side effect (a pure record read; the write-ahead commit at (6)
        // clears it, and the established id is re-persisted post-handshake).
        // A read failure is surfaced honestly and start proceeds without a
        // resume offer (a fresh session) — surfaced-not-silent (AI-18).
        let resume_session: Option<String> = if is_acp {
            match registry.spawn_record(&name) {
                Ok(record) => record.and_then(|r| r.acp_session_id),
                Err(err) => {
                    // A distinct local name (not `notice`): the embed-clean
                    // audit pins the `emit_diagnostic(&notice)` shape to the
                    // ONE memory-delivery route (DC-10).
                    let resume_read_notice = format!(
                        "{}: the previous ACP session id could not be read from the spawn \
                         record ({err}); a fresh session will open",
                        name.as_str(),
                    );
                    self.emit_diagnostic(&resume_read_notice);
                    None
                }
            }
        } else {
            None
        };
        // The Agent Home + effective config are resolved EARLIER than the
        // (2b) block below because the acp launch needs the resolved config.
        // Both are pure reads (no state change), so an early failure still
        // rejects the start with no spurious transition — the same atomicity
        // the later resolution had.
        let home = registry.agent_home(&name);
        let mut effective = registry
            .effective_config(&name, crate::domain::ConfigLayer::empty())
            .map_err(|e| config_to_engine(&name, e))?;
        // Prefer the launch SNAPSHOTTED at registration — this removes the fragile
        // start-time manifest re-read that dropped `args` on hosted CI runners
        // (the agent spawned with the right binary but ZERO args). Fall back to
        // re-reading the manifest ONLY when the snapshot carries no launch: a
        // native adapter (→ NativeHasNoLaunch, preserved) or an instance
        // registered before the launch was persisted (legacy snapshot). The
        // fallback RE-NEGOTIATES the contract version (retro #161): the file on
        // disk may have drifted since registration, and a manifest edited to a
        // foreign major must fail the start, not bypass the 6-6 load gate.
        // THE acp EXCEPTION (story 14-1): the builtin declares NO launch by
        // design (the launch is per-instance operator configuration), so the
        // fallback resolves from the config keys instead of erroring
        // NativeHasNoLaunch.
        let mut launch = match persisted_launch {
            Some(launch) => launch,
            None if is_acp => crate::acp::resolve_acp_launch(&name, &effective)?,
            None => adapter::resolve_start_launch(&kind, manifest_path.as_deref())
                .map_err(|e| launch_to_engine(&name, e))?,
        };

        // Read the declared Metering Source (story 3-1) from the persisted adapter
        // snapshot — stamped on every UsageEvent ingested during this Run. Read here
        // (a pure snapshot read) before any side effect; a corrupt snapshot surfaces
        // the same way the launch-facts read above would.
        //
        // Story 14-3 (T2, spine AD-19): the acp kind's ACTIVE source is resolved
        // from its effective config instead of the (always self-reported) snapshot
        // — the operator's `metering.upstream_base_url` key opts the instance into
        // the engine-observed loopback channel, exactly the key an observed
        // MANIFEST agent honors. The snapshot stays untouched (a registration
        // record); the Fleet read resolves the same way, so the surfaced source
        // matches what this start does.
        let metering_source = if is_acp {
            crate::acp::resolve_acp_metering_source(&effective).to_string()
        } else {
            registry
                .metering_source(&name)
                .map_err(registry_to_engine)?
        };

        // Story 12-1 — the DETACHED REFUSAL for engine-observed instances,
        // BEFORE ANY SIDE EFFECT (no `starting` transition, no loopback
        // listener, no spawn — every fallible step below still lies ahead, so
        // the instance keeps its prior state). A detached observed instance is
        // structurally broken: the loopback forward listener lives inside THIS
        // engine process, so a detached start would inject a `base_url` whose
        // server dies the moment the CLI exits — the agent's model calls would
        // then hit a dead port (the loud stranded-listener condition adoption
        // already announces). Refuse with the why + the remediation; a
        // self-reported instance detaches fine (no listener to strand).
        if detach && metering_source == "engine-observed" {
            return Err(EngineError::DetachRefused {
                name: name.as_str().to_string(),
                detail: "the instance meters via the engine-observed channel, whose loopback \
                         forward listener lives inside the starting command — detaching would \
                         leave the agent pointed at a listener that dies with the command. \
                         Start it without --detach (the in-command supervision keeps the \
                         listener alive), or switch the adapter's metering source to \
                         self-reported."
                    .to_string(),
            });
        }

        // Read the effective (current-OS) Capability::Interaction level (story
        // 4.1 fix pass, HIGH finding — review of #79) to decide whether THIS
        // spawn should pipe stdin at all. The story's original implementation
        // piped UNCONDITIONALLY for every process; an adversarial audit showed
        // this can hang an adapter that declares no interaction support: a
        // process that blocks reading stdin at startup (a common "sniff for
        // piped input" real-CLI idiom) never sees EOF, because the engine
        // holds the pipe's write end open for the process's whole supervised
        // lifetime and nothing ever writes to it unless `send` is called — the
        // child hangs forever yet is reported `running` (readiness here is
        // just "the process didn't exit immediately"), a silent deadlock with
        // no error signal anywhere. Mirrors how the rest of this codebase
        // gates BEHAVIOR (not just callability) on declared capabilities
        // (e.g. pause's SIGSTOP-vs-noop branching). Read here (a pure
        // snapshot read, mirroring `metering_source` above) before any side
        // effect, so a corrupt snapshot rejects the start cleanly like every
        // other pre-transition read.
        let interaction_level = registry
            .effective_support(&name, Capability::Interaction)
            .map_err(registry_to_engine)?;
        let pipe_stdin = matches!(
            interaction_level,
            SupportLevel::Guaranteed | SupportLevel::BestEffort
        );

        // (2b) Map the resolved unified config into the adapter's NATIVE mechanism
        // (story 2-2, FR-12) — still before any persisted state change, so a
        // config/mapping failure rejects the start cleanly (no spurious state
        // change, no half-launched process). The effective config + Agent Home
        // were resolved EARLIER (above) because the acp kind's launch resolves
        // from the config; this block maps them. Resolve the adapter's declared
        // mapping (manifest `[config]` or the native code-declared table), then
        // apply: known keys land in their declared native target (env →
        // launch.env; flag → launch.args; file → a rendered file in the Agent
        // Home), and `agent.*` pass-through leaves are delivered VERBATIM (AC6).
        // The Agent Home already exists (created at registration); file targets
        // render into it here.
        // (2b-memory-spoof) The reserved `memory.dir` key is a DELIVERY
        // MECHANISM, never operator configuration (story 5-1's CORRECTION; docs:
        // "the operator never set this key"). Strip any hand-set value from the
        // operator layers — mirroring the reserved-identity `name` drop — so it
        // can reach neither the mapping application nor the snapshot. Without
        // this, an operator-supplied value would flow through whenever NO
        // backing is attached (the engine override layer is absent then) and
        // masquerade as engine-delivered memory. Only the invocation-override
        // layer built further below may supply this key.
        let _ = effective.remove(super::config::MEMORY_DIR_KEY);
        let mapping = adapter::resolve_config_mapping(&kind, manifest_path.as_deref())
            .map_err(|e| launch_to_engine(&name, e))?;

        // (2b-readiness) Resolve the adapter's optional declared readiness gate
        // (contract `1.1.0`): the `[readiness]` section's ONE probe + its
        // timeout budget. Pre-transition like every fallible resolution above —
        // an invalid (or drifted-since-registration) section rejects the start
        // with NO state change. A manifest without the section (or a native
        // adapter) resolves `None` — the pre-1.1.0 behavior, byte-identical.
        // The gate is consumed AFTER the ACP handshake below (ordering:
        // survival window → handshake → probe).
        let readiness_gate = adapter::resolve_readiness_probe(&kind, manifest_path.as_deref())
            .map_err(|e| launch_to_engine(&name, e))?;

        // (2b-liveness) Resolve the adapter's optional declared liveness check
        // (contract `1.1.0`, story 18-2): the `[liveness]` section's ONE
        // check + its `max_age_ms` window. Pre-transition like every fallible
        // resolution above — an invalid (or drifted-since-registration)
        // section rejects the start with NO state change. A manifest without
        // the section (or a native adapter) resolves `None` — the pre-1.1.0
        // behavior, byte-identical. The check is carried on the `Supervised`
        // record below, where the reaper evaluates it per tick while the
        // instance is EXACTLY `running`.
        let liveness_check = adapter::resolve_liveness_check(&kind, manifest_path.as_deref())
            .map_err(|e| launch_to_engine(&name, e))?;

        // (2b-memory) MANAGED MEMORY BACKING (story 5-1, spine AD-11). Read the
        // attached backing (one DB read) and — for a `filesystem` kind — ensure
        // the managed directory exists: ONE idempotent `create_dir_all`, no
        // recursion, no copy/seed/restore of CONTENTS ever (DC-7 — byte-identical
        // survival comes from non-interference; this is also the AD-17 bounded-work
        // rule: identical cost to `ensure_log_dir` below). Both happen HERE, in
        // the pre-transition block: every fallible step precedes any state change,
        // so a failure rejects the start with no spurious transition. This is the
        // defensive SELF-HEAL — attach already created it; a manual delete must
        // not wedge future starts.
        //
        // STORY 15-6: this early read is the PRE-CHECK, no longer the
        // authoritative read. Its refusal inputs (the UTF-8 + symlink refusals
        // and the self-heal below) must run BEFORE any state transition, so it
        // stays; the AUTHORITATIVE read is the snapshot inside the `→ starting`
        // transition transaction (step (3) below), which refuses typed +
        // retryably when a second process attached/detached between here and
        // there. A matching snapshot confirms everything this pre-check
        // derived (the injected memory dir, the mapping application); a
        // mismatching one rejects the start with nothing launched.
        let memory_backing = registry.memory_backing(&name).map_err(registry_to_engine)?;
        let memory_dir = memory_backing
            .as_ref()
            .filter(|backing| backing.kind == MemoryBackingKind::Filesystem)
            .map(|_| registry.agent_memory_dir(&name));
        if let Some(dir) = &memory_dir {
            // Strict UTF-8 BEFORE anything else: this path is DELIVERED to the
            // agent at the reserved key (`invocation_overrides` stringifies it),
            // and a lossy coercion there would hand the agent a mangled path
            // while every local check still passed. A non-UTF-8 state-dir path
            // is effectively impossible for a sane install; if one shows up, it
            // fails LOUD here, pre-transition, with no side effect.
            if dir.to_str().is_none() {
                return Err(EngineError::Log {
                    name: name.as_str().to_string(),
                    path: dir.to_string_lossy().into_owned(),
                    detail: "the managed memory directory path is not valid UTF-8, so it \
                             cannot be delivered safely at the reserved 'memory.dir' key"
                        .to_string(),
                });
            }
            // Symlink refusal mirrors the attach-side guard (registry's
            // ensure_managed_memory_dir): never follow a link out of the Agent
            // Home — e.g. one planted between attach and start.
            if std::fs::symlink_metadata(dir)
                .map(|m| m.is_symlink())
                .unwrap_or(false)
            {
                return Err(EngineError::Log {
                    name: name.as_str().to_string(),
                    path: dir.to_string_lossy().into_owned(),
                    detail: "the managed memory directory path is a symlink; refusing to \
                             follow it"
                        .to_string(),
                });
            }
            std::fs::create_dir_all(dir).map_err(|e| EngineError::Log {
                name: name.as_str().to_string(),
                path: dir.to_string_lossy().into_owned(),
                detail: format!("could not ensure the managed memory directory: {e}"),
            })?;
        }

        // (2b-observed) ENGINE-OBSERVED metering (story 3-4, AC-A/AC6): for an
        // `engine-observed` instance, START the loopback forward listener HERE
        // (before the mapping application + the `starting` transition, so a listener
        // failure rejects the start cleanly with NO state change — mirroring the
        // secret/snapshot failures), then INJECT its loopback `http://127.0.0.1:<port>`
        // address as a `metering.base_url` INVOCATION-OVERRIDE so the adapter's
        // EXISTING config-mapping (2-2) delivers it into the agent's native mechanism
        // (e.g. env `OPENAI_BASE_URL`). The address is ENGINE-computed (the engine is
        // the sole authority — AC-B); the adapter merely receives it. A `self-reported`
        // instance leaves `observed_listener` None and its start path UNCHANGED. The
        // held listener is moved into `Supervised` on success; on any later start
        // failure its `Drop` aborts the accept-loop task (RAII teardown, no leak).
        let observed_listener =
            self.start_observed_listener(&name, &metering_source, &effective)?;
        // The effective config the MAPPING applies: for an observed instance it
        // carries the engine-injected loopback base_url (story 3-4), and for a
        // filesystem-backed instance the engine-computed managed memory dir at the
        // reserved `memory.dir` key (story 5-1) — both INVOCATION overrides (the
        // strongest layer, AD-9), so a hand-set lower-layer value cannot win. The
        // SNAPSHOT (2c) below stays on the plain `effective` (the operator config),
        // so NEITHER injected value is persisted as "what applied" — honest
        // provenance (3-4's rule; 5-1's CORRECTION extends it: `memory.dir` is a
        // delivery mechanism, not operator configuration).
        let mut mapping_effective = match invocation_overrides(
            observed_listener.as_ref().map(ObservedListener::base_url),
            memory_dir.as_deref(),
        ) {
            Some(layer) => registry
                .effective_config(&name, layer)
                .map_err(|e| config_to_engine(&name, e))?,
            None => effective.clone(),
        };
        // (2b-memory-spoof, the override branch — story 11-3, A1) The re-fold
        // above re-derives the config from EVERY layer, so a hand-set reserved
        // key RESURRECTS here: the base strip above removed it from
        // `effective`, but this fresh fold never saw that strip. A hand-set
        // `memory.dir` must not ride the re-fold into the mapping application.
        // When the engine injected NO managed dir (no filesystem backing), any
        // `memory.dir` in the fold is exactly such a hand-set resurrection —
        // strip it, exactly like the base path. When the engine DID inject the
        // dir, its own override leaf is the fold's winner (the invocation layer
        // is the strongest, AD-9) and MUST survive the strip — a hand-set
        // lower-layer value cannot beat it, so stripping there would only
        // break the engine's own delivery.
        if memory_dir.is_none() {
            let _ = mapping_effective.remove(super::config::MEMORY_DIR_KEY);
        }

        // (2b-memory-delivery) DC-10 honesty (AD-11 Delivery clause): when a
        // `filesystem` backing is attached but the resolved mapping declares NO
        // target for the reserved key, say so ONCE through the diagnostic
        // emission (AD-12: stderr by default, the host's story-10-2 sink when
        // installed) — naming the instance, the managed path, and the fact
        // that the agent will not receive it. The start still SUCCEEDS: the
        // directory guarantee holds regardless, and refusing an
        // otherwise-healthy agent because its adapter maps no memory key would
        // be a regression. Deliberately NOT generalized to other unmapped
        // keys (story 2-2 Decision 6 stands; memory is special only because
        // the operator took an explicit attach action and is owed the truth
        // about its effect). Pure decision fn (unit-tested); this is the only
        // emission site, routed through `emit_diagnostic` (story 10-2).
        //
        // STORY 15-6: the emission moved BELOW the `→ starting` transition
        // transaction and now derives from the TRANSACTIONAL SNAPSHOT (the
        // authoritative read), not this pre-check — a start whose snapshot
        // disagrees with the pre-check rejects and emits NOTHING (a notice
        // for a launch that never happened would be a lie). When the
        // snapshot matches (the only way the start proceeds), the
        // snapshot-derived notice is byte-identical to this pre-check
        // derivation. The `mapping` needed by the notice is already resolved
        // (above) and unchanged by the transition.

        // (2b-secret) Resolve every `secret:NAME` leaf into a SecretString BEFORE
        // the mapping application (story 2-4, spine AD-10, AC-A/AC9). This is where
        // display and delivery DIVERGE: `effective`'s `display()`-based surfaces
        // (the snapshot at (2c), `config get`) stay MASKED, but the resolved
        // cleartext flows into `apply_config_mapping` so the ADAPTER gets a usable
        // key. Resolution (env → the 0600 secrets file) runs here, still before any
        // persisted state change, so an unresolved/ill-permissioned secret REJECTS
        // the start cleanly (no half-launch, mirroring the config-apply + snapshot
        // failures) — a typed `EngineError::Secret` that NEVER echoes a value.
        let secrets = registry
            .resolve_secrets(&mapping_effective)
            .map_err(|e| secret_to_engine(&name, e))?;
        // (AI-27 shadow capture) The pre-apply launch env, snapshotted BEFORE
        // the mapping application mutates `launch.env` (whatever the persisted
        // registration snapshot carried): an env-targeted mapping whose name
        // already exists here OVERWRITES that variable (documented precedence —
        // config wins, last-write-wins, untouched), and the value diff below
        // makes that shadow VISIBLE instead of silent (story 11-2).
        let base_env: std::collections::BTreeMap<String, String> = launch.env.clone();
        let mapping_report = adapter::apply_config_mapping(
            &mut launch,
            &mapping,
            &mapping_effective,
            &secrets,
            &home,
        )
        .map_err(|e| config_apply_to_engine(&name, e))?;

        // (AI-27 shadow visibility) ONE diagnostic naming every launch env var
        // the config mapping overwrote — deliberately NOT worded "base-launch":
        // the pre-apply `launch.env` is whatever the persisted registration
        // snapshot carried (the manifest `[lifecycle.start]` env or the
        // code-declared launch), a set this diagnostic must not over-claim. The
        // start still SUCCEEDS — the precedence is unchanged (the config value
        // won in `launch.env`) and has always been the documented behavior; only
        // the silence was the bug. Routed through `emit_diagnostic` (story 10-2:
        // stderr by default, the host's sink when installed), like the
        // memory-delivery notice above. The message is formatted into a local
        // first, exactly like that notice (the embed-clean audit pins the inline
        // `emit_diagnostic(&format!(` shape to the ONE breadcrumb route).
        let shadowed = shadowed_env_keys(&base_env, &launch.env);
        if !shadowed.is_empty() {
            let shadow_notice = format!(
                "{}: the config mapping overwrote launch environment variable(s) {} — \
                 the config value wins for this start (documented precedence). Rename the \
                 env target or unset the config key if the original value was intended.",
                name.as_str(),
                shadowed.join(", "),
            );
            self.emit_diagnostic(&shadow_notice);
        }

        // (AI-39 runtime) ONE warn-only diagnostic naming the config keys whose
        // resolved SECRET cleartext was delivered into FLAG targets — i.e. the
        // keys now on the process argv, world-readable cross-user. This is the
        // runtime half of the steering the set-time warning starts (story 11-2):
        // the argv boundary itself stays ACCEPTED (documented; no rejection
        // semantics were ever ratified) — the diagnostic rides the same
        // `emit_diagnostic` channel as the notices above, so operators (and the
        // existing audit trail) see it on every start that delivers one. The
        // message is formatted into a local first (see the shadow notice above).
        if !mapping_report.secret_flag_keys.is_empty() {
            let flag_notice = format!(
                "{}: secret-carrying config key(s) [{}] resolve into FLAG targets — their \
                 cleartext is passed on the agent's command line, where argv is readable \
                 by other local users (ps, /proc/<pid>/cmdline). Prefer an env or file \
                 target for these keys.",
                name.as_str(),
                mapping_report.secret_flag_keys.join(", "),
            );
            self.emit_diagnostic(&flag_notice);
        }

        // (2c) Persist the effective-config snapshot into the Agent Home (story
        // 2-3, spine AD-9 "start resolves to an EffectiveConfig snapshot persisted
        // in the Agent Home, every value tagged with its source layer" + AD-6
        // "effective-config snapshots are files inside the Agent Home"). The
        // resolved `effective` is already in hand from (2b); write it HERE, right
        // after the mapping application and BEFORE the `starting` transition below,
        // so a snapshot-write failure rejects the start cleanly (NO state change —
        // exactly mirroring how the config-apply failure at (2b) rejects before the
        // transition). The snapshot is a PROMISED AD-9 artifact (a Host/debugging
        // record of "what will apply on next start"), not a best-effort nicety, so
        // its failure is a typed start error. Because RESTART also flows through
        // this path (story 1-6), the snapshot is refreshed on restart too (AC7:
        // OVERWRITTEN every successful start/restart, never a stale resolution). It
        // is NOT written at registration (there is no "effective at start" until a
        // start happens) and NOT deleted at stop.
        registry
            .write_effective_config_snapshot(&name, &effective)
            .map_err(snapshot_to_engine)?;

        // Read the per-instance Restart Policy so the write-ahead record carries
        // it (AD-15 per-instance configurable). Read once, before any side effect.
        let policy = registry
            .effective_restart_policy(&name)
            .map_err(registry_to_engine)?;

        // The spawned agent's stdout/stderr go to a SEPARATE agent.log, never the
        // engine's JSON-Lines transition-event log (instance.log) — otherwise the
        // agent's plain-text output would corrupt the structured event log.
        let agent_log_path = registry.agent_output_log_path(&name);
        // Ensure the log directory exists (AD-12 seed) so spawn can redirect
        // stdout/stderr into it and we can append transition events.
        self.ensure_log_dir(registry, &name)?;

        // Anchor the usage-ingestion cursor at the agent-output log length BEFORE
        // the spawn (story 3-1). This Run's own output is appended AFTER this point,
        // so ingestion reads ALL of it — while a PRIOR Run's already-captured lines
        // (a stop→start reuses the same append-only agent.log) stay BEHIND the cursor
        // and are never re-ingested under this fresh Run id. Capturing it HERE (not
        // after the readiness watch below) is essential: a fast agent emits its first
        // usage lines within the ~300ms readiness window, so a cursor set post-
        // readiness would skip them — the ingestion bug this prevents. For an `acp`
        // spawn (story 14-1) the READER appends the raw lines to this same file (the
        // stdout is a pipe, not a redirect), so the cursor semantics are unchanged.
        // Story 14-3 (T3): the acp kind's STDERR sentinel channel anchors the same
        // way, at the captured stderr log's pre-spawn length — a prior Run's
        // captured stderr lines stay behind the cursor, never re-ingested under
        // this fresh Run id.
        let usage_cursor = self.agent_log_len(registry, &name);
        let stderr_usage_cursor = self.agent_stderr_log_len(registry, &name);

        // (3) registered/stopped/failed → starting — TRANSACTIONALLY (story
        // 15-6). The state persist and the AUTHORITATIVE backing read share
        // ONE `BEGIN IMMEDIATE` transaction, conditional on the state and
        // backing this start's pre-check reads observed. A second process's
        // attach/detach (or lifecycle transition) landing anywhere inside
        // start's read→transition window refuses here, typed and retryable,
        // with the transaction rolled back — no `starting` row, no launch
        // against a stale view. On success the returned snapshot is the
        // authoritative backing this launch reflects.
        let backing_snapshot = self.transition_with_backing_snapshot(
            registry,
            &name,
            instance.state,
            starting,
            TransitionCause::command(LifecycleCommand::Start.as_str()),
            memory_backing,
        )?;

        // (3b) The DC-10 delivery notice, derived from the TRANSACTIONAL
        // SNAPSHOT (story 15-6 — see the (2b-memory-delivery) block above):
        // reaching this line proves snapshot == pre-check, so the derived
        // dir is byte-identical to the pre-check's; deriving it from the
        // snapshot keeps the notice's semantics tied to what the launch
        // actually delivers, surviving the reorder.
        let snapshot_memory_dir = backing_snapshot
            .as_ref()
            .filter(|backing| backing.kind == MemoryBackingKind::Filesystem)
            .map(|_| registry.agent_memory_dir(&name));
        if let Some(notice) =
            memory_delivery_notice(snapshot_memory_dir.as_deref(), &mapping, &name)
        {
            self.emit_diagnostic(&notice);
        }

        // Story 17-4: resolve the engine's capture posture into THIS spawn's
        // wiring — the supervisor owns the decision, the backends stay dumb
        // executors. `full` (the default) is the byte-identical pre-17-4
        // triple. `metadata` keeps the SAME three-path redirect (the
        // crash-immune raw files exist while running; the capture's tailer
        // writes metadata-only records and stop unlinks the raw files).
        // `off` resolves all three paths to `None` — the backends' existing
        // `Stdio::null()` arm — so nothing is ever written, no tailer runs,
        // and no `output.log` exists; the honest-absence surfaces (typed
        // read errors, the fleet gap notice's `capture-off` sentinel) key off
        // the mode, never off fabricated data.
        let capture_mode = self.log_capture_mode();
        let (stdout_capture, attributed_capture, stderr_capture) = match capture_mode {
            LogCaptureMode::Off => {
                // Surfaced, not silent (AI-18): an off-mode start DEGRADES
                // log-sentinel metering (the sentinel drain has no log to
                // drain) — say so once per start, on the engine diagnostic
                // surface, never a fabricated size downstream.
                self.emit_diagnostic(
                    "log capture is off: agent output is not captured; usage sentinels \
                     parsed from agent logs are unavailable for this instance (fleet shows \
                     a `capture-off` gap)",
                );
                (None, None, None)
            }
            LogCaptureMode::Full | LogCaptureMode::Metadata => (
                Some(agent_log_path.clone()),
                Some(registry.attributed_output_log_path(&name)),
                Some(registry.agent_stderr_log_path(&name)),
            ),
        };

        let spec = SpawnSpec {
            exec: launch.exec.clone(),
            args: launch.args,
            env: launch.env,
            working_dir: home.clone(),
            log_file: stdout_capture,
            // Story 4-2 (AC-E): capture is unconditional, computed from the
            // SAME Registry path authority as `log_file` (never gated on
            // `pipe_stdin`/`Capability::Interaction` — that gate governs only
            // the stdin *write* direction). For an `acp` spawn (story 14-1)
            // the stdout capture file is fed by the connection's READER (the
            // stdout itself is the protocol pipe) — the raw record stays
            // byte-honest, and the tailer attribution machinery is unchanged.
            // Story 17-4: "unconditional" now scopes to Full/Metadata; `off`
            // resolves all three paths `None` above (the null-stream arm).
            attributed_log_path: attributed_capture,
            // Fix pass (review of #80): the crash-immune raw STDERR capture,
            // computed from the SAME path authority, paired 1:1:1 with
            // `log_file`/`attributed_log_path` (all three Some together in
            // Full/Metadata; all three None in Off).
            stderr_log_file: stderr_capture,
            instance_name: name.as_str().to_string(),
            // Story 12-1: a DETACHED spawn is never given a stdin pipe — the
            // pipe's write end would be held by this soon-to-exit CLI, and a
            // child writing to (or a later engine sending into) a dead peer's
            // pipe is an EPIPE strand. `Stdio::null()` gives the child an
            // immediate, honest EOF instead; after re-adoption `send` fails
            // with the ordinary adopted-instance interaction error (an adopted
            // handle has no recoverable pipe either), which is exactly the
            // behavior the spec's I/O matrix pins.
            pipe_stdin: pipe_stdin && !detach,
            // Story 14-1: the acp transport ALSO pipes stdout (the protocol
            // stream) — but a DETACHED acp spawn gets no transport at all
            // (the pipe halves would die with the spawning CLI), mirroring
            // the stdin force-clear above; the connection build below then
            // refuses honestly. Every other kind keeps the crash-immune
            // direct file redirect, byte-unchanged.
            pipe_stdout: crate::acp::spawn_pipe_stdout(is_acp, detach),
            // The spawn-time disarm flag (see the port's `SpawnSpec::detach`
            // docs): the Unix handle skips its Drop killpg; the Windows spawn
            // never creates/assigns the kill-on-close Job Object.
            detach,
            // Story 17-2: the Host's `child_env_allowlist` flips this to
            // `false` so the child's environment is CLEARED first and only the
            // manifest-declared mappings are applied (the supervisor owns the
            // decision, the backends stay dumb executors). Every pre-17-2
            // caller (and the default) keeps full inheritance.
            inherit_env: !self.child_env_allowlist,
            // Story 17-4: THIS spawn's capture posture, resolved above.
            capture_mode,
        };

        // (4) Spawn. A spawn failure lands the instance in `failed` with the
        // diagnostic preserved and no zombie (the backend spawned nothing / reaps).
        let mut handle = match self.backend.spawn(&spec) {
            Ok(handle) => handle,
            Err(err) => return Err(self.fail_launch(registry, &name, &err)),
        };

        // (5) Readiness watch: a process that dies immediately (especially
        // non-zero) during startup is a launch failure (AC2). Watch briefly;
        // `watch_startup` returns the exit code (if it died) or `None` (ready).
        if let Some(exit_code) = self.watch_startup(&mut handle) {
            let detail = match exit_code {
                Some(c) => format!("exited immediately during startup with code {c}"),
                None => "exited immediately during startup".to_string(),
            };
            // Reap already done by poll; nothing survives.
            return Err(self.fail_launch_detail(registry, &name, detail));
        }

        // (6) Commit the write-ahead spawn record (AD-5) BEFORE the instance is
        // treated as supervised — "no spawn without its record committed first".
        // A fresh start resets the restart count to 0; a restart keeps its
        // attempt count. The fingerprint is the PID-reuse guard for later
        // orphan adoption. A record-commit failure fails the start (leaving the
        // instance `failed`) — we must not run an unrecorded supervised process.
        //
        // Story 15-2: the Run id is minted HERE, BEFORE the record commit, and
        // rides the record — the durable Run identity. A later engine (any
        // intervening command, or a crash restart) ADOPTS this process and
        // RESUMES this Run instead of minting its own, so per-run budgets /
        // usage and the breach latch survive across commands. Every genuine
        // (re)spawn — operator start, stop→start, or a Restart Policy restart —
        // flows through here, so each rewrites the record with a FRESH id: the
        // Run resets exactly when a new process is spawned, never on adoption.
        let run_id = RunId::mint();
        let restart_count = restart.map(|(attempt, _)| attempt).unwrap_or(0);
        let record = SpawnRecord {
            name: name.clone(),
            fingerprint: self.backend.fingerprint(&handle),
            restart_policy: policy,
            restart_count,
            last_known_cause: None,
            // Story 12-1 AMENDMENT (review loop 1): detached-ness RIDES THE
            // RECORD — it is a cross-lifetime property, so every later
            // `adopt_orphans` reads it and re-holds the handle DISARMED. This
            // is the durable-detach promise across N commands, not just the
            // first.
            detach,
            // Story 14-2: the fresh record commits WITHOUT a session id — the
            // resume offer was read above (pre-commit), and the id THIS Run
            // establishes lands right after the handshake via
            // `set_acp_session_id`. Committing `None` here is what clears any
            // settle-retained id: a Run whose handshake has not completed has,
            // honestly, no session yet.
            acp_session_id: None,
            // Story 15-2: the durable Run identity (minted just above, so the
            // id is durable from the first moment the process is supervised).
            run_id: Some(run_id.clone()),
            // The live-owner adoption boundary: THIS engine process is the
            // supervising owner from the moment the record commits — a
            // concurrent engine reading the record can tell a LIVE owner
            // (skip: the agent is supervised here) from a crashed one (adopt
            // armed, story 1-6). `None` on a degraded start-time read keeps
            // the pre-boundary adopt-armed posture, never a spawn failure.
            owner: self.backend.self_fingerprint(),
        };
        if let Err(e) = registry.write_spawn_record(&record) {
            // Persisting the record failed: kill the just-spawned process and
            // land the instance in `failed` so we never leave an unrecorded
            // process behind (AD-5 safety).
            //
            // Review round 2: `drop(handle)` alone only tears the process down
            // when the handle is ATTACHED (drop = group/job kill). A DETACHED
            // handle is DISARMED — its Drop deliberately kills nothing — so a
            // detached start whose record commit fails would LEAK a live,
            // unrecorded process. The teardown is therefore explicit on the
            // detach path: a short graceful window through the backend stop
            // (which still works on a detached handle — the pgid/job is
            // unchanged), then the drop releases whatever remains.
            //
            // AUDIT of the other post-spawn error paths in this function (why
            // this is the ONLY fix site): between `spawn` and this commit the
            // sole fallible step is `watch_startup`, whose failure branch runs
            // only when the process ALREADY exited (reaped by the poll —
            // nothing survives, detached or not); every `?` after the commit
            // happens once the record is DURABLE, so a dropped detached
            // handle there leaves a recorded (adoptable) process, not an
            // unrecorded one.
            if detach {
                let _ = self.backend.stop(&mut handle, Duration::from_millis(500));
            }
            drop(handle);
            return Err(self.fail_launch_detail(
                registry,
                &name,
                format!("could not commit the write-ahead spawn record: {e}"),
            ));
        }

        // (6b) ACP HANDSHAKE (story 14-1, spine AD-19) — for an `acp` instance,
        // take ownership of the child's pipe halves, start the connection
        // (the reader thread owns the stdout stream and re-records every raw
        // line into `agent.log`), and drive the BOUNDED handshake (protocol
        // version 1, DEFAULT client capabilities — advertise-nothing, D3; the
        // Agent Home as cwd; `mcpServers: []`). Story 14-2 (D4): the
        // handshake is RESUME-AWARE — the session id persisted by a previous
        // Run (`resume_session`, read pre-commit above) is offered via
        // `session/load` when the fresh agent advertises `loadSession`, with
        // a `session/new` fallback + surfaced note otherwise; the id THIS Run
        // establishes is persisted to the record post-handshake.
        //
        // Order: AFTER the write-ahead record commit (AD-5 — the spawn is a
        // recorded, adoptable process from this point on; a handshake failure
        // tears the process down explicitly and the stale record honestly
        // reconciles, exactly like the record-commit-failure path above) and
        // BEFORE the `starting → running` transition (the instance reaches
        // `running` only once a session EXISTS). Bounded by
        // [`crate::acp::HANDSHAKE_TIMEOUT`] across both round trips — the
        // accepted AI-59/AD-17 bounded-wait shape (a hung agent refuses the
        // start instead of wedging supervision); the engine locks are held
        // for the wait exactly like `stop`'s KILL_CONFIRM window.
        //
        // Version tolerance: an agent countering a version outside the
        // tolerated set {1} closes the transport + refuses the start
        // (traffic-free — the error names both integers, never any payload).
        let acp = if is_acp {
            let stdin_state = self.backend.take_stdin(&mut handle);
            let stdout = self.backend.take_stdout(&mut handle);
            let capture = self.backend.log_capture(&handle);
            match crate::acp::AcpConnection::start(
                name.as_str(),
                stdin_state,
                stdout,
                agent_log_path.clone(),
                capture,
            ) {
                Ok(connection) => {
                    match crate::acp::handshake(&connection, &home, resume_session.as_deref()) {
                        Ok(outcome) => {
                            // Story 14-2 (D4): persist the established session id
                            // at the point the connection establishes it — the
                            // durable input for the NEXT start's `session/load`
                            // resume offer. A persist failure is surfaced (the
                            // resume would be silently lost otherwise, AI-18) and
                            // never fatal: the instance is running with a live
                            // session regardless.
                            if let Err(err) =
                                registry.set_acp_session_id(&name, Some(&outcome.session_id))
                            {
                                // A distinct local name (not `notice`): the
                                // embed-clean audit pins the
                                // `emit_diagnostic(&notice)` shape to the ONE
                                // memory-delivery route (DC-10).
                                let persist_notice = format!(
                                    "{}: the established ACP session id could not be persisted to \
                                 the spawn record ({err}); the next start will open a fresh \
                                 session instead of resuming",
                                    name.as_str(),
                                );
                                self.emit_diagnostic(&persist_notice);
                            }
                            // The ONE resume-outcome note (AI-18): surfaced when
                            // the session was resumed or a persisted id could not
                            // be; silent on a first start's fresh session.
                            if let Some(note) =
                                crate::acp::resume_outcome_note(name.as_str(), &outcome)
                            {
                                self.emit_diagnostic(&note);
                            }
                            Some(connection)
                        }
                        Err(handshake_err) => {
                            // Close + refuse: tear the transport down (closing
                            // stdin), kill the just-spawned process through the
                            // backend's bounded stop, and land the instance
                            // `failed` with the traffic-free refusal detail.
                            drop(connection);
                            let _ = self.backend.stop(&mut handle, Duration::from_millis(500));
                            drop(handle);
                            return Err(self.fail_launch_detail(
                                registry,
                                &name,
                                format!("ACP handshake failed: {handshake_err}"),
                            ));
                        }
                    }
                }
                Err(connection_err) => {
                    // No transport (e.g. a detached acp spawn has no stdin
                    // pipe): the same close + refuse shape.
                    let _ = self.backend.stop(&mut handle, Duration::from_millis(500));
                    drop(handle);
                    return Err(self.fail_launch_detail(registry, &name, connection_err));
                }
            }
        } else {
            None
        };

        // (6c) READINESS PROBE (contract `1.1.0`) — the adapter's declared
        // probe gates `starting → running`: the instance reaches `running`
        // only once the probe passes within its own `timeout_ms`, on the
        // ENGINE-owned poll cadence. Ordering: the startup survival watch (5)
        // ran first, then the ACP handshake (6b) — the probe is the LAST gate.
        //
        // The wait runs synchronously under the supervisor lock (the same
        // accepted bounded-wait posture as the handshake: the probe budget is
        // declared in the manifest, so supervision cannot wedge unboundedly),
        // and DETACHED starts share it — `--detach` returns ready-or-failed,
        // bounded, exactly like a handshake-bearing start.
        //
        // Three outcomes, each landed honestly:
        // * Ready → fall through to (7) unchanged.
        // * TimedOut → TERMINAL: stop + drop the handle (the handshake-failure
        //   teardown shape), land `starting → failed` with the
        //   `readiness-timeout` cause naming the probe + the honest wait, and
        //   refuse the start. NO Restart Policy consult — an adapter that
        //   survived startup but never declared ready is a broken deployment;
        //   a restart would replay the same failure.
        // * ProcessExited → the EXISTING crash path, verbatim: consult the
        //   Restart Policy (`plan_restart` — the write-ahead record exists
        //   from (6)), land the `Crashed` transition, and QUEUE the restart
        //   plan so the very next engine-cadence pass executes it with normal
        //   backoff timing (the start path must return, so it cannot sleep the
        //   backoff itself).
        if let Some(gate) = &readiness_gate {
            // Surfaced, not silent (AI-18): the probe wait is a NEW, operator-
            // visible phase of the start — say the engine is waiting, and on
            // WHAT (the rendered probe), so a start that later times out was
            // already explained live on stderr.
            let probe_note = format!(
                "{}: waiting up to {}ms for the declared readiness probe: {}",
                name.as_str(),
                gate.timeout_ms,
                super::super::readiness::describe(&gate.probe),
            );
            self.emit_diagnostic(&probe_note);
            // The log-contains probe reads the ENGINE-owned agent output log
            // from the pre-spawn cursor — the same file + cursor the usage
            // ingestion uses. Under `capture = "off"` there IS no log: the
            // probe is unsatisfiable there and runs out its budget honestly
            // (the timeout names the probe; the capture-off note above already
            // said output is not captured).
            let probe_log_path: Option<&std::path::Path> = match capture_mode {
                LogCaptureMode::Off => None,
                LogCaptureMode::Full | LogCaptureMode::Metadata => Some(agent_log_path.as_path()),
            };
            let outcome = match super::super::readiness::run_probe(
                gate,
                &self.backend,
                &mut handle,
                probe_log_path,
                usage_cursor,
                &home,
            ) {
                super::super::readiness::ProbeOutcome::TimedOut {
                    description,
                    waited_ms,
                    poll_errors,
                } => {
                    // Review round 1 (E1): the budget expired on the runner's
                    // LAST poll — the process may have died in the window
                    // since that snapshot. ONE re-poll before condemning: an
                    // exit remaps to the crash path below (the same
                    // Restart-Policy consult a wait-caught exit gets), so a
                    // start that failed because the process DIED is never
                    // mislabeled a readiness timeout.
                    match self.backend.poll(&mut handle) {
                        Ok(ProcessStatus::Exited { code }) => {
                            super::super::readiness::ProbeOutcome::ProcessExited(code)
                        }
                        Ok(ProcessStatus::Alive) => {
                            super::super::readiness::ProbeOutcome::TimedOut {
                                description,
                                waited_ms,
                                poll_errors,
                            }
                        }
                        Err(poll_err) => {
                            // Surfaced, not silent (AI-18): the death re-check
                            // could not run, so the timeout conclusion carries
                            // that caveat visibly.
                            self.emit_diagnostic(&format!(
                                "{}: the pre-timeout liveness re-check failed ({poll_err}); \
                                 treating the outcome as a readiness timeout",
                                name.as_str()
                            ));
                            super::super::readiness::ProbeOutcome::TimedOut {
                                description,
                                waited_ms,
                                poll_errors,
                            }
                        }
                    }
                }
                other => other,
            };
            match outcome {
                super::super::readiness::ProbeOutcome::Ready { waited_ms } => {
                    // Review round 1: the pass is SURFACED beside the
                    // wait-begins note above — the operator sees both halves
                    // of the wait (began / passed after how long), and the
                    // ordering (pass strictly BEFORE the `starting →
                    // running` transition below) becomes observable evidence.
                    self.emit_diagnostic(&format!(
                        "{}: readiness probe passed after {}ms: {}",
                        name.as_str(),
                        waited_ms,
                        super::super::readiness::describe(&gate.probe),
                    ));
                }
                super::super::readiness::ProbeOutcome::TimedOut {
                    description,
                    waited_ms,
                    poll_errors,
                } => {
                    // Teardown first (the handshake-failure shape: a short
                    // graceful stop — which also works on a DETACHED handle —
                    // then the drop releases whatever remains).
                    let _ = self.backend.stop(&mut handle, Duration::from_millis(500));
                    // Review round 1 (E2): a graceful window is an offer, not
                    // a guarantee — if the process is STILL alive after it,
                    // escalate to the stop ladder's final leg (a zero-window
                    // stop is SIGKILL + confirm on unix) instead of dropping a
                    // possibly-live handle onto the OS.
                    if matches!(self.backend.poll(&mut handle), Ok(ProcessStatus::Alive)) {
                        if let Err(kill_err) = self.backend.stop(&mut handle, Duration::ZERO) {
                            self.emit_diagnostic(&format!(
                                "{}: readiness-timeout teardown could not force-kill the \
                                 surviving process: {kill_err}",
                                name.as_str()
                            ));
                        }
                    }
                    drop(handle);
                    let mut detail = format!(
                        "{description} did not pass within its {waited_ms}ms budget — the \
                         instance is failed and will NOT be restarted; check the agent's \
                         readiness mechanism (the probe targets the instance's own home, \
                         logs, or listener)"
                    );
                    // Review round 1 (E5): the runner tolerates a liveness
                    // poll ERROR as alive, so a timeout that watched a
                    // degraded death-check says so.
                    if poll_errors > 0 {
                        detail.push_str(&format!(
                            " (process poll errored {poll_errors} times during the wait)"
                        ));
                    }
                    return Err(self.fail_launch_with_cause(
                        registry,
                        &name,
                        TransitionCause::readiness_timeout(description, waited_ms),
                        detail,
                    ));
                }
                super::super::readiness::ProbeOutcome::ProcessExited(code) => {
                    // The process died while the probe watched — caught by a
                    // wait poll, or by the pre-timeout re-check above: either
                    // way the crash path, verbatim. The poll already reaped
                    // it (the same "reap done by poll" fact the survival
                    // watch relies on), so no stop is needed — drop the
                    // handle.
                    let base_detail = match code {
                        Some(c) => {
                            format!("process exited during the readiness probe with code {c}")
                        }
                        None => "process exited during the readiness probe (terminated by \
                                 signal)"
                            .to_string(),
                    };
                    let decision = self.plan_restart(registry, &name, &base_detail);
                    let crash_log_capture = self.backend.log_capture(&handle);
                    if self
                        .transition_with_log_capture(
                            registry,
                            &name,
                            starting,
                            LifecycleState::Failed,
                            TransitionCause::crashed(decision.crash_cause.clone()),
                            crash_log_capture,
                        )
                        .is_err()
                    {
                        // The transition persist failed: the write-ahead
                        // record survives for a later reconcile (the reaper's
                        // posture) — but the start is still refused, because
                        // an exited process is never `running`.
                    }
                    // The plan is queued whether or not the row write landed:
                    // the crash is the durable world's truth (the reaper
                    // reconciles toward it), and a restart attempt against a
                    // reconciled row is honest — skipping it on a store hiccup
                    // would strand the policy's promise.
                    if let Some(plan) = decision.plan {
                        // Review round 1 (E3): surfaced, not silent — QUEUED
                        // is not EXECUTED. The next engine-cadence pass runs
                        // this plan with normal backoff; a host that exits
                        // before the next tick (a short-lived `kt agent
                        // start` that returns here) drops it. Say both facts.
                        self.emit_diagnostic(&format!(
                            "{}: restart queued (attempt {}, backoff {}ms) — it runs on the \
                             next engine-cadence pass; a host that exits before that pass \
                             does not restart it",
                            name.as_str(),
                            plan.attempt,
                            plan.delay.as_millis(),
                        ));
                        self.queue_pending_restart(plan);
                    }
                    drop(handle);
                    return Err(EngineError::LaunchFailed {
                        name: name.as_str().to_string(),
                        detail: decision.crash_cause,
                    });
                }
            }
        }

        // (7) starting → running (adapter ready, or a Restart Policy restart).
        let ready_cause = match restart {
            Some((attempt, waited)) => {
                TransitionCause::restarted(attempt, waited.as_millis() as u64)
            }
            None => TransitionCause::AdapterReady,
        };
        // Story 4-2, Task 4: `handle` already exists (spawned above) and
        // carries a live `log_capture` (capture is unconditional, AC-E), but
        // it is not YET in `self.running` (inserted below) — so the default
        // `self.transition(...)`'s `self.running`-based lookup would miss
        // it. Pass the capture explicitly so the `starting → running` line
        // lands in the attributed capture too.
        self.transition_with_log_capture(
            registry,
            &name,
            starting,
            LifecycleState::Running,
            ready_cause,
            self.backend.log_capture(&handle),
        )?;
        // The Run id was minted BEFORE the record commit (story 15-2 — the
        // durable Run identity; see step (6)): each `starting` — operator start
        // OR restart (story 1-6) — still gets a DISTINCT id (AC-B), so a
        // restarted instance opens a NEW Run whose per-run totals never bleed
        // in the previous Run's usage. The ingestion cursor was anchored at the
        // pre-spawn log length (above), so this Run ingests all of its own
        // output.
        // Story 3-4: an `engine-observed` instance holds its listener + a fresh
        // per-Run observed `sequence` minter (built here with the just-minted
        // run_id, so the ordinal resets per Run — the AD-7 Run boundary + the dedup
        // invariant). A `self-reported` instance leaves both `None` (its log-tail
        // drain is unchanged).
        let observed_source = observed_listener
            .as_ref()
            .map(|_| ObservedUsageSource::new());
        // A fresh Run starts from a ZERO poll-error streak (AI-12): a stale entry
        // from this name's PRIOR handle must never pre-load the new one.
        self.clear_poll_error_streak(&name);
        self.running.insert(
            name.clone(),
            Supervised {
                handle,
                run_id,
                metering_source,
                usage_cursor,
                usage_park_attempts: None,
                // Story 14-3 (T3): the stderr sentinel channel's fresh
                // cursor + park (acp-only in practice; zero for the rest).
                stderr_usage_cursor,
                stderr_usage_park_attempts: None,
                // A fresh Run has seen no sentinel lines yet (the gap
                // notice's honest "no lines seen" until one arrives).
                sentinel_lines_seen: false,
                // Story 14-3 (T2): carried beside the handle so the drain
                // cadence gates the stderr channel without a re-read.
                is_acp,
                // Story 12-4: a fresh Run starts with no parked observed events.
                observed_park: None,
                // A fresh Run starts with an EMPTY breach latch (story 3-2): the
                // run_id was just minted, so no scope has fired for it yet. This is
                // how the latch RESETS per Run — a persistently-over-cumulative agent
                // that stops and starts again gets a new Run + a clean latch, so it
                // can fire one cumulative breach in the new Run too.
                breached_scopes: std::collections::HashSet::new(),
                observed_listener,
                observed_source,
                // A fresh start's stop attempt has not happened yet.
                stop_unconfirmed: false,
                // A fresh start (operator or restart) spawned this process itself.
                adopted: false,
                // Story 17-4 (review finding 11): the capture posture THIS
                // spawn ran under (resolved above, threaded into the
                // SpawnSpec) — the per-Run fact the metadata-mode unlink
                // consults, so a host's CURRENT mode never deletes another
                // engine's files.
                capture_mode: Some(capture_mode),
                // Story 14-1: the acp transport lives with the handle (a fresh
                // session for this Run; teardown drops with the Supervised).
                acp,
                // Story 18-2: the declared liveness check (resolved in the
                // (2b-liveness) block above) + its FRESH failure window — a
                // new Run grants the agent a whole new window, the restart
                // resets the streak by construction.
                liveness_check,
                liveness_failing_since: None,
            },
        );

        registry.lookup(&name).map_err(registry_to_engine)
    }

    /// Start the loopback forward listener for an `engine-observed` instance
    /// (story 3-4, AC-A/AC-B/AC6), or return `Ok(None)` for a `self-reported`
    /// instance (whose start path is UNCHANGED). Runs at `starting`, BEFORE any
    /// persisted state change, so a failure rejects the start cleanly.
    ///
    /// For an `engine-observed` instance it: (1) resolves the operator-configured
    /// real upstream provider URL (`metering.upstream_base_url`) from `effective`;
    /// (2) requires the engine runtime handle (the listener's accept loop runs on
    /// it) — absent → a clear error (only the handle-less unit-test supervisor lacks
    /// it, and it never starts an observed instance); (3) binds `127.0.0.1` on the
    /// Host seam's port (loopback ONLY — AC-B; port 0 = ephemeral, the pre-17-2
    /// default) and spawns the accept loop. Every failure maps to a
    /// TRAFFIC-FREE [`EngineError::ObservedMetering`] (no body/header/key — 2-4
    /// no-leak). The returned [`ObservedListener`] is moved into `Supervised`; its
    /// `base_url` is what the caller injects via the config-mapping (AC6).
    pub(super) fn start_observed_listener(
        &self,
        name: &InstanceName,
        metering_source: &str,
        effective: &crate::domain::EffectiveConfig,
    ) -> Result<Option<ObservedListener>, EngineError> {
        // Only an `engine-observed` instance runs a listener. `self-reported`
        // (and any other) leaves it None — its start path is byte-unchanged.
        if metering_source != "engine-observed" {
            return Ok(None);
        }
        // The operator MUST configure the real upstream provider URL (there is
        // nowhere to forward otherwise). Absent → a clear start error naming the key.
        let upstream = config::resolve_upstream_base_url(effective).ok_or_else(|| {
            EngineError::ObservedMetering {
                name: name.as_str().to_string(),
                detail: format!(
                    "no upstream provider URL configured; set `{}` to the agent's real \
                     OpenAI-compatible endpoint",
                    crate::domain::METERING_UPSTREAM_BASE_URL_KEY
                ),
            }
        })?;
        // The listener's accept loop runs on the engine runtime; the sync start path
        // (on the blocking pool) cannot use `Handle::current`, so the engine threads
        // its handle in (`with_runtime`). A handle-less supervisor cannot observe.
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| EngineError::ObservedMetering {
                name: name.as_str().to_string(),
                detail: "the engine has no runtime handle to run the loopback listener \
                     (engine-observed metering requires the async engine)"
                    .to_string(),
            })?;
        // Bind loopback + spawn. The PORT comes from the story-17-2 Host seam
        // (`metering_listen_port`: 0 = the OS-ephemeral default every pre-17-2
        // caller takes; non-zero = the Host's pinned `metering.listen` port, so
        // restarts/adoption re-bind the SAME port). The ADDRESS stays
        // engine-computed loopback (AC-B). A ListenerError is TRAFFIC-FREE by
        // construction (bind/upstream-shape only — never a body/header/key), so
        // mapping it into the detail cannot leak a secret (2-4 rigor); a pinned
        // port that is already taken surfaces here as the typed bind error
        // (AI-18 — never a silent hop).
        let listener =
            ObservedListener::start_on_port(runtime, upstream, self.metering_listen_port).map_err(
                |e: ListenerError| EngineError::ObservedMetering {
                    name: name.as_str().to_string(),
                    detail: e.to_string(),
                },
            )?;
        Ok(Some(listener))
    }

    /// Watch a freshly spawned process for [`READINESS_WINDOW`]. Returns
    /// `Some(exit_code)` if the process died within the window (a launch failure,
    /// AC2 — the inner `Option<i32>` is the OS exit code, `None` if killed by a
    /// signal with no code), or `None` if it stayed alive the whole window
    /// (ready). Reaps on exit (no zombie).
    fn watch_startup(&self, handle: &mut backends::Handle) -> Option<Option<i32>> {
        let deadline = std::time::Instant::now() + STARTUP_SURVIVAL_WINDOW;
        loop {
            match self.backend.poll(handle) {
                Ok(ProcessStatus::Exited { code }) => return Some(code),
                Ok(ProcessStatus::Alive) => {}
                // A poll error during startup is treated as still-alive; the next
                // stop/poll will surface a real problem. Don't fail the start on a
                // transient poll hiccup.
                Err(_) => {}
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(READINESS_POLL);
        }
    }
}
