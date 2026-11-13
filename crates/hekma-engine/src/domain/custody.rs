//! Key custody's pinning layer (story 22-2, spine AD-22): where a custodied
//! provider key may be sent, and the in-memory cache its values live in.
//!
//! The RULES here are operator-tier and pure (table-tested):
//!
//! * **Origins travel with the key.** A `[custody.NAME]` table beside the
//!   secret in `secrets.toml` may declare `origins` (and `paths`); when it
//!   does not, the versioned built-in table in this module supplies the
//!   defaults for well-known provider hosts, matched at operator start
//!   against the configured upstream. The resolved origin is FROZEN on the
//!   Run for its life — an agent that later edits its own configuration
//!   cannot move the key.
//! * **Canonical-origin equality.** Two origins match when their scheme,
//!   lowercased host (no trailing dot) and effective port match —
//!   `https://Host` equals `https://host:443` and nothing else.
//! * **Paths are scoped.** A custodied key reaches only the synchronous
//!   inference paths on its allow-list (declared else the built-in
//!   defaults) — never batch, fine-tune, file or key-management
//!   endpoints whose spend or secrets the metering path cannot account
//!   for.
//!
//! The CACHE ([`CustodyCache`]) is the data-path half: custodied values
//! live ONLY in memory, in a map beside the published snapshot (never in
//! it), refreshed off the hot path every [`CUSTODY_REFRESH_SECS`] at most.
//! Rotation at the provider propagates within that bound; a failed refresh
//! keeps the last good value and raises a rate-limited diagnostic — a
//! stale key fails at the provider, loudly, never silently.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use hyper::header::{HeaderName, HeaderValue};
use hyper::Uri;

use crate::domain::SecretString;
use crate::ports::{CompositeSecretResolver, SecretError, SecretResolver};

/// How often the custody cache re-resolves its held secrets (the documented
/// pick-up bound for a rotated key: "within 30 s").
pub(crate) const CUSTODY_REFRESH_SECS: u64 = 30;

/// The pinning an operator may declare beside a secret in `secrets.toml`
/// (`[custody.NAME]`). `None` fields fall back to the built-in provider
/// table; see `docs/config-secrets-memory.md` for the shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustodyPin {
    /// `origins = ["https://…"]` — the canonical https origins the key may
    /// be sent to. `None` → the built-in table matched at start.
    pub origins: Option<Vec<String>>,
    /// `paths = ["/v1/…"]` — the outbound path prefixes the key may reach.
    /// `None` → the built-in provider's synchronous-inference prefixes.
    pub paths: Option<Vec<String>>,
}

/// The credential header scheme a provider expects its key in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialScheme {
    /// `authorization: Bearer <key>` (the default).
    Bearer,
    /// `x-api-key: <key>` (Anthropic's scheme).
    XApiKey,
    /// `x-goog-api-key: <key>` (Google's Gemini API key scheme).
    XGoogApiKey,
    /// `api-key: <key>` (Azure OpenAI's API key scheme).
    AzureApiKey,
}

impl CredentialScheme {
    /// The header name the scheme inserts.
    pub(crate) fn header_name(self) -> &'static str {
        match self {
            CredentialScheme::Bearer => "authorization",
            CredentialScheme::XApiKey => "x-api-key",
            CredentialScheme::XGoogApiKey => "x-goog-api-key",
            CredentialScheme::AzureApiKey => "api-key",
        }
    }

    /// The header value carrying `value`, marked sensitive (never `Debug`ed).
    pub(crate) fn header_value(self, value: &SecretString) -> Option<(HeaderName, HeaderValue)> {
        let rendered = match self {
            CredentialScheme::Bearer => format!("Bearer {}", value.expose_secret()),
            CredentialScheme::XApiKey
            | CredentialScheme::XGoogApiKey
            | CredentialScheme::AzureApiKey => value.expose_secret().to_string(),
        };
        let value = HeaderValue::from_str(&rendered).ok()?;
        let mut value = value;
        value.set_sensitive(true);
        Some((HeaderName::from_static(self.header_name()), value))
    }
}

/// One well-known provider in the versioned built-in table. `host` matches
/// the upstream's host exactly, or as a suffix when it begins with a dot
/// (per-resource hosts like `<resource>.openai.azure.com`).
struct BuiltinProvider {
    host: &'static str,
    origin: &'static str,
    paths: &'static [&'static str],
    scheme: CredentialScheme,
}

/// The built-in provider table, v1. Additions are additive and docs-versioned
/// (`docs/metering-proxy.md`); each entry's paths cover that provider's
/// SYNCHRONOUS inference endpoints only.
const BUILTIN_PROVIDERS: &[BuiltinProvider] = &[
    BuiltinProvider {
        host: "api.openai.com",
        origin: "https://api.openai.com",
        paths: &[
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/responses",
            "/v1/embeddings",
        ],
        scheme: CredentialScheme::Bearer,
    },
    BuiltinProvider {
        host: "api.anthropic.com",
        origin: "https://api.anthropic.com",
        paths: &["/v1/messages", "/v1/complete", "/v1/count_tokens"],
        scheme: CredentialScheme::XApiKey,
    },
    BuiltinProvider {
        host: "generativelanguage.googleapis.com",
        origin: "https://generativelanguage.googleapis.com",
        paths: &["/v1beta/models", "/v1/models"],
        scheme: CredentialScheme::XGoogApiKey,
    },
    BuiltinProvider {
        host: "api.mistral.ai",
        origin: "https://api.mistral.ai",
        paths: &[
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/fim/completions",
        ],
        scheme: CredentialScheme::Bearer,
    },
    BuiltinProvider {
        host: "api.groq.com",
        origin: "https://api.groq.com",
        paths: &[
            "/openai/v1/chat/completions",
            "/openai/v1/completions",
            "/v1/chat/completions",
            "/v1/embeddings",
        ],
        scheme: CredentialScheme::Bearer,
    },
    BuiltinProvider {
        host: "api.deepseek.com",
        origin: "https://api.deepseek.com",
        paths: &["/chat/completions", "/v1/chat/completions"],
        scheme: CredentialScheme::Bearer,
    },
    BuiltinProvider {
        host: "openrouter.ai",
        origin: "https://openrouter.ai",
        paths: &[
            "/api/v1/chat/completions",
            "/api/v1/completions",
            "/api/v1/embeddings",
        ],
        scheme: CredentialScheme::Bearer,
    },
    BuiltinProvider {
        host: "api.x.ai",
        origin: "https://api.x.ai",
        paths: &[
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/messages",
        ],
        scheme: CredentialScheme::Bearer,
    },
    BuiltinProvider {
        host: "router.huggingface.co",
        origin: "https://router.huggingface.co",
        paths: &["/v1/chat/completions", "/v1/completions", "/v1/embeddings"],
        scheme: CredentialScheme::Bearer,
    },
    BuiltinProvider {
        // Per-resource Azure OpenAI hosts, `<resource>.openai.azure.com`
        // (the leading dot = suffix match on a label boundary). The
        // deployment path carries the model; the api-version rides the
        // query, which is preserved verbatim.
        host: ".openai.azure.com",
        origin: "",
        paths: &["/openai/deployments"],
        scheme: CredentialScheme::AzureApiKey,
    },
];

fn builtin_by_host(host: &str) -> Option<&'static BuiltinProvider> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    BUILTIN_PROVIDERS.iter().find(|provider| {
        if provider.host.starts_with('.') {
            // A label boundary: `x.openai.azure.com`, never
            // `xopenai.azure.com`.
            host.ends_with(provider.host)
        } else {
            host == provider.host
        }
    })
}

/// The canonical (scheme, host, effective port) of a URL: lowercased host,
/// no trailing dot, the scheme's default port filled in.
pub(crate) fn canonical_origin(url: &str) -> Option<(String, String, u16)> {
    let uri: Uri = url.parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let host = uri.host()?.trim_end_matches('.').to_ascii_lowercase();
    let port = uri.port_u16().or(match scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    })?;
    Some((scheme, host, port))
}

/// Whether two origin strings are canonically EQUAL (`https://Host` matches
/// `https://host:443`; scheme, host and port must otherwise agree).
pub(crate) fn origins_equal(a: &str, b: &str) -> bool {
    match (canonical_origin(a), canonical_origin(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Whether `path` is on the allow-list `prefixes`: equal to a prefix, or
/// beginning with it followed by a `/` segment (so `/v1beta/models` covers
/// `/v1beta/models/gemini:generateContent` but never `/v1beta/modelsx`).
pub(crate) fn path_allowed(prefixes: &[String], path: &str) -> bool {
    prefixes
        .iter()
        .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
}

/// The built-in paths for `origin`'s host, when one is tabled.
pub(crate) fn builtin_paths(origin: &str) -> Option<&'static [&'static str]> {
    canonical_origin(origin)
        .and_then(|(_, host, _)| builtin_by_host(&host))
        .map(|provider| provider.paths)
}

/// The credential scheme for `origin`'s host (the built-in provider's, else
/// `Authorization: Bearer`).
pub(crate) fn scheme_for(origin: &str) -> CredentialScheme {
    canonical_origin(origin)
        .and_then(|(_, host, _)| builtin_by_host(&host))
        .map(|provider| provider.scheme)
        .unwrap_or(CredentialScheme::Bearer)
}

/// Why a custodied start (or binding creation) was refused. The messages
/// are the user-facing remediations; none carries a secret value.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum CustodyError {
    /// No origin could be pinned for the key (none declared, and the
    /// configured upstream's host has no built-in entry).
    #[error(
        "Hekma could not pin where this provider key may be sent. Declare its origins beside \
         the key in the secrets file, for example [custody.{name}] with origins = \
         [\"https://api.example-vendor.com\"]."
    )]
    NoPinnableOrigin {
        /// The secret NAME (the lookup key, never a value).
        name: String,
    },
    /// The configured upstream's origin is not among the origins pinned
    /// beside the key.
    #[error(
        "The upstream origin {origin} is not among the origins pinned beside secret \
         '{name}'. Add it there, or point this agent or binding at a pinned origin."
    )]
    UpstreamNotPinned {
        /// The offending canonical origin.
        origin: String,
        /// The secret NAME.
        name: String,
    },
}

/// The start-time custody resolution: what gets frozen on the Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedCustody {
    /// The canonical pinned origin (frozen for the Run's life).
    pub origin: String,
    /// The `secret:NAME` reference (stored, never a value).
    pub key_ref: String,
    /// The secret NAME (the cache's lookup key).
    pub name: String,
}

/// Resolve the custody origin for a key at operator start.
///
/// `declared` is the `[custody.NAME]` pin when one exists; `upstream_base`
/// the instance's configured `metering.upstream_base_url` (used to match the
/// built-in provider table); `frozen` is an already-frozen origin for a
/// KEPT Run (a Restart Policy relaunch keeps its Run and its pin — an
/// edited configuration changes nothing).
pub(crate) fn resolve_origin(
    name: &str,
    key_ref: &str,
    declared: Option<&CustodyPin>,
    upstream_base: Option<&str>,
    frozen: Option<&str>,
) -> Result<ResolvedCustody, CustodyError> {
    if let Some(frozen) = frozen {
        // The pin is frozen on the Run: relaunches keep it verbatim.
        return Ok(ResolvedCustody {
            origin: frozen.to_string(),
            key_ref: key_ref.to_string(),
            name: name.to_string(),
        });
    }
    // Declared origins: the upstream must be one of them (the frozen pin is
    // the DECLARED spelling, canonicalised only for comparison).
    if let Some(origins) = declared.and_then(|pin| pin.origins.as_deref()) {
        let upstream = upstream_base
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.trim_end_matches('/'));
        let Some(upstream) = upstream else {
            // No configured upstream to match: the FIRST declared origin is
            // the pin (a self-reported custodied agent has no upstream to
            // compare against).
            let origin = origins
                .first()
                .ok_or(CustodyError::NoPinnableOrigin {
                    name: name.to_string(),
                })?
                .trim_end_matches('/')
                .to_string();
            return Ok(ResolvedCustody {
                origin,
                key_ref: key_ref.to_string(),
                name: name.to_string(),
            });
        };
        let chosen = origins
            .iter()
            .find(|declared| origins_equal(declared, upstream))
            .ok_or_else(|| CustodyError::UpstreamNotPinned {
                origin: upstream.to_string(),
                name: name.to_string(),
            })?;
        return Ok(ResolvedCustody {
            origin: chosen.trim_end_matches('/').to_string(),
            key_ref: key_ref.to_string(),
            name: name.to_string(),
        });
    }
    // No declared origins: the built-in table entry matching the configured
    // upstream's host.
    let upstream_host = upstream_base
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(canonical_origin)
        .map(|(_, host, _)| host);
    let Some(host) = upstream_host else {
        return Err(CustodyError::NoPinnableOrigin {
            name: name.to_string(),
        });
    };
    let Some(builtin) = builtin_by_host(&host) else {
        return Err(CustodyError::NoPinnableOrigin {
            name: name.to_string(),
        });
    };
    // The suffix-matched entries (per-resource hosts) pin the CONFIGURED
    // origin, not the table's empty spelling.
    let origin = if builtin.origin.is_empty() {
        let base = upstream_base.unwrap_or_default();
        base.trim().trim_end_matches('/').to_string()
    } else {
        builtin.origin.to_string()
    };
    Ok(ResolvedCustody {
        origin,
        key_ref: key_ref.to_string(),
        name: name.to_string(),
    })
}

/// The path allow-list for a call: the declared list when one exists beside
/// the key, else the built-in provider's for the frozen origin's host.
/// An empty list allows nothing (the operator widens it deliberately).
pub(crate) fn allowed_paths(declared: Option<&CustodyPin>, origin: &str) -> Vec<String> {
    if let Some(paths) = declared.and_then(|pin| pin.paths.as_deref()) {
        return paths.to_vec();
    }
    builtin_paths(origin)
        .map(|paths| paths.iter().map(|path| path.to_string()).collect())
        .unwrap_or_default()
}

/// The schema version of [`SecretRevealedEvent`].
pub const SECRET_REVEALED_SCHEMA_VERSION: u32 = 1;

/// A custodied secret was revealed (story 22-2, P10): `hekma agent config
/// get --reveal` printed the cleartext of an instance's held provider key.
/// Committed and published BEFORE the value prints. Names the secrets,
/// never a value.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SecretRevealedEvent {
    /// The event's schema version.
    pub schema_version: u32,
    /// The Agent Instance whose configuration was revealed.
    pub instance: String,
    /// The revealed custodied secrets' NAMES (the `NAME` of `secret:NAME`).
    pub secrets: Vec<String>,
    /// When (RFC 3339).
    pub at: String,
}

/// What the published snapshot carries for a custodied Run: REFERENCES
/// only, never a value. The proxy resolves `key_ref` through the custody
/// cache at call time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CustodyInfo {
    /// The `secret:NAME` reference (an instance Run's `provider.api_key`,
    /// a binding Run's `custodied_key`).
    pub key_ref: Arc<str>,
    /// The frozen canonical origin the key may be sent to.
    pub origin: Arc<str>,
}

/// One cached custodied secret: its value and the pinning declared beside
/// it (refreshed together, off the hot path).
#[derive(Debug)]
pub(crate) struct CustodyEntry {
    pub value: SecretString,
    pub declared: Option<CustodyPin>,
}

/// The cache's diagnostic sink.
type DiagnosticFn = Arc<dyn Fn(String) + Send + Sync>;

/// The in-memory custody cache (the AD-10 amendment): custodied values
/// live ONLY here, beside — never inside — the published snapshot. A
/// background refresh re-resolves every held reference through the same
/// env-then-file resolver the start path uses; a resolution failure keeps
/// the last good value (a stale key fails at the provider, loudly).
pub(crate) struct CustodyCache {
    secrets_path: PathBuf,
    entries: RwLock<HashMap<String, Arc<CustodyEntry>>>,
    diagnostics: RwLock<Option<DiagnosticFn>>,
}

impl CustodyCache {
    pub(crate) fn new(secrets_path: PathBuf) -> Self {
        CustodyCache {
            secrets_path,
            entries: RwLock::new(HashMap::new()),
            diagnostics: RwLock::new(None),
        }
    }

    /// Install the engine's diagnostic sink (the refresher's notes route
    /// through it; before installation they are dropped — the cache is
    /// refreshable but silent, never blocked).
    pub(crate) fn install_diagnostics(&self, sink: Arc<dyn Fn(String) + Send + Sync>) {
        let mut guard = self.diagnostics.write().unwrap_or_else(|p| p.into_inner());
        *guard = Some(sink);
    }

    fn note(&self, text: String) {
        if let Some(sink) = self
            .diagnostics
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            sink(text);
        }
    }

    /// Seed one entry from a fail-closed START-path resolve: the value the
    /// supervisor just resolved (and never delivers to the agent) enters
    /// the cache so the first call can swap it before the refresher's
    /// first tick.
    pub(crate) fn seed(&self, name: &str, value: SecretString, declared: Option<CustodyPin>) {
        let mut guard = self.entries.write().unwrap_or_else(|p| p.into_inner());
        guard.insert(name.to_string(), Arc::new(CustodyEntry { value, declared }));
    }

    /// Resolve `name` now and cache it (blocking: environment and file
    /// reads — the proxy calls it through `spawn_blocking` on a cache
    /// miss, which is how a binding's key, or a custodied Run adopted
    /// after a restart, reaches the cache). `None` when it does not
    /// resolve: the call is then refused, never forwarded bare.
    pub(crate) fn resolve_now(&self, name: &str) -> Option<Arc<CustodyEntry>> {
        let resolver = CompositeSecretResolver::env_then_file(&self.secrets_path);
        let value = resolver.require(name).ok()?;
        let declared = resolver.custody_pin(name).ok()?;
        self.seed(name, value, declared);
        self.get(name)
    }

    /// The cached entry for `name` (the hot path read; never resolves).
    pub(crate) fn get(&self, name: &str) -> Option<Arc<CustodyEntry>> {
        self.entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned()
    }

    /// The names currently held (tests + the refresh loop's iteration).
    pub(crate) fn held_names(&self) -> Vec<String> {
        self.entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Re-resolve every held reference. A name that still resolves
    /// replaces its entry (a rotated key is picked up within the bound); a
    /// name that fails KEEPS its last good value and is counted here so
    /// the refresher can say so (rate-limited). A name that resolves to
    /// nothing (the secret was removed) keeps the last value too — the Run
    /// is live and its calls must fail at the provider, not vanish.
    pub(crate) fn refresh(&self) -> usize {
        let resolver = CompositeSecretResolver::env_then_file(&self.secrets_path);
        let mut failed = 0usize;
        for name in self.held_names() {
            let refreshed =
                (|| -> Result<Option<(SecretString, Option<CustodyPin>)>, SecretError> {
                    let value = resolver.require(&name)?;
                    let pin = resolver.custody_pin(&name)?;
                    Ok(Some((value, pin)))
                })();
            match refreshed {
                Ok(Some((value, declared))) => self.seed(&name, value, declared),
                Ok(None) | Err(_) => failed += 1,
            }
        }
        failed
    }

    /// Spawn the background refresher on `handle`, re-resolving every
    /// `period` (the engine passes [`CUSTODY_REFRESH_SECS`]; tests pass a
    /// shorter bound). The task holds a weak reference: dropping every
    /// strong cache handle stops it.
    pub(crate) fn start_refresher(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        period: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let cache = Arc::downgrade(self);
        handle.spawn(async move {
            let mut last_note = std::time::Instant::now() - Duration::from_secs(600);
            loop {
                tokio::time::sleep(period).await;
                let Some(cache) = cache.upgrade() else {
                    break;
                };
                // The re-resolve reads the environment and the secrets
                // file: blocking work, off the runtime's async threads.
                let refreshing = Arc::clone(&cache);
                let failed = tokio::task::spawn_blocking(move || refreshing.refresh())
                    .await
                    .unwrap_or(0);
                if failed > 0 && last_note.elapsed() >= Duration::from_secs(60) {
                    last_note = std::time::Instant::now();
                    cache.note(format!(
                        "custody cache: {failed} held provider key(s) could not be \
                         re-resolved; serving the last known value (a rotated-away key fails \
                         at the provider — update the secrets file)"
                    ));
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(origins: &[&str], paths: &[&str]) -> CustodyPin {
        CustodyPin {
            origins: (!origins.is_empty())
                .then(|| origins.iter().map(|o| o.to_string()).collect::<Vec<_>>()),
            paths: (!paths.is_empty())
                .then(|| paths.iter().map(|p| p.to_string()).collect::<Vec<_>>()),
        }
    }

    #[test]
    fn canonical_origin_equality_ignores_case_dots_and_default_ports() {
        assert!(origins_equal(
            "https://api.openai.com",
            "https://api.openai.com:443"
        ));
        assert!(origins_equal(
            "https://API.OpenAI.com.",
            "https://api.openai.com"
        ));
        assert!(!origins_equal(
            "https://api.openai.com",
            "https://api.openai.com:8443"
        ));
        assert!(!origins_equal(
            "https://api.openai.com",
            "http://api.openai.com"
        ));
        assert!(!origins_equal("https://api.openai.com", "https://evil.com"));
        // A path on one side never defeats origin equality.
        assert!(origins_equal(
            "https://api.groq.com/openai/v1",
            "https://api.groq.com"
        ));
    }

    #[test]
    fn the_builtin_table_matches_hosts_and_suffixes() {
        assert!(builtin_by_host("api.openai.com").is_some());
        assert!(builtin_by_host("API.OPENAI.COM.").is_some());
        assert!(builtin_by_host("my-resource.openai.azure.com").is_some());
        assert!(builtin_by_host("api.openai.example.com").is_none());
        // A suffix entry matches on a label boundary only.
        assert!(builtin_by_host("evilopenai.azure.com").is_none());
        assert_eq!(
            builtin_by_host("my-resource.openai.azure.com")
                .unwrap()
                .scheme,
            CredentialScheme::AzureApiKey
        );
        assert_eq!(
            builtin_by_host("generativelanguage.googleapis.com")
                .unwrap()
                .scheme,
            CredentialScheme::XGoogApiKey
        );
        assert_eq!(
            builtin_by_host("api.anthropic.com").unwrap().scheme,
            CredentialScheme::XApiKey
        );
        assert_eq!(
            builtin_by_host("api.openai.com").unwrap().scheme,
            CredentialScheme::Bearer
        );
        // Every non-suffix entry carries a canonical origin.
        for provider in BUILTIN_PROVIDERS {
            if provider.host.starts_with('.') {
                assert!(provider.origin.is_empty());
            } else {
                assert!(!provider.origin.is_empty());
                assert!(origins_equal(provider.origin, provider.origin));
            }
        }
    }

    #[test]
    fn path_prefixes_match_on_segment_boundaries() {
        let prefixes: Vec<String> = ["/v1beta/models", "/v1/chat/completions"]
            .iter()
            .map(|p| p.to_string())
            .collect();
        assert!(path_allowed(&prefixes, "/v1/chat/completions"));
        assert!(path_allowed(&prefixes, "/v1beta/models"));
        assert!(path_allowed(
            &prefixes,
            "/v1beta/models/gemini-pro:generateContent"
        ));
        assert!(!path_allowed(&prefixes, "/v1beta/modelsx"));
        assert!(!path_allowed(&prefixes, "/v1/files"));
        assert!(!path_allowed(&prefixes, "/v1/engines"));
        assert!(!path_allowed(&Vec::new(), "/v1/chat/completions"));
    }

    #[test]
    fn declared_origins_pin_the_declared_spelling_and_require_the_upstream_on_the_list() {
        let declared = pin(&["https://api.example-vendor.com"], &[]);
        let resolved = resolve_origin(
            "PROVIDER",
            "secret:PROVIDER",
            Some(&declared),
            Some("https://api.example-vendor.com/v1"),
            None,
        )
        .unwrap();
        assert_eq!(resolved.origin, "https://api.example-vendor.com");
        assert_eq!(resolved.key_ref, "secret:PROVIDER");

        let err = resolve_origin(
            "PROVIDER",
            "secret:PROVIDER",
            Some(&declared),
            Some("https://evil.example"),
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Add it there, or point this agent or binding"),
            "{}",
            err
        );
        assert!(matches!(
            &err,
            CustodyError::UpstreamNotPinned { origin, name }
                if origin == "https://evil.example" && name == "PROVIDER"
        ));
    }

    #[test]
    fn undeclared_origins_fall_back_to_the_builtin_match_or_refuse() {
        let resolved = resolve_origin(
            "PROVIDER",
            "secret:PROVIDER",
            None,
            Some("https://api.openai.com/v1"),
            None,
        )
        .unwrap();
        assert_eq!(resolved.origin, "https://api.openai.com");

        // A suffix provider pins the CONFIGURED origin.
        let resolved = resolve_origin(
            "AZURE",
            "secret:AZURE",
            None,
            Some("https://my-resource.openai.azure.com"),
            None,
        )
        .unwrap();
        assert_eq!(resolved.origin, "https://my-resource.openai.azure.com");

        // Unknown host AND no declaration: the typed start refusal with the
        // declare-origins remediation.
        let err = resolve_origin(
            "PROVIDER",
            "secret:PROVIDER",
            None,
            Some("https://api.example-vendor.com"),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, CustodyError::NoPinnableOrigin { .. }));
        assert!(
            err.to_string()
                .contains("Declare its origins beside the key"),
            "{}",
            err
        );
        // No upstream at all: same refusal.
        assert!(resolve_origin("P", "secret:P", None, None, None).is_err());
        // Declared origins with no upstream: the first declared origin.
        let declared = pin(&["https://api.example-vendor.com"], &[]);
        let resolved = resolve_origin("P", "secret:P", Some(&declared), None, None).unwrap();
        assert_eq!(resolved.origin, "https://api.example-vendor.com");
    }

    #[test]
    fn a_frozen_origin_survives_any_configuration_change() {
        // A kept Run (a Restart Policy relaunch) keeps its frozen pin
        // verbatim: the declared list, the built-in table and the (edited)
        // upstream are all ignored.
        let declared = pin(&["https://elsewhere.example"], &[]);
        let resolved = resolve_origin(
            "PROVIDER",
            "secret:PROVIDER",
            Some(&declared),
            Some("https://evil.example"),
            Some("https://api.openai.com"),
        )
        .unwrap();
        assert_eq!(resolved.origin, "https://api.openai.com");
    }

    #[test]
    fn allowed_paths_prefer_the_declared_list_else_the_builtin() {
        let declared = pin(&["https://api.openai.com"], &["/custom/path"]);
        assert_eq!(
            allowed_paths(Some(&declared), "https://api.openai.com"),
            vec!["/custom/path".to_string()]
        );
        assert!(allowed_paths(None, "https://api.openai.com")
            .contains(&"/v1/chat/completions".to_string()));
        assert!(!allowed_paths(None, "https://api.openai.com").contains(&"/v1/files".to_string()));
        // An origin with no builtin entry and no declared list allows
        // nothing (the operator widens deliberately).
        assert!(allowed_paths(None, "https://api.example-vendor.com").is_empty());
    }

    #[test]
    fn the_cache_serves_seeded_values_and_picks_up_changes_on_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "PROVIDER = \"sentinel-one\"\n").unwrap();
        crate::backends::restrict_to_owner(&path).unwrap();
        let cache = CustodyCache::new(path.clone());
        cache.seed(
            "PROVIDER",
            SecretString::new("sentinel-one"),
            Some(pin(&["https://api.openai.com"], &[])),
        );
        let entry = cache.get("PROVIDER").unwrap();
        assert_eq!(entry.value.expose_secret(), "sentinel-one");

        // The refresh re-resolves through the file: a rotated key is
        // picked up, pins included.
        std::fs::write(&path, "PROVIDER = \"sentinel-two\"\n").unwrap();
        crate::backends::restrict_to_owner(&path).unwrap();
        assert_eq!(cache.refresh(), 0);
        let entry = cache.get("PROVIDER").unwrap();
        assert_eq!(entry.value.expose_secret(), "sentinel-two");

        // A failed re-resolution keeps the last good value and is counted.
        std::fs::write(&path, "OTHER = \"x\"\n").unwrap();
        crate::backends::restrict_to_owner(&path).unwrap();
        assert_eq!(cache.refresh(), 1);
        let entry = cache.get("PROVIDER").unwrap();
        assert_eq!(
            entry.value.expose_secret(),
            "sentinel-two",
            "a failed refresh keeps the last good value"
        );
        assert!(cache.get("NEVER-SEEDED").is_none());
    }

    #[test]
    fn the_bearer_and_x_api_key_schemes_build_sensitive_headers() {
        let (name, value) = CredentialScheme::Bearer
            .header_value(&SecretString::new("sk-sentinel"))
            .unwrap();
        assert_eq!(name.as_str(), "authorization");
        assert!(value.is_sensitive());
        assert_eq!(value.to_str().unwrap(), "Bearer sk-sentinel");
        let (name, value) = CredentialScheme::XApiKey
            .header_value(&SecretString::new("sk-sentinel"))
            .unwrap();
        assert_eq!(name.as_str(), "x-api-key");
        assert!(value.is_sensitive());
        assert_eq!(value.to_str().unwrap(), "sk-sentinel");
        for (scheme, header) in [
            (CredentialScheme::XGoogApiKey, "x-goog-api-key"),
            (CredentialScheme::AzureApiKey, "api-key"),
        ] {
            let (name, value) = scheme
                .header_value(&SecretString::new("sk-sentinel"))
                .unwrap();
            assert_eq!(name.as_str(), header);
            assert!(value.is_sensitive());
            assert_eq!(value.to_str().unwrap(), "sk-sentinel");
        }
    }
}
