//! The proxy tests (story 23-1): identification as one table over a
//! hand-built snapshot (pure, no I/O).

use std::collections::HashMap;
use std::sync::Arc;

use super::identify::{identify, Attribution, CallFacts, Identified, Refusal, Target, Via};
use crate::domain::credential::{mint, TokenClass, TokenHash};
use crate::domain::runs::snapshot::{
    BindingEntry, BindingTable, Disposition, KeyEntry, RevokedSet, RunEntry, RunSnapshot,
};
use crate::domain::runs::wire::WireIdentity;
use crate::domain::runs::{ControlDepth, RunLimits, RunRecord, RunStatus};
use crate::domain::RunId;

const UPSTREAM: &str = "http://127.0.0.1:9/v1";
const BINDING_UPSTREAM: &str = "https://api.example.com/v1";
const CATCH_ALL_UPSTREAM: &str = "https://fallback.example.com";

fn run_entry(id: &str, parent: Option<&str>, upstream: Option<&str>) -> RunEntry {
    RunEntry {
        record: RunRecord {
            run_id: RunId::from_wire(id),
            control_depth: ControlDepth::Supervised,
            wire_path: true,
            status: RunStatus::Active,
            anomalies: Vec::new(),
            parent_run_id: parent.map(RunId::from_wire),
            instance: Some("agent".to_string()),
            process_owner: parent.is_none(),
            end_cause: None,
            created_at: "t".into(),
            updated_at: "t".into(),
            ended_at: None,
        },
        labels: Vec::new(),
        limits: RunLimits::default(),
        policy: None,
        disposition: Disposition::default(),
        upstream: upstream.map(Arc::from),
        custody: None,
        identity: None,
    }
}

/// The world every row runs against:
/// - `run_P` (a supervised agent with an upstream) and its child `run_C`;
/// - `run_O`, another agent's Run;
/// - a Run Key for `run_C`, one for `run_O`, a recently revoked one, a
///   refresh credential and a key nobody issued;
/// - bindings `/openai` (labelled) and `/plain` (unlabelled);
/// - the labelled binding's own Run `run_B`, and one open split `run_S`.
struct World {
    snapshot: RunSnapshot,
    child_key: String,
    other_key: String,
    revoked_key: String,
    refresh: String,
    unknown_key: String,
}

fn token(class: TokenClass) -> (String, TokenHash) {
    let minted = mint(class).unwrap();
    let hash = minted.stub().hash;
    (minted.into_secret().expose_secret().to_string(), hash)
}

fn key_entry(run: &str, class: TokenClass) -> Arc<KeyEntry> {
    Arc::new(KeyEntry {
        class,
        run_id: RunId::from_wire(run),
        token_id: format!("tid-{run}"),
        launch_generation: None,
        custody_eligible: false,
        process_owner: false,
    })
}

fn labels() -> Vec<(String, String)> {
    vec![("team".to_string(), "core".to_string())]
}

fn openai() -> BindingEntry {
    BindingEntry {
        name: "openai".into(),
        prefix: "/openai".into(),
        upstream: Arc::from(BINDING_UPSTREAM),
        labels: labels(),
        custodied_key: None,
        identity_headers: None,
    }
}

fn world() -> World {
    let mut runs = HashMap::new();
    for entry in [
        run_entry("run_P", None, Some(UPSTREAM)),
        run_entry("run_C", Some("run_P"), None),
        run_entry("run_O", None, Some("http://127.0.0.1:10/v1")),
        run_entry("run_B", None, None),
        run_entry("run_S", None, None),
    ] {
        runs.insert(entry.record.run_id.clone(), Arc::new(entry));
    }
    let (child_key, child_hash) = token(TokenClass::RunKey);
    let (other_key, other_hash) = token(TokenClass::RunKey);
    let (revoked_key, revoked_hash) = token(TokenClass::RunKey);
    let (refresh, refresh_hash) = token(TokenClass::Refresh);
    let (unknown_key, _) = token(TokenClass::RunKey);
    let mut keys = HashMap::new();
    keys.insert(child_hash, key_entry("run_C", TokenClass::RunKey));
    keys.insert(other_hash, key_entry("run_O", TokenClass::RunKey));
    // A refresh credential in the index is still never a Run identity.
    keys.insert(refresh_hash, key_entry("run_C", TokenClass::Refresh));
    let mut revoked = RevokedSet::default();
    revoked.insert(revoked_hash, std::time::Instant::now());
    let bindings = BindingTable::new(vec![
        openai(),
        BindingEntry {
            name: "plain".into(),
            prefix: "/plain".into(),
            upstream: Arc::from(BINDING_UPSTREAM),
            labels: Vec::new(),
            custodied_key: None,
            identity_headers: None,
        },
    ]);
    let mut identities = HashMap::new();
    identities.insert(
        WireIdentity::Binding {
            binding: "openai".into(),
            labels: labels(),
        }
        .key(),
        RunId::from_wire("run_B"),
    );
    identities.insert(
        WireIdentity::Split {
            binding: "openai".into(),
            external: "seen".into(),
            labels: labels(),
        }
        .key(),
        RunId::from_wire("run_S"),
    );
    World {
        snapshot: RunSnapshot {
            runs: runs.into_iter().collect(),
            keys: keys.into_iter().collect(),
            revoked: Arc::new(revoked),
            bindings: Arc::new(bindings),
            identities: identities.into_iter().collect(),
            ..RunSnapshot::default()
        },
        child_key,
        other_key,
        revoked_key,
        refresh,
        unknown_key,
    }
}

fn facts(path: &str) -> CallFacts<'_> {
    CallFacts {
        path,
        ..CallFacts::default()
    }
}

fn refused(identified: &Identified) -> Option<Refusal> {
    match identified {
        Identified::Refused(refusal) => Some(*refusal),
        Identified::Call { .. } => None,
    }
}

fn call(identified: Identified) -> (Target, Attribution, bool) {
    match identified {
        Identified::Call {
            target,
            attribution,
            hint_ignored,
        } => (target, attribution, hint_ignored),
        Identified::Refused(refusal) => panic!("refused: {refusal:?}"),
    }
}

fn run(id: &str) -> Attribution {
    Attribution::Run(RunId::from_wire(id))
}

#[test]
fn a_credential_in_the_url_is_refused_before_anything_else() {
    let w = world();
    let encoded = w.child_key.replace('_', "%5F");
    for (path, query) in [
        (format!("/openai/{}", w.child_key), None),
        (
            "/openai/v1".to_string(),
            Some(format!("key={}", w.child_key)),
        ),
        ("/openai/v1".to_string(), Some(format!("key={encoded}"))),
        (format!("/_hekma/runs/run_P/{}", w.refresh), None),
        // Even an unknown token, and even on a path that would not route.
        (format!("/nowhere/{}", w.unknown_key), None),
    ] {
        let f = CallFacts {
            path: &path,
            query: query.as_deref(),
            // A valid key in the header does not rescue it.
            run_key: Some(&w.child_key),
            ..CallFacts::default()
        };
        assert_eq!(
            refused(&identify(&w.snapshot, &f)),
            Some(Refusal::CredentialInUrl),
            "{path} {query:?}"
        );
    }
}

#[test]
fn a_token_in_the_api_key_slot_is_never_forwarded() {
    let w = world();
    let bearer_child = format!("Bearer {}", w.child_key);
    let bearer_unknown = format!("bearer {}", w.unknown_key);
    let bearer_revoked = format!("BEARER {}", w.revoked_key);
    for (authorization, api_key, expected) in [
        // A live Run Key: it names the Run, but custody is not here yet.
        (
            Some(bearer_child.as_str()),
            None,
            Refusal::CustodyUnavailable,
        ),
        (
            None,
            Some(w.child_key.as_str()),
            Refusal::CustodyUnavailable,
        ),
        (
            Some(bearer_unknown.as_str()),
            None,
            Refusal::UnknownCredential,
        ),
        (
            Some(bearer_revoked.as_str()),
            None,
            Refusal::CredentialRevoked,
        ),
        (None, Some(w.refresh.as_str()), Refusal::UnknownCredential),
        // A malformed `hk_` value is unknown, not a provider key.
        (None, Some("hk_run_short"), Refusal::UnknownCredential),
    ] {
        let f = CallFacts {
            path: "/openai/v1/chat",
            authorization,
            api_key,
            ..CallFacts::default()
        };
        assert_eq!(
            refused(&identify(&w.snapshot, &f)),
            Some(expected),
            "{authorization:?} {api_key:?}"
        );
    }
    // A provider key in the slot is not a Hekma credential at all.
    let f = CallFacts {
        path: "/openai/v1/chat",
        authorization: Some("Bearer sk-provider-key"),
        api_key: Some("sk-ant-provider"),
        ..CallFacts::default()
    };
    assert!(refused(&identify(&w.snapshot, &f)).is_none());
}

#[test]
fn a_presented_run_key_names_the_run_and_is_scoped_on_a_run_route() {
    let w = world();
    // Unknown, revoked and refresh keys in the Run Key header.
    for (key, expected) in [
        (w.unknown_key.as_str(), Refusal::UnknownCredential),
        (w.revoked_key.as_str(), Refusal::CredentialRevoked),
        (w.refresh.as_str(), Refusal::UnknownCredential),
        ("not-a-token", Refusal::UnknownCredential),
    ] {
        let f = CallFacts {
            run_key: Some(key),
            ..facts("/openai/v1")
        };
        assert_eq!(refused(&identify(&w.snapshot, &f)), Some(expected), "{key}");
    }
    // The child's key on its parent's Run route: attributed to the child,
    // forwarded to the parent's upstream, the prefix stripped.
    let f = CallFacts {
        run_key: Some(&w.child_key),
        ..facts("/_hekma/runs/run_P/chat/completions")
    };
    let (target, attribution, _) = call(identify(&w.snapshot, &f));
    assert_eq!(attribution, run("run_C"));
    assert_eq!(&*target.upstream, UPSTREAM);
    assert_eq!(target.rest, "/chat/completions");
    assert_eq!(target.via, Via::RunRoute);
    assert_eq!(target.token_id.as_deref(), Some("tid-run_C"));
    // Another Run's key on that route is out of scope.
    let f = CallFacts {
        run_key: Some(&w.other_key),
        ..facts("/_hekma/runs/run_P/chat")
    };
    assert_eq!(
        refused(&identify(&w.snapshot, &f)),
        Some(Refusal::RunOutOfScope)
    );
    // The child's key on the child's own route: the child, whose upstream
    // is read up the chain from its parent.
    let f = CallFacts {
        run_key: Some(&w.child_key),
        ..facts("/_hekma/runs/run_C")
    };
    let (target, attribution, _) = call(identify(&w.snapshot, &f));
    assert_eq!(attribution, run("run_C"));
    assert_eq!(&*target.upstream, UPSTREAM);
    assert_eq!(target.rest, "");
    // A key on a binding: the key's Run, the binding's upstream (a key
    // never chooses where a call goes).
    let f = CallFacts {
        run_key: Some(&w.other_key),
        ..facts("/openai/v1/chat")
    };
    let (target, attribution, _) = call(identify(&w.snapshot, &f));
    assert_eq!(attribution, run("run_O"));
    assert_eq!(&*target.upstream, BINDING_UPSTREAM);
    assert_eq!(target.via, Via::Binding("openai".into()));
}

#[test]
fn routes_match_on_segment_boundaries_and_the_reserved_prefix_never_routes() {
    let w = world();
    // The Run route without a key: the route's Run.
    let (target, attribution, _) = call(identify(&w.snapshot, &facts("/_hekma/runs/run_P/x")));
    assert_eq!(attribution, run("run_P"));
    assert_eq!(target.rest, "/x");
    for path in [
        // A Run route naming no open Run.
        "/_hekma/runs/run_GONE/chat",
        // The reserved prefix outside a Run route.
        "/_hekma",
        "/_hekma/other",
        // Not a segment boundary, and nothing else matches.
        "/openai2/v1",
        "/",
        "/elsewhere",
    ] {
        assert_eq!(
            refused(&identify(&w.snapshot, &facts(path))),
            Some(Refusal::NoRoute),
            "{path}"
        );
    }
    // `/openai` itself and beneath it.
    for (path, rest) in [("/openai", ""), ("/openai/v1/chat", "/v1/chat")] {
        let (target, _, _) = call(identify(&w.snapshot, &facts(path)));
        assert_eq!(target.rest, rest, "{path}");
    }
}

#[test]
fn bindings_attribute_to_their_run_or_the_unattributed_run() {
    let w = world();
    // The labelled binding's open Run.
    let (_, attribution, _) = call(identify(&w.snapshot, &facts("/openai/v1")));
    assert_eq!(attribution, run("run_B"));
    // An unlabelled binding: the unattributed Run, ensured (none open).
    let (target, attribution, _) = call(identify(&w.snapshot, &facts("/plain/v1")));
    assert_eq!(target.via, Via::Binding("plain".into()));
    assert_eq!(
        attribution,
        Attribution::Ensure {
            identity: WireIdentity::Unattributed,
            fallback: None,
            fallback_identity: None,
        }
    );
    // A labelled binding whose Run is not open yet is ensured too.
    let mut fresh = world();
    fresh.snapshot.identities.clear();
    let (_, attribution, _) = call(identify(&fresh.snapshot, &facts("/openai/v1")));
    assert_eq!(
        attribution,
        Attribution::Ensure {
            identity: WireIdentity::Binding {
                binding: "openai".into(),
                labels: labels(),
            },
            fallback: None,
            fallback_identity: None,
        }
    );
}

#[test]
fn hints_name_children_split_bindings_and_are_ignored_beneath_the_unattributed_run() {
    let w = world();
    // Beneath a Run route: a child of the route's Run, with Caller labels.
    let f = CallFacts {
        hint: Some("task-1"),
        hint_labels: Some("step=plan"),
        ..facts("/_hekma/runs/run_P/chat")
    };
    let (_, attribution, ignored) = call(identify(&w.snapshot, &f));
    assert!(!ignored);
    assert_eq!(
        attribution,
        Attribution::Ensure {
            identity: WireIdentity::Child {
                parent: RunId::from_wire("run_P"),
                external: "task-1".into(),
                caller_labels: vec![("step".into(), "plan".into())],
            },
            fallback: Some(RunId::from_wire("run_P")),
            fallback_identity: None,
        }
    );
    // Beneath a Run Key: a child of the key's Run.
    let f = CallFacts {
        run_key: Some(&w.other_key),
        hint: Some("task-2"),
        ..facts("/openai/v1")
    };
    let (_, attribution, _) = call(identify(&w.snapshot, &f));
    let Attribution::Ensure {
        identity: WireIdentity::Child { parent, .. },
        ..
    } = attribution
    else {
        panic!("a child: {attribution:?}");
    };
    assert_eq!(parent.as_str(), "run_O");
    // Beneath a labelled binding: a split, an existing one reused.
    let f = CallFacts {
        hint: Some("seen"),
        ..facts("/openai/v1")
    };
    assert_eq!(call(identify(&w.snapshot, &f)).1, run("run_S"));
    let f = CallFacts {
        hint: Some("new-split"),
        ..facts("/openai/v1")
    };
    assert_eq!(
        call(identify(&w.snapshot, &f)).1,
        Attribution::Ensure {
            identity: WireIdentity::Split {
                binding: "openai".into(),
                external: "new-split".into(),
                labels: labels(),
            },
            fallback: Some(RunId::from_wire("run_B")),
            fallback_identity: None,
        }
    );
    // A split beneath a binding whose own Run is not open falls back to
    // ensuring that Run.
    let mut fresh = world();
    fresh.snapshot.identities.clear();
    let (_, attribution, _) = call(identify(&fresh.snapshot, &f));
    let Attribution::Ensure {
        fallback,
        fallback_identity,
        ..
    } = attribution
    else {
        panic!("ensured: {attribution:?}");
    };
    assert_eq!(fallback, None);
    assert_eq!(
        fallback_identity,
        Some(WireIdentity::Binding {
            binding: "openai".into(),
            labels: labels(),
        })
    );
    // Beneath the unattributed Run: ignored and counted.
    let f = CallFacts {
        hint: Some("anyone"),
        ..facts("/plain/v1")
    };
    let (_, attribution, ignored) = call(identify(&w.snapshot, &f));
    assert!(ignored);
    assert!(matches!(
        attribution,
        Attribution::Ensure {
            identity: WireIdentity::Unattributed,
            ..
        }
    ));
    // A malformed hint (too long, or not visible ASCII) is ignored too.
    let long = "x".repeat(129);
    for bad in [long.as_str(), "has space", "tab\there"] {
        let f = CallFacts {
            hint: Some(bad),
            ..facts("/_hekma/runs/run_P/chat")
        };
        let (_, attribution, ignored) = call(identify(&w.snapshot, &f));
        assert!(ignored, "{bad}");
        assert_eq!(attribution, run("run_P"), "{bad}");
    }
    // A blank hint is no hint.
    let f = CallFacts {
        hint: Some("  "),
        ..facts("/_hekma/runs/run_P/chat")
    };
    let (_, attribution, ignored) = call(identify(&w.snapshot, &f));
    assert!(!ignored);
    assert_eq!(attribution, run("run_P"));
}

/// Story 23-2: an `x-hekma-announce` claims the hint namespaces — a child
/// beneath a key or a route, `binding:<name>` beneath ANY binding (never
/// ignored, unlike a hint beneath the unattributed Run) — wins over a hint
/// on the same call, never draws custody, and a malformed one is ignored.
#[test]
fn an_announce_claims_the_hint_namespaces_and_wins_over_a_hint() {
    use crate::domain::runs::announce::{AnnounceClaim, AnnounceScope};
    let w = world();
    let claim =
        |scope: AnnounceScope, external: &str, labels: Vec<(String, String)>| AnnounceClaim {
            scope,
            external: external.into(),
            caller_labels: labels,
            credentialed: false,
            created_by: None,
            issuer: "announce".into(),
            wire: true,
            telemetry: false,
        };
    // Beneath a labelled binding, with Caller labels: `binding:openai`,
    // falling back to the binding's own Run.
    let f = CallFacts {
        announce: Some("job-7"),
        hint_labels: Some("step=plan"),
        ..facts("/openai/v1")
    };
    let (target, attribution, ignored) = call(identify(&w.snapshot, &f));
    assert!(!ignored);
    assert!(target.custody.is_none() && target.token_id.is_none());
    assert_eq!(
        attribution,
        Attribution::Announce {
            claim: claim(
                AnnounceScope::Binding("openai".into()),
                "job-7",
                vec![("step".into(), "plan".into())]
            ),
            fallback: Some(RunId::from_wire("run_B")),
            fallback_identity: None,
        }
    );
    // Beneath an unlabelled binding: still its own `binding:plain` claim,
    // falling back to the unattributed Run.
    let f = CallFacts {
        announce: Some("job-8"),
        ..facts("/plain/v1")
    };
    let (_, attribution, ignored) = call(identify(&w.snapshot, &f));
    assert!(!ignored);
    assert_eq!(
        attribution,
        Attribution::Announce {
            claim: claim(AnnounceScope::Binding("plain".into()), "job-8", Vec::new()),
            fallback: None,
            fallback_identity: Some(WireIdentity::Unattributed),
        }
    );
    // Beneath a Run route and beneath a Run Key: a child.
    let f = CallFacts {
        announce: Some("task"),
        ..facts("/_hekma/runs/run_P/chat")
    };
    let (_, attribution, _) = call(identify(&w.snapshot, &f));
    assert_eq!(
        attribution,
        Attribution::Announce {
            claim: claim(
                AnnounceScope::Parent(RunId::from_wire("run_P")),
                "task",
                Vec::new()
            ),
            fallback: Some(RunId::from_wire("run_P")),
            fallback_identity: None,
        }
    );
    let f = CallFacts {
        run_key: Some(&w.other_key),
        announce: Some("task"),
        ..facts("/openai/v1")
    };
    let (target, attribution, _) = call(identify(&w.snapshot, &f));
    assert!(target.token_id.is_some());
    let Attribution::Announce { claim: c, .. } = attribution else {
        panic!("an announce: {attribution:?}");
    };
    assert_eq!(c.scope, AnnounceScope::Parent(RunId::from_wire("run_O")));
    // An announce and a hint: the announce wins, the hint is counted.
    let f = CallFacts {
        announce: Some("job-7"),
        hint: Some("seen"),
        ..facts("/openai/v1")
    };
    let (_, attribution, ignored) = call(identify(&w.snapshot, &f));
    assert!(ignored, "the hint is counted as ignored");
    assert!(matches!(attribution, Attribution::Announce { .. }));
    // A malformed announce is ignored (counted); a blank one is none.
    for bad in ["has space", &"x".repeat(129)] {
        let f = CallFacts {
            announce: Some(bad),
            ..facts("/openai/v1")
        };
        let (_, attribution, ignored) = call(identify(&w.snapshot, &f));
        assert!(ignored, "{bad}");
        assert_eq!(attribution, run("run_B"));
    }
    let f = CallFacts {
        announce: Some("  "),
        ..facts("/openai/v1")
    };
    let (_, attribution, ignored) = call(identify(&w.snapshot, &f));
    assert!(!ignored);
    assert_eq!(attribution, run("run_B"));
    // A revoked key with an announce: the refusal wins, nothing is claimed.
    let f = CallFacts {
        run_key: Some(&w.revoked_key),
        announce: Some("job-7"),
        ..facts("/openai/v1")
    };
    assert_eq!(
        refused(&identify(&w.snapshot, &f)),
        Some(Refusal::CredentialRevoked)
    );
}

/// Story 22-2 (P4): custody is drawn ONLY by a custody-eligible Run Key —
/// in the API-key slot or in `x-hekma-run-key` — whose Run (or an
/// ancestor, or its binding) holds a key; everything else is forwarded as
/// 23-1 does, or refused `custody_unavailable` when the slot asked for it.
#[test]
fn only_an_eligible_run_key_draws_custody_and_a_binding_key_needs_one_too() {
    use crate::domain::custody::CustodyInfo;
    let mut w = world();
    let held = CustodyInfo {
        key_ref: Arc::from("secret:OPENAI"),
        origin: Arc::from("https://api.openai.com"),
    };
    // run_P holds a key (its child run_C inherits it); run_O holds none.
    let mut parent = (*w.snapshot.runs[&RunId::from_wire("run_P")]).clone();
    parent.custody = Some(held.clone());
    w.snapshot
        .runs
        .insert(RunId::from_wire("run_P"), Arc::new(parent));
    // A binding with a custodied key, its Run run_V and a child run_VC.
    let vault = BindingEntry {
        name: "vault".into(),
        prefix: "/vault".into(),
        upstream: Arc::from("https://api.anthropic.com"),
        labels: labels(),
        custodied_key: Some("secret:VAULT".into()),
        identity_headers: None,
    };
    let mut entries = vec![openai(), vault.clone()];
    entries.push(BindingEntry {
        name: "plain".into(),
        prefix: "/plain".into(),
        upstream: Arc::from(BINDING_UPSTREAM),
        labels: Vec::new(),
        custodied_key: None,
        identity_headers: None,
    });
    w.snapshot.bindings = Arc::new(BindingTable::new(entries));
    let mut binding_run = run_entry("run_V", None, None);
    binding_run.identity = Some(
        WireIdentity::Binding {
            binding: "vault".into(),
            labels: labels(),
        }
        .key(),
    );
    w.snapshot
        .runs
        .insert(RunId::from_wire("run_V"), Arc::new(binding_run));
    w.snapshot.identities.insert(
        WireIdentity::Binding {
            binding: "vault".into(),
            labels: labels(),
        }
        .key(),
        RunId::from_wire("run_V"),
    );
    w.snapshot.runs.insert(
        RunId::from_wire("run_VC"),
        Arc::new(run_entry("run_VC", Some("run_V"), None)),
    );
    let mut key = |run: &str, eligible: bool| {
        let (value, hash) = token(TokenClass::RunKey);
        let mut entry = (*key_entry(run, TokenClass::RunKey)).clone();
        entry.custody_eligible = eligible;
        w.snapshot.keys.insert(hash, Arc::new(entry));
        value
    };
    let child_eligible = key("run_C", true);
    let child_ineligible = key("run_C", false);
    let other_eligible = key("run_O", true);
    let vault_child_eligible = key("run_VC", true);
    let vault_ineligible = key("run_V", false);
    let snapshot = &w.snapshot;
    let vault_held = CustodyInfo {
        key_ref: Arc::from("secret:VAULT"),
        origin: Arc::from("https://api.anthropic.com"),
    };

    let custody_of = |f: CallFacts<'_>| call(identify(snapshot, &f)).0.custody;
    let refusal = |f: CallFacts<'_>| refused(&identify(snapshot, &f));
    let run_route = "/_hekma/runs/run_P/v1/chat/completions";
    let bearer = |key: &str| format!("Bearer {key}");

    // An eligible key draws its chain's custody, in either place.
    let slot = bearer(&child_eligible);
    assert_eq!(
        custody_of(CallFacts {
            path: run_route,
            authorization: Some(&slot),
            ..CallFacts::default()
        }),
        Some(held.clone())
    );
    assert_eq!(
        custody_of(CallFacts {
            path: run_route,
            run_key: Some(&child_eligible),
            ..CallFacts::default()
        }),
        Some(held.clone())
    );
    // An INELIGIBLE key in x-hekma-run-key names the Run but never draws
    // custody: the call is forwarded with whatever key the client sent.
    assert_eq!(
        custody_of(CallFacts {
            path: run_route,
            run_key: Some(&child_ineligible),
            ..CallFacts::default()
        }),
        None
    );
    // In the slot it is refused (the slot asks for a swap).
    let slot = bearer(&child_ineligible);
    assert_eq!(
        refusal(CallFacts {
            path: run_route,
            authorization: Some(&slot),
            ..CallFacts::default()
        }),
        Some(Refusal::CustodyUnavailable)
    );
    // An eligible slot key overridden by an ineligible x-hekma-run-key key:
    // no custody, so the slot's request for one is refused.
    let slot = bearer(&child_eligible);
    assert_eq!(
        refusal(CallFacts {
            path: run_route,
            authorization: Some(&slot),
            run_key: Some(&child_ineligible),
            ..CallFacts::default()
        }),
        Some(Refusal::CustodyUnavailable)
    );
    // An eligible key whose Run holds no key: refused in the slot, plain
    // in x-hekma-run-key.
    let slot = bearer(&other_eligible);
    assert_eq!(
        refusal(CallFacts {
            path: "/_hekma/runs/run_O/v1/chat",
            api_key: Some(slot.trim_start_matches("Bearer ")),
            ..CallFacts::default()
        }),
        Some(Refusal::CustodyUnavailable)
    );
    assert_eq!(
        custody_of(CallFacts {
            path: "/_hekma/runs/run_O/v1/chat",
            run_key: Some(&other_eligible),
            ..CallFacts::default()
        }),
        None
    );
    // A labelled binding with a custodied key: reached bare, never; by an
    // ineligible key of its Run, never; by an eligible key of a Run
    // beneath it, its key pinned to the binding's upstream.
    assert_eq!(custody_of(facts("/vault/v1/messages")), None);
    assert_eq!(
        custody_of(CallFacts {
            path: "/vault/v1/messages",
            run_key: Some(&vault_ineligible),
            ..CallFacts::default()
        }),
        None
    );
    assert_eq!(
        custody_of(CallFacts {
            path: "/vault/v1/messages",
            run_key: Some(&vault_child_eligible),
            ..CallFacts::default()
        }),
        Some(vault_held)
    );
    // The unattributed Run never draws custody.
    assert_eq!(custody_of(facts("/plain/v1/chat")), None);
}

#[test]
fn the_legacy_root_route_catches_what_no_binding_or_run_route_claims() {
    let mut w = world();
    w.snapshot.legacy_root = Some(RunId::from_wire("run_P"));
    let (target, attribution, _) = call(identify(&w.snapshot, &facts("/v1/chat/completions")));
    assert_eq!(attribution, run("run_P"));
    assert_eq!(target.via, Via::LegacyRoot);
    assert_eq!(target.rest, "/v1/chat/completions");
    assert_eq!(&*target.upstream, UPSTREAM);
    // A binding still wins over it.
    let (target, _, _) = call(identify(&w.snapshot, &facts("/openai/v1")));
    assert_eq!(target.via, Via::Binding("openai".into()));
    // A legacy Run with no recorded upstream does not route.
    w.snapshot.legacy_root = Some(RunId::from_wire("run_B"));
    assert_eq!(
        refused(&identify(&w.snapshot, &facts("/v1/chat"))),
        Some(Refusal::NoRoute)
    );
    // A `/` binding is the catch-all instead, and the longest prefix wins.
    let mut w = world();
    w.snapshot.bindings = Arc::new(BindingTable::new(vec![
        BindingEntry {
            name: "all".into(),
            prefix: "/".into(),
            upstream: Arc::from(CATCH_ALL_UPSTREAM),
            labels: Vec::new(),
            custodied_key: None,
            identity_headers: None,
        },
        openai(),
    ]));
    let (target, _, _) = call(identify(&w.snapshot, &facts("/v1/chat")));
    assert_eq!(target.via, Via::Binding("all".into()));
    assert_eq!(target.rest, "/v1/chat");
    let (target, _, _) = call(identify(&w.snapshot, &facts("/openai/v1")));
    assert_eq!(target.via, Via::Binding("openai".into()));
}

// ---- The failure seams, end to end inside the crate ----

mod seams {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::{BindingSpec, Engine, EngineOptions, ListRunsQuery, ListingView};

    const COMPLETION: &str =
        r#"{"model":"gpt-demo","usage":{"prompt_tokens":10,"completion_tokens":5}}"#;

    /// An upstream answering every request with [`COMPLETION`], counting.
    fn upstream() -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&served);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let count = Arc::clone(&count);
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 8192];
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.read(&mut buffer);
                    count.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{COMPLETION}",
                            COMPLETION.len()
                        )
                        .as_bytes(),
                    );
                });
            }
        });
        (format!("http://127.0.0.1:{port}/v1"), served)
    }

    /// POST a small chat body; (status, body).
    fn post(port: u16, path: &str, extra: &str) -> (u16, String) {
        let body = r#"{"model":"gpt-demo"}"#;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n{extra}\
                     content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw);
        let status = raw
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let body = raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string());
        (status, body.unwrap_or_default())
    }

    pub(super) fn engine_with_binding(
        labels: &[(&str, &str)],
    ) -> (tempfile::TempDir, Engine, u16, Arc<AtomicUsize>) {
        let state = tempfile::tempdir().unwrap();
        let engine =
            Engine::open_with_options(Some(state.path().to_path_buf()), EngineOptions::default())
                .unwrap();
        let port = engine.metering_proxy().unwrap().port;
        let (base, served) = upstream();
        engine
            .blocking()
            .create_binding(BindingSpec {
                name: "gw".into(),
                prefix: "/gw".into(),
                upstream: base,
                labels: labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                custodied_key: None,
                identity_headers: None,
            })
            .unwrap();
        (state, engine, port, served)
    }

    fn input_tokens(engine: &Engine) -> u64 {
        engine
            .blocking()
            .list_runs(ListRunsQuery {
                view: ListingView::All,
                limit: 0,
                ..ListRunsQuery::default()
            })
            .unwrap()
            .items
            .iter()
            .map(|run| run.usage.input_tokens)
            .sum()
    }

    #[test]
    fn a_run_not_created_within_the_ack_bound_is_a_retryable_refusal_never_forwarded() {
        // The writer's bound is on a job being TAKEN: one slow creation
        // holds the writer, so a second creation queued behind it is not
        // taken within the bound and its call is refused, never forwarded.
        let (_state, engine, port, served) = engine_with_binding(&[("team", "core")]);
        let shared = engine.proxy_shared_for_tests().unwrap();
        *shared.ensure_delay.lock().unwrap() = Some(Duration::from_millis(2_500));
        let slow = std::thread::spawn(move || post(port, "/gw/chat", "x-hekma-run: slow\r\n"));
        std::thread::sleep(Duration::from_millis(300));
        let (status, body) = post(port, "/gw/chat", "x-hekma-run: queued\r\n");
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("hekma_run_unavailable"), "{body}");
        // The slow creation itself completes and is served.
        assert_eq!(slow.join().unwrap().0, 200);
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "the refused call was forwarded"
        );
        *shared.ensure_delay.lock().unwrap() = None;
        assert_eq!(post(port, "/gw/chat", "x-hekma-run: queued\r\n").0, 200);
    }

    #[test]
    fn no_free_record_permit_refuses_before_any_dial() {
        let (_state, engine, port, served) = engine_with_binding(&[("team", "core")]);
        let shared = engine.proxy_shared_for_tests().unwrap();
        // Open the binding's Run first (Run creation is not the subject).
        assert_eq!(post(port, "/gw/chat", "").0, 200);
        let before = served.load(Ordering::SeqCst);
        let held =
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(Arc::clone(&shared.pump.permits).acquire_many_owned(
                    u32::try_from(super::super::records::RECORD_PERMITS).unwrap(),
                ))
                .unwrap();
        let started = Instant::now();
        let (status, body) = post(port, "/gw/chat", "");
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("hekma_ledger_unavailable"), "{body}");
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert_eq!(served.load(Ordering::SeqCst), before);
        drop(held);
        assert_eq!(post(port, "/gw/chat", "").0, 200);
    }

    #[test]
    fn a_failing_close_job_is_retried_then_skipped_loudly_never_double_counted() {
        let (_state, engine, port, _served) = engine_with_binding(&[("team", "core")]);
        let shared = engine.proxy_shared_for_tests().unwrap();
        // Three failures, then success: counted exactly once.
        shared.pump.fail_closes.store(3, Ordering::SeqCst);
        assert_eq!(post(port, "/gw/chat", "").0, 200);
        let deadline = Instant::now() + Duration::from_secs(15);
        while input_tokens(&engine) < 10 {
            assert!(Instant::now() < deadline, "the retried close never landed");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(input_tokens(&engine), 10);
        assert!(shared.pump.stats.retried.load(Ordering::SeqCst) >= 3);
        // More failures than the attempt bound: skipped, and counted as such.
        shared.pump.fail_closes.store(1_000, Ordering::SeqCst);
        assert_eq!(post(port, "/gw/chat", "").0, 200);
        let deadline = Instant::now() + Duration::from_secs(15);
        while shared.pump.stats.skipped.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "the poisoned close was never skipped"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        shared.pump.fail_closes.store(0, Ordering::SeqCst);
        assert_eq!(input_tokens(&engine), 10, "a skipped close counted");
    }

    #[test]
    fn concurrent_first_calls_leave_no_pending_creation_gate_behind() {
        // Review round 1, finding 6: the calls that wait on a creation and
        // then find the Run published return early; they must still release
        // the identity's gate, or one entry leaks per raced identity.
        let (_state, engine, port, _served) = engine_with_binding(&[("team", "core")]);
        let shared = engine.proxy_shared_for_tests().unwrap();
        *shared.ensure_delay.lock().unwrap() = Some(Duration::from_millis(200));
        let calls: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(move || post(port, "/gw/chat", "x-hekma-run: raced\r\n")))
            .collect();
        for call in calls {
            assert_eq!(call.join().unwrap().0, 200);
        }
        *shared.ensure_delay.lock().unwrap() = None;
        assert!(
            shared.pending.lock().unwrap().is_empty(),
            "a pending-creation gate leaked"
        );
    }

    #[test]
    fn a_close_whose_calls_record_outlived_a_state_rollback_still_counts_its_usage() {
        // Review round 1, finding 5: the calls store is its own file, so a
        // commit can keep its record while state.db rolls back. The retry
        // then finds the record; the usage must still land, once.
        let (_state, engine, port, _served) = engine_with_binding(&[("team", "core")]);
        let shared = engine.proxy_shared_for_tests().unwrap();
        shared.pump.prerecord_closes.store(1, Ordering::SeqCst);
        assert_eq!(post(port, "/gw/chat", "").0, 200);
        let deadline = Instant::now() + Duration::from_secs(15);
        while input_tokens(&engine) < 10 {
            assert!(Instant::now() < deadline, "the usage was dropped on replay");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(shared.pump.prerecord_closes.load(Ordering::SeqCst), 0);
        assert_eq!(post(port, "/gw/chat", "").0, 200);
        let deadline = Instant::now() + Duration::from_secs(15);
        while input_tokens(&engine) < 20 {
            assert!(Instant::now() < deadline, "the next call never landed");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(input_tokens(&engine), 20, "each call counted exactly once");
    }
}

/// Story 22-3: the Scent-Check Modes at the proxy, end to end against a
/// labelled binding (whose calls carry no Run Key, so they are unscented).
mod scent {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use crate::domain::runs::policy::ScentCheckMode;
    use crate::domain::runs::{Anomaly, PolicyLimits, RunEventKind};
    use crate::{CallsQuery, EngineEvent, ListRunsQuery, ListingView, PolicyChange};

    fn raw(port: u16, method: &str, path: &str, extra: &str) -> (u16, String) {
        let body = if method == "POST" {
            r#"{"model":"gpt-demo"}"#
        } else {
            ""
        };
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n{extra}\
                     content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        let status = answer
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        (status, answer)
    }

    fn mode(scent: ScentCheckMode) -> PolicyChange {
        PolicyChange {
            name: "scent".into(),
            selector: Some("team=core".parse().unwrap()),
            set: PolicyLimits {
                scent_check: Some(scent),
                ..PolicyLimits::default()
            },
            clear: Vec::new(),
            rates: Default::default(),
        }
    }

    fn binding_run_anomalies(engine: &crate::Engine) -> Vec<Anomaly> {
        engine
            .blocking()
            .list_runs(ListRunsQuery {
                view: ListingView::All,
                limit: 0,
                ..ListRunsQuery::default()
            })
            .unwrap()
            .items
            .into_iter()
            .find(|run| run.labels.iter().any(|label| label.key == "team"))
            .map(|run| run.record.anomalies)
            .unwrap_or_default()
    }

    fn wait_for(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn shadow_serves_and_marks_once_and_enforce_refuses_unscented_calls_only() {
        let (_state, engine, port, served) = super::seams::engine_with_binding(&[("team", "core")]);
        let facade = engine.blocking();
        let mut events = facade.subscribe();

        // `shadow`: served, and the binding's Run gets `unscented` once.
        facade
            .create_policy(mode(ScentCheckMode::Shadow), false)
            .unwrap();
        for _ in 0..2 {
            let (status, answer) = raw(port, "POST", "/gw/v1/chat", "");
            assert_eq!(status, 200, "{answer}");
        }
        assert_eq!(served.load(Ordering::SeqCst), 2);
        wait_for("the unscented Anomaly", || {
            binding_run_anomalies(&engine).contains(&Anomaly::UnscentedCall)
        });
        // Let both closes land, then count the Anomaly's events: one.
        std::thread::sleep(Duration::from_millis(500));
        let mut raised = 0;
        while let Ok(event) = events.try_recv() {
            if let EngineEvent::Run(run) = event {
                if run.change == RunEventKind::AnomaliesChanged
                    && run.anomalies.contains(&Anomaly::UnscentedCall)
                {
                    raised += 1;
                }
            }
        }
        assert_eq!(raised, 1, "the Anomaly is raised once, not per call");

        // `enforce`: the re-resolution clears the Anomaly, and an unscented
        // call is refused before any dial — a GET too (no safe-method
        // exemption).
        facade
            .change_policy(mode(ScentCheckMode::Enforce), false)
            .unwrap();
        wait_for("the Anomaly to clear", || {
            !binding_run_anomalies(&engine).contains(&Anomaly::UnscentedCall)
        });
        for method in ["POST", "GET"] {
            let (status, answer) = raw(port, method, "/gw/v1/chat", "");
            assert_eq!(status, 401, "{method}: {answer}");
            assert!(answer.contains("hekma_unscented_call"), "{answer}");
            assert!(answer.contains("x-should-retry: false"), "{answer}");
        }
        assert_eq!(served.load(Ordering::SeqCst), 2, "nothing was forwarded");
        // An unknown Run Key keeps its own answer under `enforce`.
        let unknown = format!("x-hekma-run-key: hk_run_{}\r\n", "9".repeat(52));
        let (status, answer) = raw(port, "POST", "/gw/v1/chat", &unknown);
        assert_eq!(status, 401);
        assert!(answer.contains("hekma_unknown_credential"), "{answer}");
        // The refusals are recorded with disposition `enforce`.
        wait_for("the enforce records", || {
            facade
                .list_calls(CallsQuery {
                    disposition: Some("enforce".into()),
                    ..CallsQuery::all()
                })
                .map(|page| page.items.len() == 2)
                .unwrap_or(false)
        });

        // `off`: served again.
        facade
            .change_policy(mode(ScentCheckMode::Off), false)
            .unwrap();
        let (status, answer) = raw(port, "POST", "/gw/v1/chat", "");
        assert_eq!(status, 200, "{answer}");
        assert_eq!(served.load(Ordering::SeqCst), 3);
    }
}

#[test]
fn the_identity_header_switch_resolves_binding_then_flag_then_the_stored_root() {
    // Story 21-8: root on/off x binding unset/on/off/none x flag
    // absent/off/on — the binding's override wins, then the run's flag,
    // then the stored root.
    let entry = |name: &str, prefix: &str, over: Option<bool>| BindingEntry {
        name: name.into(),
        prefix: prefix.into(),
        upstream: Arc::from(BINDING_UPSTREAM),
        labels: labels(),
        custodied_key: None,
        identity_headers: over,
    };
    for root in [true, false] {
        let table = BindingTable::new(vec![
            entry("inherits", "/a", None),
            entry("on", "/b", Some(true)),
            entry("off", "/c", Some(false)),
        ])
        .with_identity_headers(root);
        for flag in [None, Some(false), Some(true)] {
            let base = flag.unwrap_or(root);
            for (binding, expected) in [
                (None, base),
                (Some("inherits"), base),
                (Some("ghost"), base),
                (Some("on"), true),
                (Some("off"), false),
            ] {
                assert_eq!(
                    table.identity_headers_for(flag, binding),
                    expected,
                    "root {root} flag {flag:?} binding {binding:?}"
                );
            }
        }
    }
    // An empty table (the default) is on.
    assert!(BindingTable::default().identity_headers_for(None, None));
    // The names are the wire contract's, unchanged.
    assert_eq!(
        crate::metering::proxy::wire::RUN_ID_HEADER,
        "x-hekma-run-id"
    );
    assert_eq!(
        crate::metering::proxy::wire::LABELS_HEADER,
        "x-hekma-labels"
    );
}

#[test]
fn beyond_loopback_every_remote_call_authenticates_and_a_proxy_access_credential_keeps_to_its_binding(
) {
    // Story 22-5: the posture table — bind × peer × credential × prefix.
    let mut w = world();
    let (pxy, pxy_hash) = token(TokenClass::ProxyAccess);
    let (revoked_pxy, revoked_pxy_hash) = token(TokenClass::ProxyAccess);
    let (unknown_pxy, _) = token(TokenClass::ProxyAccess);
    let (supervised_key, supervised_hash) = token(TokenClass::RunKey);
    w.snapshot.keys.insert(
        supervised_hash,
        Arc::new(KeyEntry {
            process_owner: true,
            ..(*key_entry("run_O", TokenClass::RunKey)).clone()
        }),
    );
    let mut vault = openai();
    vault.custodied_key = Some("secret:VAULT".into());
    w.snapshot.bindings = Arc::new(
        BindingTable::new(vec![
            vault,
            BindingEntry {
                name: "plain".into(),
                prefix: "/plain".into(),
                upstream: Arc::from(BINDING_UPSTREAM),
                labels: Vec::new(),
                custodied_key: None,
                identity_headers: None,
            },
        ])
        .with_proxy_access(
            vec![(pxy_hash, "openai".into(), "tid-pxy".into())],
            vec![revoked_pxy_hash],
        ),
    );
    let bearer = |value: &str| format!("Bearer {value}");
    let posture = |path, beyond_loopback, peer_loopback| CallFacts {
        beyond_loopback,
        peer_loopback,
        ..facts(path)
    };

    // A remote peer with no credential: refused before the route.
    let remote = posture("/openai/v1", true, false);
    assert_eq!(
        refused(&identify(&w.snapshot, &remote)),
        Some(Refusal::AuthenticationRequired)
    );
    let remote_unrouted = posture("/nowhere", true, false);
    assert_eq!(
        refused(&identify(&w.snapshot, &remote_unrouted)),
        Some(Refusal::AuthenticationRequired)
    );
    // A loopback peer on the same listener keeps today's rules.
    let (target, attribution, _) = call(identify(&w.snapshot, &posture("/openai/v1", true, true)));
    assert_eq!(attribution, run("run_B"));
    assert!(target.proxy_access.is_none() && target.custody.is_none());
    // A loopback bind never asks (the default facts are loopback-bound).
    assert!(refused(&identify(&w.snapshot, &facts("/openai/v1"))).is_none());

    // The proxy-access credential: its binding, its token id, the binding's
    // custodied key — and never a scented call.
    for slot in ["authorization", "x-api-key"] {
        let auth = bearer(&pxy);
        let mut f = posture("/openai/v1", true, false);
        if slot == "authorization" {
            f.authorization = Some(&auth);
        } else {
            f.api_key = Some(&pxy);
        }
        let (target, attribution, _) = call(identify(&w.snapshot, &f));
        assert_eq!(attribution, run("run_B"), "{slot}");
        assert_eq!(target.proxy_access.as_deref(), Some("tid-pxy"));
        assert!(
            target.token_id.is_none(),
            "a proxy-access call is unscented"
        );
        assert_eq!(
            target.custody.as_ref().map(|c| &*c.key_ref),
            Some("secret:VAULT")
        );
    }
    // Out of scope: another binding, a Run route.
    let auth = bearer(&pxy);
    for path in ["/plain/v1", "/_hekma/runs/run_P/v1"] {
        let f = CallFacts {
            authorization: Some(&auth),
            ..posture(path, true, false)
        };
        assert_eq!(
            refused(&identify(&w.snapshot, &f)),
            Some(Refusal::CredentialOutOfScope),
            "{path}"
        );
    }
    // Revoked and unknown.
    for (presented, expected) in [
        (&revoked_pxy, Refusal::CredentialRevoked),
        (&unknown_pxy, Refusal::UnknownCredential),
    ] {
        let auth = bearer(presented);
        let f = CallFacts {
            authorization: Some(&auth),
            ..posture("/openai/v1", true, false)
        };
        assert_eq!(refused(&identify(&w.snapshot, &f)), Some(expected));
    }
    // On a loopback bind, or in x-hekma-run-key, a proxy-access credential
    // is not one: unknown (even when revoked).
    for presented in [&pxy, &revoked_pxy] {
        let auth = bearer(presented);
        let loopback = CallFacts {
            authorization: Some(&auth),
            ..facts("/openai/v1")
        };
        assert_eq!(
            refused(&identify(&w.snapshot, &loopback)),
            Some(Refusal::UnknownCredential)
        );
        let in_run_key = CallFacts {
            run_key: Some(presented),
            ..posture("/openai/v1", true, false)
        };
        assert_eq!(
            refused(&identify(&w.snapshot, &in_run_key)),
            Some(Refusal::UnknownCredential)
        );
    }

    // A Run Key: a non-supervised Run's from anywhere; a supervised Run's
    // from loopback peers only.
    let f = CallFacts {
        run_key: Some(&w.child_key),
        ..posture("/openai/v1", true, false)
    };
    let (target, attribution, _) = call(identify(&w.snapshot, &f));
    assert_eq!(attribution, run("run_C"));
    assert!(target.token_id.is_some());
    let remote_supervised = CallFacts {
        run_key: Some(&supervised_key),
        ..posture("/openai/v1", true, false)
    };
    assert_eq!(
        refused(&identify(&w.snapshot, &remote_supervised)),
        Some(Refusal::UnknownCredential)
    );
    let local_supervised = CallFacts {
        run_key: Some(&supervised_key),
        ..posture("/openai/v1", true, true)
    };
    assert_eq!(
        call(identify(&w.snapshot, &local_supervised)).1,
        run("run_O")
    );
    // A Run Key and a proxy-access credential together: the key identifies
    // (scented); the credential must still be in scope.
    let auth = bearer(&pxy);
    let both = CallFacts {
        run_key: Some(&w.child_key),
        authorization: Some(&auth),
        ..posture("/openai/v1", true, false)
    };
    let (target, attribution, _) = call(identify(&w.snapshot, &both));
    assert_eq!(attribution, run("run_C"));
    assert!(target.token_id.is_some() && target.proxy_access.is_some());
    let both_elsewhere = CallFacts {
        run_key: Some(&w.child_key),
        authorization: Some(&auth),
        ..posture("/plain/v1", true, false)
    };
    assert_eq!(
        refused(&identify(&w.snapshot, &both_elsewhere)),
        Some(Refusal::CredentialOutOfScope)
    );
    assert!(w.snapshot.bindings.has_proxy_access());
    assert!(!BindingTable::default().has_proxy_access());
}

#[test]
fn a_peer_is_loopback_only_on_127_8_and_the_ipv6_loopback() {
    use std::net::IpAddr;
    for (ip, loopback) in [
        ("127.0.0.1", true),
        ("127.9.9.9", true),
        ("::1", true),
        ("::ffff:127.0.0.1", true),
        ("0.0.0.0", false),
        ("::", false),
        ("10.0.0.5", false),
        ("::ffff:10.0.0.5", false),
    ] {
        let ip: IpAddr = ip.parse().unwrap();
        assert_eq!(super::peer_is_loopback(ip), loopback, "{ip}");
    }
}

/// Story 24-4: one retention pass over a real writer — what it compacts,
/// which calls files it deletes, the one line it says, and that a store
/// failure mid-pass stops it with a note instead of looping.
mod retention_pass {
    use std::sync::{Arc, Mutex};

    use crate::store::{SqliteStore, StoreWriter};

    fn seeded(sql: &str) -> (tempfile::TempDir, StoreWriter) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        drop(SqliteStore::open(&db).unwrap());
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 process_owner, end_cause, created_at, updated_at, ended_at) VALUES \
                 ('run_OLD', 'announced', 0, 'ended', '', 0, 'closed', '2020-01-01T00:00:00Z', \
                 '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z');",
            )
            .unwrap();
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let store = SqliteStore::open(&db).unwrap();
        store.attach_calls(None).unwrap();
        (dir, StoreWriter::spawn(store).unwrap())
    }

    fn pass(writer: &StoreWriter, calls: Option<&std::path::Path>) -> Vec<String> {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let diagnostics: Arc<dyn Fn(String) + Send + Sync> =
            Arc::new(move |line| sink.lock().unwrap().push(line));
        let handle = writer.handle().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(super::super::retention_pass(&handle, calls, &diagnostics));
        let lines = lines.lock().unwrap().clone();
        lines
    }

    #[test]
    fn a_pass_compacts_deletes_old_calls_files_and_records_both() {
        let (dir, writer) = seeded("");
        let calls = dir.path().join("calls");
        std::fs::create_dir_all(&calls).unwrap();
        drop(rusqlite::Connection::open(calls.join("2020-01-01.db")).unwrap());
        let lines = pass(&writer, Some(&calls));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("1 ended Run(s)") && lines[0].contains("1 per-call record file(s)"),
            "{lines:?}"
        );
        assert!(!calls.join("2020-01-01.db").exists());
        let (compacted, dropped) = writer
            .call(|ctx| {
                let state = ctx.store().compaction_state()?;
                Ok((state.runs_compacted, state.segments_dropped))
            })
            .unwrap();
        assert_eq!((compacted, dropped), (1, 1));
        // Nothing left: the next pass says nothing.
        assert!(pass(&writer, Some(&calls)).is_empty());
    }

    #[test]
    fn a_store_failure_stops_the_pass_with_a_note() {
        let (_dir, writer) = seeded("DROP TABLE compacted_runs;");
        let lines = pass(&writer, None);
        assert!(
            lines.iter().any(|line| line.contains("stopped early")),
            "{lines:?}"
        );
        // Without the retention table the pass cannot read its windows and
        // does nothing.
        let (_dir, writer) = seeded("DROP TABLE retention;");
        assert!(pass(&writer, None).is_empty());
    }
}
