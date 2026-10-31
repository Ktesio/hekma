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

    def test_rename_banner_links_canonical_docs_url(self) -> None:
        # The site lives at /docs/<page>/ since the docs restructure; the
        # flat /migration form only survives as a legacy 301. Generated
        # release bodies must carry the canonical URL so they never depend
        # on the redirect (and stay correct if the 301 is ever retired).
        body = release_docs.render_release_body("v1.2.3", None, [])

        self.assertIn("https://hekma.ktesio.dev/docs/migration/", body)
        self.assertNotIn("https://hekma.ktesio.dev/migration)", body)

    def test_rename_banner_absent_before_v0_8_0(self) -> None:
        body = release_docs.render_release_body("v0.7.2", None, [])

        self.assertNotIn("migration guide", body)

    def test_asset_table_has_all_tier_one_targets_and_checksums(self) -> None:
        table = "\n".join(release_docs.render_asset_table("v1.2.3"))

        for _platform, target, extension in release_docs.TARGETS:
            self.assertIn(f"hekma-v1.2.3-{target}.{extension}", table)
            self.assertIn(f"hekma-v1.2.3-{target}.{extension}.sha256", table)
        self.assertIn("hekma-v1.2.3-checksums.txt", table)

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

    def test_pretag_rehearsal_keeps_the_previous_tag_lower_bound(self) -> None:
        # Pre-tag dry run: the tag has no ref yet, so HEAD stands in for it —
        # but the previous release's lower bound must survive, or the
        # rehearsal aggregates full history instead of the pending range.
        self.assertEqual(
            "v0.9.0..HEAD",
            release_docs.resolve_rev("v0.10.0", "v0.9.0", tag_exists=False),
        )
        # Post-tag (the release workflow): the exact tag range.
        self.assertEqual(
            "v0.9.0..v0.10.0",
            release_docs.resolve_rev("v0.10.0", "v0.9.0", tag_exists=True),
        )
        # No previous tag (fresh history, or CI's v0.0.0 smoke on a
        # tag-less shallow checkout): bare HEAD.
        self.assertEqual("HEAD", release_docs.resolve_rev("v0.0.0", None, tag_exists=False))

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
        self.assertIn("cargo +stable publish --locked -p hekma", workflow)
        # v0.8.0 rename: the release writes the renamed formula AND the
        # tap-root formula_renames.json (preserving other entries), so
        # `brew install|upgrade ktesio` resolves to hekma.
        self.assertIn('renames["ktesio"] = "hekma"', workflow)
        self.assertIn("git -C homebrew-tap add Formula/hekma.rb formula_renames.json", workflow)
        # The rename ENGAGES only when the old formula file is gone (brew
        # consults formula_renames.json solely then) — the workflow must
        # delete Formula/ktesio.rb in the same tap commit.
        self.assertIn(
            "git -C homebrew-tap rm --ignore-unmatch Formula/ktesio.rb", workflow
        )
        # M4 ordering guard: the CLI publish waits for the hekma-* library
        # chain to be live on crates.io.
        self.assertIn("for lib in hekma-adapter-api hekma-adapters-hermes hekma-engine; do", workflow)
        self.assertIn('--output homebrew-tap/Formula/hekma.rb', workflow)
        self.assertNotIn("packages: write", workflow)
        self.assertNotIn("oras-project/setup-oras", workflow)
        self.assertNotIn("oras push", workflow)
        self.assertNotIn("ghcr.io", workflow)
        self.assertNotIn("GHCR_TOKEN", workflow)
        self.assertNotIn("org.opencontainers.image", workflow)
        self.assertNotIn("application/vnd.ktesio.release.v1", workflow)

    def test_release_workflow_builds_and_verifies_the_static_musl_leg(self) -> None:
        # The static Linux leg is a matrix-flag-gated addition to the same
        # release flow: musl-tools provisioned at build time, the C deps
        # compiled through musl-gcc (gated $GITHUB_ENV export — no empty-string
        # env on other legs), a static-ness gate, the shipped archive PROVEN to
        # contain all three binaries, and hekma-host staged on THIS leg only —
        # the glibc archive keeps its pinned `hekma + hkm` contents.
        workflow = (release_docs.ROOT / ".github" / "workflows" / "release.yml").read_text(
            encoding="utf-8"
        )

        self.assertIn("target: x86_64-unknown-linux-musl", workflow)
        self.assertIn("musl: true", workflow)
        self.assertIn("sudo apt-get install -y musl-tools", workflow)
        self.assertIn("CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER", workflow)
        self.assertIn("CC_x86_64_unknown_linux_musl", workflow)
        # Both wordings: rustc's musl default is static-pie (`file` prints
        # "static-pie linked"); plain static binaries print "statically
        # linked". The pin and the gate must not drift apart.
        self.assertIn("static(ally|-pie) linked", workflow)
        # The archive-content gate: all three binaries listed from the
        # shipped tarball (the `./` prefix matches what Package Unix tars).
        for binary in ("./hekma", "./hkm", "./hekma-host"):
            self.assertIn(f'tar -tzf "dist/${{asset}}" | grep -qx "{binary}"', workflow)
        # The GATING: every musl-only concern is conditioned on the matrix
        # flag, and the hekma-host stage specifically runs NOWHERE else — a
        # workflow that stages hekma-host on every leg (or gates nothing)
        # fails this pin.
        for step in (
            "Install musl tools",
            "Configure the musl build env",
            "Verify static",
            "Verify archive contents",
            "Stage hekma-host",
        ):
            start = workflow.index(f"name: {step}")
            end = workflow.index("- name:", start)
            step_body = workflow[start:end]
            self.assertIn("if: matrix.musl", step_body)
        self.assertGreaterEqual(workflow.count("if: matrix.musl"), 5)

    def test_homebrew_formula_uses_release_assets_and_checksums(self) -> None:
        checksums = {
            "hekma-v1.2.3-x86_64-apple-darwin.tar.gz": "a" * 64,
            "hekma-v1.2.3-aarch64-apple-darwin.tar.gz": "b" * 64,
            "hekma-v1.2.3-x86_64-unknown-linux-gnu.tar.gz": "c" * 64,
            "hekma-v1.2.3-x86_64-pc-windows-msvc.zip": "d" * 64,
            # The static musl archive rides the aggregate checksums file; the
            # formula generator must TOLERATE the extra entry (its missing-
            # asset check only requires its own Homebrew targets).
            "hekma-v1.2.3-x86_64-unknown-linux-musl.tar.gz": "e" * 64,
        }

        formula = homebrew_formula.render_formula("v1.2.3", checksums)

        self.assertIn('class Hekma < Formula', formula)
        self.assertIn('version "1.2.3"', formula)
        self.assertIn("on_macos do", formula)
        self.assertIn("on_arm do", formula)
        self.assertIn("on_intel do", formula)
        self.assertIn("x86_64-apple-darwin", formula)
        self.assertIn("aarch64-apple-darwin", formula)
        self.assertIn("on_linux do", formula)
        self.assertIn("x86_64-unknown-linux-gnu", formula)
        self.assertNotIn("x86_64-pc-windows-msvc", formula)
        # Homebrew installs the glibc Linux artifact; the static musl archive
        # is a release-page download for containers/scratch. The formula must
        # contain NO musl reference even when the aggregate checksums file
        # carries the musl asset.
        self.assertNotIn("musl", formula)
        # BOTH shipped binaries install from the one formula.
        self.assertIn('bin.install "hekma"', formula)
        self.assertIn('bin.install "hkm"', formula)
        self.assertIn('shell_output("#{bin}/hkm --version")', formula)
        # v0.9.0+: the standard SPDX id, quoted (a string, not a Ruby
        # symbol); the old custom license is gone from the formula.
        self.assertIn('license "Apache-2.0"', formula)
        self.assertNotIn("license :any", formula)
        self.assertNotIn("Noncommercial-Attribution", formula)

    def test_homebrew_checksum_parser_accepts_sha256sum_lines(self) -> None:
        checksums = homebrew_formula.parse_checksums(
            "\n".join(
                [
                    f"{'A' * 64}  hekma-v1.2.3-x86_64-apple-darwin.tar.gz",
                    f"{'b' * 64} *hekma-v1.2.3-aarch64-apple-darwin.tar.gz",
                ]
            )
        )

        self.assertEqual("a" * 64, checksums["hekma-v1.2.3-x86_64-apple-darwin.tar.gz"])
        self.assertEqual("b" * 64, checksums["hekma-v1.2.3-aarch64-apple-darwin.tar.gz"])

    def test_ci_runs_coverage_after_primary_gates(self) -> None:
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )

        # The coverage job gates on EVERY primary gate — including the
        # governance-vectors job story 19-1 added, so a vectors artifact that
        # fails Host-side verification blocks coverage too.
        self.assertIn(
            "needs: [fmt, clippy, test, build, docs, boundary, semver, governance-vectors]",
            ci,
        )
        # Wedge forensics (2026-10-17, #292): the coverage job is pinned to
        # ubuntu-26.04 — the same class-2 runner-freeze mitigation as the
        # test matrix's linux leg. Both
        # coverage runs of bc561ee on the 24.04 image died ~52 min into
        # Enforce coverage with "the hosted runner lost communication with the
        # server" (the wedge starving the runner itself), while the PR's own
        # ubuntu-26.04 test legs were green twice. Scope the assertion to the
        # coverage JOB so it cannot be satisfied by another job's runner label,
        # and so a regression back to the drifting ubuntu-latest label — or a
        # reordering that moves the string out of this job — fails loudly.
        job_start = ci.index("  coverage:")
        import re as _re

        next_job = _re.search(r"\n  [A-Za-z_-]", ci[job_start + 1 :])
        # The coverage job may be LAST in the file — slice to end-of-file then.
        job_end = job_start + 1 + next_job.start() if next_job else len(ci)
        coverage_job = ci[job_start:job_end]
        self.assertIn("name: coverage", coverage_job)
        self.assertIn("runs-on: ubuntu-26.04", coverage_job)
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
        # nextest does not execute them. nextest is installed from a
        # VERSION+SHA256-pinned official release tarball (the install step's
        # own comment carries the why: the source build was ~6m per cold leg
        # and its ~/.cargo/bin cache kept getting LRU-evicted) — pinned
        # first-party artifacts keep the SHA-pinned-actions supply-chain
        # posture. Assert the whole shape so a regression back to the
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
        self.assertIn("cargo +stable build -p hekma-conformance --bin fake_agent", ci)
        # hermes_shim (story 6-2) joins fake_agent in the stale-helper guard for
        # the identical reason — assert its rm + rebuild pair in BOTH jobs too.
        self.assertIn("rm -f target/debug/hermes_shim target/debug/hermes_shim.exe", ci)
        self.assertIn("cargo +stable build -p hekma-conformance --bin hermes_shim", ci)
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
            "cargo +stable build -p hekma-conformance --bin fake_agent",
            coverage_step,
        )
        # hermes_shim's pair is asserted in the coverage job's own script too.
        self.assertIn(
            "rm -f target/debug/hermes_shim target/debug/hermes_shim.exe",
            coverage_step,
        )
        self.assertIn(
            "cargo +stable build -p hekma-conformance --bin hermes_shim",
            coverage_step,
        )
        # The pinned-prebuilt install: exact version + per-OS sha256 pins +
        # the release-tarball URL + the hash gate. The old floating source
        # install and the obsolete nextest-bin cache key must both stay GONE
        # (that cache was the eviction victim that forced the ~6m rebuilds).
        self.assertIn('NEXTEST_VERSION: "0.9.149"', ci)
        self.assertIn(
            "cargo-nextest-${NEXTEST_VERSION}-${art}.tar.gz",
            ci,
        )
        self.assertIn('sha256sum "$tmp" 2>/dev/null || shasum -a 256 "$tmp"', ci)
        # The Windows path normalization: Git Bash's $RUNNER_TEMP is a
        # Windows drive-letter path whose colon GNU tar parses as a REMOTE
        # HOST spec ("Cannot connect to D:") — without this guard the
        # extract can never succeed on windows-latest. Digest marker strip:
        # MSYS sha256sum prefixes digests of backslash paths with '\'.
        self.assertIn('tmp="$(cygpath -u "$RUNNER_TEMP")/cargo-nextest.tar.gz"', ci)
        self.assertIn('got="${got#\\\\}"', ci)
        self.assertNotIn("cargo +stable install cargo-nextest", ci)
        self.assertNotIn("${{ runner.os }}-cargo-nextest-bin", ci)
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
        # The per-crate tarpaulin invocation shape (llvm + skip-clean + the
        # per-crate --timeout budget + verbose preserved; -p "$pkg" + Lcov out
        # into cov/$pkg replaces --workspace).
        # Whitespace-normalized (line-continuation backslashes and indentation
        # collapsed): the 2026-09-28 bounded retry loop (#239) wraps the
        # invocation in an `if`, which necessarily re-indents the continuation
        # lines — the STRUCTURE (the full flag sequence, in order) is the pin,
        # not the column it starts at.
        ci_flat = " ".join(ci.replace("\\\n", " ").split())
        self.assertIn(
            'cargo +stable tarpaulin --engine llvm --skip-clean --timeout "$tarp_timeout" '
            "--verbose --exclude-files 'crates/hekma-conformance/src/bin/*' "
            "--exclude-files 'crates/hekma-engine/examples/*' "
            '-p "$pkg" --out Lcov --output-dir "cov/$pkg"',
            ci_flat,
        )
        # Per-crate tarpaulin budget (#334): --timeout is tarpaulin's
        # PER-TEST-BINARY cap, and the engine's wall-clock-bound integration
        # suites (exit_codes/drain/liveness/readiness, serial under
        # RUST_TEST_THREADS=1) outgrew the flat 180 s — killed at exactly
        # 180 s mid-suite, deterministically, on 8/8 attempts across two
        # stacked PRs. The case arm is the budget: the engine gets 420 s,
        # every other crate keeps 180 s. Pin BOTH arms so a future edit
        # cannot silently drop the engine budget (back to the deterministic
        # overrun) or inflate the default for every crate.
        self.assertIn('case "$pkg" in', ci)
        self.assertIn("hekma-engine) tarp_timeout=420 ;;", ci)
        self.assertIn("*) tarp_timeout=180 ;;", ci)
        # The exact six workspace crates, lightest → heaviest with hekma-engine
        # LAST (so a heavy-crate OOM still leaves the lighter crates' numbers
        # logged). Story 17-2 added hekma-host before the engines.
        self.assertIn(
            "set -- hekma-adapter-api hekma-conformance hekma-adapters-hermes "
            "hekma-host hekma hekma-engine",
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
        # out to. --timeout "$tarp_timeout" lifts tarpaulin's 60 s per-test-binary
        # default (per-crate budget: 180 s, 420 s for the engine — see the
        # case-arm pin above) so a heavy survival suite under instrumentation is
        # not killed spuriously.
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

    def test_docs_probe_workflow_watches_every_published_page(self) -> None:
        # A scheduled probe watches the DEPLOYED docs site (Cloudflare Pages,
        # built from the ktesio.dev repository) because a silent deploy death
        # went unnoticed for two weeks (Aug 2026) while repo CI stayed green.
        # Pin the shape: schedule + manual dispatch, no `needs:` coupling to
        # CI, every page in the SITE'S OWN sitemap probed for HTTP 200 with
        # each failure NAMED (never fail-fast), an empty or missing sitemap is
        # itself a failure, and two title markers so a 200-from-an-error-page
        # cannot false-pass.
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
        # The page list is the site's own sitemap, read at probe time. The
        # docs moved out of this repository (2026-10-22), so docs/meta.json
        # must not drive the probe again.
        self.assertIn('"$site/sitemap.xml"', probe)
        self.assertIn("grep -o '<loc>[^<]*</loc>'", probe)
        self.assertNotIn("jq -r", probe)
        self.assertNotIn("<<< 'docs/meta.json'", probe)
        self.assertFalse(
            [line for line in probe.splitlines()
             if "docs/meta.json" in line and not line.strip().startswith("#")],
            "docs/meta.json must not drive the probe",
        )
        self.assertNotIn("actions/checkout", probe)
        # An unreachable or near-empty sitemap fails instead of passing on an
        # empty list.
        self.assertIn("could not be fetched", probe)
        self.assertIn('if [ "$docs_count" -lt 10 ]; then', probe)
        # Every page must answer 200, and each failure is named.
        self.assertIn('site="https://hekma.ktesio.dev"', probe)
        self.assertIn("hekma.ktesio.dev page is DOWN", probe)
        self.assertIn("expected 200", probe)
        self.assertNotIn("docs.ktesio.dev", probe)
        # Title markers on the landing page and the docs home.
        self.assertIn('"<title>Hekma: the control plane for agent workloads</title>"', probe)
        self.assertIn('"<title>Hekma docs</title>"', probe)
        # The legacy URLs baked into published release bodies and the
        # deprecated crates' READMEs must keep 301ing to the exact target,
        # including the maintainer pages that moved back to this repository.
        self.assertIn("/migration $mount/migration/", probe)
        self.assertIn("/release-notes $mount/release-notes/", probe)
        self.assertIn("/testing $repo_docs/testing.md", probe)
        self.assertIn("legacy URL broke", probe)
        # The installer URLs keep serving.
        self.assertIn('"https://cli.ktesio.dev/hekma/install.sh"', probe)

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

        self.assertIn("os: [ubuntu-26.04, macos-latest, windows-latest]", ci)
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
        # Clean shape: build test binaries once (--no-run), ONE unified run for
        # all OSes, doctests separate.
        self.assertIn(
            "cargo +stable nextest run --workspace --all-targets --no-run", ci
        )
        self.assertIn("- name: Run tests (nextest)", ci)
        self.assertIn("cargo +stable nextest run --workspace --all-targets", ci)
        self.assertIn("cargo +stable test --workspace --doc", ci)
        # Wedge forensics (2026-10-17): the ubuntu leg's RUN is wall-clock
        # capped AND session-quarantined. The cap alone was NOT enough: the
        # x86-ubuntu wedge family ignores even SIGKILL, and `timeout(1)`
        # WAITS for its child after signalling, so it could not enforce the
        # cap either (run 39044536797: the 30m step cap fired, the step was
        # still in_progress at +38m, the 40m JOB timeout fired, and the
        # service force-cancelled at exactly +45m — discarding the whole
        # job-log blob and leaving the post-mortem step pending). The shipped
        # shape instead runs the suite double-forked into its OWN session
        # (`( setsid ... & )` re-parents it to init, outside the step shell's
        # process group AND descendant chain) while the step shell SUPERVISES
        # by polling an exit-code file. FALSIFIED FOR CLASS 2 (2026-10-17,
        # runs 39048612882/39048623940, jobs 117442316042/117442346937):
        # with the quarantine live the run step was STILL in_progress at the
        # exact +45:00 service force-cancels — the 30m supervisor cap never
        # fired and both log blobs came back BlobNotFound, so the active
        # wedge FREEZES THE RUNNER ITSELF (a proven-correct bash poll loop
        # never reached its deadline; the mechanism was dev-box-smoked). The
        # quarantine still bounds class 1 (D-state suite on a live runner):
        # a healthy end replays the suite's exit code; a cap
        # expiry fails the step (124) with the captured suite output + a
        # live process snapshot in the step log, and the always-on
        # post-mortem snapshot step then captures the still-live quarantined
        # tree (stat/wchan names a D-state leftover's kernel wait channel)
        # plus a PSI readout, uploaded as a per-OS artifact. Class 2 is
        # bounded by the ENVIRONMENT instead: the linux leg pins
        # ubuntu-26.04 and its JOB timeout tightens via the same
        # matrix data (recalibrated 15m→25m on 2026-10-31 after the
        # lane outgrew the cap — a freeze recurrence costs the 25+5m
        # service window, not 45m). The cap rides
        # the matrix `include` as DATA (not an in-step OS comparison). Pin
        # the deliberate shape so a future edit cannot silently drop the
        # observability, loosen the fast-fail timeout, or resurrect the
        # un-enforceable timeout(1) form.
        self.assertIn("nextest_wallclock_cap: 30m", ci)
        self.assertIn("setsid bash -c", ci)
        self.assertIn('echo $? > "$exitf"', ci)
        self.assertIn("[supervisor] WALL-CLOCK CAP HIT", ci)
        self.assertNotIn("--kill-after=", ci)
        self.assertIn("- name: Wedge post-mortem snapshot", ci)
        self.assertIn("name: wedge-postmortem-${{ matrix.os }}", ci)
        self.assertIn("- os: ubuntu-26.04", ci)
        self.assertIn("test_job_timeout_minutes: 25", ci)
        # Default test-job budget 40→50 (2026-10-20): the suite passed 1,671
        # nextest tests with epic-17 and the windows-latest leg rides 33-38m
        # green; run 39472620931 lost ~9m to a double cold-cache miss (new
        # Cargo.lock key + an evicted Windows-cargo-nextest-bin that rebuilt
        # nextest from source) and the 40m job timeout cancelled the suite
        # mid-run. 50 keeps cold-cache headroom; ubuntu rides 25m via the
        # matrix include above (recalibrated from 15m on 2026-10-31 — green
        # runs had reached 653–818s of job time and run 41193980277 capped
        # out at 15m mid-suite with a healthy runner).
        self.assertIn(
            "timeout-minutes: ${{ matrix.test_job_timeout_minutes || 50 }}", ci
        )
        self.assertIn('status-level = "leak"', nextest)
        # The SOURCE fix: a max-threads=1 test-group over exactly the engine
        # integration binaries, with the existing retries + slow-timeout untouched.
        self.assertIn("[test-groups.engine-integration-serial]", nextest)
        self.assertIn("max-threads = 1", nextest)
        self.assertIn('filter = "kind(test) & package(hekma-engine)"', nextest)
        self.assertIn('test-group = "engine-integration-serial"', nextest)
        # The 2026-10-17 extension: the CLI integration binary (agent_cli) joins
        # the same serialized group after PR trees carrying #281's supervision
        # legs + the raw-follow rewrite wedged the 2-core ubuntu runner into the
        # #106 D-state family (3/3 job-timeout cancels; macOS/Windows green).
        self.assertIn('filter = "kind(test) & package(hekma)"', nextest)
        # Probe 2 of the same wedge: with both integration surfaces serialized
        # the lane STILL wedged (8/8 overnight, main trees included) — #281 also
        # grew process-spawning at kind(lib) (supervisor/tests.rs release-pass
        # legs SIGSTOP/SIGCONT-ing live detached fake_agents), which the
        # kind(test) filters never covered. The lib unit tests join the group.
        self.assertIn('filter = "kind(lib) & package(hekma-engine)"', nextest)
        self.assertIn("retries = 2", nextest)
        self.assertIn("slow-timeout = ", nextest)
        # The temporary bisect scaffold and the watchdog/off-pipe machinery are GONE
        # (no per-OS special-casing, no per-binary steps, no in-step capture hacks).
        self.assertNotIn("ubuntu bisect:", ci)
        self.assertNotIn("binary_id(=hekma-engine::", ci)
        self.assertNotIn("RUNLOG", ci)
        self.assertNotIn("kill -USR1 $$", ci)
        self.assertNotIn("matrix.os == 'ubuntu-latest'", ci)
        self.assertNotIn("matrix.os != 'ubuntu-latest'", ci)

    def test_ci_windows_leg_runs_the_parallel_heavy_nextest_profile(self) -> None:
        # 2026-10-27: the `engine-integration-serial` group (the #106/#281
        # x86-ubuntu 2-core D-state wedge fix above) had grown the
        # windows-latest test step to 40m02s serialized (51m51s job, run
        # 40383210835) — past the 50m job budget on main (runs 40355871363
        # and 40337860524 both cancelled at the windows timeout). The wedge
        # never occurred on windows/macos (parallelism proven green there
        # pre-#106), so the windows leg selects the `parallel-heavy`
        # profile: same suite/settings, the three serialization overrides
        # SHADOWED by same-filter overrides routing to an unbounded
        # test-group (nextest consults the SELECTED profile's overrides
        # first; an empty custom profile would inherit `default`'s
        # serialization — verified on cargo-nextest 0.9.148). Measured:
        # 12m15s step / 24m01s job (run 40459586527). Lock the per-leg
        # selection (matrix DATA, never a per-OS step branch), the single
        # uniform invocation, and the shadow-pair equality with the
        # serialized filters.
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        nextest = (release_docs.ROOT / ".config" / "nextest.toml").read_text(
            encoding="utf-8"
        )
        # Selection rides the matrix include as DATA: every leg gets a
        # profile, windows diverges, exactly the other two stay `default`.
        self.assertIn("nextest_profile: default", ci)
        self.assertEqual(ci.count("nextest_profile: default"), 2)
        self.assertIn("nextest_profile: parallel-heavy", ci)
        self.assertEqual(ci.count("nextest_profile: parallel-heavy"), 1)
        self.assertIn('PROFILE="${{ matrix.nextest_profile }}"', ci)
        # The run step stays ONE uniform invocation for all OSes: both the
        # capped and uncapped paths read the same matrix var, and no step
        # branches on matrix.os (the bisect-era guards above).
        self.assertIn('--profile "$PROFILE"', ci)
        self.assertEqual(ci.count('--profile "$PROFILE"'), 2)
        self.assertNotIn("--profile parallel-heavy", ci)
        # The unbounded group + shadow profile exist.
        self.assertIn("[test-groups.unbounded]", nextest)
        self.assertIn("max-threads = 2147483647", nextest)
        self.assertIn("[profile.parallel-heavy]", nextest)
        # Shadow-pair equality: each serialized filter appears EXACTLY
        # twice — once under `default`'s overrides (serialized group) and
        # once under `parallel-heavy`'s (unbounded shadow) — and the three
        # shadows route to the unbounded group. The equality IS the
        # contract: a dropped or drifted shadow silently re-serializes the
        # windows leg (or shadows a filter nothing serializes).
        for serialized_filter in (
            'filter = "kind(test) & package(hekma-engine)"',
            'filter = "kind(test) & package(hekma)"',
            'filter = "kind(lib) & package(hekma-engine)"',
        ):
            self.assertEqual(nextest.count(serialized_filter), 2)
        self.assertEqual(nextest.count('test-group = "unbounded"'), 3)
        # The wedge's home leg keeps the serialized default.
        self.assertIn("- os: ubuntu-26.04", ci)
        self.assertIn("nextest_wallclock_cap: 30m", ci)

    def test_ci_enforces_workspace_boundary_and_semver_gates(self) -> None:
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )

        # Stable jobs select +stable so the root rust-toolchain.toml pin (MSRV
        # 1.96.1) does not silently redirect them off latest stable (AI-17).
        self.assertIn("cargo +stable check -p hekma", ci)
        self.assertIn("cargo +stable tree -p hekma -e normal,build --all-features", ci)
        # Boundary gate is an allowlist: only these internal edges may exist.
        self.assertIn("hekma-(engine|adapter-api|adapters-hermes|host)", ci)
        # OS-cfg gate uses the broadened class pattern (compound cfg forms).
        self.assertIn("cfg[!(]?.*(unix|windows|target_os|target_family)", ci)
        self.assertIn("crates/hekma-engine/src/backends/", ci)
        # OS-cfg allowlist covers honestly-unix-gated engine integration tests
        # (AI-35 disclosure convention) alongside the backends home. Assert the
        # FULL allowlist LINE shape (review blind-12): a bare substring would
        # also match a stale comment quoting the pattern, so a narrowed
        # allowlist (e.g. a dropped legacy-file entry) must fail here. The
        # 2026-09-22 hardening narrowed the engine-tests entry from the whole
        # tests/ directory to the TWO files that genuinely use OS-cfg
        # (atomic_config_writes.rs, memory.rs), each justified in the step
        # comment — the pin follows the new line exactly. Story 15-3 added
        # the conformance tck.rs entry (the EngineOpen drive's unix-only
        # unwritable-root shape, justified in the step comment). Story 16-1
        # added the conformance test_notes.rs entry (the uncaptured-note
        # helper's raw fd/handle write arms — platform-only imports that
        # cannot be data-driven, justified in the step comment). Story 17-2
        # added the hekma-host signals.rs entry (tokio's signal-stream
        # registration is unix/windows-cfg'd inside the tokio crate itself;
        # the host's SIGTERM wait cannot be written data-driven). Story 17-3
        # added the hekma-host control_api.rs entry (the Control-API suite
        # WRITES a 0600 secrets.toml so the engine's secrets-file leg resolves
        # — PermissionsExt is the OS API boundary there, the same sanctioned
        # class as the engine's atomic_config_writes.rs; the suite's
        # Windows-skip itself is data-driven OsId::current, never cfg) and —
        # review item 24b — the hekma serve_cli.rs entry (the real-binary
        # Control-API leg writes the SAME 0600 secrets file). Story 17-4
        # added the hekma-host binary_flags.rs entry (the binary's
        # --log-capture e2e writes the SAME 0600 secrets file — the
        # identical sanctioned class; the leg's Windows skip stays
        # data-driven OsId::current, never cfg).
        self.assertIn(
            r"allowlist='^crates/hekma-engine/src/backends/"
            r"|^crates/hekma/src/update_check\.rs:"
            r"|^crates/hekma/src/cli/self_update\.rs:"
            r"|^crates/hekma-engine/tests/atomic_config_writes\.rs:"
            r"|^crates/hekma-engine/tests/memory\.rs:"
            r"|^crates/hekma-conformance/src/tck\.rs:"
            r"|^crates/hekma-conformance/src/test_notes\.rs:"
            r"|^crates/hekma-conformance/src/test_support\.rs:"
            r"|^crates/hekma-host/src/signals\.rs:"
            r"|^crates/hekma-host/tests/control_api\.rs:"
            r"|^crates/hekma-host/tests/binary_flags\.rs:"
            r"|^crates/hekma/tests/serve_cli\.rs:'",
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
            r"allowlist='^crates/hekma-engine/src/domain/cost\.rs:"
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
        # RENAME-AWARE BASELINE STRATEGY (v0.8.0 Hekma migration): the
        # pre-rename freeze baselines cannot serve same-name baseline diffs,
        # so until fresh baselines are pinned to the migration's main-side
        # merge commit (post-merge; update ci.yml + this file together),
        # scripts/rename_surface_check.sh compiles an external-consumer
        # fixture against BOTH the baseline rev's old-named crate and HEAD's
        # renamed crate. Pins: both invocations + their freeze SHAs.
        self.assertIn(
            "bash scripts/rename_surface_check.sh \\",
            ci,
        )
        self.assertIn(
            "4119db37b5288b990144d28f995ee14a69271b5e \\\n"
            "            crates/hekma-adapter-api \\\n"
            "            ktesio-adapter-api hekma-adapter-api",
            ci,
        )
        self.assertIn(
            "49da96b6f1c695edd6afd379526deba42cf6da60 \\\n"
            "            crates/hekma-engine \\\n"
            "            ktesio-engine hekma-engine",
            ci,
        )
        self.assertIn(
            "fixtures/external-consumers/adapter-api-consumer/src/main.rs", ci
        )
        self.assertIn(
            "fixtures/external-consumers/engine-consumer/src/main.rs", ci
        )
        # The BOTH-GRAPH consumer (ktesio-engine 0.3.0 + hekma-engine in one
        # registry-resolved graph) arms at first hekma-engine publish.
        self.assertIn(
            "(cd fixtures/external-consumers/both-graph", ci
        )
        # The DEPRECATION SHIMS (one-shot ktesio-* final versions under
        # deprecated/) compile-check against the published hekma-* crates,
        # armed at first publish like the both-graph consumer.
        self.assertIn("for shim in deprecated/ktesio-engine deprecated/ktesio-adapter-api deprecated/ktesio-adapters-hermes; do", ci)
        # Story 7-4 arms the SAME mechanism for hekma-engine, against a
        # DIFFERENT honest freeze point (NOT the contract freeze 4119db3 —
        # the engine's unpublished surface legitimately grew after the
        # contract froze; a 4119db3 baseline fails major lints on that
        # honest history). Current baseline: 49da96b, the epic-12 merge
        # (2026-09-15) — the latest deliberate post-freeze surface
        # extension (`EngineError::DetachRefused` + `SpawnSpec.detach`,
        # announced in PR #184; the prior extension was epic-11's
        # `EngineError::ResumeUnsupported`, AI-7, PR #181) superseded the
        # embedding-surface freeze (armed at 7-3's 8a8b328, pinned to
        # 12f3aab per the main-history constraint). Both baseline revs get
        # the cat-file resolvability check so a history rewrite is a CLEAR
        # infra error. Maintenance: bump all four pins (two here, two in
        # ci.yml) together at the next deliberate freeze.

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
            "three armed check-release sites: the fresh post-rename "
            "baselines pinned to 1e71389 — the PR #193 epic-14 squash "
            "(2026-09-22) — for hekma-adapter-api + hekma-engine, "
            "plus the crates.io release-to-release loop; "
            f"found {check_release_sites}",
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

    def test_config_version_mirror_across_the_cli_and_host_crates(self) -> None:
        # Review item 14: the Control API serves the SAME config-get document
        # the CLI does, so its schema-version constant must STAY EQUAL to the
        # CLI's. The two live in different crates (the CLI crate is not a host
        # dependency — AD-2), so the host cannot see the CLI's constant at
        # compile time; the dto.rs re-declaration documents this pin as the
        # mirror's enforcement, and THIS source scan is it: both declarations
        # are read from source and compared, so drift fails the automation
        # run (not a doc claim that greps nothing).
        cli_source = (release_docs.ROOT / "crates/hekma/src/cli/agent.rs").read_text(
            encoding="utf-8"
        )
        host_source = (
            release_docs.ROOT / "crates/hekma-host/src/control/dto.rs"
        ).read_text(encoding="utf-8")

        def declared_version(source: str, marker: str) -> int:
            for line in source.splitlines():
                stripped = line.split("//", 1)[0].strip()
                if stripped.startswith(marker) and "=" in stripped:
                    value = stripped.rsplit("=", 1)[1].strip().rstrip(";")
                    return int(value)
            raise AssertionError(f"no {marker} declaration found in source")

        cli_version = declared_version(cli_source, "const CONFIG_GET_SCHEMA_VERSION")
        host_version = declared_version(host_source, "pub(crate) const CONFIG_GET_SCHEMA_VERSION")
        self.assertEqual(
            cli_version,
            host_version,
            "the Control API's config document version drifted from the CLI's "
            "config-get version — the two surfaces render the SAME document "
            "and must stamp it identically",
        )

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
            / "hekma-engine"
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
            "cargo +stable build --release --example embedding-quickstart -p hekma-engine",
            build_job,
        )
        # The RUN step is guarded: a step-level timeout independent of the
        # agent's own idle cap, and an existence assert with a clear error so
        # a build-step regression fails loudly instead of "file not found".
        self.assertIn("timeout-minutes: 5", build_job)
        self.assertIn("test -x target/release/examples/embedding-quickstart", build_job)
        self.assertIn("target/release/examples/embedding-quickstart", build_job)
        self.assertTrue(example.is_file(), "the quickstart example must exist")

    def test_ci_stand_in_host_lists_every_all_covers_tag(self) -> None:
        # Story 19-1 review round 1: the governance-vectors job's stand-in-Host
        # step re-lists the D5 rule tags as a python literal — a NINTH tag added
        # to the Rust `ALL_COVERS` const would silently miss it and the CI
        # check would under-verify. This pin greps the const from the engine
        # source, resolves each variant to its exact kebab-case wire form via
        # the `as_str` mapping (so the case conversion can never drift), and
        # asserts every wire string appears in the ci.yml literal: the two
        # lists cannot diverge.
        governance_src = (
            release_docs.ROOT
            / "crates"
            / "hekma-engine"
            / "src"
            / "domain"
            / "governance.rs"
        ).read_text(encoding="utf-8")

        import re as _re

        # The `ALL_COVERS` const's variant list …
        const_body = governance_src.split("pub const ALL_COVERS", 1)[1].split("];", 1)[0]
        variants = _re.findall(r"CoverTag::(\w+)", const_body)
        self.assertTrue(variants, "the ALL_COVERS const must list CoverTag variants")
        # … and the `as_str` mapping variant → exact wire string (single source
        # of the kebab-case form; the test re-derives, never re-states it).
        as_str_body = governance_src.split("pub fn as_str(&self)", 1)[1].split("}\n}", 1)[0]
        wire_by_variant = dict(_re.findall(r"CoverTag::(\w+) => \"([\w-]+)\"", as_str_body))
        missing_mappings = [v for v in variants if v not in wire_by_variant]
        self.assertEqual(
            [],
            missing_mappings,
            "every ALL_COVERS variant needs an as_str wire form",
        )
        tags = [wire_by_variant[v] for v in variants]

        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        job_start = ci.index("  governance-vectors:")
        next_job = _re.search(r"\n  [A-Za-z_-]", ci[job_start + 1 :])
        job_end = job_start + 1 + next_job.start() if next_job else len(ci)
        job = ci[job_start:job_end]
        for tag in tags:
            self.assertIn(
                f'"{tag}"',
                job,
                f"the stand-in-Host literal must list the `{tag}` rule "
                "(keep ci.yml in lockstep with the Rust ALL_COVERS const)",
            )
        # The job's header sanity-checks stay present: a Host pins the
        # schema version + vectors version.
        self.assertIn('document["schema_version"]', job)
        self.assertIn('document["vectors_version"]', job)
        # Story 19-2: the stand-in Host must also require every case KIND the
        # Rust `Case` enum serializes (a fourth kind added in Rust without the
        # CI check would go unverified). The variants are read from the enum
        # and kebab-cased the way its `rename_all = "kebab-case"` does.
        case_body = governance_src.split("pub enum Case {", 1)[1].split("\n}", 1)[0]
        case_variants = _re.findall(r"^    (\w+)\(", case_body, _re.MULTILINE)
        self.assertTrue(case_variants, "the Case enum must list its variants")
        for variant in case_variants:
            kind = _re.sub(r"(?<!^)(?=[A-Z])", "-", variant).lower()
            self.assertIn(
                f'kinds.get("{kind}")',
                job,
                f"the stand-in Host must require the `{kind}` case kind",
            )

    def test_ci_perf_budgets_job_runs_the_harness_blocking(self) -> None:
        # Story 7-5 (NFR-4): the designated perf job runs the perf-budgets
        # harness on every PR and GATES the measured budgets — reads p99 and
        # RSS/instance gate hard; CPU/instance carries the harness's
        # documented shared-runner tolerance over a median-of-3 window in CI.
        # Story 15-5: the run also carries --baseline against the COMMITTED
        # canonical baseline (benches/baselines/perf-ubuntu-x86_64.json), so
        # the leg ratchet-gates (no gated metric worse than baseline x 1.15,
        # additive to the absolute budgets); the committed baseline file
        # itself must exist.
        # Guard the whole shape so the gate cannot silently drop out: the job
        # exists, builds the release example, carries the stale-helper rm +
        # rebuild guard (the harness SPAWNS fake_agent, so the test/coverage
        # -job rule "any job that spawns agents carries it" applies), runs
        # the binary BLOCKING (the run block's last line IS the bare binary
        # invocation + its flags, and the block contains no
        # failure-swallowing idiom), uploads the JSON report artifact, and
        # the harness example file itself exists.
        ci = (release_docs.ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        example = (
            release_docs.ROOT
            / "crates"
            / "hekma-engine"
            / "examples"
            / "perf-budgets.rs"
        )
        baseline = (
            release_docs.ROOT / "benches" / "baselines" / "perf-ubuntu-x86_64.json"
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
            "cargo +stable build --release -p hekma-engine --example perf-budgets",
            perf_job,
        )
        # The stale-helper guard: rm + explicit rebuild for the fake_agent
        # this harness spawns (same defect class the test job hit twice).
        self.assertIn(
            "rm -f target/release/fake_agent target/release/fake_agent.exe", perf_job
        )
        self.assertIn(
            "cargo +stable build --release -p hekma-conformance --bin fake_agent",
            perf_job,
        )
        # Blocking, pinned structurally beyond substrings: the RUN step's
        # block must END on the bare binary invocation followed ONLY by
        # flag/value tokens — a flags-only suffix (review round 1
        # re-tightening: the old `(?:\s.*)?` accepted ANY trailing text, so
        # a `| tee`, `&&`, or redirect suffix could swallow the harness's
        # nonzero exit again) — and must contain no failure-swallowing
        # idiom. The `set -euo pipefail` line carries the step's real
        # protection and is pinned in its own right.
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
            last_line,
            r"^\s*target/release/examples/perf-budgets"
            r"(?:\s(?:--\S+|[^\s|;&><`$()]+))*$",
        )
        self.assertIn("set -euo pipefail", run_block)
        # The ratchet invocation itself is pinned, not merely permitted: the
        # run gates against the committed canonical baseline (story 15-5).
        self.assertIn(
            "--baseline benches/baselines/perf-ubuntu-x86_64.json", run_block
        )
        self.assertNotIn("|| true", run_block)
        self.assertNotIn("exit 0", run_block)
        # Story 17-1: the serve-shape scenario is its OWN blocking run step
        # (gated budgets CI never runs are not gates) with the SAME
        # no-swallow contract — pinned structurally like the default step:
        # flags-only last line (EXACTLY `--serve-shape`, no baseline — the
        # canonical baseline carries no serve metrics and the serve gates
        # bind only on serve runs), `set -euo pipefail`, no failure
        #-swallowing idiom, and its own report-path env var.
        self.assertIn("- name: Run the serve-shape scenario", perf_job)
        serve_step = perf_job.index("- name: Run the serve-shape scenario")
        serve_text = perf_job[perf_job.index("run: |", serve_step) :]
        next_serve_step = _re.search(r"\n      - name:", serve_text[1:])
        serve_block = (
            serve_text[: 1 + next_serve_step.start()] if next_serve_step else serve_text
        )
        serve_last_line = next(
            line for line in reversed(serve_block.splitlines()) if line.strip()
        )
        self.assertEqual(
            serve_last_line.strip(),
            "target/release/examples/perf-budgets --serve-shape",
            "the serve run's last line is the bare binary + its flag — no "
            "redirection, no chaining",
        )
        self.assertIn("set -euo pipefail", serve_block)
        self.assertNotIn("|| true", serve_block)
        self.assertNotIn("exit 0", serve_block)
        self.assertNotIn(
            "--baseline", serve_block, "the serve run does not ratchet"
        )
        # The serve step's OWN env slice (review round 3: scoped to the step,
        # not the whole job — the default step's report path must not satisfy
        # this assertion): the serve run writes its report file for the
        # shared artifact.
        serve_upload = perf_job.index("- name: Upload the measured report", serve_step)
        serve_step_slice = perf_job[serve_step:serve_upload]
        self.assertIn(
            "PERF_BUDGETS_REPORT_PATH: perf-budgets-serve-report.json",
            serve_step_slice,
            "the serve step writes its own report file (perf-budgets-serve-report.json)",
        )
        # The measured record is also uploaded as the job's artifact (even on
        # a red run): the harness writes it via the env var, the upload step
        # publishes it, and the upload is not skipped on failure.
        self.assertIn("PERF_BUDGETS_REPORT_PATH: perf-budgets-report.json", perf_job)
        self.assertIn("actions/upload-artifact@", perf_job)
        upload = perf_job[perf_job.index("- name: Upload the measured report") :]
        self.assertIn("if: always()", upload)
        self.assertIn("actions/upload-artifact@", upload)
        self.assertIn("perf-budgets-report.json", upload)
        self.assertIn(
            "perf-budgets-serve-report.json",
            upload,
            "story 17-1: ONE artifact carries both report files (default + serve)",
        )
        self.assertTrue(example.is_file(), "the perf-budgets harness example must exist")
        self.assertTrue(
            baseline.is_file(),
            "the committed canonical perf baseline must exist (the CI run "
            "gates its ratchet against it)",
        )

    def test_publish_flags_match_the_post_v070_release_state(self) -> None:
        """Publish-day state (go recorded 2026-09-09, release issue #176):
        the three library crates are PUBLISHED (no publish=false) and
        hekma-conformance KEEPS its flag (dev/test kit, separate decision).
        This pin now catches an accidental re-add of publish=false to the
        published crates, or a flip of conformance's flag."""
        for crate in ("hekma-adapter-api", "hekma-adapters-hermes", "hekma-engine"):
            manifest = (release_docs.ROOT / "crates" / crate / "Cargo.toml").read_text(
                encoding="utf-8"
            )
            self.assertNotIn("publish = false", manifest, crate)
            self.assertNotIn("publish.workspace", manifest, crate)
        conformance = (release_docs.ROOT / "crates" / "hekma-conformance" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        # The dev/test kit KEEPS its flag — publishing it is a separate,
        # undecided decision (docs/release-process.md decision log).
        self.assertIn("publish = false", conformance, "hekma-conformance")

    def test_conformance_git_pin_points_at_a_real_hekma_conformance_rev(self) -> None:
        # The third-party dev-dependency examples pin a git rev because the
        # kit is not published yet; the rev MUST name a commit where the crate
        # actually exists as `hekma-conformance` (post-rename). The pre-rename
        # commit 20ddc204… only has `ktesio-conformance`, so a copy of the
        # README snippet failed to resolve. Current pin: the v0.9.0 release
        # commit — update deliberately (same rule as the snippet's comment).
        pin = "14fbf772710e9151f7a90be8d504a4f67bd95531"
        for rel in ("README.md", "docs/testing.md"):
            doc = (release_docs.ROOT / rel).read_text(encoding="utf-8")
            self.assertIn(
                f'hekma-conformance = {{ git = "https://github.com/Ktesio/hekma", rev = "{pin}" }}',
                doc,
                rel,
            )
        # The stale pre-rename rev must not creep back into either example.
        for rel in ("README.md", "docs/testing.md"):
            doc = (release_docs.ROOT / rel).read_text(encoding="utf-8")
            self.assertNotIn("20ddc204403a5c412e0e3249d4609dd47c30854e", doc, rel)

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

    def fake_hekma(self, directory: Path, output: str = "hekma 0.8.0") -> Path:
        path = directory / "hekma"
        path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{output}'\n", encoding="utf-8")
        path.chmod(0o755)
        return path

    def test_install_sh_migrates_a_legacy_kt_in_place_with_a_note(self) -> None:
        # D7 clean break: a legacy `kt` is recognized as OURS, the manual
        # channel installs hekma+hkm BESIDE it, and (outside dry-run) a
        # visible retirement note names the old binary. In dry-run the
        # planned install names both binaries and the existing directory.
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

        self.assertIn("(hekma + hkm)", result.stdout)
        self.assertIn(f"to {bin_dir}", result.stdout)

    def test_install_sh_updates_an_existing_hekma_manual_install(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bin_dir = Path(tmp) / "bin"
            bin_dir.mkdir()
            hekma_path = self.fake_hekma(bin_dir)
            result = self.run_install_sh(
                {
                    "KTESIO_INSTALL_TEST_KT_PATH": str(hekma_path),
                    "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                }
            )

        self.assertIn(f"to {bin_dir}", result.stdout)

    def test_install_sh_prefers_homebrew_for_new_installs(self) -> None:
        result = self.run_install_sh({"KTESIO_INSTALL_TEST_HAS_BREW": "1"})

        self.assertIn("DRY RUN: brew install ktesio/tap/hekma", result.stdout)

    def test_install_sh_uses_cargo_when_homebrew_is_unavailable(self) -> None:
        result = self.run_install_sh({"KTESIO_INSTALL_TEST_HAS_CARGO": "1"})

        self.assertIn("DRY RUN: cargo install hekma --force", result.stdout)

    def test_install_sh_uses_prebuilt_binary_without_package_managers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_install_sh({"HOME": tmp})

        self.assertIn(
            "DRY RUN: install prebuilt x86_64-unknown-linux-gnu (hekma + hkm)",
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

        self.assertIn("DRY RUN: brew upgrade ktesio/tap/hekma", result.stdout)

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

        self.assertIn("DRY RUN: cargo install hekma --force", result.stdout)

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

        # The manual migration installs BOTH binaries into the existing
        # install DIRECTORY (beside the retired kt).
        self.assertIn(f"to {bin_dir}", result.stdout)

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
        self.assertIn("HEKMA_INSTALL_DIR", output)
        self.assertIn("KTESIO_INSTALL_DIR", output)

    def test_install_sh_rejects_unsupported_binary_target_without_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_install_sh(
                {"HOME": tmp, "KTESIO_INSTALL_TEST_ARCH": "aarch64"},
                expect_success=False,
            )

        self.assertIn("No prebuilt Hekma binary is available", result.stderr)

    def test_install_sh_refuses_non_ktesio_kt_conflict(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            kt_path = self.fake_kt(Path(tmp), "not ktesio")
            result = self.run_install_sh(
                {"KTESIO_INSTALL_TEST_KT_PATH": str(kt_path)},
                expect_success=False,
            )

        self.assertIn("Refusing to overwrite non-Ktesio command", result.stderr)


class WorkflowScriptSyntaxTests(unittest.TestCase):
    """Every `run: |` shell block embedded in the workflows must parse.

    A malformed embedded script passed all content pins and only exploded
    in CI (the v0.9.0-era semver job: an insertion landed inside a case
    statement, `syntax error near unexpected token '||'`). This gate runs
    `bash -n` on every sh/bash block so syntax breaks fail locally."""

    WORKFLOWS = (
        "ci.yml",
        "release.yml",
        "docs-probe.yml",
        "migration-matrix.yml",
    )

    def test_every_embedded_shell_run_block_parses(self) -> None:
        checked = 0
        for workflow in self.WORKFLOWS:
            path = release_docs.ROOT / ".github" / "workflows" / workflow
            text = path.read_text(encoding="utf-8")
            lines = text.splitlines()
            for idx, line in enumerate(lines):
                stripped = line.strip()
                if stripped != "run: |":
                    continue
                indent = len(line) - len(line.lstrip())
                shell = "sh"
                for back in range(idx - 1, max(idx - 6, -1), -1):
                    back_line = lines[back].strip()
                    if back_line.startswith("shell:"):
                        shell = back_line.split(":", 1)[1].strip()
                        break
                    if back_line.startswith("- name:") or back_line.startswith("- uses:"):
                        break
                if "pwsh" in shell or "powershell" in shell:
                    continue
                block: list[str] = []
                for later in lines[idx + 1 :]:
                    later_indent = len(later) - len(later.lstrip())
                    if later.strip() and later_indent <= indent:
                        break
                    block.append(later[indent + 2 :] if later.startswith(" " * (indent + 2)) else later.lstrip())
                script = "\n".join(block)
                with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as handle:
                    handle.write(script)
                    temp_name = handle.name
                try:
                    result = subprocess.run(
                        ["bash", "-n", temp_name],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        check=False,
                    )
                finally:
                    os.unlink(temp_name)
                self.assertEqual(
                    0,
                    result.returncode,
                    f"{workflow} run block near '{lines[idx + 1].strip()[:60]}' "
                    f"does not parse: {result.stderr}",
                )
                checked += 1
        self.assertGreaterEqual(checked, 8, "expected to check at least the known shell blocks")


if __name__ == "__main__":
    unittest.main(verbosity=2)
