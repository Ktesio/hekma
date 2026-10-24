//! The adapter gallery's validation lane: every manifest shipped under
//! `docs/adapters/manifests/<agent>/` is validated by the FULL mock
//! conformance suite with its bare-word `exec` PATH-shadowed by the bundled
//! `fake_agent` helper — the `hermes_shim` precedent
//! (`crates/hekma-engine/tests/hermes.rs`). CI never launches a real agent:
//! the published execs are bare program names (the Adapter Contract's own
//! PATH resolution rule, `docs/manifest.md`), and this test resolves each
//! one to a fresh COPY of the fake helper named exactly as the manifest's
//! exec word, prepended to `PATH` for the run. A user copying the manifest
//! resolves the same bare word to the real binary on their machine.
//!
//! Per manifest, the test also asserts the gallery page exists
//! (`docs/adapters/<agent>.md`) — the drift guard: a manifest without its
//! page fails the build with a named reason, and a page whose manifest does
//! not conform fails with the section + reason printed verbatim.
//!
//! Serialization: `PATH` is process-global, so the whole lane is ONE
//! `#[test]` fn looping over the manifest dirs — under `cargo nextest` each
//! test is its own process, and under plain `cargo test` no other test in
//! this file touches `PATH`. The guard restores the inherited value
//! unconditionally (happy path AND panic unwind). The temp dir uses its own
//! prefix so it cannot collide with the engine-integration-serial group's
//! dirs (this lane serializes internally; it does not touch that group).

use std::path::{Path, PathBuf};
use std::time::Instant;

use hekma_adapter_api::Manifest;
use hekma_conformance::{fake_agent_bin, run_mock_conformance, TckSection};

/// The repo root, derived from this crate's manifest dir
/// (`<repo>/crates/hekma-conformance`, two hops up) — NOT from the process
/// cwd, which cargo does not guarantee.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve the repo root from CARGO_MANIFEST_DIR")
}

struct PathGuard {
    original: Option<std::ffi::OsString>,
}

impl PathGuard {
    /// Prepend `dir` to the current `PATH`, returning the guard that
    /// restores it on drop (the hermes.rs pattern).
    fn prepend(dir: &Path) -> Self {
        let original = std::env::var_os("PATH").map(|v| v.to_os_string());
        let joined = {
            let mut paths: Vec<PathBuf> =
                std::env::split_paths(std::env::var_os("PATH").as_deref().unwrap_or_default())
                    .collect();
            paths.insert(0, dir.to_path_buf());
            std::env::join_paths(paths).expect("join PATH")
        };
        // SAFETY: `set_var`/`remove_var` are unsafe on every edition since
        // Rust 1.82 (this workspace is edition 2021); the race analysis is
        // in the module doc (process-global mutation, one looping test owns
        // it, the guard's Drop restores unconditionally).
        unsafe {
            std::env::set_var("PATH", &joined);
        }
        PathGuard { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(original) => unsafe {
                std::env::set_var("PATH", original);
            },
            // The variable may be absent on exotic runners; deleting it
            // reproduces that pristine state.
            None => unsafe {
                std::env::remove_var("PATH");
            },
        }
    }
}

/// Copy the fake helper into `dir` named exactly as the manifest's bare
/// exec word (`.exe` suffix on Windows via `std::env::consts::EXE_SUFFIX`),
/// removing any stale copy at the destination first (the stale-helper trap:
/// a stale copy silently does less and the suite burns its budgets on a
/// timing-shaped failure — `docs/testing.md`). Copying preserves permission
/// bits (both binaries are cargo-built executables), so no chmod is needed
/// on any platform.
fn install_exec_shadow(dir: &Path, exec_word: &str) -> PathBuf {
    let dest = dir.join(format!("{exec_word}{}", std::env::consts::EXE_SUFFIX));
    let _ = std::fs::remove_file(&dest);
    std::fs::copy(fake_agent_bin(), &dest).expect("copy fake_agent as the manifest's exec word");
    dest
}

#[test]
fn every_gallery_manifest_conforms_under_a_path_shadowed_fake_exec() {
    let manifests = repo_root().join("docs/adapters/manifests");
    if !manifests.exists() {
        // A packaged crate's test run (hekma-conformance is headed to
        // crates.io) has no docs tree at all — the lane is a repo-side
        // guard, not a crate contract. Skip VISIBLY, never silently.
        hekma_conformance::note_uncaptured(
            "gallery lane skipped: no docs/adapters/manifests in this checkout",
        );
        return;
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&manifests)
        .expect("read the gallery manifests directory")
        .map(|entry| entry.expect("gallery manifests entry").path())
        .collect();
    for path in &dirs {
        assert!(
            path.is_dir(),
            "stray non-directory entry under docs/adapters/manifests (not a \
             manifest dir, would ship unvalidated): {}",
            path.display()
        );
    }
    dirs.retain(|path| path.is_dir());
    dirs.sort();
    assert!(
        !dirs.is_empty(),
        "the gallery ships no manifests under {} — the gallery lane validates \
         docs/adapters/manifests/<agent>/adapter.toml entries",
        manifests.display()
    );

    for dir in &dirs {
        let agent = dir
            .file_name()
            .expect("manifest dir name")
            .to_string_lossy()
            .into_owned();

        // Drift guard: every shipped manifest must have its gallery page.
        let page = repo_root()
            .join("docs/adapters")
            .join(format!("{agent}.md"));
        assert!(
            page.exists(),
            "gallery drift: manifest '{agent}' ships without its page at {}",
            page.display()
        );

        // The manifest must parse with the engine's own parser, its declared
        // kind must match its directory, and its published exec must be a
        // BARE program name (the contract's PATH resolution rule) — never a
        // path: a path-shaped exec would make the shipped manifest
        // machine-specific and un-shadowable.
        let text =
            std::fs::read_to_string(dir.join("adapter.toml")).expect("read the gallery manifest");
        let manifest =
            Manifest::from_toml_str(&text).expect("the gallery manifest parses (adapter.toml)");
        assert_eq!(
            manifest.adapter_kind(),
            Some(agent.as_str()),
            "gallery manifest '{agent}': [adapter].kind must match the directory name"
        );
        let launch = manifest
            .lifecycle
            .and_then(|lifecycle| lifecycle.start)
            .unwrap_or_else(|| panic!("gallery manifest '{agent}' declares no [lifecycle.start]"));
        let exec_word = launch.exec;
        assert!(
            !exec_word.is_empty(),
            "gallery manifest '{agent}': exec must be a bare program name, got an empty string"
        );
        assert!(
            exec_word.split_whitespace().count() == 1,
            "gallery manifest '{agent}': exec must be ONE bare word (no \
             embedded arguments), got {exec_word:?}"
        );
        assert!(
            !exec_word.contains('/') && !exec_word.contains('\\'),
            "gallery manifest '{agent}': exec must be a bare program name \
             resolved on PATH, got {exec_word:?}"
        );

        // The page must document the manifest's exec word verbatim — the
        // PATH-requirement prose and the shipped manifest cannot drift
        // apart silently.
        let page_text = std::fs::read_to_string(&page).expect("read the gallery page");
        assert!(
            page_text.contains(&format!("`{exec_word}`")),
            "gallery drift: the page for '{agent}' never names its \
             manifest's exec word `{exec_word}` in backticks"
        );

        // Shadow the exec word with the fake helper, then run the FULL
        // suite. The guard restores PATH before the next manifest's dir is
        // even read (the guard drops at the end of this block's scope).
        let shim_dir = tempfile::Builder::new()
            .prefix("hekma-gallery-")
            .tempdir()
            .expect("tempdir for the exec shadow");
        install_exec_shadow(shim_dir.path(), &exec_word);
        let _path_guard = PathGuard::prepend(shim_dir.path());

        let started = Instant::now();
        let report = run_mock_conformance(dir);
        let elapsed = started.elapsed();

        // On Fail: print the section + reason verbatim, then fail naming
        // the same reasons — never a silent red.
        for (section, reason) in report.failures() {
            eprintln!("gallery manifest '{agent}' FAILED {section}: {reason}");
        }
        assert!(
            report.is_conformant(),
            "gallery manifest '{agent}' ({exec_word}) is not conformant: failures = {:?}",
            report.failures()
        );

        // The FULL fixed section set, in the fixed pipeline order — the
        // gallery's "conformant" claim covers every section, never a
        // subset. Derived from `TckSection::ALL` itself so the lane grows
        // with the kit (new sections land green-or-red, never silently
        // unasserted).
        let got: Vec<&str> = report.sections.iter().map(|s| s.section.as_str()).collect();
        let expected: Vec<&str> = TckSection::ALL.iter().map(|section| section.id()).collect();
        assert_eq!(
            got, expected,
            "gallery manifest '{agent}': the full fixed section set must run, \
             in the fixed order"
        );

        println!(
            "gallery manifest '{agent}' (exec {exec_word:?}): conformant — \
             full {}-section suite in {elapsed:?}",
            report.sections.len()
        );
    }

    // Nav drift guard: every gallery page must appear in ALL THREE nav
    // surfaces — docs/meta.json, docs/source.config.ts, and the index
    // table — and every index link must target an existing page. A page
    // that vanishes from any surface fails the build by name.
    let adapters_dir = repo_root().join("docs/adapters");
    let meta =
        std::fs::read_to_string(repo_root().join("docs/meta.json")).expect("read docs/meta.json");
    let source_cfg = std::fs::read_to_string(repo_root().join("docs/source.config.ts"))
        .expect("read docs/source.config.ts");
    let index = std::fs::read_to_string(repo_root().join("docs/adapters.md"))
        .expect("read the gallery index");
    for entry in std::fs::read_dir(&adapters_dir)
        .expect("read docs/adapters")
        .map(|entry| entry.expect("adapters entry").path())
    {
        if entry.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let stem = entry
            .file_stem()
            .expect("page stem")
            .to_string_lossy()
            .into_owned();
        assert!(
            meta.contains(&format!("\"adapters/{stem}\"")),
            "nav drift: docs/meta.json does not register adapters/{stem}"
        );
        assert!(
            source_cfg.contains(&format!("'adapters/{stem}.md'")),
            "nav drift: docs/source.config.ts does not list adapters/{stem}.md"
        );
        assert!(
            index.contains(&format!("(adapters/{stem}.md)")),
            "nav drift: the gallery index does not link adapters/{stem}.md"
        );
    }
    for link in index.split("(adapters/").skip(1) {
        let stem = link.split('.').next().unwrap_or("");
        if stem.is_empty() {
            continue;
        }
        assert!(
            adapters_dir.join(format!("{stem}.md")).exists(),
            "the gallery index links a missing page: adapters/{stem}.md"
        );
    }
}
