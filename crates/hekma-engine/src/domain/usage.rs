//! Usage metering domain types (spine AD-6/AD-7, story 3-1) — the Usage Ledger's
//! leaf primitives.
//!
//! This module owns the metering-capture domain shapes: the [`RunId`] (the
//! `starting`→next-terminal span the supervisor mints), the [`UsageEvent`] (the
//! AD-7 minimum ledger row + the replay-dedup `sequence` ordinal), the
//! [`UsageTotals`] rollup a read sums, and the AD-14 [`UsageUpdateEvent`] wire
//! struct. It is pure data + a monotonic Run-id minter — no I/O, no ports, no
//! ledger write (those live in the store + supervisor).
//!
//! ## Scope boundary (story 3-1 is CAPTURE → LEDGER)
//!
//! A [`UsageEvent`] is TOKENS ONLY (AD-8: no currency ON THE ADAPTER-FACING WIRE
//! TYPE). There is NO dollar field, NO `EstimateLabel`, NO budget/headroom on the
//! event itself — those stay engine-side. Story 3-3 DERIVES dollars from these
//! token counts (via [`super::cost::cost_micros`]) and persists the effective
//! `Rate` per ledger ROW as an engine-side column (NOT on this frozen wire type —
//! AD-6, no retro-repricing); the derived cost + `EstimateLabel` surface in the
//! Fleet view ([`super::fleet::UsageView`]), never on the `UsageEvent` wire. The
//! `metering_source` rides as its wire string (the
//! [`hekma_adapter_api::MeteringSource`] kebab-case form) so the row is
//! self-describing without a cross-crate enum dependency in the ledger.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The schema version stamped on every emitted [`UsageUpdateEvent`] (AD-14).
///
/// A SEPARATE constant from `EVENT_SCHEMA_VERSION` (the transition-event schema)
/// and `FLEET_SCHEMA_VERSION` (the Fleet document) — the three wire shapes evolve
/// independently, so a change to one must not force a version bump on the others
/// (the same discipline `FLEET_SCHEMA_VERSION` records). It starts at 1, aligned
/// with the sibling event schemas. Bumped only on an INCOMPATIBLE change to the
/// usage-update shape; adding a field is backward-additive and does NOT bump it.
pub const USAGE_SCHEMA_VERSION: u32 = 1;

/// A Run identifier (spine AD-7) — the span from a `starting` transition to the
/// next terminal state (`stopped`/`failed`) of an Agent Instance.
///
/// Minted FRESH at each `starting` transition (including a Restart-Policy restart,
/// story 1-6 — a restarted instance opens a NEW Run), so per-run usage totals
/// never bleed across a crash/restart boundary. The id must be unique per run and
/// stable for that run's lifetime; [`RunId::mint`] derives it from the system
/// clock (nanoseconds) plus a process-global monotonic counter, so two Runs that
/// start in the SAME clock nanosecond still get DISTINCT ids (the counter breaks
/// the tie). It is persisted verbatim on each `usage_events` row (`run_id`
/// column) and is the `(instance_id, run_id, sequence)` dedup key's middle field.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId(String);

/// Process-global monotonic counter that breaks ties between two Runs minted in
/// the same clock nanosecond (or on a clock that does not advance between two
/// rapid mints). Never reset; wraps only after 2^64 mints (unreachable).
static RUN_NONCE: AtomicU64 = AtomicU64::new(0);

impl RunId {
    /// Mint a fresh, per-run-unique Run id.
    ///
    /// Shape: `run-<unix_nanos>-<nonce>`. The `unix_nanos` orders runs by wall
    /// time (useful for a human reading the ledger); the monotonic `nonce`
    /// guarantees uniqueness even when the clock is coarse or two mints land in
    /// the same nanosecond. A clock set before the epoch clamps `unix_nanos` to 0
    /// (the nonce still keeps it unique) — this is an id, not a correctness value.
    pub fn mint() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let nonce = RUN_NONCE.fetch_add(1, Ordering::Relaxed);
        Self(format!("run-{nanos}-{nonce}"))
    }

    /// Reconstruct a [`RunId`] from its stored wire string (a DB read / a
    /// deserialized event). No validation — the ledger stores whatever was minted.
    pub fn from_wire(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The Run id's wire string (the `run_id` column value + the serde form).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One usage measurement committed to the append-only Usage Ledger (spine AD-7).
///
/// Carries EXACTLY the AD-7 minimum shape — `{instance, run_id, input_tokens,
/// output_tokens, metering_source, occurred_at}` — plus the replay-dedup
/// `sequence` ordinal (the agent-supplied, per-Run-monotonic key that makes
/// "no double-count on replay" a DB invariant via `UNIQUE(instance_id, run_id,
/// sequence)`) and — since story 14-6 — the cached-token subset. Token counts are
/// `u64` (matching the SQLite `INTEGER` column, decoded via `.max(0) as u64`).
/// TOKENS ONLY — no dollars, no label, no budget (AD-8; those are later Epic-3
/// stories).
///
/// `Serialize`/`Deserialize` (snake_case) so it rides the AD-14
/// [`UsageUpdateEvent`] wire and can round-trip through `kt --json` / the 7-2
/// Host event bus without a second dialect.
///
/// **The cached subset (story 14-6, D8):** `cached_tokens` is the cached SUBSET
/// of `input_tokens` under the ledger's INPUT-INCLUSIVE invariant
/// (`0 <= cached_tokens <= input_tokens` — see [`crate::metering::parse`]'s
/// module docs). `Some(n)` = the cached count is KNOWN (every freshly-committed
/// event; `Some(0)` is a valid known-zero); `None` = UNKNOWN — a pre-v7 ledger
/// row (the additive v7 column is NULL there) or a legacy wire payload
/// (`#[serde(default)]`, so old JSON still deserializes). The dedup UNIQUE key
/// is UNCHANGED (the cached count is not part of it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// The Agent Instance this measurement belongs to (its unique name).
    pub instance: String,
    /// The Run (spine AD-7) this measurement was reported during.
    pub run_id: RunId,
    /// Input (prompt) tokens the agent reported for this event — INCLUSIVE of
    /// the cached subset (`0 <= cached_tokens <= input_tokens`, the 14-6
    /// invariant).
    pub input_tokens: u64,
    /// Output (completion) tokens the agent reported for this event.
    pub output_tokens: u64,
    /// The cached subset of `input_tokens` (story 14-6). `None` = unknown
    /// (a pre-v7 ledger row); a freshly-committed event is always `Some`
    /// (known-zero allowed). NOT part of the dedup key.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    /// The Metering Source that produced this event, as its wire string
    /// (`self-reported` / `engine-observed`). Frozen as a string so the ledger
    /// row is self-describing without depending on the adapter-api enum.
    pub metering_source: String,
    /// The per-Run-monotonic replay-dedup ordinal the agent supplies. Combined
    /// with `(instance, run_id)` it is the ledger's UNIQUE no-double-count key: a
    /// re-delivered event with the same `(run_id, sequence)` is a recognized
    /// replay, skipped rather than re-inserted.
    pub sequence: u64,
    /// RFC 3339 UTC timestamp the engine stamped when it committed the event.
    pub occurred_at: String,
}

impl UsageEvent {
    /// The total tokens (input + output) this single event reports. Saturating so
    /// a pathological pair near `u64::MAX` cannot overflow (it is a rollup helper,
    /// not a correctness-critical sum). The cached subset (story 14-6) is NOT
    /// added again — it rides INSIDE `input` (the INPUT-INCLUSIVE invariant).
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// A rollup of token usage over some scope (a whole instance, or one Run) — the
/// "rollup aggregates" half of AD-6's Usage Ledger, summed on read this story.
///
/// TOKENS ONLY (AD-8): input + output token sums, no dollars/headroom. An absent
/// instance (or a Run with no events) totals [`UsageTotals::zero`] — a truthful
/// zero, distinct from the Epic-1 "metering does not exist" absence.
///
/// **The cached rollup (story 14-6):** `cached_tokens` is the summed cached
/// SUBSET of the scope's input tokens (the INPUT-INCLUSIVE invariant — cached
/// rides INSIDE `input`, it is never added to the total again).
/// `Some(sum)` = every contributing row's cached count is known (a scope with
/// no rows at all is a truthful `Some(0)`); `None` = the scope CONTAINS a row
/// whose cached count is unknown (a pre-v7 row) — the honest absence token a
/// surface renders as `—`, never a fabricated zero (AI-18).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Summed input (prompt) tokens over the scope (INCLUSIVE of the cached
    /// subset — the 14-6 invariant).
    pub input_tokens: u64,
    /// Summed output (completion) tokens over the scope.
    pub output_tokens: u64,
    /// The summed cached subset of the input (story 14-6). `None` = the scope
    /// contains at least one row with an UNKNOWN cached count (a pre-v7 row);
    /// `Some(0)` = known-zero (including an empty scope).
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

impl UsageTotals {
    /// The all-zero total (no events in scope). The cached rollup is a truthful
    /// `Some(0)` — no rows means nothing unknown about the (empty) scope.
    pub const fn zero() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: Some(0),
        }
    }

    /// The combined input + output tokens (saturating — see [`UsageEvent::total_tokens`]).
    /// The cached subset is NOT added again: it rides inside `input` (the 14-6
    /// INPUT-INCLUSIVE invariant), so token budgets enforce on this same total.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// The outcome of an append-only ledger write (spine AD-6/AD-7) — did the row
/// land, or was it a recognized replay?
///
/// Returned by the store's `record_usage_event`. `Inserted` = a brand-new event
/// row; `DuplicateReplay` = the `(instance_id, run_id, sequence)` UNIQUE key
/// already existed, so the write was a NO-OP (the no-double-count DB invariant,
/// AC-A) — NOT an error. The commit choke point maps this to "counted once".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    /// A new ledger row was appended (this event was counted).
    Inserted,
    /// The event's dedup key already existed — a re-delivered batch. Nothing was
    /// inserted; the ledger total is unchanged (AC-A no-double-count).
    DuplicateReplay,
}

impl RecordOutcome {
    /// Whether this outcome appended a new row (`true` for [`RecordOutcome::Inserted`]).
    pub fn is_inserted(&self) -> bool {
        matches!(self, RecordOutcome::Inserted)
    }
}

/// A committed-usage event on the AD-14 event surface — the versioned wire struct
/// `kt --json` and the 7-2 Host event bus share ("one event schema, two
/// consumers").
///
/// AD-14 names "usage updates" among the versioned engine event structs. 3-1
/// FREEZES the wire shape — a [`USAGE_SCHEMA_VERSION`]-stamped struct carrying
/// the committed [`UsageEvent`] — and EMITS it from the ledger-commit choke point,
/// so `kt --json` and the Host stream cannot drift into two dialects. Subscription
/// DELIVERY shipped in story 7-2: the choke point publishes this struct onto the
/// event bus right after the row commits (story 3-1's original return-it-for-
/// observation seam stands unchanged). TOKENS ONLY — no dollars in the payload
/// (3-3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageUpdateEvent {
    /// The usage-event schema version ([`USAGE_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The committed usage measurement (the AD-7 minimum shape + dedup ordinal).
    pub event: UsageEvent,
}

impl UsageUpdateEvent {
    /// Wrap a committed [`UsageEvent`], stamping the current [`USAGE_SCHEMA_VERSION`].
    pub fn new(event: UsageEvent) -> Self {
        Self {
            schema_version: USAGE_SCHEMA_VERSION,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(seq: u64) -> UsageEvent {
        UsageEvent {
            instance: "demo".to_string(),
            run_id: RunId::from_wire("run-1"),
            input_tokens: 10,
            output_tokens: 20,
            cached_tokens: Some(0),
            metering_source: "self-reported".to_string(),
            sequence: seq,
            occurred_at: "2026-07-06T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn mint_produces_distinct_run_ids_even_back_to_back() {
        // AC-B: two `starting` transitions mint DISTINCT run ids. Minting in a
        // tight loop is the worst case (same clock nanosecond) — the monotonic
        // nonce must still make every id unique.
        let ids: Vec<RunId> = (0..1000).map(|_| RunId::mint()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "all minted run ids must be unique");
        // Shape sanity: starts with the `run-` prefix.
        assert!(ids[0].as_str().starts_with("run-"), "{}", ids[0]);
    }

    #[test]
    fn run_id_round_trips_through_wire_and_display() {
        let id = RunId::from_wire("run-42-7");
        assert_eq!(id.as_str(), "run-42-7");
        assert_eq!(id.to_string(), "run-42-7");
        // serde round-trip (it rides the usage-update wire).
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"run-42-7\"");
        let back: RunId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn usage_event_is_tokens_only_and_round_trips_snake_case() {
        // AC4: the AD-7 minimum shape + the dedup ordinal + the 14-6 additive
        // cached-subset field, snake_case on the wire, NO dollar/label/budget
        // field.
        let event = sample_event(3);
        let value: serde_json::Value = serde_json::to_value(&event).unwrap();
        // Exactly the AD-7 fields + sequence + the additive cached_tokens.
        let obj = value.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "cached_tokens",
                "input_tokens",
                "instance",
                "metering_source",
                "occurred_at",
                "output_tokens",
                "run_id",
                "sequence",
            ],
            "UsageEvent must be the AD-7 shape + sequence + the 14-6 cached subset, nothing more"
        );
        // No dollar/label/budget leaked in.
        assert!(obj.get("cost").is_none());
        assert!(obj.get("dollars").is_none());
        assert!(obj.get("estimate_label").is_none());
        assert!(obj.get("budget").is_none());
        // Lossless round-trip.
        let back: UsageEvent = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn total_tokens_sums_and_saturates() {
        assert_eq!(sample_event(1).total_tokens(), 30);
        let big = UsageEvent {
            input_tokens: u64::MAX,
            output_tokens: 5,
            ..sample_event(1)
        };
        assert_eq!(big.total_tokens(), u64::MAX, "saturating, never overflow");
    }

    #[test]
    fn usage_totals_zero_and_sum() {
        let z = UsageTotals::zero();
        assert_eq!(z.input_tokens, 0);
        assert_eq!(z.output_tokens, 0);
        assert_eq!(z.total_tokens(), 0);
        // 14-6: an empty scope's cached rollup is a truthful KNOWN zero (nothing
        // unknown about an empty scope), never the unknown-absence None.
        assert_eq!(z.cached_tokens, Some(0));
        let t = UsageTotals {
            input_tokens: 100,
            output_tokens: 250,
            cached_tokens: Some(40),
        };
        assert_eq!(t.total_tokens(), 350);
        // The cached subset does NOT re-enter the total (the inclusive invariant:
        // it rides inside input) — 350, not 390.
        assert_eq!(
            t.total_tokens(),
            350,
            "cached rides inside input, never added again"
        );
        // Default is the unknown-cached absence (a DEGRADED read's honest shape —
        // `unwrap_or_default` on a failed ledger read must not fabricate zeros).
        assert_eq!(
            UsageTotals::default(),
            UsageTotals {
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: None,
            }
        );
    }

    #[test]
    fn usage_event_cached_token_additive_wire_back_compat() {
        // 14-6: an OLD wire payload (pre-14-6 JSON, no cached_tokens key)
        // deserializes to the UNKNOWN absence (None) — backward-additive, the
        // usage-update schema version does NOT bump (per USAGE_SCHEMA_VERSION's
        // own additive-field rule).
        let old = r#"{"instance":"demo","run_id":"run-1","input_tokens":10,
            "output_tokens":20,"metering_source":"self-reported","sequence":1,
            "occurred_at":"2026-07-06T00:00:00Z"}"#;
        let back: UsageEvent = serde_json::from_str(old).unwrap();
        assert_eq!(back.cached_tokens, None, "old payload -> unknown cached");
        assert_eq!(back.input_tokens, 10);
        // And a fresh payload round-trips the known subset.
        let fresh = sample_event(1);
        let json = serde_json::to_string(&fresh).unwrap();
        let back: UsageEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fresh);
    }

    #[test]
    fn record_outcome_is_inserted_predicate() {
        assert!(RecordOutcome::Inserted.is_inserted());
        assert!(!RecordOutcome::DuplicateReplay.is_inserted());
    }

    #[test]
    fn usage_update_event_carries_schema_version_and_round_trips() {
        // AD-14: the versioned wire struct `kt --json` + 7-2 share. It carries the
        // schema version + the committed event, snake_case, and round-trips.
        let update = UsageUpdateEvent::new(sample_event(5));
        assert_eq!(update.schema_version, USAGE_SCHEMA_VERSION);
        let value: serde_json::Value = serde_json::to_value(&update).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(USAGE_SCHEMA_VERSION)
        );
        assert_eq!(value["event"]["sequence"], serde_json::json!(5));
        assert_eq!(value["event"]["input_tokens"], serde_json::json!(10));
        // Tokens only — no dollars in the payload.
        assert!(value["event"].get("cost").is_none());
        let json = serde_json::to_string(&update).unwrap();
        let back: UsageUpdateEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, update);
    }
}
