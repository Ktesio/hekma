#!/bin/sh
set -eu

REPO="Ktesio/ktesio"
TAP="ktesio/tap/ktesio"
CRATE="ktesio"
BIN="kt"
LATEST_RELEASE_URL="https://api.github.com/repos/${REPO}/releases/latest"
RELEASE_BASE_URL="https://github.com/${REPO}/releases/download"

METHOD="${KTESIO_INSTALL_METHOD:-auto}"

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
  is_truthy "${KTESIO_INSTALL_DRY_RUN:-}"
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

find_existing_kt() {
  if [ "${KTESIO_INSTALL_TEST_KT_PATH+x}" ]; then
    if [ -n "$KTESIO_INSTALL_TEST_KT_PATH" ]; then
      printf '%s\n' "$KTESIO_INSTALL_TEST_KT_PATH"
    fi
    return 0
  fi

  command -v "$BIN" 2>/dev/null || true
}

is_ktesio_binary() {
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

brew_has_ktesio() {
  if [ "${KTESIO_INSTALL_TEST_BREW_INSTALLED+x}" ]; then
    [ "$KTESIO_INSTALL_TEST_BREW_INSTALLED" = "1" ]
    return
  fi

  command_exists brew || return 1
  brew list --formula ktesio >/dev/null 2>&1 ||
    brew list --formula "$TAP" >/dev/null 2>&1
}

detect_existing_method() {
  kt_path=$1

  case "$kt_path" in
    */Cellar/ktesio/*)
      say "brew"
      return 0
      ;;
  esac

  if brew_has_ktesio; then
    say "brew"
    return 0
  fi

  cargo_home="${CARGO_HOME:-}"
  if [ -z "$cargo_home" ] && [ -n "${HOME:-}" ]; then
    cargo_home="$HOME/.cargo"
  fi

  if [ -n "$cargo_home" ] && path_starts_with "$kt_path" "$cargo_home/bin"; then
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

  fail "KTESIO_INSTALL_DIR is required when HOME is not set."
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
    fail "Could not resolve the latest Ktesio release tag from GitHub."
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
      fail "No prebuilt Ktesio binary is available for ${os_name}/${arch_name}. Install Rust and run: cargo install ktesio --force"
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

prepare_binary_target() {
  existing_path="${1:-}"

  if [ -n "${KTESIO_INSTALL_DIR:-}" ]; then
    install_dir=$KTESIO_INSTALL_DIR
  elif [ -n "$existing_path" ]; then
    install_dir=$(path_dirname "$existing_path")
  else
    install_dir=$(default_install_dir)
  fi

  if [ -d "$install_dir" ]; then
    :
  elif is_dry_run; then
    :
  else
    mkdir -p "$install_dir" || fail "Could not create install directory: $install_dir"
  fi

  if [ -d "$install_dir" ] && [ ! -w "$install_dir" ]; then
    fail "$install_dir is not writable. Set KTESIO_INSTALL_DIR to a writable directory on PATH."
  fi

  target_path="$install_dir/$BIN"
  if [ -e "$target_path" ] && ! is_ktesio_binary "$target_path"; then
    fail "Refusing to overwrite non-Ktesio executable at $target_path."
  fi

  say "$target_path"
}

install_with_binary() {
  existing_path="${1:-}"
  target_path=$(prepare_binary_target "$existing_path")
  install_dir=$(path_dirname "$target_path")
  target=$(detect_release_target)

  if is_dry_run; then
    say "DRY RUN: install prebuilt $target to $target_path"
    if ! dir_is_on_path "$install_dir"; then
      warn "$install_dir is not on PATH. Add it before running kt."
    fi
    return 0
  fi

  tag=$(latest_release_tag)
  asset="ktesio-${tag}-${target}.tar.gz"
  asset_url="${RELEASE_BASE_URL}/${tag}/${asset}"

  tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/ktesio-install.XXXXXX")
  trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM
  package_dir="$tmpdir/package"
  mkdir -p "$package_dir"

  say "Downloading Ktesio ${tag} for ${target}..."
  download_file "$asset_url" "$tmpdir/$asset"
  download_file "${asset_url}.sha256" "$tmpdir/${asset}.sha256"
  verify_checksum "$tmpdir/$asset" "$tmpdir/${asset}.sha256"

  tar -xzf "$tmpdir/$asset" -C "$package_dir"
  if [ ! -f "$package_dir/$BIN" ]; then
    fail "Release archive did not contain $BIN."
  fi

  cp "$package_dir/$BIN" "$target_path"
  chmod 755 "$target_path"

  say "Installed Ktesio to $target_path"
  if ! dir_is_on_path "$install_dir"; then
    warn "$install_dir is not on PATH. Add it before running kt."
  fi
  "$target_path" --version
}

install_auto() {
  existing_kt=$(find_existing_kt)

  if [ -n "$existing_kt" ]; then
    if ! is_ktesio_binary "$existing_kt"; then
      fail "Refusing to overwrite non-Ktesio kt command at $existing_kt."
    fi

    existing_method=$(detect_existing_method "$existing_kt")
    case "$existing_method" in
      brew)
        install_with_brew upgrade
        ;;
      cargo)
        install_with_cargo
        ;;
      manual)
        install_with_binary "$existing_kt"
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
      fail "KTESIO_INSTALL_METHOD must be one of: auto, brew, cargo, binary."
      ;;
  esac

  existing_kt=$(find_existing_kt)
  if [ -n "$existing_kt" ] && ! is_ktesio_binary "$existing_kt"; then
    fail "Refusing to overwrite non-Ktesio kt command at $existing_kt."
  fi

  case "$METHOD" in
    auto)
      install_auto
      ;;
    brew)
      if brew_has_ktesio; then
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
      if [ -n "$existing_kt" ]; then
        existing_method=$(detect_existing_method "$existing_kt")
      fi
      if [ "$existing_method" = "manual" ]; then
        install_with_binary "$existing_kt"
      else
        install_with_binary ""
      fi
      ;;
  esac
}

main "$@"
