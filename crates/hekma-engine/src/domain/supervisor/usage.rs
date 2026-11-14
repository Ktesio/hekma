//! usage family — split out of the supervisor monolith by story 13-3
//! (epic-13; boundary ratified 2026-09-19, study: Option B). Pure move:
//! additional `impl Supervisor` block, no signature or behavior change.

use super::*;
use crate::domain::pipeline;
use crate::domain::{CostCap, Rate, TokenBudget};

/// WHICH captured-output channel a self-reported usage drain reads (story
/// 14-3, T3). The two channels are byte-identical in SEMANTICS (the same
/// [`USAGE_SENTINEL_PREFIX`] convention, the same cursor + AI-41 park
/// discipline, the same commit choke point) and differ only in FILE and
/// ownership:
///
/// * [`UsageChannel::Stdout`] — every kind's `agent.log` (the stdout capture;
///   for an `acp` instance this is fed by the ACP connection's reader, whose
///   lines are protocol, never sentinels — stdout purity, spine AD-19).
/// * [`UsageChannel::Stderr`] — the `agent-stderr.log` capture; the acp
///   kind's sentinel channel, because its stdout is the protocol stream and
///   "no reading of agent stdout as anything but ACP" is a Never rule. A
///   cooperative ACP agent therefore emits `KTESIO_USAGE {json}` on STDERR,
///   which the backend's crash-immune direct redirect lands in this file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsageChannel {
    /// The captured stdout log (`agent.log`) — every kind's sentinel channel.
    Stdout,
    /// The captured stderr log (`agent-stderr.log`) — the acp kind's sentinel
    /// channel (story 14-3, T3).
    Stderr,
}

impl UsageChannel {
    /// The captured log FILE this channel drains.
    fn log_path(&self, registry: &Registry, name: &InstanceName) -> std::path::PathBuf {
        match self {
            UsageChannel::Stdout => registry.agent_output_log_path(name),
            UsageChannel::Stderr => registry.agent_stderr_log_path(name),
        }
    }

    /// The channel's byte cursor on `supervised`.
    fn cursor(&self, supervised: &Supervised) -> u64 {
        match self {
            UsageChannel::Stdout => supervised.usage_cursor,
            UsageChannel::Stderr => supervised.stderr_usage_cursor,
        }
    }

    /// Set the channel's byte cursor on `supervised`.
    fn set_cursor(&self, supervised: &mut Supervised, cursor: u64) {
        match self {
            UsageChannel::Stdout => supervised.usage_cursor = cursor,
            UsageChannel::Stderr => supervised.stderr_usage_cursor = cursor,
        }
    }

    /// The channel's AI-41 park state on `supervised`.
    fn park(&self, supervised: &Supervised) -> Option<(u64, u32)> {
        match self {
            UsageChannel::Stdout => supervised.usage_park_attempts,
            UsageChannel::Stderr => supervised.stderr_usage_park_attempts,
        }
    }

    /// Set the channel's AI-41 park state on `supervised`.
    fn set_park(&self, supervised: &mut Supervised, park: Option<(u64, u32)>) {
        match self {
            UsageChannel::Stdout => supervised.usage_park_attempts = park,
            UsageChannel::Stderr => supervised.stderr_usage_park_attempts = park,
        }
    }

    /// The metering-source string stamped on this channel's ingested events.
    /// Stdout keeps the instance's DECLARED source (byte-identical to the
    /// pre-14-3 behavior). Stderr stamps `self-reported` EXPLICITLY: a
    /// sentinel line is by definition a SELF-REPORT, so an acp instance that
    /// opted into the observed channel (declared `engine-observed`) still
    /// stamps its stderr sentinel rows with the channel that actually
    /// produced them — the ledger names the honest provenance, never the
    /// instance's declaration alone.
    fn metering_source(&self, supervised: &Supervised) -> String {
        match self {
            UsageChannel::Stdout => supervised.metering_source.clone(),
            UsageChannel::Stderr => "self-reported".to_string(),
        }
    }
}

impl Supervisor {
    /// Drain self-reported usage from EVERY currently-running instance (the reaper
    /// cadence). Best-effort per instance — one instance's drain failure never
    /// blocks another's or crash detection.
    ///
    /// This is the MID-RUN cadence: the process is (believed) still alive, so a
    /// half-written final line is left for the next pass ([`DrainMode::MidRun`]).
    ///
    /// Story 14-3 (T3): an `acp` instance drains BOTH its self-reported
    /// channels — the captured stdout log (protocol lines; no sentinels can
    /// live there) AND the captured stderr log (its sentinel channel). The
    /// per-channel drains are independent cursors over independent files, so
    /// a line can never be ingested twice.
    pub(super) fn drain_usage_all(&mut self, registry: &Registry) {
        // Story 21-3 (review round 2): the cross-process policy-cache check
        // rides the tick (one tiny aggregate read — never per event). A
        // degraded reload is SURFACED here, once per continuous degraded
        // state (the open notes are drained only at open and would grow
        // unbounded); a healthy tick re-arms the notice (and a failed load
        // retries next tick). A successful RELOAD re-checks this engine's
        // supervised Runs: another engine's raise unlatched rows (its own
        // fence reasons must lift), and its lowering or new ceiling fires
        // here under this engine's fence and lifecycle (round 3, M1).
        let refresh = registry.refresh_policy_cache();
        match refresh.degraded {
            Some(note) => {
                if !self.policies_degraded_noted {
                    self.policies_degraded_noted = true;
                    self.emit_diagnostic(&format!(
                        "the Policies could not be reloaded: {note} — enforcement continues \
                         under the last good set with the breach action held at pause"
                    ));
                }
            }
            None => {
                if self.policies_degraded_noted && !registry.policy_cache_degraded() {
                    self.policies_degraded_noted = false;
                }
            }
        }
        let names: Vec<InstanceName> = self.running.keys().cloned().collect();
        if refresh.reloaded {
            self.recheck_after_policy_reload(registry, &names);
        }
        for name in names {
            self.drain_self_reported_for(registry, &name, DrainMode::MidRun);
        }
    }

    /// Re-check the supervised Runs after the policy set reloaded (story
    /// 21-3, review round 3 M1): another engine's change may have
    /// unlatched a Run (lift this engine's fence reasons whose latch rows
    /// are gone) or lowered a ceiling / added one this engine supervises
    /// (evaluate now, under this engine's own fence and lifecycle — the
    /// foreign engine cannot latch what it cannot execute).
    fn recheck_after_policy_reload(&mut self, registry: &Registry, names: &[InstanceName]) {
        for name in names {
            let Some(supervised) = self.running.get(name) else {
                continue;
            };
            let Some(run_id) = registry.open_process_run(name).ok().flatten() else {
                continue;
            };
            // Story 23-1: a foreign raise that unlatched a reason reaches
            // the proxy through the snapshot (the data-version reload
            // republishes the Run's entry); nothing to lift here.
            // Evaluate: a lowered or newly-added ceiling this engine
            // supervises fires here (enforce_budget is idempotent under
            // the latch rows).
            let metering_source = supervised.metering_source.clone();
            self.enforce_budget(registry, name, &run_id, &metering_source);
        }
    }

    /// Drain BOTH self-reported sentinel channels for `name` under `mode`:
    /// the captured stdout log (every kind, the story-3-1 channel) plus — for
    /// an `acp` instance only — the captured stderr log (story 14-3, T3).
    /// The stop/crash-reap TERMINAL callers route through here too, so a
    /// final stderr sentinel line flushed without a newline is not stranded
    /// (the H1 rule, per channel).
    pub(super) fn drain_self_reported_for(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        mode: DrainMode,
    ) {
        self.drain_usage_for(registry, name, mode);
        if self.running.get(name).is_some_and(|s| s.is_acp) {
            self.drain_stderr_usage_for(registry, name, mode);
        }
    }

    /// Drain the NEWLY-captured tail of one instance's captured stdout log
    /// (the story-3-1 channel), ingesting each well-formed usage sentinel line
    /// through the commit choke point ([`Supervisor::ingest_usage`]), and
    /// advance the read cursor. The acp kind's STDERR twin is
    /// [`Supervisor::drain_stderr_usage_for`] (story 14-3); both share the
    /// channel-generic [`Supervisor::drain_channel_usage_for`].
    ///
    /// Reads from the per-instance cursor to the file's end (only the bytes written
    /// since the last drain), parses usage lines via the self-reported
    /// [`UsageSource`](crate::ports::UsageSource), and records each. A read error
    /// (log gone / unreadable) is a best-effort skip — the DB is the source of
    /// truth, and the next pass retries. Malformed usage lines are skipped inside
    /// the parser (a diagnostic, never fatal — AD-12).
    ///
    /// **AI-41 (billing honesty): the cursor only advances past DURABLE bytes.**
    /// Every parsed event is ingested FIRST; the cursor moves past the consumed
    /// block only when every event committed (or was a recognized duplicate
    /// replay). A store error parks the cursor where it was — the failed event is
    /// retried on the next drain, and already-committed neighbors in the same
    /// block re-drift safely into the DB dedup key (`DuplicateReplay`, never a
    /// double-count). The pre-fix advance-then-ingest order silently dropped any
    /// event whose INSERT failed.
    ///
    /// The `mode` decides how the TAIL is treated (story 3-1 under-count fix, H1):
    /// * [`DrainMode::MidRun`] — the process may still be mid-`writeln!`, so only
    ///   bytes UP TO the last newline are consumed; a partial trailing line waits
    ///   for the next drain (it lands whole then).
    /// * [`DrainMode::Terminal`] — the process is DEAD (drain-on-stop / drain-on-
    ///   reap); no more bytes will ever append, so a final usage line flushed
    ///   WITHOUT a trailing newline is consumed to end-of-log rather than stranded
    ///   (which the next Run's cursor would skip past → a permanent under-count).
    ///
    /// Log-shrink guard (M2): if the file is shorter than the cursor (a truncate /
    /// rotation — nothing in-tree does this yet; Epic 4 owns rotation), we do NOT
    /// re-read from 0 under the same live `run_id` (that would re-ingest already-
    /// counted lines → a double-count, an INFLATED bill). We instead treat it as an
    /// anomaly: advance the cursor to the new length and ingest nothing this pass.
    /// Proper rotation handling is deferred to Epic 4.
    pub(super) fn drain_usage_for(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        mode: DrainMode,
    ) {
        self.drain_channel_usage_for(registry, name, mode, UsageChannel::Stdout);
    }

    /// The acp kind's STDERR sentinel drain (story 14-3, T3, spine AD-19): the
    /// byte-identical twin of [`Supervisor::drain_usage_for`] over the captured
    /// stderr log. An `acp` instance's stdout is the ACP protocol stream (stdout
    /// purity is a Never rule), so a cooperative agent's `KTESIO_USAGE {json}`
    /// lines arrive on STDERR — captured by the backend's crash-immune direct
    /// redirect into `agent-stderr.log`, where this drain tail-reads them under
    /// the SAME cursor + AI-41 park discipline. Events stamp `self-reported`
    /// (the channel's own truth — see [`UsageChannel::metering_source`]).
    pub(super) fn drain_stderr_usage_for(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        mode: DrainMode,
    ) {
        self.drain_channel_usage_for(registry, name, mode, UsageChannel::Stderr);
    }

    /// The channel-generic implementation both self-reported drains share. Every
    /// behavior below (the AI-63 tail read, the H1/M2 `plan_drain` rules, the
    /// AI-41 ingest-before-advance + park/bound discipline) is byte-identical
    /// per channel; only the file, the cursor/park state, and the stamped
    /// metering source differ (see [`UsageChannel`]).
    fn drain_channel_usage_for(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        mode: DrainMode,
        channel: UsageChannel,
    ) {
        // Only running/adopted instances have a cursor + metering context.
        let (cursor, run_id, metering_source) = match self.running.get(name) {
            Some(s) => (
                channel.cursor(s),
                s.run_id.clone(),
                channel.metering_source(s),
            ),
            None => return,
        };
        let path = channel.log_path(registry, name);
        // AI-63 (billing-critical stall fix): read ONLY the tail written since the
        // last drain (`[cursor, len)`) — NEVER the whole never-rotated `agent.log`.
        // `read_usage_tail` returns the SAME bytes the old whole-file read's
        // `bytes[cursor..]` slice held (and catches the M2 shrink, where
        // `len - cursor` would underflow), so the BILLING decision below —
        // `plan_drain` on those exact tail bytes — is byte-identical to before.
        // See `read_usage_tail`'s docs for the full equivalence + snapshot proof.
        let tail = match read_usage_tail(&path, cursor) {
            // Read error: best-effort skip, cursor untouched, retry next pass —
            // identical to the old `let Ok(bytes) = std::fs::read(..) else { return }`.
            UsageTail::Unavailable => return,
            // M2 shrink guard: snap the cursor to the file's new (shorter) length
            // and ingest nothing — identical to the old `DrainPlan::Shrunk { .. }`
            // arm (never re-read from 0 under the same live `run_id` → no
            // double-count → no inflated bill).
            UsageTail::Shrunk { new_cursor } => {
                if let Some(s) = self.running.get_mut(name) {
                    channel.set_cursor(s, new_cursor);
                }
                return;
            }
            UsageTail::Tail { bytes } => bytes,
        };
        // `tail` == the old code's `bytes[cursor..]`. Feeding it to `plan_drain`
        // with a 0 base makes the SAME `(bytes, cursor, mode)` decision on the same
        // bytes, just in tail-relative coordinates: the returned `range` slices
        // `tail` directly (0-based), and the returned count is added to `cursor` to
        // recover the ABSOLUTE cursor. The MidRun (up-to-last-newline) and Terminal
        // (whole tail, incl. a newline-less final line — H1) rules are computed
        // purely from these bytes, so BOTH are preserved unchanged.
        match plan_drain(&tail, 0, mode) {
            DrainPlan::Consume {
                range,
                new_cursor: consumed,
            } => {
                let block = String::from_utf8_lossy(&tail[range]);
                let parsed = self.shared.usage_source.drain(&block);
                // Story 14-3 (T3): a parsed sentinel line on EITHER channel is
                // the instance's "sentinel: lines seen" state — the honest gap
                // notice reports it instead of "no lines seen" once any line
                // has arrived this Run. Recorded at PARSE time (not commit):
                // "seen" is about the agent's behavior, not the store's.
                if !parsed.is_empty() {
                    if let Some(s) = self.running.get_mut(name) {
                        s.sentinel_lines_seen = true;
                    }
                }
                // AI-41 (billing honesty): ingest FIRST; advance the cursor past
                // the consumed block ONLY when every parsed event committed (or
                // was a recognized duplicate replay). The pre-fix code advanced
                // the cursor BEFORE ingesting, so a store error silently DROPPED
                // the event(s) behind it — usage that never reached the ledger
                // and was never retried. Now a store error PARKS the cursor
                // where it was: the failed event is retried on the next drain,
                // and any already-committed neighbor in the same block re-drifts
                // safely into the DB dedup key (`DuplicateReplay` — never a
                // double-count; see the design note on AI-41's dedup safety).
                let mut committed_count = 0usize;
                let mut busy = false;
                for usage in &parsed {
                    match self.ingest_usage(registry, name, &run_id, &metering_source, usage, mode)
                    {
                        Ok(_) => committed_count += 1,
                        Err(err) => {
                            // Park at the FIRST store error — later events in
                            // this block are not even attempted, so they cannot
                            // leapfrog the failed one.
                            busy = is_busy(&err);
                            break;
                        }
                    }
                }
                // `cursor + consumed` equals the old whole-file path's
                // `new_cursor` (which was `cursor + consumed`) exactly —
                // reached only when every event behind the advance is durable.
                if committed_count == parsed.len() {
                    if let Some(s) = self.running.get_mut(name) {
                        channel.set_cursor(s, cursor + consumed);
                        channel.set_park(s, None);
                    }
                } else if mode == DrainMode::Terminal {
                    // AI-41 (loop 1): on the TERMINAL drain there IS no next
                    // pass — the handle is being removed right after, and with
                    // it the cursor and the Run context. The park-and-retry
                    // claim must not silently fail exactly where loss is
                    // likeliest: say the batch is lost, out loud.
                    let lost = parsed.len() - committed_count;
                    let loss = format!(
                        "{}: {lost} usage event(s) in the final drained block could not be \
                         committed to the Usage Ledger and were NOT counted — this is the \
                         terminal drain (the process is dead or the handle is being removed), \
                         so the batch cannot be retried and is lost",
                        name.as_str(),
                    );
                    self.emit_diagnostic(&loss);
                } else {
                    // AI-41 (loop 2): BOUND the park. A permanently failing
                    // event (a row poisoned beyond what the dedup key covers)
                    // would otherwise wedge the cursor at this offset forever —
                    // silently stranding every later usage event for the Run.
                    // After USAGE_PARK_MAX_ATTEMPTS failed passes AT THE SAME
                    // offset, skip the block with a loud diagnostic: billing
                    // honesty cuts both ways — announce the loss, don't strand
                    // the ledger.
                    //
                    // Story 21-1: a `Busy` refusal (the writer's 1 s bound under
                    // another process's write lock) is contention, never a
                    // poisoned row — it is retried without spending an attempt,
                    // so bounded contention can never skip billable usage.
                    let attempts = match self.running.get(name).and_then(|s| channel.park(s)) {
                        Some((parked_cursor, n)) if parked_cursor == cursor => {
                            if busy {
                                n
                            } else {
                                n + 1
                            }
                        }
                        _ => u32::from(!busy),
                    };
                    if attempts >= USAGE_PARK_MAX_ATTEMPTS {
                        let lost = parsed.len() - committed_count;
                        if let Some(s) = self.running.get_mut(name) {
                            channel.set_cursor(s, cursor + consumed);
                            channel.set_park(s, None);
                        }
                        let skip = format!(
                            "{}: {lost} usage event(s) at byte offset {cursor} failed to \
                             commit on {attempts} consecutive drains and are SKIPPED (not \
                             counted) — the cursor moves past them so the rest of the Run's \
                             usage keeps counting; investigate the Usage Ledger store",
                            name.as_str(),
                        );
                        self.emit_diagnostic(&skip);
                    } else if let Some(s) = self.running.get_mut(name) {
                        channel.set_park(s, Some((cursor, attempts)));
                    }
                }
            }
            // Nothing to consume this pass (an empty tail, or a MidRun tail with no
            // newline yet) — leave the cursor where it is, exactly as before.
            DrainPlan::Nothing => {}
            // Unreachable by construction: `plan_drain` returns `Shrunk` only when
            // its `cursor` argument exceeds the slice length, and the base here is
            // 0 (`0 > len` is impossible). The REAL shrink is handled above in
            // `read_usage_tail`, where the file length is known WITHOUT a whole-file
            // read. Leave the cursor untouched (nothing was ingested, so no
            // miscount) — a billing path must never panic.
            DrainPlan::Shrunk { .. } => {}
        }
        // Story 17-4 (review findings 9/10): BOTH channels' raw files get the
        // coordinated truncate (`agent.log` AND `agent-stderr.log`), and the
        // check runs on the TERMINAL drain too — a cap-crossing burst between
        // the last mid-run tick and stop must not leave the file over-cap at
        // rest forever. The terminal check runs BEFORE the metadata-mode
        // unlink and the at-rest handoff; the process is dead by then, so no
        // appends race it.
        if mode == DrainMode::MidRun || mode == DrainMode::Terminal {
            self.maybe_truncate_raw_log(registry, name, channel, mode);
        }
    }

    /// The PURE truncate decision (story 17-4; the [`should_rotate`]-style
    /// seam — unit-tested without I/O): the raw `agent.log` has reached its
    /// cap and must be truncated. Cap-as-parameter so tests drive the
    /// coordinated-truncate path at toy sizes. `pub(super)` — the sibling
    /// `tests` module pins the seam directly.
    pub(super) fn should_truncate(file_len: u64, cap: u64) -> bool {
        file_len >= cap
    }

    /// The COORDINATED raw-log truncate (story 17-4, the AD-12 amendment;
    /// review findings 8/9/10 generalized it): per CHANNEL (`agent.log` /
    /// `agent-stderr.log`) and per DRAIN MODE (mid-run AND terminal — a
    /// cap-crossing burst between the last mid-run tick and stop must not
    /// leave the file over-cap at rest). The child's stdout/stderr fds are
    /// inode-pinned to their direct-redirect files, so rename-generations
    /// cannot bound the live files — but the raw opens are O_APPEND, so
    /// TRUNCATING to 0 at a drain tick is safe the moment BOTH consumers sit
    /// past the cut:
    ///
    /// * the usage-drain cursor (this channel's) — this drain just ran, so
    ///   the cursor is as caught-up as it gets this tick;
    /// * the capture tailer's cursor for the same stream — polled live.
    ///
    /// If either lags (a partial trailing line unconsumed, a tailer pass
    /// behind), truncation is DEFERRED — never forced (unread bytes would be
    /// silently destroyed). On the TERMINAL drain there is no next tick, so
    /// the tailer gets one forced catch-up pass first (`LogCapture::
    /// catch_up_now`) — after death no more bytes arrive, so the cursor
    /// catches up fully and the truncate fires instead of deferring into an
    /// at-rest over-cap file.
    ///
    /// When both are past the cut the engine truncates to 0, snaps BOTH
    /// cursors to 0 (so post-truncate appends are ingested/tailed from byte
    /// 0 — no Shrunk skip, no double-count), and emits ONE diagnostic notice
    /// per truncation (surfaced, not silent). The truncate + the tailer
    /// cursor snap happen UNDER ONE hold of the capture's cursor mutex
    /// (review finding 8): a tailer pass can never observe the truncated
    /// file with a pre-truncate cursor (its shrink guard would swallow the
    /// post-truncate appends). A truncate FAILURE is latched (review
    /// finding 14): the deferral notice fires ONCE per continuous failure
    /// state and re-arms only when a truncate succeeds — a persistently
    /// failing `set_len` must not re-announce every tick. Best-effort: a
    /// failure is a surfaced diagnostic and a deferral, never a failed drain.
    ///
    /// Known (accepted, documented) race: a child appending between the
    /// length check and the `set_len(0)` can lose those few bytes. The window
    /// is microseconds; O_APPEND writes after the truncate continue cleanly
    /// from the new end. A billing-correct alternative does not exist without
    /// kernel support (the fd pin forbids rename-generations).
    fn maybe_truncate_raw_log(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        channel: UsageChannel,
        mode: DrainMode,
    ) {
        let capture = self.log_capture_for(name);
        if mode == DrainMode::Terminal {
            // The tailer's last chance: fold everything the dead process
            // wrote into the attributed capture BEFORE the at-rest check.
            if let Some(capture) = &capture {
                capture.catch_up_now();
            }
        }
        let path = channel.log_path(registry, name);
        let Ok(len) = std::fs::metadata(&path).map(|m| m.len()) else {
            return; // no file (Off mode never creates one; absence is fine)
        };
        if !Self::should_truncate(len, self.agent_log_cap()) {
            self.clear_truncate_deferral(channel);
            return;
        }
        // Consumer 1: the usage cursor. It must sit at (or past) the file's
        // end — a lagging cursor means unread bytes; defer.
        let usage_cursor = match self.running.get(name) {
            Some(s) => channel.cursor(s),
            None => return,
        };
        if usage_cursor < len {
            return;
        }
        // Consumer 2: the capture tailer. An ADOPTED handle has no capture
        // (and no tailer thread) — there is no second cursor to lag, so the
        // file's only reader is the usage drain, already verified above. An
        // Off-mode instance has no file at all (returned above).
        let tailer_cursor = match &capture {
            Some(capture) => match channel {
                UsageChannel::Stdout => capture.stdout_tail_cursor(),
                UsageChannel::Stderr => capture.stderr_tail_cursor(),
            },
            None => len,
        };
        if tailer_cursor < len {
            return; // tailer lagging — deferred (mid-run: to the next tick)
        }
        // Both consumers past the cut: truncate, snap the tailer's cursor
        // under the SAME lock hold, snap this drain's cursor, surface.
        let file_label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "the raw capture file".to_string());
        let truncated = match &capture {
            Some(capture) => capture.truncate_raw_and_snap(&path, channel == UsageChannel::Stdout),
            None => std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .ok()
                .and_then(|file| file.set_len(0).ok()),
        };
        match truncated {
            Some(()) => {
                self.clear_truncate_deferral(channel);
                if let Some(s) = self.running.get_mut(name) {
                    channel.set_cursor(s, 0);
                }
                let truncated_notice = format!(
                    "{}: {} reached the {}MB capture bound and was truncated to 0 \
                     (usage cursor and tailer were both past the cut; metering is \
                     double-count-free)",
                    name.as_str(),
                    file_label,
                    self.agent_log_cap() / (1024 * 1024)
                );
                self.emit_diagnostic(&truncated_notice);
            }
            None => {
                // Review finding 14: latch the deferral — ONE notice per
                // continuous failure state, re-armed by the next success.
                if self.note_truncate_deferral(channel) {
                    let truncate_deferred = format!(
                        "{}: {} reached the {}MB capture bound but the truncate failed; \
                         deferring (this notice latches until a truncate succeeds)",
                        name.as_str(),
                        file_label,
                        self.agent_log_cap() / (1024 * 1024)
                    );
                    self.emit_diagnostic(&truncate_deferred);
                }
            }
        }
    }

    /// The truncate-deferral latch, per channel (review finding 14): records
    /// a failed truncate and returns whether this is the FIRST failure of a
    /// continuous state (the caller emits only then).
    fn note_truncate_deferral(&mut self, channel: UsageChannel) -> bool {
        match channel {
            UsageChannel::Stdout => std::mem::replace(&mut self.agent_log_truncate_deferred, true),
            UsageChannel::Stderr => {
                std::mem::replace(&mut self.agent_stderr_truncate_deferred, true)
            }
        }
    }

    /// Re-arm the truncate-deferral latch (a truncate succeeded — a fresh
    /// failure may announce itself again).
    fn clear_truncate_deferral(&mut self, channel: UsageChannel) {
        match channel {
            UsageChannel::Stdout => self.agent_log_truncate_deferred = false,
            UsageChannel::Stderr => self.agent_stderr_truncate_deferred = false,
        }
    }

    /// Unlink the instance's TWO raw capture files in METADATA mode (story
    /// 17-4) — the at-rest bound: the raw redirects exist only TRANSIENTLY
    /// while the process runs (the crash-immune direct-redirect mechanism
    /// requires the files; the pipe alternative is the documented
    /// review-of-#80 liveness regression). Called at every TERMINAL path
    /// AFTER the final usage drain (metadata parses before unlink — the
    /// sentinel metering has already had its last pass), so a stopped Run
    /// leaves NO agent content on disk: only the metadata-shaped `output.log`
    /// (and the instance/breach logs) remain.
    ///
    /// The gate is the RUN's recorded spawn-time capture mode (review
    /// finding 11) — `Some(Metadata)`, meaning THIS engine spawned the
    /// process under metadata — never the engine's CURRENT mode: an adopted
    /// Run (`None`, or any non-metadata posture) keeps its raw logs, because
    /// a host's mode must not destroy files another engine's Run wrote. A
    /// Run-less call (no `running` entry at all) falls back to the engine's
    /// mode: production callers (stop, reap) always hold the entry — the
    /// fallback exists for direct callers that have no Run to consult, and
    /// degrades to exactly the pre-finding-11 behavior there.
    ///
    /// Best-effort with SURFACED failure (AI-18): a missing file is the
    /// ordinary Off-adjacent case (never created) and stays silent; any other
    /// unlink failure emits a diagnostic naming the path and the error, and
    /// never fails the stop/reap itself.
    pub(super) fn unlink_raw_logs_if_metadata(&self, registry: &Registry, name: &InstanceName) {
        let metadata = match self.running.get(name).map(|s| s.capture_mode) {
            Some(Some(LogCaptureMode::Metadata)) => true,
            Some(_) => false,
            None => self.log_capture_mode == LogCaptureMode::Metadata,
        };
        if !metadata {
            return;
        }
        for path in [
            registry.agent_output_log_path(name),
            registry.agent_stderr_log_path(name),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    let unlink_failure = format!(
                        "{}: could not unlink the raw capture file {} (metadata mode keeps no \
                         agent content at rest): {e}",
                        name.as_str(),
                        path.display()
                    );
                    self.emit_diagnostic(&unlink_failure);
                }
            }
        }
    }

    /// Drain the proxy's evaluation inbox (story 23-1): publish each
    /// committed proxied close's usage event, then its breaches and Run
    /// events, and execute its process actions — exactly as
    /// [`Self::apply_evaluation`] does after a usage commit — in commit
    /// order, under the locks the reaper tick holds. A Run event batch a
    /// proxy job committed (a wire Run's `created`) is published as is.
    pub(crate) fn drain_proxy_inbox(&mut self, registry: &Registry) {
        for item in self.shared.inbox.drain() {
            match item {
                pipeline::InboxItem::RunEvents(events) => self.publish_run_events(events),
                pipeline::InboxItem::Proxied(commit) => {
                    let commit = *commit;
                    let (update, append) = commit.update;
                    self.note_outbox_append(&append);
                    self.publish(append.seq, EngineEvent::UsageUpdate(update));
                    match commit.instance {
                        Some(name) => self.apply_evaluation(registry, &name, commit.evaluation),
                        None => {
                            // An instance-less Run: its breaches publish
                            // and act on no process (`block` already rode
                            // the snapshot).
                            for committed in &commit.evaluation.breaches {
                                self.note_outbox_append(&committed.outbox);
                                self.publish(
                                    committed.outbox.seq,
                                    EngineEvent::BudgetBreach(committed.event.clone()),
                                );
                            }
                            for detail in &commit.evaluation.degraded_reads {
                                self.emit_diagnostic(&format!(
                                    "a proxied call's budget check degraded: {detail}"
                                ));
                            }
                            self.publish_run_events(commit.evaluation.run_events);
                        }
                    }
                }
            }
        }
    }

    /// THE ledger-commit choke point (story 3-1, spine AD-7) — the SOLE writer of
    /// the `usage_events` table.
    ///
    /// Constructs the full [`UsageEvent`] from the agent-supplied [`ParsedUsage`]
    /// plus the engine-stamped fields (the current Run id, the instance name, the
    /// metering source, and the commit timestamp), then records it in its OWN
    /// transaction via `record_usage_event` (AD-6: one transaction per event). A
    /// re-delivered batch is classified [`RecordOutcome::DuplicateReplay`] by the
    /// DB `UNIQUE` index and is a no-op (AC-A no-double-count). On a fresh insert it
    /// builds the AD-14 [`UsageUpdateEvent`] (the wire shape frozen in 3-1;
    /// delivered on the event bus since story 7-2).
    ///
    /// **AI-41 — a store error is REPORTED, never silently dropped:** the method
    /// returns the failure so the self-reported drain can PARK its cursor
    /// and retry the event on the next pass (the observed drain reports + skips —
    /// it has no cursor). A diagnostic is emitted through the AD-12 sink either
    /// way, so dropped usage is always VISIBLE. Usage ingestion still never
    /// crashes the supervisor or a lifecycle op (the ledger is advisory to the
    /// RUN, not gating it) — honest reporting, not a panic.
    ///
    /// **12-4 AMENDMENT (review loop 1) — the diagnostic is MODE-aware:** the
    /// `mode` the CALLER is draining under decides what the failure text may
    /// honestly claim. A `MidRun` caller WILL re-attempt, so the text says the
    /// event "is parked for retry on the next drain". A `Terminal` caller
    /// (stop / crash-reap) has NO next pass — its text says the event is LOST
    /// and will NOT be retried — because a terminal diagnostic that promised a
    /// retry would contradict the adjacent LOST notice and lie to the operator
    /// (surfaced-not-silent means the surface must also be TRUE).
    ///
    /// **The AD-7 single-writer invariant lives here:** no other code path may call
    /// `record_usage_event`.
    ///
    /// **The AD-7 ENFORCEMENT stage lives here too (story 3-2):** IMMEDIATELY after
    /// a fresh `Inserted` commit — in the SAME synchronous path, before returning —
    /// this method reads the CURRENT resolved [`TokenBudget`] + [`BreachAction`]
    /// (a LIVE config read, so a budget changed while `running` applies on the very
    /// next event — AC-B), reads the just-committed per-run + cumulative token
    /// totals (3-1's `usage_totals`/`run_totals`), and calls the pure
    /// [`BudgetEvaluator`]. On a [`BreachDecision::Breached`] it RECORDS the breach
    /// event FIRST/independently ([`Self::record_breach`]) — so a best-effort/
    /// unsupported/failed pause never loses the breach record (FR-21 "always
    /// recorded regardless of action") — and THEN executes the action via Epic-1's
    /// lifecycle (`pause`/`stop`/`warn`). This is the SOLE enforcement site (the
    /// AD-7 companion to the single-writer invariant). A [`RecordOutcome::DuplicateReplay`]
    /// is NOT evaluated (nothing new was committed → no new breach can occur).
    /// Ingestion + enforcement stay best-effort to the RUN: a store/lifecycle error
    /// is a diagnostic, NEVER a supervisor crash (3-1's rule extended to enforcement).
    pub(super) fn ingest_usage(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        metering_source: &str,
        parsed: &ParsedUsage,
        mode: DrainMode,
    ) -> Result<Option<UsageUpdateEvent>, crate::domain::error::RegistryError> {
        let event = assemble_usage_event(
            parsed,
            name.as_str(),
            run_id.clone(),
            metering_source,
            now_rfc3339(),
        );
        // Story 21-1: ONE store-writer job commits the row (stamped with the
        // launch generation it was metered under), both rollups, the usage
        // outbox row, and every breach the new totals fire — the AD-7 pipeline
        // in one transaction (AD-27). The evaluator's inputs (the LIVE config
        // read, AC-B; the Rate in force at commit, 3-3's no-retro-repricing)
        // travel in the job: the writer reads no engine state (D2).
        let generation = self
            .running
            .get(name)
            .map_or(0, |supervised| supervised.launch_generation);
        // Story 21-3 (review round 2): usage attributed to a CHILD Run
        // prices at the child's OWN resolved Rate (its Policy's flat Rate)
        // — the ledger is per-Run priced, so a child-only Policy Cost Cap
        // sees cost priced at the child's Rate, not the owner's (or none).
        // The owner's own usage prices at the evaluator's Rate as before.
        let row_rate = if self.running.get(name).map(|s| &s.run_id) == Some(run_id) {
            None
        } else {
            match registry.run_rate(run_id) {
                Ok(rate) => rate,
                Err(err) => {
                    // Best effort with a surfaced note: the row prices at
                    // the owner's Rate (or unpriced) — never silently, and
                    // no retro-repricing exists to repair it later.
                    self.emit_diagnostic(&format!(
                        "{}: the attributed Run's Rate could not be read ({err}); its usage row \
                         prices at the owning Run's Rate",
                        name.as_str(),
                    ));
                    None
                }
            }
        };
        let job = pipeline::UsageJob {
            event,
            generation,
            row_rate,
            eval: self.eval_input(registry, name, run_id, metering_source),
        };
        match registry
            .writer()
            .call(move |ctx| pipeline::commit_usage(ctx, job))
        {
            // A fresh row: publish what committed in commit order (usage, then
            // each breach), then execute the process Breach Actions before the
            // drain returns (D2: the caller applies them under its locks, so a
            // short-lived CLI engine never exits with one queued).
            Ok(commit) => {
                let Some((update, append)) = commit.update else {
                    // A recognized replay — no double-count, no event emitted
                    // (nothing new was committed; AC-A).
                    return Ok(None);
                };
                self.note_outbox_append(&append);
                self.publish(append.seq, EngineEvent::UsageUpdate(update.clone()));
                self.apply_evaluation(registry, name, commit.evaluation);
                Ok(Some(update))
            }
            // AI-41: a store error must never SILENTLY drop the event. Report it
            // (diagnostic + Err) so the drains can park and retry the exact same
            // event on the next pass — the self-reported drain (its byte cursor)
            // since AI-41, and the OBSERVED drain (its pending buffer) since story
            // 12-4. Never a supervisor crash. The writer rolled the whole job back,
            // so nothing of it (row, rollups, breaches) landed.
            Err(err) => {
                let err = crate::domain::error::RegistryError::Store(err);
                // Caller-factual (AI-41 loop 1; MODE-aware per the 12-4
                // AMENDMENT): a MIDRUN drain will really retry, so the text
                // says the event is parked for the next pass and not counted
                // until that commit succeeds (it does not claim the retry can
                // never fail — the bound skips loudly). A TERMINAL drain has
                // NO next pass, so its text must never claim "parked for
                // retry" (that would contradict the adjacent LOST notice) —
                // it names the loss and the why instead.
                let failure = if mode == DrainMode::Terminal {
                    format!(
                        "{}: a usage event could not be committed to the Usage Ledger: \
                         {err} — this is the terminal drain (the process is dead or the \
                         handle is being removed), so the event is LOST and will NOT be \
                         retried",
                        name.as_str(),
                    )
                } else {
                    format!(
                        "{}: a usage event could not be committed to the Usage Ledger: {err} — \
                         it is parked for retry on the next drain and is NOT counted until \
                         that commit succeeds",
                        name.as_str(),
                    )
                };
                self.emit_diagnostic(&failure);
                Err(err)
            }
        }
    }

    /// The pipeline's evaluator inputs for one instance and Run (story
    /// 21-1): the LIVE resolved limits (AC-B — a change while `running`
    /// applies on the very next event), whether the instance is
    /// supervised, and its wire fence. Since story 21-3 the limits resolve
    /// through the ONE Policy resolver (`Registry::resolved_limits`):
    /// explicit Run limits, explicit instance config, the kept-on-pause
    /// marker, matching Policies and the default Policy — the same pure
    /// function every surface calls. A failed config or store read fails
    /// CLOSED to `pause` arms (AD-7: never the new default by accident).
    pub(super) fn eval_input(
        &self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        metering_source: &str,
    ) -> pipeline::EvalInput {
        // The ARMS resolve for the instance's process-owning Run — the
        // pipeline's process level (`pipeline::levels_for`) — so a child
        // Run's own `run_limits` never leak into the instance arms (each
        // level's own ceilings ride its own level) and the instance's
        // explicit ceilings still apply however usage is attributed. The
        // attributed `run_id` stays the evaluation's target. Passing no
        // Run lets `resolved_limits` read the open process-owning Run
        // itself — and a FAILED read fail-closes INSIDE it (pause, arms
        // kept, note carried), never a silent label-less resolve.
        let _ = run_id;
        let resolution = registry.resolved_limits(name, None);
        let supervised = self.running.get(name);
        pipeline::EvalInput {
            name: Some(name.clone()),
            run_id: run_id.clone(),
            metering_source: metering_source.to_string(),
            token_budget: resolution.tokens,
            token_action: resolution.effective_action,
            rate: resolution.rate,
            cost_cap: resolution.dollars,
            cost_action: resolution.effective_action,
            supervised: supervised.is_some(),
            runs: Some(Arc::clone(registry.run_publisher())),
            #[cfg(test)]
            totals_fault: self.totals_read_fault_names.contains(name),
        }
    }

    /// The AD-7 ENFORCEMENT stage without a new usage row (story 3-2/3-3,
    /// moved into the store writer by story 21-1): evaluate the instance's
    /// budgets for `run_id` on the committed totals and commit every breach
    /// that fires. Called where a Run (re)starts against an already-spent
    /// ledger — adoption, and a Run start under `block` (19-2) — so a
    /// crossing that has not fired yet fires exactly once. A usage commit
    /// evaluates inside its own job ([`Self::ingest_usage`]).
    ///
    /// **Idempotence — at most one breach per (dimension, scope) per Run and
    /// action:** the durable `run_latches` table (story 21-1) holds every
    /// pair that fired, under the action that fired it; the writer filters
    /// [`governance::decide`]'s latch input to the CURRENTLY configured
    /// action, so a reconfigured action fires once more under the new action
    /// (the 15-2 adoption rule, now live too). One decision implementation
    /// (19-1), shared with the conformance vectors.
    pub(super) fn enforce_budget(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        metering_source: &str,
    ) {
        let input = self.eval_input(registry, name, run_id, metering_source);
        let armed =
            input.token_budget.is_set() || (input.rate.is_some() && input.cost_cap.is_set());
        if !armed {
            // The common un-governed path: nothing to evaluate, no writer trip.
            return;
        }
        // cfg(test) FAULT-INJECTION SEAM (the `transition_fault_names`
        // precedent, story 21-1): an instance armed in
        // `usage_eval_fault_names` fails the evaluation's writer job ITSELF —
        // before any breach is built — so the surfaced Err arm below (no
        // breach recorded, the next usage event evaluates again) runs
        // deterministically without staging a real store outage. No-op in
        // every non-test build.
        #[cfg(test)]
        let evaluation = if self.usage_eval_fault_names.contains(name) {
            Err(crate::ports::StoreError::Backend(
                "injected usage-evaluation fault (test seam)".to_string(),
            ))
        } else {
            registry
                .writer()
                .call(move |ctx| pipeline::evaluate(ctx, &input))
        };
        #[cfg(not(test))]
        let evaluation = registry
            .writer()
            .call(move |ctx| pipeline::evaluate(ctx, &input));
        match evaluation {
            Ok(evaluation) => self.apply_evaluation(registry, name, evaluation),
            Err(err) => self.log_enforcement_diagnostic(
                registry,
                name,
                &format!(
                    "the budget evaluation could not be committed ({err}); no breach was \
                     recorded, and the next usage event evaluates again"
                ),
            ),
        }
    }

    /// Everything after a pipeline job committed its breaches (story 21-1):
    /// surface any degraded totals read, mirror each breach to `breaches.log`
    /// (best-effort), publish each in commit order, then EXECUTE the Breach
    /// Actions through Epic-1's lifecycle (AD-15 — a reason, not a new edge).
    /// The breaches are already durable, so a failing pause never loses the
    /// record (FR-21 "always recorded regardless of action"). Each breach
    /// executes its action in commit order, as before: when a token breach
    /// already paused the instance, the dollar breach's pause meets the
    /// transition gate and says so on the diagnostic surface.
    pub(super) fn apply_evaluation(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        evaluation: pipeline::Evaluation,
    ) {
        for detail in &evaluation.degraded_reads {
            self.log_enforcement_diagnostic(registry, name, detail);
        }
        for committed in &evaluation.breaches {
            self.mirror_breach(registry, name, &committed.event);
            self.note_outbox_append(&committed.outbox);
            self.publish(
                committed.outbox.seq,
                EngineEvent::BudgetBreach(committed.event.clone()),
            );
        }
        // Story 21-2: the `breach` Anomalies those breaches raised, committed
        // after them.
        self.publish_run_events(evaluation.run_events);
        // Only the process-owning Run's breaches act on the process; a child
        // Run's breach is recorded under `warn` (its own action cannot act on
        // the shared process before wire hold exists).
        for committed in evaluation
            .breaches
            .iter()
            .filter(|committed| committed.process_level)
        {
            let decision = committed.decision;
            if decision.action == BreachAction::Block {
                self.note_block(registry, name, decision.dimension, decision.scope);
                continue;
            }
            let cause = match decision.dimension {
                BreachDimension::Tokens => TransitionCause::budget_exceeded(
                    decision.scope,
                    decision.limit,
                    decision.observed,
                ),
                // v1 the estimate label is always `estimated`.
                BreachDimension::Dollars => TransitionCause::cost_cap_exceeded(
                    decision.scope,
                    Micros(decision.limit as i64),
                    Micros(decision.observed as i64),
                    EstimateLabel::Estimated,
                ),
            };
            match decision.action {
                // No lifecycle transition — the breach event is the whole guardrail.
                BreachAction::Warn | BreachAction::Block => {}
                BreachAction::Pause => self.enforce_pause(registry, name, cause),
                BreachAction::Stop => self.enforce_stop(registry, name, cause),
            }
        }
    }

    /// The pure decision input's ARMS for one instance (shared by
    /// [`Self::enforce_budget`] and the story-19-2 [`Self::run_start_fence`]):
    /// one arm per ARMED dimension, `None` = skipped. `run` names the Run whose
    /// per-run totals are read; `None` is a FRESH Run whose per-run totals are
    /// zero by construction (nothing has been committed under it yet), so only
    /// the cumulative totals are read.
    fn governance_arms(
        &self,
        registry: &Registry,
        name: &InstanceName,
        run: Option<&RunId>,
        (budget, token_action): (TokenBudget, BreachAction),
        (rate, cost_cap, cost_action): (Option<Rate>, CostCap, BreachAction),
    ) -> (Option<governance::TokenArm>, Option<governance::DollarArm>) {
        let token_armed = budget.is_set();
        let dollar_armed = rate.is_some() && cost_cap.is_set();
        let token = token_armed.then(|| governance::TokenArm {
            run_total: run
                .map(|run_id| {
                    self.surfaced_read(registry, name, "per-run token totals", || {
                        registry.run_usage_totals(name, run_id)
                    })
                    .map(|t| t.total_tokens())
                    .unwrap_or(0)
                })
                .unwrap_or(0),
            cumulative_total: self
                .surfaced_read(registry, name, "cumulative token totals", || {
                    registry.usage_totals(name)
                })
                .map(|t| t.total_tokens())
                .unwrap_or(0),
            budget,
            action: token_action,
        });
        let dollar = dollar_armed.then(|| governance::DollarArm {
            run_cost: run
                .map(|run_id| {
                    self.surfaced_read(registry, name, "per-run dollar cost", || {
                        registry.run_cost_totals(name, run_id)
                    })
                    .unwrap_or(Micros::ZERO)
                })
                .unwrap_or(Micros::ZERO),
            cumulative_cost: self
                .surfaced_read(registry, name, "cumulative dollar cost", || {
                    registry.cost_totals(name)
                })
                .unwrap_or(Micros::ZERO),
            cap: cost_cap,
            action: cost_action,
        });
        (token, dollar)
    }

    /// Whether the instance's CURRENT resolved Breach Action is `block`
    /// (story 19-2; story 21-3: the resolution, not the config alone — a
    /// Policy can supply `block`). A failed read is `false` — the same
    /// degrade `enforce_budget` takes (it returns early on that read).
    pub(super) fn block_configured(&self, registry: &Registry, name: &InstanceName) -> bool {
        registry.resolved_limits(name, None).effective_action == BreachAction::Block
    }

    /// The RUN-START fence evaluation (story 19-2): before a Run's process
    /// spawns, evaluate the pure [`governance::decide`] against that Run —
    /// a fresh Run's per-run totals are zero; a Restart Policy relaunch keeps
    /// its Run (story 21-2), so `kept_run`'s per-run totals count — with
    /// cumulative totals from the ledger and an empty latch, and RAISE
    /// the returned reason bits for every `block` decision — so a Run that
    /// begins at or over a `block` ceiling refuses its very first model call
    /// instead of letting one through before the first usage event fires the
    /// breach. Only `block` decisions act here; `pause`/`stop`/`warn` keep
    /// their first-usage-event semantics untouched. Nothing is recorded or
    /// latched here (the Run is not supervised yet): the caller runs
    /// [`Self::enforce_budget`] once the `Supervised` exists, which records
    /// and latches the same decisions under the new run id. Part of the AD-7
    /// pipeline (same core, same module) — no other path raises a fence.
    /// Returns the raised reasons (non-zero → the caller records them even
    /// if the action changed in between: a raised fence is never unrecorded).
    pub(super) fn run_start_fence(
        &self,
        registry: &Registry,
        name: &InstanceName,
        kept_run: Option<&RunId>,
    ) -> u8 {
        // Story 21-3: the Run-start fence resolves through the ONE Policy
        // resolver — a Policy can supply the ceilings and the `block`
        // action, not only the instance config.
        let resolution = registry.resolved_limits(name, kept_run);
        let budget = resolution.tokens;
        let token_action = resolution.effective_action;
        let rate = resolution.rate;
        let cost_cap = resolution.dollars;
        let cost_action = resolution.effective_action;
        if token_action != BreachAction::Block && cost_action != BreachAction::Block {
            return 0;
        }
        let (token, dollar) = self.governance_arms(
            registry,
            name,
            kept_run,
            (budget, token_action),
            (rate, cost_cap, cost_action),
        );
        let outcome = governance::decide(&governance::GovernanceInput {
            token,
            dollar,
            latched: Vec::new(),
        });
        // Story 23-1: the reasons ride the snapshot's pending route for the
        // new Run (the shared proxy refuses its first call) until the latch
        // the caller records lands on the Run's own entry.
        outcome
            .decisions
            .iter()
            .filter(|decision| decision.action == BreachAction::Block)
            .fold(0u8, |bits, decision| {
                bits | crate::domain::runs::snapshot::Disposition::bit(governance::LatchKey::new(
                    decision.dimension,
                    decision.scope,
                ))
            })
    }

    /// Surface a committed `block` breach (story 19-2; since story 23-1 the
    /// breach's latch republished the Run's snapshot entry before the ack,
    /// and the shared proxy refuses the Run's — and every Run beneath it —
    /// further model calls): with a wire path the fence is up; a Run with
    /// none (a `self-reported` source reached through a hand-edited layer)
    /// cannot be fenced: the breach stays recorded, acts as `warn`, and the
    /// diagnostic says so loudly (AI-18; never a silent no-op).
    pub(super) fn note_block(
        &self,
        registry: &Registry,
        name: &InstanceName,
        dimension: BreachDimension,
        scope: BreachScope,
    ) {
        let reason = governance::LatchKey::new(dimension, scope);
        let Some(supervised) = self.running.get(name) else {
            return;
        };
        if supervised.observed_route {
            self.log_enforcement_diagnostic(
                registry,
                name,
                &format!(
                    "budget breach ({} {}) — the wire fence is UP: the metering proxy refuses \
                     further model calls on this Run and every Run beneath it with HTTP 402 \
                     `{}` (code `{}`) until the ceiling is raised or the Run ends",
                    dimension.as_str(),
                    scope.as_str(),
                    governance::FENCE_ERROR_TYPE,
                    governance::fence_code(reason),
                ),
            );
        } else {
            let source = supervised.metering_source.clone();
            self.log_enforcement_diagnostic(
                registry,
                name,
                &format!(
                    "budget breach ({} {}) is configured to `block`, but this instance's model \
                     calls do not pass through the metering proxy (metering source `{source}`) \
                     — the fence could NOT be raised; the breach is recorded and acts as \
                     `warn`. Set `budget.breach_action` to pause or stop, or meter the instance \
                     through the metering proxy",
                    dimension.as_str(),
                    scope.as_str(),
                ),
            );
        }
    }

    /// Execute a `pause` Breach Action honestly (story 3-2 AC6, honoring story
    /// 1-5's Capability Declaration). Drives `running → paused` and STAMPS the
    /// resulting transition with the [`TransitionCause::BudgetExceeded`] cause (so
    /// the lifecycle log explains WHY), via [`Self::pause`]. A best-effort pause
    /// still transitions (1-5) and the breach is already recorded; an UNSUPPORTED
    /// pause fails fast in [`Self::pause`] — we do NOT fake a pause and do NOT
    /// silently escalate to stop (AC6), we surface the honest diagnostic on the
    /// engine log (the breach event already captured the fact). All best-effort:
    /// never a supervisor crash.
    pub(super) fn enforce_pause(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        cause: TransitionCause,
    ) {
        match self.pause_with_cause(registry, name, cause) {
            Ok(_) => {}
            Err(e) => {
                // Honest surface (AD-12): pause could not be honored (unsupported /
                // not running / backend hiccup). The breach is ALREADY recorded; log
                // and move on — no fake pause, no escalation.
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!("budget breach pause could not be honored: {e}"),
                );
            }
        }
    }

    /// Re-apply a kept Run's latched process Breach Action after a Restart
    /// Policy relaunch (story 21-2). A relaunch keeps the Run and with it the
    /// latch, so a `pause` or `stop` that already fired would never fire again
    /// on the relaunched process: a crash while breach-paused would come back
    /// unpaused with its spend unbounded. A latch recorded under the
    /// CURRENTLY configured action is re-executed with its breach's cause; a
    /// latch under another action re-fires on its own at the next evaluation.
    pub(super) fn reapply_latched_actions(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
    ) {
        // Story 21-3: the latched actions re-apply under the RESOLVED
        // action (a Policy can supply it), not the config alone.
        let resolution = registry.resolved_limits(name, Some(run_id));
        let token_action = resolution.effective_action;
        let cost_action = resolution.effective_action;
        let breaches = match registry.reader().run_breaches(name, run_id) {
            Ok(breaches) => breaches,
            Err(err) => {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!(
                        "the relaunched Run's breach records could not be read ({err}); a \
                         `pause` or `stop` it had already fired is not re-applied"
                    ),
                );
                return;
            }
        };
        // Round 3, L3: only breaches whose `(dimension, scope)` is STILL
        // latched re-apply — the Operator may have raised the ceiling
        // since (the raise unlatches), and re-applying a spent breach
        // paused a relaunched agent under a ceiling it no longer crosses.
        let latch_rows: Vec<crate::domain::LatchKey> = registry
            .latches(run_id)
            .map(|rows| rows.into_iter().map(|(key, _)| key).collect())
            .unwrap_or_default();
        let latched = breaches.into_iter().rev().find(|event| {
            let configured = match event.dimension {
                BreachDimension::Tokens => token_action,
                BreachDimension::Dollars => cost_action,
            };
            event.action == configured
                && matches!(event.action, BreachAction::Pause | BreachAction::Stop)
                && latch_rows.contains(&crate::domain::LatchKey::new(event.dimension, event.scope))
        });
        let Some(event) = latched else {
            return;
        };
        self.log_enforcement_diagnostic(
            registry,
            name,
            &format!(
                "the Restart Policy relaunched this instance into the same Run, whose {} {} \
                 ceiling had already fired `{}` — re-applying it",
                event.dimension.as_str(),
                event.scope.as_str(),
                event.action.as_str(),
            ),
        );
        let cause = match event.dimension {
            BreachDimension::Tokens => {
                TransitionCause::budget_exceeded(event.scope, event.limit, event.observed)
            }
            BreachDimension::Dollars => TransitionCause::cost_cap_exceeded(
                event.scope,
                event.dollar_limit.unwrap_or(Micros(event.limit as i64)),
                event
                    .dollar_observed
                    .unwrap_or(Micros(event.observed as i64)),
                EstimateLabel::Estimated,
            ),
        };
        match event.action {
            BreachAction::Pause => self.enforce_pause(registry, name, cause),
            _ => self.enforce_stop(registry, name, cause),
        }
    }

    /// Execute a `stop` Breach Action (story 3-2). Drives `running → stopping →
    /// stopped` (story 1-4) and, before that, records the [`TransitionCause::BudgetExceeded`]
    /// as the WHY marker on the `running → stopping` edge (the stop path itself
    /// records the graceful/forced escalation on the terminal edge). Best-effort:
    /// a stop error is logged, never a crash (the breach is already recorded).
    pub(super) fn enforce_stop(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        cause: TransitionCause,
    ) {
        match self.stop_with_cause(registry, name, cause) {
            Ok(_) => {}
            Err(e) => {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!("budget breach stop could not be honored: {e}"),
                );
            }
        }
    }

    /// Record a TOKEN [`BudgetBreachEvent`] outside a usage commit (story 3-2
    /// shape; since story 21-1 the pipeline's own breaches commit inside the
    /// usage job, so this records history for the paths that need a breach on
    /// file — tests seed adoption scenarios through it). The breach row, its
    /// latch and its outbox row commit in one writer job, then the record is
    /// mirrored and published like any pipeline breach.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_token_breach(
        &self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        scope: BreachScope,
        limit: u64,
        observed: u64,
        action: BreachAction,
        metering_source: &str,
    ) {
        let event = BudgetBreachEvent::new(
            name.as_str(),
            run_id.as_str(),
            scope,
            limit,
            observed,
            action,
            metering_source,
            now_rfc3339(),
        );
        self.persist_breach_event(registry, name, &event);
    }

    /// Commit one built breach record (row + latch + outbox) through the
    /// store writer, then mirror and publish it. A failed commit is surfaced:
    /// the breach record is the FR-21 mandated durable artifact, so a lost one
    /// is never silent.
    #[cfg(test)]
    fn persist_breach_event(
        &self,
        registry: &Registry,
        name: &InstanceName,
        event: &BudgetBreachEvent,
    ) {
        let job_event = event.clone();
        // FAULT-INJECTION SEAM (the `usage_eval_fault_names` twin): an armed
        // name fails the breach-record writer job ITSELF, driving the
        // mandated-record-lost Err arm deterministically. Both the seam field
        // and this whole helper are test-only, so no cfg split is needed.
        let committed = if self.breach_record_fault_names.contains(name) {
            Err(crate::ports::StoreError::Backend(
                "injected breach-record fault (test seam)".to_string(),
            ))
        } else {
            registry
                .writer()
                .call(move |ctx| pipeline::record_breach(ctx, &job_event))
        };
        match committed {
            Ok(append) => {
                self.mirror_breach(registry, name, event);
                self.note_outbox_append(&append);
                self.publish(append.seq, EngineEvent::BudgetBreach(event.clone()));
            }
            Err(e) => {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!(
                        "could not record the budget breach event in state.db: {e} — the \
                         mandated breach record was NOT written"
                    ),
                );
            }
        }
    }

    /// Append a committed breach to the instance's `breaches.log` mirror
    /// (story 21-1: best-effort, after commit — state.db holds the record, so
    /// a failed mirror is a diagnostic, never a lost breach).
    /// `pub(super)`: the policy-change job's unsupervised-instance breach
    /// surface (supervisor/runs.rs) mirrors the same way.
    pub(super) fn mirror_breach(
        &self,
        registry: &Registry,
        name: &InstanceName,
        event: &BudgetBreachEvent,
    ) {
        let path = registry.instance_breach_log_path(name);
        let mirrored = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(|e| e.to_string())
            .and_then(|()| append_breach_event(&path, event));
        if let Err(e) = mirrored {
            self.log_enforcement_diagnostic(
                registry,
                name,
                &format!(
                    "the budget breach is recorded in state.db, but its breaches.log mirror at \
                     {} could not be appended: {e}",
                    path.display()
                ),
            );
        }
    }

    /// Surface one enforcement diagnostic through the engine's diagnostic
    /// emission (AD-12: enforcement diagnostics ride the engine log / stderr —
    /// or the host's story-10-2 sink when one is installed — NEVER `kt`
    /// stdout, NEVER a crash). Used when a breach action (pause/stop) could
    /// not be honored — the breach itself is already durably recorded in the
    /// breach log, so this is only an operator breadcrumb, not the record of
    /// the breach. `registry` is unused (the diagnostic is not persisted to a
    /// strict-parse log to avoid corrupting the transition-event reader) but
    /// kept for signature symmetry with the other enforcement helpers.
    fn log_enforcement_diagnostic(&self, _registry: &Registry, name: &InstanceName, detail: &str) {
        let breadcrumb = format!("{}: {detail}", name.as_str());
        self.emit_diagnostic(&breadcrumb);
    }

    /// One enforcement totals/cost read, SURFACED (story 19-1 review round 1,
    /// the AI-18 surfaced-not-silent rule): `Ok` passes through untouched; an
    /// `Err` emits the enforcement diagnostic naming the instance + the
    /// degraded scope + the reason, and the caller's zero substitute still
    /// flows into the decision input — the pre-existing story-3-2/3-3
    /// degrade-to-zero semantics UNCHANGED (a zero ceiling still fires on the
    /// fabricated total; only the silence is fixed). Deciding DIFFERENTLY on a
    /// degraded read (skipping the arm, failing closed) is a money-path
    /// behavior change, deferred to its own story — the deferred-work entry
    /// from 19-1's review round 1 records it with its trigger.
    fn surfaced_read<T>(
        &self,
        registry: &Registry,
        name: &InstanceName,
        scope: &str,
        read: impl FnOnce() -> Result<T, crate::domain::RegistryError>,
    ) -> Result<T, crate::domain::RegistryError> {
        // cfg(test) FAULT-INJECTION SEAM (the `compensation_fault_names`
        // precedent): an armed instance's read fails with an injected error so
        // the surfaced-degradation arm is reachable deterministically.
        #[cfg(test)]
        let result = if self.totals_read_fault_names.contains(name) {
            Err(crate::domain::RegistryError::Io {
                name: name.as_str().to_string(),
                path: "<totals read (story 19-1 test seam)>".to_string(),
                source: std::io::Error::other("injected totals-read fault (test seam)"),
            })
        } else {
            read()
        };
        #[cfg(not(test))]
        let result = read();
        if let Err(error) = &result {
            let detail = format!(
                "the {scope} read failed — enforcement degrades it to zero and decides \
                 on the degraded total (a zero ceiling still fires on it): {error}"
            );
            self.log_enforcement_diagnostic(registry, name, &detail);
        }
        result
    }

    /// Read back the recorded [`BudgetBreachEvent`]s for an instance from its
    /// breach log (observation helper for tests / embedders — the AD-14 seed, NOT
    /// the 7-2 bus). Empty vec if none recorded yet.
    pub fn read_breach_events(
        registry: &Registry,
        name: &str,
    ) -> Result<Vec<BudgetBreachEvent>, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        registry
            .breach_events(&name)
            .map_err(super::registry_to_engine)
    }
}

/// Whether a ledger commit failed only because the store writer stayed busy
/// past its bound (story 21-1) — retryable contention, never a poisoned row.
fn is_busy(err: &crate::domain::error::RegistryError) -> bool {
    matches!(
        err,
        crate::domain::error::RegistryError::Store(crate::ports::StoreError::Busy { .. })
    )
}
