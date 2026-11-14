//! The announce job's tables (story 23-2): namespaces, collisions, caps,
//! the mint-once rule, and the heartbeat and close authority.

use super::*;
use crate::domain::credential::{mint, AnnounceCredential, TokenClass};
use crate::domain::runs::wire::{ensure_wire_run, Ensured, WireIdentity};
use crate::domain::runs::RunEventKind;
use crate::store::proxy_sqlite::BindingRow;

const NOW: u64 = 1_800_000_000;

fn binding(name: &str, labels: Vec<(String, String)>) -> BindingRow {
    BindingRow {
        name: name.into(),
        prefix: format!("/{name}"),
        upstream: "https://api.example.com/v1".into(),
        custodied_key: None,
        labels,
        created_at: "t".into(),
        updated_at: "t".into(),
        identity_headers: None,
    }
}

fn store() -> SqliteStore {
    let store = SqliteStore::open_in_memory().unwrap();
    store
        .put_binding(&binding("openai", vec![("team".into(), "core".into())]))
        .unwrap();
    store
}

/// An active announce credential for `binding`.
fn credential(store: &SqliteStore, binding: &str) -> AnnounceCredential {
    let minted = mint(TokenClass::Announce).unwrap();
    let hash = minted.hash();
    store
        .insert_binding_credential(binding, &hash, &minted.token_id(), "announce", "t")
        .unwrap();
    AnnounceCredential {
        binding: binding.into(),
        token_id: minted.token_id(),
        hash,
    }
}

fn endpoint(credential: &AnnounceCredential, external: &str) -> AnnounceClaim {
    AnnounceClaim {
        scope: AnnounceScope::Binding(credential.binding.clone()),
        external: external.into(),
        caller_labels: vec![("step".into(), "plan".into())],
        credentialed: true,
        created_by: Some(credential.hash),
        issuer: credential.token_id.clone(),
        wire: false,
        telemetry: false,
    }
}

fn header(scope: AnnounceScope, external: &str) -> AnnounceClaim {
    AnnounceClaim {
        scope,
        external: external.into(),
        caller_labels: Vec::new(),
        credentialed: false,
        created_by: None,
        issuer: "announce".into(),
        wire: true,
        telemetry: false,
    }
}

fn on(binding: &str) -> AnnounceScope {
    AnnounceScope::Binding(binding.into())
}

fn pair() -> Option<(TokenStub, TokenStub)> {
    Some((
        mint(TokenClass::RunKey).unwrap().stub(),
        mint(TokenClass::Refresh).unwrap().stub(),
    ))
}

fn try_claim(
    store: &SqliteStore,
    claim: &AnnounceClaim,
    now: u64,
) -> Result<(Claimed, RunChanges), RunError> {
    announce(
        store,
        claim,
        RunId::mint(),
        pair(),
        &LeaseOptions::default(),
        now,
    )
}

fn claim(store: &SqliteStore, c: &AnnounceClaim, now: u64) -> (Claimed, RunChanges) {
    try_claim(store, c, now).unwrap()
}

fn created(store: &SqliteStore, c: &AnnounceClaim) -> RunId {
    match claim(store, c, NOW).0 {
        Claimed::Created { run, minted: true } => run,
        other => panic!("not created: {other:?}"),
    }
}

fn labels_of(store: &SqliteStore, run: &RunId) -> Vec<(String, String, LabelSource)> {
    store
        .run_labels(run)
        .unwrap()
        .into_iter()
        .map(|label| (label.key, label.value, label.source))
        .collect()
}

fn at(secs: u64) -> Option<String> {
    Some(crate::time::format_rfc3339(secs))
}

#[test]
fn an_endpoint_announce_creates_an_observed_run_with_a_lease_and_one_eligible_pair() {
    let store = store();
    let cred = credential(&store, "openai");
    let (claimed, changes) = claim(&store, &endpoint(&cred, "job-7"), NOW);
    let Claimed::Created { run, minted: true } = claimed else {
        panic!("{claimed:?}")
    };
    let record = store.run_record(&run).unwrap().unwrap();
    assert_eq!(record.control_depth, ControlDepth::Observed);
    assert!(!record.wire_path, "it announced without traffic");
    assert_eq!(record.status, RunStatus::Active);
    assert_eq!(
        store.identity_run("binding:openai", "job-7").unwrap(),
        Some(run.clone())
    );
    assert_eq!(store.run_lease(&run).unwrap(), (at(NOW + 60), at(NOW)));
    assert_eq!(
        store.run_created_by(&run).unwrap().as_deref(),
        Some(cred.hash.as_bytes().as_slice())
    );
    // The binding's labels bound, the announce's as Caller labels.
    let labels = labels_of(&store, &run);
    assert!(labels.contains(&("team".into(), "core".into(), LabelSource::Bound)));
    assert!(labels.contains(&("step".into(), "plan".into(), LabelSource::Caller)));
    // One `created` event and one pair, custody-eligible, issued by the
    // announce credential.
    let created_events = changes
        .events
        .iter()
        .filter(|e| e.change == RunEventKind::Created)
        .count();
    assert_eq!(created_events, 1);
    assert_eq!(changes.credentials.len(), 2);
    assert!(changes
        .credentials
        .iter()
        .all(|e| e.issued_by.as_deref() == Some(cred.token_id.as_str())));
    let tokens = store.run_tokens(&run).unwrap();
    assert_eq!(tokens.len(), 2);
    assert!(tokens.iter().all(|t| t.custody_eligible));
}

#[test]
fn a_header_announce_is_announced_on_the_wire_and_its_key_is_never_custody_eligible() {
    let store = store();
    let run = created(&store, &header(on("openai"), "job-7"));
    let record = store.run_record(&run).unwrap().unwrap();
    assert_eq!(record.control_depth, ControlDepth::Announced);
    assert!(record.wire_path);
    assert_eq!(store.run_created_by(&run).unwrap(), None);
    let tokens = store.run_tokens(&run).unwrap();
    assert_eq!(tokens.len(), 2);
    assert!(tokens.iter().all(|t| !t.custody_eligible));
}

#[test]
fn a_re_announce_renews_upserts_caller_labels_and_never_mints() {
    let store = store();
    let cred = credential(&store, "openai");
    let run = created(&store, &endpoint(&cred, "job-7"));
    let mut again = endpoint(&cred, "job-7");
    again.caller_labels = vec![("step".into(), "act".into())];
    let (claimed, changes) = claim(&store, &again, NOW + 30);
    assert_eq!(claimed, Claimed::Renewed { run: run.clone() });
    assert!(changes.credentials.is_empty(), "no mint on a re-announce");
    assert_eq!(store.run_tokens(&run).unwrap().len(), 2);
    let kinds: Vec<RunEventKind> = changes.events.iter().map(|e| e.change).collect();
    assert_eq!(kinds, vec![RunEventKind::LabelsChanged]);
    let labels = labels_of(&store, &run);
    assert!(labels.contains(&("step".into(), "act".into(), LabelSource::Caller)));
    assert!(!labels.iter().any(|(_, v, _)| v == "plan"));
    assert!(labels.contains(&("team".into(), "core".into(), LabelSource::Bound)));
    assert_eq!(store.run_lease(&run).unwrap().0, at(NOW + 90));
    // The same labels again: nothing but the renewal.
    let (_, quiet) = claim(&store, &again, NOW + 31);
    assert!(quiet.events.is_empty(), "{:?}", quiet.events);
    // A Caller label on a bound key is refused, re-announce or fresh.
    for external in ["job-7", "job-8"] {
        let mut bound = endpoint(&cred, external);
        bound.caller_labels = vec![("team".into(), "edge".into())];
        assert!(matches!(
            try_claim(&store, &bound, NOW + 32),
            Err(RunError::LabelBound { .. })
        ));
    }
    assert_eq!(store.identity_run("binding:openai", "job-8").unwrap(), None);
}

#[test]
fn a_discovered_split_later_announced_stays_one_run_and_changes_depth_once() {
    let store = store();
    let (ensured, _) = ensure_wire_run(
        &store,
        &WireIdentity::Split {
            binding: "openai".into(),
            external: "job-7".into(),
            labels: vec![("team".into(), "core".into())],
        },
        RunId::mint(),
        "2026-11-13T00:00:00Z",
    )
    .unwrap();
    let Ensured::Run(split) = ensured else {
        panic!("{ensured:?}")
    };
    assert!(
        store.run_lease(&split).unwrap().0.is_none(),
        "a wire Run holds no Lease"
    );
    let cred = credential(&store, "openai");
    let (claimed, changes) = claim(&store, &endpoint(&cred, "job-7"), NOW);
    assert_eq!(claimed, Claimed::Renewed { run: split.clone() });
    let depth_changes = changes
        .events
        .iter()
        .filter(|e| e.change == RunEventKind::DepthChanged)
        .count();
    assert_eq!(depth_changes, 1);
    let record = store.run_record(&split).unwrap().unwrap();
    assert_eq!(record.control_depth, ControlDepth::Announced);
    assert!(record.wire_path);
    assert!(
        store.run_lease(&split).unwrap().0.is_some(),
        "announcing grants a Lease"
    );
    assert!(
        store.run_tokens(&split).unwrap().is_empty(),
        "a claim on an open Run mints nothing"
    );
}

#[test]
fn an_ended_identity_goes_to_a_new_run_and_caps_and_gone_scopes_create_nothing() {
    let store = store();
    let cred = credential(&store, "openai");
    let first = created(&store, &endpoint(&cred, "job-7"));
    let mut record = store.run_record(&first).unwrap().unwrap();
    record.status = RunStatus::Ended;
    record.ended_at = Some("t".into());
    store.save_run(&record).unwrap();
    let second = created(&store, &endpoint(&cred, "job-7"));
    assert_ne!(second, first);
    // An unknown binding, and the per-binding cap.
    assert_eq!(
        claim(&store, &header(on("gone"), "x"), NOW).0,
        Claimed::ParentGone
    );
    for n in 0..MAX_CALLER_OPEN_CHILDREN - 1 {
        created(&store, &header(on("openai"), &format!("s{n}")));
    }
    assert_eq!(
        claim(&store, &header(on("openai"), "one-more"), NOW).0,
        Claimed::Capped
    );
    // An open identity is still renewed past the cap.
    assert!(matches!(
        claim(&store, &endpoint(&cred, "job-7"), NOW).0,
        Claimed::Renewed { .. }
    ));
}

#[test]
fn a_child_announce_needs_an_open_parent_and_respects_the_root_cap() {
    let store = store();
    let root = created(&store, &header(on("openai"), "root"));
    let child = created(&store, &header(AnnounceScope::Parent(root.clone()), "c"));
    let record = store.run_record(&child).unwrap().unwrap();
    assert_eq!(record.parent_run_id.as_ref(), Some(&root));
    assert_eq!(
        store
            .identity_run(&format!("run:{}", root.as_str()), "c")
            .unwrap(),
        Some(child.clone())
    );
    let ghost = AnnounceScope::Parent(RunId::from_wire("run_GHOST"));
    assert_eq!(
        claim(&store, &header(ghost, "x"), NOW).0,
        Claimed::ParentGone
    );
    // A held parent's announced child starts held.
    let mut held = store.run_record(&child).unwrap().unwrap();
    held.status = RunStatus::Held;
    store.save_run(&held).unwrap();
    let beneath = created(&store, &header(AnnounceScope::Parent(child.clone()), "d"));
    assert_eq!(
        store.run_record(&beneath).unwrap().unwrap().status,
        RunStatus::Held
    );
    // Fill the root's subtree to the per-root cap across two parents (each
    // under the per-parent cap): the next announce anywhere below is capped.
    assert_eq!(store.open_descendants(&root).unwrap(), 2);
    let mut n = 0;
    while store.open_descendants(&root).unwrap() < MAX_ROOT_OPEN_DESCENDANTS {
        let parent = if n % 2 == 0 { &root } else { &child };
        created(
            &store,
            &header(AnnounceScope::Parent(parent.clone()), &format!("f{n}")),
        );
        n += 1;
    }
    let over = header(AnnounceScope::Parent(beneath.clone()), "over");
    assert_eq!(claim(&store, &over, NOW).0, Claimed::Capped);
    assert!(root_is_full(&store, &beneath).unwrap());
}

#[test]
fn heartbeat_and_close_follow_the_authority_cells() {
    let store = store();
    let cred = credential(&store, "openai");
    store
        .put_binding(&binding("anthropic", Vec::new()))
        .unwrap();
    let other = credential(&store, "anthropic");
    let ours = created(&store, &endpoint(&cred, "job-7"));
    let theirs = created(&store, &header(on("openai"), "wire"));
    let options = LeaseOptions::default();
    let by = |c: &AnnounceCredential| LeaseActor::Announcer(c.clone());
    // The creating credential heartbeats its Run; another binding's, or a
    // Run it did not create, is out of scope.
    heartbeat(&store, &by(&cred), &ours, &options, NOW + 10).unwrap();
    assert_eq!(store.run_lease(&ours).unwrap().0, at(NOW + 70));
    for (who, run) in [(&other, &ours), (&cred, &theirs)] {
        assert!(matches!(
            heartbeat(&store, &by(who), run, &options, NOW),
            Err(RunError::RunOutOfScope { .. })
        ));
    }
    let none = RunId::from_wire("run_NONE");
    assert!(matches!(
        heartbeat(&store, &by(&cred), &none, &options, NOW),
        Err(RunError::NotFound { .. })
    ));
    // A heartbeat lifts a silent Run with one status change.
    let mut silent = store.run_record(&ours).unwrap().unwrap();
    silent.status = RunStatus::Silent;
    store.save_run(&silent).unwrap();
    let lifted = heartbeat(&store, &by(&cred), &ours, &options, NOW + 20).unwrap();
    assert_eq!(lifted.events.len(), 1);
    assert_eq!(lifted.events[0].change, RunEventKind::StatusChanged);
    assert_eq!(
        store.run_record(&ours).unwrap().unwrap().status,
        RunStatus::Active
    );
    // A wire Run (no Lease) heartbeated by the Operator is granted one.
    let (Ensured::Run(wire), _) = ensure_wire_run(
        &store,
        &WireIdentity::Split {
            binding: "openai".into(),
            external: "split".into(),
            labels: Vec::new(),
        },
        RunId::mint(),
        "t",
    )
    .unwrap() else {
        panic!("no split")
    };
    assert!(store.run_lease(&wire).unwrap().0.is_none());
    let operator = LeaseActor::Actor(RunActor::Operator);
    heartbeat(&store, &operator, &wire, &options, NOW).unwrap();
    assert_eq!(store.run_lease(&wire).unwrap().0, at(NOW + 60));
    // Close: not a Run it created, then its own; an ended Run's heartbeat
    // is refused.
    assert!(matches!(
        close_as(&store, &by(&cred), &theirs, "t"),
        Err(RunError::RunOutOfScope { .. })
    ));
    let closed = close_as(&store, &by(&cred), &ours, "t").unwrap();
    assert!(closed
        .events
        .iter()
        .any(|e| e.change == RunEventKind::Ended));
    assert!(!closed.credentials.is_empty(), "its tokens are revoked");
    assert!(matches!(
        heartbeat(&store, &by(&cred), &ours, &options, NOW),
        Err(RunError::RunEnded { .. })
    ));
    assert!(matches!(
        close_as(&store, &by(&cred), &none, "t"),
        Err(RunError::NotFound { .. })
    ));
    // The Operator closes through the Caller path.
    assert!(close_as(&store, &operator, &theirs, "t").is_ok());
    // A revoked credential, and one never issued.
    store
        .revoke_binding_credential("openai", "announce", "t", "operator")
        .unwrap();
    assert!(matches!(
        heartbeat(&store, &by(&cred), &theirs, &options, NOW),
        Err(RunError::CredentialRevoked)
    ));
    let never = AnnounceCredential {
        binding: "openai".into(),
        token_id: "x".into(),
        hash: mint(TokenClass::Announce).unwrap().hash(),
    };
    assert!(matches!(
        heartbeat(&store, &by(&never), &theirs, &options, NOW),
        Err(RunError::CredentialUnknown)
    ));
}

#[test]
fn announce_inputs_follow_the_hint_rules() {
    assert!(validate("job-7", &[("team".into(), "core".into())]).is_ok());
    for bad in ["", "a b"] {
        assert!(matches!(validate(bad, &[]), Err(RunError::Invalid { .. })));
    }
    for label in [("Team", "core"), ("team", "secret:X")] {
        let labels = [(label.0.to_string(), label.1.to_string())];
        assert!(matches!(
            validate("job", &labels),
            Err(RunError::Invalid { .. })
        ));
    }
    let many: Vec<(String, String)> = (0..17).map(|i| (format!("k{i}"), "v".into())).collect();
    assert!(matches!(
        validate("job", &many),
        Err(RunError::Invalid { .. })
    ));
}

#[test]
fn an_endpoint_announced_run_moves_to_announced_on_its_first_proxied_call() {
    let store = store();
    let cred = credential(&store, "openai");
    let run = created(&store, &endpoint(&cred, "job-7"));
    let mut changes = RunChanges::default();
    super::super::lease::note_wire_call(&store, &mut changes, &run, NOW).unwrap();
    let record = store.run_record(&run).unwrap().unwrap();
    assert_eq!(record.control_depth, ControlDepth::Announced);
    assert!(record.wire_path);
    assert_eq!(changes.events.len(), 1);
    assert_eq!(changes.events[0].change, RunEventKind::DepthChanged);
    // A second call, or a wire Run (no Lease), changes nothing.
    let mut again = RunChanges::default();
    super::super::lease::note_wire_call(&store, &mut again, &run, NOW).unwrap();
    assert!(again.events.is_empty());
    let wire = created(&store, &header(on("openai"), "wire"));
    super::super::lease::note_wire_call(&store, &mut again, &wire, NOW).unwrap();
    assert!(again.events.is_empty());
    let ghost = RunId::from_wire("run_GHOST");
    super::super::lease::note_wire_call(&store, &mut again, &ghost, NOW).unwrap();
    assert!(again.events.is_empty());
}
