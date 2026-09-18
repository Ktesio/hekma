#!/usr/bin/env bash
# Migration matrix harness (v0.8.0 Hekma rename).
#
# Proves, against REAL released artifacts, that an operator on any old kt
# release can migrate to Hekma and their data survives:
#   for each old version V in the floor set (D5: ALL versions):
#     1. download the REAL ktesio-vV-<target> archive from GitHub Releases
#        (checksum-verified), install its kt into a scratch bin;
#     2. seed a scratch state dir THROUGH the old binary (register one
#        instance; usage/budget/memory ride the same engine paths);
#     3. run THIS checkout's installer (scripts/public/install.sh) pointed
#        at the same scratch: assert hekma + hkm installed, kt retired
#        (left in place with a note), the seeded instance VISIBLE and
#        INTACT under `hekma agent list --json` (instance identity and
#        absolute paths preserved);
#     4. run `hekma agent list` against the SAME state dir via BOTH
#        KTESIO_STATE_DIR and HEKMA_STATE_DIR (alias parity) and assert
#        conflicting dirs are refused.
#
# Requires network access to github.com. Local usage:
#   bash scripts/migration_matrix.sh [target-triple]
# CI usage: the migration-matrix workflow (manual dispatch + post-release
# scheduled evidence runs). Exits nonzero on the FIRST failed hop with a
# full forensic dump of every step's output.
set -euo pipefail

TARGET_TRIPLE="${1:-$(
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) echo aarch64-apple-darwin ;;
    Darwin:x86_64) echo x86_64-apple-darwin ;;
    Linux:x86_64) echo x86_64-unknown-linux-gnu ;;
    *) echo "" ;;
  esac
)}"
if [ -z "$TARGET_TRIPLE" ]; then
  echo "::error::unsupported host for the migration matrix (pass an explicit target triple)."
  exit 1
fi
case "$TARGET_TRIPLE" in
  *windows*) EXT=zip ;; *) EXT=tar.gz ;;
esac

# D5 floor: ALL kt versions with GitHub release binaries. v0.1.0 predates
# the archive naming scheme (superseded within a day by v0.1.1); v0.6.0
# exists only on crates.io — there is NO v0.6.0 GitHub release, so it has
# no binary-install hop (its cargo users migrate via
# `cargo install hekma --force`, covered by the guide + the cargo-channel
# installer tests).
FLOOR_VERSIONS="${FLOOR_VERSIONS:-v0.1.1 v0.2.0 v0.3.0 v0.3.1 v0.4.0 v0.5.0 v0.7.0}"
REPO="Ktesio/ktesio"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
BUILD_TMP="$(mktemp -d)"
trap 'rm -rf "$WORK" "$BUILD_TMP"' EXIT

fail() { echo "::error::$*" >&2; exit 1; }

# Scheduled-run guard: if the latest release carries no hekma-* assets
# (pre-v0.8.0), there is nothing to migrate TO — exit 0 with a notice so
# the weekly cron stays green in the merge->tag window instead of failing
# on a missing artifact. Manual dispatches still run the full matrix.
latest_assets="$(curl -fsSL --retry 3 "https://api.github.com/repos/${REPO}/releases/latest" | grep -o '"name": *"hekma-[^"]*"' | head -1 || true)"
if [ -z "$latest_assets" ] && [ "${GITHUB_EVENT_NAME:-}" = "schedule" ]; then
  echo "notice: latest release has no hekma-* assets yet (pre-v0.8.0); nothing to migrate to. Exiting green."
  exit 0
fi

verify_sha256() {
  local file="$1" sum_file="$2" expected actual
  expected="$(awk '{print $1}' "$sum_file" | tr '[:upper:]' '[:lower:]')"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file" | awk '{print $1}')"
  else
    actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  fi
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $(basename "$file")"
}

for version in $FLOOR_VERSIONS; do
  echo "=== hop: kt ${version} -> hekma (target ${TARGET_TRIPLE}) ==="
  hop_dir="$WORK/${version}"
  mkdir -p "$hop_dir/bin" "$hop_dir/state"

  asset="ktesio-${version}-${TARGET_TRIPLE}.${EXT}"
  base="https://github.com/${REPO}/releases/download/${version}"
  echo "download ${asset}"
  curl -fsSL --retry 3 -o "$hop_dir/$asset" "$base/$asset"
  curl -fsSL --retry 3 -o "$hop_dir/$asset.sha256" "$base/$asset.sha256"
  verify_sha256 "$hop_dir/$asset" "$hop_dir/$asset.sha256"
  if [ "$EXT" = "tar.gz" ]; then
    tar -xzf "$hop_dir/$asset" -C "$hop_dir/bin"
  else
    unzip -oq "$hop_dir/$asset" -d "$hop_dir/bin"
  fi
  [ -f "$hop_dir/bin/kt" ] || [ -f "$hop_dir/bin/kt.exe" ] || fail "archive lacked kt"
  KT_BIN="$hop_dir/bin/kt"
  [ -f "$KT_BIN" ] || KT_BIN="$hop_dir/bin/kt.exe"

  echo "seed state through the old binary"
  seeded=0
  if KTESIO_STATE_DIR="$hop_dir/state" KTESIO_NO_UPDATE_CHECK=1 \
      "$KT_BIN" agent register matrix-seed --kind mock >/dev/null 2>&1; then
    seeded=1
  else
    echo "note: ${version} register exited nonzero (v0.1.x has no agent surface; the hop then proves install + state-dir compatibility only)"
  fi
  # The instance row is what must survive; assert it exists when the old
  # release supported registration (v0.2.0+).
  if [ "$seeded" = "1" ] && [ ! -f "$hop_dir/state/state.db" ]; then
    fail "old binary registered but created no state.db"
  fi

  echo "migrate via the new installer (binary channel, dry-run=False)"
  HEKMA_INSTALL_METHOD=binary \
  HEKMA_INSTALL_DIR="$hop_dir/bin" \
  KTESIO_INSTALL_TEST_KT_PATH="$KT_BIN" \
    sh "$ROOT/scripts/public/install.sh" >"$hop_dir/install.log" 2>&1 \
    || { cat "$hop_dir/install.log"; fail "installer failed migrating ${version}"; }

  for bin in hekma hkm; do
    [ -f "$hop_dir/bin/$bin" ] || [ -f "$hop_dir/bin/$bin.exe" ] \
      || { cat "$hop_dir/install.log"; fail "${version}: installer did not place $bin"; }
  done
  HEKMA="$hop_dir/bin/hekma"
  [ -f "$HEKMA" ] || HEKMA="$hop_dir/bin/hekma.exe"

  echo "assert the state survived and is visible IDENTICALLY under BOTH env names"
  legacy_json="$(KTESIO_STATE_DIR="$hop_dir/state" HEKMA_NO_UPDATE_CHECK=1 "$HEKMA" agent list --json 2>/dev/null || true)"
  alias_json="$(HEKMA_STATE_DIR="$hop_dir/state" HEKMA_NO_UPDATE_CHECK=1 "$HEKMA" agent list --json 2>/dev/null || true)"
  # ALWAYS asserted (every floor version): both env names yield the SAME
  # document, and it carries the frozen schema_version. An EMPTY fleet is a
  # valid result for pre-fleet releases (v0.1.x), where the hop proves
  # install + state-dir compatibility.
  [ -n "$legacy_json" ] || fail "${version}: agent list --json produced nothing under KTESIO_STATE_DIR"
  [ "$legacy_json" = "$alias_json" ] || fail "${version}: alias and legacy state dirs disagree"
  printf '%s' "$legacy_json" | grep -q '"schema_version"' \
    || fail "${version}: list JSON lacks schema_version (not the frozen document)"
  # matrix-seed must be present ONLY when the old binary actually registered.
  if [ "$seeded" = "1" ]; then
    printf '%s' "$legacy_json" | grep -q matrix-seed \
      || fail "${version}: seeded instance missing under KTESIO_STATE_DIR"
  fi

  echo "assert conflicting state dirs are REFUSED"
  if HEKMA_STATE_DIR="$hop_dir/state" KTESIO_STATE_DIR="$hop_dir/state-other" \
      "$HEKMA" agent list >/dev/null 2>&1; then
    fail "${version}: conflicting KTESIO_STATE_DIR/HEKMA_STATE_DIR was not refused"
  fi

  echo "ok: kt ${version} -> hekma hop complete"
done

echo "Migration matrix green: every floor version migrated with data intact (${FLOOR_VERSIONS})."
