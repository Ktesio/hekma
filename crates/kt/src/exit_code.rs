//! The `kt` numeric exit-code contract (story 4-3, FR-26 / PRD §7).
//!
//! `kt` returns a documented, stable set of numeric exit codes so it is
//! scriptable without the Embedding Interface. This is a **v1 compatibility
//! surface** governed by PRD §7 (announce → one-minor notice → remove-at-major):
//! the numbers below are FROZEN, and the compatibility tests
//! (`crates/kt/tests/agent_cli.rs` + this module's own unit tests) pin them so an
//! unannounced change fails CI on all three OSes.
//!
//! **How each code is actually gated** (fix pass, 2026-07-21 — the chain has three
//! links and all three are now pinned; previously the middle one was not):
//! 1. *which code a diagnostic classifies as* — this module's unit tests, cross-OS;
//! 2. *which diagnostic each engine/registry condition PRODUCES* —
//!    `cli::agent::tests::every_engine_error_mapper_arm_preserves_its_documented_exit_code`
//!    and its `map_error` sibling, cross-OS. Without link 2 an adversarial pass
//!    could (and did) retarget `map_engine_error`'s `InteractionTimedOut` arm at
//!    `AgentIo`, silently demoting the documented `6` to `1`, with every test green;
//! 3. *that `main` exits with the classified code* — the end-to-end tests in
//!    `agent_cli.rs`, cross-OS for `0`/`1`/`2`/`3`/`4`, plus Unix-only end-to-end
//!    proofs for `5`. Codes `5` and `6` have no cross-OS end-to-end path (both need
//!    a genuinely running child); links 1+2 gate them everywhere instead.
//!
//! ## The table (ratified 2026-07-20)
//!
//! | Code | Meaning | Mapped from (`crate::error` diagnostics) |
//! |------|---------|------------------------------------------|
//! | `0` | Success | `Ok(())` |
//! | `1` | General/internal error (catch-all) | `AgentIo`, `AgentStore`, `AgentConfig`, `AgentLaunchFailed`, `AgentManifestInvalid`, `AgentContractIncompatible`, `AgentManifestUnreadable`, `AgentNoMeteringSource`, `AgentNoCapabilities`, `SelfUpdateFailed`, + any unmapped error |
//! | `2` | Usage error (invalid invocation) | clap parse/usage (unchanged — clap exits `2` itself), `AgentInvalidName`, `AgentUnknownKind`, `AgentUnknownConfigKey`, `AgentDuplicateName` |
//! | `3` | Not found | `AgentNotFound`, `AgentManifestNotFound` |
//! | `4` | Invalid state | `AgentNotRunning`, `AgentRunningRequiresForce`, `AgentInvalidTransition`, `AgentStopUnconfirmed`, `AgentMemoryHotSwap`, `AgentMemoryKindConflict` |
//! | `5` | Unsupported capability | `AgentCapabilityUnsupported`, `AgentResumeUnsupported`, `AgentInteractionUnavailable` |
//! | `6` | Timed out | `AgentInteractionTimedOut` |
//!
//! ## Why a downcast classifier (not a `CliError` enum)
//!
//! The 22 diagnostics are independent `thiserror` + `miette` structs boxed as
//! `Box<dyn std::error::Error>` (miette lives in `kt` only — conventions). Rather
//! than wrap all of them in one enum (which would touch every `map_*` mapper and
//! all 22 structs), `main` classifies the boxed error here by DOWNCAST — the
//! low-churn approach the story recommends. A `CliError` enum is the cleaner
//! long-term shape but was deliberately deferred to keep this change localized
//! (recorded decision; propose to the reviewer if preferred).
//!
//! **Catch-all behavior (the one design risk worth flagging):** any error type not
//! explicitly matched below — including a NEW diagnostic added later without a
//! classifier arm — falls to [`ExitCode::General`] (`1`). This preserves the
//! pre-4-3 "every runtime error → 1" behavior and is the documented default. A new
//! diagnostic that ought to carry a different code must be added to both the match
//! below AND its unit test (which is exactly the "announce" gate).

// Only the diagnostics the classifier matches by downcast are imported here; the
// code-1 diagnostics all fall through the catch-all arm (so they are NOT named in
// `classify`), and are imported inside the test module where they are constructed.
use crate::error::{
    AgentCapabilityUnsupported, AgentDuplicateName, AgentInteractionTimedOut,
    AgentInteractionUnavailable, AgentInvalidName, AgentInvalidTransition, AgentManifestNotFound,
    AgentMemoryHotSwap, AgentMemoryKindConflict, AgentNotFound, AgentNotRunning,
    AgentResumeUnsupported, AgentRunningRequiresForce, AgentStopUnconfirmed, AgentUnknownConfigKey,
    AgentUnknownKind,
};

/// The documented, stable `kt` process exit codes (story 4-3). A FROZEN v1
/// compatibility surface — see the module docs for the governing table.
///
/// The discriminants ARE the wire contract; changing a number is a breaking
/// change under PRD §7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// `0` — the command succeeded.
    ///
    /// Never CONSTRUCTED: a successful run simply returns from `main`, and the OS
    /// exit status is then `0` — `classify` is only ever called on an `Err`. The
    /// variant exists because `0` is part of the documented, frozen table (and the
    /// compatibility test pins its number), so the contract lives in ONE place
    /// rather than being half-implicit.
    #[allow(dead_code)]
    Success = 0,
    /// `1` — a general/internal error, or any error not otherwise classified.
    General = 1,
    /// `2` — an invalid CLI invocation (shared with clap's own usage exit).
    Usage = 2,
    /// `3` — a named instance or manifest does not exist.
    NotFound = 3,
    /// `4` — the instance is not in a state that permits the operation.
    InvalidState = 4,
    /// `5` — the agent's Capability Declaration forbids the operation.
    Unsupported = 5,
    /// `6` — a bounded operation exceeded its deadline.
    TimedOut = 6,
}

impl ExitCode {
    /// The numeric code as the `i32` [`std::process::exit`] takes.
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Classify a runtime diagnostic into its documented [`ExitCode`] (story 4-3).
///
/// `main` calls this on the boxed error returned by `run_cli` (clap's own
/// parse/usage errors already exited `2`, and `--help`/`--version` exited `0`,
/// from inside `Cli::parse()`, so they never reach here). Every `crate::error`
/// diagnostic is matched by downcast; anything unmatched — including a future
/// diagnostic without an arm here — falls to [`ExitCode::General`] (`1`), the
/// documented catch-all that preserves the pre-4-3 behavior.
pub fn classify(err: &(dyn std::error::Error + 'static)) -> ExitCode {
    // 3 — not found.
    if err.is::<AgentNotFound>() || err.is::<AgentManifestNotFound>() {
        ExitCode::NotFound
    // 2 — usage error (invalid invocation): a bad name, an unknown kind/config
    // key, or a duplicate name. (clap's parse/usage errors are already `2`.)
    } else if err.is::<AgentInvalidName>()
        || err.is::<AgentUnknownKind>()
        || err.is::<AgentUnknownConfigKey>()
        || err.is::<AgentDuplicateName>()
    {
        ExitCode::Usage
    // 4 — invalid state: the instance is not in a state that permits the op.
    } else if err.is::<AgentNotRunning>()
        || err.is::<AgentRunningRequiresForce>()
        || err.is::<AgentInvalidTransition>()
        || err.is::<AgentStopUnconfirmed>()
        // Story 5-1: a Memory Backing cannot be hot-swapped (attach/detach need
        // a terminal state; no force escape) and kinds never conflict-overwrite.
        || err.is::<AgentMemoryHotSwap>()
        || err.is::<AgentMemoryKindConflict>()
    {
        ExitCode::InvalidState
    // 5 — unsupported capability: the Capability Declaration forbids it.
    } else if err.is::<AgentCapabilityUnsupported>()
        || err.is::<AgentResumeUnsupported>()
        || err.is::<AgentInteractionUnavailable>()
    {
        ExitCode::Unsupported
    // 6 — timed out: a bounded operation exceeded its deadline.
    } else if err.is::<AgentInteractionTimedOut>() {
        ExitCode::TimedOut
    // 1 — general/internal error AND the documented catch-all. This arm covers
    // the modeled code-1 diagnostics (`AgentIo`, `AgentStore`, `AgentConfig`,
    // `AgentLaunchFailed`, `AgentManifestInvalid`, `AgentContractIncompatible`,
    // `AgentManifestUnreadable`, `AgentNoMeteringSource`, `AgentNoCapabilities`,
    // `SelfUpdateFailed`) AND any
    // OTHER `dyn Error` not matched above — including a future diagnostic added
    // without a classifier arm — so an unclassified error preserves the pre-4-3
    // "every runtime error → 1" behavior rather than panicking. Its mapping to
    // `1` is pinned by `general_and_internal_diagnostics_map_to_one` +
    // `an_unmapped_error_falls_through_to_the_general_catch_all`.
    } else {
        ExitCode::General
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The code-1 diagnostics are only constructed in these tests (the classifier
    // reaches them through the catch-all, so they are not named in `classify`).
    use crate::error::{
        AgentConfig, AgentContractIncompatible, AgentIo, AgentLaunchFailed, AgentManifestInvalid,
        AgentManifestUnreadable, AgentNoCapabilities, AgentNoMeteringSource, AgentStore,
        SelfUpdateFailed,
    };

    /// Box a diagnostic exactly as the `map_*` mappers do (`.into()`), so the
    /// classifier sees the same `Box<dyn Error>` shape `main` receives.
    fn boxed(err: impl std::error::Error + 'static) -> Box<dyn std::error::Error> {
        Box::new(err)
    }

    #[test]
    fn exit_code_numbers_are_the_frozen_v1_contract() {
        // The DISCRIMINANTS are the compatibility surface (PRD §7). Pinning them
        // here makes an unannounced renumber fail CI (the "announce" gate).
        assert_eq!(ExitCode::Success.code(), 0);
        assert_eq!(ExitCode::General.code(), 1);
        assert_eq!(ExitCode::Usage.code(), 2);
        assert_eq!(ExitCode::NotFound.code(), 3);
        assert_eq!(ExitCode::InvalidState.code(), 4);
        assert_eq!(ExitCode::Unsupported.code(), 5);
        assert_eq!(ExitCode::TimedOut.code(), 6);
    }

    #[test]
    fn general_and_internal_diagnostics_map_to_one() {
        // Every code-1 diagnostic from the ratified table.
        for err in [
            boxed(AgentIo {
                message: "io".into(),
            }),
            boxed(AgentStore {
                message: "store".into(),
            }),
            boxed(AgentConfig {
                message: "config".into(),
            }),
            boxed(AgentLaunchFailed {
                message: "launch".into(),
            }),
            boxed(AgentManifestInvalid {
                message: "invalid".into(),
            }),
            boxed(AgentContractIncompatible {
                message: "incompatible contract".into(),
            }),
            boxed(AgentManifestUnreadable {
                message: "unreadable".into(),
            }),
            boxed(AgentNoMeteringSource {
                message: "no-metering".into(),
            }),
            boxed(AgentNoCapabilities {
                message: "no-caps".into(),
            }),
            boxed(SelfUpdateFailed {
                message: "self-update".into(),
            }),
        ] {
            assert_eq!(
                classify(err.as_ref()),
                ExitCode::General,
                "{err} should be General (1)",
            );
        }
    }

    #[test]
    fn usage_diagnostics_map_to_two() {
        for err in [
            boxed(AgentInvalidName {
                message: "bad".into(),
            }),
            boxed(AgentUnknownKind {
                message: "kind".into(),
            }),
            boxed(AgentUnknownConfigKey {
                message: "key".into(),
            }),
            boxed(AgentDuplicateName {
                message: "dup".into(),
            }),
        ] {
            assert_eq!(
                classify(err.as_ref()),
                ExitCode::Usage,
                "{err} should be Usage (2)",
            );
        }
    }

    #[test]
    fn not_found_diagnostics_map_to_three() {
        for err in [
            boxed(AgentNotFound {
                message: "gone".into(),
            }),
            boxed(AgentManifestNotFound {
                message: "no-manifest".into(),
            }),
        ] {
            assert_eq!(
                classify(err.as_ref()),
                ExitCode::NotFound,
                "{err} should be NotFound (3)",
            );
        }
    }

    #[test]
    fn invalid_state_diagnostics_map_to_four() {
        for err in [
            boxed(AgentNotRunning {
                message: "not-running".into(),
            }),
            boxed(AgentRunningRequiresForce {
                message: "force".into(),
            }),
            boxed(AgentInvalidTransition {
                message: "transition".into(),
            }),
            boxed(AgentStopUnconfirmed {
                message: "stop".into(),
            }),
            // Story 5-1: the two memory-guard diagnostics join code 4 — no new
            // exit-code number was minted (DC-4).
            boxed(crate::error::AgentMemoryHotSwap {
                message: "hot-swap".into(),
            }),
            boxed(crate::error::AgentMemoryKindConflict {
                message: "kind-conflict".into(),
            }),
        ] {
            assert_eq!(
                classify(err.as_ref()),
                ExitCode::InvalidState,
                "{err} should be InvalidState (4)",
            );
        }
    }

    #[test]
    fn unsupported_capability_diagnostics_map_to_five() {
        for err in [
            boxed(AgentCapabilityUnsupported {
                message: "unsupported".into(),
            }),
            // AI-7: the dedicated resume-under-unsupported-pause diagnostic joins
            // code 5 — the same capability-unsupported class, no new number (DC-4).
            boxed(crate::error::AgentResumeUnsupported {
                message: "resume-unsupported".into(),
            }),
            boxed(AgentInteractionUnavailable {
                message: "unavailable".into(),
            }),
        ] {
            assert_eq!(
                classify(err.as_ref()),
                ExitCode::Unsupported,
                "{err} should be Unsupported (5)",
            );
        }
    }

    #[test]
    fn timed_out_diagnostic_maps_to_six() {
        let err = boxed(AgentInteractionTimedOut {
            message: "timed out".into(),
        });
        assert_eq!(classify(err.as_ref()), ExitCode::TimedOut);
    }

    #[test]
    fn an_unmapped_error_falls_through_to_the_general_catch_all() {
        // A plain std error that is NONE of the modeled diagnostics must still
        // classify (never panic) — the documented catch-all → General (1), which
        // preserves the pre-4-3 "every runtime error → 1" behavior.
        let err: Box<dyn std::error::Error> = "some other failure".into();
        assert_eq!(classify(err.as_ref()), ExitCode::General);
    }
}
