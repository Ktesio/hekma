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
//! beneath the unattributed Run. Story 23-2: an `x-hekma-announce` header
//! claims the same namespaces — a child beneath a key or a route, else
//! `binding:<name>` beneath ANY binding (labelled or not: it mints a
//! credential, so it is never ignored) — and wins over a hint on the same
//! call (the hint is then counted as ignored). The upstream is always the route's or the
//! binding's — a Run Key never chooses where a call goes.

use std::sync::Arc;

use crate::domain::credential::{parse_presented, scan_url, TokenClass, TokenHash};
use crate::domain::custody::CustodyInfo;
use crate::domain::runs::announce::{AnnounceClaim, AnnounceScope};
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
    /// Story 23-2: the `x-hekma-announce` header.
    pub announce: Option<&'a str>,
    /// Story 22-5: the listener binds beyond loopback, so a proxy-access
    /// credential in the API-key slot is honoured, and a non-loopback
    /// peer must authenticate.
    pub beyond_loopback: bool,
    /// Story 22-5: the connection's peer is loopback (`127.0.0.0/8`,
    /// `::1`). Always true on a loopback-bound listener.
    pub peer_loopback: bool,
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
    /// Story 22-5: beyond loopback, a call from a non-loopback peer with
    /// no credential at all.
    AuthenticationRequired,
    /// Story 22-5: a valid proxy-access credential on another binding's
    /// prefix, or on a Run route.
    CredentialOutOfScope,
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
    /// The custody the call may draw on (story 22-2): present when the
    /// call is authenticated by a custody-eligible Run Key AND its
    /// attribution Run (or an ancestor) holds a key. The swap at forward
    /// time draws ONLY on this — a binding's Run reached with no
    /// credential, the unattributed Run and a header-announced Run never
    /// get one (they hold no eligible credential to begin with).
    pub custody: Option<CustodyInfo>,
    /// Story 22-5: the token id of the proxy-access credential that
    /// authenticated the call (beyond loopback). It never makes the call
    /// scented: only a Run Key does.
    pub proxy_access: Option<String>,
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
    /// Story 23-2: an announce to claim through the writer (renewing an
    /// open identity, or creating its Run and minting its pair); the
    /// fallbacks as for [`Attribution::Ensure`], when a cap is reached.
    Announce {
        claim: AnnounceClaim,
        fallback: Option<RunId>,
        fallback_identity: Option<WireIdentity>,
    },
}

/// The outcome of identification. Built once per proxied call, on the hot
/// path, so the size difference is accepted rather than boxing the common
/// variant (one heap allocation per call).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
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

/// Where a call stays when its split or announce cannot claim its own Run:
/// the base Run, or the identity the base would ensure.
fn fallback_of(base: Attribution) -> (Option<RunId>, Option<WireIdentity>) {
    match base {
        Attribution::Run(run) => (Some(run), None),
        Attribution::Ensure { identity, .. } => (None, Some(identity)),
        Attribution::Announce {
            fallback,
            fallback_identity,
            ..
        } => (fallback, fallback_identity),
    }
}

pub(crate) fn bearer(value: &str) -> Option<&str> {
    let value = value.trim();
    let (scheme, rest) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
}

fn looks_like_token(value: &str) -> bool {
    value.len() >= 3 && value.as_bytes()[..3].eq_ignore_ascii_case(b"hk_")
}

/// A presented token's verdict against the snapshot.
enum KeyVerdict {
    /// The key's Run, token id and custody eligibility (recorded at mint:
    /// supervised starts, credentialed opens and their descendants are
    /// eligible; the swap draws only on an eligible key).
    Run(RunId, String, bool),
    Unknown,
    Revoked,
}

/// `remote`: the call came beyond loopback from a non-loopback peer (story
/// 22-5) — a supervised Run's key is then unknown: a supervised agent is
/// local by definition, so its key from the network is a stolen key.
fn verdict(snapshot: &RunSnapshot, raw: &str, remote: bool) -> KeyVerdict {
    let Some((class, secret)) = parse_presented(raw.trim()) else {
        return KeyVerdict::Unknown;
    };
    let hash = TokenHash::of_token(secret.expose_secret());
    match snapshot.keys.get(&hash) {
        Some(entry) if remote && entry.process_owner => KeyVerdict::Unknown,
        Some(entry) if entry.class == TokenClass::RunKey && class == TokenClass::RunKey => {
            KeyVerdict::Run(
                entry.run_id.clone(),
                entry.token_id.clone(),
                entry.custody_eligible,
            )
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

    // 2. A presented Run Key; an `hk_` token in the API-key slot. Story
    // 22-2: an ELIGIBLE Run Key in the slot is custody's identification —
    // the call proceeds like an `x-hekma-run-key` call and the swap (or,
    // when its Run holds no key, the `custody_unavailable` refusal below)
    // is decided once the attribution Run is known. An INELIGIBLE key in
    // the slot keeps the refusal: the slot means "swap for me", and only
    // an eligible key ever may.
    let slot = facts
        .authorization
        .and_then(bearer)
        .filter(|value| looks_like_token(value))
        .or(facts.api_key.map(str::trim).filter(|v| looks_like_token(v)));
    // `(run, token id, custody-eligible)` of the key that identifies the
    // call.
    let remote = facts.beyond_loopback && !facts.peer_loopback;
    let mut custody_slot = false;
    // Story 22-5: beyond loopback, a proxy-access credential in the slot
    // authenticates the call for its own binding (`(binding, token id)`).
    let mut proxy_access: Option<(String, String)> = None;
    let proxy_access_slot = slot.filter(|raw| {
        facts.beyond_loopback
            && matches!(
                parse_presented(raw.trim()),
                Some((TokenClass::ProxyAccess, _))
            )
    });
    let slot = if let Some(raw) = proxy_access_slot {
        let Some((_, secret)) = parse_presented(raw.trim()) else {
            return Identified::Refused(Refusal::UnknownCredential);
        };
        match snapshot
            .bindings
            .proxy_access(&TokenHash::of_token(secret.expose_secret()))
        {
            Ok((binding, token_id)) => {
                proxy_access = Some((binding.to_string(), token_id.to_string()));
            }
            Err(true) => return Identified::Refused(Refusal::CredentialRevoked),
            Err(false) => return Identified::Refused(Refusal::UnknownCredential),
        }
        None
    } else {
        slot
    };
    let key = if let Some(raw) = slot {
        match verdict(snapshot, raw, remote) {
            KeyVerdict::Run(run, token_id, true) => {
                custody_slot = true;
                Some((run, token_id, true))
            }
            KeyVerdict::Run(..) => {
                return Identified::Refused(Refusal::CustodyUnavailable);
            }
            KeyVerdict::Unknown => return Identified::Refused(Refusal::UnknownCredential),
            KeyVerdict::Revoked => return Identified::Refused(Refusal::CredentialRevoked),
        }
    } else {
        None
    };
    let key = match facts.run_key.map(str::trim).filter(|v| !v.is_empty()) {
        Some(raw) => match verdict(snapshot, raw, remote) {
            KeyVerdict::Run(run, token_id, eligible) => Some((run, token_id, eligible)),
            KeyVerdict::Unknown => return Identified::Refused(Refusal::UnknownCredential),
            KeyVerdict::Revoked => return Identified::Refused(Refusal::CredentialRevoked),
        },
        None => key,
    };

    // Story 22-2: ONLY a custody-eligible Run Key (the slot above, or
    // `x-hekma-run-key`) draws on the custody its Run holds — decided HERE,
    // before the route, so a key in the API-key slot whose Run holds no key
    // keeps the same `custody_unavailable` answer whatever the path. A call
    // with no eligible credential NEVER draws custody (an ineligible key in
    // `x-hekma-run-key`, a labelled binding's Run reached bare, the
    // unattributed Run, a header-announced Run): it is forwarded with
    // whatever key the client sent.
    let mut custody = key
        .as_ref()
        .filter(|(_, _, eligible)| *eligible)
        .and_then(|(run, _, _)| snapshot.custody_of(run));
    if custody_slot && custody.is_none() {
        return Identified::Refused(Refusal::CustodyUnavailable);
    }
    let key = key.map(|(run, token_id, _)| (run, token_id));
    // Story 22-5: beyond loopback, a call from a non-loopback peer carries
    // a credential or is refused — before the route, the body or any dial.
    if remote && key.is_none() && proxy_access.is_none() {
        return Identified::Refused(Refusal::AuthenticationRequired);
    }

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

    // Story 22-5: a proxy-access credential is scoped to its own binding's
    // prefix (never a Run route, never another binding) — checked even when
    // a Run Key on the same call identifies it (a presented credential is
    // never silently ignored). Without a Run Key it draws its binding's
    // custodied key, under the swap's rules.
    if let Some((binding, _)) = &proxy_access {
        match &route {
            Route::Binding { name, .. } if name == binding => {}
            _ => return Identified::Refused(Refusal::CredentialOutOfScope),
        }
        if key.is_none() {
            custody = snapshot.bindings.by_name(binding).and_then(|entry| {
                Some(crate::domain::custody::CustodyInfo {
                    key_ref: Arc::from(entry.custodied_key.as_deref()?),
                    origin: Arc::clone(&entry.upstream),
                })
            });
        }
    }
    let proxy_access = proxy_access.map(|(_, token_id)| token_id);

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

    // Story 23-2: the announce, before the hint (it wins; a hint on the
    // same call is counted as ignored). A malformed external id is ignored
    // like a malformed hint. A key or a Run route always resolves to a Run,
    // so every well-formed announce has a scope.
    let announce = facts.announce.map(str::trim).filter(|a| !a.is_empty());
    let announce_ignored = announce.is_some_and(|a| !valid_external_id(a));
    let scope =
        announce
            .filter(|a| valid_external_id(a))
            .and_then(|_| match (&key, &route, &base) {
                (Some(_), _, Attribution::Run(parent))
                | (None, Route::Run(_), Attribution::Run(parent)) => {
                    Some(AnnounceScope::Parent(parent.clone()))
                }
                (None, Route::Binding { name, .. }, _) => {
                    Some(AnnounceScope::Binding(name.clone()))
                }
                _ => None,
            });
    if let (Some(scope), Some(external)) = (scope, announce) {
        let caller_labels = facts
            .hint_labels
            .and_then(parse_hint_labels)
            .unwrap_or_default();
        let (fallback, fallback_identity) = fallback_of(base);
        return Identified::Call {
            target: Target {
                upstream,
                rest,
                via,
                token_id,
                custody,
                proxy_access,
            },
            attribution: Attribution::Announce {
                claim: AnnounceClaim {
                    scope,
                    external: external.to_string(),
                    caller_labels,
                    credentialed: false,
                    created_by: None,
                    issuer: "announce".to_string(),
                    wire: true,
                    telemetry: false,
                },
                fallback,
                fallback_identity,
            },
            hint_ignored: facts.hint.is_some_and(|hint| !hint.trim().is_empty()),
        };
    }

    // The hint.
    let mut hint_ignored = announce_ignored;
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
                let (fallback, fallback_identity) = fallback_of(base);
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
            custody,
            proxy_access,
        },
        attribution,
        hint_ignored,
    }
}
