//! The host-owned OTLP telemetry exporter (AD-21): an OPT-IN module that
//! subscribes read-only to the engine event bus and POSTs `otlp-metrics v1`
//! metric families to an operator-configured collector over OTLP/HTTP+JSON —
//! hand-rolled, zero new runtime dependencies.
//!
//! The privacy default is structural, not a flag default: with NO
//! `--otlp-endpoint` configured the exporter DOES NOT EXIST — no task is
//! spawned, no socket is dialed, nothing leaves the machine (asserted by
//! `tests/otlp_export.rs`). It is metrics-only by construction: no event
//! payload content, no prompts/responses, no secrets — header values ride a
//! redacting newtype and secret references (`secret:NAME`) resolve at
//! startup through the engine's composite resolver, refusing typed and
//! naming the secret when unresolvable (the `--bearer-secret` rule).
//!
//! The exporter degrades visibly and never delays supervision: every loss
//! (bus lag, queue overflow, a dropped batch) is surfaced through a
//! `[hekma] ` note AND the `hekma.otlp.dropped` self-metric; every POST is
//! timeout-bounded; the shutdown handshake bounds the final flush so the
//! host's exit is never held past it. No durable telemetry persistence —
//! the outbox stays the only durable export surface.

pub mod client;
pub mod payload;
pub mod task;

// The operator/host-facing types re-exported at the module root (ServeOptions
// carries OtlpOptions; the serve path builds the exporter handle).
#[allow(unused_imports)]
pub(crate) use task::ExporterConfig;
pub use task::OtlpExporter;

use std::fmt;

/// The flush interval's default (whole seconds) — the AD-21 default.
pub const DEFAULT_FLUSH_INTERVAL_SECS: u64 = 10;

/// The inclusive bounds of the flush interval (whole seconds): a sub-second
/// flush is a request storm, a multi-hour one silently strands dashboards —
/// refused everywhere, the stop-window discipline.
pub const MIN_FLUSH_INTERVAL_SECS: u64 = 1;
pub const MAX_FLUSH_INTERVAL_SECS: u64 = 3_600;

/// One `--otlp-headers` value — a literal, or a `secret:NAME` reference.
/// The manual `Debug` NEVER prints the value (the no-leak discipline: the
/// raw text of a literal IS the secret material an operator is injecting,
/// and a reference names its env/filesystem source). Construction is the
/// only way text enters.
#[derive(Clone)]
pub struct OtlpHeaderValue(String);

impl OtlpHeaderValue {
    /// Wrap a raw header value (a literal, or `secret:NAME`).
    pub fn new(raw: impl Into<String>) -> Self {
        OtlpHeaderValue(raw.into())
    }

    /// The raw text (literal, or the `secret:NAME` reference as typed).
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the value is a `secret:NAME` reference.
    pub fn is_secret_ref(&self) -> bool {
        self.secret_name().is_some()
    }

    /// The referenced secret NAME (`secret:<name>` form), if any. The name
    /// must be non-empty — `secret:` alone is a parse refusal at the split.
    pub fn secret_name(&self) -> Option<&str> {
        self.0
            .strip_prefix("secret:")
            .filter(|name| !name.is_empty())
    }
}

impl fmt::Debug for OtlpHeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the value — not even a length hint (the BearerToken rule).
        f.write_str("OtlpHeaderValue(<redacted>)")
    }
}

/// The parsed `--otlp-*` configuration — ONE surface, parsed BOTH places
/// (the host binary's hand-rolled loop and the clap `Serve` variant both
/// call [`OtlpOptions::from_flags`], so the two entry points cannot drift).
/// The DEFAULT (`OtlpOptions::default()`) is the exporter ABSENT.
#[derive(Clone, Default)]
pub struct OtlpOptions {
    /// The collector base URL (`http://host:port[/base]`) — `None` = the
    /// exporter does not exist. http-only in v1 (`https://` is a documented
    /// deferral, refused with a typed message naming it).
    pub endpoint: Option<String>,
    /// The headers to attach to every POST (auth and friends). Values may be
    /// `secret:NAME` references — resolved at serve time through the
    /// engine's composite resolver.
    pub headers: Vec<(String, OtlpHeaderValue)>,
    /// The flush interval override (whole seconds; `None` =
    /// [`DEFAULT_FLUSH_INTERVAL_SECS`]).
    pub flush_interval: Option<u64>,
}

impl OtlpOptions {
    /// Parse the three raw flag values (the single parse site for BOTH CLI
    /// surfaces). Errors are the human refusal — callers map them onto the
    /// usage class (exit 2). Header spec shape: `k=v[,k=v...]`; a value may
    /// be `secret:NAME`.
    pub fn from_flags(
        endpoint: Option<String>,
        headers_raw: Option<&str>,
        flush_interval: Option<u64>,
    ) -> Result<Self, String> {
        let mut headers = Vec::new();
        if let Some(spec) = headers_raw {
            for pair in spec.split(',') {
                if pair.is_empty() {
                    continue;
                }
                let (name, value) = pair.split_once('=').ok_or_else(|| {
                    format!("--otlp-headers entries are name=value pairs (got {pair:?})")
                })?;
                let name = name.trim();
                if name.is_empty() || name.contains([' ', ':']) {
                    return Err(format!(
                        "--otlp-headers names must be non-empty header names without spaces \
                         or colons (got {name:?})"
                    ));
                }
                // P4b: CR/LF (or any control character) in a NAME or a VALUE
                // is header/HTTP response-splitting material — refused at
                // parse time, the usage class, instead of being left to the
                // HTTP builder's silent error state.
                if name.chars().any(char::is_control) {
                    return Err(format!(
                        "--otlp-headers names must not contain control characters \
                         (CR/LF and friends — got {name:?})"
                    ));
                }
                if value.chars().any(char::is_control) {
                    return Err(format!(
                        "--otlp-headers value for {name:?} must not contain control \
                         characters (CR/LF and friends)"
                    ));
                }
                // P4b: duplicate names are refused (HTTP header names are
                // case-insensitive on the wire — a pair differing only in
                // case would silently shadow or double-send).
                let lowered = name.to_ascii_lowercase();
                if headers
                    .iter()
                    .any(|(existing, _): &(String, OtlpHeaderValue)| {
                        existing.eq_ignore_ascii_case(&lowered)
                    })
                {
                    return Err(format!(
                        "--otlp-headers names must be unique (got {name:?} twice — \
                         header names are case-insensitive)"
                    ));
                }
                if value.starts_with("secret:") && value.len() == "secret:".len() {
                    return Err(format!(
                        "--otlp-headers value for {name:?} names an EMPTY secret \
                         (`secret:`) — give the secret's NAME (`secret:THE_NAME`)"
                    ));
                }
                headers.push((name.to_string(), OtlpHeaderValue::new(value)));
            }
        }
        let options = OtlpOptions {
            endpoint,
            headers,
            flush_interval,
        };
        options.validate()?;
        Ok(options)
    }

    /// Whether the exporter exists (the structural opt-in: endpoint set).
    pub fn configured(&self) -> bool {
        self.endpoint.is_some()
    }

    /// The effective flush interval (the single defaulting site).
    pub fn effective_flush_interval(&self) -> u64 {
        self.flush_interval.unwrap_or(DEFAULT_FLUSH_INTERVAL_SECS)
    }

    /// The typed validation the host lib re-runs regardless of which CLI
    /// parsed the flags (defense in depth — a library caller constructing
    /// [`super::ServeOptions`] by hand gets the same refusals).
    pub fn validate(&self) -> Result<(), String> {
        if let Some(endpoint) = &self.endpoint {
            // P3c: the STRICT refusals (query/fragment, non-numeric port, a
            // base already ending in /v1/metrics) live here — a rejected
            // INVOCATION, never a runtime surprise inside the export loop.
            client::validate_endpoint(endpoint)?;
        } else if !self.headers.is_empty() {
            return Err(
                "--otlp-headers requires --otlp-endpoint: there is no export to \
                 authenticate while the exporter is off"
                    .to_string(),
            );
        } else if self.flush_interval.is_some() {
            // P4c: the headers rule, verbatim, for the interval — a flush
            // cadence for an exporter that does not exist is a half-config
            // surface, refused on BOTH the clap layer (requires) and here.
            return Err(
                "--otlp-flush-interval requires --otlp-endpoint: there is no \
                 export to flush while the exporter is off"
                    .to_string(),
            );
        }
        if let Some(secs) = self.flush_interval {
            if !(MIN_FLUSH_INTERVAL_SECS..=MAX_FLUSH_INTERVAL_SECS).contains(&secs) {
                return Err(format!(
                    "invalid --otlp-flush-interval {secs}: the flush interval must be a \
                     whole number of seconds from {MIN_FLUSH_INTERVAL_SECS} to \
                     {MAX_FLUSH_INTERVAL_SECS}"
                ));
            }
        }
        Ok(())
    }

    /// Resolve `secret:NAME` header values through the engine's composite
    /// resolver (env, then the state-dir secrets file — AD-10; the host
    /// never builds a second resolution mechanism). An unresolvable name is
    /// a typed refusal NAMING the secret (the `--bearer-secret` rule, the
    /// I/O matrix's exit-2 class); the RESOLVED values are returned as plain
    /// strings consumed only by the HTTP client — never options, logs, or
    /// notices.
    pub fn resolve_headers(
        &self,
        resolver: &hekma_engine::ports::CompositeSecretResolver,
    ) -> Result<Vec<(String, String)>, String> {
        let mut resolved = Vec::with_capacity(self.headers.len());
        for (name, value) in &self.headers {
            match value.secret_name() {
                Some(secret_name) => match resolver.require(secret_name) {
                    Ok(secret) => resolved.push((name.clone(), secret.expose_secret().to_string())),
                    Err(err) => {
                        return Err(format!(
                            "the OTLP header {name:?} references secret {secret_name:?}, \
                             which could not be resolved ({err}) — set it in the \
                             environment or the state dir's secrets file before serving \
                             with the exporter enabled"
                        ));
                    }
                },
                None => resolved.push((name.clone(), value.expose().to_string())),
            }
        }
        Ok(resolved)
    }
}

impl fmt::Debug for OtlpOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Headers print NAME + "<redacted>" — never a literal value, never
        // even a secret NAME's resolution state.
        let headers: Vec<(String, &str)> = self
            .headers
            .iter()
            .map(|(name, _)| (name.clone(), "<redacted>"))
            .collect();
        f.debug_struct("OtlpOptions")
            .field("endpoint", &self.endpoint)
            .field("headers", &headers)
            .field("flush_interval", &self.flush_interval)
            .finish()
    }
}

/// The `service.instance.id` resource attribute: derived from the state
/// directory root (the lock identity — the same root the single-writer
/// `host.lock` guards), stable across restarts of the same state dir and
/// distinct across different ones. A hash, not the path: the raw filesystem
/// path never rides the telemetry payload.
///
/// The hash is a hand-rolled FNV-1a (64-bit) over the CANONICALIZED path's
/// bytes — pinned and toolchain-stable BY CONSTRUCTION (P4a): std's
/// `DefaultHasher` documents its internal algorithm as unspecified and free
/// to change between Rust versions, so an MSRV bump could silently re-key
/// every state dir's identity mid-series. FNV-1a is 6 lines, dependency-free,
/// and its outputs are fixed forever.
pub fn service_instance_id(state_base: &std::path::Path) -> String {
    // Canonicalize best-effort: the live host calls this on an existing
    // state root (absolute); a test (or a not-yet-created root) falls back
    // to the path as given — either way the SAME input always yields the
    // SAME id within and across processes on one toolchain.
    let canonical = std::fs::canonicalize(state_base).unwrap_or_else(|_| state_base.to_path_buf());
    // FNV-1a over the path bytes (lossy-encoded for non-UTF-8 OS strings —
    // the identity only needs distinctness + stability, not round-tripping).
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_options_are_the_absent_exporter() {
        let options = OtlpOptions::default();
        assert!(!options.configured());
        assert_eq!(
            options.effective_flush_interval(),
            DEFAULT_FLUSH_INTERVAL_SECS
        );
        assert!(options.validate().is_ok());
    }

    #[test]
    fn from_flags_parses_headers_and_defaults_the_interval() {
        let options = OtlpOptions::from_flags(
            Some("http://127.0.0.1:4318".to_string()),
            Some("authorization=secret:OTLP_AUTH, x-tenant=acme"),
            None,
        )
        .unwrap();
        assert_eq!(options.headers.len(), 2);
        assert_eq!(options.headers[0].0, "authorization");
        assert!(options.headers[0].1.is_secret_ref());
        assert_eq!(options.headers[0].1.secret_name(), Some("OTLP_AUTH"));
        assert_eq!(options.headers[1].1.expose(), "acme");
        assert!(!options.headers[1].1.is_secret_ref());
        assert_eq!(
            options.effective_flush_interval(),
            DEFAULT_FLUSH_INTERVAL_SECS
        );
    }

    #[test]
    fn malformed_header_specs_are_typed_refusals() {
        let err = OtlpOptions::from_flags(Some("http://h".to_string()), Some("auth-only"), None)
            .unwrap_err();
        assert!(err.contains("name=value"), "{err}");
        let err = OtlpOptions::from_flags(Some("http://h".to_string()), Some("=value"), None)
            .unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
        let err = OtlpOptions::from_flags(Some("http://h".to_string()), Some("a=1,=2"), None)
            .unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
    }

    #[test]
    fn an_empty_secret_reference_is_a_typed_refusal() {
        let err = OtlpOptions::from_flags(
            Some("http://h".to_string()),
            Some("authorization=secret:"),
            None,
        )
        .unwrap_err();
        // The split into name/ref succeeds only for a NON-empty name, so the
        // bare `secret:` falls out as an unresolvable-shape value — the
        // resolver would look for a secret literally named ""; refuse at
        // parse time instead.
        assert!(!err.is_empty());
    }

    #[test]
    fn out_of_bounds_flush_intervals_are_typed_refusals() {
        let err = OtlpOptions::from_flags(Some("http://h".to_string()), None, Some(0)).unwrap_err();
        assert!(err.contains("--otlp-flush-interval"), "{err}");
        assert!(OtlpOptions::from_flags(
            Some("http://h".to_string()),
            None,
            Some(MAX_FLUSH_INTERVAL_SECS)
        )
        .is_ok());
        assert!(OtlpOptions::from_flags(
            Some("http://h".to_string()),
            None,
            Some(MAX_FLUSH_INTERVAL_SECS + 1)
        )
        .is_err());
    }

    #[test]
    fn headers_without_an_endpoint_are_refused_the_exporter_does_not_half_exist() {
        let err = OtlpOptions::from_flags(None, Some("authorization=x"), None).unwrap_err();
        assert!(err.contains("--otlp-endpoint"), "{err}");
    }

    #[test]
    fn a_flush_interval_without_an_endpoint_is_refused_on_the_lib_surface_too() {
        // P4c: the clap `requires` rule mirrored here — a hand-built options
        // value gets the same typed refusal the CLI user gets.
        let err = OtlpOptions::from_flags(None, None, Some(10)).unwrap_err();
        assert!(err.contains("--otlp-flush-interval"), "{err}");
        assert!(err.contains("--otlp-endpoint"), "{err}");
        assert!(
            OtlpOptions {
                flush_interval: Some(10),
                ..Default::default()
            }
            .validate()
            .is_err(),
            "the struct-level guard fires too"
        );
    }

    #[test]
    fn control_characters_in_header_names_or_values_are_refused() {
        // P4b: CR/LF (and any control char) is response-splitting material.
        let err = OtlpOptions::from_flags(
            Some("http://h".to_string()),
            Some("x-bad\r-injected=ok"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("control"), "{err}");
        let err = OtlpOptions::from_flags(
            Some("http://h".to_string()),
            Some("authorization=Bearer a\rb"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("control"), "{err}");
        let err = OtlpOptions::from_flags(
            Some("http://h".to_string()),
            Some("authorization=Bearer a\nb"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("control"), "{err}");
    }

    #[test]
    fn duplicate_header_names_are_refused_case_insensitively() {
        // P4b: exact duplicates AND case-only variants (HTTP names are
        // case-insensitive on the wire).
        let err = OtlpOptions::from_flags(
            Some("http://h".to_string()),
            Some("x-tenant=acme,x-tenant=other"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("unique"), "{err}");
        let err = OtlpOptions::from_flags(
            Some("http://h".to_string()),
            Some("x-tenant=acme,X-Tenant=other"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("unique"), "{err}");
        // Distinct names are fine.
        assert!(OtlpOptions::from_flags(
            Some("http://h".to_string()),
            Some("x-tenant=acme,x-other=1"),
            None
        )
        .is_ok());
    }

    #[test]
    fn malformed_endpoints_are_refused_at_flag_parse_with_named_reasons() {
        // P3c: the strict endpoint refusals ride from_flags → validate.
        let err =
            OtlpOptions::from_flags(Some("http://h:4318?x=1".to_string()), None, None).unwrap_err();
        assert!(err.contains("query string"), "{err}");
        let err =
            OtlpOptions::from_flags(Some("http://h:4318#f".to_string()), None, None).unwrap_err();
        assert!(err.contains("fragment"), "{err}");
        let err = OtlpOptions::from_flags(Some("http://h: collector".to_string()), None, None)
            .unwrap_err();
        // The authority sanity check fires first (space in authority).
        assert!(!err.is_empty());
        let err =
            OtlpOptions::from_flags(Some("http://h:notaport".to_string()), None, None).unwrap_err();
        assert!(err.contains("numeric"), "{err}");
        let err = OtlpOptions::from_flags(Some("http://h:4318/v1/metrics".to_string()), None, None)
            .unwrap_err();
        assert!(err.contains("BASE URL"), "{err}");
    }

    #[test]
    fn the_debug_of_header_values_never_shows_the_value() {
        let value = OtlpHeaderValue::new("super-secret-token");
        let rendered = format!("{value:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        let options = OtlpOptions::from_flags(
            Some("http://h".to_string()),
            Some("authorization=super-secret-token"),
            None,
        )
        .unwrap();
        let rendered = format!("{options:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn the_instance_id_is_a_path_derived_hash_not_the_path() {
        let id = service_instance_id(std::path::Path::new("/tmp/state-a"));
        let again = service_instance_id(std::path::Path::new("/tmp/state-a"));
        let other = service_instance_id(std::path::Path::new("/tmp/state-b"));
        assert_eq!(id, again, "stable across restarts of the same state dir");
        assert_ne!(id, other, "distinct across different state dirs");
        assert!(
            !id.contains('/'),
            "the raw path never rides the payload: {id}"
        );
        assert_eq!(id.len(), 16);
    }

    #[test]
    fn the_instance_id_hash_is_pinned_fnv1a_toolchain_stable() {
        // P4a: the id is FNV-1a (64-bit), pinned against INDEPENDENT known
        // vectors — a toolchain bump (or a std hasher change) can never
        // silently re-key a state dir's identity mid-series.
        // FNV-1a("") = 0xcbf29ce484222325 (the offset basis itself)
        assert_eq!(
            service_instance_id(std::path::Path::new("")),
            "cbf29ce484222325"
        );
        // FNV-1a("a") = 0xaf63dc4c8601ec8c — a nonexistent relative path
        // falls back to the given bytes, so the vector applies verbatim.
        assert_eq!(
            service_instance_id(std::path::Path::new("a")),
            "af63dc4c8601ec8c"
        );
        // FNV-1a("foobar") = 0x85944171f73967e8
        assert_eq!(
            service_instance_id(std::path::Path::new("foobar")),
            "85944171f73967e8"
        );
    }
}
