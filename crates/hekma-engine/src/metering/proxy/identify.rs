//! Per-call identification over one published snapshot (story 23-1, spine
//! AD-26) — PURE: no I/O, no lock, no clock. Table-tested in `tests.rs`.
//!
//! Order:
//! 1. a credential in the URL → `credential_in_url`;
//! 2. a presented Run Key (`x-hekma-run-key`), or an `hk_` token in the
//!    API-key slot (`authorization: Bearer`, `x-api-key`) → the key's Run,
//!    `unknown_credential`, `credential_revoked` or `custody_unavailable`;
//! 3. the engine-made Run route `/_hekma/runs/<run id>/…`;
//! 4. the longest Operator binding on a segment boundary;
//! 5. the legacy root route (one adopted pre-proxy agent), else `no_route`.
//!
//! The Run is then the key's (scope-checked against a Run route), else the
//! route's, else the binding's own Run when it has labels, else the single
//! unattributed Run; an `x-hekma-run` hint names a child (beneath a key or
//! a route) or a split Run (beneath a labelled binding) and is ignored
//! beneath the unattributed Run. The upstream is always the route's or the
//! binding's — a Run Key never chooses where a call goes.

use std::sync::Arc;

use crate::domain::credential::{parse_presented, scan_url, TokenClass, TokenHash};
use crate::domain::runs::snapshot::RunSnapshot;
use crate::domain::runs::wire::{parse_hint_labels, valid_external_id, WireIdentity};
use crate::domain::RunId;

/// The engine-made Run route prefix.
pub(crate) const RUN_ROUTE_PREFIX: &str = "/_hekma/runs/";

/// The request facts identification reads (header values already
/// extracted; nothing else of the request).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CallFacts<'a> {
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub run_key: Option<&'a str>,
    pub authorization: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub hint: Option<&'a str>,
    pub hint_labels: Option<&'a str>,
}

/// Why a call was refused before any upstream dial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    CredentialInUrl,
    CustodyUnavailable,
    UnknownCredential,
    CredentialRevoked,
    RunOutOfScope,
    NoRoute,
}

/// How the call reached its Run (for the record's binding field and the
/// counters).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    RunRoute,
    LegacyRoot,
    Binding(String),
}

/// The identified destination of a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    /// The upstream base URL.
    pub upstream: Arc<str>,
    /// The path after the route or binding prefix (`""` allowed).
    pub rest: String,
    pub via: Via,
    /// The presented Run Key's token id, when one identified the call.
    pub token_id: Option<String>,
}

/// Which Run a call is attributed to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Attribution {
    /// An open Run in the snapshot.
    Run(RunId),
    /// A wire Run to ensure through the writer; `fallback` is where the
    /// call stays when the breadth cap is reached (`None` → ensure
    /// `fallback_identity` instead).
    Ensure {
        identity: WireIdentity,
        fallback: Option<RunId>,
        fallback_identity: Option<WireIdentity>,
    },
}

/// The outcome of identification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Identified {
    Call {
        target: Target,
        attribution: Attribution,
        /// A hint that was ignored (beneath the unattributed Run, or
        /// malformed) — counted.
        hint_ignored: bool,
    },
    Refused(Refusal),
}

fn bearer(value: &str) -> Option<&str> {
    let value = value.trim();
    let (scheme, rest) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
}

fn looks_like_token(value: &str) -> bool {
    value.len() >= 3 && value.as_bytes()[..3].eq_ignore_ascii_case(b"hk_")
}

/// A presented token's verdict against the snapshot.
enum KeyVerdict {
    Run(RunId, String),
    Unknown,
    Revoked,
}

fn verdict(snapshot: &RunSnapshot, raw: &str) -> KeyVerdict {
    let Some((class, secret)) = parse_presented(raw.trim()) else {
        return KeyVerdict::Unknown;
    };
    let hash = TokenHash::of_token(secret.expose_secret());
    match snapshot.keys.get(&hash) {
        Some(entry) if entry.class == TokenClass::RunKey && class == TokenClass::RunKey => {
            KeyVerdict::Run(entry.run_id.clone(), entry.token_id.clone())
        }
        Some(_) => KeyVerdict::Unknown,
        None if snapshot.revoked.contains(&hash) => KeyVerdict::Revoked,
        None => KeyVerdict::Unknown,
    }
}

/// Identify one call (see the module docs).
pub(crate) fn identify(snapshot: &RunSnapshot, facts: &CallFacts<'_>) -> Identified {
    // 1. A credential in the URL (raw or percent-decoded) — before anything.
    let target = match facts.query {
        Some(query) => format!("{}?{query}", facts.path),
        None => facts.path.to_string(),
    };
    if scan_url(&target).found {
        return Identified::Refused(Refusal::CredentialInUrl);
    }

    // 2. A presented Run Key; an `hk_` token in the API-key slot.
    let slot = facts
        .authorization
        .and_then(bearer)
        .filter(|value| looks_like_token(value))
        .or(facts.api_key.map(str::trim).filter(|v| looks_like_token(v)));
    if let Some(raw) = slot {
        return Identified::Refused(match verdict(snapshot, raw) {
            KeyVerdict::Run(..) => Refusal::CustodyUnavailable,
            KeyVerdict::Unknown => Refusal::UnknownCredential,
            KeyVerdict::Revoked => Refusal::CredentialRevoked,
        });
    }
    let key = match facts.run_key.map(str::trim).filter(|v| !v.is_empty()) {
        Some(raw) => match verdict(snapshot, raw) {
            KeyVerdict::Run(run, token_id) => Some((run, token_id)),
            KeyVerdict::Unknown => return Identified::Refused(Refusal::UnknownCredential),
            KeyVerdict::Revoked => return Identified::Refused(Refusal::CredentialRevoked),
        },
        None => None,
    };

    // 3–5. The route.
    enum Route {
        Run(RunId),
        Binding {
            name: String,
            labelled: bool,
            labels: Vec<(String, String)>,
        },
    }
    let (route, upstream, rest, via) =
        if let Some(after) = facts.path.strip_prefix(RUN_ROUTE_PREFIX) {
            let (id, rest) = match after.find('/') {
                Some(index) => (&after[..index], &after[index..]),
                None => (after, ""),
            };
            let run = RunId::from_wire(id);
            // The route's upstream is its process-owning Run's (a child's
            // route reads up the chain to the Run that recorded one), or the
            // pending route's before the launch job commits.
            match snapshot.route_upstream(&run) {
                Some(upstream) => (Route::Run(run), upstream, rest.to_string(), Via::RunRoute),
                None => return Identified::Refused(Refusal::NoRoute),
            }
        } else if facts.path == "/_hekma" || facts.path.starts_with("/_hekma/") {
            return Identified::Refused(Refusal::NoRoute);
        } else if let Some((binding, rest)) = snapshot.bindings.matching(facts.path) {
            (
                Route::Binding {
                    name: binding.name.clone(),
                    labelled: !binding.labels.is_empty(),
                    labels: binding.labels.clone(),
                },
                Arc::clone(&binding.upstream),
                rest.to_string(),
                Via::Binding(binding.name.clone()),
            )
        } else if let Some(legacy) = snapshot.legacy_root.clone() {
            match snapshot.route_upstream(&legacy) {
                Some(upstream) => (
                    Route::Run(legacy),
                    upstream,
                    facts.path.to_string(),
                    Via::LegacyRoot,
                ),
                None => return Identified::Refused(Refusal::NoRoute),
            }
        } else {
            return Identified::Refused(Refusal::NoRoute);
        };

    // The Run: the key's (scoped on a Run route), else the route's, else the
    // binding's own Run, else the unattributed Run.
    let token_id = key.as_ref().map(|(_, token_id)| token_id.clone());
    let base: Attribution = match (&key, &route) {
        (Some((key_run, _)), Route::Run(route_run)) => {
            if !snapshot.is_within(key_run, route_run) {
                return Identified::Refused(Refusal::RunOutOfScope);
            }
            Attribution::Run(key_run.clone())
        }
        (Some((key_run, _)), Route::Binding { .. }) => Attribution::Run(key_run.clone()),
        (None, Route::Run(route_run)) => Attribution::Run(route_run.clone()),
        (
            None,
            Route::Binding {
                name,
                labelled,
                labels,
            },
        ) => {
            let identity = if *labelled {
                WireIdentity::Binding {
                    binding: name.clone(),
                    labels: labels.clone(),
                }
            } else {
                WireIdentity::Unattributed
            };
            match snapshot.identities.get(&identity.key()) {
                Some(run) => Attribution::Run(run.clone()),
                None => Attribution::Ensure {
                    identity,
                    fallback: None,
                    fallback_identity: None,
                },
            }
        }
    };

    // The hint.
    let mut hint_ignored = false;
    let attribution = match facts.hint.map(str::trim).filter(|h| !h.is_empty()) {
        None => base,
        Some(hint) if !valid_external_id(hint) => {
            hint_ignored = true;
            base
        }
        Some(hint) => match (&key, &route, base) {
            // Beneath a Run Key or a Run route: a child of that Run.
            (Some(_), _, Attribution::Run(parent))
            | (None, Route::Run(_), Attribution::Run(parent)) => {
                let caller_labels = facts
                    .hint_labels
                    .and_then(parse_hint_labels)
                    .unwrap_or_default();
                let identity = WireIdentity::Child {
                    parent: parent.clone(),
                    external: hint.to_string(),
                    caller_labels,
                };
                match snapshot.identities.get(&identity.key()) {
                    Some(child) => Attribution::Run(child.clone()),
                    None => Attribution::Ensure {
                        identity,
                        fallback: Some(parent),
                        fallback_identity: None,
                    },
                }
            }
            // Beneath a labelled binding: a split Run with its labels.
            (
                None,
                Route::Binding {
                    name,
                    labelled: true,
                    labels,
                },
                base,
            ) => {
                let identity = WireIdentity::Split {
                    binding: name.clone(),
                    external: hint.to_string(),
                    labels: labels.clone(),
                };
                let (fallback, fallback_identity) = match base {
                    Attribution::Run(run) => (Some(run), None),
                    Attribution::Ensure { identity, .. } => (None, Some(identity)),
                };
                match snapshot.identities.get(&identity.key()) {
                    Some(split) => Attribution::Run(split.clone()),
                    None => Attribution::Ensure {
                        identity,
                        fallback,
                        fallback_identity,
                    },
                }
            }
            // Beneath the unattributed Run: ignored (anyone on loopback
            // could otherwise mint Runs without any Operator setup).
            (_, _, base) => {
                hint_ignored = true;
                base
            }
        },
    };
    Identified::Call {
        target: Target {
            upstream,
            rest,
            via,
            token_id,
        },
        attribution,
        hint_ignored,
    }
}
