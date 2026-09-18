//! Engine-observed metering (spine AD-7 / FR-19 `engine-observed` half), story
//! 3-4 — the loopback forward listener + the OpenAI-compatible `usage` parse.
//!
//! This is the spine-reserved `src/metering/` home ("pipeline + loopback listener
//! (AD-7)"). It owns the ONE genuinely new subsystem 3-4 adds: a per-instance,
//! loopback-only ([`listener::ObservedListener`]) HTTP forward proxy that observes
//! the agent's OpenAI-compatible model traffic and skims usage from the responses
//! ([`parse::parse_openai_usage`]). The parsed counts flow — via the shared
//! [`listener::ObservedQueue`] the supervisor's reaper drains — into the SAME
//! `Supervisor::ingest_usage` choke point 3-1 built, tagged `engine-observed`, so
//! 3-2's token budgets and 3-3's dollar caps enforce on observed usage with ZERO
//! re-plumbing (the enforcement reads committed totals, which now include
//! `engine-observed` rows).
//!
//! ## Engine-INTERNAL (AD-2)
//!
//! `kt` never sees the listener — it reads rendered Fleet totals + the
//! `engine-observed` Metering Source through the existing accessors. This module
//! re-exports ONLY what the supervisor needs (the listener + the queue type);
//! nothing crosses the `kt` boundary.
//!
//! ## Portable — NO OS-conditional compilation (the crux, NOT `backends/`)
//!
//! Async TCP + HTTP over loopback is portable `std`/tokio/`hyper` — identical on
//! Linux, macOS, and Windows with NO OS-conditional attributes (no per-OS `#[cfg]`
//! gate anywhere in this module). So it lives in the engine CORE here, NOT in
//! `backends/`, and the OS-cfg grep gate stays green. See [`listener`] for the
//! loopback-only bind (AC-B) + the no-leak discipline (the proxy carries the
//! agent's API key upstream but leaks NONE of it into the ledger/logs/errors).

pub(crate) mod listener;
mod parse;

pub(crate) use listener::{ListenerError, ObservedListener};
