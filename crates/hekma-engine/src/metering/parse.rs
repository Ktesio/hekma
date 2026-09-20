//! The OpenAI-compatible `usage` PARSE (spine AD-7 `engine-observed` half),
//! story 3-4 — pure, cross-OS by construction (NO OS cfg).
//!
//! The engine-observed listener ([`super::listener`]) forwards the agent's model
//! traffic to the real upstream and, on a completion response, skims the standard
//! OpenAI-compatible `usage` object out of the body:
//!
//! ```json
//! {"choices": [ ... ], "usage": {"prompt_tokens": 128, "completion_tokens": 512}}
//! ```
//!
//! The two fields map onto the SAME [`ParsedUsage`](crate::ports::ParsedUsage)
//! shape the self-reported channel yields (AD-14 snake_case columns):
//! `prompt_tokens → input_tokens`, `completion_tokens → output_tokens`. The
//! ENGINE mints the per-Run `sequence` (the observed agent supplies none — see
//! [`super::ObservedListener`]), so this function returns ONLY the token counts.
//!
//! ## Robustness (best-effort to the RUN — mirrors 3-1's malformed-line skip)
//!
//! A body with NO `usage` object, a malformed/partial JSON body, or a `usage`
//! object missing a field yields `None` — the observation is SKIPPED (a
//! diagnostic, never a panic, never on `kt` stdout). Observation is best-effort:
//! the agent's call still succeeds (the listener relays the response faithfully
//! regardless), so a parse miss loses one measurement, never the call.
//!
//! ## Streaming (story 12-2 — the named provider-parse seam)
//!
//! An OpenAI STREAMING response (`"stream": true`) relays usage ONLY in a
//! terminal Server-Sent-Events `data:` frame, and only when the request carried
//! `stream_options.include_usage` — which the listener injects on forwarded
//! streaming requests (see [`super::listener`]). [`parse_openai_sse_usage`]
//! parses that SSE shape: an O(1)-memory line scan over the buffered body that
//! keeps the LAST `data:` frame bearing a usable `usage` object (`"usage": null`
//! intermediate frames are the documented normal). This function IS the
//! provider-parse seam the epic ratified: it hard-codes the OpenAI SSE shape
//! (v1), and a non-OpenAI provider usage schema is a future extension that
//! swaps IN here — behind this named boundary, not by editing the listener.
//! Bounded by AD-7's `[ASSUMPTION: OpenAI-compatible usage JSON covers v1
//! targets]`.
//!
//! ## Cached tokens (story 14-6, D8) — THE INPUT-INCLUSIVE INVARIANT
//!
//! Every parse in this module yields a THIRD count, the cached-token figure,
//! alongside `(input, output)`. The ledger invariant it feeds (pinned here and
//! in the store tests) is **INPUT-INCLUSIVE (the OpenAI convention)**:
//!
//! > `input_tokens` stored in the ledger INCLUDES the cached tokens —
//! > `0 <= cached_tokens <= input_tokens` — and `cached_tokens` is the
//! > SUBSET of the input that was served from the provider's prompt cache.
//!
//! Consequences (all enforced by tests):
//!
//! * The total billed token count stays `input + output` — cached tokens are
//!   real billed tokens that ride INSIDE `input` (they are never added a
//!   second time), so token budgets enforce on the same totals as before.
//! * Cost prices the NON-cached remainder of the input at the input rate and
//!   the cached subset at the (cheaper) cached rate — see
//!   [`crate::domain::cost`]. The inclusive convention makes that a pure
//!   re-split of the input direction; no total changes.
//! * The parse ENFORCES the invariant: a provider report whose cached count
//!   EXCEEDS its input count is nonsense under the convention → the whole
//!   parse is `None` (skipped), the same robustness rule as a negative count.
//!
//! This convention was chosen over the EXCLUSIVE one (Anthropic's native
//! `input_tokens`, which excludes cache reads) because the self-reported
//! sentinel contract (`KTESIO_USAGE`) already carries prompt-side inclusive
//! counts from every existing emitter — an exclusive convention would silently
//! re-define what every existing agent reports. Emissions stay byte-compatible;
//! only the optional cached subset is added.
//!
//! ## The provider shapes (the 12-2 named seam)
//!
//! * **OpenAI** (primary): `usage.prompt_tokens` / `usage.completion_tokens`,
//!   with the cached figure at `usage.prompt_tokens_details.cached_tokens`
//!   (`prompt_tokens` includes the cached subset — the convention's namesake).
//! * **Anthropic** (14-6): `usage.input_tokens` / `usage.output_tokens` plus
//!   `cache_creation_input_tokens` and `cache_read_input_tokens`. Anthropic's
//!   native `input_tokens` EXCLUDES both cache counters, so the parse
//!   NORMALIZES to the inclusive invariant: `input = input_tokens +
//!   cache_creation_input_tokens + cache_read_input_tokens`, `cached =
//!   cache_read_input_tokens` (cache READS are the cached subset; cache
//!   CREATION is billed at write-input rates and therefore counts as plain
//!   input — the documented 1.25×-write premium is NOT modeled, a known
//!   approximation, not a silent one).
//! * **Gemini** (`usageMetadata.cachedContentTokenCount`): NO Gemini usage
//!   shape reaches this seam today (the loopback listener forwards
//!   OpenAI-compatible traffic only, and no in-tree adapter emits it) — the
//!   shape is deferred until a source actually emits it; this seam is where it
//!   swaps in.
//!
//! ## Absence semantics (honest lower bound, AD-8)
//!
//! A KNOWN cached field present → its value (`0` is valid); a KNOWN field
//! ABSENT → known-ZERO (the provider did not claim any cached tokens); an
//! UNKNOWN field stays uninterpreted (never guessed into a count). A cached
//! field present but malformed (negative / float / string) → the whole parse
//! is `None` (skip), matching the token-count robustness rule.

/// The parsed OpenAI-compatible `usage` counts skimmed from a completion response
/// body: `(input_tokens, output_tokens, cached_tokens)`, mapped from
/// `prompt_tokens` / `completion_tokens` / the cached shape (see the module docs
/// for the INPUT-INCLUSIVE invariant and the per-provider shapes).
///
/// Returns `None` when the body is not a JSON object with a well-formed `usage`
/// object carrying both integer token fields (a missing/partial/malformed body, or a
/// streaming SSE body) — the caller SKIPS it (best-effort, never a panic). All
/// three counts are read as non-negative `u64` (a negative or non-integer field → the
/// whole parse is `None`, since a negative token count is nonsense), and the
/// inclusive invariant `cached <= input` is enforced (a larger cached count is
/// nonsense under the convention → `None`).
///
/// PURE — no I/O, no OS cfg. Tolerates extra fields (the real response carries
/// `choices`, `model`, `id`, and often `usage` sub-fields like
/// `total_tokens` we ignore): only the three counts are read, so a provider
/// adding fields never breaks the parse.
pub fn parse_openai_usage(body: &[u8]) -> Option<(u64, u64, u64)> {
    // Parse leniently into a serde_json::Value: the real response is a large object
    // with many fields we do not model, so a typed struct with deny_unknown_fields
    // would reject legitimate responses. We read exactly the three counts we need.
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage = value.get("usage")?;
    // The 12-2 named seam dispatches on the usage object's SHAPE (14-6): the
    // OpenAI shape (a `prompt_tokens` field) is primary; the Anthropic shape
    // (`input_tokens` + the cache counters) is the documented second shape. A
    // usage object carrying BOTH (a translating gateway) reads as OpenAI — the
    // primary shape wins. Anything else is `None` (skipped), as before.
    if usage.get("prompt_tokens").is_some() {
        read_openai_shape(usage)
    } else {
        read_anthropic_shape(usage)
    }
}

/// Read the OPENAI shape's three counts from a `usage` object:
/// `prompt_tokens` → input, `completion_tokens` → output, and the cached
/// subset from `prompt_tokens_details.cached_tokens` (absent → known-zero).
/// Enforces the inclusive invariant (`cached <= input`) — a violation is a
/// nonsense report → `None` (skip).
fn read_openai_shape(usage: &serde_json::Value) -> Option<(u64, u64, u64)> {
    let input = usage_field(usage, "prompt_tokens")?;
    let output = usage_field(usage, "completion_tokens")?;
    let cached = openai_cached_tokens(usage)?;
    if cached > input {
        // The INPUT-INCLUSIVE INVARIANT (14-6): prompt_tokens includes the cached
        // subset, so a cached count above the prompt total is a malformed report
        // — the measurement is skipped, never clamped into a fabricated shape.
        return None;
    }
    Some((input, output, cached))
}

/// Read the ANTHROPIC shape's counts from a `usage` object and NORMALIZE them
/// onto the INPUT-INCLUSIVE invariant (see the module docs): Anthropic's
/// `input_tokens` EXCLUDES both cache counters, so
/// `input = input_tokens + cache_creation_input_tokens + cache_read_input_tokens`
/// and `cached = cache_read_input_tokens` (cache READS are the cached subset;
/// cache CREATION bills at write-input rates → plain input). Cache-creation
/// counts at the plain input rate models the documented 1.25× write premium as
/// its base rate — a KNOWN approximation, surfaced here, never a silent one.
/// The inclusive invariant holds BY CONSTRUCTION (the normalized input
/// contains the read count).
fn read_anthropic_shape(usage: &serde_json::Value) -> Option<(u64, u64, u64)> {
    let base = usage_field(usage, "input_tokens")?;
    let output = usage_field(usage, "output_tokens")?;
    let creation = optional_usage_field(usage, "cache_creation_input_tokens")?;
    let read = optional_usage_field(usage, "cache_read_input_tokens")?;
    let input = base.saturating_add(creation).saturating_add(read);
    Some((input, output, read))
}

/// Read the OpenAI cached subset: `usage.prompt_tokens_details.cached_tokens`.
///
/// ABSENCE semantics (the honest lower bound): the `prompt_tokens_details`
/// object absent, or `cached_tokens` absent inside it → known-ZERO (the
/// provider claimed no cached tokens). A PRESENT but malformed value
/// (negative / float / string) → `None` (the whole parse skips — a token count
/// is a non-negative whole number, the same rule as the two primary counts).
fn openai_cached_tokens(usage: &serde_json::Value) -> Option<u64> {
    match usage.get("prompt_tokens_details") {
        None => Some(0),
        Some(details) => match details.get("cached_tokens") {
            None => Some(0),
            Some(value) => value.as_u64(),
        },
    }
}

/// Read a single non-negative integer token field from the `usage` object.
///
/// Accepts a JSON integer (the OpenAI form). A field that is absent, a non-integer
/// (float/string/null), or negative yields `None` — which makes the whole
/// [`parse_openai_usage`] return `None` (skip), never a partial or wrapped count.
fn usage_field(usage: &serde_json::Value, key: &str) -> Option<u64> {
    let field = usage.get(key)?;
    // as_u64 accepts a non-negative JSON integer; a float / negative / string is
    // None (a token count is a non-negative whole number — anything else is
    // malformed and the measurement is skipped).
    field.as_u64()
}

/// Read an OPTIONAL non-negative integer cache-counter field (the Anthropic
/// shape's `cache_*_input_tokens`). ABSENT → known-ZERO (the honest lower
/// bound — the provider claimed none); PRESENT but malformed (negative /
/// float / string) → `None` (the whole parse skips).
fn optional_usage_field(usage: &serde_json::Value, key: &str) -> Option<u64> {
    match usage.get(key) {
        None => Some(0),
        Some(field) => field.as_u64(),
    }
}

/// Parse the TERMINAL OpenAI SSE `usage` frame out of a buffered STREAMING
/// response body (story 12-2) — the named provider-parse seam (see the module
/// docs). Returns the same `(input_tokens, output_tokens, cached_tokens)` triple
/// as [`parse_openai_usage`], with the cached subset read from the terminal
/// frame's `prompt_tokens_details.cached_tokens` (absent → known-zero; the
/// inclusive invariant enforced — a frame whose cached count exceeds its prompt
/// count is discarded like any malformed frame). The SSE seam stays OPENAI-SHAPED
/// (v1): the Anthropic shape rides the non-streaming body parse only, since no
/// Anthropic-native SSE frame reaches this listener.
///
/// The OpenAI streaming contract: the response is a Server-Sent-Events stream of
/// `data: {json}` frames; every frame carries `"usage": null` EXCEPT the terminal
/// one (emitted only when the request set `stream_options.include_usage`), whose
/// `usage` object holds the full token totals. The scanner walks the buffered
/// body LINE BY LINE (no `Vec` of lines, no state machine — O(1) auxiliary
/// memory over the body size, the ratified "small machine" shape), lazily parses
/// each `data:` frame's JSON, and keeps the LAST frame bearing a usable `usage`
/// object. A final `data: [DONE]` sentinel frame is not JSON and parses to
/// `None` — skipped like any non-JSON line.
///
/// Returns `None` when NO `data:` frame carries a usable `usage` object (the
/// agent never asked for usage, the stream was an error body, the body was
/// malformed, or `usage` was missing a field) — the caller SKIPS it silently
/// (best-effort observation; the counts stay lower-bound-honest). Both counts
/// are read as non-negative `u64` (a negative or non-integer field discards THAT
/// frame; an earlier usable frame still wins). PURE — no I/O, no OS cfg.
///
/// **Known wire-legal shape this scanner does NOT support (named on purpose,
/// review round 2):** a MULTI-LINE SSE event — one logical event whose `data:`
/// payload is split across several consecutive `data:` lines, which SSE joins
/// with `\n` into a single payload — is silently dropped. This scanner treats
/// each line as a self-contained frame, so a multi-line event's fragments do
/// not parse as standalone JSON (`None` per line). No known OpenAI-compatible
/// provider splits the usage frame that way (the usage object is emitted as
/// one single-line frame), so the accepted cost is a skipped measurement — a
/// lower-bound-honest miss, never a wrong count — not a relay failure. If a
/// provider ever does, the seam is here: join consecutive `data:` lines
/// before parsing, per the SSE aggregation rule.
pub(crate) fn parse_openai_sse_usage(body: &[u8]) -> Option<(u64, u64, u64)> {
    // The LAST usable `data:` frame wins (later frames supersede earlier ones).
    let mut last: Option<(u64, u64, u64)> = None;
    for raw_line in body.split(|b| *b == b'\n') {
        // SSE allows CRLF terminators; hyper's buffered body preserves whatever
        // the upstream sent, so drop one trailing `\r` first.
        let line = strip_cr(raw_line);
        // Only `data:` frames can carry usage. Comments (`:`), `event:`/`id:`
        // lines, and the blank frame separator are not usage carriers — skip.
        let Some(rest) = line.strip_prefix(b"data:".as_slice()) else {
            continue;
        };
        // The `data: <payload>` spelling (one leading space) is the SSE
        // convention; tolerate the no-space spelling too.
        let payload = rest.strip_prefix(b" ".as_slice()).unwrap_or(rest);
        ingest_data_payload(payload, &mut last);
    }
    last
}

/// Drop one trailing carriage return (CRLF line terminators).
fn strip_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _ => line,
    }
}

/// Lazily parse ONE `data:` payload as JSON and, if it carries a usable
/// `usage` object, record it as the current last-known usage. A malformed or
/// non-usage payload (`[DONE]`, partial JSON, an error object) leaves `last`
/// untouched — never a panic, never a partial count. The cached subset rides
/// the OpenAI shape (`prompt_tokens_details.cached_tokens`, absent →
/// known-zero; a cached count above the prompt total discards the frame — the
/// INPUT-INCLUSIVE invariant, 14-6).
fn ingest_data_payload(payload: &[u8], last: &mut Option<(u64, u64, u64)>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return;
    };
    // `usage: null` is the documented NORMAL for every non-terminal frame —
    // `get` returns Some(Value::Null), `usage_field` then rejects it (as_u64 on
    // null is None), and the frame contributes nothing.
    let Some(usage) = value.get("usage") else {
        return;
    };
    let Some((input, output, cached)) = read_openai_shape(usage) else {
        return;
    };
    *last = Some((input, output, cached));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_completion_body() {
        // AC-A / AC5: a non-streaming completion response with a standard `usage`
        // object yields the mapped (input, output) counts.
        let body = br#"{
            "id": "chatcmpl-abc",
            "object": "chat.completion",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}],
            "usage": {"prompt_tokens": 128, "completion_tokens": 512, "total_tokens": 640}
        }"#;
        assert_eq!(parse_openai_usage(body), Some((128, 512, 0)));
    }

    #[test]
    fn maps_prompt_to_input_and_completion_to_output() {
        // The field-name mapping is the crux: prompt_tokens → input_tokens,
        // completion_tokens → output_tokens (NOT swapped).
        let body = br#"{"usage": {"prompt_tokens": 7, "completion_tokens": 3}}"#;
        let (input, output, cached) = parse_openai_usage(body).expect("well-formed");
        assert_eq!(input, 7, "prompt_tokens maps to input");
        assert_eq!(output, 3, "completion_tokens maps to output");
        assert_eq!(cached, 0, "no cached shape reported -> known-zero");
    }

    #[test]
    fn tolerates_extra_and_unknown_fields() {
        // A real response carries many fields (system_fingerprint, usage sub-detail
        // objects, service_tier); the two counts still parse (no deny_unknown_fields).
        let body = br#"{
            "system_fingerprint": "fp_x",
            "service_tier": "default",
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 22,
                "total_tokens": 33,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 4}
            }
        }"#;
        assert_eq!(parse_openai_usage(body), Some((11, 22, 0)));
    }

    #[test]
    fn a_body_with_no_usage_object_is_skipped() {
        // A response WITHOUT `usage` (some endpoints, or an error body) → None
        // (skipped, no panic). Observation is best-effort — the call still succeeded.
        let body = br#"{"id": "x", "choices": [{"index": 0}]}"#;
        assert_eq!(parse_openai_usage(body), None);
    }

    #[test]
    fn a_usage_missing_a_field_is_skipped() {
        // A `usage` object missing one of the two counts → None (never a partial
        // count that would half-bill).
        let only_prompt = br#"{"usage": {"prompt_tokens": 5}}"#;
        assert_eq!(parse_openai_usage(only_prompt), None);
        let only_completion = br#"{"usage": {"completion_tokens": 5}}"#;
        assert_eq!(parse_openai_usage(only_completion), None);
    }

    #[test]
    fn a_malformed_or_partial_body_is_skipped_not_panicked() {
        // A truncated / non-JSON / empty body → None, never a panic (a
        // network-truncated body, or a plain-text error page all land here;
        // the SSE *stream* shape is `parse_openai_sse_usage`'s job since
        // story 12-2, and this JSON-body parse still correctly skips it).
        for bad in [
            &b"not json at all"[..],
            &b"{\"usage\": {\"prompt_tokens\": 1,"[..], // truncated
            &b""[..],                                   // empty
            &b"data: {\"usage\":{}}\n\n"[..],           // an SSE stream chunk
        ] {
            assert_eq!(parse_openai_usage(bad), None, "must skip: {bad:?}");
        }
    }

    #[test]
    fn a_negative_or_non_integer_count_is_skipped() {
        // A negative or float token count is nonsense (a token count is a
        // non-negative whole number) → None, never a wrapped/truncated value.
        let negative = br#"{"usage": {"prompt_tokens": -1, "completion_tokens": 2}}"#;
        assert_eq!(parse_openai_usage(negative), None);
        let float = br#"{"usage": {"prompt_tokens": 1.5, "completion_tokens": 2}}"#;
        assert_eq!(parse_openai_usage(float), None);
        let string = br#"{"usage": {"prompt_tokens": "10", "completion_tokens": 2}}"#;
        assert_eq!(parse_openai_usage(string), None);
    }

    #[test]
    fn zero_counts_are_valid() {
        // A legitimate zero-token event (an empty completion) parses as (0, 0) —
        // distinct from a missing field (None).
        let body = br#"{"usage": {"prompt_tokens": 0, "completion_tokens": 0}}"#;
        assert_eq!(parse_openai_usage(body), Some((0, 0, 0)));
    }

    #[test]
    fn a_large_count_within_u64_parses() {
        // A large but valid count parses (the storage-boundary clamp to i64 is the
        // ledger's job, story 3-1 — here we just read the u64 faithfully).
        let body = br#"{"usage": {"prompt_tokens": 9007199254740993, "completion_tokens": 1}}"#;
        assert_eq!(
            parse_openai_usage(body),
            Some((9_007_199_254_740_993, 1, 0))
        );
    }

    // ---- Story 14-6 (D8): cached tokens — the INPUT-INCLUSIVE invariant ----

    #[test]
    fn openai_cached_tokens_parse_from_prompt_tokens_details() {
        // The OpenAI cached shape: `prompt_tokens_details.cached_tokens` is the
        // cached SUBSET of prompt_tokens (the inclusive invariant — cached <=
        // input), returned as the third count.
        let body = br#"{"usage": {"prompt_tokens": 128, "completion_tokens": 16,
            "prompt_tokens_details": {"cached_tokens": 100}}}"#;
        assert_eq!(parse_openai_usage(body), Some((128, 16, 100)));
    }

    #[test]
    fn openai_cached_absent_is_known_zero_not_unknown() {
        // Absence semantics (honest lower bound): BOTH the details object and the
        // cached_tokens field inside it are KNOWN fields — absent → known-ZERO
        // (the provider claimed no cached tokens), never an uninterpreted miss.
        let no_details = br#"{"usage": {"prompt_tokens": 7, "completion_tokens": 3}}"#;
        assert_eq!(parse_openai_usage(no_details), Some((7, 3, 0)));
        let empty_details =
            br#"{"usage": {"prompt_tokens": 7, "completion_tokens": 3, "prompt_tokens_details": {}}}"#;
        assert_eq!(parse_openai_usage(empty_details), Some((7, 3, 0)));
    }

    #[test]
    fn openai_cached_zero_is_a_valid_present_value() {
        // A present `cached_tokens: 0` is a real zero (a cache miss) — its value
        // is taken verbatim, never reinterpreted.
        let body = br#"{"usage": {"prompt_tokens": 7, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 0}}}"#;
        assert_eq!(parse_openai_usage(body), Some((7, 3, 0)));
    }

    #[test]
    fn openai_cached_above_prompt_violates_the_inclusive_invariant_and_is_skipped() {
        // THE INVARIANT PIN: prompt_tokens INCLUDES the cached subset, so a cached
        // count above the prompt total is a malformed report — the whole parse is
        // None (skipped), never a clamped or wrapped shape.
        let body = br#"{"usage": {"prompt_tokens": 50, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 51}}}"#;
        assert_eq!(parse_openai_usage(body), None);
    }

    #[test]
    fn openai_cached_malformed_value_skips_the_measurement() {
        // A present but non-integer cached value (negative / float / string) is a
        // nonsense token count → the whole parse is None (the same rule as the
        // two primary counts), never a partial count.
        let negative = br#"{"usage": {"prompt_tokens": 7, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": -1}}}"#;
        assert_eq!(parse_openai_usage(negative), None);
        let float = br#"{"usage": {"prompt_tokens": 7, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 1.5}}}"#;
        assert_eq!(parse_openai_usage(float), None);
        let string = br#"{"usage": {"prompt_tokens": 7, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": "5"}}}"#;
        assert_eq!(parse_openai_usage(string), None);
    }

    #[test]
    fn anthropic_shape_normalizes_onto_the_inclusive_invariant() {
        // THE ANTHROPIC MAPPING PIN: Anthropic's native `input_tokens` EXCLUDES
        // both cache counters, so the parse normalizes input to
        // `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`
        // and reports `cache_read_input_tokens` as the cached subset. A 10-token
        // fresh prompt + 4 creation + 6 read = an INCLUSIVE input of 20, cached 6.
        let body = br#"{"usage": {"input_tokens": 10, "output_tokens": 8,
            "cache_creation_input_tokens": 4, "cache_read_input_tokens": 6}}"#;
        assert_eq!(parse_openai_usage(body), Some((20, 8, 6)));
        // The invariant holds BY CONSTRUCTION: cached (6) <= input (20).
    }

    #[test]
    fn anthropic_cache_fields_absent_are_known_zero() {
        // An Anthropic-shaped usage with NO cache counters (a provider that never
        // cached) normalizes to input = input_tokens, cached = 0 (known-zero).
        let body = br#"{"usage": {"input_tokens": 10, "output_tokens": 8}}"#;
        assert_eq!(parse_openai_usage(body), Some((10, 8, 0)));
        // Each counter independently absent → known-zero for that counter.
        let only_read = br#"{"usage": {"input_tokens": 10, "output_tokens": 8,
            "cache_read_input_tokens": 6}}"#;
        assert_eq!(parse_openai_usage(only_read), Some((16, 8, 6)));
        let only_creation = br#"{"usage": {"input_tokens": 10, "output_tokens": 8,
            "cache_creation_input_tokens": 4}}"#;
        assert_eq!(parse_openai_usage(only_creation), Some((14, 8, 0)));
    }

    #[test]
    fn anthropic_malformed_cache_field_skips_the_measurement() {
        // A present-but-malformed Anthropic cache counter → None (never a
        // partial count), matching the OpenAI cached-field rule.
        let bad = br#"{"usage": {"input_tokens": 10, "output_tokens": 8,
            "cache_read_input_tokens": -6}}"#;
        assert_eq!(parse_openai_usage(bad), None);
        let bad2 = br#"{"usage": {"input_tokens": 10, "output_tokens": 8,
            "cache_creation_input_tokens": "4"}}"#;
        assert_eq!(parse_openai_usage(bad2), None);
    }

    #[test]
    fn a_usage_with_neither_shape_is_still_skipped() {
        // The shape dispatch never fabricates counts: a usage object with neither
        // the OpenAI nor the Anthropic required fields is None (skip), as before.
        let body = br#"{"usage": {"total_tokens": 33}}"#;
        assert_eq!(parse_openai_usage(body), None);
    }

    #[test]
    fn an_openai_shape_with_both_field_sets_reads_as_openai() {
        // A translating gateway emitting BOTH shapes reads as the PRIMARY (OpenAI)
        // shape — prompt_tokens wins; the Anthropic fields stay uninterpreted.
        let body = br#"{"usage": {"prompt_tokens": 50, "completion_tokens": 3,
            "input_tokens": 999, "output_tokens": 999,
            "prompt_tokens_details": {"cached_tokens": 20}}}"#;
        assert_eq!(parse_openai_usage(body), Some((50, 3, 20)));
    }

    // ---- Story 12-2: the terminal SSE usage frame (`parse_openai_sse_usage`) ----

    fn sse_frame(json: &str) -> String {
        format!("data: {json}\n\n")
    }

    #[test]
    fn sse_terminal_usage_frame_parses() {
        // The OpenAI streaming contract, exactly: content frames with
        // `"usage": null`, then the terminal frame carrying the totals.
        let mut body = String::new();
        body.push_str(&sse_frame(
            r#"{"id":"1","choices":[{"delta":{"content":"hi"}}],"usage":null}"#,
        ));
        body.push_str(&sse_frame(
            r#"{"id":"1","choices":[{"delta":{"content":"!"}}],"usage":null}"#,
        ));
        body.push_str(&sse_frame(
            r#"{"id":"1","choices":[],"usage":{"prompt_tokens":128,"completion_tokens":512,"total_tokens":640}}"#,
        ));
        body.push_str("data: [DONE]\n\n");
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((128, 512, 0)));
    }

    #[test]
    fn sse_the_last_usage_bearing_frame_wins() {
        // Multi-frame: a scanner contract is "keep the LAST usable frame" — a
        // (non-OpenAI-shaped but tolerated) stream with TWO usage frames must
        // yield the later one, never the earlier or a double count.
        let mut body = String::new();
        body.push_str(&sse_frame(
            r#"{"usage":{"prompt_tokens":1,"completion_tokens":2}}"#,
        ));
        body.push_str(&sse_frame(
            r#"{"usage":{"prompt_tokens":3,"completion_tokens":4}}"#,
        ));
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((3, 4, 0)));
    }

    #[test]
    fn sse_null_and_missing_usage_intermediates_are_ignored() {
        // `"usage": null` frames (the documented normal) and frames with no
        // usage key at all contribute nothing — and do not CLOBBER an earlier
        // usable frame's counts.
        let mut body = String::new();
        body.push_str(&sse_frame(
            r#"{"usage":{"prompt_tokens":7,"completion_tokens":9}}"#,
        ));
        body.push_str(&sse_frame(r#"{"usage":null}"#));
        body.push_str(&sse_frame(r#"{"choices":[]}"#));
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((7, 9, 0)));
    }

    #[test]
    fn sse_no_usable_usage_frame_is_none() {
        // The terminal usage frame only arrives when include_usage was asked
        // for; without it (or on error bodies) there is nothing to count.
        let mut only_null = String::new();
        only_null.push_str(&sse_frame(r#"{"choices":[],"usage":null}"#));
        only_null.push_str("data: [DONE]\n\n");
        assert_eq!(parse_openai_sse_usage(only_null.as_bytes()), None);
        // Non-SSE bytes, an empty body, and a truncated stream all skip.
        for empty in [
            &b""[..],
            &b"not sse at all"[..],
            &b"data: {\"usage\": {\"prompt_to"[..], // truncated frame
            &b": a comment\n\n"[..],
            &b"event: done\ndata: [DONE]\n\n"[..],
        ] {
            assert_eq!(parse_openai_sse_usage(empty), None, "must skip: {empty:?}");
        }
    }

    #[test]
    fn sse_malformed_frames_are_skipped_not_panicked() {
        // A malformed frame in the middle must not poison the scan (later
        // usable frames still count) — and a malformed count inside a frame
        // discards THAT frame, never a partial or wrapped value.
        let mut body = String::new();
        body.push_str("data: {not json\n\n");
        body.push_str(&sse_frame(
            r#"{"usage":{"prompt_tokens":true,"completion_tokens":2}}"#,
        ));
        body.push_str(&sse_frame(
            r#"{"usage":{"prompt_tokens":11,"completion_tokens":22}}"#,
        ));
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((11, 22, 0)));
    }

    #[test]
    fn sse_crlf_terminators_and_no_space_spelling_parse() {
        // SSE allows CRLF line terminators; the `data:<payload>` no-space
        // spelling is tolerated by the scanner (both are wire-legal shapes a
        // lenient proxy must not mis-skip).
        let crlf = "data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\r\n\r\ndata: [DONE]\r\n\r\n";
        assert_eq!(parse_openai_sse_usage(crlf.as_bytes()), Some((5, 6, 0)));
        let no_space = "data:{\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\n\n";
        assert_eq!(parse_openai_sse_usage(no_space.as_bytes()), Some((5, 6, 0)));
    }

    #[test]
    fn sse_zero_counts_are_valid_and_negative_counts_are_skipped() {
        // Zero is a legitimate terminal count (an empty completion) — distinct
        // from None (no frame). A negative count discards its frame (a token
        // count is a non-negative whole number — never a wrapped value).
        let zeros = "data: {\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\n\n";
        assert_eq!(parse_openai_sse_usage(zeros.as_bytes()), Some((0, 0, 0)));
        let negative = "data: {\"usage\":{\"prompt_tokens\":-1,\"completion_tokens\":2}}\n\n";
        assert_eq!(parse_openai_sse_usage(negative.as_bytes()), None);
    }

    #[test]
    fn sse_cached_tokens_parse_from_the_terminal_frame() {
        // 14-6 on the SSE seam: the terminal frame's
        // `prompt_tokens_details.cached_tokens` is the cached subset (inclusive —
        // cached <= prompt); an absent details object is known-zero.
        let mut body = String::new();
        body.push_str(&sse_frame(r#"{"choices":[],"usage":null}"#));
        body.push_str(&sse_frame(
            r#"{"choices":[],"usage":{"prompt_tokens":128,"completion_tokens":16,"prompt_tokens_details":{"cached_tokens":100}}}"#,
        ));
        assert_eq!(
            parse_openai_sse_usage(body.as_bytes()),
            Some((128, 16, 100))
        );
        let absent =
            sse_frame(r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#);
        assert_eq!(parse_openai_sse_usage(absent.as_bytes()), Some((7, 3, 0)));
    }

    #[test]
    fn sse_frame_violating_the_inclusive_invariant_is_discarded() {
        // A terminal frame whose cached count exceeds its prompt count is
        // nonsense under the INPUT-INCLUSIVE invariant — the frame is discarded
        // like any malformed one (an earlier usable frame still wins).
        let mut body = String::new();
        body.push_str(&sse_frame(
            r#"{"usage":{"prompt_tokens":5,"completion_tokens":6}}"#,
        ));
        body.push_str(&sse_frame(
            r#"{"usage":{"prompt_tokens":9,"completion_tokens":1,
               "prompt_tokens_details":{"cached_tokens":10}}}"#,
        ));
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((5, 6, 0)));
    }

    #[test]
    fn sse_stays_openai_shaped_an_anthropic_frame_is_skipped() {
        // The SSE seam stays OPENAI-SHAPED (v1, documented): an Anthropic-shaped
        // usage frame (no prompt_tokens) contributes nothing — never a partial
        // or mis-shaped count.
        let frame = sse_frame(
            r#"{"usage":{"input_tokens":10,"output_tokens":8,"cache_read_input_tokens":6}}"#,
        );
        assert_eq!(parse_openai_sse_usage(frame.as_bytes()), None);
    }
}
