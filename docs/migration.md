---
title: Migration to Hekma
description: Migrating from Ktesio kt releases (v0.1.1–v0.7.0) to Hekma 0.8.0 — commands, install channels, the data guarantee, and the environment settings map.
---

# Migration to Hekma

Hekma 0.8.0 is the rename of the product formerly released as **Ktesio**
(the `ktesio` crate, the `kt` binary). Ktesio remains the publisher and
umbrella brand — Hekma is a [ktesio.dev](https://ktesio.dev) project —
but the product you install and run is now **Hekma**: the `hekma` command,
the short alias `hkm`, and the `hekma-*` crates.

**The one thing that does not change: your data.** Hekma reads the exact
same state directory `kt` used. Nothing is moved, merged, reset, or
deleted — instances, usage ledgers, budgets, memory backings, secrets, and
permissions all survive as-is.

## TL;DR — migrate any kt install

Re-run the installer; it detects the existing install and follows its
original channel:

```bash
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | sh
```

```powershell
irm https://cli.ktesio.dev/hekma/install.ps1 | iex
```

You get `hekma` and `hkm` (both standalone, both the same CLI). Your old
`kt` binary **keeps working but can no longer update** — see
[Why kt self-update no longer reaches 0.8.0](#why-kt-self-update-no-longer-reaches-080),
then remove it at your leisure.

## What changed, precisely

| Before (≤ 0.7.0) | Hekma 0.8.0 |
|---|---|
| Command `kt` | `hekma` (primary) + `hkm` (alias) — `kt` is retired |
| Crate `ktesio` | `hekma` (the `ktesio` crate is frozen at 0.7.0, preserved on crates.io) |
| Library `ktesio-engine` 0.3.0 | `hekma-engine` 0.4.0 (same line, continued) |
| Release archives `ktesio-v*-*` | `hekma-v*-*` |
| Homebrew `ktesio/tap/ktesio` | `ktesio/tap/hekma` (same org tap; the formula renamed) |
| State env `KTESIO_STATE_DIR` | still works — **and** `HEKMA_STATE_DIR` is accepted |
| `KTESIO_NO_UPDATE_CHECK` | still works — **and** `HEKMA_NO_UPDATE_CHECK` |
| `KTESIO_INSTALL_METHOD/_DIR/_DRY_RUN` | still work — **and** the `HEKMA_*` equivalents |
| Docs `docs.ktesio.dev` | `hekma.ktesio.dev` (the old host redirects) |

Unchanged: JSON output shapes and schema versions, exit codes 0–6, the
`KTESIO_USAGE` self-report sentinel agents emit, the `KTESIO_MEMORY_DIR`
mock-adapter mapping, `HERMES_HOME`, the adapter contract
(`contract_version = "1.0.0"`), and the license.

## Migrating each install channel

### Homebrew

The formula was renamed **inside the same tap** (`ktesio/tap`), and the tap
carries a `formula_renames.json` mapping, so a plain upgrade follows the
rename:

```bash
brew upgrade ktesio/tap/hekma
```

> On newer Homebrew versions with tap trust gates, an upgrade may first
> refuse with "Refusing to load formula … from untrusted tap ktesio/tap."
> — trust your own tap once with `brew trust ktesio/tap`, then re-run the
> upgrade. (Verified live: this exact path migrates a real 0.3.1 keg to
> `ktesio/tap/hekma` 0.8.0 in one command, removing the old keg.)

Fresh installs (and the documented reinstall path):

```bash
brew install ktesio/tap/hekma
```

Uninstall:

```bash
brew uninstall ktesio/tap/hekma
```

### Cargo

```bash
cargo install hekma --force
```

This installs `hekma` and `hkm` into `$CARGO_HOME/bin`. The old
cargo-installed `kt` stays behind as an orphan of the frozen `ktesio`
crate; remove it with:

```bash
cargo uninstall ktesio
```

### Manual binary (curl archive or direct download)

Download the `hekma-v<version>-<target>` archive from
[GitHub Releases](https://github.com/Ktesio/ktesio/releases), unpack, and
place **both** `hekma` and `hkm` on your `PATH` (beside your old `kt` is
fine). The installer does this for you; if you migrate by hand, both
binaries must come from the same release so they stay version-matched.
`hekma self-update` (manual channel) replaces both binaries on every
update — running it as `hkm` still refreshes `hekma`, and vice versa.

Remove the retired `kt` whenever you like:

```bash
rm "$(command -v kt)"
```

## Why kt self-update no longer reaches 0.8.0

A deliberate decision (no compatibility window on release artifacts): from
0.8.0 on, GitHub releases carry only `hekma-*` archives. Old `kt`
binaries' self-update constructs `ktesio-v<tag>-<target>` download URLs,
which no longer exist — the update fails with a download/checksum error
rather than partially upgrading. Re-running the installer (or cargo/brew)
is the documented migration path and covers **every** released `kt`
version, v0.1.1 through v0.7.0.

## Environment settings map

`KTESIO_*` settings keep working; the `HEKMA_*` names are preferred going
forward. When both state-dir variables are set they must name the **same**
absolute path — different paths is a hard error (an ambiguous state root is
never silently resolved).

| Setting | Status |
|---|---|
| `KTESIO_STATE_DIR` | honored; alias `HEKMA_STATE_DIR` |
| `KTESIO_NO_UPDATE_CHECK` | honored; alias `HEKMA_NO_UPDATE_CHECK` (either truthy disables; `CI` too) |
| `KTESIO_INSTALL_METHOD` / `_DIR` / `_DRY_RUN` | honored; `HEKMA_INSTALL_*` aliases preferred |
| `KTESIO_USAGE` (agent-side sentinel) | **unchanged** — agents keep emitting this line |
| `KTESIO_MEMORY_DIR` (mock adapter memory) | **unchanged** |
| `HERMES_HOME` (Hermes' own variable) | **unchanged** |

A note on piping the installer: an assignment like
`KTESIO_INSTALL_METHOD=binary curl … | sh` sets the variable on **curl**,
not on the shell reading the script. Put it on the `sh` segment instead:

```bash
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | KTESIO_INSTALL_METHOD=binary sh
```

## Windows notes

- New default install dir: `%LOCALAPPDATA%\hekma\bin`. The installer
  reuses the legacy `%LOCALAPPDATA%\ktesio\bin` (or `~/.ktesio\bin`) when
  your existing install lives there, so Hekma lands beside the old
  binary instead of shadowing it on `PATH`.
- Replacing a running binary: stop any running `kt`/`hekma` processes
  before migrating, then re-run the installer.

## FAQ

**Is `hkm` a different program?** No — `hekma` and `hkm` are two names
for the same CLI (`hkm --version` reports the shared `hekma` identity).
Ship both in scripts where brevity matters; both are standalone.

**Do I need to re-register agents?** No. Open the same state directory as
before (the default never changed) and your Fleet is exactly as you left
it.

**Is the old `kt` dangerous to keep?** No — it is simply frozen at its
last version and can no longer self-update. The installer never deletes
it; it prints a visible note naming the file and how to remove it.

**What happened to the `ktesio` crate on crates.io?** Preserved, frozen at
0.7.0. It is not yanked — historic lockfiles keep resolving — but no new
versions will be published under that name.
