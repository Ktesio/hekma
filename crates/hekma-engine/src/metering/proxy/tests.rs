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
            runs,
            keys,
            revoked: Arc::new(revoked),
            bindings: Arc::new(bindings),
            identities,
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
        },
        openai(),
    ]));
    let (target, _, _) = call(identify(&w.snapshot, &facts("/v1/chat")));
    assert_eq!(target.via, Via::Binding("all".into()));
    assert_eq!(target.rest, "/v1/chat");
    let (target, _, _) = call(identify(&w.snapshot, &facts("/openai/v1")));
    assert_eq!(target.via, Via::Binding("openai".into()));
}
