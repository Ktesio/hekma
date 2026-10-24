//! Restart Policy as DATA (spine AD-15) — the pure backoff + crash-loop math.
//!
//! Story 1-6's Restart Policy executor is split into a pure value module (this
//! file) and the side-effecting driver (the supervisor + engine cadence). This
//! module owns ONLY the decisions that are pure functions of the consecutive
//! failure count:
//!
//! * [`RestartPolicy`] — `never` | `on-failure`, the per-instance mode (AD-15
//!   "per-instance configurable"). The default is [`RestartPolicy::OnFailure`]
//!   (`[ASSUMPTION]` — AD-15 does not name the default; `on-failure` matches the
//!   story's "unattended agents are safe to run" intent; recorded in the Dev
//!   Agent Record).
//! * [`BackoffSchedule`] — the exponential backoff `delay_for(n)` =
//!   `min(base · 2^(n-1), cap)`. PRODUCTION constants are exactly `base = 1s`,
//!   `×2`, `cap = 60s` ([`BackoffSchedule::production`]). Tests build a SCALED
//!   schedule with a smaller base so the crash-loop / backoff tests run in
//!   milliseconds without weakening the production constants.
//! * [`MAX_CONSECUTIVE_FAILURES`] = 5 + [`is_crash_loop`] — crash-loop trips at
//!   EXACTLY 5 consecutive failures (AD-15).
//!
//! PURE — no I/O, no OS, no clock (mirrors [`super::transition`]). The supervisor
//! reads the policy per-instance, asks this module for the delay, and the engine
//! cadence owns the actual timer; a clean run RESETS the consecutive count (the
//! reset is the supervisor's bookkeeping, not this module's).

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The crash-loop threshold (spine AD-15): after this many CONSECUTIVE failures
/// an `on-failure` instance stops retrying and is left `failed` with the
/// crash-loop reason stated. Exactly 5, per the spine — never weakened.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// The production backoff base (spine AD-15): the first restart waits this long.
const PRODUCTION_BASE: Duration = Duration::from_secs(1);

/// The production backoff cap (spine AD-15): no restart waits longer than this.
const PRODUCTION_CAP: Duration = Duration::from_secs(60);

/// The per-instance Restart Policy mode (spine AD-15, AC4).
///
/// Drives the supervisor's restart executor on a DETECTED crash:
/// * [`RestartPolicy::Never`] — leave the instance `failed` immediately with the
///   exit cause, no restart.
/// * [`RestartPolicy::OnFailure`] — restart with exponential backoff (see
///   [`BackoffSchedule`]); after [`MAX_CONSECUTIVE_FAILURES`] consecutive
///   failures stop and leave `failed` with the crash-loop reason.
///
/// The wire form is kebab-case (`never` / `on-failure`) so it round-trips
/// through the DB and any future config layer with a stable, matchable token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Never restart; a crash leaves the instance `failed` with the exit cause.
    Never,
    /// Restart a crashed instance with exponential backoff, up to the crash-loop
    /// threshold. The AD-15 default (see [`RestartPolicy::default`]).
    OnFailure,
}

impl RestartPolicy {
    /// The stable wire/label form (`"never"` / `"on-failure"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            RestartPolicy::Never => "never",
            RestartPolicy::OnFailure => "on-failure",
        }
    }

    /// Parse the wire form back into a [`RestartPolicy`]. Returns `None` for an
    /// unrecognized string (e.g. a value from a future schema) — callers decide
    /// how to treat that (the registry defaults it to [`RestartPolicy::default`]).
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "never" => Some(RestartPolicy::Never),
            "on-failure" => Some(RestartPolicy::OnFailure),
            _ => None,
        }
    }

    /// Whether this policy restarts a crashed instance at all.
    ///
    /// `never` never restarts; `on-failure` restarts (subject to the crash-loop
    /// threshold, which the executor checks via [`is_crash_loop`]).
    pub fn restarts_on_crash(&self) -> bool {
        matches!(self, RestartPolicy::OnFailure)
    }
}

impl Default for RestartPolicy {
    /// The AD-15 default. `[ASSUMPTION]`: the spine does not name the default
    /// mode; `on-failure` is chosen to match "unattended agents are safe to run"
    /// (a crash is recovered by policy rather than leaving the agent down).
    fn default() -> Self {
        RestartPolicy::OnFailure
    }
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The exponential-backoff schedule (spine AD-15): `delay_for(n)` =
/// `min(base · 2^(n-1), cap)` for the `n`-th consecutive failure (`n >= 1`).
///
/// PRODUCTION is `base = 1s`, `×2`, `cap = 60s` ([`BackoffSchedule::production`]),
/// giving `1s, 2s, 4s, 8s, 16s, 32s, 60s(capped), 60s…`. The doubling factor is
/// fixed at ×2 (the spine's); only the `base` is injectable so tests can run the
/// crash-loop / backoff legs in milliseconds without changing the production
/// constants (the shape is identical; only the unit scales).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackoffSchedule {
    base: Duration,
    cap: Duration,
}

impl BackoffSchedule {
    /// The PRODUCTION schedule: base 1s, ×2 per consecutive failure, cap 60s
    /// (spine AD-15). This is what the engine constructs at runtime.
    pub fn production() -> Self {
        Self {
            base: PRODUCTION_BASE,
            cap: PRODUCTION_CAP,
        }
    }

    /// A schedule with a custom `base` and `cap` (test injection).
    ///
    /// The doubling factor stays ×2 (the spine's); this constructor only scales
    /// the unit so tests do not sleep for real seconds. Production code never
    /// calls this — it uses [`BackoffSchedule::production`].
    pub fn with_base_and_cap(base: Duration, cap: Duration) -> Self {
        Self { base, cap }
    }

    /// The base delay (first restart). Exposed for diagnostics/tests.
    pub fn base(&self) -> Duration {
        self.base
    }

    /// The cap (maximum delay). Exposed for diagnostics/tests.
    pub fn cap(&self) -> Duration {
        self.cap
    }

    /// The backoff delay before the `n`-th consecutive restart (`n >= 1`):
    /// `min(base · 2^(n-1), cap)`.
    ///
    /// `n == 0` is treated as `n == 1` (no restart has a zero-th attempt; the
    /// first restart waits `base`). The doubling is computed in a saturating way
    /// so a large `n` cannot overflow — it simply clamps to `cap`.
    pub fn delay_for(&self, consecutive_failures: u32) -> Duration {
        let n = consecutive_failures.max(1);
        // 2^(n-1) as a saturating multiplier on the base. For any n where the
        // shift would overflow, the result is already >= cap, so clamp to cap.
        let shift = n - 1;
        // u32 shift beyond 63 (on the u64 nanos) definitely exceeds the cap;
        // guard it to avoid an overflow panic in debug builds.
        if shift >= 63 {
            return self.cap;
        }
        let base_nanos = self.base.as_nanos();
        let scaled = base_nanos.saturating_mul(1u128 << shift);
        let cap_nanos = self.cap.as_nanos();
        let clamped = scaled.min(cap_nanos);
        // clamped <= cap_nanos which fits in u64 nanos for any sane cap.
        Duration::from_nanos(clamped.min(u64::MAX as u128) as u64)
    }
}

impl Default for BackoffSchedule {
    fn default() -> Self {
        Self::production()
    }
}

/// Whether `consecutive_failures` has reached the crash-loop threshold
/// ([`MAX_CONSECUTIVE_FAILURES`]) — at which point an `on-failure` instance
/// stops retrying and is left `failed` with the crash-loop reason (spine AD-15).
///
/// Trips at EXACTLY 5: `is_crash_loop(4)` is false (a 5th restart is allowed),
/// `is_crash_loop(5)` is true (no 6th restart).
pub fn is_crash_loop(consecutive_failures: u32) -> bool {
    consecutive_failures >= MAX_CONSECUTIVE_FAILURES
}

/// The stable-run window (story 18-3): a run that held `running`
/// CONTINUOUSLY for at least this long proves enough stability that its
/// crash RESETS the consecutive-failure counter first — the crash then
/// charges as failure #1. ENGINE-owned supervision policy, never adapter
/// surface (the 18-3 Design Note; the 18-1 engine-owned-cadence precedent).
/// PRODUCTION is exactly 30 s, pinned by
/// `stable_run_window_is_production_pinned` below; tests scale it through
/// the `window` parameter of [`next_consecutive_failures`] the same way
/// [`BackoffSchedule`] scales its base — the production constant itself is
/// never weakened.
pub const STABLE_RUN_WINDOW: Duration = Duration::from_secs(30);

/// The pure reset-then-charge arithmetic (story 18-3): given the counter's
/// current value and how long the just-ended run held `running`
/// continuously, return the NEXT counter value and whether a stable-run
/// reset happened.
///
/// * `stable_run >= window` → reset FIRST, then this crash charges as
///   failure #1 of a fresh streak: `(1, true)`.
/// * anything else → the plain increment the pre-18-3 code always did:
///   `(current + 1, false)`.
///
/// `stable_run: None` never resets — that is the adopted-instance case (no
/// in-memory anchor: adoption re-proves nothing about stability) and the
/// pause-gate case (the anchor is cleared while paused). PURE: the caller
/// owns the store writes and the crash detail line; this module only
/// decides.
pub fn next_consecutive_failures(
    current: u32,
    stable_run: Option<Duration>,
    window: Duration,
) -> (u32, bool) {
    match stable_run {
        Some(run) if run >= window => (1, true),
        // Saturating, never wrapping: a `u32::MAX` streak that somehow
        // keeps charging must not panic (debug builds) or wrap to 0 —
        // 0 would masquerade as a fresh streak.
        _ => (current.saturating_add(1), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_constants_are_exactly_the_spine_values() {
        // AD-15: base 1s, cap 60s, ×2 (implicit), crash-loop at 5. Guard the
        // production constants so a test-speed override can never leak into prod.
        let sched = BackoffSchedule::production();
        assert_eq!(sched.base(), Duration::from_secs(1));
        assert_eq!(sched.cap(), Duration::from_secs(60));
        assert_eq!(MAX_CONSECUTIVE_FAILURES, 5);
    }

    #[test]
    fn default_backoff_is_the_production_schedule() {
        // `BackoffSchedule::default()` must equal production (1s×2 cap 60s) — the
        // engine relies on the default being the real constants, never a stub.
        assert_eq!(BackoffSchedule::default(), BackoffSchedule::production());
    }

    #[test]
    fn backoff_sequence_is_1_2_4_8_16_32_then_capped_at_60() {
        // The exact AD-15 sequence: 1s, 2s, 4s, 8s, 16s, 32s, then 60s cap
        // (64s would exceed it), staying at 60s thereafter.
        let s = BackoffSchedule::production();
        assert_eq!(s.delay_for(1), Duration::from_secs(1));
        assert_eq!(s.delay_for(2), Duration::from_secs(2));
        assert_eq!(s.delay_for(3), Duration::from_secs(4));
        assert_eq!(s.delay_for(4), Duration::from_secs(8));
        assert_eq!(s.delay_for(5), Duration::from_secs(16));
        assert_eq!(s.delay_for(6), Duration::from_secs(32));
        // 2^6 = 64s > 60s cap.
        assert_eq!(s.delay_for(7), Duration::from_secs(60));
        assert_eq!(s.delay_for(8), Duration::from_secs(60));
        assert_eq!(s.delay_for(20), Duration::from_secs(60));
    }

    #[test]
    fn delay_for_zero_is_treated_as_first_restart() {
        // Defensive: a zero-th attempt has no meaning; it waits the base.
        let s = BackoffSchedule::production();
        assert_eq!(s.delay_for(0), s.delay_for(1));
        assert_eq!(s.delay_for(0), Duration::from_secs(1));
    }

    #[test]
    fn delay_for_huge_n_clamps_to_cap_without_overflow() {
        // A pathological consecutive count must clamp to the cap, never panic
        // (the 1u128 << shift guard + saturating multiply).
        let s = BackoffSchedule::production();
        assert_eq!(s.delay_for(u32::MAX), Duration::from_secs(60));
        assert_eq!(s.delay_for(1000), Duration::from_secs(60));
    }

    #[test]
    fn crash_loop_trips_at_exactly_five() {
        // AD-15: stop after EXACTLY 5 consecutive failures. Below 5 allows a
        // retry; at/above 5 is a crash loop.
        assert!(!is_crash_loop(0));
        assert!(!is_crash_loop(1));
        assert!(!is_crash_loop(4));
        assert!(is_crash_loop(5));
        assert!(is_crash_loop(6));
        assert!(is_crash_loop(100));
    }

    #[test]
    fn never_policy_does_not_restart_on_crash() {
        assert!(!RestartPolicy::Never.restarts_on_crash());
        assert!(RestartPolicy::OnFailure.restarts_on_crash());
    }

    #[test]
    fn default_policy_is_on_failure() {
        // [ASSUMPTION] recorded: AD-15 does not name the default; on-failure
        // matches "unattended agents are safe to run".
        assert_eq!(RestartPolicy::default(), RestartPolicy::OnFailure);
    }

    #[test]
    fn policy_wire_form_round_trips() {
        for policy in [RestartPolicy::Never, RestartPolicy::OnFailure] {
            let wire = policy.as_str();
            assert_eq!(RestartPolicy::from_wire(wire), Some(policy), "{wire}");
            assert_eq!(policy.to_string(), wire);
        }
        assert_eq!(RestartPolicy::from_wire("bogus"), None);
        // The kebab-case serde form matches as_str() so the DB string and any
        // config/event JSON never diverge.
        let json = serde_json::to_string(&RestartPolicy::OnFailure).unwrap();
        assert_eq!(json, "\"on-failure\"");
        let back: RestartPolicy = serde_json::from_str("\"never\"").unwrap();
        assert_eq!(back, RestartPolicy::Never);
    }

    #[test]
    fn scaled_schedule_preserves_the_doubling_shape() {
        // Test injection: a millisecond base keeps the ×2 shape (10ms, 20ms,
        // 40ms, …) and its own cap, so the crash-loop/backoff tests run fast
        // WITHOUT changing production constants.
        let s = BackoffSchedule::with_base_and_cap(
            Duration::from_millis(10),
            Duration::from_millis(50),
        );
        assert_eq!(s.delay_for(1), Duration::from_millis(10));
        assert_eq!(s.delay_for(2), Duration::from_millis(20));
        assert_eq!(s.delay_for(3), Duration::from_millis(40));
        // 80ms > 50ms cap.
        assert_eq!(s.delay_for(4), Duration::from_millis(50));
        assert_eq!(s.delay_for(10), Duration::from_millis(50));
    }

    #[test]
    fn stable_run_window_is_production_pinned() {
        // Story 18-3: the engine-owned window is exactly 30 s in production.
        // Guard the constant so a test-speed override can never leak into
        // prod (mirrors `production_constants_are_exactly_the_spine_values`).
        assert_eq!(STABLE_RUN_WINDOW, Duration::from_secs(30));
    }

    #[test]
    fn stable_run_resets_then_charges_failure_one() {
        // A run that held `running` >= the window resets the streak FIRST:
        // no matter how long the previous history was, this crash is #1.
        for current in [0u32, 1, 4, 5, 50] {
            let (next, reset) = next_consecutive_failures(
                current,
                Some(Duration::from_secs(30)),
                STABLE_RUN_WINDOW,
            );
            assert_eq!(next, 1, "current {current} resets then charges 1");
            assert!(reset, "current {current} reports the reset");
        }
    }

    #[test]
    fn stable_window_boundary_is_inclusive_at_exactly_the_window() {
        // The spec's `>=` at exactly 30 s: a run of exactly the window IS
        // stable. One tick under is not.
        let (next, reset) =
            next_consecutive_failures(3, Some(STABLE_RUN_WINDOW), STABLE_RUN_WINDOW);
        assert_eq!(next, 1);
        assert!(reset);
        let (next, reset) = next_consecutive_failures(
            3,
            Some(STABLE_RUN_WINDOW - Duration::from_nanos(1)),
            STABLE_RUN_WINDOW,
        );
        assert_eq!(next, 4);
        assert!(!reset);
    }

    #[test]
    fn young_run_increments_as_today() {
        // Below the window, the arithmetic is the pre-18-3 increment — the
        // helper must be a no-op against the old behavior.
        for current in [0u32, 1, 4] {
            let (next, reset) =
                next_consecutive_failures(current, Some(Duration::from_secs(1)), STABLE_RUN_WINDOW);
            assert_eq!(next, current + 1);
            assert!(!reset);
        }
    }

    #[test]
    fn no_anchor_never_resets() {
        // `None` — the adopted-instance case (no in-memory anchor) and the
        // paused-gate case — never resets, whatever the count.
        for current in [0u32, 4, 5, 99] {
            let (next, reset) = next_consecutive_failures(current, None, STABLE_RUN_WINDOW);
            assert_eq!(next, current + 1);
            assert!(!reset);
        }
    }

    #[test]
    fn saturated_counter_never_panics_or_wraps() {
        // A `u32::MAX` streak keeps charging as `u32::MAX` — no debug-overflow
        // panic, no wrap to 0 (0 would masquerade as a fresh streak). The
        // reset arm still wins first, so saturation is invisible to stable
        // runs either way.
        assert_eq!(
            next_consecutive_failures(u32::MAX, None, STABLE_RUN_WINDOW),
            (u32::MAX, false)
        );
        assert_eq!(
            next_consecutive_failures(u32::MAX, Some(STABLE_RUN_WINDOW), STABLE_RUN_WINDOW),
            (1, true)
        );
    }

    #[test]
    fn window_parameter_scales_for_tests() {
        // The test-scaling seam: the same helper with a millisecond window
        // keeps the exact shape (>= inclusive, reset-to-1, no-anchor
        // increment) so integration tests never sleep for real seconds.
        let window = Duration::from_millis(40);
        assert_eq!(
            next_consecutive_failures(2, Some(Duration::from_millis(40)), window),
            (1, true)
        );
        assert_eq!(
            next_consecutive_failures(2, Some(Duration::from_millis(39)), window),
            (3, false)
        );
        assert_eq!(next_consecutive_failures(2, None, window), (3, false));
    }
}
