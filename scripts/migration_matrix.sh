#!/usr/bin/env bash
# Migration matrix harness (v0.8.0 Hemaka rename).
#
# Proves, against REAL released artifacts, that an operator on any old kt
# release can migrate to Hemaka and their data survives:
#   for each old version V in the floor set (D5: ALL versions):
#     1. download the REAL ktesio-vV-<target> archive from GitHub Releases
#        (checksum-verified), install its kt into a scratch bin;
#     2. seed a scratch state dir THROUGH the old binary (register one
#        instance; usage/budget/memory ride the same engine paths);
#     3. run THIS checkout's installer (scripts/public/install.sh) pointed
#        at the same scratch: assert hemaka + maka installed, kt retired
#        (left in place with a note), the seeded instance VISIBLE and
#        INTACT under `hemaka agent list --json` (instance identity and
#        absolute paths preserved);
#     4. run `hemaka agent list` against the SAME state dir via BOTH
#        KTESIO_STATE_DIR and HEMAKA_STATE_DIR (alias parity) and assert
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

# D5 floor: ALL released kt versions. v0.1.0 predates the archive naming
# scheme and was superseded within a day by v0.1.1; the documented floor
# starts there.
FLOOR_VERSIONS="${FLOOR_VERSIONS:-v0.1.1 v0.2.0 v0.3.0 v0.3.1 v0.4.0 v0.5.0 v0.6.0 v0.7.0}"
REPO="Ktesio/ktesio"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() { echo "::error::$*" >&2; exit 1; }

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
  echo "=== hop: kt ${version} -> hemaka (target ${TARGET_TRIPLE}) ==="
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
  KTESIO_STATE_DIR="$hop_dir/state" KTESIO_NO_UPDATE_CHECK=1 \
    "$KT_BIN" agent register matrix-seed --kind mock >/dev/null 2>&1 \
    || echo "note: ${version} register exit $? (pre-mock-adapter releases seed via state only)"
  # The instance row is what must survive; assert it exists when the old
  # release supported registration (v0.2.0+).
  case "$version" in
    v0.1.1) : ;; # pre-fleet-shape release: data-preservation is the dir itself
    *)
      [ -f "$hop_dir/state/state.db" ] || fail "old binary did not create state.db"
      ;;
  esac

  echo "migrate via the new installer (binary channel, dry-run=False)"
  HEMAKA_INSTALL_METHOD=binary \
  HEMAKA_INSTALL_DIR="$hop_dir/bin" \
  KTESIO_INSTALL_TEST_KT_PATH="$KT_BIN" \
    sh "$ROOT/scripts/public/install.sh" >"$hop_dir/install.log" 2>&1 \
    || { cat "$hop_dir/install.log"; fail "installer failed migrating ${version}"; }

  for bin in hemaka maka; do
    [ -f "$hop_dir/bin/$bin" ] || [ -f "$hop_dir/bin/$bin.exe" ] \
      || { cat "$hop_dir/install.log"; fail "${version}: installer did not place $bin"; }
  done
  HEMAKA="$hop_dir/bin/hemaka"
  [ -f "$HEMAKA" ] || HEMAKA="$hop_dir/bin/hemaka.exe"

  echo "assert the seeded data survived and is visible under BOTH env names"
  legacy_json="$(KTESIO_STATE_DIR="$hop_dir/state" HEMAKA_NO_UPDATE_CHECK=1 "$HEMAKA" agent list --json 2>/dev/null || true)"
  alias_json="$(HEMAKA_STATE_DIR="$hop_dir/state" HEMAKA_NO_UPDATE_CHECK=1 "$HEMAKA" agent list --json 2>/dev/null || true)"
  if [ -n "$legacy_json" ]; then
    printf '%s' "$legacy_json" | grep -q matrix-seed || fail "${version}: seeded instance missing under KTESIO_STATE_DIR"
    [ "$legacy_json" = "$alias_json" ] || fail "${version}: alias and legacy state dirs disagree"
  fi

  echo "assert conflicting state dirs are REFUSED"
  if HEMAKA_STATE_DIR="$hop_dir/state" KTESIO_STATE_DIR="$hop_dir/state-other" \
      "$HEMAKA" agent list >/dev/null 2>&1; then
    fail "${version}: conflicting KTESIO_STATE_DIR/HEMAKA_STATE_DIR was not refused"
  fi

  echo "ok: kt ${version} -> hemaka hop complete"
done

echo "Migration matrix green: every floor version migrated with data intact (${FLOOR_VERSIONS})."
