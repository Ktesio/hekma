//! Run Key mechanics (story 22-1), proven at the job level with Runs seeded
//! directly — no child process, every OS: issuance and the one-time pair,
//! the Caller's subtree scope and breadth cap, refresh and reuse detection,
//! revocation with and without the cascade, revocation at a Run's end and a
//! launch's exit, in-job re-validation, URL exposure, and the snapshot index
//! after every job and after reopen.

use super::ops::{self, LaunchCredentials, RunFate, SupervisedRun};
use super::*;
use crate::domain::credential::{
    hash_presented, mint, tests as test_seams, CredentialChange, CredentialMatch, TokenClass,
    TokenHash, TokenState,
};
use crate::domain::name::InstanceName;
use crate::domain::{EngineEvent, Registry, Supervisor};
use crate::ports::RunRecords as _;

const INSTANCE: &str = "keys";

struct World {
    #[allow(dead_code)]
    state: tempfile::TempDir,
    registry: Registry,
    supervisor: Supervisor,
    /// The open process-owning Run of [`INSTANCE`].
    root: RunId,
}

fn record(run: &RunId, process_owner: bool, parent: Option<&RunId>) -> RunRecord {
    RunRecord {
        run_id: run.clone(),
        control_depth: if process_owner {
            ControlDepth::Supervised
        } else {
            ControlDepth::Announced
        },
        wire_path: false,
        status: RunStatus::Active,
        anomalies: Vec::new(),
        parent_run_id: parent.cloned(),
        instance: Some(INSTANCE.to_string()),
        process_owner,
        end_cause: None,
        created_at: "t".to_string(),
        updated_at: "t".to_string(),
        ended_at: None,
    }
}

fn world() -> World {
    let state = tempfile::TempDir::new().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let root = RunId::mint();
    let owner = record(&root, true, None);
    registry
        .writer
        .call(move |ctx| {
            let store = ctx.store();
            store.execute_batch_for_test(
                "INSERT INTO agent_instances (id, name, kind, state, agent_home, created_at, \
                 updated_at) VALUES (1, 'keys', 'mock', 'running', '/h', 't', 't')",
            );
            store.insert_run(&crate::ports::NewRun {
                record: owner,
                labels: Vec::new(),
                limits: Default::default(),
                identity: None,
            })
        })
        .unwrap();
    World {
        state,
        registry,
        supervisor: Supervisor::new(),
        root,
    }
}

impl World {
    /// Open a child as `actor` under `parent`, with credentials.
    fn open(&self, actor: RunActor, parent: &RunId) -> Result<OpenedRun, RunError> {
        let mut request = ChildRunRequest::under(RunParent::Run(parent.clone()));
        request.issue_credentials = true;
        self.supervisor
            .open_child_run_as(&self.registry, actor, request)
    }

    /// The Operator opens a child of the root, with credentials.
    fn operator_child(&self) -> (RunId, crate::domain::IssuedCredentials) {
        let opened = self.open(RunActor::Operator, &self.root).unwrap();
        (
            opened.view.record.run_id,
            opened.credentials.expect("issued"),
        )
    }

    /// What the snapshot answers for a presented token (the facade's
    /// lookup, without the Engine).
    fn authenticate(&self, presented: &crate::domain::SecretString) -> Option<CredentialMatch> {
        let hash = hash_presented(presented);
        let snapshot = self.registry.run_snapshot();
        let (stored, entry) = snapshot.keys.get_key_value(&hash)?;
        stored.ct_eq(&hash).then(|| CredentialMatch {
            class: entry.class,
            run_id: entry.run_id.clone(),
            token_id: entry.token_id.clone(),
            supervised: entry.launch_generation.is_some(),
            hash: *stored,
        })
    }

    fn caller(&self, presented: &crate::domain::SecretString) -> RunActor {
        self.authenticate(presented)
            .expect("the key authenticates")
            .actor()
    }

    fn token_state(&self, presented: &crate::domain::SecretString) -> TokenState {
        let hash = hash_presented(presented);
        self.registry
            .store
            .snapshot(|store| store.token(&hash))
            .unwrap()
            .expect("a row")
            .state
    }

    fn run(&self, run: &RunId) -> RunRecord {
        self.registry.run_record(run).unwrap().unwrap()
    }

    /// Every credential event in the outbox.
    fn credential_events(&self) -> Vec<crate::domain::CredentialEvent> {
        crate::domain::export_core(&self.registry, None, 10_000)
            .unwrap()
            .events
            .into_iter()
            .filter_map(|entry| match entry.event {
                EngineEvent::Credential(event) => Some(*event),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn an_operator_open_issues_a_pair_once_and_stores_only_hashes() {
    let w = world();
    let opened = w.open(RunActor::Operator, &w.root).unwrap();
    let credentials = opened.credentials.as_ref().unwrap();
    let key = credentials.run_key().expose_secret().to_string();
    let refresh = credentials.refresh_credential().expose_secret().to_string();
    assert!(key.starts_with("hk_run_") && key.len() == 59);
    assert!(refresh.starts_with("hk_ref_") && refresh.len() == 59);
    // Debug of the one-time value carries no token.
    let debug = format!("{opened:?}");
    assert!(
        !debug.contains(&key) && !debug.contains(&refresh),
        "{debug}"
    );
    // The Run Key authenticates as a Run Key of that Run; the refresh
    // credential is never in the snapshot.
    let matched = w.authenticate(credentials.run_key()).unwrap();
    assert_eq!(matched.class, TokenClass::RunKey);
    assert_eq!(matched.run_id, opened.view.record.run_id);
    assert_eq!(matched.token_id, credentials.token_id());
    assert!(!matched.supervised);
    assert!(w.authenticate(credentials.refresh_credential()).is_none());
    // The rows hold the hash and the token id (its first 12 hex) only.
    let rows = w
        .registry
        .store
        .snapshot(|store| store.run_tokens(&opened.view.record.run_id))
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.token_id, row.hash.token_id());
        assert_eq!(row.state, TokenState::Active);
        assert!(row.custody_eligible, "an Operator open is custody-eligible");
        assert!(row.issued_by_hash.is_none());
    }
    assert!(rows[0].paired_hash.unwrap().ct_eq(&rows[1].hash));
    // One `issued` event per token, issued by the Operator, no value.
    let events = w.credential_events();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(
        |e| e.change == CredentialChange::Issued && e.issued_by.as_deref() == Some("operator")
    ));
    let rendered = serde_json::to_string(&events).unwrap();
    assert!(!rendered.contains(&key) && !rendered.contains(&refresh));
    // The metadata listing names token ids, never a value.
    let keys = w.registry.run_keys(&opened.view.record.run_id).unwrap();
    assert_eq!(keys.len(), 2);
    let listing = serde_json::to_string(&keys).unwrap();
    assert!(!listing.contains(&key) && listing.contains(&credentials.token_id()));
    // Without the request bit, nothing is minted.
    let bare = w
        .supervisor
        .open_child_run_as(
            &w.registry,
            RunActor::Operator,
            ChildRunRequest::under(RunParent::Run(w.root.clone())),
        )
        .unwrap();
    assert!(bare.credentials.is_none());
    assert!(w
        .registry
        .run_keys(&bare.view.record.run_id)
        .unwrap()
        .is_empty());
}

#[test]
fn a_failing_random_source_refuses_the_open_with_nothing_written() {
    let w = world();
    let before = w.credential_events().len();
    let runs_before = w.registry.open_runs().unwrap().len();
    test_seams::fail_rng(true);
    let refused = w.open(RunActor::Operator, &w.root);
    test_seams::fail_rng(false);
    let err = refused.unwrap_err();
    assert_eq!(err.code(), "credential-unavailable");
    assert!(err.to_string().contains("random source failed"), "{err}");
    assert_eq!(w.registry.open_runs().unwrap().len(), runs_before);
    assert_eq!(w.credential_events().len(), before);
    assert!(w.registry.run_snapshot().keys.is_empty());
    // A refresh refuses the same way, before anything is read.
    let (_, credentials) = w.operator_child();
    test_seams::fail_rng(true);
    let refused = w
        .supervisor
        .refresh_credentials(&w.registry, credentials.refresh_credential());
    test_seams::fail_rng(false);
    assert_eq!(refused.unwrap_err().code(), "credential-unavailable");
    assert_eq!(
        w.token_state(credentials.refresh_credential()),
        TokenState::Active
    );
}

#[test]
fn a_caller_acts_only_within_its_own_subtree() {
    let w = world();
    let (child, credentials) = w.operator_child();
    let (sibling, _) = w.operator_child();
    let caller = w.caller(credentials.run_key());
    // Open beneath its own Run, and beneath a descendant.
    let mut request = ChildRunRequest::under(RunParent::Run(child.clone()));
    request.issue_credentials = true;
    request.labels = vec![("task".to_string(), "review".to_string())];
    let grandchild = w
        .supervisor
        .open_child_run_as(&w.registry, caller.clone(), request)
        .unwrap();
    // Its labels are Caller labels (they never select a Policy).
    assert!(grandchild.view.labels.iter().all(|l| !l.is_bound()));
    let gc_id = grandchild.view.record.run_id.clone();
    let gc_credentials = grandchild.credentials.unwrap();
    let rows = w
        .registry
        .store
        .snapshot(|store| store.run_tokens(&gc_id))
        .unwrap();
    // issued_by = the opener; custody inherited.
    let opener = hash_presented(credentials.run_key());
    assert!(rows
        .iter()
        .all(|r| r.issued_by_hash.unwrap().ct_eq(&opener)));
    assert!(rows.iter().all(|r| r.custody_eligible));
    let issued = w.credential_events();
    assert!(issued
        .iter()
        .filter(|e| e.run_id == gc_id)
        .all(|e| e.issued_by.as_deref() == Some(credentials.token_id().as_str())));
    let deeper = w.open(caller.clone(), &gc_id).unwrap();
    assert!(deeper.credentials.is_some());
    // Out of scope: a sibling, the parent (the root), an unrelated Run.
    for target in [&sibling, &w.root] {
        let err = w.open(caller.clone(), target).unwrap_err();
        assert_eq!(err.code(), "run-out-of-scope", "{target}");
        let err = w
            .supervisor
            .close_run_as(&w.registry, caller.clone(), target)
            .unwrap_err();
        assert_eq!(err.code(), "run-out-of-scope");
    }
    // The grandchild's own key cannot reach up to its parent either.
    let gc_caller = w.caller(gc_credentials.run_key());
    assert_eq!(
        w.open(gc_caller.clone(), &child).unwrap_err().code(),
        "run-out-of-scope"
    );
    // A missing Run is its own not-found.
    assert_eq!(
        w.open(caller.clone(), &RunId::from_wire("run_missing"))
            .unwrap_err()
            .code(),
        "run-not-found"
    );
    // Close a descendant; closing ends it and revokes its keys.
    w.supervisor
        .close_run_as(&w.registry, caller.clone(), &deeper.view.record.run_id)
        .unwrap();
    assert!(w.run(&deeper.view.record.run_id).is_ended());
    assert!(w
        .authenticate(deeper.credentials.as_ref().unwrap().run_key())
        .is_none());
    // A retried external id returns the existing Run and no credentials;
    // out of scope it is refused.
    let mut request = ChildRunRequest::under(RunParent::Run(child.clone()));
    request.issue_credentials = true;
    request.external_id = Some("job-1".to_string());
    let first = w
        .supervisor
        .open_child_run_as(&w.registry, caller.clone(), request.clone())
        .unwrap();
    assert!(first.credentials.is_some());
    let again = w
        .supervisor
        .open_child_run_as(&w.registry, caller.clone(), request.clone())
        .unwrap();
    assert_eq!(again.view.record.run_id, first.view.record.run_id);
    assert!(again.credentials.is_none(), "display once");
    let mut sib = ChildRunRequest::under(RunParent::Run(sibling.clone()));
    sib.issue_credentials = true;
    let sib = w
        .supervisor
        .open_child_run_as(&w.registry, RunActor::Operator, sib)
        .unwrap();
    let sib_run = sib.view.record.run_id.clone();
    let sibling_caller = w.caller(sib.credentials.as_ref().unwrap().run_key());
    // Another Run's key using the same external id gets its own child: the
    // id lives in each opening Run's namespace.
    let mut other = request;
    other.parent = RunParent::Run(sib_run.clone());
    let theirs = w
        .supervisor
        .open_child_run_as(&w.registry, sibling_caller, other)
        .unwrap();
    assert_ne!(theirs.view.record.run_id, first.view.record.run_id);
    assert_eq!(theirs.view.record.parent_run_id.as_ref(), Some(&sib_run));
}

#[test]
fn a_callers_external_ids_never_capture_or_reveal_an_operator_or_host_run() {
    let w = world();
    let (child, credentials) = w.operator_child();
    let caller = w.caller(credentials.run_key());
    let with_id = |parent: &RunId, external: &str| {
        let mut request = ChildRunRequest::under(RunParent::Run(parent.clone()));
        request.external_id = Some(external.to_string());
        request
    };
    // A Run Key squats an id it guesses the Host will use…
    let squat = w
        .supervisor
        .open_child_run_as(&w.registry, caller.clone(), with_id(&child, "ci-job-4711"))
        .unwrap();
    // …and the Host's later open with that id gets its OWN Run, where it
    // asked for it — never the Caller's Run in the Caller's subtree.
    for actor in [RunActor::Host, RunActor::Operator] {
        let hosts = w
            .supervisor
            .open_child_run_as(&w.registry, actor, with_id(&w.root, "ci-job-4711"))
            .unwrap();
        assert_ne!(hosts.view.record.run_id, squat.view.record.run_id);
        assert_eq!(hosts.view.record.parent_run_id.as_ref(), Some(&w.root));
    }
    // The Host's retry is idempotent within its own binding.
    let first = w
        .supervisor
        .open_child_run_as(&w.registry, RunActor::Host, with_id(&w.root, "job-77"))
        .unwrap();
    let again = w
        .supervisor
        .open_child_run_as(&w.registry, RunActor::Operator, with_id(&w.root, "job-77"))
        .unwrap();
    assert_eq!(again.view.record.run_id, first.view.record.run_id);
    // A Caller presenting the Host's id neither gets that Run nor a refusal
    // naming it: it opens a fresh child of its own.
    let probe = w
        .supervisor
        .open_child_run_as(&w.registry, caller.clone(), with_id(&child, "job-77"))
        .unwrap();
    assert_ne!(probe.view.record.run_id, first.view.record.run_id);
    assert_eq!(probe.view.record.parent_run_id.as_ref(), Some(&child));
    // And the Caller's own retry still returns its own Run.
    let retry = w
        .supervisor
        .open_child_run_as(&w.registry, caller, with_id(&child, "ci-job-4711"))
        .unwrap();
    assert_eq!(retry.view.record.run_id, squat.view.record.run_id);
}

#[test]
fn a_caller_tightens_limits_on_its_subtree_and_may_not_raise_them() {
    let w = world();
    let mut supervisor = Supervisor::new();
    let (child, credentials) = w.operator_child();
    let (sibling, _) = w.operator_child();
    let caller = w.caller(credentials.run_key());
    let set = |tokens: u64| RunLimitChange {
        set: RunLimits {
            token_per_run: Some(tokens),
            ..RunLimits::default()
        },
        clear: Vec::new(),
    };
    let results = supervisor
        .set_run_limits_as(
            &w.registry,
            std::slice::from_ref(&child),
            &set(500),
            caller.clone(),
            false,
        )
        .unwrap();
    assert!(results[0].1.is_ok(), "{:?}", results[0].1);
    // Raising the Caller's own ceiling is refused.
    let results = supervisor
        .set_run_limits_as(
            &w.registry,
            std::slice::from_ref(&child),
            &set(900),
            caller.clone(),
            false,
        )
        .unwrap();
    assert_eq!(results[0].1.as_ref().unwrap_err().code(), "raise-forbidden");
    // A sibling, and the root, are out of scope (also on a dry run).
    for dry_run in [false, true] {
        for target in [&sibling, &w.root] {
            let results = supervisor
                .set_run_limits_as(
                    &w.registry,
                    std::slice::from_ref(target),
                    &set(100),
                    caller.clone(),
                    dry_run,
                )
                .unwrap();
            assert_eq!(
                results[0].1.as_ref().unwrap_err().code(),
                "run-out-of-scope"
            );
        }
    }
    // A revoked key is refused inside the job.
    w.supervisor
        .revoke_run_keys(&w.registry, &child, false)
        .unwrap();
    let results = supervisor
        .set_run_limits_as(
            &w.registry,
            std::slice::from_ref(&child),
            &set(100),
            caller.clone(),
            false,
        )
        .unwrap();
    assert_eq!(
        results[0].1.as_ref().unwrap_err().code(),
        "credential-revoked"
    );
    let results = supervisor
        .set_run_limits_as(
            &w.registry,
            std::slice::from_ref(&child),
            &set(100),
            caller,
            true,
        )
        .unwrap();
    assert_eq!(
        results[0].1.as_ref().unwrap_err().code(),
        "credential-revoked"
    );
}

#[test]
fn a_caller_may_open_at_most_the_cap_of_open_children_under_one_parent() {
    let w = world();
    let (child, credentials) = w.operator_child();
    let parent = child.clone();
    // Seed MAX - 1 open children under the caller's own Run directly.
    w.registry
        .writer
        .call(move |ctx| {
            let store = ctx.store();
            for _ in 0..(MAX_CALLER_OPEN_CHILDREN - 1) {
                store.insert_run(&crate::ports::NewRun {
                    record: record(&RunId::mint(), false, Some(&parent)),
                    labels: Vec::new(),
                    limits: Default::default(),
                    identity: None,
                })?;
            }
            Ok(())
        })
        .unwrap();
    let caller = w.caller(credentials.run_key());
    // The 1,000th open child is allowed; the 1,001st is refused.
    w.open(caller.clone(), &child).unwrap();
    let err = w.open(caller, &child).unwrap_err();
    assert_eq!(err.code(), "too-many-children");
    // The Operator is not capped.
    w.open(RunActor::Operator, &child).unwrap();
}

#[test]
fn a_refresh_rotates_once_and_a_reused_refresh_credential_revokes_the_pair() {
    let w = world();
    let (child, first) = w.operator_child();
    let rotated = w
        .supervisor
        .refresh_credentials(&w.registry, first.refresh_credential())
        .unwrap();
    assert_eq!(rotated.run_id(), &child);
    // The old Run Key is refused on the next request; the new one works.
    assert!(w.authenticate(first.run_key()).is_none());
    assert!(w.authenticate(rotated.run_key()).is_some());
    assert_eq!(w.token_state(first.run_key()), TokenState::Revoked);
    assert_eq!(
        w.token_state(first.refresh_credential()),
        TokenState::Retired
    );
    // The new pair: same Run, issued by the used refresh credential, naming
    // what it replaced.
    let events = w.credential_events();
    let issued_new: Vec<_> = events
        .iter()
        .filter(|e| e.change == CredentialChange::Issued && e.run_id == child)
        .skip(2)
        .collect();
    assert_eq!(issued_new.len(), 2);
    assert!(issued_new
        .iter()
        .all(|e| e.issued_by.as_deref() == Some(first.refresh_token_id().as_str())));
    assert_eq!(
        issued_new[0].prior_token_id.as_deref(),
        Some(first.token_id().as_str())
    );
    let rotated_events: Vec<_> = events
        .iter()
        .filter(|e| e.change == CredentialChange::Rotated)
        .collect();
    assert_eq!(
        rotated_events.len(),
        2,
        "the used refresh and its paired key"
    );
    // A Run Key is not a refresh credential; an unknown one is unauthorized.
    assert_eq!(
        w.supervisor
            .refresh_credentials(&w.registry, rotated.run_key())
            .unwrap_err()
            .code(),
        "unauthorized"
    );
    let unknown = mint(TokenClass::Refresh).unwrap();
    assert_eq!(
        w.supervisor
            .refresh_credentials(&w.registry, unknown.secret())
            .unwrap_err()
            .code(),
        "unauthorized"
    );
    // Reuse: the retired credential again revokes the CURRENT pair, raises
    // the Anomaly, emits reuse-detected, and answers credential-revoked.
    let reuse = w
        .supervisor
        .refresh_credentials(&w.registry, first.refresh_credential())
        .unwrap_err();
    assert_eq!(reuse.code(), "credential-revoked");
    assert!(w.authenticate(rotated.run_key()).is_none());
    assert_eq!(w.token_state(rotated.run_key()), TokenState::Revoked);
    assert_eq!(
        w.token_state(rotated.refresh_credential()),
        TokenState::Revoked
    );
    assert!(w.run(&child).anomalies.contains(&Anomaly::CredentialReuse));
    let events = w.credential_events();
    let reuse_events: Vec<_> = events
        .iter()
        .filter(|e| e.change == CredentialChange::ReuseDetected)
        .collect();
    assert_eq!(reuse_events.len(), 1);
    assert_eq!(reuse_events[0].token_id, first.refresh_token_id());
    // A revoked refresh credential is refused with no change.
    let quiet = w.credential_events().len();
    assert_eq!(
        w.supervisor
            .refresh_credentials(&w.registry, rotated.refresh_credential())
            .unwrap_err()
            .code(),
        "credential-revoked"
    );
    assert_eq!(w.credential_events().len(), quiet);
    // The Operator's revoke-keys clears the Anomaly.
    let outcome = w
        .supervisor
        .revoke_run_keys(&w.registry, &child, true)
        .unwrap();
    assert!(outcome.reuse_cleared);
    assert!(!w.run(&child).anomalies.contains(&Anomaly::CredentialReuse));
}

#[test]
fn a_used_refresh_credential_is_no_kill_switch_once_reuse_is_detected_or_acknowledged() {
    let w = world();
    let refresh = |presented: &crate::domain::SecretString| {
        w.supervisor.refresh_credentials(&w.registry, presented)
    };
    // The Operator's acknowledgement retires every used credential: an old
    // one presented later is a plain refusal — no reuse, no Anomaly, no
    // revocation of the Run's current pair.
    let (child, first) = w.operator_child();
    let second = refresh(first.refresh_credential()).unwrap();
    w.supervisor
        .revoke_run_keys(&w.registry, &child, false)
        .unwrap();
    assert_eq!(
        w.token_state(first.refresh_credential()),
        TokenState::Revoked
    );
    let quiet = w.credential_events().len();
    assert_eq!(
        refresh(first.refresh_credential()).unwrap_err().code(),
        "credential-revoked"
    );
    assert!(!w.run(&child).anomalies.contains(&Anomaly::CredentialReuse));
    assert_eq!(w.credential_events().len(), quiet);
    let _ = second;

    // Reuse detection itself retires the whole used chain: after the
    // Operator acknowledges, NO older credential re-triggers it.
    let (other, one) = w.operator_child();
    let two = refresh(one.refresh_credential()).unwrap();
    let three = refresh(two.refresh_credential()).unwrap();
    assert_eq!(
        refresh(one.refresh_credential()).unwrap_err().code(),
        "credential-revoked"
    );
    assert!(w.run(&other).anomalies.contains(&Anomaly::CredentialReuse));
    assert_eq!(w.token_state(three.run_key()), TokenState::Revoked);
    for used in [one.refresh_credential(), two.refresh_credential()] {
        assert_eq!(w.token_state(used), TokenState::Revoked);
    }
    let outcome = w
        .supervisor
        .revoke_run_keys(&w.registry, &other, true)
        .unwrap();
    assert!(outcome.reuse_cleared);
    let quiet = w.credential_events().len();
    for used in [one.refresh_credential(), two.refresh_credential()] {
        assert_eq!(refresh(used).unwrap_err().code(), "credential-revoked");
    }
    assert!(!w.run(&other).anomalies.contains(&Anomaly::CredentialReuse));
    assert_eq!(w.credential_events().len(), quiet, "no reuse, no change");
}

#[test]
fn revoke_keys_cascades_to_credentials_issued_under_the_run_by_default() {
    let w = world();
    let (child, credentials) = w.operator_child();
    let caller = w.caller(credentials.run_key());
    let grandchild = w.open(caller, &child).unwrap();
    let gc = grandchild.credentials.unwrap();
    // The grandchild refreshes once: its refresh chain is still under the
    // child's key.
    let gc_rotated = w
        .supervisor
        .refresh_credentials(&w.registry, gc.refresh_credential())
        .unwrap();
    let great = w
        .open(
            w.caller(gc_rotated.run_key()),
            &grandchild.view.record.run_id,
        )
        .unwrap()
        .credentials
        .unwrap();
    // --no-cascade: only the child's own pair.
    let outcome = w
        .supervisor
        .revoke_run_keys(&w.registry, &child, false)
        .unwrap();
    assert_eq!(outcome.revoked.len(), 2);
    assert!(outcome
        .revoked
        .iter()
        .all(|r| !r.cascaded && r.run_id == child));
    assert!(w.authenticate(credentials.run_key()).is_none());
    assert!(w.authenticate(gc_rotated.run_key()).is_some());
    assert!(w.authenticate(great.run_key()).is_some());
    // The cascade (the default) reaches the grandchild's current pair and
    // the great-grandchild's, through the rotation.
    let outcome = w
        .supervisor
        .revoke_run_keys(&w.registry, &child, true)
        .unwrap();
    assert_eq!(outcome.revoked.len(), 4, "{outcome:?}");
    assert!(outcome.revoked.iter().all(|r| r.cascaded));
    assert!(w.authenticate(gc_rotated.run_key()).is_none());
    assert!(w.authenticate(great.run_key()).is_none());
    assert_eq!(
        w.token_state(great.refresh_credential()),
        TokenState::Revoked
    );
    // Revocation never ends a Run.
    assert!(!w.run(&grandchild.view.record.run_id).is_ended());
    // A missing Run is not found.
    assert_eq!(
        w.supervisor
            .revoke_run_keys(&w.registry, &RunId::from_wire("run_none"), true)
            .unwrap_err()
            .code(),
        "run-not-found"
    );
}

#[test]
fn a_job_authenticated_before_a_revocation_is_refused_after_it() {
    let w = world();
    let (child, credentials) = w.operator_child();
    // Authentication passed (the snapshot match is in hand) …
    let stale = w.caller(credentials.run_key());
    // … a job that committed before the revocation stands …
    let before = w.open(stale.clone(), &child).unwrap();
    // … then the key is revoked before the next job runs (the barrier).
    w.supervisor
        .revoke_run_keys(&w.registry, &child, false)
        .unwrap();
    let runs = w.registry.open_runs().unwrap().len();
    let events = w.credential_events().len();
    let err = w.open(stale.clone(), &child).unwrap_err();
    assert_eq!(err.code(), "credential-revoked");
    // Nothing it would have written exists.
    assert_eq!(w.registry.open_runs().unwrap().len(), runs);
    assert_eq!(w.credential_events().len(), events);
    let err = w
        .supervisor
        .close_run_as(&w.registry, stale, &before.view.record.run_id)
        .unwrap_err();
    assert_eq!(err.code(), "credential-revoked");
    assert!(!w.run(&before.view.record.run_id).is_ended());
    // The child that committed first stands, with its own live key.
    assert!(w
        .authenticate(before.credentials.as_ref().unwrap().run_key())
        .is_some());
    // A key the store never issued is unknown.
    let forged = RunActor::Caller {
        run_id: child,
        token_hash: TokenHash::of("hk_run_never"),
    };
    assert_eq!(w.open(forged, &w.root).unwrap_err().code(), "unauthorized");
}

#[test]
fn a_run_end_revokes_its_keys_and_children_that_outlive_it_keep_theirs() {
    let w = world();
    let (child, credentials) = w.operator_child();
    let grandchild = w.open(RunActor::Operator, &child).unwrap();
    w.supervisor
        .close_run_as(&w.registry, RunActor::Operator, &child)
        .unwrap();
    assert!(w.authenticate(credentials.run_key()).is_none());
    let revoked: Vec<_> = w
        .credential_events()
        .into_iter()
        .filter(|e| e.change == CredentialChange::Revoked && e.run_id == child)
        .collect();
    assert_eq!(revoked.len(), 2);
    assert!(revoked
        .iter()
        .all(|e| e.cause.as_deref() == Some("run-ended")));
    // The grandchild outlives its parent's end, with its key.
    assert!(!w.run(&grandchild.view.record.run_id).is_ended());
    assert!(w
        .authenticate(grandchild.credentials.as_ref().unwrap().run_key())
        .is_some());
}

/// One supervised launch at the job level: the Run created or kept and its
/// key committed with the record's generation.
fn launch(w: &World, run: &RunId, fresh: bool) -> crate::domain::MintedToken {
    let key = mint(TokenClass::RunKey).unwrap();
    let stub = key.stub();
    let supervised = SupervisedRun {
        instance: InstanceName::new(INSTANCE).unwrap(),
        run_id: run.clone(),
        wire_path: false,
        labels: Vec::new(),
        fresh,
        inputs: None,
    };
    let publisher = std::sync::Arc::clone(w.registry.run_publisher());
    w.registry
        .writer
        .call(move |ctx| {
            use crate::ports::RunStore as _;
            let generation = ctx
                .store()
                .bump_launch_generation(&InstanceName::new(INSTANCE).unwrap())?;
            let mut changes = ops::ensure_supervised(ctx.store(), &supervised, "t")?;
            changes.extend(ops::issue_launch(
                ctx.store(),
                &supervised.run_id,
                generation,
                LaunchCredentials {
                    key: stub,
                    refresh: None,
                },
                "t",
            )?);
            ops::commit(ctx, &publisher, changes)
        })
        .unwrap();
    key
}

fn transition(w: &World, state: crate::domain::LifecycleState, fate: RunFate) {
    let publisher = std::sync::Arc::clone(w.registry.run_publisher());
    w.registry
        .writer
        .call(move |ctx| {
            // As the transition job runs it: the exit's revocations first, in
            // the job's own transaction, then the projection.
            let name = InstanceName::new(INSTANCE).unwrap();
            let mut changes = ops::revoke_on_exit(ctx.store(), &name, state, fate, "t")?;
            changes.extend(ops::project_transition(
                ctx.store(),
                &name,
                state,
                fate,
                "crashed",
                "t",
            )?);
            ops::commit(ctx, &publisher, changes)
        })
        .unwrap();
}

#[test]
fn each_launch_has_its_own_key_revoked_when_its_process_exits() {
    use crate::domain::LifecycleState;
    let w = world();
    let run = w.root.clone();
    let first = launch(&w, &run, false);
    let matched = w.authenticate(first.secret()).unwrap();
    assert!(matched.supervised);
    assert_eq!(matched.run_id, run);
    // Two forced relaunches: each crash records the exit and revokes that
    // launch's key in the same job; each relaunch keeps the Run.
    transition(&w, LifecycleState::Failed, RunFate::Relaunching);
    let second = launch(&w, &run, false);
    transition(&w, LifecycleState::Failed, RunFate::Relaunching);
    let third = launch(&w, &run, false);
    assert!(w.authenticate(first.secret()).is_none());
    assert!(w.authenticate(second.secret()).is_none());
    assert!(w.authenticate(third.secret()).is_some());
    assert_eq!(w.run(&run).run_id, run);
    assert!(!w.run(&run).is_ended());
    let rows = w
        .registry
        .store
        .snapshot(|store| store.run_tokens(&run))
        .unwrap();
    let generations: Vec<_> = rows.iter().map(|r| r.launch_generation).collect();
    assert_eq!(generations, vec![Some(1), Some(2), Some(3)]);
    let exited: Vec<_> = w
        .credential_events()
        .into_iter()
        .filter(|e| e.change == CredentialChange::Revoked)
        .collect();
    assert_eq!(exited.len(), 2);
    assert!(exited
        .iter()
        .all(|e| e.cause.as_deref() == Some("process-exited")));
    // An adoption (a kept Run, no launch) changes no credential.
    let before = w.credential_events().len();
    let publisher = std::sync::Arc::clone(w.registry.run_publisher());
    let adopt = SupervisedRun {
        instance: InstanceName::new(INSTANCE).unwrap(),
        run_id: run.clone(),
        wire_path: false,
        labels: Vec::new(),
        fresh: false,
        inputs: None,
    };
    w.registry
        .writer
        .call(move |ctx| {
            let changes = ops::ensure_supervised(ctx.store(), &adopt, "t")?;
            ops::commit(ctx, &publisher, changes)
        })
        .unwrap();
    assert_eq!(w.credential_events().len(), before);
    assert!(w.authenticate(third.secret()).is_some());
    // A stop records the exit and ends the Run: the key is revoked once,
    // `process-exited`; the snapshot no longer holds it.
    transition(&w, LifecycleState::Stopped, RunFate::Terminal);
    assert!(w.run(&run).is_ended());
    assert!(w.authenticate(third.secret()).is_none());
    assert!(w.registry.run_snapshot().keys.is_empty());
}

#[test]
fn a_launch_with_a_mapped_refresh_credential_issues_a_supervised_pair_its_exit_revokes() {
    // An Adapter that maps `run.refresh_key` gets a refresh credential with
    // the launch's key: both carry the launch generation, only the key is
    // indexed, the refresh credential resolves as supervised (so the host
    // refuses it beyond loopback), and the exit revokes both.
    let w = world();
    let run = w.root.clone();
    let key = mint(TokenClass::RunKey).unwrap();
    let refresh = mint(TokenClass::Refresh).unwrap();
    let (key_stub, refresh_stub) = (key.stub(), refresh.stub());
    let publisher = std::sync::Arc::clone(w.registry.run_publisher());
    let r = run.clone();
    w.registry
        .writer
        .call(move |ctx| {
            use crate::ports::RunStore as _;
            let name = InstanceName::new(INSTANCE).unwrap();
            let generation = ctx.store().bump_launch_generation(&name)?;
            let changes = ops::issue_launch(
                ctx.store(),
                &r,
                generation,
                LaunchCredentials {
                    key: key_stub,
                    refresh: Some(refresh_stub),
                },
                "t",
            )?;
            ops::commit(ctx, &publisher, changes)
        })
        .unwrap();
    assert!(w.authenticate(key.secret()).unwrap().supervised);
    assert!(w.authenticate(refresh.secret()).is_none(), "never indexed");
    match crate::domain::registry_keys::presented_state(&w.registry.store, refresh.secret())
        .unwrap()
    {
        crate::domain::PresentedState::Active(matched) => {
            assert!(matched.supervised);
            assert_eq!(matched.class, TokenClass::Refresh);
        }
        other => panic!("the refresh credential is active: {other:?}"),
    }
    let rows = w
        .registry
        .store
        .snapshot(|store| store.run_tokens(&run))
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.launch_generation == Some(1)));
    assert!(rows[0].paired_hash.unwrap().ct_eq(&rows[1].hash));
    transition(
        &w,
        crate::domain::LifecycleState::Failed,
        RunFate::Relaunching,
    );
    assert_eq!(w.token_state(key.secret()), TokenState::Revoked);
    assert_eq!(w.token_state(refresh.secret()), TokenState::Revoked);
    // An issue for a Run that is gone writes nothing.
    let stub = mint(TokenClass::RunKey).unwrap().stub();
    let publisher = std::sync::Arc::clone(w.registry.run_publisher());
    let missing = w
        .registry
        .writer
        .call(move |ctx| {
            let changes = ops::issue_launch(
                ctx.store(),
                &RunId::from_wire("run_gone"),
                9,
                LaunchCredentials {
                    key: stub,
                    refresh: None,
                },
                "t",
            )?;
            let empty = changes.credentials.is_empty() && changes.keys.is_empty();
            ops::commit(ctx, &publisher, changes)?;
            Ok(empty)
        })
        .unwrap();
    assert!(missing);
}

#[test]
fn a_relaunch_revokes_every_earlier_generation_even_when_its_exit_was_never_recorded() {
    // The exit's revocation is the first line; the launch record is the
    // second: a new generation is issued only after every earlier one of
    // the Run is out of service.
    let w = world();
    let run = w.root.clone();
    let first = launch(&w, &run, false);
    // No exit transition is recorded for the first launch.
    let second = launch(&w, &run, false);
    assert!(w.authenticate(first.secret()).is_none());
    assert_eq!(w.token_state(first.secret()), TokenState::Revoked);
    assert!(w.authenticate(second.secret()).is_some());
    let revoked: Vec<_> = w
        .credential_events()
        .into_iter()
        .filter(|e| e.change == CredentialChange::Revoked)
        .collect();
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].launch_generation, Some(1));
    assert_eq!(revoked[0].cause.as_deref(), Some("process-exited"));
}

#[test]
fn removing_an_instance_drops_its_runs_keys_from_the_index() {
    // `hekma rm --force` deletes the instance with its Runs and their
    // credential rows: the published key index must not keep honouring
    // those keys (a snapshot-only reader would).
    let w = world();
    let run = w.root.clone();
    let key = launch(&w, &run, false);
    let (_, child) = w.operator_child();
    assert!(w.authenticate(key.secret()).is_some());
    assert!(w.authenticate(child.run_key()).is_some());
    w.registry
        .remove(INSTANCE, crate::domain::RemoveDisposition::Retain, true)
        .unwrap();
    assert!(w.authenticate(key.secret()).is_none());
    assert!(w.authenticate(child.run_key()).is_none());
    assert!(w.registry.run_snapshot().keys.is_empty());
}

#[test]
fn a_stop_that_ends_the_run_revokes_every_remaining_token_in_the_same_job() {
    use crate::domain::LifecycleState;
    let w = world();
    let run = w.root.clone();
    let key = launch(&w, &run, false);
    // A Control-API pair on the process-owning Run itself (an embedder
    // might issue one): no launch generation, so only the end revokes it.
    let extra = mint(TokenClass::RunKey).unwrap();
    let extra_stub = extra.stub();
    let r = run.clone();
    let publisher = std::sync::Arc::clone(w.registry.run_publisher());
    w.registry
        .writer
        .call(move |ctx| {
            let record = ctx.store().run_record(&r)?.unwrap();
            let mut changes = ops::RunChanges::default();
            super::keys::issue(
                ctx.store(),
                &mut changes,
                &record,
                super::keys::Issue {
                    token: extra_stub,
                    launch_generation: None,
                    paired: None,
                    issued_by_hash: None,
                    issuer: "host".to_string(),
                    custody_eligible: true,
                    prior_token_id: None,
                },
                "t",
            )?;
            ops::commit(ctx, &publisher, changes)
        })
        .unwrap();
    assert!(w.authenticate(extra.secret()).is_some());
    transition(&w, LifecycleState::Stopped, RunFate::Terminal);
    assert!(w.authenticate(key.secret()).is_none());
    assert!(w.authenticate(extra.secret()).is_none());
    let causes: Vec<_> = w
        .credential_events()
        .into_iter()
        .filter(|e| e.change == CredentialChange::Revoked)
        .map(|e| e.cause.unwrap())
        .collect();
    assert_eq!(causes, vec!["process-exited", "run-ended"]);
}

#[test]
fn exposed_tokens_are_revoked_and_unknown_ones_change_nothing() {
    let w = world();
    let (_, credentials) = w.operator_child();
    let unknown = mint(TokenClass::RunKey).unwrap();
    let count = w
        .supervisor
        .revoke_exposed(
            &w.registry,
            &[
                crate::domain::SecretString::new(credentials.run_key().expose_secret()),
                crate::domain::SecretString::new(unknown.secret().expose_secret()),
            ],
        )
        .unwrap();
    assert_eq!(count, 1);
    assert!(w.authenticate(credentials.run_key()).is_none());
    let event = w
        .credential_events()
        .into_iter()
        .find(|e| e.change == CredentialChange::Revoked)
        .unwrap();
    assert_eq!(event.cause.as_deref(), Some("exposed-in-url"));
    // Twice is a no-op.
    assert_eq!(
        w.supervisor
            .revoke_exposed(
                &w.registry,
                &[crate::domain::SecretString::new(
                    credentials.run_key().expose_secret()
                )],
            )
            .unwrap(),
        0
    );
}

#[test]
fn the_key_index_reloads_at_open_and_revocations_survive_a_reopen() {
    let w = world();
    let (child, live) = w.operator_child();
    let (_, revoked) = w.operator_child();
    w.supervisor
        .revoke_run_keys(&w.registry, revoked.run_id(), false)
        .unwrap();
    let base = w.state.path().to_path_buf();
    let World {
        state, registry, ..
    } = w;
    drop(registry);
    let reopened = Registry::open(Some(base)).unwrap();
    let snapshot = reopened.run_snapshot();
    let live_hash = hash_presented(live.run_key());
    let entry = snapshot.key(&live_hash).expect("the live key reloads");
    assert_eq!(entry.run_id, child);
    assert!(snapshot.key(&hash_presented(revoked.run_key())).is_none());
    assert_eq!(snapshot.keys.len(), 1, "refresh credentials never load");
    drop(state);
}

#[test]
fn two_tokens_sharing_a_token_id_are_recorded_and_authenticated_independently() {
    // R1.3: the token id is a label — force a collision and both tokens
    // still resolve by their full hash, to their own Runs.
    let w = world();
    test_seams::force_token_id(Some("abcdefabcdef"));
    let a = mint(TokenClass::RunKey).unwrap();
    let b = mint(TokenClass::RunKey).unwrap();
    test_seams::force_token_id(None);
    assert_ne!(a.secret().expose_secret(), b.secret().expose_secret());
    let (a_run, _) = w.operator_child();
    let (b_run, _) = w.operator_child();
    for (token, run) in [(&a, &a_run), (&b, &b_run)] {
        let stub = token.stub();
        let run = run.clone();
        let publisher = std::sync::Arc::clone(w.registry.run_publisher());
        w.registry
            .writer
            .call(move |ctx| {
                let record = ctx.store().run_record(&run)?.unwrap();
                let mut changes = ops::RunChanges::default();
                // The forced id rides the row (a label, not a key).
                ctx.store().insert_token(&crate::ports::TokenRow {
                    hash: stub.hash,
                    token_id: "abcdefabcdef".to_string(),
                    class: TokenClass::RunKey,
                    run_id: record.run_id.clone(),
                    launch_generation: None,
                    paired_hash: None,
                    issued_by_hash: None,
                    custody_eligible: false,
                    state: TokenState::Active,
                    issued_at: "t".to_string(),
                    revoked_at: None,
                    revoke_cause: None,
                })?;
                changes.keys.push(super::snapshot::KeyChange::Add(
                    stub.hash,
                    super::snapshot::KeyEntry {
                        class: TokenClass::RunKey,
                        run_id: record.run_id.clone(),
                        token_id: "abcdefabcdef".to_string(),
                        launch_generation: None,
                        custody_eligible: false,
                        process_owner: false,
                    },
                ));
                ops::commit(ctx, &publisher, changes)
            })
            .unwrap();
    }
    assert_eq!(w.authenticate(a.secret()).unwrap().run_id, a_run);
    assert_eq!(w.authenticate(b.secret()).unwrap().run_id, b_run);
    // Revoking one by its hash leaves the other.
    w.supervisor
        .revoke_run_keys(&w.registry, &a_run, false)
        .unwrap();
    assert!(w.authenticate(a.secret()).is_none());
    assert!(w.authenticate(b.secret()).is_some());
}

#[test]
fn the_snapshot_subject_of_a_credential_event_is_its_run() {
    let w = world();
    let (child, _) = w.operator_child();
    let event = w.credential_events().pop().unwrap();
    let event = EngineEvent::Credential(Box::new(event));
    assert_eq!(w.registry.event_subject_run(&event), Some(child.clone()));
    match super::snapshot::subject_labels(&w.registry.run_snapshot(), &event) {
        super::snapshot::SubjectLabels::Found(_) => {}
        other => panic!("expected the Run in the snapshot, got {other:?}"),
    }
}

#[test]
fn a_callers_open_time_ceilings_land_in_the_caller_layer_and_only_tighten() {
    // A Caller's ceilings at open must never become the child's Operator/Host
    // layer (which outranks every Policy, the default included).
    let w = world();
    let (child, credentials) = w.operator_child();
    let caller = w.caller(credentials.run_key());
    let mut request = ChildRunRequest::under(RunParent::Run(child.clone()));
    request.limits = RunLimits {
        token_per_run: Some(700),
        ..RunLimits::default()
    };
    let opened = w
        .supervisor
        .open_child_run_as(&w.registry, caller, request)
        .unwrap();
    let id = opened.view.record.run_id;
    let reader = w.registry.reader();
    assert_eq!(reader.run_limits(&id).unwrap(), RunLimits::default());
    assert_eq!(
        reader.run_caller_limits(&id).unwrap().token_per_run,
        Some(700)
    );
    let resolution = w.registry.run_view(&id).unwrap().policy.unwrap();
    assert_eq!(resolution.tokens.per_run, Some(700));
    assert_eq!(resolution.sources.tokens_per_run, LimitSource::Caller);
    // The Operator's open keeps writing the Run layer.
    let mut request = ChildRunRequest::under(RunParent::Run(child));
    request.limits = RunLimits {
        token_per_run: Some(700),
        ..RunLimits::default()
    };
    let opened = w
        .supervisor
        .open_child_run_as(&w.registry, RunActor::Operator, request)
        .unwrap();
    assert_eq!(
        reader
            .run_limits(&opened.view.record.run_id)
            .unwrap()
            .token_per_run,
        Some(700)
    );
}
