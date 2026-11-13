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
            custodied_key: None,
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
                    "POST {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n{extra}\
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

    fn engine_with_binding(
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
