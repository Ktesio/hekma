#!/usr/bin/env bash
# Rename-aware semver surface check (v0.8.0 Hekma migration).
#
# WHY THIS EXISTS: the CI semver gate's in-repo freeze baselines
# (ktesio-adapter-api @ 4119db3, ktesio-engine @ 49da96b) contain the
# PRE-rename package names, so `cargo semver-checks check-release -p
# <new-name> --baseline-rev <old-rev>` cannot resolve a same-named
# baseline package at that revision — by construction, not by drift.
# Until fresh baselines are pinned to the migration's main-side merge
# commit (the same post-merge step PRs #181/#184 took for their freeze
# bumps; the repo squash-merges, so the merge SHA exists only after
# landing), THIS check holds the line: a fixed external-consumer fixture
# compiles against BOTH the baseline rev's old-named crate source AND
# the current tree's renamed crate. The renamed public surface is
# compatible iff BOTH builds succeed. No compatibility is ever claimed
# from a skipped check — if either variant fails to build, the gate
# fails loudly.
#
# Usage: rename_surface_check.sh <baseline-rev> <crate-dir> <old-name> <new-name> <fixture-src>
#   <baseline-rev>  main-history rev carrying the OLD crate (freeze point)
#   <crate-dir>     crate directory relative to the repo root, e.g. crates/hekma-engine
#                   (the OLD name's directory at the baseline rev is derived: the
#                   baseline worktree still has the old dir; we resolve it by package
#                   name via cargo metadata)
#   <old-name>      package name at the baseline rev (e.g. ktesio-engine)
#   <new-name>      package name in the current tree (e.g. hekma-engine)
#   <fixture-src>   path to the consumer fixture's main.rs
set -euo pipefail

baseline_rev="$1"
crate_dir="$2"
old_name="$3"
new_name="$4"
fixture_src="$5"

root="$(git rev-parse --show-toplevel)"
base="$(mktemp -d)"
worktree="$base/worktree"
cleanup() {
  git worktree remove --force "$worktree" >/dev/null 2>&1 || true
  rm -rf "$base"
}
trap cleanup EXIT

git -C "$root" cat-file -e "${baseline_rev}^{commit}" || {
  echo "::error::semver rename-surface baseline rev ${baseline_rev} is not resolvable (history rewrite or shallow clone?)."
  exit 1
}
git -C "$root" worktree add --quiet "$worktree" "$baseline_rev"

# Resolve the baseline rev's crate directory BY PACKAGE NAME (the
# directory was renamed in the current tree; the baseline still has the
# old one).
old_dir="$( (cd "$worktree" && cargo metadata --no-deps --format-version 1) |
  python3 -c 'import json,sys; m=json.load(sys.stdin); print(next(p["manifest_path"] for p in m["packages"] if p["name"]==sys.argv[1]), end="")' "$old_name" )"
old_dir="$(dirname "$old_dir")"
if [ -z "$old_dir" ] || [ ! -d "$old_dir" ]; then
  echo "::error::could not locate ${old_name} at baseline rev ${baseline_rev}."
  exit 1
fi

BUILD_DIR_BASE="$base/builds"
mkdir -p "$BUILD_DIR_BASE"

build_variant() {
  local base="$1" name="$2" crate_path="$3" label="$4"
  local build_dir
  build_dir="$BUILD_DIR_BASE/$label/consumer"
  mkdir -p "$build_dir/src"
  cp "$fixture_src" "$build_dir/src/main.rs"
  cat > "$build_dir/Cargo.toml" <<EOF
[package]
name = "rename-surface-consumer"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
# Neutral alias + package rename: the SAME consumer source builds against
# either crate name (exactly how a real host pins/renames a dependency).
engine = { path = "${crate_path}", package = "${name}" }
EOF
  echo "::group::rename-surface build (${label}: ${name} @ ${crate_path})"
  ( cd "$build_dir" && cargo +stable build --quiet )
  echo "::endgroup::"
}

build_variant "$worktree" "$old_name" "$old_dir" "baseline ${baseline_rev}"
build_variant "$root" "$new_name" "${root}/${crate_dir}" "current HEAD"

echo "Rename surface check green: the consumer fixture compiles against BOTH ${old_name}@${baseline_rev} and ${new_name}@HEAD."
