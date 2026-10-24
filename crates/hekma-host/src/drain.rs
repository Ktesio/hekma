//! The host's shutdown drain — now a THIN DELEGATE.
//!
//! The drain itself moved INTO the engine (issue #328 item 3):
//! [`Engine::shutdown`] owns the fleet-wide stop ladder, so a plain embedding
//! gets the graceful path without depending on this host crate. The host owns
//! the SIGNAL; the engine owns the STOP (AD-20) — taken to its conclusion,
//! the fleet-wide drain IS stops, so it lives with them. Every semantic of
//! the story 17-2 drain is unchanged — the live-rows plan (`Running` AND
//! `Paused`, resumed-then-stopped through the bounded ladder), the one-pass
//! newcomer re-enumeration, the honest still-alive roll-up, the
//! `ExitedOnItsOwn`/`Graceful`/`Forced` classification discipline, and the
//! exact `[hekma] `-prefixed notice stream. The findings record and the pins
//! live with the implementation now (the engine's `shutdown` module);
//! `tests/drain_subprocess.rs` + `tests/serve.rs` still prove the HOST wiring
//! end-to-end through this delegate.
//!
//! "Drain" here is the bounded stop ladder only; the AD-15 drain composite
//! (in-flight request draining) landed as story 18-4 —
//! `POST /v0/instances/{name}/drain` (control API) and `hekma agent drain`
//! (CLI) — and remains a DIFFERENT act from this shutdown path: the
//! composite pauses ONE agent through its adapter's declared pause command,
//! waits (bounded) for its declared idle signal, then stops it; THIS module
//! still stops the whole live fleet on SIGTERM. The split stands.
//!
//! One crossover to name honestly (story 18-4): a COOPERATIVELY paused
//! instance (its adapter's declared pause command, not an OS suspension) in
//! this shutdown fleet is resumed-then-stopped by the SAME ladder, but the
//! "resume" step skips the OS signal for it (the engine's stop ladder knows
//! the pause is cooperative) — no OS resume exists to send, and the
//! graceful signal reaches the agent directly.
//!
//! Review findings, in this module:
//!
//! * **Finding 4 — the stops are ISSUED CONCURRENTLY**: one bounded stop per
//!   running agent, all issued, all joined (`JoinSet`) — the per-instance
//!   outcomes settle and surface as they finish, never in a fixed
//!   issue-order lockstep. The WALL CLOCK, honestly stated: the engine's
//!   stop ladder runs under its supervision lock (the ratified coarse
//!   two-mutex model — one lock held per whole ladder, measured and
//!   re-ratified by the serve-shape scenario), so instances that must run
//!   out their full graceful window before the force-kill escalation QUEUE
//!   behind each other — worst case Σ(window) over the fleet. Agents that
//!   exit promptly on the graceful signal hold the lock briefly and settle
//!   fast. Parallel ladders (≈ one window total) need the engine's
//!   stop-ladder locking to change — the archived per-instance lock-model
//!   plan, an architecture decision, not a host-layer change; this
//!   issue-all/join structure is the seam that becomes parallel the moment
//!   the engine's stops do.
//! * **Finding 5 — the plan is re-enumerated**: an instance that enters
//!   `Running` after the first snapshot (a concurrent `agent start`, a
//!   restart policy firing mid-drain) gets ONE extra ladder pass; anything
//!   still `Running` after that cap is SURFACED honestly (AI-18 — never
//!   silently handed to the engine Drop's un-graceful kill-on-drop).
//! * **Finding 6 — an already-exited instance is honest**: a stop that
//!   errors because the instance ended ON ITS OWN mid-drain (its state now
//!   reads stopped/failed) is classified `ExitedOnItsOwn`, not `Failed` —
//!   the drain's goal (no surviving process) is met, and the record says so
//!   without claiming the ladder did it.
//!
//! Outcome discipline (AI-18 surfaced-not-silent): the drain NEVER aborts on
//! a per-instance failure — one un-stoppable agent must not strand the
//! others' stops — and every outcome (graceful / forced / unverified /
//! exited-on-its-own / failed) is both returned to the caller and emitted as
//! a notice. A failed stop degrades the SHUTDOWN's tidiness, never its
//! honesty: `serve` still exits 0 (the drain ran), with the failures on the
//! record.
//!
//! This module keeps the host's public surface stable: `drain_and_stop` (the
//! call `serve`'s signal paths make) plus the drain vocabulary
//! (`DrainOutcome`, `StopResult`, `drain_plan`) re-exported from the engine —
//! same names, same shapes, one implementation.

use std::sync::Arc;
use std::time::Duration;

use hekma_engine::{DiagnosticSink, Engine};

pub use hekma_engine::{drain_plan, DrainOutcome, StopResult};

/// The shutdown drain, delegated to [`Engine::shutdown`]: fleet → plan →
/// bounded stops → newcomer re-enumeration → honest still-alive roll-up.
/// Every outcome is noted through `sink` AND returned. Returns after the
/// LAST stop settles; the caller drops the engine only afterwards. The
/// engine is shared with serve's other handles, hence the `Arc`.
pub async fn drain_and_stop(
    engine: &Arc<Engine>,
    window: Duration,
    sink: &DiagnosticSink,
) -> Vec<DrainOutcome> {
    engine.shutdown(window, Some(sink)).await
}
