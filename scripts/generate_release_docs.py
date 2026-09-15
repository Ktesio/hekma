#!/usr/bin/env python3
"""Generate changelog, release notes, and GitHub Release body for a tag."""

from __future__ import annotations

import argparse
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
REPO = "Ktesio/ktesio"
TARGETS = [
    ("macOS Intel", "x86_64-apple-darwin", "tar.gz"),
    ("macOS Apple Silicon", "aarch64-apple-darwin", "tar.gz"),
    ("Windows x64", "x86_64-pc-windows-msvc", "zip"),
    ("Linux x64", "x86_64-unknown-linux-gnu", "tar.gz"),
]
SECTION_ORDER = [
    ("feat", "Features"),
    ("fix", "Fixes"),
    ("docs", "Documentation"),
    ("test", "Tests"),
    ("ci", "CI"),
    ("refactor", "Refactors"),
    ("chore", "Maintenance"),
    ("other", "Other Changes"),
]
CONVENTIONAL_RE = re.compile(r"^(?P<type>[a-z]+)(?:\([^)]+\))?!?: (?P<summary>.+)$")


@dataclass
class Commit:
    sha: str
    subject: str


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="Release tag, e.g. v0.1.0")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "target" / "release-docs")
    parser.add_argument("--update-files", action="store_true")
    args = parser.parse_args()

    args.output_dir.mkdir(parents=True, exist_ok=True)

    previous_tag = previous_semver_tag(args.tag)
    commits = commits_for_range(args.tag, previous_tag)
    section = render_release_section(args.tag, previous_tag, commits)
    body = render_release_body(args.tag, previous_tag, commits)

    (args.output_dir / "CHANGELOG.section.md").write_text(section, encoding="utf-8")
    (args.output_dir / "RELEASE_NOTES.section.md").write_text(section, encoding="utf-8")
    (args.output_dir / "release-body.md").write_text(body, encoding="utf-8")

    if args.update_files:
        upsert_release_section(
            ROOT / "CHANGELOG.md",
            "# Changelog\n\nAll notable changes to Ktesio are generated from git history.\n",
            args.tag,
            section,
        )
        upsert_release_section(
            ROOT / "docs" / "RELEASE_NOTES.md",
            "# Release Notes\n\nRelease notes are generated when a version tag is published.\n",
            args.tag,
            section,
        )

    print(f"Generated release docs for {args.tag}")
    return 0


def previous_semver_tag(tag: str) -> str | None:
    current = semver_key(tag)
    tags = [
        candidate
        for candidate in git("tag", "--list", "v[0-9]*").splitlines()
        if candidate != tag and semver_key(candidate) is not None
        and (current is None or (semver_key(candidate) or (0, 0, 0)) < current)
    ]
    tags.sort(key=lambda candidate: semver_key(candidate) or (0, 0, 0), reverse=True)
    return tags[0] if tags else None


def semver_key(tag: str) -> tuple[int, int, int] | None:
    match = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", tag)
    if not match:
        return None
    return tuple(int(part) for part in match.groups())


def commits_for_range(tag: str, previous_tag: str | None) -> list[Commit]:
    rev = f"{previous_tag}..{tag}" if previous_tag else tag
    if not ref_exists(tag):
        rev = "HEAD"
    output = git("log", "--format=%H%x01%s", rev)
    commits: list[Commit] = []
    for line in output.splitlines():
        sha, subject = line.split("\x01", 1)
        if subject.startswith("Merge "):
            continue
        commits.append(Commit(sha=sha[:7], subject=subject))
    return commits


def render_release_section(tag: str, previous_tag: str | None, commits: list[Commit]) -> str:
    compare = (
        f"[{previous_tag}...{tag}](https://github.com/{REPO}/compare/{previous_tag}...{tag})"
        if previous_tag
        else "Initial release history"
    )
    lines = [f"## {tag}", "", f"Comparison: {compare}", ""]
    lines.extend(render_asset_table(tag))
    lines.append("")
    lines.extend(render_commit_sections(commits))
    return "\n".join(lines).rstrip() + "\n"


def render_release_body(tag: str, previous_tag: str | None, commits: list[Commit]) -> str:
    compare = (
        f"[{previous_tag}...{tag}](https://github.com/{REPO}/compare/{previous_tag}...{tag})"
        if previous_tag
        else "Initial release history"
    )
    lines = [
        f"# Ktesio {tag}",
        "",
        "Install the archive for your platform, unpack it, and place `kt` on your PATH.",
        "",
        "## Downloads",
        "",
    ]
    lines.extend(render_asset_table(tag))
    lines += ["", "## Changes", "", f"Comparison: {compare}", ""]
    lines.extend(render_commit_sections(commits))
    return "\n".join(lines).rstrip() + "\n"


def render_commit_sections(commits: list[Commit]) -> list[str]:
    lines: list[str] = []
    grouped = group_commits(commits)
    for key, title in SECTION_ORDER:
        items = grouped.get(key, [])
        if not items:
            continue
        lines += [f"### {title}", ""]
        for commit in items:
            lines.append(
                f"- {clean_subject(commit.subject)} ([{commit.sha}](https://github.com/{REPO}/commit/{commit.sha}))"
            )
        lines.append("")

    if not commits:
        lines += ["### Changes", "", "- No commits found for this release range.", ""]

    return lines


def render_asset_table(tag: str) -> list[str]:
    lines = [
        "| Platform | Target | Archive | Checksum |",
        "|----------|--------|---------|----------|",
    ]
    for platform, target, extension in TARGETS:
        archive = f"ktesio-{tag}-{target}.{extension}"
        url = f"https://github.com/{REPO}/releases/download/{tag}/{archive}"
        checksum = f"{url}.sha256"
        lines.append(f"| {platform} | `{target}` | [{archive}]({url}) | [sha256]({checksum}) |")
    aggregate = f"https://github.com/{REPO}/releases/download/{tag}/ktesio-{tag}-checksums.txt"
    lines.append(f"| All | checksums | [ktesio-{tag}-checksums.txt]({aggregate}) | - |")
    return lines


def group_commits(commits: list[Commit]) -> dict[str, list[Commit]]:
    grouped: dict[str, list[Commit]] = {}
    for commit in commits:
        match = CONVENTIONAL_RE.match(commit.subject)
        key = match.group("type") if match else "other"
        if key not in {item[0] for item in SECTION_ORDER}:
            key = "other"
        grouped.setdefault(key, []).append(commit)
    return grouped


def clean_subject(subject: str) -> str:
    match = CONVENTIONAL_RE.match(subject)
    return match.group("summary") if match else subject


def upsert_release_section(path: Path, header: str, tag: str, section: str) -> None:
    text = path.read_text(encoding="utf-8") if path.exists() else header.rstrip() + "\n"
    pattern = re.compile(rf"^## {re.escape(tag)}\n.*?(?=^## |\Z)", re.DOTALL | re.MULTILINE)
    if pattern.search(text):
        text = pattern.sub(section, text)
    else:
        first_section = re.search(r"^## ", text, re.MULTILINE)
        if first_section:
            text = text[: first_section.start()] + section + "\n" + text[first_section.start():]
        else:
            text = text.rstrip() + "\n\n" + section
    path.write_text(text.rstrip() + "\n", encoding="utf-8")


def ref_exists(ref: str) -> bool:
    return subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", ref],
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    ).returncode == 0


def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True)


if __name__ == "__main__":
    raise SystemExit(main())
