//! The lifecycle state machine as DATA (spine AD-15) — the transition table.
//!
//! One place decides "given the current [`LifecycleState`] and a
//! [`LifecycleCommand`], what is the next state (or the uniform error)". The
//! table is a total `match` over the two enums — which IS "data" per AD-15 (a
//! total function over a finite product is a table; no external data file is
//! needed). The supervisor calls [`next_state`] to decide, then persists the new
//! state and emits the transition event. Keeping this function PURE (no I/O, no
//! adapter, no OS) makes it exhaustively unit-testable and keeps the domain core
//! clean (AD-1).
//!
//! ## Command-driven vs event-driven edges (documented shape)
//!
//! `Start`, `Stop`, `Pause`, and `Resume` are OPERATOR **commands** — the rows
//! in [`next_state`]. The remaining edges this story reaches are SUPERVISOR
//! reactions to process events, not commands, and are applied directly by the
//! supervisor (each still persisting + emitting the AD-14 event):
//!
//! * `starting → running`  — adapter ready (process spawned and not immediately dead)
//! * `starting → failed`   — launch error
//! * `stopping → stopped`  — process exited (gracefully or after a forced kill)
//! * `running → failed`    — crash detected (story 1-6, event-driven; the
//!   supervisor's reaper applies it when `backend.poll` reports an unrequested
//!   `Exited` for an instance the store still shows `running`/`paused`)
//!
//! Modeling only the command edges here keeps the AC4 uniform-error contract
//! precise: `InvalidTransition` is returned for a rejected COMMAND (e.g. `Stop`
//! on `stopped`, `Start` on `running`, `Pause` on `stopped`, `Resume` on
//! `running`), and the event-driven edges never reject (the supervisor only
//! applies them when the corresponding process event has actually happened).
//!
//! ## Reachable this story (1-6 wires the crash + restart edges)
//!
//! `registered → starting → running → stopping → stopped`, plus
//! `starting → failed` (launch error) and the `stopped → starting` restart of a
//! previously stopped instance (FR-5: start applies to registered *or* stopped).
//! Story 1-5 wires `paused`: `running --Pause--> paused`,
//! `paused --Resume--> running`, and `paused --Stop--> stopping` (the spine
//! state diagram's `paused --> stopping`, so a paused instance is stoppable).
//! Story 1-6 wires the SURVIVAL edges: the `running → failed` crash edge
//! (EVENT-driven, applied by the supervisor's reaper — see the event-driven set
//! above, NOT a command row) and `failed --Start--> starting` (a COMMAND row, so
//! a `failed` instance is restartable by both the Restart Policy executor and an
//! explicit `kt agent start`). The Restart Policy math lives in
//! [`super::restart`], a pure value module. The whole table stays PURE + total.

use thiserror::Error;

use super::lifecycle::LifecycleState;

/// An operator lifecycle command (spine AD-15 verbs).
///
/// The full operator command set through story 1-5: [`LifecycleCommand::Start`],
/// [`LifecycleCommand::Stop`], [`LifecycleCommand::Pause`], and
/// [`LifecycleCommand::Resume`] (the ratified AD-15 verbs). `Pause`/`Resume` join
/// the table in story 1-5, making `paused` reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleCommand {
    /// Start a registered (or previously stopped) instance.
    Start,
    /// Stop a running (or paused) instance.
    Stop,
    /// Pause a running instance (story 1-5).
    Pause,
    /// Resume a paused instance (story 1-5).
    Resume,
}

impl LifecycleCommand {
    /// A short, stable label used in diagnostics and the transition event cause.
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecycleCommand::Start => "start",
            LifecycleCommand::Stop => "stop",
            LifecycleCommand::Pause => "pause",
            LifecycleCommand::Resume => "resume",
        }
    }
}

impl std::fmt::Display for LifecycleCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The single uniform lifecycle error class (spine AD-15; AC4).
///
/// Every invalid `(state, command)` pair returns
/// [`LifecycleError::InvalidTransition`] — the SAME error for every adapter,
/// because the rejection comes from the shared table in the engine core before
/// any adapter code runs (AC4 "same error class for every adapter"). `thiserror`
/// in the engine; `kt` maps it to a miette diagnostic.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LifecycleError {
    /// The command is not valid from the instance's current state (e.g. `stop`
    /// on `stopped`, `start` on `running`). Names both so the diagnostic is
    /// precise and identical across adapters.
    #[error("cannot {command} an Agent Instance while it is '{from}'")]
    InvalidTransition {
        /// The state the instance was in when the command arrived.
        from: LifecycleState,
        /// The command that was rejected.
        command: LifecycleCommand,
    },
}

/// The transition table (AD-15): `(state, command) -> next state | error`.
///
/// PURE — no I/O. Total over the two enums: every reachable command edge maps to
/// the right next state; every other pair returns the single
/// [`LifecycleError::InvalidTransition`] class (AC4). The supervisor calls this,
/// then persists + emits.
pub fn next_state(
    from: LifecycleState,
    command: LifecycleCommand,
) -> Result<LifecycleState, LifecycleError> {
    use LifecycleCommand::*;
    use LifecycleState::*;
    match (from, command) {
        // Start a registered instance, or restart a previously stopped one
        // (FR-5: start applies to registered OR stopped instances).
        (Registered, Start) => Ok(Starting),
        (Stopped, Start) => Ok(Starting),
        // Restart a FAILED instance (story 1-6, AC3): the `failed → starting`
        // command edge. Applied by both the Restart Policy executor (on a
        // detected crash of an `on-failure` instance) and an explicit
        // `kt agent start` on a `failed` instance. This EXTENDS FR-5's
        // "registered OR stopped" to also cover `failed`.
        (Failed, Start) => Ok(Starting),
        // Stop a running OR paused instance (spine diagram: paused --> stopping).
        (Running, Stop) => Ok(Stopping),
        (Paused, Stop) => Ok(Stopping),
        // Pause a running instance; resume a paused one (story 1-5, AC4).
        (Running, Pause) => Ok(Paused),
        (Paused, Resume) => Ok(Running),
        // Every other (state, command) pair is an invalid COMMAND transition.
        // The event-driven edges (starting→running, starting→failed,
        // stopping→stopped, and the story-1-6 running→failed crash edge) are
        // applied by the supervisor on process events, not through this command
        // table, so they are not rows here.
        (from, command) => Err(LifecycleError::InvalidTransition { from, command }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LifecycleCommand::*;
    use LifecycleState::*;

    #[test]
    fn start_from_registered_enters_starting() {
        assert_eq!(next_state(Registered, Start), Ok(Starting));
    }

    #[test]
    fn start_from_stopped_restarts_into_starting() {
        // FR-5: a previously stopped instance can be started again.
        assert_eq!(next_state(Stopped, Start), Ok(Starting));
    }

    #[test]
    fn stop_from_running_enters_stopping() {
        assert_eq!(next_state(Running, Stop), Ok(Stopping));
    }

    #[test]
    fn invalid_command_pairs_all_yield_the_same_error_class() {
        // A representative set of invalid pairs (AC4). Each must be the ONE
        // uniform InvalidTransition class, naming from + command. NOTE: as of
        // story 1-5, (Paused, Stop), (Running, Pause) and (Paused, Resume) are
        // VALID rows and are asserted in the exhaustive test below.
        let invalid = [
            (Stopped, Stop),      // stop on stopped
            (Running, Start),     // start on running
            (Registered, Stop),   // stop on registered
            (Starting, Start),    // start while starting
            (Starting, Stop),     // stop while starting
            (Stopping, Start),    // start while stopping
            (Stopping, Stop),     // stop while stopping
            (Failed, Stop),       // stop on failed
            (Paused, Start),      // start on paused
            (Stopped, Pause),     // pause on stopped (1-5)
            (Registered, Pause),  // pause on registered (1-5)
            (Paused, Pause),      // pause on paused (1-5)
            (Starting, Pause),    // pause while starting (1-5)
            (Stopping, Pause),    // pause while stopping (1-5)
            (Failed, Pause),      // pause on failed (1-5)
            (Running, Resume),    // resume on running (1-5)
            (Stopped, Resume),    // resume on stopped (1-5)
            (Registered, Resume), // resume on registered (1-5)
            (Starting, Resume),   // resume while starting (1-5)
            (Stopping, Resume),   // resume while stopping (1-5)
            (Failed, Resume),     // resume on failed (1-5)
        ];
        for (from, command) in invalid {
            let err = next_state(from, command).unwrap_err();
            assert_eq!(
                err,
                LifecycleError::InvalidTransition { from, command },
                "({from:?}, {command:?}) must be InvalidTransition"
            );
        }
    }

    #[test]
    fn exhaustive_over_every_state_command_pair() {
        // AD-15 "exhaustively unit-tested": drive EVERY (state, command) pair and
        // assert the reachable ones map correctly and all others reject. This is
        // the cheapest, most complete coverage in the story.
        let all_states = [
            Registered, Starting, Running, Paused, Stopping, Stopped, Failed,
        ];
        let all_commands = [Start, Stop, Pause, Resume];
        for from in all_states {
            for command in all_commands {
                let result = next_state(from, command);
                let expected = match (from, command) {
                    (Registered, Start) => Ok(Starting),
                    (Stopped, Start) => Ok(Starting),
                    (Failed, Start) => Ok(Starting),
                    (Running, Stop) => Ok(Stopping),
                    (Paused, Stop) => Ok(Stopping),
                    (Running, Pause) => Ok(Paused),
                    (Paused, Resume) => Ok(Running),
                    (from, command) => Err(LifecycleError::InvalidTransition { from, command }),
                };
                assert_eq!(result, expected, "({from:?}, {command:?})");
            }
        }
    }

    #[test]
    fn pause_resume_stop_from_paused_are_the_wired_1_5_edges() {
        // AC4 (1-5): the three edges story 1-5 adds are the ONLY new Ok rows.
        assert_eq!(next_state(Running, Pause), Ok(Paused));
        assert_eq!(next_state(Paused, Resume), Ok(Running));
        // A paused instance must be stoppable (spine diagram: paused --> stopping).
        assert_eq!(next_state(Paused, Stop), Ok(Stopping));
    }

    #[test]
    fn restart_from_failed_enters_starting() {
        // AC3 (1-6): the ONE new command row story 1-6 adds — a `failed`
        // instance is restartable (by the Restart Policy executor or an explicit
        // `kt agent start`). Every other command from `failed` still rejects.
        assert_eq!(next_state(Failed, Start), Ok(Starting));
        assert!(matches!(
            next_state(Failed, Stop),
            Err(LifecycleError::InvalidTransition { .. })
        ));
        assert!(matches!(
            next_state(Failed, Pause),
            Err(LifecycleError::InvalidTransition { .. })
        ));
        assert!(matches!(
            next_state(Failed, Resume),
            Err(LifecycleError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn error_message_names_state_and_command() {
        let err = next_state(Stopped, Stop).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stop"), "{msg}");
        assert!(msg.contains("stopped"), "{msg}");
    }

    #[test]
    fn command_labels_are_stable() {
        assert_eq!(Start.as_str(), "start");
        assert_eq!(Stop.as_str(), "stop");
        assert_eq!(Pause.as_str(), "pause");
        assert_eq!(Resume.as_str(), "resume");
        assert_eq!(Start.to_string(), "start");
        assert_eq!(Pause.to_string(), "pause");
        assert_eq!(Resume.to_string(), "resume");
    }
}
