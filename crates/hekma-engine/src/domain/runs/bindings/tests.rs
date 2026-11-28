//! Binding validation and job tests (story 23-1).

use std::sync::Arc;

use super::*;
use crate::store::{SqliteStore, StoreWriter};

#[test]
fn prefixes_follow_the_segment_rules_and_the_reserved_prefix_is_refused() {
    for good in [
        "/",
        "/openai",
        "/a/b-c/d.e_f~g",
        &format!("/{}", "a".repeat(127)),
    ] {
        assert!(validate_prefix(good).is_ok(), "{good}");
    }
    for bad in [
        "",
        "openai",
        "/openai/",
        "//x",
        "/a//b",
        "/a/../b",
        "/./a",
        "/a b",
        "/a?b",
        "/_hekma",
        "/_HEKMA/runs",
        "/_hekma/x",
        &format!("/{}", "a".repeat(128)),
    ] {
        assert!(
            matches!(validate_prefix(bad), Err(BindingError::Invalid { ref field, .. }) if field == "prefix"),
            "{bad}"
        );
    }
    // A prefix that only starts with the reserved word is fine.
    assert!(validate_prefix("/_hekmax").is_ok());
}

#[test]
fn upstreams_must_be_plain_http_urls_with_a_host() {
    for good in [
        "https://api.openai.com/v1",
        "http://127.0.0.1:9000",
        "HTTPS://Example.com",
    ] {
        assert!(validate_upstream("upstream", good).is_ok(), "{good}");
    }
    for bad in [
        "ftp://x",
        "api.openai.com",
        "https://user:pw@api.example.com/v1",
        "https://api.example.com@evil/",
        "https://api.example.com/v1?key=1",
        "https://api.example.com/v1#frag",
        "https:///v1",
        "https://exa mple.com",
    ] {
        assert!(validate_upstream("upstream", bad).is_err(), "{bad}");
    }
}

#[test]
fn a_custodied_key_must_be_a_secret_reference() {
    assert!(validate_custodied_key("custodied_key", "secret:OPENAI_KEY").is_ok());
    for bad in ["sk-live-123", "secret:", "secret:a b", "env:X"] {
        assert!(
            validate_custodied_key("custodied_key", bad).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn labels_and_names_are_validated() {
    assert!(validate_labels(&[("team".into(), "core".into())]).is_ok());
    assert!(validate_labels(&[("team".into(), "a".into()), ("team".into(), "b".into())]).is_err());
    assert!(validate_labels(&[("bad key".into(), "a".into())]).is_err());
    assert!(validate_name("name", "openai").is_ok());
    assert!(validate_name("name", "Bad Name").is_err());
}

#[test]
fn the_table_matches_the_longest_prefix_on_a_segment_boundary() {
    let row = |name: &str, prefix: &str| BindingRow {
        name: name.into(),
        prefix: prefix.into(),
        upstream: format!("http://{name}"),
        custodied_key: None,
        labels: Vec::new(),
        created_at: "t".into(),
        updated_at: "t".into(),
    };
    let table = table_of(&[
        row("root", "/"),
        row("oa", "/openai"),
        row("oav1", "/openai/v1"),
    ]);
    let hit = |path: &str| {
        table
            .matching(path)
            .map(|(entry, rest)| (entry.name.clone(), rest.to_string()))
    };
    assert_eq!(
        hit("/openai/v1/chat"),
        Some(("oav1".into(), "/chat".into()))
    );
    assert_eq!(hit("/openai/v2"), Some(("oa".into(), "/v2".into())));
    assert_eq!(hit("/openai"), Some(("oa".into(), "".into())));
    assert_eq!(
        hit("/openai2/x"),
        Some(("root".into(), "/openai2/x".into()))
    );
    assert!(table.by_name("oa").is_some());
    let no_root = table_of(&[row("oa", "/openai")]);
    assert!(no_root.matching("/openai2").is_none());
    assert!(no_root.matching("/other").is_none());
}

fn writer() -> (
    tempfile::TempDir,
    StoreWriter,
    SqliteStore,
    Arc<SnapshotPublisher>,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("state.db");
    let store = SqliteStore::open(&db).unwrap();
    store.attach_calls(None).unwrap();
    let writer = StoreWriter::spawn(store).unwrap();
    let reader = SqliteStore::open_read_only(&db).unwrap();
    (tmp, writer, reader, Arc::new(SnapshotPublisher::default()))
}

fn run_job<R: Send + 'static>(
    writer: &StoreWriter,
    publisher: &Arc<SnapshotPublisher>,
    work: impl FnOnce(&mut BindingJob<'_, '_>) -> Result<R, BindingError> + Send + 'static,
) -> Result<R, BindingError> {
    let publisher = Arc::clone(publisher);
    let slot: Arc<std::sync::Mutex<Option<BindingError>>> = Arc::default();
    let error = Arc::clone(&slot);
    let result = writer.call(move |ctx| {
        let mut job = BindingJob {
            ctx,
            publisher: &publisher,
        };
        match work(&mut job) {
            Ok(value) => Ok(Some(value)),
            Err(BindingError::Store(err)) => Err(err),
            Err(other) => {
                *error.lock().unwrap() = Some(other);
                Ok(None)
            }
        }
    });
    match result {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Err(slot.lock().unwrap().take().unwrap()),
        Err(err) => Err(BindingError::Store(err)),
    }
}

fn spec(name: &str, prefix: &str) -> BindingSpec {
    BindingSpec {
        name: name.into(),
        prefix: prefix.into(),
        upstream: "https://api.openai.com/v1".into(),
        labels: vec![("team".into(), "core".into())],
        custodied_key: None,
    }
}

#[test]
fn create_change_route_credential_and_delete_each_emit_one_event_and_publish() {
    let (_tmp, writer, reader, publisher) = writer();
    let created = run_job(&writer, &publisher, |job| {
        create(job, spec("openai", "/openai"), None, "t1")
    })
    .unwrap();
    assert_eq!(created.events.len(), 1);
    assert_eq!(created.binding.as_ref().unwrap().prefix, "/openai");
    assert!(publisher.current().bindings.by_name("openai").is_some());
    // A duplicate name, an equal prefix and the legacy root are refused.
    assert!(matches!(
        run_job(&writer, &publisher, |job| create(
            job,
            spec("openai", "/x"),
            None,
            "t"
        )),
        Err(BindingError::Exists { .. })
    ));
    assert!(matches!(
        run_job(&writer, &publisher, |job| create(
            job,
            spec("other", "/openai"),
            None,
            "t"
        )),
        Err(BindingError::PrefixTaken { .. })
    ));
    assert!(matches!(
        run_job(&writer, &publisher, |job| {
            create(job, spec("root", "/"), Some("run_LEGACY".into()), "t")
        }),
        Err(BindingError::LegacyRoute { .. })
    ));
    let changed = run_job(&writer, &publisher, |job| {
        update(
            job,
            "openai",
            BindingPatch {
                labels: Some(vec![("team".into(), "edge".into())]),
                custodied_key: Some(Some("secret:OPENAI".into())),
                ..BindingPatch::default()
            },
            None,
            "t2",
        )
    })
    .unwrap();
    assert_eq!(changed.binding.as_ref().unwrap().labels[0].1, "edge");
    assert_eq!(changed.notes, vec![custody_note()]);
    let routed = run_job(&writer, &publisher, |job| {
        put_route(
            job,
            "openai",
            RouteSpec {
                name: "backup".into(),
                upstream: "https://backup.example.com/v1".into(),
                custodied_key: None,
            },
            "t3",
        )
    })
    .unwrap();
    assert_eq!(routed.binding.unwrap().routes.len(), 1);
    // Issue twice: the second revokes the first in the same job.
    let first = bindings_mint(&writer, &publisher, BindingCredentialClass::ProxyAccess);
    let second = bindings_mint(&writer, &publisher, BindingCredentialClass::ProxyAccess);
    assert_ne!(first, second);
    let listed = credentials(&reader, "openai").unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed.iter().filter(|c| c.state == "active").count(), 1);
    let revoked = run_job(&writer, &publisher, |job| {
        revoke_credential(job, "openai", BindingCredentialClass::ProxyAccess, "t5")
    })
    .unwrap();
    assert_eq!(revoked.events.len(), 1);
    let again = run_job(&writer, &publisher, |job| {
        revoke_credential(job, "openai", BindingCredentialClass::ProxyAccess, "t5")
    })
    .unwrap();
    assert!(again.events.is_empty() && again.notes.len() == 1);
    let removed = run_job(&writer, &publisher, |job| {
        remove_route(job, "openai", "backup", "t6")
    })
    .unwrap();
    assert!(removed.binding.unwrap().routes.is_empty());
    assert!(matches!(
        run_job(&writer, &publisher, |job| remove_route(
            job, "openai", "backup", "t6"
        )),
        Err(BindingError::RouteNotFound { .. })
    ));
    assert_eq!(list(&reader, None, 10).unwrap().len(), 1);
    assert!(list(&reader, Some("openai"), 10).unwrap().is_empty());
    let deleted = run_job(&writer, &publisher, |job| delete(job, "openai", "t7")).unwrap();
    assert_eq!(deleted.events.len(), 1);
    assert!(publisher.current().bindings.by_name("openai").is_none());
    assert!(matches!(
        run_job(&writer, &publisher, |job| delete(job, "openai", "t7")),
        Err(BindingError::NotFound { .. })
    ));
    assert!(credentials(&reader, "openai").is_err());
}

fn bindings_mint(
    writer: &StoreWriter,
    publisher: &Arc<SnapshotPublisher>,
    class: BindingCredentialClass,
) -> String {
    let minted = mint(class).unwrap();
    let stub = minted.stub();
    assert!(minted
        .secret()
        .expose_secret()
        .starts_with(class.token_class().prefix()));
    let (outcome, token_id) = run_job(writer, publisher, move |job| {
        issue_credential(job, "openai", class, stub, "t4")
    })
    .unwrap();
    assert!(!outcome.events.is_empty());
    // No event carries the token.
    for (event, _) in &outcome.events {
        let json = serde_json::to_string(event).unwrap();
        assert!(!json.contains(minted.secret().expose_secret()));
    }
    token_id
}

#[test]
fn classes_round_trip_and_the_issued_debug_never_shows_the_token() {
    for class in [
        BindingCredentialClass::Announce,
        BindingCredentialClass::ProxyAccess,
    ] {
        assert_eq!(
            BindingCredentialClass::from_wire(class.as_str()),
            Some(class)
        );
    }
    assert_eq!(BindingCredentialClass::from_wire("x"), None);
    let issued = IssuedBindingCredential {
        class: BindingCredentialClass::Announce,
        token_id: "abc".into(),
        token: SecretString::new("hk_ann_SENTINEL"),
    };
    assert!(!format!("{issued:?}").contains("SENTINEL"));
}

#[test]
fn limits_bound_bindings_and_routes() {
    let (_tmp, writer, _reader, publisher) = writer();
    for i in 0..MAX_BINDINGS {
        run_job(&writer, &publisher, move |job| {
            create(job, spec(&format!("b{i}"), &format!("/p{i}")), None, "t")
        })
        .unwrap();
    }
    assert!(matches!(
        run_job(&writer, &publisher, |job| create(
            job,
            spec("extra", "/extra"),
            None,
            "t"
        )),
        Err(BindingError::TooMany { .. })
    ));
    for i in 0..MAX_BINDING_ROUTES {
        run_job(&writer, &publisher, move |job| {
            put_route(
                job,
                "b0",
                RouteSpec {
                    name: format!("r{i}"),
                    upstream: "http://x".into(),
                    custodied_key: None,
                },
                "t",
            )
        })
        .unwrap();
    }
    assert!(matches!(
        run_job(&writer, &publisher, |job| {
            put_route(
                job,
                "b0",
                RouteSpec {
                    name: "r99".into(),
                    upstream: "http://x".into(),
                    custodied_key: None,
                },
                "t",
            )
        }),
        Err(BindingError::TooMany { .. })
    ));
}

#[test]
fn a_patch_tells_an_absent_custodied_key_from_a_null_one() {
    let absent: BindingPatch = serde_json::from_str(r#"{"prefix":"/x"}"#).unwrap();
    assert_eq!(
        absent.custodied_key, None,
        "absent leaves the key unchanged"
    );
    let null: BindingPatch = serde_json::from_str(r#"{"custodied_key":null}"#).unwrap();
    assert_eq!(null.custodied_key, Some(None), "null clears the key");
    let set: BindingPatch =
        serde_json::from_str(r#"{"custodied_key":"secret:OPENAI_KEY"}"#).unwrap();
    assert_eq!(
        set.custodied_key,
        Some(Some("secret:OPENAI_KEY".to_string())),
        "a value sets it"
    );
}
