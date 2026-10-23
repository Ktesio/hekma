//! Integration tests for `hekma agent governance-vectors` (story 19-1, AD-7/FR-44).
//!
//! These drive the REAL `hekma` binary (via `CARGO_BIN_EXE_hekma`) and pin the
//! artifact's release-surface contract: the human summary exits 0 naming the
//! pin facts, and `--json` emits stdout BYTE-FOR-BYTE equal to the committed
//! `vectors-v1.json` — so a Host can literally `hekma agent governance-vectors
//! --json | sha256sum` and match the published `.sha256`.

mod helpers;

use std::path::Path;

use helpers::{run_hekma_agent, TestContext};

/// The committed artifact + its `.sha256` sidecar, resolved from the repo
/// (the engine crate's `governance/` directory).
fn committed_artifact_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../hekma-engine/governance/vectors-v1.json")
}

/// The house skip-guard (the real-agent smokes' skip-unless-present posture,
/// surfaced through 16-1's uncaptured-note channel): the artifact lives in the
/// ENGINE crate's `governance/` directory via a SIBLING-CHECKOUT relative
/// path, which does not exist when the published `hekma` crate is tested
/// standalone (19-1 review round 1). Absent artifact → the stderr skip note
/// + `false`; the caller returns Ok.
fn artifact_present_or_skip() -> bool {
    if committed_artifact_path().exists() {
        return true;
    }
    hekma_conformance::note_uncaptured(
        "note: governance-vectors e2e skipped: the committed engine artifact is not in \
         this checkout (the hekma crate is tested standalone)",
    );
    false
}

#[test]
fn governance_vectors_json_stdout_is_byte_identical_to_the_committed_artifact() {
    if !artifact_present_or_skip() {
        return;
    }
    let state = TestContext::new();
    let ctx = TestContext::new();
    let run = run_hekma_agent(
        &["agent", "governance-vectors", "--json"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert!(
        run.success,
        "the --json path exits 0; stderr: {}",
        run.stderr
    );
    let committed =
        std::fs::read_to_string(committed_artifact_path()).expect("the committed artifact reads");
    assert_eq!(
        run.stdout, committed,
        "stdout is the canonical artifact, byte for byte (no added or missing newline)"
    );
}

#[test]
fn governance_vectors_summary_names_version_checksum_and_case_count() {
    if !artifact_present_or_skip() {
        return;
    }
    let state = TestContext::new();
    let ctx = TestContext::new();
    let run = run_hekma_agent(
        &["agent", "governance-vectors"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert!(
        run.success,
        "the human path exits 0; stderr: {}",
        run.stderr
    );
    // The checksum on stdout matches the committed `.sha256` sidecar.
    let sidecar = std::fs::read_to_string(committed_artifact_path().with_extension("json.sha256"))
        .expect("the committed sidecar reads");
    let sidecar_digest = sidecar
        .split_whitespace()
        .next()
        .expect("the sidecar's `<digest>  <path>` shape");
    assert!(
        run.stdout.contains(sidecar_digest),
        "the summary carries the published sha256 ({sidecar_digest}): {}",
        run.stdout
    );
    // The pin facts: the version and the case count come from the artifact.
    let document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(committed_artifact_path()).expect("read"))
            .expect("the committed artifact parses");
    assert!(
        run.stdout.contains(
            document["vectors_version"]
                .as_str()
                .expect("version is a string"),
        ),
        "the summary names the vectors version: {}",
        run.stdout
    );
    let case_count = document["cases"].as_array().expect("cases array").len();
    assert!(
        run.stdout.contains(&format!("cases     {case_count} ")),
        "the summary names the case count ({case_count}): {}",
        run.stdout
    );
}
