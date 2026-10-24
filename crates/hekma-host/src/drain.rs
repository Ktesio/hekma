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
