#!/bin/sh
set -eu

# Hemaka installer (a Ktesio project). Installs the `hemaka` + `maka`
# binaries; detects an existing Hemaka OR legacy `kt` install and migrates
# it along its original install channel.
#
# Compatibility (v0.8.0 rename, ratified 2026-09-16):
#   * The legacy KTESIO_INSTALL_* environment names keep working; the
#     HEMAKA_INSTALL_* aliases are preferred (either is accepted).
#   * A legacy `kt` binary is NEVER deleted silently: manual-channel
#     migrations install hemaka+maka beside it and print a retirement
#     note; cargo/brew channels follow their package manager.
#   * The legacy data directory is untouched (the engine keeps reading it).
REPO="Ktesio/ktesio"
TAP="ktesio/tap/hemaka"
CRATE="hemaka"
BIN="hemaka"
MAKA="maka"
LEGACY_BIN="kt"
LATEST_RELEASE_URL="https://api.github.com/repos/${REPO}/releases/latest"
RELEASE_BASE_URL="https://github.com/${REPO}/releases/download"

say() {
  printf '%s\n' "$*"
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# First non-empty value among the named environment variables (the
# forward-looking HEMAKA_* name wins; the legacy KTESIO_* name is the
# fallback), or the default when none is set.
first_env() {
  default="$1"
  shift
  for name in "$@"; do
    eval "value=\${$name:-}"
    if [ -n "$value" ]; then
      printf '%s' "$value"
      return 0
    fi
  done
  printf '%s' "$default"
}

METHOD="$(first_env auto HEMAKA_INSTALL_METHOD KTESIO_INSTALL_METHOD)"

is_truthy() {
  case "${1:-}" in
    "" | 0 | false | FALSE | no | NO | off | OFF)
      return 1
      ;;
    *)
      return 0
      ;;
  esac
}

is_dry_run() {
  dry="$(first_env "" HEMAKA_INSTALL_DRY_RUN KTESIO_INSTALL_DRY_RUN)"
  is_truthy "$dry"
}

command_exists() {
  case "$1" in
    brew)
      if [ "${KTESIO_INSTALL_TEST_HAS_BREW+x}" ]; then
        [ "$KTESIO_INSTALL_TEST_HAS_BREW" = "1" ]
        return
      fi
      ;;
    cargo)
      if [ "${KTESIO_INSTALL_TEST_HAS_CARGO+x}" ]; then
        [ "$KTESIO_INSTALL_TEST_HAS_CARGO" = "1" ]
        return
      fi
      ;;
  esac

  command -v "$1" >/dev/null 2>&1
}

run_or_dry() {
  if is_dry_run; then
    say "DRY RUN: $*"
    return 0
  fi

  "$@"
}

path_dirname() {
  case "$1" in
    */*)
      printf '%s\n' "${1%/*}"
      ;;
    *)
      printf '.\n'
      ;;
  esac
}

path_starts_with() {
  path=$1
  prefix=$2
  case "$path" in
    "$prefix" | "$prefix"/*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

# The existing install to migrate: `hemaka` first, then the retired
# `kt`. Empty when neither is on PATH. (The KTESIO_INSTALL_TEST_KT_PATH
# seam overrides the lookup for installer tests.)
find_existing_binary() {
  if [ "${KTESIO_INSTALL_TEST_KT_PATH+x}" ]; then
    if [ -n "$KTESIO_INSTALL_TEST_KT_PATH" ]; then
      printf '%s\n' "$KTESIO_INSTALL_TEST_KT_PATH"
    fi
    return 0
  fi

  found="$(command -v "$BIN" 2>/dev/null || true)"
  if [ -z "$found" ]; then
    found="$(command -v "$LEGACY_BIN" 2>/dev/null || true)"
  fi
  if [ -n "$found" ]; then
    printf '%s\n' "$found"
  fi
  return 0
}

# `hemaka --version` (and `maka --version` — the alias reports the shared
# identity) prints "hemaka <version>".
is_hemaka_binary() {
  output=$("$1" --version 2>/dev/null || true)
  case "$output" in
    "hemaka "[0-9]* | "hemaka v"[0-9]*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

# The retired `kt` (pre-rename releases) prints "kt <version>".
is_legacy_kt_binary() {
  output=$("$1" --version 2>/dev/null || true)
  case "$output" in
    "kt "[0-9]* | "kt v"[0-9]*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

is_owned_binary() {
  is_hemaka_binary "$1" || is_legacy_kt_binary "$1"
}

brew_has_hemaka() {
  if [ "${KTESIO_INSTALL_TEST_BREW_INSTALLED+x}" ]; then
    [ "$KTESIO_INSTALL_TEST_BREW_INSTALLED" = "1" ]
    return
  fi

  command_exists brew || return 1
  # Any of the four names counts: the renamed formula under either
  # spelling, or the pre-rename `ktesio` formula (a legacy keg migrates
  # through the tap's formula_renames.json on upgrade).
  brew list --formula hemaka >/dev/null 2>&1 ||
    brew list --formula "$TAP" >/dev/null 2>&1 ||
    brew list --formula ktesio >/dev/null 2>&1 ||
    brew list --formula ktesio/tap/ktesio >/dev/null 2>&1
}

detect_existing_method() {
  existing_path=$1

  case "$existing_path" in
    */Cellar/hemaka/* | */Cellar/ktesio/*)
      say "brew"
      return 0
      ;;
  esac

  if brew_has_hemaka; then
    say "brew"
    return 0
  fi

  cargo_home="${CARGO_HOME:-}"
  if [ -z "$cargo_home" ] && [ -n "${HOME:-}" ]; then
    cargo_home="$HOME/.cargo"
  fi

  if [ -n "$cargo_home" ] && path_starts_with "$existing_path" "$cargo_home/bin"; then
    say "cargo"
    return 0
  fi

  say "manual"
}

default_install_dir() {
  if [ -n "${HOME:-}" ]; then
    say "$HOME/.local/bin"
    return 0
  fi

  fail "HEMAKA_INSTALL_DIR (or KTESIO_INSTALL_DIR) is required when HOME is not set."
}

dir_is_on_path() {
  dir=$1
  case ":${PATH:-}:" in
    *":$dir:"*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

download_to_stdout() {
  url=$1
  if command_exists curl; then
    curl -fsSL "$url"
    return
  fi
  if command_exists wget; then
    wget -qO- "$url"
    return
  fi

  fail "curl or wget is required for binary installation."
}

download_file() {
  url=$1
  output=$2
  if command_exists curl; then
    curl -fsSL "$url" -o "$output"
    return
  fi
  if command_exists wget; then
    wget -q "$url" -O "$output"
    return
  fi

  fail "curl or wget is required for binary installation."
}

latest_release_tag() {
  release_json=$(download_to_stdout "$LATEST_RELEASE_URL")
  tag=$(printf '%s\n' "$release_json" |
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
    sed -n '1p')

  if [ -z "$tag" ]; then
    fail "Could not resolve the latest Hemaka release tag from GitHub."
  fi

  say "$tag"
}

detect_release_target() {
  os_name="${KTESIO_INSTALL_TEST_OS:-$(uname -s)}"
  arch_name="${KTESIO_INSTALL_TEST_ARCH:-$(uname -m)}"

  case "$os_name:$arch_name" in
    Darwin:x86_64 | Darwin:amd64)
      say "x86_64-apple-darwin"
      ;;
    Darwin:arm64 | Darwin:aarch64)
      say "aarch64-apple-darwin"
      ;;
    Linux:x86_64 | Linux:amd64)
      say "x86_64-unknown-linux-gnu"
      ;;
    *)
      fail "No prebuilt Hemaka binary is available for ${os_name}/${arch_name}. Install Rust and run: cargo install hemaka --force"
      ;;
  esac
}

sha256_file() {
  file=$1
  if command_exists sha256sum; then
    sha256sum "$file" | awk '{print $1}'
    return
  fi
  if command_exists shasum; then
    shasum -a 256 "$file" | awk '{print $1}'
    return
  fi

  fail "sha256sum or shasum is required to verify release archives."
}

verify_checksum() {
  archive=$1
  checksum_file=$2
  expected=$(awk '{print $1; exit}' "$checksum_file" | tr '[:upper:]' '[:lower:]')
  actual=$(sha256_file "$archive" | tr '[:upper:]' '[:lower:]')

  if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    fail "Checksum verification failed for $(basename "$archive")."
  fi
}

install_with_brew() {
  action=$1
  command_exists brew || fail "Homebrew is not available on PATH."

  if [ "$action" = "upgrade" ]; then
    run_or_dry brew upgrade "$TAP"
  else
    run_or_dry brew install "$TAP"
  fi
}

install_with_cargo() {
  command_exists cargo || fail "Cargo is not available on PATH."
  run_or_dry cargo install "$CRATE" --force
}

# Visible, explicit note when a retired `kt` is left on disk (never a
# silent deletion): manual-channel migrations keep the old binary.
note_retired_kt() {
  retired_path=$1
  if [ -e "$retired_path" ] && is_legacy_kt_binary "$retired_path"; then
    warn "the retired kt binary was left at $retired_path — Hemaka 0.8.0 replaced it with hemaka + maka; remove it with: rm $retired_path"
  fi
}

prepare_binary_target() {
  existing_path="${1:-}"

  install_dir="$(first_env "" HEMAKA_INSTALL_DIR KTESIO_INSTALL_DIR)"
  if [ -z "$install_dir" ]; then
    if [ -n "$existing_path" ]; then
      install_dir=$(path_dirname "$existing_path")
    else
      install_dir=$(default_install_dir)
    fi
  fi

  if [ -d "$install_dir" ]; then
    :
  elif is_dry_run; then
    :
  else
    mkdir -p "$install_dir" || fail "Could not create install directory: $install_dir"
  fi

  if [ -d "$install_dir" ] && [ ! -w "$install_dir" ]; then
    fail "$install_dir is not writable. Set HEMAKA_INSTALL_DIR (or KTESIO_INSTALL_DIR) to a writable directory on PATH."
  fi

  for candidate in "$BIN" "$MAKA"; do
    target_path="$install_dir/$candidate"
    if [ -e "$target_path" ] && ! is_owned_binary "$target_path"; then
      fail "Refusing to overwrite non-Ktesio executable at $target_path."
    fi
  done
  # An unrelated command squatting on the retired `kt` name is protected
  # the same way — the installer never touches it.
  legacy_path="$install_dir/$LEGACY_BIN"
  if [ -e "$legacy_path" ] && ! is_owned_binary "$legacy_path"; then
    fail "Refusing to overwrite non-Ktesio executable at $legacy_path."
  fi

  say "$install_dir"
}

install_with_binary() {
  existing_path="${1:-}"
  install_dir=$(prepare_binary_target "$existing_path")
  target=$(detect_release_target)

  if is_dry_run; then
    say "DRY RUN: install prebuilt $target ($BIN + $MAKA) to $install_dir"
    if ! dir_is_on_path "$install_dir"; then
      warn "$install_dir is not on PATH. Add it before running hemaka."
    fi
    return 0
  fi

  tag=$(latest_release_tag)
  asset="hemaka-${tag}-${target}.tar.gz"
  asset_url="${RELEASE_BASE_URL}/${tag}/${asset}"

  tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/hemaka-install.XXXXXX")
  trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM
  package_dir="$tmpdir/package"
  mkdir -p "$package_dir"

  say "Downloading Hemaka ${tag} for ${target}..."
  download_file "$asset_url" "$tmpdir/$asset"
  download_file "${asset_url}.sha256" "$tmpdir/${asset}.sha256"
  verify_checksum "$tmpdir/$asset" "$tmpdir/${asset}.sha256"

  tar -xzf "$tmpdir/$asset" -C "$package_dir"
  for candidate in "$BIN" "$MAKA"; do
    if [ ! -f "$package_dir/$candidate" ]; then
      fail "Release archive did not contain $candidate."
    fi
  done

  for candidate in "$BIN" "$MAKA"; do
    cp "$package_dir/$candidate" "$install_dir/$candidate"
    chmod 755 "$install_dir/$candidate"
  done

  say "Installed Hemaka to $install_dir/$BIN and $install_dir/$MAKA"
  note_retired_kt "$install_dir/$LEGACY_BIN"
  # Explicit method=binary over what detection says is a cargo-channel
  # install leaves the cargo-managed kt in place — say so visibly rather
  # than letting ~/.cargo/bin/kt linger unexplained.
  if [ -n "$existing_path" ]; then
    cargo_home="${CARGO_HOME:-}"
    if [ -z "$cargo_home" ] && [ -n "${HOME:-}" ]; then
      cargo_home="$HOME/.cargo"
    fi
    if [ -n "$cargo_home" ] && path_starts_with "$existing_path" "$cargo_home/bin"; then
      warn "an explicit binary-method install left the cargo-managed kt at $existing_path; run 'cargo uninstall ktesio' to remove it"
    fi
  fi
  if ! dir_is_on_path "$install_dir"; then
    warn "$install_dir is not on PATH. Add it before running hemaka."
  fi
  "$install_dir/$BIN" --version
}

install_auto() {
  existing=$(find_existing_binary)

  if [ -n "$existing" ]; then
    if ! is_owned_binary "$existing"; then
      fail "Refusing to overwrite non-Ktesio command at $existing."
    fi

    existing_method=$(detect_existing_method "$existing")
    case "$existing_method" in
      brew)
        install_with_brew upgrade
        ;;
      cargo)
        install_with_cargo
        cargo_home="${CARGO_HOME:-}"
        if [ -z "$cargo_home" ] && [ -n "${HOME:-}" ]; then
          cargo_home="$HOME/.cargo"
        fi
        if [ -n "$cargo_home" ]; then
          note_retired_kt "$cargo_home/bin/$LEGACY_BIN"
        fi
        ;;
      manual)
        install_with_binary "$existing"
        ;;
      *)
        fail "Unknown existing install method: $existing_method"
        ;;
    esac
    return 0
  fi

  if command_exists brew; then
    install_with_brew install
    return 0
  fi

  if command_exists cargo; then
    install_with_cargo
    return 0
  fi

  install_with_binary ""
}

main() {
  case "$METHOD" in
    auto)
      ;;
    brew | cargo | binary)
      ;;
    *)
      fail "HEMAKA_INSTALL_METHOD (or KTESIO_INSTALL_METHOD) must be one of: auto, brew, cargo, binary."
      ;;
  esac

  existing=$(find_existing_binary)
  if [ -n "$existing" ] && ! is_owned_binary "$existing"; then
    fail "Refusing to overwrite non-Ktesio command at $existing."
  fi

  case "$METHOD" in
    auto)
      install_auto
      ;;
    brew)
      if brew_has_hemaka; then
        install_with_brew upgrade
      else
        install_with_brew install
      fi
      ;;
    cargo)
      install_with_cargo
      ;;
    binary)
      existing_method=""
      if [ -n "$existing" ]; then
        existing_method=$(detect_existing_method "$existing")
      fi
      if [ "$existing_method" = "manual" ]; then
        install_with_binary "$existing"
      else
        install_with_binary ""
      fi
      ;;
  esac
}

main "$@"
