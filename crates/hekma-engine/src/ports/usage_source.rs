//! The [`UsageSource`] ingestion port (hexagonal, spine AD-1/AD-7) — the ingest
//! side of the metering pipeline seam, story 3-1.
//!
//! ## Why this is NOT `MeteringSource`
//!
//! [`hekma_adapter_api::MeteringSource`] is the DECLARATION enum an adapter uses
//! to say WHICH kind of metering it does (`self-reported` / `engine-observed`),
//! resolved at registration. This port is a DIFFERENT thing — the AD-1 side port
//! the spine reserves that YIELDS [`UsageEvent`] fields from a running instance.
//! It is named [`UsageSource`] precisely so it does not shadow that enum.
//!
//! ## Two impls behind one port: self-reported (AC6) + engine-observed (story 3-4)
//!
//! The FIRST impl is [`SelfReportedUsageSource`]: it parses the `KTESIO_USAGE
//! {json}` stdout-sentinel-line convention out of the per-instance agent-output
//! log the engine already captures (AD-12), turning each well-formed line into a
//! [`ParsedUsage`]. A malformed line is a diagnostic (skipped, never a crash,
//! never on `kt` stdout).
//!
//! The SECOND impl is [`ObservedUsageSource`] (story 3-4): the `engine-observed`
//! source, fed by the loopback forward listener (`crate::metering`). It differs
//! from self-reported in its DRIVE MODEL — self-reported is a log-tail DRAIN
//! cadence, engine-observed is EVENT-DRIVEN: the listener parses the OpenAI
//! `usage` object out of each completion response and pushes the counts to a queue
//! the supervisor's reaper drains, and [`ObservedUsageSource`] MINTS the per-Run
//! `sequence` for each (the agent supplies none — it does not know it is being
//! observed). So the observed source does NOT implement the log-tail
//! [`UsageSource::drain`]; it is a thin engine-side minter that yields the SAME
//! [`ParsedUsage`] the self-reported channel yields (the crux: downstream of the
//! choke point the two are byte-identical except the `engine-observed` tag).
//!
//! Either way the engine-stamped fields (Run id, instance, metering source,
//! timestamp) are filled by the commit choke point, NOT here — a source yields
//! only the per-event token fields + the dedup ordinal, so it never writes the
//! ledger (keeping the AD-7 single-writer invariant).
//!
//! ## Purity (cross-OS, no cfg)
//!
//! The parser is pure `std` with NO OS-conditional code — the sentinel line is
//! the same text on every OS, so a usage test never needs an OS gate (the OS-cfg
//! CI gate would reject one here anyway).

use crate::domain::UsageEvent;

/// The stdout sentinel-line prefix a self-reporting agent emits to convey one
/// usage measurement to the engine (spine AD-12 channel, AC6).
///
/// The recorded convention: a line `KTESIO_USAGE {json}` on the agent's stdout
/// (captured into the per-instance agent-output log). The JSON object carries the
/// AGENT-supplied fields — `sequence`, `input_tokens`, `output_tokens` (snake_case
/// per AD-14). Both the `fake_agent` emitter and any real adapter MUST agree on
/// this token. A line NOT starting with it is ordinary agent output, ignored.
pub const USAGE_SENTINEL_PREFIX: &str = "KTESIO_USAGE ";

/// The AGENT-supplied half of a usage event, parsed from one sentinel line
/// (spine AD-7). The engine-stamped fields (instance, Run id, metering source,
/// timestamp) are added by the commit choke point when it constructs the full
/// [`UsageEvent`] — this carries ONLY what the agent reports, so the port never
/// mints a Run id or writes the ledger.
///
/// `cached_tokens` (story 14-6, D8) is the cached SUBSET of `input_tokens`
/// under the ledger's INPUT-INCLUSIVE invariant (`0 <= cached <= input` — see
/// [`crate::metering::parse`]'s module docs): the agent's reported
/// `input_tokens` INCLUDES the cached portion. A sentinel line without the
/// optional `cached_tokens` field parses as known-zero (the agent claimed no
/// cached tokens — an honest conservative reading, never a fabricated count).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedUsage {
    /// The per-Run-monotonic replay-dedup ordinal the agent stamped (AC-A key).
    pub sequence: u64,
    /// Input (prompt) tokens the agent reported — INCLUSIVE of the cached
    /// subset (`0 <= cached_tokens <= input_tokens`, the 14-6 invariant).
    pub input_tokens: u64,
    /// Output (completion) tokens the agent reported.
    pub output_tokens: u64,
    /// The cached subset of `input_tokens` (story 14-6). A line without the
    /// optional field reports known-zero.
    pub cached_tokens: u64,
}

/// The wire shape of the `{json}` in a `KTESIO_USAGE {json}` sentinel line.
///
/// A private serde mirror of [`ParsedUsage`] used only to deserialize the line
/// body. `deny_unknown_fields` keeps a typo'd field an honest parse failure (the
/// line is skipped) rather than a silently-dropped value. Field names are
/// snake_case (AD-14), matching the `usage_events` column names.
/// `cached_tokens` is OPTIONAL (story 14-6): an absence parses as known-zero so
/// every pre-14-6 emitter's line still parses byte-identically.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageLine {
    sequence: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: Option<u64>,
}

/// Parse ONE captured agent-output line into a [`ParsedUsage`], or `None` if it
/// is not a (well-formed) usage sentinel line (spine AD-12/AC6).
///
/// Returns:
/// * `Some(ParsedUsage)` — the line began with [`USAGE_SENTINEL_PREFIX`] and its
///   JSON body parsed into the expected snake_case fields.
/// * `None` — either the line is ordinary agent output (no sentinel prefix) OR the
///   sentinel body was MALFORMED (bad JSON, missing/extra field, wrong type). A
///   malformed usage line is a diagnostic, never a crash and never fatal — the
///   caller logs+skips it (AD-12). The two `None` cases are intentionally merged:
///   the caller treats "not a usage line" and "a broken usage line" the same way
///   (ignore it), so a garbled agent stream can never poison ingestion.
///
/// Pure — no I/O, no OS cfg. The leading/trailing whitespace of the whole line is
/// tolerated (a captured line may carry a trailing `\r` on some platforms).
///
/// The optional `cached_tokens` field (story 14-6) is the cached SUBSET of
/// `input_tokens` under the INPUT-INCLUSIVE invariant — a line whose cached
/// count EXCEEDS its input count is malformed (nonsense under the invariant)
/// and is skipped, exactly like a bad type.
pub fn parse_usage_line(line: &str) -> Option<ParsedUsage> {
    let line = line.trim();
    let body = line.strip_prefix(USAGE_SENTINEL_PREFIX)?;
    let parsed: UsageLine = serde_json::from_str(body.trim()).ok()?;
    // Absent optional field → known-zero (the agent claimed no cached tokens).
    let cached_tokens = parsed.cached_tokens.unwrap_or(0);
    // THE INPUT-INCLUSIVE INVARIANT (14-6): input_tokens includes the cached
    // subset, so cached > input is a malformed report — skip the line, never a
    // clamped or fabricated shape.
    if cached_tokens > parsed.input_tokens {
        return None;
    }
    Some(ParsedUsage {
        sequence: parsed.sequence,
        input_tokens: parsed.input_tokens,
        output_tokens: parsed.output_tokens,
        cached_tokens,
    })
}

/// Scan a block of captured agent output (possibly many lines) for usage sentinel
/// lines, returning every well-formed [`ParsedUsage`] in order (spine AD-7/AC6).
///
/// Non-usage lines and malformed usage lines are skipped (never an error). This
/// is the batch form the supervisor's ingestion drain uses over the newly-read
/// tail of the agent-output log. Pure — no I/O.
pub fn parse_usage_block(block: &str) -> Vec<ParsedUsage> {
    block.lines().filter_map(parse_usage_line).collect()
}

/// The ingestion port (spine AD-1 side port; AD-7 ingest seam) — YIELDS the
/// per-event usage fields for a running instance from a block of CAPTURED AGENT
/// OUTPUT (the self-reported log-tail drive model).
///
/// The impl behind this method is [`SelfReportedUsageSource`] (self-reported over
/// the AD-12 stdout capture). The SECOND source, [`ObservedUsageSource`] (story
/// 3-4, `engine-observed`), does NOT ride this method — it is EVENT-DRIVEN from
/// the loopback listener (there is no captured-output text to drain; the counts
/// arrive already parsed from the model traffic), so it mints its [`ParsedUsage`]
/// directly (see [`ObservedUsageSource::mint`]). Both yield the SAME
/// [`ParsedUsage`] into the SAME commit choke point; the port NEVER writes the
/// ledger and never mints a Run id — the commit choke point stamps the rest
/// (instance, Run id, metering source, timestamp) and records it (the AD-7
/// single-writer invariant).
pub trait UsageSource {
    /// Extract every well-formed usage measurement from a block of captured agent
    /// output (the newly-read tail of the per-instance agent-output log). Malformed
    /// lines are skipped (a diagnostic, never fatal — AD-12). Ordered as emitted.
    fn drain(&self, captured_output: &str) -> Vec<ParsedUsage>;
}

/// The v1 self-reported ingestion source (spine AD-7/AD-12, AC5/AC6): it reads the
/// `KTESIO_USAGE {json}` sentinel lines out of the agent's captured stdout.
///
/// Stateless — it just parses. The caller (the supervisor's ingestion drain) owns
/// the per-instance read cursor over the agent-output log and hands each new block
/// here. This is the sole [`UsageSource`] this story ships; it is what the
/// `fake_agent` emitter (and any real self-reporting adapter) feeds.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelfReportedUsageSource;

impl SelfReportedUsageSource {
    /// Construct the self-reported ingestion source.
    pub fn new() -> Self {
        Self
    }
}

impl UsageSource for SelfReportedUsageSource {
    fn drain(&self, captured_output: &str) -> Vec<ParsedUsage> {
        parse_usage_block(captured_output)
    }
}

/// The `engine-observed` ingestion source (spine AD-7 v1 / FR-19 engine-observed
/// half, story 3-4) — the SECOND impl beside [`SelfReportedUsageSource`].
///
/// Where self-reported parses agent-emitted stdout lines, engine-observed derives
/// usage from the agent's MODEL TRAFFIC through the loopback forward listener
/// (`crate::metering`), which parses the OpenAI-compatible `usage` object out of
/// each completion response. It is EVENT-DRIVEN, not a log-tail drainer, so it
/// does NOT implement [`UsageSource::drain`] — instead the listener pushes each
/// parsed `(input, output)` pair to a queue the supervisor's reaper drains, and
/// this source [`mint`](Self::mint)s the per-Run [`ParsedUsage`].
///
/// ## The engine-minted `sequence` (the crux difference from self-reported)
///
/// A self-reporting agent stamps its own per-Run `sequence` (the replay-dedup
/// ordinal). An OBSERVED completion has no agent-stamped ordinal — the agent does
/// not know it is being observed — so the ENGINE mints it: a per-Run MONOTONIC
/// counter, incremented once per observed completion, so the `UNIQUE(instance_id,
/// run_id, sequence)` dedup index (story 3-1) still holds. The counter lives on
/// this source, which is held per-instance for the Run; a fresh Run gets a fresh
/// source (counter reset to 0), mirroring how the self-reported cursor + breach
/// latch reset per Run.
///
/// RETRY-DEDUP POSTURE (recorded limit): each forwarded call is a DISTINCT
/// observed event and gets its own ordinal, so the counter guarantees uniqueness
/// within a Run (no double-count from the ENGINE side). Unlike the self-reported
/// channel — where the agent RE-STAMPS the same `sequence` on a re-delivered batch
/// so the DB recognizes the replay — a provider-side retry the agent makes is a
/// SEPARATE HTTP call through the proxy, so it is observed as a separate event
/// (the proxy cannot know two calls are semantically the same request). This is
/// the accepted v1 limit: the counter prevents ENGINE double-counting; it does not
/// de-duplicate an agent's own provider retries (those are genuinely distinct
/// calls the agent chose to make, and each consumed real tokens).
#[derive(Debug)]
pub struct ObservedUsageSource {
    /// The per-Run monotonic `sequence` counter (the engine-minted replay-dedup
    /// ordinal). Starts at 0; a fresh Run builds a fresh source, so it resets.
    /// `AtomicU64` so a single `&self` mint is cheap + the source needs no `&mut`.
    next_sequence: std::sync::atomic::AtomicU64,
}

impl ObservedUsageSource {
    /// Construct a fresh observed source with the `sequence` counter at 0 (built at
    /// each `starting` transition for an `engine-observed` instance, so the ordinal
    /// resets per Run — the AD-7 Run boundary).
    pub fn new() -> Self {
        Self {
            next_sequence: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Mint the next per-Run [`ParsedUsage`] for one observed completion (story
    /// 3-4): stamp the next monotonic `sequence` onto the parsed token counts. The
    /// engine-stamped fields (instance, Run id, metering source, timestamp) are
    /// still filled by the commit choke point via [`assemble_usage_event`] — this
    /// yields exactly the same AGENT-half shape the self-reported parser yields, so
    /// the two sources are indistinguishable downstream of the choke point (except
    /// the `engine-observed` tag the commit stamps). `input`/`output` map from the
    /// OpenAI `prompt_tokens`/`completion_tokens` the listener parsed; `cached`
    /// (story 14-6) is the cached SUBSET of `input` under the INPUT-INCLUSIVE
    /// invariant (the parse layer enforces `cached <= input`).
    pub fn mint(&self, input: u64, output: u64, cached: u64) -> ParsedUsage {
        let sequence = self
            .next_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ParsedUsage {
            sequence,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        }
    }
}

impl Default for ObservedUsageSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the AGENT-supplied half of a [`UsageEvent`] convenience helper for
/// tests / the fake agent: format a well-formed `KTESIO_USAGE {json}` sentinel
/// line for the given fields. Kept beside the parser so the emitter and the parser
/// never drift on the token / field names. Pure.
///
/// This is the ONE canonical formatter; the `fake_agent` emitter re-implements the
/// same shape in pure `std` (it cannot depend on the engine), and the parser round-
/// trips it — a unit test asserts they agree.
///
/// The optional `cached_tokens` field (story 14-6) is emitted ONLY when
/// non-zero, so a no-cache event's line stays BYTE-IDENTICAL to the pre-14-6
/// convention (an older line consumer never sees a field it does not know);
/// the parser reads an absent field as known-zero, so the omission is lossless.
pub fn format_usage_line(usage: &ParsedUsage) -> String {
    if usage.cached_tokens > 0 {
        format!(
            "{USAGE_SENTINEL_PREFIX}{{\"sequence\":{},\"input_tokens\":{},\"output_tokens\":{},\
             \"cached_tokens\":{}}}",
            usage.sequence, usage.input_tokens, usage.output_tokens, usage.cached_tokens
        )
    } else {
        format!(
            "{USAGE_SENTINEL_PREFIX}{{\"sequence\":{},\"input_tokens\":{},\"output_tokens\":{}}}",
            usage.sequence, usage.input_tokens, usage.output_tokens
        )
    }
}

/// Assemble a full [`UsageEvent`] from the agent-supplied [`ParsedUsage`] plus the
/// engine-stamped fields (spine AD-7). The commit choke point calls this — it is
/// the ONE place the two halves are joined, so the AD-7 minimum shape is
/// constructed identically everywhere. Pure (no I/O).
///
/// The parsed cached count (story 14-6) is stamped as a KNOWN value (`Some`) —
/// every freshly-assembled event knows its cached subset (known-zero allowed);
/// `None` on the event type is reserved for a PRE-v7 ledger row's unknown
/// cached count (the additive v7 column is NULL there).
pub fn assemble_usage_event(
    parsed: &ParsedUsage,
    instance: &str,
    run_id: crate::domain::RunId,
    metering_source: &str,
    occurred_at: String,
) -> UsageEvent {
    UsageEvent {
        instance: instance.to_string(),
        run_id,
        input_tokens: parsed.input_tokens,
        output_tokens: parsed.output_tokens,
        cached_tokens: Some(parsed.cached_tokens),
        metering_source: metering_source.to_string(),
        sequence: parsed.sequence,
        occurred_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_sentinel_line() {
        // AC6: a well-formed `KTESIO_USAGE {json}` line yields the agent-supplied
        // fields (snake_case).
        let parsed = parse_usage_line(
            "KTESIO_USAGE {\"sequence\":2,\"input_tokens\":11,\"output_tokens\":22}",
        )
        .expect("a well-formed line parses");
        assert_eq!(
            parsed,
            ParsedUsage {
                sequence: 2,
                input_tokens: 11,
                output_tokens: 22,
                cached_tokens: 0,
            }
        );
    }

    #[test]
    fn ignores_a_non_usage_line() {
        // An ordinary agent output line (no sentinel prefix) is not a usage line.
        assert_eq!(parse_usage_line("fake_agent ready pid=1234"), None);
        assert_eq!(parse_usage_line("heartbeat 3"), None);
        assert_eq!(parse_usage_line(""), None);
    }

    #[test]
    fn skips_a_malformed_sentinel_line_without_panicking() {
        // AC6: a bad usage line is a diagnostic, never a crash. Each of these has
        // the prefix but a broken body → None (skipped), no panic.
        let bad = [
            "KTESIO_USAGE not json at all",
            "KTESIO_USAGE {\"sequence\":2}", // missing token fields
            "KTESIO_USAGE {\"sequence\":-1,\"input_tokens\":1,\"output_tokens\":1}", // negative (not u64)
            "KTESIO_USAGE {\"sequence\":1,\"input_tokens\":1,\"output_tokens\":1,\"extra\":9}", // unknown field
            "KTESIO_USAGE {\"sequence\":\"x\",\"input_tokens\":1,\"output_tokens\":1}", // wrong type
            "KTESIO_USAGE ", // empty body
            // 14-6: a cached count ABOVE the input total violates the
            // INPUT-INCLUSIVE invariant (input includes cached) — malformed.
            "KTESIO_USAGE {\"sequence\":1,\"input_tokens\":5,\"output_tokens\":1,\"cached_tokens\":6}",
        ];
        for line in bad {
            assert_eq!(parse_usage_line(line), None, "must skip: {line}");
        }
    }

    #[test]
    fn tolerates_trailing_whitespace_and_carriage_return() {
        // A captured line may carry a trailing \r (platform capture); it still
        // parses. Pure text handling, no OS cfg.
        let parsed = parse_usage_line(
            "KTESIO_USAGE {\"sequence\":0,\"input_tokens\":1,\"output_tokens\":2}\r",
        )
        .expect("trailing CR tolerated");
        assert_eq!(parsed.sequence, 0);
    }

    #[test]
    fn parses_an_optional_cached_tokens_field() {
        // 14-6: the optional `cached_tokens` field parses as the cached SUBSET of
        // input_tokens (the INPUT-INCLUSIVE invariant — the agent's input_tokens
        // already includes it).
        let parsed = parse_usage_line(
            "KTESIO_USAGE {\"sequence\":3,\"input_tokens\":100,\"output_tokens\":20,\"cached_tokens\":80}",
        )
        .expect("a line with cached_tokens parses");
        assert_eq!(parsed.cached_tokens, 80);
        assert_eq!(parsed.input_tokens, 100);
    }

    #[test]
    fn a_pre_14_6_line_without_cached_tokens_is_known_zero() {
        // BACKWARD COMPATIBILITY PIN: every pre-14-6 emitter's line (no
        // cached_tokens field) still parses byte-identically, with the absent
        // optional field read as known-ZERO (the agent claimed no cached tokens).
        let parsed = parse_usage_line(
            "KTESIO_USAGE {\"sequence\":1,\"input_tokens\":10,\"output_tokens\":20}",
        )
        .expect("the legacy line shape still parses");
        assert_eq!(parsed.cached_tokens, 0);
    }

    #[test]
    fn parse_block_returns_only_well_formed_usage_lines_in_order() {
        // The batch form skips non-usage AND malformed lines, keeping the good ones
        // in emission order.
        let block = "fake_agent ready pid=1\n\
             KTESIO_USAGE {\"sequence\":0,\"input_tokens\":1,\"output_tokens\":2}\n\
             heartbeat 0\n\
             KTESIO_USAGE broken\n\
             KTESIO_USAGE {\"sequence\":1,\"input_tokens\":3,\"output_tokens\":4}\n";
        let parsed = parse_usage_block(block);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].sequence, 0);
        assert_eq!(parsed[1].sequence, 1);
        assert_eq!(parsed[1].input_tokens, 3);
    }

    #[test]
    fn self_reported_source_drains_a_captured_block() {
        let source = SelfReportedUsageSource::new();
        let block = "KTESIO_USAGE {\"sequence\":7,\"input_tokens\":5,\"output_tokens\":6}\n";
        let out = source.drain(block);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sequence, 7);
    }

    #[test]
    fn format_and_parse_round_trip_agree_on_the_convention() {
        // The canonical formatter and the parser MUST agree (the fake_agent emitter
        // mirrors format_usage_line in pure std; this guards the shared convention).
        let usage = ParsedUsage {
            sequence: 9,
            input_tokens: 123,
            output_tokens: 456,
            cached_tokens: 0,
        };
        let line = format_usage_line(&usage);
        assert!(line.starts_with(USAGE_SENTINEL_PREFIX));
        assert_eq!(parse_usage_line(&line), Some(usage));
        // A non-zero cached subset rides the line (14-6) and round-trips.
        let cached = ParsedUsage {
            sequence: 10,
            input_tokens: 200,
            output_tokens: 30,
            cached_tokens: 150,
        };
        let cached_line = format_usage_line(&cached);
        assert!(
            cached_line.contains("\"cached_tokens\":150"),
            "{cached_line}"
        );
        assert_eq!(parse_usage_line(&cached_line), Some(cached));
    }

    #[test]
    fn observed_source_mints_mapped_fields_with_a_monotonic_per_run_sequence() {
        // Story 3-4 (Task 3): the observed source stamps the OpenAI-parsed counts
        // (prompt→input, completion→output) onto a per-Run monotonic `sequence`
        // starting at 0, incrementing once per observed completion.
        let source = ObservedUsageSource::new();
        let first = source.mint(128, 512, 100);
        assert_eq!(
            first,
            ParsedUsage {
                sequence: 0,
                input_tokens: 128,
                output_tokens: 512,
                cached_tokens: 100,
            }
        );
        let second = source.mint(7, 3, 0);
        assert_eq!(second.sequence, 1, "sequence increments per observed event");
        assert_eq!(second.input_tokens, 7);
        assert_eq!(second.output_tokens, 3);
        assert_eq!(second.cached_tokens, 0, "a cache miss mints known-zero");
        let third = source.mint(0, 0, 0);
        assert_eq!(third.sequence, 2);
    }

    #[test]
    fn observed_sequences_are_unique_within_a_run_and_reset_per_run() {
        // The engine-minted ordinal must be UNIQUE within a Run (the dedup index),
        // and a FRESH source (a new Run) resets to 0 — mirroring the per-Run cursor.
        let run1 = ObservedUsageSource::new();
        let seqs: Vec<u64> = (0..100).map(|_| run1.mint(1, 1, 0).sequence).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            seqs.len(),
            "all sequences unique within a Run"
        );
        assert_eq!(seqs[0], 0);
        assert_eq!(seqs[99], 99);
        // A new Run's source starts fresh at 0 (the per-Run reset).
        let run2 = ObservedUsageSource::new();
        assert_eq!(
            run2.mint(5, 5, 0).sequence,
            0,
            "a fresh Run resets the ordinal"
        );
    }

    #[test]
    fn assemble_joins_agent_and_engine_fields_into_the_ad7_shape() {
        // The commit choke point joins the agent-supplied ParsedUsage with the
        // engine-stamped fields (Run id, instance, source, timestamp) into the AD-7
        // minimum shape.
        let parsed = ParsedUsage {
            sequence: 4,
            input_tokens: 8,
            output_tokens: 16,
            cached_tokens: 5,
        };
        let event = assemble_usage_event(
            &parsed,
            "svc",
            crate::domain::RunId::from_wire("run-xyz"),
            "self-reported",
            "2026-07-06T00:00:00Z".to_string(),
        );
        assert_eq!(event.instance, "svc");
        assert_eq!(event.run_id.as_str(), "run-xyz");
        assert_eq!(event.input_tokens, 8);
        assert_eq!(event.output_tokens, 16);
        assert_eq!(
            event.cached_tokens,
            Some(5),
            "a fresh event's cached is KNOWN"
        );
        assert_eq!(event.metering_source, "self-reported");
        assert_eq!(event.sequence, 4);
        assert_eq!(event.occurred_at, "2026-07-06T00:00:00Z");
    }
}
