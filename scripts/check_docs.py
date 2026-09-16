#!/usr/bin/env python3
"""Validate Markdown links, JSON snippets, and stale repository references."""

from __future__ import annotations

import json
import re
import shlex
import sys
from pathlib import Path
from urllib.parse import unquote


ROOT = Path(__file__).resolve().parents[1]
DOC_PATHS = [
    *ROOT.glob("*.md"),
    *ROOT.glob("docs/*.md"),
    *ROOT.glob("docs/design/*.md"),
]
STALE_PATTERNS = [
    "github.com/imagdy/skills",
    "../specs/002-project-docs/quickstart.md",
    "specs/005-github-ci-pipeline",
    "docs/RELEASES_NOTES.md",
    # Git is not a runtime dependency — these warnings were removed from the
    # installers and the Homebrew formula (review 2026-02).
    "most kt commands need git at runtime",
    'depends_on "git"',
    # Older contract revisions must not resurface outside CHANGELOG/release notes
    # (narrow patterns so version-history files keep passing). The freeze policy
    # names the WHOLE 0.x seed family — 0.1.0/0.2.0 included (retro #165, D3).
    "contract_version = \"0.1.0\"",
    'contract_version "0.1.0"',
    "contract_version = \"0.2.0\"",
    'contract_version "0.2.0"',
    "contract_version = \"0.3.0\"",
    'contract_version "0.3.0"',
    "contract_version = \"0.4.0\"",
    'contract_version "0.4.0"',
    # Single-quote / compact spellings of the same stale seeds (review finding:
    # the guard listed only double-quoted spaced forms; TOML accepts all four).
    "contract_version = '0.1.0'",
    "contract_version='0.1.0'",
    "contract_version = '0.2.0'",
    "contract_version='0.2.0'",
    "contract_version = '0.3.0'",
    "contract_version='0.3.0'",
    "contract_version = '0.4.0'",
    "contract_version='0.4.0'",
    # Regression guards for two FIXED stale texts (epic-6 retro remediation):
    # the breach-event field list that omitted the dollar dimension (the fix
    # reads "… metering source, timestamp — the dimension is `tokens` or
    # `dollars` …"), and the hermes native-memory misstatement (commands).
    # Deliberately narrow: "(tokens only)" alone also appears in legitimate
    # token-total prose.
    "timestamp; tokens only",
    # Regression guard for the THIRD fixed stale text (epic-6 retro
    # remediation, PR #172): the hermes native-memory sentence claimed adapters
    # "own their memory entirely" — replaced by the delegation wording. NOTE:
    # STALE_PATTERNS are literal substrings, not regexes — the original entry
    # "own[s] their memory entirely" could therefore never match anything
    # (dead guard). The literal below is the verified historical stale text
    # (pickaxe: docs/commands.md at 613d4ec..4119db3 — no "owns" variant ever
    # existed), so the guard is live again.
    "own their memory entirely",
]

LINK_RE = re.compile(r"(?<!!)\[[^\]]+\]\(([^)]+)\)")
JSON_FENCE_RE = re.compile(r"```json\s*(.*?)```", re.DOTALL | re.IGNORECASE)
BASH_FENCE_RE = re.compile(r"```(?:bash|sh|shell)\s*(.*?)```", re.DOTALL | re.IGNORECASE)
# The CLI command allowlist is shared by every shipped binary name —
# `kt` (retired at 0.8.0, still allowed in historical/compat examples),
# `hemaka`, and `maka` run the same subcommand tree.
CLI_COMMAND_NAMES = ("kt", "hemaka", "maka")
KT_COMMANDS = {
    "self-update",
    "help",
    "agent",
}
# `kt agent` owns its own subcommand tree (crates/kt/src/main.rs `AgentCommands`).
# Model it the same way as the top level — an allowlist per nesting level — so the
# agent-runner surface validates without a blanket bypass.
AGENT_COMMANDS = {
    "register",
    "remove",
    "start",
    "stop",
    "pause",
    "resume",
    "send",
    "logs",
    "usage",
    "list",
    "show",
    "config",
    "memory",
}
# `kt agent config` nests one level deeper (`ConfigCommands`).
CONFIG_COMMANDS = {"get", "set"}
# `kt agent memory` nests one level deeper (`MemoryCommands`, story 5-1).
MEMORY_COMMANDS = {"attach", "detach"}


def main() -> int:
    errors: list[str] = []

    for path in DOC_PATHS:
        text = path.read_text(encoding="utf-8")
        rel_path = path.relative_to(ROOT)

        for pattern in STALE_PATTERNS:
            if pattern in text:
                errors.append(f"{rel_path}: stale reference `{pattern}`")

        for match in LINK_RE.finditer(text):
            target = match.group(1).strip()
            validate_link(path, target, errors)

        for index, match in enumerate(JSON_FENCE_RE.finditer(text), start=1):
            snippet = match.group(1).strip()
            if not snippet:
                continue
            try:
                json.loads(snippet)
            except json.JSONDecodeError as exc:
                errors.append(f"{rel_path}: invalid JSON fence #{index}: {exc}")

        for index, match in enumerate(BASH_FENCE_RE.finditer(text), start=1):
            validate_command_examples(path, index, match.group(1), errors)

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1

    print(f"Validated {len(DOC_PATHS)} Markdown files")
    return 0


def validate_link(path: Path, target: str, errors: list[str]) -> None:
    if (
        target.startswith(("http://", "https://", "mailto:", "#"))
        or target.startswith("app://")
        or target.startswith("file:")
    ):
        return

    clean_target = target.split("#", 1)[0].split("?", 1)[0]
    if not clean_target:
        return

    clean_target = unquote(clean_target)
    candidate = (path.parent / clean_target).resolve()

    try:
        candidate.relative_to(ROOT)
    except ValueError:
        errors.append(f"{path.relative_to(ROOT)}: link escapes repo: {target}")
        return

    if not candidate.exists():
        errors.append(f"{path.relative_to(ROOT)}: broken link: {target}")


def validate_command_examples(
    path: Path, fence_index: int, snippet: str, errors: list[str]
) -> None:
    rel_path = path.relative_to(ROOT)
    for line_number, raw_line in enumerate(snippet.splitlines(), start=1):
        line = raw_line.strip()
        if not line or line.startswith("#") or line.endswith("\\"):
            continue
        if not any(name in line for name in CLI_COMMAND_NAMES):
            continue
        try:
            tokens = shlex.split(line)
        except ValueError as exc:
            errors.append(
                f"{rel_path}: shell fence #{fence_index}, line {line_number}: cannot parse command: {exc}"
            )
            continue
        if not tokens:
            continue

        command_index = 0
        while command_index < len(tokens) and "=" in tokens[command_index]:
            command_index += 1
        if command_index >= len(tokens):
            continue

        binary = tokens[command_index]
        if not any(
            binary == name or binary.endswith(f"/{name}")
            for name in CLI_COMMAND_NAMES
        ):
            continue

        if len(tokens) <= command_index + 1:
            errors.append(
                f"{rel_path}: shell fence #{fence_index}, line {line_number}: CLI example is missing a command"
            )
            continue

        command = tokens[command_index + 1]
        if command.startswith("-"):
            continue
        if command not in KT_COMMANDS:
            errors.append(
                f"{rel_path}: shell fence #{fence_index}, line {line_number}: unknown `kt` command `{command}`"
            )
            continue
        if command == "agent":
            validate_agent_subcommands(
                rel_path, fence_index, line_number, tokens[command_index + 2 :], errors
            )


def first_non_flag(tokens: list[str]) -> tuple[int, str] | None:
    """Return the (index, token) of the first non-flag token, or None if every token
    is a flag (or the list is empty)."""
    for index, token in enumerate(tokens):
        if not token.startswith("-"):
            return index, token
    return None


def validate_agent_subcommands(
    rel_path: Path,
    fence_index: int,
    line_number: int,
    rest: list[str],
    errors: list[str],
) -> None:
    """Validate the `kt agent` subcommand tree (crates/kt/src/main.rs `AgentCommands`
    / `ConfigCommands`), mirroring the top-level allowlist at each nesting level.

    `rest` is the tokens AFTER `kt agent`. A flag-only tail (e.g. `kt agent --help`)
    is left to clap and not flagged; only a present-but-unknown subcommand is an error.
    """
    found = first_non_flag(rest)
    if found is None:
        return
    index, subcommand = found
    if subcommand not in AGENT_COMMANDS:
        errors.append(
            f"{rel_path}: shell fence #{fence_index}, line {line_number}: unknown `kt agent` command `{subcommand}`"
        )
        return
    if subcommand == "config":
        config_found = first_non_flag(rest[index + 1 :])
        if config_found is None:
            return
        _, config_subcommand = config_found
        if config_subcommand not in CONFIG_COMMANDS:
            errors.append(
                f"{rel_path}: shell fence #{fence_index}, line {line_number}: "
                f"unknown `kt agent config` command `{config_subcommand}`"
            )
    if subcommand == "memory":
        memory_found = first_non_flag(rest[index + 1 :])
        if memory_found is None:
            return
        _, memory_subcommand = memory_found
        if memory_subcommand not in MEMORY_COMMANDS:
            errors.append(
                f"{rel_path}: shell fence #{fence_index}, line {line_number}: "
                f"unknown `kt agent memory` command `{memory_subcommand}`"
            )


if __name__ == "__main__":
    raise SystemExit(main())
