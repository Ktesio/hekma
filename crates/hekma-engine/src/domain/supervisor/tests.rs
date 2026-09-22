use super::*;
use crate::adapter::{AdapterRef, StartLaunch};
use crate::domain::RestartPolicy;
use std::time::Instant;

/// A fast backoff schedule so the crash/restart/crash-loop lib tests never
/// sleep for real seconds (production stays 1s×2 cap 60s — Task 2 guards it).
fn fast_backoff() -> BackoffSchedule {
    BackoffSchedule::with_base_and_cap(Duration::from_millis(5), Duration::from_millis(20))
}

/// Write a manifest whose `[lifecycle.start]` exec is `fake_agent` + `args`.
fn write_fake_manifest(dir: &Path, kind: &str, args: &[&str]) {
    let bin = hekma_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n",
            exec = bin.to_string_lossy(),
        );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// Register a `fake_agent`-backed instance under `name` with `args`, in a
/// fresh state dir. Returns the (state dir, manifest dir, registry).
fn setup_fake(name: &str, args: &[&str]) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    write_fake_manifest(manifest.path(), name, args);
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry
        .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    (state, manifest, registry)
}

/// Story 2-2: write a `fake_agent` manifest with `args` PLUS a `[config]`
/// mapping section (`config_toml` is the section body, e.g.
/// `"[config.model]\nflag = \"--model\"\n"`). Used by the manifest end-to-end
/// mapping proofs.
fn write_fake_manifest_with_config(dir: &Path, kind: &str, args: &[&str], config_toml: &str) {
    let bin = hekma_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n\n\
             {config_toml}",
            exec = bin.to_string_lossy(),
        );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// Register a `fake_agent`-backed instance carrying a `[config]` mapping.
/// Returns the (state dir, manifest dir, registry).
fn setup_fake_with_config(
    name: &str,
    args: &[&str],
    config_toml: &str,
) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    write_fake_manifest_with_config(manifest.path(), name, args, config_toml);
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry
        .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    (state, manifest, registry)
}

/// Poll for the `fake_agent` readiness marker (`--marker <path>`, written
/// at startup right after the ready line and BEFORE the `--dump` file) —
/// the AI-35/38 readiness handshake (story 11-5): a `_live` test proceeds
/// as soon as the spawned agent is PROVABLY up, on every OS, instead of
/// being gated off macOS/Windows on an OS-fragile wall-clock assumption
/// about spawn latency. The generous bound absorbs loaded CI runners (and
/// the instrumented coverage run); the poll returns the moment the file
/// appears, so the happy path pays nothing.
fn wait_for_marker(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if path.exists() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "readiness marker never appeared at {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll for a `--dump` file to appear (the spawned `fake_agent` writes it at
/// startup, right after its readiness `--marker`) and return its contents,
/// bounded — avoids racing the spawn. Call [`Self::wait_for_marker`] first
/// for the readiness handshake.
fn wait_for_dump(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
        assert!(
            Instant::now() < deadline,
            "dump file never appeared at {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until `poll_once` reports the crash (returns its plans), bounded.
fn wait_for_crash(sup: &mut Supervisor, registry: &Registry) -> Vec<RestartPlan> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let plans = sup.poll_once(registry);
        // Once the instance has crashed it is no longer in `running`; the
        // crash transition has landed. `poll_once` returns the plan on the
        // pass that detects the exit.
        if !plans.is_empty() {
            return plans;
        }
        // Also stop once nothing is supervised AND state is failed (a `never`
        // policy returns no plan but still crashes).
        if sup.running.is_empty() {
            return plans;
        }
        assert!(Instant::now() < deadline, "crash was never detected");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn state_of(registry: &Registry, name: &str) -> LifecycleState {
    registry
        .lookup(&InstanceName::new(name).unwrap())
        .unwrap()
        .state
}

// ---- Story 2-2: unified→native config mapping proven at start (AC-A/AC-B) ----

#[test]
fn mock_native_start_maps_model_to_the_declared_env_target() {
    // AC-A + AC8 (the MOCK/native proof). The builtin `mock` is INERT (no live
    // process — NativeHasNoLaunch), so a `mock` start cannot spawn to observe.
    // Per the recorded inert-mock strategy (Decision 8), we assert on the
    // MAPPED launch the mapping application PRODUCES: register a mock, set the
    // documented `model` key (2-1), then resolve the mock's code-declared
    // mapping + the effective config and apply — the mock's declared native
    // target (env `MODEL`) must carry the value. This is exactly the transform
    // the start seam runs; a launchable native agent is a manifest adapter.
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry.register("mck", "mock").unwrap();
    let name = InstanceName::new("mck").unwrap();
    registry.set_config(&name, "model", "gpt-4").unwrap();

    // Resolve exactly as start_inner would for a native adapter.
    let (kind, manifest_path, launch) = registry.adapter_launch_facts(&name).unwrap();
    assert_eq!(kind, "mock");
    assert!(manifest_path.is_none(), "mock is native (no manifest)");
    assert!(launch.is_none(), "mock is native (no snapshotted launch)");
    let effective = registry
        .effective_config(&name, crate::domain::ConfigLayer::empty())
        .unwrap();
    let mapping = adapter::resolve_config_mapping(&kind, manifest_path.as_deref()).unwrap();
    // The mock declares `model` → env `MODEL`.
    assert_eq!(mapping.target("model").unwrap().env_var(), Some("MODEL"));

    // Apply onto a bare launch (the mock has no [lifecycle.start] template;
    // this is the launch shape the mapping would produce).
    let mut launch = StartLaunch {
        exec: "mock".to_string(),
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
    };
    adapter::apply_config_mapping(
        &mut launch,
        &mapping,
        &effective,
        &std::collections::BTreeMap::new(),
        &registry.agent_home(&name),
    )
    .unwrap();
    assert_eq!(
        launch.env.get("MODEL").map(String::as_str),
        Some("gpt-4"),
        "the documented model key must land in the mock's declared env target"
    );
}

// ---- Story 11-2: AI-27 (env-shadow visibility) + AI-39 (secret→flag runtime) ----

/// Story 11-2: write a manifest whose `[lifecycle.start]` declares a BASE
/// env var (`BASEVAR`) and whose exec is a guaranteed-missing binary — so
/// the spawn (which happens AFTER the diagnostic emissions) fails on EVERY
/// OS, keeping the emission proofs OS-agnostic (no live process needed,
/// mirroring the `_live` gates' rationale).
fn write_manifest_with_base_env(dir: &Path, kind: &str, config_toml: &str) {
    let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = \"ktesio-definitely-missing-binary\"\nargs = []\nenv = {{ BASEVAR = \"base-value\" }}\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n\n\
             {config_toml}"
        );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// Register an instance from [`Self::write_manifest_with_base_env`].
fn setup_base_env_instance(
    name: &str,
    config_toml: &str,
) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    write_manifest_with_base_env(manifest.path(), name, config_toml);
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry
        .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    (state, manifest, registry)
}

#[test]
fn shadowed_env_keys_names_only_the_overwritten_base_vars() {
    // The pure AI-27 diff: an apply only INSERTS, so every base var survives
    // into the post-apply env — a base var counts as shadowed exactly when
    // its VALUE changed. A new-name target and an identical re-write yield
    // NOTHING (the quiet path must stay quiet).
    let base: std::collections::BTreeMap<String, String> = [
        ("SHADOWED".to_string(), "old".to_string()),
        ("KEPT".to_string(), "untouched".to_string()),
    ]
    .into_iter()
    .collect();
    let mut after = base.clone();
    after.insert("SHADOWED".to_string(), "new".to_string());
    after.insert("NEWVAR".to_string(), "fresh".to_string());

    assert_eq!(
        shadowed_env_keys(&base, &after),
        vec!["SHADOWED".to_string()],
        "exactly the value-changed base var is named, deterministically sorted"
    );
    // A mapping that re-writes the identical value is not a shadow.
    let mut same_value = base.clone();
    same_value.insert("SHADOWED".to_string(), "old".to_string());
    same_value.insert("NEWVAR".to_string(), "fresh".to_string());
    assert!(shadowed_env_keys(&base, &same_value).is_empty());
    // An empty base (no [lifecycle.start] env) can never shadow.
    let empty = std::collections::BTreeMap::new();
    assert!(shadowed_env_keys(&empty, &after).is_empty());
}

#[test]
fn start_emits_one_diagnostic_when_a_mapped_env_target_shadows_a_base_var() {
    // AI-27 end-to-end: the manifest launches with BASEVAR=base-value; the
    // config maps `model` → env BASEVAR. The start's launch carries the
    // CONFIG value (precedence untouched — the insert won), and exactly ONE
    // diagnostic names the shadowed variable. The spawn itself fails (the
    // exec is deliberately missing) — AFTER the emission, proving the
    // diagnostic rides the start path regardless of launch outcome.
    let (_state, _manifest, registry) =
        setup_base_env_instance("shdw", "[config.model]\nenv = \"BASEVAR\"\n");
    let name = InstanceName::new("shdw").unwrap();
    registry.set_config(&name, "model", "config-value").unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    let buffer = install_capture_sink(&mut sup);
    let err = sup.start(&registry, "shdw").unwrap_err();
    assert!(
        matches!(err, EngineError::LaunchFailed { .. }),
        "the missing-binary spawn fails, but only AFTER the emissions; got {err:?}"
    );

    let captured = sink_text(&buffer);
    let lines: Vec<&str> = captured.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly ONE diagnostic must be emitted; got {lines:?}"
    );
    assert!(
        lines[0].contains("shdw") && lines[0].contains("BASEVAR"),
        "the diagnostic names the instance + the shadowed var: {}",
        lines[0]
    );
    // Review-1 patch 6: the wording must NOT claim the shadowed vars are
    // "base-launch"/template vars (the snapshot may carry other launch env).
    assert!(
        lines[0].contains("launch environment variable(s)") && !lines[0].contains("base-launch"),
        "the diagnostic must say 'launch environment variable(s)' without the \
             base-launch claim: {}",
        lines[0]
    );
}

#[test]
fn start_stays_quiet_when_mapped_env_targets_are_all_new_names() {
    // AI-27's clean path: the mapping targets a var the base launch does
    // NOT carry — no diagnostic at all (an empty capture). Same failing-exec
    // manifest so the only difference IS the shadow.
    let (_state, _manifest, registry) =
        setup_base_env_instance("quiet", "[config.model]\nenv = \"NEWVAR\"\n");
    let name = InstanceName::new("quiet").unwrap();
    registry.set_config(&name, "model", "config-value").unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    let buffer = install_capture_sink(&mut sup);
    let _ = sup.start(&registry, "quiet");

    assert!(
        sink_text(&buffer).is_empty(),
        "a non-shadowing start must emit NOTHING; got {}",
        sink_text(&buffer)
    );
}

/// Restore-on-drop guard for a process-global env var a test set (review-1
/// patch 7): the match-prev/restore tail the sibling tests use is skipped
/// when an assertion panics, leaking the sentinel into sibling tests on the
/// shared process env — the guard restores even on failure.
struct EnvGuard(&'static str, Option<std::ffi::OsString>);

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self(key, prev)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.1.take() {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

#[test]
fn start_emits_one_diagnostic_when_a_secret_resolves_into_a_flag_target() {
    // AI-39 runtime end-to-end: a `model = secret:KEY` leaf mapped to a FLAG
    // target resolves and delivers cleartext into argv (the accepted
    // boundary), and the start emits exactly ONE warn-only diagnostic naming
    // the key. The SET-TIME half is pinned on the same shape: the registry
    // set succeeded AND returned the steering warning (warn-only, exit 0).
    const SENTINEL: &str = "flag-steering-sentinel";
    let env_key = "KTESIO_SUP_FLAG_STEERING_KEY";
    let _env = EnvGuard::set(env_key, SENTINEL);

    let (_state, _manifest, registry) =
        setup_base_env_instance("flgrt", "[config.model]\nflag = \"--model\"\n");
    let name = InstanceName::new("flgrt").unwrap();
    let warnings = registry
        .set_config(&name, "model", &format!("secret:{env_key}"))
        .unwrap();
    assert_eq!(warnings.len(), 1, "the set-time warning fires (AI-33)");
    assert!(warnings[0].contains("model"), "{}", warnings[0]);

    let mut sup = Supervisor::with_backoff(fast_backoff());
    let buffer = install_capture_sink(&mut sup);
    let _ = sup.start(&registry, "flgrt");

    let captured = sink_text(&buffer);
    let lines: Vec<&str> = captured.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly ONE runtime diagnostic must be emitted; got {lines:?}"
    );
    assert!(
        lines[0].contains("flgrt") && lines[0].contains("model"),
        "the diagnostic names the instance + the key: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("FLAG"),
        "the diagnostic names the flag-target argv fact: {}",
        lines[0]
    );
    // NEVER the resolved value — the diagnostic names the fact, not the key.
    assert!(
        !lines[0].contains(SENTINEL),
        "the diagnostic must not leak the cleartext: {}",
        lines[0]
    );
    // The EnvGuard restores the env var even on a failed assertion.
}

// ---- The launch-snapshot fix (hosted-runner arg-loss): start uses the
//      REGISTRATION snapshot, never a start-time manifest re-read ----

#[test]
fn start_uses_the_registration_launch_snapshot_not_a_manifest_reread() {
    // The FIX, at the EXACT seam the supervisor uses (`adapter_launch_facts`),
    // OS-agnostically (no spawn — runs on every platform, including the
    // macOS/Windows CI that dropped the args): register a fake_agent manifest
    // with distinctive args, DELETE the manifest file, then read the launch
    // facts. The persisted exec + args + env still come back intact — and the
    // fallback re-read now FAILS (the manifest is gone), proving the snapshot,
    // not a re-read, is what carries the launch into `start`.
    let (_state, manifest, registry) =
        setup_fake("snap", &["--emit-usage", "5", "--linger-ms", "600000"]);
    let name = InstanceName::new("snap").unwrap();

    // Remove the manifest entirely — any start-time re-read of it now fails.
    std::fs::remove_file(manifest.path().join("adapter.toml")).unwrap();

    let (kind, manifest_path, launch) = registry.adapter_launch_facts(&name).unwrap();
    assert_eq!(kind, "snap");
    assert!(
        manifest_path.is_some(),
        "a manifest adapter records its path"
    );
    let launch = launch.expect("the launch is snapshotted at registration");
    let bin = hekma_conformance::fake_agent_bin();
    assert_eq!(launch.exec, bin.to_string_lossy().into_owned());
    // The manifest's [lifecycle.start] args survived — INCLUDING the args the
    // hosted runners dropped on re-read.
    assert_eq!(
        launch.args,
        vec!["--emit-usage", "5", "--linger-ms", "600000"]
    );

    // The fallback re-read WOULD fail now (the manifest is gone): proof that
    // the snapshot — not a re-read — is what makes the start work.
    assert!(
        adapter::resolve_start_launch(&kind, manifest_path.as_deref()).is_err(),
        "the manifest re-read is gone/broken; the snapshot carried the launch"
    );
}

#[test]
fn manifest_start_uses_the_snapshot_launch_even_when_the_manifest_changes_live() {
    // The FIX end-to-end: after registration the launch is FIXED by the
    // snapshot, so mutating the manifest's [lifecycle.start] args no longer
    // affects the started process. Register a fake_agent, REWRITE its manifest
    // with a decoy arg only a re-read would surface, then START — the spawned
    // argv carries the ORIGINAL args and NOT the decoy. Runs on ALL three
    // OSes (AI-35/38, story 11-5): the spawn+observe is a readiness
    // HANDSHAKE — the manifest passes `--marker`, the test waits for the
    // agent's marker file, then for its argv `--dump` — so no OS-fragile
    // wall-clock assumption about spawn latency remains (see
    // `wait_for_marker`).
    let dump = tempfile::tempdir().unwrap();
    let dump_path = dump.path().join("argv.txt");
    let marker_path = dump.path().join("ready.marker");
    let marker = marker_path.to_str().unwrap();
    let (_state, manifest, registry) = setup_fake(
        "del",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            dump_path.to_str().unwrap(),
            "--marker",
            marker,
        ],
    );

    // Rewrite the manifest AFTER registration, appending a decoy arg. The
    // manifest stays valid (no [config]), so the unchanged config-mapping
    // re-read still succeeds; only a LAUNCH re-read would surface the decoy.
    write_fake_manifest(
        manifest.path(),
        "del",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            dump_path.to_str().unwrap(),
            "--marker",
            marker,
            "--decoy-from-reread",
        ],
    );

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "del").unwrap();
    assert_eq!(state_of(&registry, "del"), LifecycleState::Running);

    // The spawned fake_agent dumped its argv: the ORIGINAL start args are there,
    // and the post-registration decoy is NOT — the launch came from the
    // registration snapshot, not a re-read of the mutated manifest.
    wait_for_marker(&marker_path);
    let dumped = wait_for_dump(&dump_path);
    assert!(
        dumped.lines().any(|l| l == "arg=--linger-ms"),
        "the snapshotted start args must reach argv; dump=\n{dumped}"
    );
    assert!(
        !dumped.lines().any(|l| l == "arg=--decoy-from-reread"),
        "the re-read decoy must NOT appear — the launch came from the snapshot; dump=\n{dumped}"
    );
}

#[test]
fn manifest_start_maps_model_to_the_declared_flag_target_live() {
    // Cross-OS (AI-35/38, story 11-5): the delivery logic proven here —
    // unified config → native env/flag/file mapping — is OS-agnostic engine
    // code, identical on every OS, and the `_live` spawn+observe is now a
    // readiness HANDSHAKE (the manifest passes `--marker`; the test waits
    // for the marker file, then the argv `--dump` — see `wait_for_marker`),
    // so the old "fragile spawn on macOS/Windows CI" Linux-only gate is
    // dropped and the proof runs on all three legs.
    // AC-A + AC8 (the MANIFEST proof, live). A `fake_agent` manifest declares
    // `[config.model]` → flag `--model`; set model, start the REAL process
    // with `--dump`, and assert the mapped flag landed in the spawned
    // process's argv (observed via the dump file — no stdout race).
    let dump = tempfile::tempdir().unwrap();
    let dump_path = dump.path().join("argv.txt");
    let marker_path = dump.path().join("ready.marker");
    let (_state, _manifest, registry) = setup_fake_with_config(
        "flg",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            dump_path.to_str().unwrap(),
            "--marker",
            marker_path.to_str().unwrap(),
        ],
        "[config.model]\nflag = \"--model\"\n",
    );
    let name = InstanceName::new("flg").unwrap();
    registry.set_config(&name, "model", "gpt-4o").unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "flg").unwrap();
    assert_eq!(state_of(&registry, "flg"), LifecycleState::Running);

    // The spawned fake_agent dumped its argv; the mapped flag + value are there.
    wait_for_marker(&marker_path);
    let dumped = wait_for_dump(&dump_path);
    assert!(
        dumped.lines().any(|l| l == "arg=--model"),
        "the mapped --model flag must reach the process argv; dump=\n{dumped}"
    );
    assert!(
        dumped.lines().any(|l| l == "arg=gpt-4o"),
        "the mapped model VALUE must reach the process argv; dump=\n{dumped}"
    );
    // Teardown.
    let _ = sup.stop(&registry, "flg", Some(Duration::from_millis(200)));
}

#[test]
fn secret_leaf_delivers_cleartext_to_the_adapter_but_masks_snapshot_and_events() {
    // Cross-OS (AI-35/38, story 11-5): see
    // manifest_start_maps_model_to_the_declared_flag_target_live — the
    // `_live` observe is a readiness handshake (`--marker` → `--dump`), so
    // the Linux-only gate is dropped.
    // Story 2-4 (AC-A/AC9 delivery + AC-B no-leak, engine level). A
    // `model = secret:NAME` leaf resolves (env resolver) to a sentinel; the
    // spawned agent's argv carries the CLEARTEXT (usable), while the persisted
    // snapshot AND every transition event carry the MASK, never the sentinel.
    // Uses a UNIQUE env-var name to avoid racing sibling in-process tests.
    const SENTINEL: &str = "s3cr3t-engine-sentinel-abc";
    let env_key = "KTESIO_SUP_SECRET_TEST_KEY";
    let prev = std::env::var_os(env_key);
    std::env::set_var(env_key, SENTINEL);

    let dump = tempfile::tempdir().unwrap();
    let dump_path = dump.path().join("argv.txt");
    let marker_path = dump.path().join("ready.marker");
    let (_state, _manifest, registry) = setup_fake_with_config(
        "sekeng",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            dump_path.to_str().unwrap(),
            "--marker",
            marker_path.to_str().unwrap(),
        ],
        "[config.model]\nflag = \"--model\"\n",
    );
    let name = InstanceName::new("sekeng").unwrap();
    registry
        .set_config(&name, "model", &format!("secret:{env_key}"))
        .unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "sekeng").unwrap();
    assert_eq!(state_of(&registry, "sekeng"), LifecycleState::Running);

    // (POSITIVE) the spawned process argv carries the resolved CLEARTEXT.
    wait_for_marker(&marker_path);
    let dumped = wait_for_dump(&dump_path);
    assert!(
        dumped.lines().any(|l| l == format!("arg={SENTINEL}")),
        "the resolved secret cleartext must reach the process argv; dump=\n{dumped}"
    );

    // (NO-LEAK) the persisted snapshot masks the secret.
    let snapshot = std::fs::read_to_string(registry.paths().effective_config_snapshot(&name))
        .expect("snapshot written");
    assert!(
        !snapshot.contains(SENTINEL),
        "the snapshot leaked the secret:\n{snapshot}"
    );
    assert!(
        snapshot.contains("secret:****"),
        "snapshot must mask; {snapshot}"
    );

    // (NO-LEAK) no transition event payload carries the sentinel (AD-14).
    let events = Supervisor::read_events(&registry, "sekeng").unwrap();
    let events_json = serde_json::to_string(&events).unwrap();
    assert!(
        !events_json.contains(SENTINEL),
        "a transition event leaked the secret:\n{events_json}"
    );

    // Teardown + restore env.
    let _ = sup.stop(&registry, "sekeng", Some(Duration::from_millis(200)));
    match prev {
        Some(v) => std::env::set_var(env_key, v),
        None => std::env::remove_var(env_key),
    }
}

#[test]
fn unresolved_secret_rejects_start_before_any_state_change() {
    // Story 2-4 (AC5/AC9, engine level): a `secret:NAME` unresolved by env AND
    // the (absent) secrets file rejects the start with a typed EngineError::Secret
    // that NEVER echoes a value, leaving the instance in its PRIOR state and NO
    // snapshot written. The env var is deliberately unset.
    let env_key = "KTESIO_SUP_DEFINITELY_UNSET_SECRET_KEY_XYZ";
    std::env::remove_var(env_key);
    let (_state, _manifest, registry) = setup_fake("noresolve_eng", &["--linger-ms", "600000"]);
    let name = InstanceName::new("noresolve_eng").unwrap();
    registry
        .set_config(&name, "model", &format!("secret:{env_key}"))
        .unwrap();
    let prior = state_of(&registry, "noresolve_eng");

    let mut sup = Supervisor::with_backoff(fast_backoff());
    let err = sup.start(&registry, "noresolve_eng").unwrap_err();
    match &err {
        EngineError::Secret { name: n, detail } => {
            assert_eq!(n, "noresolve_eng");
            // Names the NAME + resolvers, NEVER a value.
            assert!(detail.contains(env_key), "detail must name NAME; {detail}");
        }
        other => panic!("expected EngineError::Secret, got {other:?}"),
    }
    // Prior state preserved; no snapshot written (rejected before both).
    assert_eq!(state_of(&registry, "noresolve_eng"), prior);
    assert!(
        !registry.paths().effective_config_snapshot(&name).exists(),
        "an unresolved secret must reject before the snapshot write"
    );
}

#[test]
fn manifest_start_maps_model_to_the_declared_file_target_live() {
    // AC-A + AC4 (the MANIFEST FILE proof, live). A `fake_agent` manifest
    // declares `[config.model]` → a file target; set model, start, and assert
    // the engine RENDERED the native config file into the Agent Home at the
    // declared native key (the engine is the sole writer — path authority).
    let (_state, _manifest, registry) = setup_fake_with_config(
        "fil",
        &["--linger-ms", "600000"],
        "[config.model]\nfile = { path = \"config/agent.toml\", key = \"llm.model\" }\n",
    );
    let name = InstanceName::new("fil").unwrap();
    registry.set_config(&name, "model", "claude-opus").unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "fil").unwrap();
    assert_eq!(state_of(&registry, "fil"), LifecycleState::Running);

    // The engine rendered the native config file into the Agent Home.
    let rendered = registry.agent_home(&name).join("config/agent.toml");
    assert!(
        rendered.is_file(),
        "the file target must render into the home"
    );
    let parsed: toml::Table = std::fs::read_to_string(&rendered).unwrap().parse().unwrap();
    assert_eq!(
        parsed["llm"]["model"].as_str(),
        Some("claude-opus"),
        "the documented model key must land at the declared native key path"
    );
    // Teardown.
    let _ = sup.stop(&registry, "fil", Some(Duration::from_millis(200)));
}

#[test]
fn manifest_start_delivers_agent_pass_through_verbatim_live() {
    // Cross-OS (AI-35/38, story 11-5): see
    // manifest_start_maps_model_to_the_declared_flag_target_live — the
    // `_live` observe is a readiness handshake (`--marker` → `--dump`), so
    // the Linux-only gate is dropped.
    // AC-B (the `agent.*` verbatim proof, live). Set an `agent.*` pass-through
    // key, start the REAL fake_agent with `--dump`, and assert the value was
    // delivered VERBATIM into the native mechanism (an env var named by the
    // verbatim key-tail) — no rewriting, no known-key mapping.
    let dump = tempfile::tempdir().unwrap();
    let dump_path = dump.path().join("env.txt");
    let marker_path = dump.path().join("ready.marker");
    // No [config] mapping at all — pass-through does not need one (AC6).
    let (_state, _manifest, registry) = setup_fake_with_config(
        "pth",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            dump_path.to_str().unwrap(),
            "--marker",
            marker_path.to_str().unwrap(),
        ],
        "",
    );
    let name = InstanceName::new("pth").unwrap();
    registry
        .set_config(&name, "agent.CUSTOM_TOKEN", "verbatim-xyz")
        .unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "pth").unwrap();
    assert_eq!(state_of(&registry, "pth"), LifecycleState::Running);

    wait_for_marker(&marker_path);
    let dumped = wait_for_dump(&dump_path);
    assert!(
        dumped.lines().any(|l| l == "env=CUSTOM_TOKEN=verbatim-xyz"),
        "the agent.* value must be delivered verbatim into the native env; dump=\n{dumped}"
    );
    // Teardown.
    let _ = sup.stop(&registry, "pth", Some(Duration::from_millis(200)));
}

// ---- Story 2-3: the persisted effective-config snapshot at start (AC5/AC6/AC7) ----

/// Parse the persisted effective-config snapshot for `name` and return the
/// entry map (key → (rendered value, source label)). Panics if the file is
/// missing/unparseable (the test wants it present).
fn read_snapshot_entries(
    registry: &Registry,
    name: &str,
) -> std::collections::BTreeMap<String, (String, String)> {
    let path = registry
        .paths()
        .effective_config_snapshot(&InstanceName::new(name).unwrap());
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["key"].as_str().unwrap().to_string(),
                (
                    e["value"].as_str().unwrap().to_string(),
                    e["source"].as_str().unwrap().to_string(),
                ),
            )
        })
        .collect()
}

#[test]
fn start_writes_the_effective_config_snapshot_tagged_with_source() {
    // AC5 (AD-9/AD-6): starting an instance writes the effective-config
    // snapshot FILE into the Agent Home at EnginePaths::effective_config_snapshot,
    // and it parses + carries model=<v> tagged `instance`. Register a live
    // fake_agent, set model, start it, assert the snapshot.
    let (_state, _manifest, registry) = setup_fake("snp", &["--linger-ms", "600000"]);
    let name = InstanceName::new("snp").unwrap();
    registry.set_config(&name, "model", "gpt-4o").unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "snp").unwrap();
    assert_eq!(state_of(&registry, "snp"), LifecycleState::Running);

    let path = registry.paths().effective_config_snapshot(&name);
    assert!(path.is_file(), "the snapshot must exist at {path:?}");
    let entries = read_snapshot_entries(&registry, "snp");
    assert_eq!(
        entries.get("model"),
        Some(&("gpt-4o".to_string(), "instance".to_string())),
        "model must be present tagged `instance`; entries={entries:?}"
    );
    // Teardown.
    let _ = sup.stop(&registry, "snp", Some(Duration::from_millis(200)));
}

#[test]
fn restart_via_start_inner_overwrites_the_snapshot_with_the_new_value() {
    // AC7: the snapshot is OVERWRITTEN on every start — a re-start (which flows
    // through the SAME start_inner seam, story 1-6) refreshes it with the newly
    // resolved value, never a stale earlier resolution. Start, stop, change the
    // value, start again; the snapshot reflects the LATEST value.
    let (_state, _manifest, registry) = setup_fake("rsn", &["--linger-ms", "600000"]);
    let name = InstanceName::new("rsn").unwrap();
    registry.set_config(&name, "model", "first").unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "rsn").unwrap();
    assert_eq!(
        read_snapshot_entries(&registry, "rsn").get("model"),
        Some(&("first".to_string(), "instance".to_string()))
    );
    sup.stop(&registry, "rsn", Some(Duration::from_millis(200)))
        .unwrap();

    // Change the value and start again (stopped → starting → running via
    // start_inner). The snapshot must be overwritten with the new value.
    registry.set_config(&name, "model", "second").unwrap();
    sup.start(&registry, "rsn").unwrap();
    assert_eq!(state_of(&registry, "rsn"), LifecycleState::Running);
    assert_eq!(
        read_snapshot_entries(&registry, "rsn").get("model"),
        Some(&("second".to_string(), "instance".to_string())),
        "the snapshot must reflect the latest resolved value after re-start (AC7)"
    );
    // Teardown.
    let _ = sup.stop(&registry, "rsn", Some(Duration::from_millis(200)));
}

#[test]
fn snapshot_write_failure_rejects_the_start_before_the_starting_transition() {
    // AC6: a snapshot-write failure rejects the start with NO state change (the
    // write lands before the `starting` transition). Force the write to fail by
    // making the snapshot path a DIRECTORY, then assert start errors and the
    // instance stays in its prior state (`registered`), with NO agent spawned.
    let (_state, _manifest, registry) = setup_fake("bad", &["--linger-ms", "600000"]);
    let name = InstanceName::new("bad").unwrap();
    registry.set_config(&name, "model", "gpt-4").unwrap();
    // A directory where the snapshot file must be → std::fs::write fails.
    let snap_path = registry.paths().effective_config_snapshot(&name);
    std::fs::create_dir(&snap_path).unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    let err = sup.start(&registry, "bad").unwrap_err();
    assert!(
        matches!(&err, EngineError::Snapshot { name, .. } if name == "bad"),
        "expected a typed Snapshot error, got {err:?}"
    );
    // The instance stayed in its prior state — the start was rejected cleanly
    // BEFORE the `starting` transition (no spurious state change, AC6).
    assert_eq!(state_of(&registry, "bad"), LifecycleState::Registered);
}

#[test]
fn snapshot_to_engine_maps_snapshot_write_and_falls_back_for_others() {
    // Unit-cover the snapshot error mapper: a SnapshotWrite maps to the
    // dedicated EngineError::Snapshot naming the instance + path; any other
    // registry error falls back to the shared registry mapper (NotFound here).
    let mapped = snapshot_to_engine(crate::domain::RegistryError::SnapshotWrite {
        name: "demo".into(),
        path: "/x/agents/demo/effective-config.json".into(),
        detail: "disk full".into(),
    });
    match mapped {
        EngineError::Snapshot { name, path, detail } => {
            assert_eq!(name, "demo");
            assert!(path.ends_with("effective-config.json"), "path={path}");
            assert_eq!(detail, "disk full");
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
    // Fallback: a non-snapshot registry error goes through registry_to_engine.
    let fallback = snapshot_to_engine(crate::domain::RegistryError::NotFound {
        name: "demo".into(),
    });
    assert!(matches!(fallback, EngineError::NotFound { name } if name == "demo"));
}

#[test]
fn crash_of_a_never_policy_instance_lands_failed_no_restart() {
    // AC-A / AC5: a `never`-policy instance that crashes lands `failed` with a
    // `crashed` cause + NO restart. Set policy=never, start a crash-after
    // agent, poll until the crash is detected, assert failed + no plan.
    let (_state, _manifest, registry) = setup_fake("nevr", &["--crash-after-ms", "450"]);
    registry
        .set_restart_policy(&InstanceName::new("nevr").unwrap(), RestartPolicy::Never)
        .unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "nevr").unwrap();
    assert_eq!(state_of(&registry, "nevr"), LifecycleState::Running);

    let plans = wait_for_crash(&mut sup, &registry);
    assert!(plans.is_empty(), "never policy must NOT schedule a restart");
    assert_eq!(state_of(&registry, "nevr"), LifecycleState::Failed);

    // The crash was recorded with a `crashed` cause.
    let events = Supervisor::read_events(&registry, "nevr").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Failed);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(cause.contains("crashed"), "cause={cause}");
}

#[test]
fn on_failure_crash_restarts_increments_count_then_a_clean_run_resets() {
    // AC-A / AC4: an `on-failure` instance that crashes is restarted, the
    // restart count increments, the `failed→starting`… restart event records
    // the backoff, and a subsequent CLEAN start resets the count to 0. Uses
    // the injected fast backoff so no real seconds elapse.
    let (_state, _manifest, registry) = setup_fake("recov", &["--crash-after-ms", "450"]);
    // Default policy is on-failure; be explicit.
    registry
        .set_restart_policy(
            &InstanceName::new("recov").unwrap(),
            RestartPolicy::OnFailure,
        )
        .unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "recov").unwrap();

    // Detect the crash → a restart plan for attempt 1.
    let plans = wait_for_crash(&mut sup, &registry);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].attempt, 1);
    assert_eq!(state_of(&registry, "recov"), LifecycleState::Failed);

    // Perform the restart after its (fast) backoff. The instance is running
    // again and the record shows restart_count == 1.
    std::thread::sleep(plans[0].delay);
    sup.restart(&registry, "recov", plans[0].attempt, plans[0].delay)
        .unwrap();
    assert_eq!(state_of(&registry, "recov"), LifecycleState::Running);
    let rec = registry
        .spawn_record(&InstanceName::new("recov").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rec.restart_count, 1);

    // The restart event recorded the count + waited backoff.
    let events = Supervisor::read_events(&registry, "recov").unwrap();
    let restart_evt = events
        .iter()
        .find(|e| matches!(e.cause, TransitionCause::Restarted { .. }))
        .expect("a restart event must be recorded");
    match &restart_evt.cause {
        TransitionCause::Restarted { count, .. } => assert_eq!(*count, 1),
        _ => unreachable!(),
    }

    // A CLEAN stop then start resets the consecutive count to 0 (AC4).
    sup.stop(&registry, "recov", Some(Duration::from_millis(200)))
        .unwrap();
    sup.start(&registry, "recov").unwrap();
    let rec = registry
        .spawn_record(&InstanceName::new("recov").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rec.restart_count, 0, "a fresh start resets the count");
    // Teardown.
    let _ = sup.stop(&registry, "recov", Some(Duration::from_millis(200)));
}

#[test]
fn crash_loop_stops_after_exactly_five_consecutive_failures() {
    // AC4: an instance that crashes immediately every restart stops after
    // EXACTLY 5 consecutive failures, left `failed` with the crash-loop
    // reason. Drive the crash→restart cycle manually with the fast backoff;
    // the 5th restart's crash yields NO further plan (crash-loop), and the
    // recorded cause states the crash loop.
    let (_state, _manifest, registry) = setup_fake("loopy", &["--crash-after-ms", "400"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "loopy").unwrap();

    let mut last_attempt = 0;
    // Up to 5 restarts, each preceded by a detected crash.
    for _ in 0..6 {
        let plans = wait_for_crash(&mut sup, &registry);
        assert_eq!(state_of(&registry, "loopy"), LifecycleState::Failed);
        if plans.is_empty() {
            // Crash-loop reached: no more restarts scheduled.
            break;
        }
        assert_eq!(plans.len(), 1);
        last_attempt = plans[0].attempt;
        std::thread::sleep(plans[0].delay);
        // The restart re-launches; it will crash again on the next poll.
        sup.restart(&registry, "loopy", plans[0].attempt, plans[0].delay)
            .unwrap();
    }

    // The last scheduled attempt was the 4th → the 5th crash trips the loop
    // (is_crash_loop(5) == true), so no 5th restart plan is issued.
    assert_eq!(
        last_attempt, 4,
        "the last restart plan should be attempt 4 (the 5th crash trips the loop)"
    );
    assert_eq!(state_of(&registry, "loopy"), LifecycleState::Failed);

    // F-Low-2: the crash-loop is a TERMINAL path, so the record's LIVE
    // fingerprint is dropped (settled to a pid-0 seed) — a later open will
    // NOT adopt-attempt the dead PID (the reconcile skips pid-0). The policy
    // is re-seeded so `show` can still report it (AC9).
    let rec = registry
        .spawn_record(&InstanceName::new("loopy").unwrap())
        .unwrap()
        .expect("a policy seed is retained after the terminal crash-loop");
    assert_eq!(
        rec.fingerprint.pid, 0,
        "the terminal path must drop the live fingerprint (pid-0 seed, not adopt-attempted)"
    );
    assert_eq!(
        rec.restart_policy,
        RestartPolicy::OnFailure,
        "the policy is retained for AC9's show"
    );
    // The crash-loop REASON now rides in the last `crashed` event's cause (so
    // `instance_status` surfaces it via its event-log fallback).
    let events = Supervisor::read_events(&registry, "loopy").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Failed);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("crash-loop") && cause.contains("5 consecutive failures"),
        "the crash-loop reason must be in the event cause; cause={cause}"
    );
}

#[test]
fn poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash() {
    // The reaper's "not a crash" branch: if the store shows the instance
    // `stopping` (an operator stop in flight) when its process exits,
    // poll_once must NOT apply a `failed` crash transition — it just drops the
    // dead handle. Start an instance, mark it `stopping` in the store, let it
    // crash, and assert poll_once returns no plans and does NOT move it to
    // `failed` (it stays `stopping`, the requested end state).
    let (_state, _manifest, registry) = setup_fake("stopping", &["--crash-after-ms", "450"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "stopping").unwrap();
    // Simulate an operator stop in flight (row → stopping) while the handle
    // is still held.
    registry
        .set_state(
            &InstanceName::new("stopping").unwrap(),
            LifecycleState::Stopping,
        )
        .unwrap();
    // Wait for the process to actually exit, then poll.
    std::thread::sleep(Duration::from_millis(700));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let plans = sup.poll_once(&registry);
        assert!(
            plans.is_empty(),
            "an exit during a requested stop is not a crash"
        );
        if sup.running.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "handle should be dropped after exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The state is NOT `failed` (no crash transition was applied); it stays
    // `stopping` (the requested end state the operator stop will finalize).
    assert_eq!(state_of(&registry, "stopping"), LifecycleState::Stopping);
}

// ---- Fix pass (review of #80 follow-up — the CRITICAL finding): the
// bound-post-SIGKILL-wait fix's retry/no-compounding/self-healing logic.
//
// These are WHITE-BOX tests: rather than needing a genuinely OS-unkillable
// process (which requires disk-exhaustion-induced uninterruptible I/O
// wait — reproduced separately via a dedicated, SAFE ramdisk experiment;
// see the story file's Dev Agent Record for the full empirical proof),
// they directly construct the EXACT bookkeeping state a real
// `BackendError::StopUnconfirmed` would have left behind
// (`Supervised::stop_unconfirmed = true`, store state `stopping`, handle
// retained) and prove `stop_inner`'s/`poll_once`'s reconciliation logic
// against it. This is deterministic and fast (no real 5s wait), mirroring
// `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`
// immediately above's own technique of forcing `stopping` via
// `registry.set_state` directly.

#[test]
fn stop_on_a_stopping_instance_without_the_unconfirmed_flag_takes_the_ordinary_path() {
    // The negative-space complement of the retry tests below: an
    // instance that is `stopping` with a held handle but is NOT marked
    // `stop_unconfirmed` (the flag ONLY a real `BackendError::
    // StopUnconfirmed` sets) must NOT take the new cheap-poll retry
    // branch — it falls through to the ORIGINAL, unchanged
    // `next_state` gate, which rejects with the uniform
    // `InvalidTransition` exactly as it did before this fix pass. This
    // proves the new branch is gated precisely on `stop_unconfirmed`,
    // not merely "state is stopping" (which
    // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`
    // above ALSO forces, for a different, pre-existing reason).
    let (_state, _manifest, registry) = setup_fake("notflagged", &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "notflagged").unwrap();
    let name = InstanceName::new("notflagged").unwrap();

    registry.set_state(&name, LifecycleState::Stopping).unwrap();
    assert!(
        !sup.running.get(&name).unwrap().stop_unconfirmed,
        "a freshly-started handle must default to NOT stop_unconfirmed"
    );

    let err = sup.stop(&registry, "notflagged", None).unwrap_err();
    assert!(
        matches!(err, EngineError::InvalidTransition(_)),
        "without the flag, this must be the ORIGINAL uniform InvalidTransition, not the new \
             StopUnconfirmed retry path: {err:?}"
    );

    // Teardown.
    let supervised = sup.running.get_mut(&name).unwrap();
    let _ = sup
        .backend
        .stop(&mut supervised.handle, Duration::from_secs(2));
}

#[test]
fn stop_retry_on_a_stuck_unconfirmed_instance_polls_cheaply_no_compounding() {
    // A retry `stop()` against an instance whose handle is marked
    // `stop_unconfirmed` (a prior real stop attempt hit
    // KILL_CONFIRM_TIMEOUT) must NOT re-run the whole
    // SIGTERM/graceful-window/SIGKILL/confirm sequence — it polls ONCE,
    // cheaply, and fails fast with the SAME honest error while the
    // process is still genuinely alive (no new signal, no new wait).
    let (_state, _manifest, registry) = setup_fake("stuck", &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "stuck").unwrap();
    let name = InstanceName::new("stuck").unwrap();

    // Simulate the aftermath of a real StopUnconfirmed (stop_inner's
    // own bookkeeping on that path — see its docs): the row is
    // `stopping`, the handle is retained, and it is marked unconfirmed.
    registry.set_state(&name, LifecycleState::Stopping).unwrap();
    sup.running.get_mut(&name).unwrap().stop_unconfirmed = true;

    let start = Instant::now();
    let err = sup.stop(&registry, "stuck", None).unwrap_err();
    let elapsed = start.elapsed();
    assert!(
        matches!(&err, EngineError::StopUnconfirmed { name, .. } if name == "stuck"),
        "expected StopUnconfirmed, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "a retry against a still-alive stuck instance must poll cheaply (ProcessBackend::poll, \
             never ProcessBackend::stop), not re-block for a whole new \
             graceful-window/SIGKILL/confirm cycle: {elapsed:?}"
    );
    // No compounding: the row is untouched (still stopping), the handle
    // is still retained (not dropped) for a further retry to reconcile.
    assert_eq!(state_of(&registry, "stuck"), LifecycleState::Stopping);
    assert!(
        sup.running.contains_key(&name),
        "the handle must be retained across a failed retry, never silently dropped"
    );

    // Teardown: really kill the still-running process so it does not
    // leak past this test.
    let supervised = sup.running.get_mut(&name).unwrap();
    let _ = sup
        .backend
        .stop(&mut supervised.handle, Duration::from_secs(2));
}

#[test]
fn stop_retry_self_heals_once_the_stuck_process_actually_exits() {
    // The self-healing counterpart: once the process behind a
    // stop_unconfirmed handle has ACTUALLY died (the OS condition that
    // made confirmation time out has cleared), a retry `stop()` must
    // reconcile the instance to `stopped` — never leaving it permanently
    // stuck just because one earlier attempt could not confirm death in
    // time.
    let (_state, _manifest, registry) = setup_fake("heals", &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "heals").unwrap();
    let name = InstanceName::new("heals").unwrap();

    registry.set_state(&name, LifecycleState::Stopping).unwrap();
    sup.running.get_mut(&name).unwrap().stop_unconfirmed = true;

    // Simulate the OS condition clearing: the process ACTUALLY exits now.
    // A real, portable kill via the SAME ProcessBackend::stop the
    // production code already uses (cfg-free at this call site — the
    // OS-specific mechanics live entirely in `backends/`), not a raw
    // OS-specific signal call, so this stays a legitimate domain-layer
    // test. The process is NOT genuinely stuck in this test (only its
    // BOOKKEEPING pretends it was), so this succeeds quickly.
    {
        let supervised = sup.running.get_mut(&name).unwrap();
        sup.backend
            .stop(&mut supervised.handle, Duration::from_secs(2))
            .expect("the process is not genuinely stuck in this test and must die promptly");
    }

    let instance = sup
        .stop(&registry, "heals", None)
        .expect("a retry must self-heal once the process is confirmed dead");
    assert_eq!(
        instance.state,
        LifecycleState::Stopped,
        "the stuck stopping row must reconcile to stopped, not stay stuck forever"
    );
    assert!(
        !sup.running.contains_key(&name),
        "the handle must be released once reconciled"
    );
}

#[test]
fn poll_once_reconciles_a_stuck_unconfirmed_stop_to_stopped_self_healing() {
    // The crash reaper's OWN reconciliation path (`poll_once`) — the
    // OTHER self-healing route besides a manual retry `stop()` (whichever
    // observes the exit first): when a `stop_unconfirmed`-marked handle's
    // process is found `Exited` during a routine reaper poll, poll_once
    // must finalize `stopping -> stopped` itself, rather than silently
    // dropping the handle (which would leave the row PERMANENTLY stuck,
    // since a later retry `stop()` would find no handle to poll).
    // Contrast directly with
    // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`
    // above: that test's contrived `stopping` row is NOT
    // `stop_unconfirmed` (it never went through a real stop attempt), so
    // it correctly keeps the ORIGINAL silent-drop behavior — proving this
    // fix pass changes behavior ONLY for the scenario it targets.
    // --crash-after-ms must comfortably EXCEED READINESS_WINDOW (300ms) or
    // the process looks like an immediate-exit launch failure (AC2)
    // instead of a clean start that later crashes — the same pitfall
    // documented above for `--linger-ms` in this file; 500ms is that
    // established, proven-safe margin.
    let (_state, _manifest, registry) = setup_fake("reaperheals", &["--crash-after-ms", "500"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "reaperheals").unwrap();
    let name = InstanceName::new("reaperheals").unwrap();

    registry.set_state(&name, LifecycleState::Stopping).unwrap();
    sup.running.get_mut(&name).unwrap().stop_unconfirmed = true;

    // Wait for the process to actually exit on its own, then let the
    // reaper observe it.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let plans = sup.poll_once(&registry);
        assert!(
            plans.is_empty(),
            "this is a reconciliation, never a crash/restart"
        );
        if !sup.running.contains_key(&name) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "handle should be reconciled after exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        state_of(&registry, "reaperheals"),
        LifecycleState::Stopped,
        "the reaper must finalize a stuck-unconfirmed stopping row to stopped, not leave it \
             permanently stuck"
    );
}

#[test]
fn poll_once_with_no_handles_is_a_noop() {
    // The empty-reaper path: with nothing supervised, poll_once returns no
    // plans and touches nothing.
    let (_state, _manifest, registry) = setup_fake("idle", &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    assert!(sup.poll_once(&registry).is_empty());
}

#[test]
fn adopt_orphans_with_no_records_adopts_nothing() {
    // The empty-reconcile path: with no persisted spawn records, adoption
    // adopts nothing and returns 0.
    let (_state, _manifest, registry) = setup_fake("idle", &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    assert_eq!(sup.adopt_orphans(&registry), 0);
    assert!(sup.running.is_empty());
}

#[test]
fn adopt_orphans_skips_a_policy_only_seed_record() {
    // A pid-0 record is a policy-only config seed (set before any start), NOT
    // a supervised process — adoption skips it (adopts nothing) and does NOT
    // reconcile the registered instance to failed or clear its policy.
    let (_state, _manifest, registry) = setup_fake("seedonly", &["--linger-ms", "600000"]);
    registry
        .set_restart_policy(
            &InstanceName::new("seedonly").unwrap(),
            RestartPolicy::Never,
        )
        .unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    assert_eq!(sup.adopt_orphans(&registry), 0);
    // Still registered; the policy seed survives (was not cleared).
    assert_eq!(state_of(&registry, "seedonly"), LifecycleState::Registered);
    let rec = registry
        .spawn_record(&InstanceName::new("seedonly").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rec.restart_policy, RestartPolicy::Never);
}

#[test]
fn default_stop_window_is_30s() {
    assert_eq!(DEFAULT_STOP_WINDOW, Duration::from_secs(30));
}

#[test]
fn read_events_from_missing_file_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nope.log");
    assert!(read_events_from(&path).unwrap().is_empty());
}

#[test]
fn append_then_read_round_trips_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("instance.log");
    let e1 = TransitionEvent::new(
        "demo",
        LifecycleState::Registered,
        LifecycleState::Starting,
        TransitionCause::command("start"),
        "2026-07-04T00:00:00Z",
    );
    let e2 = TransitionEvent::new(
        "demo",
        LifecycleState::Starting,
        LifecycleState::Running,
        TransitionCause::AdapterReady,
        "2026-07-04T00:00:01Z",
    );
    append_event(&path, &e1).unwrap();
    append_event(&path, &e2).unwrap();
    let back = read_events_from(&path).unwrap();
    assert_eq!(back, vec![e1, e2]);
}

#[test]
fn read_events_rejects_a_corrupt_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("instance.log");
    std::fs::write(&path, "{ not valid json\n").unwrap();
    let err = read_events_from(&path).unwrap_err();
    assert!(err.contains("corrupt instance-log line 1"), "{err}");
}

#[test]
fn supervisor_constructs_empty() {
    let sup = Supervisor::new();
    assert!(sup.running.is_empty());
}

#[test]
fn registry_to_engine_maps_each_variant() {
    use super::super::error::RegistryError as R;
    use super::super::name::NameError;

    // NotFound → NotFound.
    assert!(matches!(
        registry_to_engine(R::NotFound { name: "x".into() }),
        EngineError::NotFound { .. }
    ));
    // InvalidName → InvalidName.
    assert!(matches!(
        registry_to_engine(R::InvalidName {
            name: "X".into(),
            reason: NameError::BadChar,
        }),
        EngineError::InvalidName { .. }
    ));
    // Io → Log (naming the path).
    assert!(matches!(
        registry_to_engine(R::Io {
            name: "x".into(),
            path: "/p".into(),
            source: std::io::Error::other("boom"),
        }),
        EngineError::Log { .. }
    ));
    // A snapshot-shaped registry error → AdapterUnresolved.
    assert!(matches!(
        registry_to_engine(R::ManifestNotFound { path: "/m".into() }),
        EngineError::AdapterUnresolved { .. }
    ));
}

#[test]
fn launch_to_engine_wraps_as_adapter_unresolved() {
    let name = InstanceName::new("svc").unwrap();
    let err = launch_to_engine(
        &name,
        LaunchResolveError::NativeHasNoLaunch {
            kind: "mock".into(),
        },
    );
    match err {
        EngineError::AdapterUnresolved { name, detail } => {
            assert_eq!(name, "svc");
            assert!(detail.contains("no launch command"));
        }
        other => panic!("expected AdapterUnresolved, got {other}"),
    }
}

#[test]
fn config_to_engine_and_config_apply_to_engine_wrap_as_adapter_unresolved() {
    // Story 2-2: a config-resolution failure and a config-apply (file-render)
    // failure both surface as an unresolved-adapter launch failure naming the
    // instance + preserving the detail, so `start` rejects cleanly.
    let name = InstanceName::new("svc").unwrap();
    let cfg_err = config_to_engine(
        &name,
        crate::domain::ConfigError::NotFound { name: "svc".into() },
    );
    match cfg_err {
        EngineError::AdapterUnresolved { name, detail } => {
            assert_eq!(name, "svc");
            assert!(detail.contains("svc"), "detail preserved: {detail}");
        }
        other => panic!("expected AdapterUnresolved, got {other}"),
    }
    let apply_err = config_apply_to_engine(
        &name,
        ConfigApplyError::FileRender {
            key: "config/agent.toml".into(),
            path: "config/agent.toml".into(),
            detail: "disk full".into(),
        },
    );
    match apply_err {
        EngineError::AdapterUnresolved { name, detail } => {
            assert_eq!(name, "svc");
            assert!(detail.contains("config/agent.toml"), "{detail}");
            assert!(detail.contains("disk full"), "{detail}");
        }
        other => panic!("expected AdapterUnresolved, got {other}"),
    }
}

#[test]
fn start_with_an_unwritable_file_target_rejects_before_any_state_change() {
    // Story 2-2 end-to-end error path (accurate atomicity, Fix #5): a manifest
    // `[config.model]` FILE target whose parent path is blocked (a regular file
    // sits where the config directory must be in the Agent Home) fails the
    // config-mapping application at start. Because the mapping is applied BEFORE
    // the `starting` transition, the start REJECTS (AdapterUnresolved) and the
    // instance stays in its PRIOR state (`registered`) — it does NOT land
    // `failed`, and never reaches `running`. Exercises the start_inner
    // config-apply error branch + config_apply_to_engine.
    let (_state, _manifest, registry) = setup_fake_with_config(
        "badfile",
        &["--linger-ms", "600000"],
        "[config.model]\nfile = { path = \"blocked/agent.toml\", key = \"k\" }\n",
    );
    let name = InstanceName::new("badfile").unwrap();
    registry.set_config(&name, "model", "gpt-4").unwrap();
    // Block the file target's parent: put a regular FILE at <home>/blocked so
    // create_dir_all(<home>/blocked) fails when rendering blocked/agent.toml.
    let home = registry.agent_home(&name);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("blocked"), b"not a dir").unwrap();

    let mut sup = Supervisor::with_backoff(fast_backoff());
    let err = sup.start(&registry, "badfile").unwrap_err();
    assert!(
        matches!(err, EngineError::AdapterUnresolved { .. }),
        "a bad file target must fail the start; got {err}"
    );
    // Never reached running (the failure was before the starting transition, so
    // the instance stays registered).
    assert_eq!(state_of(&registry, "badfile"), LifecycleState::Registered);
}

#[test]
fn read_events_skips_blank_lines() {
    // Blank lines in the log are ignored (only JSON event lines are parsed).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("instance.log");
    let e = TransitionEvent::new(
        "demo",
        LifecycleState::Registered,
        LifecycleState::Starting,
        TransitionCause::command("start"),
        "2026-07-04T00:00:00Z",
    );
    let line = serde_json::to_string(&e).unwrap();
    std::fs::write(&path, format!("\n{line}\n\n")).unwrap();
    let back = read_events_from(&path).unwrap();
    assert_eq!(back, vec![e]);
}

// ---- Story 3-2: the breach record write error is surfaced, not swallowed ----

#[test]
fn record_breach_surfaces_a_write_failure_instead_of_swallowing_it() {
    // FR-21 ("the breach is always recorded"): if the durable breach-log write
    // fails (disk full / IO / perms), the error must be SURFACED (an honest
    // stderr diagnostic), NOT silently discarded while the action still fires —
    // otherwise the mandated record is lost with no trace. We force BOTH the
    // dir-create AND the append to fail by placing a regular FILE where the
    // per-instance log DIRECTORY (`<home>/logs`, the breach log's parent) must
    // be: `create_dir_all(parent)` fails (a file sits at that path) and the
    // subsequent append cannot open `logs/breaches.log` either. `record_breach`
    // must NOT panic (it stays non-fatal) and must not lose data silently — this
    // proves both surfaced-error branches are reachable and the enforcement path
    // survives. Pure unit test: no process, no OS gate.
    let (_state, _manifest, registry) = setup_fake("breachio", &["--linger-ms", "600000"]);
    let name = InstanceName::new("breachio").unwrap();
    // Block the log-dir path with a regular file so create_dir_all(<home>/logs)
    // fails (its target is a file, not a directory) and the append fails too.
    let log_dir = registry.instance_log_dir(&name);
    std::fs::create_dir_all(log_dir.parent().unwrap()).unwrap();
    std::fs::write(&log_dir, b"not a directory").unwrap();
    assert!(
        log_dir.is_file(),
        "the log-dir path must be a FILE to force both the dir-create and append failures"
    );

    let sup = Supervisor::with_backoff(fast_backoff());
    // Must not panic — both the dir-create and the append failures are logged
    // (surfaced) and enforcement continues rather than crashing.
    sup.record_token_breach(
        &registry,
        &name,
        &RunId::mint(),
        BreachScope::Cumulative,
        30,
        60,
        BreachAction::Warn,
        "self-reported",
    );
    // The blocking file is untouched — no breach file was sneaked in, confirming
    // the write genuinely failed and we exercised the surfaced-error branches.
    assert!(log_dir.is_file());
}

// ---- Story 3-1 drain planning: H1 terminal-tail + M2 shrink guard ----

#[test]
fn plan_drain_midrun_stops_at_the_last_newline() {
    // MID-RUN: a live process may still finish a partial final line, so only bytes
    // up to the last newline are consumed; the trailing partial waits.
    let bytes = b"a\nb\nhalf-written";
    let plan = plan_drain(bytes, 0, DrainMode::MidRun);
    // Consumes "a\nb\n" (4 bytes), leaving "half-written" for the next pass.
    assert_eq!(
        plan,
        DrainPlan::Consume {
            range: 0..4,
            new_cursor: 4
        }
    );
}

#[test]
fn plan_drain_midrun_with_no_newline_yet_consumes_nothing() {
    // A tail with no complete line yet: nothing to consume this pass (MidRun).
    assert_eq!(
        plan_drain(b"no newline yet", 0, DrainMode::MidRun),
        DrainPlan::Nothing
    );
}

#[test]
fn plan_drain_terminal_consumes_a_newline_less_final_line() {
    // H1: on a TERMINAL drain the process is dead, so a final usage line flushed
    // WITHOUT a trailing newline must be consumed to end-of-log (or it is stranded
    // and the next Run's cursor skips past it → a permanent under-count).
    let bytes = b"a\nKTESIO_USAGE {\"sequence\":0,\"input_tokens\":10,\"output_tokens\":20}";
    let plan = plan_drain(bytes, 0, DrainMode::Terminal);
    // The WHOLE tail is consumed (no trailing newline required).
    assert_eq!(
        plan,
        DrainPlan::Consume {
            range: 0..bytes.len(),
            new_cursor: bytes.len() as u64
        }
    );
}

#[test]
fn plan_drain_terminal_from_a_cursor_consumes_only_the_new_tail() {
    // The terminal tail is measured FROM the cursor (already-read bytes are not
    // re-consumed) and still needs no trailing newline.
    let bytes = b"old\nnew-tail-no-nl";
    let plan = plan_drain(bytes, 4, DrainMode::Terminal); // cursor past "old\n"
    assert_eq!(
        plan,
        DrainPlan::Consume {
            range: 4..bytes.len(),
            new_cursor: bytes.len() as u64
        }
    );
}

#[test]
fn plan_drain_shrink_snaps_the_cursor_and_ingests_nothing() {
    // M2: the log is shorter than the cursor (a truncate/rotation). We must NOT
    // re-read from 0 (double-count → inflated bill); instead snap the cursor to
    // the new length and ingest nothing. Holds for BOTH modes.
    let bytes = b"short"; // len 5
    assert_eq!(
        plan_drain(bytes, 100, DrainMode::MidRun),
        DrainPlan::Shrunk { new_cursor: 5 }
    );
    assert_eq!(
        plan_drain(bytes, 100, DrainMode::Terminal),
        DrainPlan::Shrunk { new_cursor: 5 },
        "the shrink guard applies on the terminal path too"
    );
}

#[test]
fn plan_drain_at_end_of_log_consumes_nothing() {
    // Cursor exactly at len (all bytes already read): an empty tail → Nothing, in
    // both modes (no phantom terminal consume of zero bytes).
    let bytes = b"a\nb\n";
    assert_eq!(plan_drain(bytes, 4, DrainMode::MidRun), DrainPlan::Nothing);
    assert_eq!(
        plan_drain(bytes, 4, DrainMode::Terminal),
        DrainPlan::Nothing
    );
}

// ---- AI-63: incremental usage-tail read (drain_usage_for no longer reads the
//      whole never-rotated agent.log on every reaper tick under the global lock).
//      These prove (1) the read is bounded by NEW bytes not total size, and
//      (2)/(3)/(4) the three billing semantics — MidRun tail, Terminal newline-
//      less tail, and the M2 shrink guard — are byte-identical via the new path.

/// THE fix proof: a large already-consumed prefix is NOT re-read. The per-pass
/// read is bounded by the NEW bytes (`len - cursor`), never the total file size.
#[test]
fn read_usage_tail_reads_only_the_new_bytes_not_the_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.log");
    // A big already-drained prefix (what the OLD code re-read every 250ms) plus
    // one small fresh usage line — the only bytes a drain should now touch.
    let prefix = vec![b'x'; 5 * 1024 * 1024]; // 5 MiB already consumed
    let fresh = b"KTESIO_USAGE {\"sequence\":0,\"input_tokens\":1,\"output_tokens\":2}\n";
    let mut content = prefix.clone();
    content.extend_from_slice(fresh);
    std::fs::write(&path, &content).unwrap();
    let cursor = prefix.len() as u64;

    let UsageTail::Tail { bytes } = read_usage_tail(&path, cursor) else {
        panic!("expected a Tail read");
    };
    // Read EXACTLY the new tail, not the 5 MiB prefix.
    assert_eq!(
        bytes.len(),
        fresh.len(),
        "read must be bounded by NEW bytes"
    );
    assert_eq!(bytes.as_slice(), fresh.as_slice());
    // ...and it is byte-identical to the slice the OLD whole-file read produced.
    let whole = std::fs::read(&path).unwrap();
    assert_eq!(
        bytes.as_slice(),
        &whole[cursor as usize..],
        "the tail must equal the old code's bytes[cursor..] slice"
    );
}

/// (2) MidRun across MULTIPLE sequential drains: each pass reads only the tail
/// appended since the last cursor, and the cursor advances identically to the
/// old whole-file path — proving incremental draining loses nothing.
#[test]
fn read_usage_tail_incremental_across_multiple_drains_matches_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.log");

    // Drain 1: two complete lines.
    std::fs::write(&path, b"a\nb\n").unwrap();
    assert_eq!(
        select_new(&path, 0, DrainMode::MidRun),
        (Some(b"a\nb\n".to_vec()), 4)
    );
    assert_eq!(
        select_new(&path, 0, DrainMode::MidRun),
        select_old(&path, 0, DrainMode::MidRun)
    );
    // The read at cursor 4 sees only the NEW bytes (none yet) — an empty tail.
    assert_eq!(
        read_usage_tail(&path, 4),
        UsageTail::Tail { bytes: Vec::new() }
    );

    // Drain 2: append two more lines; draining from cursor 4 consumes ONLY them.
    std::fs::write(&path, b"a\nb\nc\nd\n").unwrap();
    let got = read_usage_tail(&path, 4);
    assert_eq!(
        got,
        UsageTail::Tail {
            bytes: b"c\nd\n".to_vec()
        },
        "only the new tail"
    );
    assert_eq!(
        select_new(&path, 4, DrainMode::MidRun),
        (Some(b"c\nd\n".to_vec()), 8)
    );
    assert_eq!(
        select_new(&path, 4, DrainMode::MidRun),
        select_old(&path, 4, DrainMode::MidRun)
    );

    // A partial trailing line (no newline yet) waits — MidRun consumes nothing.
    std::fs::write(&path, b"a\nb\nc\nd\nhalf").unwrap();
    assert_eq!(select_new(&path, 8, DrainMode::MidRun), (None, 8));
    assert_eq!(
        select_new(&path, 8, DrainMode::MidRun),
        select_old(&path, 8, DrainMode::MidRun)
    );
}

/// (3) Terminal newline-less tail (the H1 fix): the process is dead, so a final
/// usage line flushed WITHOUT a trailing newline is consumed to end-of-log via
/// the incremental read too — never stranded.
#[test]
fn new_read_path_terminal_consumes_a_newline_less_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.log");
    let head = b"old\n"; // already consumed
    let newline_less_tail = b"KTESIO_USAGE {\"sequence\":0}"; // no trailing \n
    let mut content = head.to_vec();
    content.extend_from_slice(newline_less_tail);
    std::fs::write(&path, &content).unwrap();
    let cursor = head.len() as u64;
    let end = content.len() as u64;
    // From a cursor past "old\n", Terminal consumes the whole newline-less tail
    // to end-of-log (H1) — never stranding the final usage line.
    assert_eq!(
        select_new(&path, cursor, DrainMode::Terminal),
        (Some(newline_less_tail.to_vec()), end)
    );
    assert_eq!(
        select_new(&path, cursor, DrainMode::Terminal),
        select_old(&path, cursor, DrainMode::Terminal),
        "Terminal newline-less tail must be byte-identical via the incremental read"
    );
    // Contrast: MidRun would strand that partial line (no newline) — unchanged.
    assert_eq!(select_new(&path, cursor, DrainMode::MidRun), (None, cursor));
}

/// (4) M2 shrink guard: a file shorter than the cursor (truncate/rotation) snaps
/// the cursor to the new length and ingests NOTHING — never re-reads from 0
/// (which would double-count → an inflated bill).
#[test]
fn read_usage_tail_shrink_snaps_the_cursor_and_reads_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.log");
    std::fs::write(&path, b"short").unwrap(); // len 5, cursor claims 100
    assert_eq!(
        read_usage_tail(&path, 100),
        UsageTail::Shrunk { new_cursor: 5 }
    );
    // Same decision as the old whole-file path, in both modes, ingesting nothing.
    assert_eq!(select_new(&path, 100, DrainMode::MidRun), (None, 5));
    assert_eq!(
        select_new(&path, 100, DrainMode::MidRun),
        select_old(&path, 100, DrainMode::MidRun)
    );
    assert_eq!(select_new(&path, 100, DrainMode::Terminal), (None, 5));
    assert_eq!(
        select_new(&path, 100, DrainMode::Terminal),
        select_old(&path, 100, DrainMode::Terminal)
    );
}

/// A missing/unreadable log is a best-effort skip (cursor untouched), exactly
/// like the old `std::fs::read` `Err(_)` arm.
#[test]
fn read_usage_tail_missing_file_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does-not-exist.log");
    assert_eq!(read_usage_tail(&path, 0), UsageTail::Unavailable);
    assert_eq!(
        select_new(&path, 7, DrainMode::MidRun),
        (None, 7),
        "cursor untouched"
    );
    assert_eq!(
        select_new(&path, 7, DrainMode::MidRun),
        select_old(&path, 7, DrainMode::MidRun)
    );
}

/// The exhaustive equivalence harness: for a battery of (content, cursor, mode)
/// states, the block consumed AND the resulting cursor are byte-identical
/// between the OLD whole-file path and the NEW incremental path. This is the
/// reviewer's "counts are unchanged" oracle — falsifying it means the fix
/// changed billing, and this test would fail.
#[test]
fn new_read_path_matches_whole_file_path_over_a_battery_of_states() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.log");
    let contents: &[&[u8]] = &[
        b"",
        b"\n",
        b"a\nb\n",
        b"a\nb\nhalf-written",
        b"KTESIO_USAGE {\"sequence\":0,\"input_tokens\":10,\"output_tokens\":20}\n",
        b"KTESIO_USAGE {\"sequence\":0,\"input_tokens\":10,\"output_tokens\":20}", // no nl
        b"line-with-no-newline-at-all",
    ];
    for content in contents {
        std::fs::write(&path, content).unwrap();
        let len = content.len() as u64;
        // Cursors at, around, and beyond the length (the last exercises shrink).
        for cursor in [0u64, 1, len.saturating_sub(1), len, len + 1, len + 1000] {
            for mode in [DrainMode::MidRun, DrainMode::Terminal] {
                assert_eq!(
                    select_new(&path, cursor, mode),
                    select_old(&path, cursor, mode),
                    "divergence at content={content:?} cursor={cursor} mode={mode:?}"
                );
            }
        }
    }
}

/// The NEW selection logic, mirroring `drain_usage_for`'s incremental path
/// (minus the ingest side effect): returns the block that would be handed to
/// `usage_source.drain` and the resulting ABSOLUTE `usage_cursor`.
fn select_new(path: &Path, cursor: u64, mode: DrainMode) -> (Option<Vec<u8>>, u64) {
    match read_usage_tail(path, cursor) {
        UsageTail::Unavailable => (None, cursor),
        UsageTail::Shrunk { new_cursor } => (None, new_cursor),
        UsageTail::Tail { bytes } => match plan_drain(&bytes, 0, mode) {
            DrainPlan::Consume {
                range,
                new_cursor: consumed,
            } => (Some(bytes[range].to_vec()), cursor + consumed),
            DrainPlan::Nothing | DrainPlan::Shrunk { .. } => (None, cursor),
        },
    }
}

/// The OLD selection logic, mirroring the PRE-AI-63 `drain_usage_for` (whole-file
/// `std::fs::read` + `plan_drain(&bytes, cursor, mode)`) — the reference the new
/// path must match byte-for-byte.
fn select_old(path: &Path, cursor: u64, mode: DrainMode) -> (Option<Vec<u8>>, u64) {
    let Ok(bytes) = std::fs::read(path) else {
        return (None, cursor);
    };
    match plan_drain(&bytes, cursor, mode) {
        DrainPlan::Shrunk { new_cursor } => (None, new_cursor),
        DrainPlan::Nothing => (None, cursor),
        DrainPlan::Consume { range, new_cursor } => (Some(bytes[range].to_vec()), new_cursor),
    }
}

// ---- Story 4-2: read_agent_log_since's follow-cursor planning (AC-D/AC-H) ----

#[test]
fn plan_follow_consumes_only_complete_lines_leaving_a_partial_tail() {
    let bytes = b"a\nb\nhalf-written";
    assert_eq!(
        plan_follow(bytes, 0),
        FollowPlan::Consume {
            range: 0..4,
            new_cursor: 4
        }
    );
}

#[test]
fn plan_follow_with_no_newline_yet_consumes_nothing() {
    assert_eq!(
        plan_follow(b"no newline yet", 0),
        FollowPlan::Consume {
            range: 0..0,
            new_cursor: 0
        }
    );
}

#[test]
fn plan_follow_from_a_cursor_consumes_only_the_new_tail() {
    let bytes = b"old\nnew-tail\n";
    assert_eq!(
        plan_follow(bytes, 4),
        FollowPlan::Consume {
            range: 4..bytes.len(),
            new_cursor: bytes.len() as u64
        }
    );
}

#[test]
fn plan_follow_shrink_snaps_the_cursor_and_delivers_nothing() {
    // AC-D/AC-H rotation-notice path: the file is shorter than the
    // cursor (a rotation happened since the last poll). Snap, deliver
    // nothing this pass — the caller detects the snap-back itself.
    let bytes = b"short"; // len 5
    assert_eq!(
        plan_follow(bytes, 100),
        FollowPlan::Shrunk { new_cursor: 5 }
    );
}

#[test]
fn plan_follow_at_end_of_log_consumes_nothing() {
    let bytes = b"a\nb\n";
    assert_eq!(
        plan_follow(bytes, 4),
        FollowPlan::Consume {
            range: 4..4,
            new_cursor: 4
        }
    );
}

// ---- Story 4-2: engine-attributed line rendering (Task 4) ----

#[test]
fn cause_suffix_and_engine_transition_line_text_cover_every_transition_cause() {
    // A direct, pure-function proof of the RENDERER's own completeness,
    // independent of which causes the current supervisor wiring happens
    // to route a live log_capture through (e.g. a launch-failure never
    // gets a log_capture today — see the Dev Agent Record) — the match
    // itself must stay exhaustive and correct for every variant.
    let cases: Vec<(TransitionCause, &str)> = vec![
        (TransitionCause::command("start"), " (start)"),
        (TransitionCause::AdapterReady, ""),
        (
            TransitionCause::launch_error("boom"),
            " (launch error: boom)",
        ),
        (TransitionCause::StopGraceful, ""),
        (
            TransitionCause::stop_forced("escalated"),
            " (forced: escalated)",
        ),
        (
            TransitionCause::pause_best_effort("windows"),
            " (best-effort: windows)",
        ),
        (
            TransitionCause::resume_best_effort("windows"),
            " (best-effort: windows)",
        ),
        (
            TransitionCause::crashed("exit code 1"),
            " (crashed: exit code 1)",
        ),
        (
            TransitionCause::restarted(2, 500),
            " (restart #2, waited 500ms)",
        ),
        (
            TransitionCause::budget_exceeded(BreachScope::PerRun, 1000, 1200),
            " (breach: per-run tokens)",
        ),
        (
            TransitionCause::cost_cap_exceeded(
                BreachScope::Cumulative,
                Micros(5_000_000),
                Micros(5_250_000),
                EstimateLabel::Estimated,
            ),
            " (breach: cumulative dollars)",
        ),
    ];
    for (cause, want_suffix) in cases {
        assert_eq!(cause_suffix(&cause), want_suffix, "{cause:?}");
    }

    // engine_transition_line_text wraps the suffix into the full "engine:
    // A -> B(...)" sentence.
    let event = TransitionEvent::new(
        "svc",
        LifecycleState::Running,
        LifecycleState::Paused,
        TransitionCause::pause_best_effort("windows"),
        "2026-07-15T00:00:00Z",
    );
    assert_eq!(
        engine_transition_line_text(&event),
        "engine: running -> paused (best-effort: windows)"
    );
}

// ---- Story 4-2: Supervisor::read_agent_log / read_agent_log_since ----

fn log_line(instance: &str, stream: LogStream, text: &str, at: &str) -> LogLine {
    LogLine::new(instance, stream, text, at)
}

#[test]
fn read_agent_log_on_an_unregistered_name_is_not_found() {
    // The deliberate improvement over read_events/read_breach_events'
    // precedent: an unregistered name is NotFound, not a silent empty.
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let err = Supervisor::read_agent_log(&registry, "ghost").unwrap_err();
    assert!(matches!(err, EngineError::NotFound { .. }), "{err:?}");
}

#[test]
fn read_agent_log_on_a_registered_but_never_started_instance_is_empty_not_an_error() {
    let (_state, _manifest, registry) = setup_fake("neverstarted", &["--linger-ms", "600000"]);
    let (lines, cursor) = Supervisor::read_agent_log(&registry, "neverstarted").unwrap();
    assert!(lines.is_empty());
    assert_eq!(cursor, 0, "no file yet → the cursor starts at 0");
}

#[test]
fn read_agent_log_reports_a_damaged_generation_as_a_typed_error_naming_the_file() {
    // `kt agent logs` is the only window a user has into what an agent
    // actually said, so a damaged log must FAIL LOUDLY and name the exact
    // file. The dangerous alternative is not a panic — it is a silent skip:
    // dropping an unparseable line (or an unreadable generation) would return
    // a shorter, plausible-looking log and hide agent output the user is
    // reading precisely because something went wrong. All three damage sites
    // are asserted because they are three separate arms on the read path, and
    // each one names a DIFFERENT path (a rotated generation vs the current
    // one), which is the part that makes the diagnostic actionable.
    let corrupt_line = "{not valid json}\n";

    // (1) The CURRENT generation contains an unparseable line.
    {
        let (_state, _manifest, registry) = setup_fake("badcurrent", &["--linger-ms", "600000"]);
        let name = InstanceName::new("badcurrent").unwrap();
        std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
        let current = registry.attributed_output_log_path(&name);
        std::fs::write(&current, corrupt_line).unwrap();

        let err = Supervisor::read_agent_log(&registry, "badcurrent").unwrap_err();
        match err {
            EngineError::Log { name, path, detail } => {
                assert_eq!(name, "badcurrent");
                assert_eq!(path, current.to_string_lossy());
                // The line NUMBER is what makes this fixable by hand.
                assert!(detail.contains("corrupt output-log line 1"), "{detail}");
            }
            other => panic!("expected EngineError::Log, got {other:?}"),
        }
    }

    // (2) A ROTATED generation contains an unparseable line — the error must
    // name THAT generation's path, not the current one, or the user deletes
    // the wrong file.
    {
        let (_state, _manifest, registry) = setup_fake("badgen", &["--linger-ms", "600000"]);
        let name = InstanceName::new("badgen").unwrap();
        std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
        let rotated = registry.attributed_output_log_generation_path(&name, 1);
        std::fs::write(&rotated, corrupt_line).unwrap();
        // A perfectly good current generation must NOT rescue the read.
        std::fs::write(
            registry.attributed_output_log_path(&name),
            format!(
                "{}\n",
                serde_json::to_string(&log_line(
                    "badgen",
                    LogStream::Engine,
                    "fine",
                    "2026-07-15T00:00:00Z"
                ))
                .unwrap()
            ),
        )
        .unwrap();

        let err = Supervisor::read_agent_log(&registry, "badgen").unwrap_err();
        match err {
            EngineError::Log { path, .. } => {
                assert_eq!(path, rotated.to_string_lossy());
            }
            other => panic!("expected EngineError::Log, got {other:?}"),
        }
    }

    // (3) The CURRENT log path is unreadable for a reason OTHER than "missing"
    // — a directory sits where the file belongs. A missing file is a legitimate
    // empty log (asserted elsewhere); this must NOT be quietly folded into that
    // case, because "no output yet" and "your log is broken" are different
    // answers to the user's question.
    {
        let (_state, _manifest, registry) = setup_fake("blocked", &["--linger-ms", "600000"]);
        let name = InstanceName::new("blocked").unwrap();
        std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
        let current = registry.attributed_output_log_path(&name);
        std::fs::create_dir(&current).unwrap();

        let err = Supervisor::read_agent_log(&registry, "blocked").unwrap_err();
        match err {
            EngineError::Log { name, path, detail } => {
                assert_eq!(name, "blocked");
                assert_eq!(path, current.to_string_lossy());
                assert!(!detail.is_empty(), "the OS detail must be preserved");
            }
            other => panic!("expected EngineError::Log, got {other:?}"),
        }
    }
}

#[test]
fn read_agent_log_concatenates_generations_oldest_to_newest() {
    // AC-A/AC-G: hand-craft the 3 generations directly (deterministic,
    // no real rotation/process needed) and assert the read order is
    // oldest-generation-first, current-generation-last — append order,
    // never a timestamp re-sort.
    let (_state, _manifest, registry) = setup_fake("gens", &["--linger-ms", "600000"]);
    let name = InstanceName::new("gens").unwrap();
    std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();

    let write_line = |path: &Path, l: &LogLine| {
        std::fs::write(path, format!("{}\n", serde_json::to_string(l).unwrap())).unwrap();
    };
    write_line(
        &registry.attributed_output_log_generation_path(&name, 2),
        &log_line(
            "gens",
            LogStream::AgentOut,
            "oldest",
            "2026-07-15T00:00:00Z",
        ),
    );
    write_line(
        &registry.attributed_output_log_generation_path(&name, 1),
        &log_line(
            "gens",
            LogStream::AgentErr,
            "middle",
            "2026-07-15T00:00:01Z",
        ),
    );
    let current_path = registry.attributed_output_log_path(&name);
    write_line(
        &current_path,
        &log_line("gens", LogStream::Engine, "newest", "2026-07-15T00:00:02Z"),
    );

    let (lines, cursor) = Supervisor::read_agent_log(&registry, "gens").unwrap();
    let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, vec!["oldest", "middle", "newest"]);
    let streams: Vec<LogStream> = lines.iter().map(|l| l.stream).collect();
    assert_eq!(
        streams,
        vec![LogStream::AgentOut, LogStream::AgentErr, LogStream::Engine]
    );
    // The returned cursor (M1, review of #80) is the CURRENT generation's
    // exact byte length — matching read_agent_log_since's cursor shape —
    // never the concatenated multi-generation total.
    assert_eq!(
        cursor,
        std::fs::metadata(&current_path).unwrap().len(),
        "cursor must be the CURRENT generation's byte length only"
    );
}

#[test]
fn read_agent_log_since_on_an_unregistered_name_is_not_found() {
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let err = Supervisor::read_agent_log_since(&registry, "ghost", 0).unwrap_err();
    assert!(matches!(err, EngineError::NotFound { .. }), "{err:?}");
}

#[test]
fn read_agent_log_since_happy_path_reads_only_the_new_tail() {
    let (_state, _manifest, registry) = setup_fake("since", &["--linger-ms", "600000"]);
    let name = InstanceName::new("since").unwrap();
    std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
    let path = registry.attributed_output_log_path(&name);

    let l1 = log_line("since", LogStream::AgentOut, "one", "2026-07-15T00:00:00Z");
    std::fs::write(&path, format!("{}\n", serde_json::to_string(&l1).unwrap())).unwrap();
    let (first, cursor1) = Supervisor::read_agent_log_since(&registry, "since", 0).unwrap();
    assert_eq!(first, vec![l1]);
    assert!(cursor1 > 0);

    // No new bytes yet: an empty read at the same cursor.
    let (none, cursor_same) =
        Supervisor::read_agent_log_since(&registry, "since", cursor1).unwrap();
    assert!(none.is_empty());
    assert_eq!(cursor_same, cursor1);

    // Append a second line; read_agent_log_since(cursor1) returns ONLY it.
    let l2 = log_line("since", LogStream::AgentErr, "two", "2026-07-15T00:00:01Z");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    use std::io::Write as _;
    writeln!(f, "{}", serde_json::to_string(&l2).unwrap()).unwrap();
    drop(f);
    let (second, cursor2) = Supervisor::read_agent_log_since(&registry, "since", cursor1).unwrap();
    assert_eq!(second, vec![l2]);
    assert!(cursor2 > cursor1);
}

#[test]
fn read_agent_log_since_detects_a_rotation_shrink_and_snaps_the_cursor() {
    // AC-D/AC-H: simulate a rotation having happened between two polls by
    // shrinking the current-generation file below the previously
    // returned cursor. The caller (Task 6's CLI) detects this by
    // comparing `next_cursor < cursor` — assert that property holds.
    let (_state, _manifest, registry) = setup_fake("rot", &["--linger-ms", "600000"]);
    let name = InstanceName::new("rot").unwrap();
    std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
    let path = registry.attributed_output_log_path(&name);

    let l1 = log_line(
        "rot",
        LogStream::AgentOut,
        "before-rotation",
        "2026-07-15T00:00:00Z",
    );
    std::fs::write(&path, format!("{}\n", serde_json::to_string(&l1).unwrap())).unwrap();
    let (_lines, cursor) = Supervisor::read_agent_log_since(&registry, "rot", 0).unwrap();
    assert!(cursor > 0);

    // Simulate rotation: the current generation is now a FRESH, SHORTER
    // file (as if it had just been rotated and a new line appended).
    let l2 = log_line(
        "rot",
        LogStream::AgentOut,
        "after-rotation",
        "2026-07-15T00:00:05Z",
    );
    std::fs::write(&path, format!("{}\n", serde_json::to_string(&l2).unwrap())).unwrap();
    let new_len = std::fs::metadata(&path).unwrap().len();
    assert!(new_len < cursor, "the fixture must genuinely shrink");

    let (lines, next_cursor) = Supervisor::read_agent_log_since(&registry, "rot", cursor).unwrap();
    assert!(lines.is_empty(), "nothing delivered on the shrink pass");
    assert!(
        next_cursor < cursor,
        "the returned cursor must snap BELOW the one just passed in — the \
             caller's rotation-notice signal"
    );
    assert_eq!(next_cursor, new_len);
}

#[test]
fn read_agent_log_on_an_invalid_name_is_invalid_name() {
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let err = Supervisor::read_agent_log(&registry, "Not Valid!").unwrap_err();
    assert!(matches!(err, EngineError::InvalidName { .. }), "{err:?}");
}

#[test]
fn read_agent_log_since_on_an_invalid_name_is_invalid_name() {
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let err = Supervisor::read_agent_log_since(&registry, "Not Valid!", 0).unwrap_err();
    assert!(matches!(err, EngineError::InvalidName { .. }), "{err:?}");
}

#[test]
fn read_agent_log_since_on_a_registered_but_never_started_instance_is_empty() {
    // The current-generation file does not exist yet at all (the
    // instance was registered but never started) — an honest empty
    // read (Vec::new fallback for a NotFound file), never an error.
    let (_state, _manifest, registry) = setup_fake("neverstarted2", &["--linger-ms", "600000"]);
    let (lines, cursor) = Supervisor::read_agent_log_since(&registry, "neverstarted2", 0).unwrap();
    assert!(lines.is_empty());
    assert_eq!(cursor, 0);
}

// ---- Story 3-4: engine-observed base_url injection + source selection ----

#[test]
fn invocation_overrides_build_the_reserved_metering_leaf() {
    // AC6 (story 3-4): the engine injects the loopback URL at the reserved
    // `metering.base_url` key as an INVOCATION override, so the adapter's
    // config-mapping delivers it.
    let layer = invocation_overrides(Some("http://127.0.0.1:54321"), None).unwrap();
    let resolved = crate::domain::resolve([
        crate::domain::ConfigLayer::empty(),
        crate::domain::ConfigLayer::empty(),
        crate::domain::ConfigLayer::empty(),
        layer,
    ]);
    assert_eq!(
        resolved
            .value_display(crate::domain::METERING_BASE_URL_KEY)
            .as_deref(),
        Some("http://127.0.0.1:54321"),
        "the loopback URL lands at the reserved metering.base_url leaf"
    );
}

#[test]
fn invocation_overrides_build_the_reserved_memory_dir_leaf() {
    // Story 5-1: the managed memory dir lands at the reserved `memory.dir`
    // leaf as an INVOCATION override.
    let layer = invocation_overrides(None, Some(Path::new("/state/agents/demo/memory"))).unwrap();
    let resolved = crate::domain::resolve([
        crate::domain::ConfigLayer::empty(),
        crate::domain::ConfigLayer::empty(),
        crate::domain::ConfigLayer::empty(),
        layer,
    ]);
    assert_eq!(
        resolved
            .value_display(crate::domain::MEMORY_DIR_KEY)
            .as_deref(),
        Some("/state/agents/demo/memory"),
        "the managed dir lands at the reserved memory.dir leaf"
    );
    // And a hand-set lower-layer value CANNOT win (AD-9 invocation precedence).
    let lower = crate::domain::ConfigLayer::parse(
        crate::domain::SourceLayer::Instance,
        "<test>",
        "[memory]\ndir = \"/operator/set\"\n",
    )
    .unwrap();
    let resolved = crate::domain::resolve([
        crate::domain::ConfigLayer::empty(),
        crate::domain::ConfigLayer::empty(),
        lower,
        invocation_overrides(None, Some(Path::new("/engine/computed/memory"))).unwrap(),
    ]);
    assert_eq!(
        resolved
            .value_display(crate::domain::MEMORY_DIR_KEY)
            .as_deref(),
        Some("/engine/computed/memory"),
    );
}

#[test]
fn invocation_overrides_compose_both_and_are_none_for_neither() {
    // Both engine-injected values compose into ONE layer; neither → None so
    // the caller keeps the plain operator config.
    let combined = invocation_overrides(Some("http://127.0.0.1:1"), Some(Path::new("/m"))).unwrap();
    let resolved = crate::domain::resolve([
        crate::domain::ConfigLayer::empty(),
        crate::domain::ConfigLayer::empty(),
        crate::domain::ConfigLayer::empty(),
        combined,
    ]);
    assert_eq!(
        resolved
            .value_display(crate::domain::METERING_BASE_URL_KEY)
            .as_deref(),
        Some("http://127.0.0.1:1")
    );
    assert_eq!(
        resolved
            .value_display(crate::domain::MEMORY_DIR_KEY)
            .as_deref(),
        Some("/m")
    );
    assert!(invocation_overrides(None, None).is_none());
}

#[test]
fn memory_delivery_notice_fires_only_when_attached_but_unmapped() {
    // DC-10 decision table: attached + unmapped ⇒ the notice names the
    // instance + path + the reserved key; mapped or unattached ⇒ silence.
    let name = InstanceName::new("svc").unwrap();
    let dir = Path::new("/state/agents/svc/memory");
    let unmapped = ConfigMapping::new(); // declares nothing
    let notice = memory_delivery_notice(Some(dir), &unmapped, &name).unwrap();
    assert!(notice.contains("svc"), "{notice}");
    assert!(notice.contains(dir.to_string_lossy().as_ref()), "{notice}");
    assert!(notice.contains("memory.dir"), "{notice}");

    let mapped = ConfigMapping::new().with(
        crate::domain::MEMORY_DIR_KEY,
        hekma_adapter_api::ConfigTarget::env("SVC_MEMORY_DIR"),
    );
    assert!(memory_delivery_notice(Some(dir), &mapped, &name).is_none());
    assert!(memory_delivery_notice(None, &unmapped, &name).is_none());
}

#[test]
fn self_reported_start_observed_listener_is_a_no_op() {
    // Source selection: a `self-reported` instance's start path is UNCHANGED —
    // start_observed_listener returns Ok(None), NO listener (even with an upstream
    // configured, which a self-reported instance ignores).
    let (_state, _manifest, registry) = setup_fake("selfrep_obs", &["--linger-ms", "1000"]);
    let name = InstanceName::new("selfrep_obs").unwrap();
    registry
        .set_config(&name, "metering.upstream_base_url", "http://127.0.0.1:9")
        .unwrap();
    let effective = registry
        .effective_config(&name, crate::domain::ConfigLayer::empty())
        .unwrap();
    let sup = Supervisor::with_backoff(fast_backoff());
    // self-reported (the fake manifest declares self-reported) → Ok(None).
    let result = sup
        .start_observed_listener(&name, "self-reported", &effective)
        .expect("self-reported is a no-op, not an error");
    assert!(
        result.is_none(),
        "a self-reported instance runs no listener"
    );
}

#[test]
fn engine_observed_without_upstream_rejects_with_a_clear_error() {
    // AC-A: an `engine-observed` instance with NO configured upstream URL rejects
    // start_observed_listener with a traffic-free ObservedMetering error naming the
    // key (nothing to forward to). Uses the handle-less test supervisor, but the
    // upstream check fails FIRST (before the runtime-handle check), so the error
    // names the missing config key.
    let (_state, _manifest, registry) = setup_fake("obs_noup", &["--linger-ms", "1000"]);
    let name = InstanceName::new("obs_noup").unwrap();
    let effective = registry
        .effective_config(&name, crate::domain::ConfigLayer::empty())
        .unwrap();
    let sup = Supervisor::with_backoff(fast_backoff());
    let err = match sup.start_observed_listener(&name, "engine-observed", &effective) {
        Err(e) => e,
        Ok(_) => panic!("an engine-observed instance with no upstream must reject"),
    };
    match err {
        EngineError::ObservedMetering { name: n, detail } => {
            assert_eq!(n, "obs_noup");
            assert!(
                detail.contains("metering.upstream_base_url"),
                "detail names the missing key: {detail}"
            );
        }
        other => panic!("expected ObservedMetering, got {other:?}"),
    }
}

#[test]
fn engine_observed_without_runtime_handle_rejects_cleanly() {
    // With an upstream configured but NO engine runtime handle (the handle-less
    // test supervisor), an engine-observed start rejects with a clear, traffic-free
    // error rather than panicking — the async engine is required to observe.
    let (_state, _manifest, registry) = setup_fake("obs_nort", &["--linger-ms", "1000"]);
    let name = InstanceName::new("obs_nort").unwrap();
    registry
        .set_config(&name, "metering.upstream_base_url", "http://127.0.0.1:9")
        .unwrap();
    let effective = registry
        .effective_config(&name, crate::domain::ConfigLayer::empty())
        .unwrap();
    let sup = Supervisor::with_backoff(fast_backoff()); // no runtime handle
    let err = match sup.start_observed_listener(&name, "engine-observed", &effective) {
        Err(e) => e,
        Ok(_) => panic!("no runtime handle must reject an engine-observed start"),
    };
    assert!(
        matches!(err, EngineError::ObservedMetering { .. }),
        "expected ObservedMetering, got {err:?}"
    );
    assert!(
        err.to_string().contains("runtime"),
        "names the cause: {err}"
    );
}

// ---- Story 12-1: detached start — the refusal + the in-process shape ----

/// Write an `engine-observed` manifest (the `adoption.rs` shape) whose start
/// exec is `fake_agent` + `args`.
fn write_observed_manifest(dir: &Path, kind: &str, args: &[&str]) {
    let bin = hekma_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"engine-observed\"\n\n\
             [config.\"metering.base_url\"]\nenv = \"OPENAI_BASE_URL\"\n",
            exec = bin.to_string_lossy(),
        );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

#[test]
fn detached_start_of_an_engine_observed_instance_is_refused_before_any_side_effect() {
    // Story 12-1 (the AC): `start_detached` on an `engine-observed`
    // manifest refuses BEFORE ANY SIDE EFFECT — no `starting` transition,
    // no loopback listener, no spawn, no write-ahead record (a refusal
    // must not leave a phantom `running` row or a live half-start). The
    // error names WHY (the listener dies with the command) + the
    // remediation. The handle-less supervisor suffices: the refusal fires
    // before the listener would ever need a runtime.
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    write_observed_manifest(manifest.path(), "obsdetach", &["--linger-ms", "600000"]);
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry
        .register_with_adapter(
            "obsdetach",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    let err = sup.start_detached(&registry, "obsdetach").unwrap_err();
    match &err {
        EngineError::DetachRefused { name, detail } => {
            assert_eq!(name, "obsdetach");
            assert!(
                detail.contains("listener") && detail.contains("dies with the command"),
                "the refusal must name the listener-lifetime why: {detail}"
            );
            assert!(
                detail.contains("without --detach"),
                "the refusal must carry the remediation: {detail}"
            );
        }
        other => panic!("expected DetachRefused, got {other:?}"),
    }
    // NO side effects: the row is still `registered`, and no write-ahead
    // spawn record exists (nothing was spawned or half-started).
    let instance = registry
        .lookup(&InstanceName::new("obsdetach").unwrap())
        .unwrap();
    assert_eq!(
        instance.state,
        LifecycleState::Registered,
        "the refusal must leave the prior state untouched"
    );
    assert!(
        registry
            .spawn_record(&InstanceName::new("obsdetach").unwrap())
            .unwrap()
            .is_none(),
        "the refusal must not leave a write-ahead spawn record"
    );
    // And a NON-detached start of the same instance is NOT refused by this
    // arm (it proceeds to the listener path — here the handle-less
    // supervisor's ObservedMetering failure, which proves the refusal is
    // detach-specific, not a general observed-start blocker).
    let err2 = sup.start(&registry, "obsdetach").unwrap_err();
    assert!(
        matches!(err2, EngineError::ObservedMetering { .. }),
        "a plain start of the observed instance must not hit DetachRefused, got {err2:?}"
    );
}

#[test]
fn detached_start_of_a_self_reported_instance_runs_and_stops_in_process() {
    // Story 12-1: a self-reported instance detaches fine (no listener to
    // strand): it starts to `running`, the write-ahead record is committed
    // (the re-adoption input — detach must NOT clear or skip it), and
    // stop/pause keep working IN-PROCESS for as long as this supervisor
    // holds the (disarmed) handle. The survival-across-exit half is proven
    // end-to-end in tests/adoption.rs; this pins the in-process contract.
    let (_state, _manifest, registry) = setup_fake("detachproc", &["--linger-ms", "600000"]);
    let name = InstanceName::new("detachproc").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    let instance = sup.start_detached(&registry, "detachproc").unwrap();
    assert_eq!(instance.state, LifecycleState::Running);
    assert!(
        registry.spawn_record(&name).unwrap().is_some(),
        "a detached start must still commit the write-ahead record (adoption input)"
    );
    let stopped = sup
        .stop(&registry, "detachproc", Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
}

#[test]
fn adopt_orphans_surfaces_the_park_loss_window_on_a_successful_adoption() {
    // AI-18 (surfaced-not-silent), 2026-09-22 hardening: a successful
    // adoption re-derives the ingest cursors at the CURRENT end of the logs,
    // so usage the previous engine still held in memory at its death (its
    // park) would be skipped SILENTLY unless the adoption says so. This pins
    // the adoption-time diagnostic through the choke point (capture sink —
    // it lands in the capture, never on stderr in tests).
    let (_state, _manifest, registry) = setup_fake("parknote", &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start_detached(&registry, "parknote").unwrap();
    // The engine "restarts": drop this supervisor — the detached handle is
    // DISARMED, so the agent survives — and re-open over the same registry.
    drop(sup);
    let mut sup2 = Supervisor::with_backoff(fast_backoff());
    let buffer = install_capture_sink(&mut sup2);
    assert_eq!(sup2.adopt_orphans(&registry), 1);
    let text = sink_text(&buffer);
    assert!(
        text.contains("adopted from a previous engine"),
        "the adoption must surface the engine-restart fact: {text}"
    );
    assert!(
        text.contains("goes unaccounted"),
        "the adoption must name the un-flushed park window: {text}"
    );
}

#[test]
fn a_detached_start_force_clears_the_stdin_pipe_despite_guaranteed_interaction() {
    // Review round 2 (verification gap): the detached stdin force-clear
    // (`pipe_stdin && !detach` in `start_inner`) had NO test — the backend
    // unit tests pre-clear the flag on their specs, so the supervisor's
    // clear was never observed end to end. The premise: a manifest that
    // DECLARES guaranteed interaction (an ATTACHED start would get a live
    // pipe). With `--detach` the pipe is force-cleared before the spawn,
    // so the observable is a HARD send failure — the no-pipe
    // `InteractionUnavailable`, never a silent write into a dead pipe and
    // never an EPIPE strand (the story 12-1 I/O matrix row).
    let (state, _manifest, registry) =
        setup_pause_guaranteed("detstdin", &["--linger-ms", "600000"]);
    let _ = state;
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start_detached(&registry, "detstdin").unwrap();
    let err = sup
        .send_input(&registry, "detstdin", "hello\n")
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::InteractionUnavailable { detail, .. }
                if detail.contains("no live stdin pipe")),
        "a detached start must hold NO stdin pipe even on a guaranteed-interaction \
             manifest; got {err:?}"
    );
    sup.stop(&registry, "detstdin", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn a_crashed_detached_agent_restarts_detached_and_the_record_keeps_the_flag() {
    // Review round 2 (behavioral): `restart` used to hardcode the spawn's
    // detach flag to `false` — a crashed DETACHED agent restarted
    // ATTACHED, and the rewritten record silently downgraded
    // detach=true→false, so the next engine exit would kill the agent the
    // operator asked to survive. The restart must read the flag off the
    // surviving record and pass it through: the restarted agent stays
    // detached and the record still carries detach=true. (A real crash is
    // simulated with `--crash-after-ms` — NOT a stop, which would CLEAR
    // the record and make the fallback-to-attached read vacuous.)
    let (_state, _manifest, registry) =
            // 450ms: AFTER the 300ms readiness window (an earlier crash is a
            // launch failure, not a post-start crash — see
            // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`).
            setup_fake("detachrestart", &["--crash-after-ms", "450"]);
    let name = InstanceName::new("detachrestart").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start_detached(&registry, "detachrestart").unwrap();
    assert!(
        registry.spawn_record(&name).unwrap().unwrap().detach,
        "premise: the detached start recorded detach=true"
    );

    // The agent crashes on its own; the reaper lands the honest
    // `running → failed` transition and the record SURVIVES (a crash is
    // not a clean stop — the record is the restart's input).
    std::thread::sleep(Duration::from_millis(700));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        sup.poll_once(&registry);
        if sup.running.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the crash was never reaped");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        registry.spawn_record(&name).unwrap().unwrap().detach,
        "the crash must leave the record (and its detach flag) intact"
    );

    // The Restart Policy restart: the flag comes off the record, the
    // restarted spawn stays detached, and the rewritten record KEEPS it.
    let instance = sup
        .restart(&registry, "detachrestart", 1, Duration::from_millis(10))
        .unwrap();
    assert_eq!(instance.state, LifecycleState::Running);
    let record = registry.spawn_record(&name).unwrap().unwrap();
    assert!(
        record.detach,
        "the RESTARTED record must still carry detach=true (the flag survives the \
             crash-restart, never silently downgraded)"
    );
    // And the restarted instance is stoppable in-process like any
    // detached handle (control ops are not disarmed — only the drop is).
    sup.stop(&registry, "detachrestart", Some(Duration::from_secs(5)))
        .unwrap();
}

// ---- Story 4-1: `send_input` — narrow branches best exercised as
// Supervisor-level unit tests (no reaper/Engine involved), complementing
// the AC-level proofs in `crates/hekma-engine/tests/interaction.rs`. ----

#[test]
fn send_input_on_invalid_name_is_rejected() {
    // The name-resolve step: an invalid name is rejected with
    // EngineError::InvalidName, BEFORE any registry lookup.
    let (_state, _manifest, registry) = setup_fake("x", &["--linger-ms", "1000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    let err = sup.send_input(&registry, "Bad Name", "hi").unwrap_err();
    assert!(
        matches!(err, EngineError::InvalidName { .. }),
        "expected InvalidName, got {err:?}"
    );
}

#[test]
fn send_input_after_the_process_exits_on_its_own_is_a_backend_error() {
    // A genuine BackendError write failure (Testing Notes: "a genuine
    // BackendError write failure if practically triggerable"). The
    // process exits ON ITS OWN (a short --linger-ms) but nothing has yet
    // reaped/transitioned the persisted row (deliberately no
    // `poll_once` call here — that would reap the handle and transition
    // to `failed`, which is exactly the race this test avoids so the
    // write is genuinely attempted). `send_input`'s write to the now
    // read-end-closed pipe fails at the OS level (EPIPE/BrokenPipe on
    // Unix), mapped to `EngineError::Backend` — the SAME generic mapping
    // every other backend op uses — never silently swallowed, never
    // misreported as `InteractionUnavailable`.
    //
    // Unix-only (EPIPE-on-closed-pipe semantics + a portable "is this pid
    // still alive" probe both need a real Unix liveness check); runtime
    // skip on Windows, NO `#[cfg]` (this file is outside the `backends`
    // allowlist) — mirrors the rest of the codebase's data-driven OS skip
    // convention.
    if OsId::current() == OsId::Windows {
        return;
    }
    // --linger-ms must comfortably EXCEED READINESS_WINDOW (300ms) or the
    // process looks like an immediate-exit launch failure (AC2) instead
    // of a clean start reaching `running`.
    let (_state, _manifest, registry) =
        setup_fake("exiter", &["--echo-stdin", "--linger-ms", "500"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "exiter").unwrap();

    // Wait past the KNOWN, self-configured linger deadline (a FIXED timer
    // this test itself set, not a guess at some other operation's
    // duration — the AI-35/38 "never guess" lesson is about polling
    // unknown async completion, which this is not). A liveness-probing
    // poll loop (`kill -0`) cannot substitute here: since nothing in this
    // test reaps the child (deliberately, so the persisted row stays
    // `running`), the process becomes a defunct zombie on exit, and a
    // zombie still answers `kill -0` as "alive" — the probe would never
    // observe the exit and the loop would spin until its own timeout.
    std::thread::sleep(Duration::from_millis(800));

    // The persisted row is STILL `running` (nothing has reaped it yet) —
    // send_input reaches the write, which fails at the OS level.
    let err = sup.send_input(&registry, "exiter", "hello").unwrap_err();
    assert!(
        matches!(err, EngineError::Backend { .. }),
        "expected Backend, got {err:?}"
    );
}

#[test]
fn send_input_writes_into_a_live_stdin_pipe_that_the_agent_never_reads_and_reports_success() {
    // `send_input`'s SUCCESS arm (`Ok(()) => Ok(())`) at Supervisor level,
    // reachable WITHOUT any stdin round trip: `fake_agent` is spawned with
    // NEITHER `--echo-stdin` NOR `--sniff-stdin-at-startup`, so it provably
    // never reads a byte of its piped stdin — the write simply lands in the
    // OS pipe buffer (64KiB on Linux, 16KiB on macOS, 4KiB on Windows; a
    // handful of bytes never fills any of them) and `write_all` + `flush`
    // return immediately. Nothing here waits on the child for anything.
    //
    // That "no round trip" property is also what makes this the CHEAPEST
    // exerciser of the arm: the four AC-level proofs in
    // `crates/hekma-engine/tests/interaction.rs` own it end to end, but
    // they each pay a real child echo, and a pure unit test that needs
    // nothing back from the child cannot be destabilised by anything
    // happening inside it.
    //
    // Bonus (branch, not line): the two sends straddle AC-F's trailing-
    // newline branch — "hello" takes the `push(b'\n')` side, "world\n" the
    // already-terminated side.
    let (_state, _manifest, registry) = setup_fake("liveio", &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "liveio").unwrap();

    sup.send_input(&registry, "liveio", "hello")
        .expect("a small write into a live, unfilled stdin pipe must succeed");
    sup.send_input(&registry, "liveio", "world\n")
        .expect("an already-newline-terminated write must succeed the same way");

    // Teardown.
    let _ = sup.stop(&registry, "liveio", Some(Duration::from_millis(200)));
}

#[test]
fn send_input_past_the_stdin_pipe_buffer_times_out_and_poisons_the_handle_until_restart() {
    // Two `send_input` arms in one flow, sharing the ONE expensive wait
    // (`STDIN_WRITE_TIMEOUT`, 5s — the product's own bound, paid once):
    //
    //   1. `Err(BackendError::StdinTimedOut { .. })` =>
    //      `EngineError::InteractionTimedOut` — the mapping arm.
    //   2. The `self.backend.stdin_timed_out(..)` PRE-FLIGHT early return —
    //      a handle whose prior write timed out is permanently poisoned for
    //      the rest of this engine session (`StdinState::TimedOut` is never
    //      recoverable; only a stop+start builds a fresh pipe), so a SECOND
    //      send must fail fast with no new write attempted.
    //
    // The two arms are told apart WITHOUT any timing argument: the ordering
    // inside `send_input` puts `stdin_timed_out` BEFORE `has_stdin`, and a
    // `TimedOut` state is not `Live`, so if arm 2 were absent the second
    // send would land on the `has_stdin` check and return the materially
    // DIFFERENT `InteractionUnavailable`. Getting `InteractionTimedOut`
    // back is therefore positive proof that the pre-flight branch ran.
    //
    // DETERMINISM (Epic-2-retro AI-35/38 — never sleep to await state).
    // There is no sleep and no polling here. `fake_agent` without
    // `--echo-stdin`/`--sniff-stdin-at-startup` provably never drains its
    // stdin, and 8MB comfortably outruns every OS pipe buffer, so the write
    // blocks with certainty rather than by luck; the 5s is the engine's own
    // bounded `recv_timeout` elapsing, not a test guessing at a duration.
    // Every assertion is on a RETURNED value. This is exactly the
    // "deterministic stuck-agent harness ... a `fake_agent` flag that
    // provably never drains stdin plus a deliberately-filled pipe, no
    // sleeps" that the Epic 4 retrospective left open as AI-69.
    //
    // INDEPENDENT VALUE BEYOND COVERAGE (AI-69). The retro recorded that
    // exit code 6 / `InteractionTimedOut` has NO end-to-end assertion on any
    // OS — it was pinned only by the `kt` crate's mapper/classifier unit
    // test, composed with the separate end-to-end proofs of codes 0-4. This
    // closes the ENGINE half of that gap: the real `Supervisor`, the real
    // backend, a real non-draining child, and a real bounded write actually
    // producing `InteractionTimedOut` — deterministically and on all three
    // OSes. It does NOT close the CLI half (the `kt` process exiting 6),
    // which still rests on the mapper pin.
    //
    // Robustness: unlike the interaction.rs proofs, this one needs nothing
    // FROM the child — only that it keeps NOT reading — so no property of
    // the child's own output can make it fail.
    let (_state, _manifest, registry) = setup_fake("stuck", &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "stuck").unwrap();

    // Far past any realistic OS pipe buffer, so the write blocks once the
    // buffer fills — the adversarial audit's original reproduction vehicle.
    let huge_payload = "x".repeat(8 * 1024 * 1024);
    let err = sup
        .send_input(&registry, "stuck", &huge_payload)
        .expect_err("a write past the buffer of a never-draining pipe must time out");
    match err {
        EngineError::InteractionTimedOut { name, timeout_secs } => {
            assert_eq!(name, "stuck");
            // Pinned to the product constant the arm forwards, not a
            // literal restated here.
            assert_eq!(timeout_secs, crate::ports::STDIN_WRITE_TIMEOUT.as_secs());
        }
        other => panic!("expected InteractionTimedOut, got {other:?}"),
    }
    // No upper wall-clock bound is asserted on that call: "bounded, not
    // indefinite" is interaction.rs's property to prove (it also needs the
    // engine's shared lock and a second instance), and a tight-margin
    // timing assertion here would buy nothing but flake surface. What this
    // test owns is the ARM, and the arm is proven by the returned value.

    let start = Instant::now();
    let err = sup
        .send_input(&registry, "stuck", "second attempt")
        .expect_err("a poisoned handle must reject every later send");
    let elapsed = start.elapsed();
    match err {
        EngineError::InteractionTimedOut { name, timeout_secs } => {
            assert_eq!(name, "stuck");
            assert_eq!(timeout_secs, crate::ports::STDIN_WRITE_TIMEOUT.as_secs());
        }
        other => panic!("expected a fast-path InteractionTimedOut, got {other:?}"),
    }
    // The fast path does no I/O at all, so it returns in microseconds. The
    // bound is deliberately the FULL production timeout rather than a tight
    // one: with ~6 orders of magnitude of headroom it cannot flake, even
    // under coverage instrumentation, yet it still fails loudly on the one
    // regression it is here to catch — a second doomed bounded write.
    assert!(
        elapsed < crate::ports::STDIN_WRITE_TIMEOUT,
        "the poisoned-handle fast path must not wait out a second bounded write: {elapsed:?}"
    );

    // Teardown: killing the group closes the pipe's read end, so the
    // abandoned 8MB write thread (never joinable by design — see
    // `write_stdin_bounded`'s docs) unblocks with EPIPE and exits on its
    // own; this test does not wait for it.
    let _ = sup.stop(&registry, "stuck", Some(Duration::from_millis(200)));
}

// ---- Epic 11 / story 11-1 tests (AI-9, AI-12, AI-41) ----

/// Write a `fake_agent` manifest declaring GUARANTEED pause for the CURRENT
/// OS (the lib-test sibling of `tests/pause.rs`'s `write_pause_manifest`).
fn write_pause_guaranteed_manifest(dir: &Path, kind: &str, args: &[&str]) {
    let bin = hekma_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let os = match OsId::current() {
        OsId::Linux => "linux",
        OsId::Macos => "macos",
        OsId::Windows => "windows",
        OsId::Other => "other",
    };
    let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]\n\n\
             [capabilities.pause]\n{os} = \"guaranteed\"\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n",
            exec = bin.to_string_lossy(),
        );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// Count `heartbeat <n>` lines in an agent-output log (0 when absent).
fn heartbeat_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|c| c.lines().filter(|l| l.starts_with("heartbeat ")).count())
        .unwrap_or(0)
}

#[test]
fn poll_verdict_tolerates_transient_errors_and_trips_at_the_threshold() {
    // AI-12 pure decision fn: a clean `Alive` resets the streak; an error
    // below MAX_CONSECUTIVE_POLL_ERRORS stays transient (the streak
    // increments — the historical tolerate-and-retry for a short error
    // burst); the error AT the threshold turns into crash input; a real
    // observed exit passes its code through (streak reset — the handle
    // leaves the map anyway).
    let poll_err = || BackendError::Control {
        op: "poll",
        detail: "boom".to_string(),
    };
    // Below the threshold: transient, streak grows one per error.
    for streak in 0..MAX_CONSECUTIVE_POLL_ERRORS - 1 {
        assert_eq!(
            poll_verdict(streak, Err(poll_err())),
            (PollVerdict::TransientError, streak + 1),
            "error {}/{MAX_CONSECUTIVE_POLL_ERRORS} must stay transient",
            streak + 1
        );
    }
    // AT the threshold: the streak stops being tolerated — crash input.
    assert_eq!(
        poll_verdict(MAX_CONSECUTIVE_POLL_ERRORS - 1, Err(poll_err())),
        (PollVerdict::PersistentError, MAX_CONSECUTIVE_POLL_ERRORS)
    );
    // Beyond (defensive; saturating): still crash input.
    assert_eq!(
        poll_verdict(MAX_CONSECUTIVE_POLL_ERRORS, Err(poll_err())),
        (
            PollVerdict::PersistentError,
            MAX_CONSECUTIVE_POLL_ERRORS + 1
        )
    );
    // Clean reads reset the streak to 0.
    assert_eq!(
        poll_verdict(MAX_CONSECUTIVE_POLL_ERRORS - 1, Ok(ProcessStatus::Alive)),
        (PollVerdict::Alive, 0)
    );
    assert_eq!(
        poll_verdict(9, Ok(ProcessStatus::Exited { code: Some(3) })),
        (PollVerdict::Exited(Some(3)), 0)
    );
    assert_eq!(
        poll_verdict(0, Ok(ProcessStatus::Exited { code: None })),
        (PollVerdict::Exited(None), 0)
    );
}

#[test]
fn poll_error_streaks_clear_when_a_handle_is_removed() {
    // AI-12 streak hygiene: a clean poll leaves no streak entry (Ok(Alive)
    // clears), and once a REAL crash is reaped the removed handle's entry is
    // gone — no stale streak can pre-load the instance's next Run.
    let (_state, _manifest, registry) = setup_fake("streak", &["--crash-after-ms", "450"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "streak").unwrap();
    // Clean passes: no entry may survive.
    sup.poll_once(&registry);
    assert!(
        sup.poll_error_streaks.is_empty(),
        "clean polls leave no streak entry: {:?}",
        sup.poll_error_streaks
    );
    // The crash is reaped; the removed handle leaves no entry behind.
    let _ = wait_for_crash(&mut sup, &registry);
    assert!(
        sup.poll_error_streaks.is_empty(),
        "a removed handle must leave no streak entry: {:?}",
        sup.poll_error_streaks
    );
}

/// An in-memory `Write` sink capturing engine diagnostics (the story-10-2
/// `DiagnosticSink`), so tests can assert a diagnostic reaches the sink.
#[derive(Clone)]
struct SharedCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install a capturing diagnostic sink on `sup`; returns the shared buffer.
fn install_capture_sink(sup: &mut Supervisor) -> Arc<Mutex<Vec<u8>>> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    sup.install_diagnostics(Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(
        &buffer,
    ))))));
    buffer
}

/// The sink's captured text so far.
fn sink_text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
}

/// Register TWO `fake_agent`-backed instances in ONE state dir (the
/// cross-handle corroboration tests need multiple live handles in one
/// supervisor). Returns the (state dir, manifest dir, registry).
fn setup_two_fakes(
    names: [&str; 2],
    args: &[&str],
) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    for name in names {
        let dir = manifest.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        write_fake_manifest(&dir, name, args);
        registry
            .register_with_adapter(name, &AdapterRef::Manifest(dir))
            .unwrap();
    }
    (state, manifest, registry)
}

#[test]
fn truncate_for_cause_preserves_short_text_and_bounds_long_text() {
    // AI-12 (loop 1) helper: short text passes through untouched; long text
    // is cut to the bound plus the ellipsis.
    assert_eq!(
        truncate_for_cause("boom", POLL_ERROR_CAUSE_MAX_CHARS),
        "boom"
    );
    let long = "x".repeat(500);
    let cut = truncate_for_cause(&long, 200);
    assert_eq!(cut.chars().count(), 201, "200 chars plus the ellipsis");
    assert!(cut.ends_with('…'), "the cut is marked: {cut}");
}

#[test]
fn poll_once_handle_specific_poll_failure_wiring_lands_failed_and_removes_the_handle() {
    // AI-12 wiring (loop 1, amendment c) — the HANDLE-SPECIFIC direction,
    // through the cfg(test) backend fault seam: ONE handle's persistent
    // poll failure drives the REAL `poll_once` crash path end to end — the
    // instance lands `failed` with the persistent-poll-failure cause
    // carrying the LAST error's text, the handle is removed (its Drop
    // kills the un-pollable group), and no stale streak/error bookkeeping
    // survives. A regression to the old silent `Err(_) => None` leaves the
    // instance `running` forever and fails this test.
    let (_state, _manifest, registry) = setup_fake("wired", &["--linger-ms", "600000"]);
    registry
        .set_restart_policy(&InstanceName::new("wired").unwrap(), RestartPolicy::Never)
        .unwrap();
    let name = InstanceName::new("wired").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "wired").unwrap();

    // Arm the seam: every poll of THIS handle now errors.
    let pid = sup.backend.pid(&sup.running.get(&name).unwrap().handle);
    sup.arm_poll_fault(pid);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let plans = sup.poll_once(&registry);
        if state_of(&registry, "wired") == LifecycleState::Failed {
            assert!(
                plans.is_empty(),
                "a `never` policy must not schedule a restart on the poll-failure crash"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the persistent poll failure never landed the instance failed"
        );
    }
    assert!(
        sup.running.is_empty(),
        "the un-pollable handle must be removed from the supervisor"
    );
    assert!(
        sup.poll_error_streaks.is_empty() && sup.poll_last_errors.is_empty(),
        "the removed handle leaves no streak or last-error entry behind"
    );
    // The crash cause carries the full story: the persistent failure AND
    // the last error's text.
    let events = Supervisor::read_events(&registry, "wired").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Failed);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(cause.contains("persistent poll failure"), "cause={cause}");
    assert!(cause.contains("last error:"), "cause={cause}");
    assert!(
        cause.contains("injected cfg(test) poll fault"),
        "the last poll error's text must reach the cause (AI-12b): {cause}"
    );
}

#[test]
fn poll_once_multi_handle_same_tick_failure_is_environmental_and_keeps_handles_alive() {
    // AI-12 wiring (loop 1, amendment a) — the ENVIRONMENTAL direction: two
    // handles erroring in the SAME tick corroborate as a backend/
    // environment-wide condition (the procfs/sysctl-outage shape). Past the
    // crash threshold many times over: NO streak may grow, NO handle may be
    // crashed (kill-on-drop would kill RUNNING agents), and the
    // environmental diagnostic must reach the diagnostic sink.
    let (_state, _manifest, registry) =
        setup_two_fakes(["enva", "envb"], &["--linger-ms", "600000"]);
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "enva").unwrap();
    sup.start(&registry, "envb").unwrap();
    let sink = install_capture_sink(&mut sup);

    let pid_a = sup.backend.pid(
        &sup.running
            .get(&InstanceName::new("enva").unwrap())
            .unwrap()
            .handle,
    );
    let pid_b = sup.backend.pid(
        &sup.running
            .get(&InstanceName::new("envb").unwrap())
            .unwrap()
            .handle,
    );
    sup.arm_poll_fault(pid_a);
    sup.arm_poll_fault(pid_b);

    for _ in 0..(MAX_CONSECUTIVE_POLL_ERRORS + 2) {
        sup.poll_once(&registry);
    }

    assert_eq!(
        sup.running.len(),
        2,
        "an environmental failure must keep every handle alive"
    );
    assert_eq!(state_of(&registry, "enva"), LifecycleState::Running);
    assert_eq!(state_of(&registry, "envb"), LifecycleState::Running);
    assert!(
        sup.poll_error_streaks.is_empty(),
        "no streak may grow on an environmental tick: {:?}",
        sup.poll_error_streaks
    );
    let text = sink_text(&sink);
    assert!(
        text.contains("environmental poll failure"),
        "the environmental diagnostic must reach the sink: {text}"
    );
    assert!(
        text.contains("handles stay alive"),
        "the diagnostic must say what the guard did: {text}"
    );
}

#[test]
fn pause_and_resume_transition_failures_fail_before_the_signal_persist_first_ai9() {
    // AI-9 (order mirrors `stop_inner`), the loop-1 extended failure-
    // injection proof (mirrors
    // `snapshot_write_failure_rejects_the_start_before_the_starting_transition`),
    // three legs + the guaranteed RESUME leg, all on one live instance:
    //
    // * Leg A (NOTHING commits): make the LOGS DIRECTORY un-creatable (a
    //   file stands where `logs/` must be) so the pause fails at
    //   `ensure_log_dir` — BEFORE any persist. The ledger must still read
    //   `running` (the transition truly did not commit) and no signal may
    //   fire.
    // * Leg B (persist commits, append fails): replace instance.log with a
    //   DIRECTORY so the transition's event append fails. The durable state
    //   LEADS (persist-first): the ledger reads `paused`, and the process
    //   keeps running — no signal was sent.
    // * Resume leg (persist-first for SIGCONT): with the process genuinely
    //   SUSPENDED (a real pause in between) and the append sabotaged again,
    //   resume fails at the append AFTER committing `paused → running` —
    //   and the heartbeat must stay FROZEN: no SIGCONT was delivered. The
    //   pre-AI-9 signal-first order would have woken the agent behind an
    //   errored resume.
    //
    // Windows runtime skips (data-driven `OsId`, NO #[cfg] — the
    // tests/pause.rs idiom): Leg A's directory-rename injection is a
    // sharing violation while the child holds agent.log open, and the two
    // frozen-heartbeat probes observe a REAL suspension that Windows
    // honestly never performs (AD-4 cooperative pause). What Windows still
    // proves cross-platform: Leg B + the resume leg — the persist-first
    // LEDGER semantics (the committed transition visible despite the
    // errored append, the typed Log error) run on every OS.
    let (_state, _manifest, registry) =
        setup_pause_guaranteed("pz", &["--heartbeat-ms", "50", "--linger-ms", "600000"]);
    let name = InstanceName::new("pz").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "pz").unwrap();
    assert_eq!(state_of(&registry, "pz"), LifecycleState::Running);
    let agent_log = registry.agent_output_log_path(&name);
    let log_path = registry.instance_log_path(&name);
    let wait_heartbeat = |at_least: usize| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if heartbeat_lines(&agent_log) >= at_least {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "heartbeat never reached {at_least}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait_heartbeat(2);

    // ---- Leg A: the pause fails BEFORE any persist. ----
    // Runtime-skipped on Windows (data-driven, NO #[cfg] — the
    // tests/pause.rs guaranteed-suspend idiom): the injection RENAMES the
    // live log directory, which Windows refuses while the child holds
    // agent.log / agent-stderr.log open inside it (a sharing violation —
    // os error 5, observed on the first windows-latest CI run). What
    // Windows misses is only this pre-persist INJECTION, not the
    // persist-first contract: legs B + resume below run on every OS.
    if OsId::current() != OsId::Windows {
        let log_dir = registry.instance_log_dir(&name);
        let held_dir = log_dir.with_extension("ai9-held");
        std::fs::rename(&log_dir, &held_dir).unwrap();
        std::fs::write(&log_dir, b"not a directory").unwrap();
        let a_err = sup.pause(&registry, "pz").unwrap_err();
        assert!(
            matches!(&a_err, EngineError::Log { .. }),
            "the ensure_log_dir failure must surface as a typed Log error, got {a_err:?}"
        );
        assert_eq!(
            state_of(&registry, "pz"),
            LifecycleState::Running,
            "with the whole transition rejected, the ledger must still read running"
        );
        std::fs::remove_file(&log_dir).unwrap();
        std::fs::rename(&held_dir, &log_dir).unwrap();
        let a_before = heartbeat_lines(&agent_log);
        std::thread::sleep(Duration::from_millis(400));
        let a_after = heartbeat_lines(&agent_log);
        assert!(
                a_after > a_before,
                "a pause rejected before any persist must NOT have signalled SIGSTOP: heartbeat {a_before} -> {a_after}"
            );
    }

    // ---- Leg B: the persist commits, the event append fails. ----
    let b_before = heartbeat_lines(&agent_log);
    std::fs::remove_file(&log_path).unwrap();
    std::fs::create_dir(&log_path).unwrap();
    let b_err = sup.pause(&registry, "pz").unwrap_err();
    assert!(
        matches!(&b_err, EngineError::Log { .. }),
        "the append failure must surface as a typed Log error, got {b_err:?}"
    );
    // Persist-first: the durable state LEADS — the ledger reads `paused`
    // even though the append (the event record) failed.
    assert_eq!(
        state_of(&registry, "pz"),
        LifecycleState::Paused,
        "persist-first: the committed transition must be visible in the ledger"
    );
    std::thread::sleep(Duration::from_millis(400));
    let b_after = heartbeat_lines(&agent_log);
    assert!(
            b_after > b_before,
            "a pause whose persist committed but errored must NOT have signalled SIGSTOP: heartbeat {b_before} -> {b_after}"
        );
    std::fs::remove_dir(&log_path).unwrap();

    // Realign: Leg B's persist committed `paused`, so a real resume (the
    // remediation the AI-9 diagnostic names) brings the ledger back to
    // `running` — the SIGCONT is a harmless no-op on the still-running
    // process.
    sup.resume(&registry, "pz").unwrap();
    assert_eq!(state_of(&registry, "pz"), LifecycleState::Running);

    // ---- The genuine suspension (so the resume leg proves SIGCONT). ----
    sup.pause(&registry, "pz").unwrap();
    assert_eq!(state_of(&registry, "pz"), LifecycleState::Paused);
    std::thread::sleep(Duration::from_millis(200));
    let frozen_before = heartbeat_lines(&agent_log);
    std::thread::sleep(Duration::from_millis(300));
    // Windows: pause there is an honest cooperative NO-OP (AD-4 — no
    // guaranteed whole-process suspend exists from std), so there is no
    // real suspension for a frozen-heartbeat probe to observe; the probe
    // is the Unix guarantee (same data-driven skip as tests/pause.rs).
    if OsId::current() != OsId::Windows {
        assert_eq!(
            heartbeat_lines(&agent_log),
            frozen_before,
            "the probe: a successful guaranteed pause really suspends (heartbeat frozen)"
        );
    }

    // ---- Resume leg: persist commits, append fails, NO SIGCONT. ----
    std::fs::remove_file(&log_path).unwrap();
    std::fs::create_dir(&log_path).unwrap();
    let r_err = sup.resume(&registry, "pz").unwrap_err();
    assert!(
        matches!(&r_err, EngineError::Log { .. }),
        "the resume append failure must surface as a typed Log error, got {r_err:?}"
    );
    assert_eq!(
        state_of(&registry, "pz"),
        LifecycleState::Running,
        "persist-first resume: the committed paused-to-running transition must be visible"
    );
    std::thread::sleep(Duration::from_millis(300));
    // The frozen-heartbeat half is Unix-only (Windows has no real
    // suspension to stay in — see the AD-4 note above); the LEDGER half
    // (persist committed `paused → running`, the Log error surfaced)
    // above is the cross-platform claim.
    if OsId::current() != OsId::Windows {
        assert_eq!(
                heartbeat_lines(&agent_log),
                frozen_before,
                "a resume whose persist committed but errored must NOT have signalled SIGCONT: the process stays suspended"
            );
    }
    std::fs::remove_dir(&log_path).unwrap();

    // Teardown: the process is SIGSTOPped (SIGTERM would only pend), so the
    // stop's forced escalation is the honest way down.
    sup.stop(&registry, "pz", Some(Duration::from_millis(600)))
        .unwrap();
}

/// Register a `fake_agent`-backed instance whose manifest declares GUARANTEED
/// pause for the current OS. Returns the (state dir, manifest dir, registry).
fn setup_pause_guaranteed(
    name: &str,
    args: &[&str],
) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    write_pause_guaranteed_manifest(manifest.path(), name, args);
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry
        .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    (state, manifest, registry)
}

/// Append usage sentinel lines (`(sequence, input, output)` triples) to the
/// instance's agent-output log — the drain's input.
fn append_usage_lines(path: &Path, lines: &[(u64, u64, u64)]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for (seq, input, output) in lines {
        f.write_all(
                format!(
                    "KTESIO_USAGE {{\"sequence\":{seq},\"input_tokens\":{input},\"output_tokens\":{output}}}\n"
                )
                .as_bytes(),
            )
            .unwrap();
    }
}

/// The (row count, summed input tokens) of `name`'s committed ledger rows,
/// via a direct connection to the state DB.
fn ledger_totals(state: &Path, name: &str) -> (i64, i64) {
    let conn = rusqlite::Connection::open(state.join("state.db")).unwrap();
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0) FROM usage_events e \
             JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1",
        [name],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[test]
fn a_failed_ledger_insert_parks_the_cursor_and_retries_exactly_once() {
    // AI-41 (billing honesty), loop-1 repaired + extended: a usage event
    // whose INSERT fails must NOT have the drain cursor advanced past it
    // (the pre-fix code advanced FIRST, silently dropping the event). Four
    // proofs in one flow:
    //   (1) a whole-table fault parks the cursor and the failure diagnostic
    //       reaches the story-10-2 sink;
    //   (2) the repair restores the FULL schema — the table AND every index
    //       including the UNIQUE(instance_id, run_id, sequence) dedup index
    //       (the loop-0 restore lost the dedup invariant);
    //   (3) the retry commits each event EXACTLY once;
    //   (4) a PARTIAL failure (a trigger fails only the SECOND insert of a
    //       block) parks the cursor, and the re-drift proves the dedup key:
    //       the already-committed first event comes back as `DuplicateReplay`
    //       (no double-count) while the failed second commits.
    let (state, _manifest, registry) = setup_fake("ledger", &["--linger-ms", "600000"]);
    let name = InstanceName::new("ledger").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "ledger").unwrap();
    let sink = install_capture_sink(&mut sup);
    let log = registry.agent_output_log_path(&name);
    let db = state.path().join("state.db");

    // (1) Whole-table fault: the drain parks, the diagnostic is audible.
    append_usage_lines(&log, &[(0, 10, 20), (1, 11, 22)]);
    let cursor_before = sup.running.get(&name).unwrap().usage_cursor;
    let conn = rusqlite::Connection::open(&db).unwrap();
    // The FULL schema of `usage_events` — the table AND every index (the
    // dedup index is its own sqlite_master row), tables first.
    let mut schema: Vec<(String, String)> = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT type, sql FROM sqlite_master \
                 WHERE tbl_name = 'usage_events' AND sql IS NOT NULL \
                 ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap();
    for row in rows {
        schema.push(row.unwrap());
    }
    drop(stmt);
    assert!(
        schema
            .iter()
            .any(|(t, sql)| t == "index" && sql.contains("CREATE UNIQUE INDEX")),
        "the fixture premise: a UNIQUE dedup index exists on usage_events"
    );
    conn.execute("DROP TABLE usage_events", []).unwrap();
    drop(conn);

    sup.drain_usage_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        sup.running.get(&name).unwrap().usage_cursor,
        cursor_before,
        "a failed INSERT must park the drain cursor (AI-41: no silent drop)"
    );
    assert!(
        sink_text(&sink).contains("could not be committed to the Usage Ledger"),
        "the ingest-failure diagnostic must reach the diagnostic sink"
    );

    // (2) Restore the FULL schema — table first, then every index.
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (_kind, sql) in &schema {
        conn.execute(sql, []).unwrap();
    }
    let unique_indexes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' \
                 AND tbl_name = 'usage_events' AND sql LIKE 'CREATE UNIQUE INDEX%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);
    assert!(
        unique_indexes >= 1,
        "the repair must restore the UNIQUE dedup index, not only the table"
    );

    // (3) The retry commits both events, each exactly once.
    sup.drain_usage_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        ledger_totals(state.path(), "ledger"),
        (2, 21),
        "the retried events must commit exactly once (10 + 11 input tokens)"
    );
    let log_len = std::fs::metadata(&log).unwrap().len();
    assert_eq!(
        sup.running.get(&name).unwrap().usage_cursor,
        log_len,
        "the cursor advances past the block only once every event committed"
    );

    // (4) PARTIAL failure: the FIRST insert of a block commits, the SECOND
    // fails (a trigger RAISEs once the table already holds 3 rows). The
    // cursor parks; after the fault is repaired, the re-drift must classify
    // the committed event as a DuplicateReplay (the UNIQUE dedup key — no
    // double-count) while the failed one finally commits.
    append_usage_lines(&log, &[(2, 1, 2), (3, 3, 4)]);
    let parked_at = sup.running.get(&name).unwrap().usage_cursor;
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "CREATE TRIGGER ai41_fail_second_insert BEFORE INSERT ON usage_events \
             WHEN (SELECT COUNT(*) FROM usage_events) >= 3 \
             BEGIN SELECT RAISE(ABORT, 'injected second-insert fault'); END;",
        [],
    )
    .unwrap();
    drop(conn);
    sup.drain_usage_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        ledger_totals(state.path(), "ledger"),
        (3, 22),
        "the first event of the block commits (10+11+1), the second fails"
    );
    assert_eq!(
        sup.running.get(&name).unwrap().usage_cursor,
        parked_at,
        "the partial failure must park the cursor before the block (re-drift both)"
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DROP TRIGGER ai41_fail_second_insert", [])
        .unwrap();
    drop(conn);
    sup.drain_usage_for(&registry, &name, DrainMode::MidRun);
    // If the dedup index were missing, the re-drifted sequence-2 line would
    // insert AGAIN (4 rows / 23 input) — (4, 25) proves DuplicateReplay.
    assert_eq!(
        ledger_totals(state.path(), "ledger"),
        (4, 25),
        "the re-drifted committed event must be a DuplicateReplay (no double-count); \
             the failed event commits on retry"
    );
    assert_eq!(
        sup.running.get(&name).unwrap().usage_cursor,
        std::fs::metadata(&log).unwrap().len(),
        "the cursor advances only once every event is durable"
    );
    // Teardown.
    sup.stop(&registry, "ledger", Some(Duration::from_millis(200)))
        .unwrap();
}

// ---- Story 12-4: the OBSERVED drain's park + bounded retry + loud skip ----

/// Give the supervisor's (already-started, self-reported) instance a REAL
/// observed listener + source — the 12-4 drain tests push `(input, output)`
/// pairs into the listener's queue directly (no model traffic needed) and
/// drive `drain_observed_for` synchronously. The listener points at a dead
/// upstream (nothing ever forwards); its accept-loop task lives on the
/// returned runtime (kept alive by the caller for the test's duration).
fn attach_observed_channel(
    sup: &mut Supervisor,
    registry: &Registry,
    name: &str,
) -> (
    tokio::runtime::Runtime,
    crate::metering::listener::ObservedQueue,
) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test runtime for the observed listener");
    let dead_upstream = {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let listener = ObservedListener::start(rt.handle(), dead_upstream).expect("listener start");
    let queue = listener.queue();
    let inst = InstanceName::new(name).unwrap();
    let s = sup.running.get_mut(&inst).expect("supervised entry");
    s.observed_listener = Some(listener);
    s.observed_source = Some(crate::ports::ObservedUsageSource::new());
    s.metering_source = "engine-observed".to_string();
    // Silence the unused-registry lint; the registry read keeps the signature
    // symmetric with the drain call sites.
    let _ = registry;
    (rt, queue)
}

/// The FULL `usage_events` schema (table + every index, tables first) read
/// from the live DB — the repair fixture the fault-injection drops and
/// restores (the AI-41 park test's exact technique, shared).
fn usage_events_schema(db: &Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT type, sql FROM sqlite_master \
                 WHERE tbl_name = 'usage_events' AND sql IS NOT NULL \
                 ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap();
    let schema: Vec<(String, String)> = rows.map(|r| r.unwrap()).collect();
    drop(stmt);
    schema
}

/// The minted `sequence` ordinals of `name`'s committed ledger rows —
/// proves the retried events kept their ORIGINAL sequences (never re-minted).
fn ledger_sequences(state: &Path, name: &str) -> Vec<i64> {
    let conn = rusqlite::Connection::open(state.join("state.db")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.sequence FROM usage_events e \
                 JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1 \
                 ORDER BY e.sequence",
        )
        .unwrap();
    let rows = stmt.query_map([name], |r| r.get::<_, i64>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn an_observed_drain_parks_and_retries_the_same_minted_events() {
    // Story 12-4 (the observed AI-41): a store failure MIDRUN parks the
    // minted events; the retry commits the EXACT same events — the same
    // minted `sequence` ordinals (never re-minted, the dedup-key stability
    // the design pins) — and the park clears once everything is durable.
    let (state, _manifest, registry) = setup_fake("obspark", &["--linger-ms", "600000"]);
    let name = InstanceName::new("obspark").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "obspark").unwrap();
    let sink = install_capture_sink(&mut sup);
    let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obspark");
    let db = state.path().join("state.db");

    // Two observed completions; then the store dies.
    queue.lock().unwrap().push_back((10, 20, 0));
    queue.lock().unwrap().push_back((11, 22, 0));
    let schema = usage_events_schema(&db);
    assert!(
        schema
            .iter()
            .any(|(t, sql)| t == "index" && sql.contains("CREATE UNIQUE INDEX")),
        "fixture premise: a UNIQUE dedup index exists on usage_events"
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DROP TABLE usage_events", []).unwrap();
    drop(conn);

    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    {
        let s = sup.running.get(&name).unwrap();
        let (pending, attempts) = s.observed_park.as_ref().expect("the drain must park");
        assert_eq!(*attempts, 1, "first failure: attempt streak 1");
        assert_eq!(
            pending.front_sequence, 0,
            "the front event's minted sequence"
        );
        assert_eq!(
            pending.events.len(),
            2,
            "both minted events park (the queue has no byte cursor)"
        );
    }
    assert!(
        sink_text(&sink).contains("parked for retry"),
        "the failure diagnostic must reach the diagnostic sink"
    );

    // Repair (FULL schema incl. the dedup index) and retry.
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (_kind, sql) in &schema {
        conn.execute(sql, []).unwrap();
    }
    drop(conn);
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        ledger_totals(state.path(), "obspark"),
        (2, 21),
        "the retried events commit exactly once"
    );
    assert_eq!(
        ledger_sequences(state.path(), "obspark"),
        vec![0, 1],
        "the retry kept the ORIGINAL minted sequences (never re-minted)"
    );
    let s = sup.running.get(&name).unwrap();
    assert!(
        s.observed_park.is_none(),
        "the park clears once every event is durable"
    );
    sup.stop(&registry, "obspark", Some(Duration::from_millis(200)))
        .unwrap();
}

#[test]
fn an_observed_partial_failure_parks_only_the_uncommitted_tail() {
    // Break-on-first-error: the FIRST event of a pass commits, the SECOND
    // fails (a trigger RAISEs from the second row on) — only the FAILED
    // event parks (discrete events need no block re-drift), and after the
    // repair the retry adds exactly the missing one (no double-count).
    let (state, _manifest, registry) = setup_fake("obspart", &["--linger-ms", "600000"]);
    let name = InstanceName::new("obspart").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "obspart").unwrap();
    let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obspart");
    let db = state.path().join("state.db");

    queue.lock().unwrap().push_back((10, 20, 0));
    queue.lock().unwrap().push_back((11, 22, 0));
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "CREATE TRIGGER obs_fail_second BEFORE INSERT ON usage_events \
             WHEN (SELECT COUNT(*) FROM usage_events) >= 1 \
             BEGIN SELECT RAISE(ABORT, 'injected second-insert fault'); END;",
        [],
    )
    .unwrap();
    drop(conn);

    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        ledger_totals(state.path(), "obspart"),
        (1, 10),
        "the first event commits, the second fails"
    );
    {
        let s = sup.running.get(&name).unwrap();
        let (pending, attempts) = s.observed_park.as_ref().expect("the failure parks");
        assert_eq!(
            pending.events.len(),
            1,
            "only the UNCOMMITTED event parks — the committed one is never retried"
        );
        assert_eq!(pending.front_sequence, 1, "the front is the second event");
        assert_eq!(*attempts, 1);
    }

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DROP TRIGGER obs_fail_second", []).unwrap();
    drop(conn);
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        ledger_totals(state.path(), "obspart"),
        (2, 21),
        "the retry adds exactly the missing event (no double-count)"
    );
    let s = sup.running.get(&name).unwrap();
    assert!(s.observed_park.is_none(), "the park clears");
    sup.stop(&registry, "obspart", Some(Duration::from_millis(200)))
        .unwrap();
}

#[test]
fn an_observed_poisoned_event_is_skipped_loudly_after_the_bound() {
    // The bounded park: an event the store PERMANENTLY rejects (a trigger
    // keyed on its token count) parks 1→2→3 times, then is SKIPPED with a
    // loud diagnostic while the events behind it keep counting — an
    // announced loss, never a wedged drain, never a silent drop.
    let (state, _manifest, registry) = setup_fake("obsskip", &["--linger-ms", "600000"]);
    let name = InstanceName::new("obsskip").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "obsskip").unwrap();
    let sink = install_capture_sink(&mut sup);
    let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obsskip");
    let db = state.path().join("state.db");

    queue.lock().unwrap().push_back((42, 1, 0)); // the poisoned event
    queue.lock().unwrap().push_back((7, 2, 0));
    queue.lock().unwrap().push_back((8, 3, 0));
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "CREATE TRIGGER obs_poison BEFORE INSERT ON usage_events \
             WHEN NEW.input_tokens = 42 \
             BEGIN SELECT RAISE(ABORT, 'injected permanent fault'); END;",
        [],
    )
    .unwrap();
    drop(conn);

    // Passes 1 and 2: the drain parks at the poisoned front.
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    {
        let s = sup.running.get(&name).unwrap();
        let (pending, attempts) = s.observed_park.as_ref().unwrap();
        assert_eq!(
            *attempts, 2,
            "two consecutive failed passes at the same front"
        );
        assert_eq!(pending.front_sequence, 0);
        assert_eq!(
            pending.events.len(),
            3,
            "nothing committed, nothing skipped yet"
        );
    }
    // Pass 3 hits the bound: the poisoned front is SKIPPED loudly; the two
    // healthy events park behind it with a FRESH streak (a new front).
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert!(
        sink_text(&sink).contains("SKIPPED (not counted)"),
        "the skip must be announced loudly"
    );
    assert!(
        sink_text(&sink).contains("sequence 0"),
        "the skip names the poisoned event's minted sequence"
    );
    {
        let s = sup.running.get(&name).unwrap();
        let (pending, attempts) = s.observed_park.as_ref().unwrap();
        assert_eq!(
            pending.front_sequence, 1,
            "the new front is the second event"
        );
        assert_eq!(pending.events.len(), 2, "the healthy events keep counting");
        assert_eq!(*attempts, 1, "a different front resets the streak");
    }
    assert_eq!(
        ledger_totals(state.path(), "obsskip"),
        (0, 0),
        "nothing committed yet (the healthy events are parked, not dropped)"
    );

    // Remove the fault: the healthy events commit on the next pass; the
    // skipped one is NOT resurrected (an announced loss stays lost).
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DROP TRIGGER obs_poison", []).unwrap();
    drop(conn);
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        ledger_totals(state.path(), "obsskip"),
        (2, 15),
        "the healthy events landed; the skipped event's 42 tokens are gone (announced)"
    );
    let s = sup.running.get(&name).unwrap();
    assert!(s.observed_park.is_none());
    sup.stop(&registry, "obsskip", Some(Duration::from_millis(200)))
        .unwrap();
}

#[test]
fn an_oversized_observed_park_drops_the_oldest_events_loudly() {
    // The 12-4 AMENDMENT cap: a store outage keeps minting events every
    // tick, so the pending park buffer must be BOUNDED
    // (OBSERVED_PARK_MAX_EVENTS) — never unbounded engine memory. When the
    // cap bites, the OLDEST parked events are dropped and the loss is
    // announced LOUDLY (count + sequence range + remediation) — a capped
    // loss, never a silent truncation; the newest events keep their park.
    let (state, _manifest, registry) = setup_fake("obscap", &["--linger-ms", "600000"]);
    let name = InstanceName::new("obscap").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "obscap").unwrap();
    let sink = install_capture_sink(&mut sup);
    let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obscap");
    let db = state.path().join("state.db");

    // MORE events than the cap, all minted in one drain while the store
    // is dead: the buffer can hold only OBSERVED_PARK_MAX_EVENTS of them.
    let total = OBSERVED_PARK_MAX_EVENTS + 2;
    for i in 0..total {
        queue.lock().unwrap().push_back((1, i as u64, 0));
    }
    let schema = usage_events_schema(&db);
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DROP TABLE usage_events", []).unwrap();
    drop(conn);
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);

    // The park holds EXACTLY the cap — the two OLDEST events (sequences
    // 0 and 1) were dropped, loudly.
    {
        let s = sup.running.get(&name).unwrap();
        let (pending, attempts) = s.observed_park.as_ref().expect("the drain still parks");
        assert_eq!(
            pending.events.len(),
            OBSERVED_PARK_MAX_EVENTS,
            "the park buffer is capped, never unbounded"
        );
        assert_eq!(
            pending.front_sequence, 2,
            "the two oldest minted events (0, 1) were the dropped ones"
        );
        assert_eq!(*attempts, 1, "a first failed pass at the (new) front");
    }
    let text = sink_text(&sink);
    assert!(
        text.contains("2 parked observed usage event(s)"),
        "the overflow must be announced with the dropped COUNT: {text}"
    );
    assert!(
        text.contains("sequences 0..1"),
        "the overflow diagnostic names the dropped sequence range: {text}"
    );
    assert!(
        text.contains("NOT counted"),
        "the dropped events are honestly not-counted, never silently held: {text}"
    );

    // Repair: the capped survivors commit with their ORIGINAL minted
    // sequences (the cap never re-mints), the dropped two stay lost.
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (_kind, sql) in &schema {
        conn.execute(sql, []).unwrap();
    }
    drop(conn);
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        ledger_sequences(state.path(), "obscap").len(),
        OBSERVED_PARK_MAX_EVENTS,
        "exactly the capped survivors landed"
    );
    assert_eq!(
        ledger_sequences(state.path(), "obscap")[0],
        2,
        "the survivors kept their original sequences (the dropped two are gone)"
    );
    let s = sup.running.get(&name).unwrap();
    assert!(s.observed_park.is_none());
    sup.stop(&registry, "obscap", Some(Duration::from_millis(200)))
        .unwrap();
}

#[test]
fn a_terminal_observed_drain_failure_announces_the_loss_without_a_retry_claim() {
    // The TERMINAL arm: a commit failure on the stop/reap drain is LOST —
    // announced explicitly (with the why and no retry claim), never a silent
    // drop and never a false "will retry". Any buffer a PRIOR MidRun pass
    // parked is dropped with the instance (the terminal pass makes no new
    // park and rescues nothing once the drain itself failed).
    let (state, _manifest, registry) = setup_fake("obslost", &["--linger-ms", "600000"]);
    let name = InstanceName::new("obslost").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "obslost").unwrap();
    let sink = install_capture_sink(&mut sup);
    let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obslost");
    let db = state.path().join("state.db");

    // (1) Store UP: event A commits immediately.
    queue.lock().unwrap().push_back((30, 40, 0));
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(ledger_totals(state.path(), "obslost"), (1, 30));

    // (2) Store DIES: event B parks on the MidRun pass.
    queue.lock().unwrap().push_back((3, 4, 0));
    let schema = usage_events_schema(&db);
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DROP TABLE usage_events", []).unwrap();
    drop(conn);
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert!(sup.running.get(&name).unwrap().observed_park.is_some());

    // (3) The TERMINAL drain (store still dead): B's failure is announced
    // as LOST — not retried, not a bounded skip — and B does NOT survive
    // into a new park. The 12-4 AMENDMENT: NO diagnostic on this path may
    // claim "parked for retry" — the shared ingest-failure text is
    // mode-aware, and the absence is asserted, not assumed (a terminal
    // diagnostic promising a retry would contradict the LOST notice). The
    // assertion scopes to the TERMINAL pass's own lines: the earlier
    // MidRun pass's "parked for retry" line is honest history (that drain
    // really did park), so only the delta may not contain the claim.
    let before_terminal = sink_text(&sink).len();
    sup.drain_observed_for(&registry, &name, DrainMode::Terminal);
    let text = sink_text(&sink)[before_terminal..].to_string();
    assert!(
        text.contains("are LOST"),
        "the terminal loss must be announced: {text}"
    );
    assert!(
        text.contains("cannot be retried"),
        "the loss notice must not claim a retry: {text}"
    );
    assert!(
        !text.contains("parked for retry"),
        "NO terminal diagnostic may claim the parked event will be retried \
             (the 12-4 AMENDMENT mode-aware text): {text}"
    );
    assert!(
        !text.contains("SKIPPED"),
        "the terminal loss is not a bounded skip: {text}"
    );
    assert!(
        sup.running.get(&name).unwrap().observed_park.is_none(),
        "the parked buffer is dropped with the instance (no phantom retry state)"
    );

    // (4) Repair: a later pass finds nothing to re-add — the lost event
    // stays lost (announced). (The DROP TABLE fixture also wiped A's row,
    // so the repaired ledger starts empty; the point is that B is never
    // resurrected.)
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (_kind, sql) in &schema {
        conn.execute(sql, []).unwrap();
    }
    drop(conn);
    sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
    assert_eq!(
        ledger_totals(state.path(), "obslost"),
        (0, 0),
        "B was announced lost and is never resurrected by a later pass"
    );
    sup.stop(&registry, "obslost", Some(Duration::from_millis(200)))
        .unwrap();
}

#[test]
fn a_post_commit_signal_failure_emits_the_divergence_breadcrumb_and_returns_err() {
    // AI-9 (loop 2) — the post-commit signal-failure branch, executed END TO
    // END through the cfg(test) signal-fault seam: persist-first commits the
    // `running → paused` transition, THEN the seam makes `signal_backend`
    // fail with an injected error, so the ledger (now `paused`) and the live
    // process DIVERGE. The branch must (a) still surface the error, (b) emit
    // the divergence breadcrumb — instance + committed state + signal error
    // + the real recovery — to the story-10-2 sink, and (c) leave the
    // committed row `paused` (the durable state leads). A regression to a
    // silent swallow, or a breadcrumb that loses any quarter of the story,
    // fails here. The budget-driven leg proves the loop-2 remediation split:
    // a breach-driven pause must recommend `stop` ONLY (the latch is spent —
    // `resume` would leave an over-budget run unenforced), never `resume`.
    let (state, _manifest, registry) =
        setup_pause_guaranteed("sigfault", &["--linger-ms", "600000"]);
    let name = InstanceName::new("sigfault").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "sigfault").unwrap();
    let sink = install_capture_sink(&mut sup);
    sup.arm_signal_fault(name.clone());

    // (a) The command fails with the backend error ...
    let err = sup.pause(&registry, "sigfault").unwrap_err();
    assert!(
        matches!(&err, EngineError::Backend { .. }),
        "the post-commit signal failure must surface as the Backend error, got {err:?}"
    );
    // (b) ... and the divergence breadcrumb reaches the sink: the committed
    // state, the divergence claim, the recovery, and the injected why.
    let text = sink_text(&sink);
    assert!(
        text.contains("says 'paused'"),
        "the breadcrumb must name the COMMITTED state: {text}"
    );
    assert!(
        text.contains("may diverge"),
        "the breadcrumb must state the divergence: {text}"
    );
    assert!(
        text.contains("kt agent resume sigfault"),
        "a plain-command pause remediation must offer `resume` to realign: {text}"
    );
    assert!(
        text.contains("injected cfg(test) signal fault"),
        "the breadcrumb must carry the signal error's text: {text}"
    );
    // (c) The transition COMMITTED before the signal failed: the row reads
    // `paused` even though the command errored.
    assert_eq!(
        state_of(&registry, "sigfault"),
        LifecycleState::Paused,
        "persist-first: the committed transition must survive the signal failure"
    );

    // Budget-driven leg (AI-9 loop 2): with the cause_override
    // BudgetExceeded the per-Run breach latch is ALREADY spent — the
    // remediation must recommend `stop` only, never `resume`.
    {
        let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        let n = conn
            .execute(
                "UPDATE agent_instances SET state = 'running' WHERE name = 'sigfault'",
                [],
            )
            .unwrap();
        assert_eq!(n, 1);
    }
    let breach = TransitionCause::budget_exceeded(BreachScope::Cumulative, 15, 30);
    let text_before = sink_text(&sink).len();
    sup.pause_with_cause(&registry, &name, breach).unwrap_err();
    let tail = &sink_text(&sink)[text_before..];
    assert!(
        tail.contains("the pause was budget-driven"),
        "the breach-driven remediation must say WHY stop is the only advice: {tail}"
    );
    assert!(
        tail.contains("kt agent stop sigfault"),
        "the breach-driven remediation must lead with `stop`: {tail}"
    );
    assert!(
        !tail.contains("kt agent resume"),
        "a breach-driven pause must NEVER advise `resume` (the latch is spent): {tail}"
    );

    // Teardown: the process was never suspended (both signals failed), so a
    // normal stop lands (stop does not consult `signal_backend`).
    sup.stop(&registry, "sigfault", Some(Duration::from_millis(600)))
        .unwrap();
}

#[test]
fn a_budget_driven_pause_with_no_in_memory_handle_wraps_the_override_in_the_qualifier() {
    // AI-8 (loop 2) — the override honesty wrap: a BUDGET-DRIVEN pause that
    // reaches `suspend_or_resume` with NO in-memory handle held (the row
    // says `running`; e.g. enforcement re-evaluating a run this engine
    // session does not hold) must record `pause-best-effort` whose detail
    // WRAPS the override ("the requested override was: ...") — never the
    // bare `budget-exceeded` cause (which would read as a performed
    // suspension of a process nothing signalled) and never a plain command.
    let (_state, _manifest, registry) =
        setup_pause_guaranteed("ovrwrap", &["--linger-ms", "600000"]);
    let name = InstanceName::new("ovrwrap").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "ovrwrap").unwrap();
    // Drop the ONLY held handle (its kill-on-drop ends the process; the row
    // still says `running`) — the AI-8 premise: a pause with nothing held
    // to signal.
    sup.running.remove(&name);
    let cause = TransitionCause::budget_exceeded(BreachScope::Cumulative, 15, 30);
    sup.pause_with_cause(&registry, &name, cause).unwrap();
    assert_eq!(state_of(&registry, "ovrwrap"), LifecycleState::Paused);
    let events = Supervisor::read_events(&registry, "ovrwrap").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Paused);
    let cause_json = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause_json.contains("\"kind\":\"pause-best-effort\""),
        "a budget pause with no in-memory handle must record the best-effort \
             qualifier, not the bare override: {cause_json}"
    );
    assert!(
        !cause_json.contains("\"kind\":\"budget-exceeded\""),
        "the override must be WRAPPED, never recorded as a performed budget \
             suspension: {cause_json}"
    );
    assert!(
        cause_json.contains("no live process handle"),
        "the qualifier must name the missing handle (the honest why): {cause_json}"
    );
    assert!(
        cause_json.contains("the requested override was:")
            && cause_json.contains("breach: cumulative"),
        "the qualifier's detail must wrap the requested BudgetExceeded override: \
             {cause_json}"
    );
    // No teardown `stop`: no handle is held (removed above; its Drop already
    // ended the process) — a stop here would be a handle-less no-op
    // transition, not a real teardown.
}

#[test]
fn a_failed_terminal_drain_announces_the_lost_batch_to_the_sink() {
    // AI-41 (loop 2) — the Terminal-drain loss notice, EXECUTED: on the
    // TERMINAL drain there IS no next pass (the handle is being removed
    // right after), so a store failure there must announce the batch LOST —
    // with the count — to the story-10-2 sink, never reuse the MidRun
    // park-and-retry story (a lie exactly where loss is likeliest). Mirrors
    // the MidRun park test's full-schema sabotage below.
    let (state, _manifest, registry) = setup_fake("tdrain", &["--linger-ms", "600000"]);
    let name = InstanceName::new("tdrain").unwrap();
    let mut sup = Supervisor::with_backoff(fast_backoff());
    sup.start(&registry, "tdrain").unwrap();
    let sink = install_capture_sink(&mut sup);
    let log = registry.agent_output_log_path(&name);
    append_usage_lines(&log, &[(0, 10, 20), (1, 11, 22)]);
    let cursor_before = sup.running.get(&name).unwrap().usage_cursor;
    let db = state.path().join("state.db");

    // Sabotage the ledger (drop the table; the full schema is restored
    // below so the stop's own terminal drain sees a real ledger).
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mut schema: Vec<String> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT sql FROM sqlite_master \
                     WHERE tbl_name = 'usage_events' AND sql IS NOT NULL \
                     ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END",
            )
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        for row in rows {
            schema.push(row.unwrap());
        }
    }
    conn.execute("DROP TABLE usage_events", []).unwrap();
    drop(conn);

    sup.drain_usage_for(&registry, &name, DrainMode::Terminal);
    let text = sink_text(&sink);
    assert!(
        text.contains("terminal drain (the process is dead"),
        "the terminal-drain loss notice must reach the sink: {text}"
    );
    assert!(
        text.contains("cannot be retried and is lost"),
        "the notice must say the batch is lost, not parked for retry: {text}"
    );
    assert!(
        text.contains("2 usage event(s)"),
        "the notice must name the lost COUNT: {text}"
    );
    assert_eq!(
        sup.running.get(&name).unwrap().usage_cursor,
        cursor_before,
        "a terminal drain that commits nothing must not advance the cursor"
    );

    // Repair the store, then tear down (the stop's own terminal drain now
    // sees a real ledger).
    let conn = rusqlite::Connection::open(&db).unwrap();
    for sql in &schema {
        conn.execute(sql, []).unwrap();
    }
    drop(conn);
    sup.stop(&registry, "tdrain", Some(Duration::from_millis(200)))
        .unwrap();
}

// ---- Story 11-5 (AI-71): the read/observation helpers' error surfaces ----
//
// These pin the documented contracts the read helpers promise but that no
// test exercised: a log that cannot be READ (not merely absent — absent is
// an honest empty) is a TYPED error naming the instance + path, never a
// silent empty vec; an invalid name is the typed InvalidName; and the
// blank-line/missing-generation tolerances are the only silent paths. A
// directory where the log file must be makes `read_to_string` fail on
// every OS (no OS-cfg — portable setup).

#[test]
fn read_events_rejects_an_invalid_name_with_the_typed_error() {
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let err = Supervisor::read_events(&registry, "Bad Name").unwrap_err();
    assert!(
        matches!(err, EngineError::InvalidName { ref name, .. } if name == "Bad Name"),
        "expected InvalidName, got {err:?}"
    );
}

#[test]
fn read_events_surfaces_an_unreadable_instance_log_as_a_typed_log_error() {
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry.register("evt", "mock").unwrap();
    let name = InstanceName::new("evt").unwrap();
    let path = registry.instance_log_path(&name);
    std::fs::create_dir_all(&path).unwrap();
    let err = Supervisor::read_events(&registry, "evt").unwrap_err();
    match &err {
        EngineError::Log {
            name: n,
            path: p,
            detail,
        } => {
            assert_eq!(n, "evt");
            assert_eq!(p, &path.to_string_lossy().into_owned());
            assert!(!detail.is_empty(), "the error must name the read failure");
        }
        other => panic!("expected EngineError::Log, got {other:?}"),
    }
}

#[test]
fn read_breach_events_rejects_an_invalid_name_with_the_typed_error() {
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    let err = Supervisor::read_breach_events(&registry, "Bad Name").unwrap_err();
    assert!(
        matches!(err, EngineError::InvalidName { ref name, .. } if name == "Bad Name"),
        "expected InvalidName, got {err:?}"
    );
}

#[test]
fn read_breach_events_surfaces_an_unreadable_breach_log_as_a_typed_log_error() {
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry.register("brk", "mock").unwrap();
    let name = InstanceName::new("brk").unwrap();
    let path = registry.instance_breach_log_path(&name);
    std::fs::create_dir_all(&path).unwrap();
    let err = Supervisor::read_breach_events(&registry, "brk").unwrap_err();
    match &err {
        EngineError::Log {
            name: n,
            path: p,
            detail,
        } => {
            assert_eq!(n, "brk");
            assert_eq!(p, &path.to_string_lossy().into_owned());
            assert!(!detail.is_empty(), "the error must name the read failure");
        }
        other => panic!("expected EngineError::Log, got {other:?}"),
    }
}

#[test]
fn read_agent_log_since_surfaces_an_unreadable_attributed_log_as_a_typed_error() {
    // The NOT-FOUND arm is the documented empty tail; every OTHER read
    // failure is a typed EngineError::Log (the live-tail reader must
    // distinguish "nothing yet" from "cannot read").
    let state = tempfile::tempdir().unwrap();
    let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
    registry.register("alr", "mock").unwrap();
    let name = InstanceName::new("alr").unwrap();
    let path = registry.attributed_output_log_path(&name);
    std::fs::create_dir_all(&path).unwrap();
    let expected_path = path.to_string_lossy().into_owned();
    let err = Supervisor::read_agent_log_since(&registry, "alr", 0).unwrap_err();
    assert!(
        matches!(err, EngineError::Log { ref name, ref path, .. } if name == "alr"
                && path == &expected_path),
        "expected a Log error naming the instance + path, got {err:?}"
    );
}

#[test]
fn log_line_helpers_skip_blank_lines_and_surface_unreadable_generations() {
    // parse_log_lines: blank lines are skipped (the documented AC-G
    // convention) — an all-blank input is Ok with nothing parsed.
    let mut out = Vec::new();
    parse_log_lines("\n   \n", &mut out).expect("blank lines are skipped, not errors");
    assert!(out.is_empty());
    // read_log_lines_from: a missing generation is a silent no-op (the
    // documented oldest-to-newest probe convention); an UNREADABLE one is
    // an error, mirroring read_events_from.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("gen-missing.log");
    let mut out = Vec::new();
    read_log_lines_from(&missing, &mut out).expect("missing generation is a no-op");
    assert!(out.is_empty());
    let obstructed = dir.path().join("gen-0.log");
    std::fs::create_dir(&obstructed).unwrap();
    let mut out = Vec::new();
    let err = read_log_lines_from(&obstructed, &mut out).unwrap_err();
    assert!(!err.is_empty(), "an unreadable generation must be an error");
}

#[test]
fn default_constructs_the_standard_supervisor() {
    // Default delegates to new() (the standard schedule); trivially
    // non-panicking, pinned so the impl block stays honest.
    let _ = Supervisor::default();
}
