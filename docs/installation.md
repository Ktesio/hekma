---
title: Installation
description: Install Hekma on macOS, Linux, or Windows with the hosted installer, Homebrew, Cargo, release archives, or source builds.
---

# Installation

Hekma is a Rust CLI. It works on macOS, Linux, and Windows with no runtime dependencies beyond the operating system.

## Prerequisites

- None for the installed binary. Rust is only required when installing through Cargo or from source.

## Install with the installer

On macOS or Linux:

```bash
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | sh
```

On Windows with PowerShell:

```powershell
irm https://cli.ktesio.dev/hekma/install.ps1 | iex
```

The installer preserves an existing Hekma install channel when it can:

- Homebrew installs are updated with `brew upgrade ktesio/tap/hekma`. If you installed from the older `imagdy/tap` location, migrate once with `brew untap imagdy/tap && brew install ktesio/tap/hekma`.
- Cargo installs are updated with `cargo install hekma --force`.
- Manual binary installs are replaced in their existing writable directory.

For new macOS and Linux installs, the installer prefers Homebrew, then Cargo,
then a prebuilt GitHub Release binary. For new Windows installs, it prefers
Cargo, then a prebuilt GitHub Release binary.

Installer overrides:

```bash
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | KTESIO_INSTALL_METHOD=binary sh
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | KTESIO_INSTALL_DIR="$HOME/.local/bin" sh
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | KTESIO_INSTALL_DRY_RUN=1 sh
```

`KTESIO_INSTALL_METHOD` accepts `auto`, `brew`, `cargo`, or `binary` on macOS
and Linux. Windows accepts `auto`, `cargo`, or `binary`.

The installer does not install Homebrew, Rust, or Cargo, and it writes no shell
profile entries. Git is not a runtime dependency. If it installs a binary into
a directory that is not on `PATH`, it prints the directory to add.

## Install from source

```bash
git clone https://github.com/Ktesio/hekma.git
cd ktesio
cargo install --path .
```

Verify:

```bash
hekma --version
hekma --help
```

## Install from crates.io

```bash
cargo install hekma
```

The crates.io package is named `hekma`; it installs the `hekma` and `hkm` binaries.

## Install from a release

Download the archive for your platform from [GitHub Releases](https://github.com/Ktesio/hekma/releases), then unpack it and place the `hekma` and `hkm` binaries on your `PATH`.

Release archives use this naming pattern:

```text
hekma-<tag>-<target>.tar.gz
hekma-<tag>-<target>.zip
```

Each release also includes `.sha256` files and an aggregate checksum file.

## Install with Homebrew

After a release is published to the Homebrew tap:

```bash
brew install ktesio/tap/hekma
```

The formula installs the prebuilt macOS or Linux release archive for your platform.

## Updating Hekma

Hekma checks GitHub Releases through an hourly cache when a subcommand runs. If a
newer release is available, it prints a small stderr notice that asks you to run:

```bash
hekma self-update
```

`hekma self-update` preserves the current install channel automatically. Homebrew
installs upgrade with Homebrew, Cargo installs upgrade with Cargo, and manual
release installs download the latest GitHub Release archive, verify its
`.sha256` checksum, and replace the current binary. A running agent keeps
executing the binary it was started with; restart it to pick up the new version.

Set `KTESIO_NO_UPDATE_CHECK=1` to skip automatic update checks.

## Uninstall

How to remove Hekma depends on the install channel:

```bash
brew uninstall ktesio/tap/hekma   # Homebrew installs
cargo uninstall hekma             # Cargo installs
```

For a manual release install, delete the `hekma` and `hkm` binaries from the directory it was
installed into.

Uninstalling removes the binary only. Hekma's own state is untouched: the state
directory (override `KTESIO_STATE_DIR`, otherwise the platform data dir) holds
`state.db`, instance Agent Homes, and any filesystem Memory Backing contents,
and `<state dir>/secrets.toml` holds any stored secrets. Delete the state
directory yourself if you want a fully clean removal — otherwise it is reused
if you reinstall.

## Platform notes

- macOS may require Xcode Command Line Tools when building from source.
- Windows users building from source need the MSVC build tools (see [Rust's Windows setup](https://rustup.rs/)).
- Linux users may need standard build tools for Rust crates.

## Next steps

- [Getting started](get-started.md)
- [Command reference](commands.md)
