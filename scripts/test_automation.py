#!/usr/bin/env python3
"""Unit tests for repository automation helpers."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import generate_release_docs as release_docs
import generate_homebrew_formula as homebrew_formula


class ReleaseDocsTests(unittest.TestCase):
    def test_release_body_has_single_download_table(self) -> None:
        body = release_docs.render_release_body("v1.2.3", None, [])

        self.assertIn("Initial release history", body)
        self.assertEqual(1, body.count("| Platform | Target | Archive | Checksum |"))

    def test_asset_table_has_all_tier_one_targets_and_checksums(self) -> None:
        table = "\n".join(release_docs.render_asset_table("v1.2.3"))

        for _platform, target, extension in release_docs.TARGETS:
            self.assertIn(f"ktesio-v1.2.3-{target}.{extension}", table)
            self.assertIn(f"ktesio-v1.2.3-{target}.{extension}.sha256", table)
        self.assertIn("ktesio-v1.2.3-checksums.txt", table)

    def test_changelog_groups_conventional_commits(self) -> None:
        grouped = release_docs.group_commits(
            [
                release_docs.Commit("abc1234", "feat: add export"),
                release_docs.Commit("def5678", "fix: repair docs"),
                release_docs.Commit("fff0000", "plain commit"),
            ]
        )

        self.assertEqual(["abc1234"], [commit.sha for commit in grouped["feat"]])
        self.assertEqual(["def5678"], [commit.sha for commit in grouped["fix"]])
        self.assertEqual(["fff0000"], [commit.sha for commit in grouped["other"]])

    def test_semver_key_accepts_only_v_tags(self) -> None:
        self.assertEqual((1, 2, 3), release_docs.semver_key("v1.2.3"))
        self.assertIsNone(release_docs.semver_key("1.2.3"))
        self.assertIsNone(release_docs.semver_key("v1.2.3-beta"))

    def test_release_workflow_contains_expected_asset_and_release_steps(self) -> None:
        workflow = (release_docs.ROOT / ".github" / "workflows" / "release.yml").read_text(
            encoding="utf-8"
        )

        for _platform, target, _extension in release_docs.TARGETS:
            self.assertIn(target, workflow)
        self.assertIn(".sha256", workflow)
        self.assertIn("checksums.txt", workflow)
        self.assertIn("gh release create", workflow)
        self.assertIn("gh release upload", workflow)
        self.assertIn("gh pr create", workflow)
        self.assertIn("generate_homebrew_formula.py", workflow)
        self.assertIn("HOMEBREW_TAP_TOKEN", workflow)
        self.assertIn("CARGO_REGISTRY_TOKEN", workflow)
        # Release publish is explicit about its toolchain: the root
        # rust-toolchain.toml pins bare cargo to the MSRV (1.96.1), but shipped
        # artifacts and the crates.io publish run on latest stable (AI-17).
        self.assertIn("cargo +stable publish --locked -p ktesio", workflow)
        self.assertNotIn("packages: write", workflow)
        self.assertNotIn("oras-project/setup-oras", workflow)
        self.assertNotIn("oras push", workflow)
        self.assertNotIn("ghcr.io", workflow)
        self.assertNotIn("GHCR_TOKEN", workflow)
        self.assertNotIn("org.opencontainers.image", workflow)
        self.assertNotIn("application/vnd.ktesio.release.v1", workflow)

    def test_homebrew_formula_uses_release_assets_and_checksums(self) -> None:
        checksums = {
            "ktesio-v1.2.3-x86_64-apple-darwin.tar.gz": "a" * 64,
            "ktesio-v1.2.3-aarch64-apple-darwin.tar.gz": "b" * 64,
            "ktesio-v1.2.3-x86_64-unknown-linux-gnu.tar.gz": "c" * 64,
            "ktesio-v1.2.3-x86_64-pc-windows-msvc.zip": "d" * 64,
        }

        formula = homebrew_formula.render_formula("v1.2.3", checksums)

        self.assertIn('class Ktesio < Formula', formula)
        self.assertIn('version "1.2.3"', formula)
        self.assertIn("on_macos do", formula)
        self.assertIn("on_arm do", formula)
        self.assertIn("on_intel do", formula)
        self.assertIn("x86_64-apple-darwin", formula)
        self.assertIn("aarch64-apple-darwin", formula)
        self.assertIn("on_linux do", formula)
        self.assertIn("x86_64-unknown-linux-gnu", formula)
        self.assertNotIn("x86_64-pc-windows-msvc", formula)
        self.assertIn('bin.install "kt"', formula)
        # The custom license has no SPDX id: Homebrew gets the unquoted `:any`
        # symbol ("license unspecified"), never a quoted string, and the
        # formula comments the real terms above the clause.
        self.assertIn("license :any", formula)
        self.assertNotIn('license "', formula)
        self.assertIn(
            "# Ktesio Noncommercial-Attribution License 1.0.0", formula
        )
        self.assertIn(
            "commercial use requires the author's written approval", formula
        )

    def test_homebrew_checksum_parser_accepts_sha256sum_lines(self) -> None:
        checksums = homebrew_formula.parse_checksums(
            "\n".join(
                [
                    f"{'A' * 64}  ktesio-v1.2.3-x86_64-apple-darwin.tar.gz",
                    f"{'b' * 64} *ktesio-v1.2.3-aarch64-apple-darwin.tar.gz",
                ]
            )
        )

        self.assertEqual("a" * 64, checksums["ktesio-v1.2.3-x86_64-apple-darwin.tar.gz"])
        self.assertEqual("b" * 64, checksums["ktesio-v1.2.3-aarch64-apple-darwin.tar.gz"])

    def test_ci_runs_coverage_after_primary_gates(self) -> None:
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )

        self.assertIn("needs: [fmt, clippy, test, build, docs, boundary, semver]", ci)
        # Stable jobs are explicit about their toolchain: the root
        # rust-toolchain.toml pins bare cargo to the MSRV (1.96.1), so these jobs
        # select +stable to keep exercising latest stable (AI-17). The `msrv` job
        # (asserted in test_ci_enforces_msrv_floor) still proves the 1.96.1 floor.
        #
        # The `test` job runs the suite via cargo-nextest (#106): nextest gives a
        # per-test kill-timeout so a hung/slow engine integration test becomes a
        # NAMED, time-boxed failure instead of a silent job-level cancel whose
        # logs GitHub discards (the ubuntu symptom), and its retries absorb the
        # windows-latest per-test timing flake. Doctests run separately because
        # nextest does not execute them. nextest is installed with the repo's
        # standard `cargo install --locked` idiom (same as the semver / tarpaulin
        # gates), not a third-party install action, so the SHA-pinned-actions
        # posture is preserved. Assert the whole shape so a regression back to the
        # hang-prone plain `cargo test` suite run — or a dropped doctest pass — is
        # caught.
        self.assertIn("cargo +stable nextest run --workspace --all-targets", ci)
        self.assertIn("cargo +stable test --workspace --doc", ci)
        # nextest (unlike `cargo test --all-targets`) does not hardlink the
        # fake_agent bin to target/debug/fake_agent, so the test job rebuilds it
        # explicitly between the stale-bin `rm` and the nextest run — otherwise the
        # process-spawning tests race on the lib's on-demand build fallback. Guard
        # both the `rm` and the explicit rebuild so neither is dropped.
        self.assertIn("rm -f target/debug/fake_agent target/debug/fake_agent.exe", ci)
        self.assertIn("cargo +stable build -p ktesio-conformance --bin fake_agent", ci)
        # hermes_shim (story 6-2) joins fake_agent in the stale-helper guard for
        # the identical reason — assert its rm + rebuild pair in BOTH jobs too.
        self.assertIn("rm -f target/debug/hermes_shim target/debug/hermes_shim.exe", ci)
        self.assertIn("cargo +stable build -p ktesio-conformance --bin hermes_shim", ci)
        # The COVERAGE job needs the very same guard, for the same reason, and a
        # workspace-wide assertIn cannot tell the two jobs apart — so scope this
        # pair to the coverage step's own script. The stale-helper defect has now
        # bitten twice: first the test job (macOS/Windows metering), then the
        # coverage job, whose `target/` cache key is Cargo.lock-derived, so a
        # helper-only change cannot bust it and the frozen binary silently
        # ignores flags added since (unknown args are `_ => {}`), making the
        # spawning tests wait out deadlines for output that can never arrive.
        coverage_step = ci[ci.index("- name: Enforce coverage") :]
        self.assertIn(
            "rm -f target/debug/fake_agent target/debug/fake_agent.exe",
            coverage_step,
        )
        self.assertIn(
            "cargo +stable build -p ktesio-conformance --bin fake_agent",
            coverage_step,
        )
        # hermes_shim's pair is asserted in the coverage job's own script too.
        self.assertIn(
            "rm -f target/debug/hermes_shim target/debug/hermes_shim.exe",
            coverage_step,
        )
        self.assertIn(
            "cargo +stable build -p ktesio-conformance --bin hermes_shim",
            coverage_step,
        )
        self.assertIn(
            "command -v cargo-nextest >/dev/null 2>&1 "
            "|| cargo +stable install cargo-nextest --locked",
            ci,
        )
        self.assertIn("${{ runner.os }}-cargo-nextest-bin", ci)
        # The old plain-`cargo test` suite command must be GONE — the only
        # remaining `cargo +stable test` in the test job is the `--doc` pass.
        self.assertNotIn("cargo +stable test --workspace --all-targets", ci)
        # Coverage runs tarpaulin ONCE PER CRATE (#101), not one --workspace pass:
        # the --workspace instrumented run OOM'd the 7 GB runner even after the
        # AI-23 fixes. Each per-crate run keeps its resident set small; the UNION of
        # the per-crate LCOV tracefiles reproduces the --workspace result exactly
        # (tarpaulin reports coverage for ALL workspace source each test binary
        # touches, so cross-crate coverage is credited), and the merged aggregate is
        # gated at the SAME workspace >= 95. Assert the whole structure so a
        # regression to the OOM-prone single --workspace pass — or a dropped merge /
        # gate — is caught. The old `--workspace --fail-under 95` command must be
        # GONE (tarpaulin's own per-run fail-under is replaced by the merged gate).
        self.assertNotIn("--workspace --fail-under 95", ci)
        # The per-crate tarpaulin invocation shape (llvm + skip-clean + timeout 180 +
        # verbose preserved; -p "$pkg" + Lcov out into cov/$pkg replaces --workspace).
        self.assertIn(
            'cargo +stable tarpaulin --engine llvm --skip-clean --timeout 180 '
            "--verbose \\\n              -p \"$pkg\" --out Lcov --output-dir "
            '"cov/$pkg"',
            ci,
        )
        # The exact five workspace crates, lightest → heaviest with ktesio-engine
        # LAST (so a heavy-crate OOM still leaves the lighter crates' numbers logged).
        self.assertIn(
            "set -- ktesio-adapter-api ktesio-conformance ktesio-adapters-hermes "
            "ktesio ktesio-engine",
            ci,
        )
        # Each per-crate run is grouped and dumps free/df first, so a per-crate OOM
        # is diagnosable (which crate, at what memory/disk) instead of one opaque
        # --workspace failure.
        self.assertIn("::group::tarpaulin -p $pkg", ci)
        self.assertIn("free -h || true", ci)
        self.assertIn("df -h / || true", ci)
        # lcov merges the per-crate tracefiles into the workspace aggregate; apt
        # installs it. --ignore-errors inconsistent tolerates lcov 2.x's benign
        # cross-run FN-start-line drift (line data unaffected).
        self.assertIn("sudo apt-get install -y -qq lcov", ci)
        self.assertIn("-o cov/merged.info --ignore-errors inconsistent", ci)
        # The merged aggregate is gated at the SAME workspace >= 95: parse the
        # `(<hit> of <found> lines)` fraction from `lcov --summary` and fail (exit 1,
        # ::error::) below 95, else pass.
        self.assertIn("lcov --summary cov/merged.info", ci)
        self.assertIn(
            r"sed -E 's/.*\(([0-9]+) of ([0-9]+) lines\).*/\1 \2/'", ci
        )
        self.assertIn("BEGIN { exit (p + 0 >= 95) ? 0 : 1 }", ci)
        self.assertIn(
            "::error::Merged workspace coverage ${pct}% (${hit}/${found} lines) "
            "is below the 95% gate",
            ci,
        )
        # --engine llvm: parity with the local macOS gate (ptrace is unavailable
        # there). llvm-tools-preview supplies the llvm-profdata/llvm-cov it shells
        # out to. --timeout 180 lifts tarpaulin's 60 s per-test default so a heavy
        # survival test under instrumentation is not killed spuriously.
        self.assertIn("rustup component add llvm-tools-preview", ci)
        # The coverage TIMEOUT fix (AI-23): a DEDICATED cache key so the instrumented
        # target — whose fingerprints differ from the other jobs' normal-profile
        # build — actually persists. The shared key gave coverage nothing reusable
        # and, running last, never saved its own, so every run recompiled the graph
        # cold and blew the cap — not the engine, not the tarpaulin binary install.
        self.assertIn(
            "${{ runner.os }}-cargo-coverage-${{ hashFiles('**/Cargo.lock') }}", ci
        )
        # The source-installed tarpaulin binary is still cached and its install made
        # idempotent (hygiene, mirroring the semver gate's binary cache, AI-1).
        self.assertIn("${{ runner.os }}-cargo-tarpaulin-bin", ci)
        self.assertIn(
            "command -v cargo-tarpaulin >/dev/null 2>&1 "
            "|| cargo +stable install cargo-tarpaulin --locked",
            ci,
        )
        # The instrumented run is serialised, given swap headroom, has `/` disk
        # reclaimed, and builds with line-tables-only debuginfo — the OOM ("runner
        # lost communication", AI-23) came from RAM+disk pressure of the instrumented
        # build (coverage counters + full debuginfo + subprocess-spawning tests), not
        # the engine. CARGO_PROFILE_DEV_DEBUG=1 leaves coverage unchanged (llvm needs
        # only line tables); the SDK cleanup frees ~30 GB on `/` where target/ lives.
        self.assertIn('RUST_TEST_THREADS: "1"', ci)
        self.assertIn('CARGO_PROFILE_DEV_DEBUG: "1"', ci)
        self.assertIn("swapon /mnt/covswap", ci)
        self.assertIn("rm -rf /usr/share/dotnet", ci)

    def test_ci_runs_on_feature_branch_pushes_and_a_nightly_schedule(self) -> None:
        # Story 11-5 (AI-37/AI-54): the full CI — the 3-OS matrix + coverage
        # included — runs on EVERY branch push (not only main, not only PRs)
        # plus a nightly schedule and manual dispatch. The old
        # `branches: [main]` push filter is what let a feature branch reach a
        # PR with locally-verified-only commits (Epic 2's merge-pressure
        # lesson); the `**` glob keeps main's push runs AND covers every
        # feature branch. Pin the whole trigger block so a regression back to
        # main-only pushes — or a dropped schedule/dispatch — fails here.
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )

        self.assertIn('branches:\n      - "**"', ci)
        self.assertNotIn("branches: [main]", ci)
        self.assertIn("pull_request:", ci)
        self.assertIn('cron: "17 7 * * *"', ci)
        self.assertIn("workflow_dispatch:", ci)
        # The dedupe story stays true: a per-ref concurrency group collapses
        # repeated pushes to the same branch (cancel-in-progress).
        self.assertIn("group: ${{ github.workflow }}-${{ github.ref }}", ci)
        self.assertIn("cancel-in-progress: true", ci)

    def test_docs_probe_workflow_watches_every_registered_page(self) -> None:
        # Story 11-5 (epic-7-retro-item-2): a scheduled probe watches the
        # DEPLOYED docs site (Cloudflare Pages, outside repo CI) because a
        # silent deploy death went unnoticed for two weeks (Aug 2026) while
        # repo CI stayed green. Pin the shape: schedule + manual dispatch, no
        # `needs:` coupling to CI, every docs/meta.json page probed for HTTP
        # 200 with each failure NAMED (never fail-fast — one run must report
        # the whole blast radius), and a content marker on the newest page so
        # a 200-from-an-error-page cannot false-pass.
        probe = (
            release_docs.ROOT / ".github" / "workflows" / "docs-probe.yml"
        ).read_text(encoding="utf-8")

        self.assertIn('cron: "3 6 * * *"', probe)
        self.assertIn("workflow_dispatch:", probe)
        # No `needs:` coupling to the CI job graph — checked per LINE so the
        # prose in a comment cannot false-fail the pin.
        self.assertFalse(
            [line for line in probe.splitlines() if line.strip().startswith("needs:")],
            "the docs probe must not declare job-level needs",
        )
        # The page list is read from the site's own registry at probe time.
        self.assertIn("jq -r '.pages[] | select(startswith(\"---\") | not)' docs/meta.json", probe)
        self.assertIn("https://docs.ktesio.dev", probe)
        # Failures name the dead page via ::error:: and fail the job.
        self.assertIn("docs.ktesio.dev page is DOWN", probe)
        self.assertIn("expected 200", probe)
        # The two special-case URL mappings mirror docs/lib/source.ts's
        # slugs(): README is the site root, RELEASE_NOTES serves at
        # /release-notes.
        self.assertIn("README) url=\"$base/\"", probe)
        self.assertIn('RELEASE_NOTES) url="$base/release-notes"', probe)
        # The newest-page content marker (a bare 200 is not enough).
        self.assertIn('marker="Release Notes"', probe)

    def test_ci_test_job_runs_on_three_os_matrix(self) -> None:
        # Story 1.4 (AD-4, NFR-2): the `test` job runs on a 3-OS matrix so the
        # per-OS ProcessBackend supervision code — in particular the Windows
        # Job-Object backend, which does not even compile on Linux — is proven on
        # a real Windows runner. Lock the matrix shape (mirrors the MSRV-floor
        # lock in test_ci_enforces_msrv_floor). Coverage stays Linux-only; the
        # matrix is the parity-honesty mechanism, not tarpaulin.
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )

        self.assertIn("os: [ubuntu-latest, macos-latest, windows-latest]", ci)
        self.assertIn("runs-on: ${{ matrix.os }}", ci)
        self.assertIn("fail-fast: false", ci)
        # Only the `test` job matrixes; the other jobs stay ubuntu-only. The
        # coverage job still stays Linux-only (now a per-crate tarpaulin split
        # merged into one aggregate, #101 — still Linux, still one job).
        self.assertIn("name: coverage", ci)

    def test_ci_test_job_single_run_with_serialized_engine_integration(self) -> None:
        # #106: a per-binary CI bisect pinned the x86-ubuntu hang to the `adoption`
        # engine integration binary (its heavy re-exec + surviving-orphan harness
        # wedged the genuine 2-core runner into an uninterruptible D-state under
        # nextest's concurrency). The fix is SOURCE-side — serialize the engine
        # integration tests via a nextest test-group (max-threads=1) — so the CI
        # `test` job is back to a SINGLE clean run for all OSes. Lock that clean
        # shape and assert the serialization exists; guard that the temporary
        # per-binary bisect scaffold AND the earlier watchdog/off-pipe machinery are
        # gone (so a future edit that reintroduces per-OS hang hacks is caught).
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        nextest = (release_docs.ROOT / ".config" / "nextest.toml").read_text(
            encoding="utf-8"
        )
        # Clean shape: build test binaries once (--no-run), ONE unified run for all
        # OSes, doctests separate.
        self.assertIn(
            "cargo +stable nextest run --workspace --all-targets --no-run", ci
        )
        self.assertIn("- name: Run tests (nextest)", ci)
        self.assertIn("cargo +stable nextest run --workspace --all-targets", ci)
        self.assertIn("cargo +stable test --workspace --doc", ci)
        # The SOURCE fix: a max-threads=1 test-group over exactly the engine
        # integration binaries, with the existing retries + slow-timeout untouched.
        self.assertIn("[test-groups.engine-integration-serial]", nextest)
        self.assertIn("max-threads = 1", nextest)
        self.assertIn('filter = "kind(test) & package(ktesio-engine)"', nextest)
        self.assertIn('test-group = "engine-integration-serial"', nextest)
        self.assertIn("retries = 2", nextest)
        self.assertIn("slow-timeout = ", nextest)
        # The temporary bisect scaffold and the watchdog/off-pipe machinery are GONE
        # (no per-OS special-casing, no per-binary steps, no in-step capture hacks).
        self.assertNotIn("ubuntu bisect:", ci)
        self.assertNotIn("binary_id(=ktesio-engine::", ci)
        self.assertNotIn("RUNLOG", ci)
        self.assertNotIn("kill -USR1 $$", ci)
        self.assertNotIn("matrix.os == 'ubuntu-latest'", ci)
        self.assertNotIn("matrix.os != 'ubuntu-latest'", ci)

    def test_ci_enforces_workspace_boundary_and_semver_gates(self) -> None:
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )

        # Stable jobs select +stable so the root rust-toolchain.toml pin (MSRV
        # 1.96.1) does not silently redirect them off latest stable (AI-17).
        self.assertIn("cargo +stable check -p ktesio", ci)
        self.assertIn("cargo +stable tree -p ktesio -e normal,build --all-features", ci)
        # Boundary gate is an allowlist: only these internal edges may exist.
        self.assertIn("ktesio-(engine|adapter-api|adapters-hermes)", ci)
        # OS-cfg gate uses the broadened class pattern (compound cfg forms).
        self.assertIn("cfg[!(]?.*(unix|windows|target_os|target_family)", ci)
        self.assertIn("crates/ktesio-engine/src/backends/", ci)
        # OS-cfg allowlist covers honestly-unix-gated engine integration tests
        # (AI-35 disclosure convention) alongside the backends home. Assert the
        # FULL allowlist LINE shape (review blind-12): a bare substring would
        # also match a stale comment quoting the pattern, so a narrowed
        # allowlist (e.g. a dropped legacy-file entry) must fail here.
        self.assertIn(
            r"allowlist='^crates/ktesio-engine/src/backends/"
            r"|^crates/kt/src/update_check\.rs:"
            r"|^crates/kt/src/cli/self_update\.rs:"
            r"|^crates/ktesio-engine/tests/'",
            ci,
        )
        # Currency gate (story 3-3, AD-8): exactly one module formats a `$` string.
        # It scans for the BROADENED set of dollar-string-building forms (`${`, `$ {`,
        # `}$`, a `"$`-prefixed string, and a bare `'$'` char) and allowlists the
        # single currency module (domain/cost.rs) PLUS the precise non-currency `$`
        # uses the broadened pattern also matches (`.contains('$')` discipline
        # read-checks; the `$PWD`/`$KT_TEST` shell vars in a backend test). Guard both
        # the pattern and the allowlist so a regression that scatters currency
        # formatting is caught — and so the broadening is not silently narrowed back.
        self.assertIn("Enforce single currency-formatting module", ci)
        self.assertIn(r"""pattern='\$ ?\{|\}\$|"\$|'\''\$'\''""", ci)
        self.assertIn(
            r"allowlist='^crates/ktesio-engine/src/domain/cost\.rs:"
            r"|contains\(.\$|\$PWD|\$KT_TEST'",
            ci,
        )
        # Semver gate: lazy install inside the armed branch, transient skip.
        # (AI-17, story 11-7: these three pins moved OUT of this file-wide
        # test into the semver-job-scoped assertions inside
        # test_ci_enforces_workspace_boundary_and_semver_gates below —
        # file-wide matches let a dropped baseline-pinned gate pass via
        # another job's site. The transient-skip regex appears exactly once
        # in ci.yml (the semver job), so the scoped pin loses no coverage.)
        # Semver gate caches the source-installed binary so it is not rebuilt
        # (~10 min) on every fresh runner (AI-1). AI-3 (story 6-6): the cache
        # key names the RESOLVED cargo-semver-checks version (content, never a
        # constant), the install pins that resolved version, and the save is
        # conditional on an install actually having run — the old constant key
        # let a dormant run freeze an entry WITHOUT the binary, which the armed
        # gate then restored on every run and reinstalled over.
        self.assertIn(
            "${{ runner.os }}-cargo-semver-checks-bin-${{ steps.semver_version.outputs.version }}",
            ci,
        )
        self.assertIn("id: semver_version", ci)
        self.assertIn("echo \"installed=true\" >> \"$GITHUB_OUTPUT\"", ci)
        self.assertIn("if: steps.semver_check.outputs.installed == 'true'", ci)
        self.assertIn("uses: actions/cache/restore@", ci)
        self.assertIn("uses: actions/cache/save@", ci)
        # AI-3 hardening (review of 6-6): a resolved version is VALIDATED
        # before it can reach an install pin, and a restored binary is
        # version-VERIFIED (not merely present) before the pinned install is
        # skipped — restore-keys can restore an older binary.
        self.assertIn('[[ "$version" =~ ^[0-9]+\\.[0-9]+\\.[0-9]+$ ]] || version=unknown', ci)
        self.assertIn("cached=\"$(cargo-semver-checks --version 2>/dev/null | tail -n 1 | awk '{print $NF}' || true)\"", ci)
        # The probe must invoke the binary DIRECTLY: `cargo cargo-semver-checks`
        # asks cargo for an external subcommand and always fails, which made
        # every cache-hit semver run reinstall over the restored binary and
        # abort (main was red across 102a7fc..b661c13).
        self.assertNotIn("cargo +stable cargo-semver-checks --version", ci)
        self.assertIn('if [ "$cached" != "$version" ]; then', ci)
        # The old constant key must not resurface.
        self.assertNotIn("key: ${{ runner.os }}-cargo-semver-checks-bin\n", ci)
        # In-repo baseline guard (#160, epic-6 retro A1): the frozen
        # ktesio-adapter-api v1 surface is diffed against the contract-v1
        # freeze commit on EVERY run — the crates.io gates below it stay
        # dormant (404 -> notice) until the HELD publish executes
        # (docs/release-process.md), so this run is the active guard against
        # an unannounced public-item removal/rename. The baseline rev must be
        # the full freeze-commit SHA; `--baseline-rev` is a stable
        # cargo-semver-checks flag (verified locally: green vs 4119db3,
        # `function_missing` failure on a removed pub item).
        self.assertIn(
            "cargo +stable semver-checks check-release -p ktesio-adapter-api "
            "--baseline-rev 4119db37b5288b990144d28f995ee14a69271b5e",
            ci,
        )
        # Story 7-4 arms the SAME mechanism for ktesio-engine, against a
        # DIFFERENT honest freeze point: the embedding-surface freeze
        # 8a8b328 (story 7-3), NOT the contract freeze 4119db3 — the engine's
        # unpublished surface legitimately grew after the contract froze (the
        # freeze itself added `LaunchResolveError::ContractIncompatible`, and
        # a 4119db3 baseline fails two major lints on that honest history),
        # so the engine's embedding surface freezes at 7-3's commit. Both
        # baseline revs get the cat-file resolvability check so a history
        # rewrite is a CLEAR infra error. Maintenance: bump all four pins
        # (two here, two in ci.yml) together at the next deliberate freeze.
        self.assertIn(
            "cargo +stable semver-checks check-release -p ktesio-engine "
            "--baseline-rev 12f3aabfadcd9a6c18c03837dbd2778b1e9f5755",
            ci,
        )
        self.assertIn("git cat-file -e 12f3aabfadcd9a6c18c03837dbd2778b1e9f5755^{commit}", ci)
        # The baseline lookup needs full history: the semver job's checkout
        # must override the default shallow clone. Scoped to the SEMVER JOB
        # BLOCK ONLY (up to the next job heading): a `fetch-depth: 0` in some
        # later job must not false-pass the semver checkout's pin.
        semver_start = ci.index("\n  semver:")
        # The next JOB heading is the next line at exactly two-space indent
        # starting a word (`\n  <name>:`) — lines inside the job are indented
        # four or more spaces, so a naive "next two spaces" slice would cut
        # inside the job body and empty the assertion's haystack.
        import re as _re
        next_job = _re.search(r"\n  [A-Za-z_-]", ci[semver_start + 1 :])
        semver_job = ci[semver_start : semver_start + 1 + next_job.start()]
        self.assertIn("fetch-depth: 0", semver_job)
        # The baseline rev must be verified resolvable BEFORE the gate runs —
        # a history rewrite/shallow clone is a CLEAR infra error, never a
        # confusing semver-checks resolution abort. Maintenance contract: at
        # the next contract major, bump the SHA here AND in ci.yml together.
        self.assertIn(
            "git cat-file -e 4119db37b5288b990144d28f995ee14a69271b5e^{commit}", ci
        )
        # AI-17 (story 11-7): the semver gate's OWN pins, scoped to the semver
        # job's text (this test — test_ci_enforces_workspace_boundary_and_semver_
        # gates — owns the semver_job slice). A file-wide match would let a
        # dropped baseline-pinned gate pass via an unrelated invocation.
        # (1) The check-release invocations: EXACTLY three armed sites in THIS
        # job, counted with comments stripped so prose cannot inflate the count.
        import re as _re

        code_only = _re.sub(r"(?m)^\s*#.*$", "", semver_job)
        check_release_sites = code_only.count(
            "cargo +stable semver-checks check-release"
        )
        self.assertEqual(
            check_release_sites,
            3,
            "the semver job must retain all three armed check-release gates "
            f"(baseline, in-repo, forward-compat); found {check_release_sites}",
        )
        # (2) The lazy install is GUARDED and ORDERED: the `command -v` probe
        # precedes the plain install (a cache hit must never pay a ~10-min
        # source rebuild), and the version-validated install form also exists.
        self.assertIn("if ! command -v cargo-semver-checks >/dev/null 2>&1; then", semver_job)
        self.assertIn(
            "cargo +stable install cargo-semver-checks --locked --force\n", semver_job
        )
        self.assertIn(
            'cargo +stable install cargo-semver-checks --locked --force --version "$version"',
            semver_job,
        )
        self.assertLess(
            semver_job.index("if ! command -v cargo-semver-checks >/dev/null 2>&1; then"),
            semver_job.index("cargo +stable install cargo-semver-checks --locked --force"),
            "the cache-hit probe must precede the plain install (AI-1's guard)",
        )
        # (3) The transient-skip regex is the semver job's own retry predicate.
        self.assertIn("000|429|5[0-9][0-9]", semver_job)

    def test_ci_builds_and_runs_the_embedding_quickstart(self) -> None:
        # Story 7-4: the embedding quickstart (the publish capstone's host
        # example) is exercised by the CI `build` job — compiled in release
        # next to the shipping binary, then RUN hermetically (its only
        # subprocess is a re-exec of itself idling under a hard cap, on a
        # temp-dir root; every leg asserts). Guard both steps so a facade
        # regression can never silently drop out of the ordinary build gate,
        # and guard the example file itself so the steps cannot outlive it.
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        example = (
            release_docs.ROOT
            / "crates"
            / "ktesio-engine"
            / "examples"
            / "embedding-quickstart.rs"
        )

        build_start = ci.index("- name: Build release binary")
        # The next JOB heading bounds the build job's own script text; a
        # missing match (job LAST in the file) slices to end-of-file instead.
        import re as _re

        next_job = _re.search(r"\n  [A-Za-z_-]", ci[build_start + 1 :])
        build_end = build_start + 1 + next_job.start() if next_job else len(ci)
        build_job = ci[build_start:build_end]
        self.assertIn(
            "cargo +stable build --release --example embedding-quickstart -p ktesio-engine",
            build_job,
        )
        # The RUN step is guarded: a step-level timeout independent of the
        # agent's own idle cap, and an existence assert with a clear error so
        # a build-step regression fails loudly instead of "file not found".
        self.assertIn("timeout-minutes: 5", build_job)
        self.assertIn("test -x target/release/examples/embedding-quickstart", build_job)
        self.assertIn("target/release/examples/embedding-quickstart", build_job)
        self.assertTrue(example.is_file(), "the quickstart example must exist")

    def test_ci_perf_budgets_job_runs_the_harness_blocking(self) -> None:
        # Story 7-5 (NFR-4): the designated perf job runs the perf-budgets
        # harness on every PR and GATES the measured budgets — reads p99 and
        # RSS/instance gate hard; CPU/instance carries the harness's
        # documented shared-runner tolerance over a median-of-3 window in CI.
        # Guard the whole shape so the gate cannot silently drop out: the job
        # exists, builds the release example, carries the stale-helper rm +
        # rebuild guard (the harness SPAWNS fake_agent, so the test/coverage
        # -job rule "any job that spawns agents carries it" applies), runs
        # the binary BLOCKING (the run block's last line IS the bare binary
        # invocation, and the block contains no failure-swallowing idiom),
        # uploads the JSON report artifact, and the harness example file
        # itself exists.
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        example = (
            release_docs.ROOT
            / "crates"
            / "ktesio-engine"
            / "examples"
            / "perf-budgets.rs"
        )
        job_start = ci.index("  perf-budgets:")
        import re as _re

        next_job = _re.search(r"\n  [A-Za-z_-]", ci[job_start + 1 :])
        # The perf job may be LAST in the file — slice to end-of-file then.
        job_end = job_start + 1 + next_job.start() if next_job else len(ci)
        perf_job = ci[job_start:job_end]
        self.assertIn("name: perf-budgets", perf_job)
        self.assertIn("runs-on: ubuntu-latest", perf_job)
        self.assertIn(
            "cargo +stable build --release -p ktesio-engine --example perf-budgets",
            perf_job,
        )
        # The stale-helper guard: rm + explicit rebuild for the fake_agent
        # this harness spawns (same defect class the test job hit twice).
        self.assertIn(
            "rm -f target/release/fake_agent target/release/fake_agent.exe", perf_job
        )
        self.assertIn(
            "cargo +stable build --release -p ktesio-conformance --bin fake_agent",
            perf_job,
        )
        # Blocking, pinned structurally beyond substrings: the RUN step's
        # block must END on the bare binary invocation (optional leading
        # whitespace) — a tee/pipe/&& suffix could swallow the harness's
        # nonzero exit — and must contain no failure-swallowing idiom.
        run_step = perf_job.index("- name: Run the perf harness")
        run_text = perf_job[perf_job.index("run: |", run_step) :]
        next_step = _re.search(r"\n      - name:", run_text[1:])
        run_block = (
            run_text[: 1 + next_step.start()] if next_step else run_text
        )
        last_line = next(
            line for line in reversed(run_block.splitlines()) if line.strip()
        )
        self.assertRegex(
            last_line, r"^\s*target/release/examples/perf-budgets\s*$"
        )
        self.assertNotIn("|| true", run_block)
        self.assertNotIn("exit 0", run_block)
        # The measured record is also uploaded as the job's artifact (even on
        # a red run): the harness writes it via the env var, the upload step
        # publishes it, and the upload is not skipped on failure.
        self.assertIn("PERF_BUDGETS_REPORT_PATH: perf-budgets-report.json", perf_job)
        self.assertIn("actions/upload-artifact@", perf_job)
        upload = perf_job[perf_job.index("- name: Upload the measured report") :]
        self.assertIn("if: always()", upload)
        self.assertIn("actions/upload-artifact@", upload)
        self.assertIn("perf-budgets-report.json", upload)
        self.assertTrue(example.is_file(), "the perf-budgets harness example must exist")

    def test_publish_flags_match_the_post_v070_release_state(self) -> None:
        """Publish-day state (go recorded 2026-09-09, release issue #176):
        the three library crates are PUBLISHED (no publish=false) and
        ktesio-conformance KEEPS its flag (dev/test kit, separate decision).
        This pin now catches an accidental re-add of publish=false to the
        published crates, or a flip of conformance's flag."""
        for crate in ("ktesio-adapter-api", "ktesio-adapters-hermes", "ktesio-engine"):
            manifest = (release_docs.ROOT / "crates" / crate / "Cargo.toml").read_text(
                encoding="utf-8"
            )
            self.assertNotIn("publish = false", manifest, crate)
            self.assertNotIn("publish.workspace", manifest, crate)
        conformance = (release_docs.ROOT / "crates" / "ktesio-conformance" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        # The dev/test kit KEEPS its flag — publishing it is a separate,
        # undecided decision (docs/release-process.md decision log).
        self.assertIn("publish = false", conformance, "ktesio-conformance")


    def test_ci_enforces_msrv_floor(self) -> None:
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )

        # MSRV job installs the pinned floor toolchain explicitly and checks the
        # whole workspace against it — --all-targets, so the DEV-dependencies
        # (notably the story-7-5 perf harness's sysinfo) compile on the floor
        # too and a future sysinfo MSRV bump past 1.96.1 reds the msrv job.
        # Keep the version in lockstep with rust-version in the root
        # Cargo.toml [workspace.package].
        self.assertIn("name: msrv", ci)
        self.assertIn("rustup toolchain install 1.96.1 --profile minimal", ci)
        self.assertIn("cargo +1.96.1 check --workspace --all-targets", ci)

        cargo_toml = (release_docs.ROOT / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn('rust-version = "1.96.1"', cargo_toml)

        # AI-17: a root rust-toolchain.toml pins bare cargo to the MSRV so
        # local `cargo build/test/clippy/fmt` need no `+1.96.1`. It must stay in
        # lockstep with rust-version; the `msrv` job above still proves the floor
        # (bare cargo in CI would otherwise resolve to this pin, not stable —
        # hence the explicit +stable on the stable jobs).
        toolchain_toml = (release_docs.ROOT / "rust-toolchain.toml").read_text(
            encoding="utf-8"
        )
        self.assertIn('channel = "1.96.1"', toolchain_toml)


class InstallerScriptTests(unittest.TestCase):
    def run_install_sh(
        self,
        env: dict[str, str],
        *,
        expect_success: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        script = release_docs.ROOT / "scripts" / "public" / "install.sh"
        merged_env = os.environ.copy()
        merged_env.update(
            {
                "KTESIO_INSTALL_DRY_RUN": "1",
                "KTESIO_INSTALL_TEST_KT_PATH": "",
                "KTESIO_INSTALL_TEST_HAS_BREW": "0",
                "KTESIO_INSTALL_TEST_HAS_CARGO": "0",
                "KTESIO_INSTALL_TEST_OS": "Linux",
                "KTESIO_INSTALL_TEST_ARCH": "x86_64",
                "CARGO_HOME": "",
            }
        )
        merged_env.update(env)

        result = subprocess.run(
            ["sh", str(script)],
            cwd=release_docs.ROOT,
            env=merged_env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        if expect_success:
            self.assertEqual(
                0,
                result.returncode,
                result.stdout + result.stderr,
            )
        else:
            self.assertNotEqual(0, result.returncode, result.stdout + result.stderr)

        return result

    def fake_kt(self, directory: Path, output: str = "kt 1.2.3") -> Path:
        path = directory / "kt"
        path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{output}'\n", encoding="utf-8")
        path.chmod(0o755)
        return path

    def test_install_sh_prefers_homebrew_for_new_installs(self) -> None:
        result = self.run_install_sh({"KTESIO_INSTALL_TEST_HAS_BREW": "1"})

        self.assertIn("DRY RUN: brew install imagdy/tap/ktesio", result.stdout)

    def test_install_sh_uses_cargo_when_homebrew_is_unavailable(self) -> None:
        result = self.run_install_sh({"KTESIO_INSTALL_TEST_HAS_CARGO": "1"})

        self.assertIn("DRY RUN: cargo install ktesio --force", result.stdout)

    def test_install_sh_uses_prebuilt_binary_without_package_managers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_install_sh({"HOME": tmp})

        self.assertIn(
            "DRY RUN: install prebuilt x86_64-unknown-linux-gnu",
            result.stdout,
        )

    def test_install_sh_updates_existing_homebrew_install(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            kt_path = self.fake_kt(Path(tmp))
            result = self.run_install_sh(
                {
                    "KTESIO_INSTALL_TEST_KT_PATH": str(kt_path),
                    "KTESIO_INSTALL_TEST_BREW_INSTALLED": "1",
                    "KTESIO_INSTALL_TEST_HAS_BREW": "1",
                }
            )

        self.assertIn("DRY RUN: brew upgrade imagdy/tap/ktesio", result.stdout)

    def test_install_sh_updates_existing_cargo_install(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            cargo_bin = home / ".cargo" / "bin"
            cargo_bin.mkdir(parents=True)
            kt_path = self.fake_kt(cargo_bin)
            result = self.run_install_sh(
                {
                    "HOME": str(home),
                    "KTESIO_INSTALL_TEST_KT_PATH": str(kt_path),
                    "KTESIO_INSTALL_TEST_HAS_CARGO": "1",
                }
            )

        self.assertIn("DRY RUN: cargo install ktesio --force", result.stdout)

    def test_install_sh_replaces_existing_manual_install(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bin_dir = Path(tmp) / "bin"
            bin_dir.mkdir()
            kt_path = self.fake_kt(bin_dir)
            result = self.run_install_sh(
                {
                    "KTESIO_INSTALL_TEST_KT_PATH": str(kt_path),
                    "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                }
            )

        self.assertIn(f"to {kt_path}", result.stdout)

    def test_install_sh_rejects_unwritable_manual_install_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bin_dir = Path(tmp) / "bin"
            bin_dir.mkdir()
            kt_path = self.fake_kt(bin_dir)
            bin_dir.chmod(0o555)
            try:
                result = self.run_install_sh(
                    {"KTESIO_INSTALL_TEST_KT_PATH": str(kt_path)},
                    expect_success=False,
                )
            finally:
                bin_dir.chmod(0o755)

        output = result.stdout + result.stderr
        self.assertIn("is not writable", output)
        self.assertIn("KTESIO_INSTALL_DIR", output)

    def test_install_sh_rejects_unsupported_binary_target_without_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_install_sh(
                {"HOME": tmp, "KTESIO_INSTALL_TEST_ARCH": "aarch64"},
                expect_success=False,
            )

        self.assertIn("No prebuilt Ktesio binary is available", result.stderr)

    def test_install_sh_refuses_non_ktesio_kt_conflict(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            kt_path = self.fake_kt(Path(tmp), "not ktesio")
            result = self.run_install_sh(
                {"KTESIO_INSTALL_TEST_KT_PATH": str(kt_path)},
                expect_success=False,
            )

        self.assertIn("Refusing to overwrite non-Ktesio kt", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
