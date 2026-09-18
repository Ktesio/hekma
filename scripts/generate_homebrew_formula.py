#!/usr/bin/env python3
"""Generate the Homebrew formula for a tagged Hekma release."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


# The repository is still Ktesio/hekma at the 0.8.0 release (the gated repo
# rename follows AFTER the release is live, so release URLs never depend on
# redirects); flip this with the canonical-URL change in 0.8.1.
REPO = "Ktesio/hekma"
FORMULA_CLASS = "Hekma"
DESCRIPTION = (
    "Run AI agents like services: supervise their lifecycle, meter real "
    "token usage, and enforce dollar budgets."
)
# Ktesio ships a custom license (no SPDX id exists), so Homebrew gets the
# `:any` symbol — to Homebrew this means the license is unspecified, not
# "any version". It must render unquoted: `license :any`.
LICENSE = ":any"
# Emitted above the `license` clause in the formula so the tap states the
# real terms even though `:any` cannot name them.
LICENSE_COMMENT = (
    "# Hekma ships the Ktesio Noncommercial-Attribution License 1.0.0 — "
    "source-available; commercial use requires the author's written approval."
)
HOMEBREW_TARGETS = [
    ("x86_64-apple-darwin", "tar.gz"),
    ("aarch64-apple-darwin", "tar.gz"),
    ("x86_64-unknown-linux-gnu", "tar.gz"),
]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="Release tag, e.g. v0.1.0")
    parser.add_argument(
        "--checksums-file",
        type=Path,
        required=True,
        help="Aggregate checksum file produced by the release workflow",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="Formula path to write; prints to stdout when omitted",
    )
    args = parser.parse_args()

    checksums = parse_checksums(args.checksums_file.read_text(encoding="utf-8"))
    formula = render_formula(args.tag, checksums)

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(formula, encoding="utf-8")
    else:
        print(formula, end="")

    return 0


def parse_checksums(text: str) -> dict[str, str]:
    checksums: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        match = re.fullmatch(r"([a-fA-F0-9]{64})\s+\*?(.+)", line)
        if not match:
            raise ValueError(f"invalid checksum line: {line}")
        checksum, asset = match.groups()
        checksums[asset] = checksum.lower()
    return checksums


def license_clause() -> str:
    """Render the formula's `license` value.

    Homebrew license symbols (`:any`, `:public_domain`, …) are Ruby symbols
    and must be unquoted; SPDX ids are strings and need double quotes.
    """
    if LICENSE.startswith(":"):
        return LICENSE
    return f'"{LICENSE}"'


def render_formula(tag: str, checksums: dict[str, str]) -> str:
    version = version_from_tag(tag)
    missing = [
        asset_name(tag, target, extension)
        for target, extension in HOMEBREW_TARGETS
        if asset_name(tag, target, extension) not in checksums
    ]
    if missing:
        raise ValueError(f"missing checksums for Homebrew assets: {', '.join(missing)}")

    intel_macos = asset_name(tag, "x86_64-apple-darwin", "tar.gz")
    arm_macos = asset_name(tag, "aarch64-apple-darwin", "tar.gz")
    linux = asset_name(tag, "x86_64-unknown-linux-gnu", "tar.gz")

    return f'''class {FORMULA_CLASS} < Formula
  desc "{DESCRIPTION}"
  homepage "https://github.com/{REPO}"
  version "{version}"
  {LICENSE_COMMENT}
  license {license_clause()}

  on_macos do
    on_arm do
      url "{release_url(tag, arm_macos)}"
      sha256 "{checksums[arm_macos]}"
    end

    on_intel do
      url "{release_url(tag, intel_macos)}"
      sha256 "{checksums[intel_macos]}"
    end
  end

  on_linux do
    url "{release_url(tag, linux)}"
    sha256 "{checksums[linux]}"
  end

  def install
    bin.install "hekma"
    bin.install "hkm"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/hekma --version")
    assert_match version.to_s, shell_output("#{{bin}}/hkm --version")
  end
end
'''


def version_from_tag(tag: str) -> str:
    match = re.fullmatch(r"v(\d+\.\d+\.\d+)", tag)
    if not match:
        raise ValueError(f"Homebrew releases require a vMAJOR.MINOR.PATCH tag: {tag}")
    return match.group(1)


def asset_name(tag: str, target: str, extension: str) -> str:
    return f"hekma-{tag}-{target}.{extension}"


def release_url(tag: str, asset: str) -> str:
    return f"https://github.com/{REPO}/releases/download/{tag}/{asset}"


if __name__ == "__main__":
    raise SystemExit(main())
