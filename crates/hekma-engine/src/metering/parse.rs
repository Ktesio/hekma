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

/// The parsed OpenAI-compatible `usage` counts skimmed from a completion response
/// body: `(input_tokens, output_tokens)`, mapped from `prompt_tokens` /
/// `completion_tokens`. See the module docs.
///
/// Returns `None` when the body is not a JSON object with a well-formed `usage`
/// object carrying both integer fields (a missing/partial/malformed body, or a
/// streaming SSE body) — the caller SKIPS it (best-effort, never a panic). Both
/// counts are read as non-negative `u64` (a negative or non-integer field → the
/// whole parse is `None`, since a negative token count is nonsense).
///
/// PURE — no I/O, no OS cfg. Tolerates extra fields (the real response carries
/// `choices`, `model`, `id`, and often `usage` sub-fields like
/// `total_tokens`/`prompt_tokens_details` we ignore): only the two counts are
/// read, so a provider adding fields never breaks the parse.
pub fn parse_openai_usage(body: &[u8]) -> Option<(u64, u64)> {
    // Parse leniently into a serde_json::Value: the real response is a large object
    // with many fields we do not model, so a typed struct with deny_unknown_fields
    // would reject legitimate responses. We read exactly the two counts we need.
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage = value.get("usage")?;
    let input = usage_field(usage, "prompt_tokens")?;
    let output = usage_field(usage, "completion_tokens")?;
    Some((input, output))
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

/// Parse the TERMINAL OpenAI SSE `usage` frame out of a buffered STREAMING
/// response body (story 12-2) — the named provider-parse seam (see the module
/// docs). Returns the same `(input_tokens, output_tokens)` pair as
/// [`parse_openai_usage`].
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
pub(crate) fn parse_openai_sse_usage(body: &[u8]) -> Option<(u64, u64)> {
    // The LAST usable `data:` frame wins (later frames supersede earlier ones).
    let mut last: Option<(u64, u64)> = None;
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
/// untouched — never a panic, never a partial count.
fn ingest_data_payload(payload: &[u8], last: &mut Option<(u64, u64)>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return;
    };
    // `usage: null` is the documented NORMAL for every non-terminal frame —
    // `get` returns Some(Value::Null), `usage_field` then rejects it (as_u64 on
    // null is None), and the frame contributes nothing.
    let Some(usage) = value.get("usage") else {
        return;
    };
    let Some(input) = usage_field(usage, "prompt_tokens") else {
        return;
    };
    let Some(output) = usage_field(usage, "completion_tokens") else {
        return;
    };
    *last = Some((input, output));
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
        assert_eq!(parse_openai_usage(body), Some((128, 512)));
    }

    #[test]
    fn maps_prompt_to_input_and_completion_to_output() {
        // The field-name mapping is the crux: prompt_tokens → input_tokens,
        // completion_tokens → output_tokens (NOT swapped).
        let body = br#"{"usage": {"prompt_tokens": 7, "completion_tokens": 3}}"#;
        let (input, output) = parse_openai_usage(body).expect("well-formed");
        assert_eq!(input, 7, "prompt_tokens maps to input");
        assert_eq!(output, 3, "completion_tokens maps to output");
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
        assert_eq!(parse_openai_usage(body), Some((11, 22)));
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
        assert_eq!(parse_openai_usage(body), Some((0, 0)));
    }

    #[test]
    fn a_large_count_within_u64_parses() {
        // A large but valid count parses (the storage-boundary clamp to i64 is the
        // ledger's job, story 3-1 — here we just read the u64 faithfully).
        let body = br#"{"usage": {"prompt_tokens": 9007199254740993, "completion_tokens": 1}}"#;
        assert_eq!(parse_openai_usage(body), Some((9_007_199_254_740_993, 1)));
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
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((128, 512)));
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
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((3, 4)));
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
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((7, 9)));
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
        assert_eq!(parse_openai_sse_usage(body.as_bytes()), Some((11, 22)));
    }

    #[test]
    fn sse_crlf_terminators_and_no_space_spelling_parse() {
        // SSE allows CRLF line terminators; the `data:<payload>` no-space
        // spelling is tolerated by the scanner (both are wire-legal shapes a
        // lenient proxy must not mis-skip).
        let crlf = "data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\r\n\r\ndata: [DONE]\r\n\r\n";
        assert_eq!(parse_openai_sse_usage(crlf.as_bytes()), Some((5, 6)));
        let no_space = "data:{\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\n\n";
        assert_eq!(parse_openai_sse_usage(no_space.as_bytes()), Some((5, 6)));
    }

    #[test]
    fn sse_zero_counts_are_valid_and_negative_counts_are_skipped() {
        // Zero is a legitimate terminal count (an empty completion) — distinct
        // from None (no frame). A negative count discards its frame (a token
        // count is a non-negative whole number — never a wrapped value).
        let zeros = "data: {\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\n\n";
        assert_eq!(parse_openai_sse_usage(zeros.as_bytes()), Some((0, 0)));
        let negative = "data: {\"usage\":{\"prompt_tokens\":-1,\"completion_tokens\":2}}\n\n";
        assert_eq!(parse_openai_sse_usage(negative.as_bytes()), None);
    }
}
