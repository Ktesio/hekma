#!/usr/bin/env python3
"""Idempotent GitHub sync for the Ktesio BMAD pivot plan.

Parses _bmad-output/planning-artifacts/epics.md (source of truth), then:
  1. ensures the BMAD label set exists
  2. ensures the "Ktesio" GitHub Project exists (owner iMagdy) and is linked
  3. creates one issue per epic and per story (skips ones whose exact title already exists)
  4. adds every issue to the project
  5. rewrites each epic issue body with a task list linking its story issues
  6. writes github-sync-map.json next to this script

Re-runnable: every step checks before it creates. Run from the repo root.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EPICS = ROOT / "_bmad-output/planning-artifacts/epics.md"
MAP_FILE = Path(__file__).resolve().parent / "github-sync-map.json"
REPO = "Ktesio/ktesio"
OWNER = "iMagdy"
PROJECT_TITLE = "Ktesio"

LABELS = [
    ("epic", "3E4B9E", "BMAD epic tracking issue"),
    ("story", "0E8A16", "BMAD story tracking issue"),
    ("pivot", "D93F0B", "Agent-runner repositioning work"),
    ("status:backlog", "EDEDED", "Story only exists in the epic file"),
    ("status:ready-for-dev", "C2E0C6", "Story file created, ready for development"),
    ("status:in-progress", "FBCA04", "Actively being implemented"),
    ("status:review", "5319E7", "Awaiting code review"),
]


def run(args: list[str], check: bool = True) -> str:
    res = subprocess.run(args, capture_output=True, text=True)
    if check and res.returncode != 0:
        raise RuntimeError(f"{' '.join(args)}\n{res.stderr.strip()}")
    return res.stdout.strip()


def kebab(title: str) -> str:
    t = title.replace("'", "")
    t = re.sub(r"[^a-zA-Z0-9]+", "-", t.lower())
    return t.strip("-")


def parse_epics() -> tuple[list[dict], list[dict]]:
    text = EPICS.read_text(encoding="utf-8")
    body = text.split("\n## Epic 1:", 1)
    body = "\n## Epic 1:" + body[1]
    epics, stories = [], []
    epic_blocks = re.split(r"\n## Epic (\d+): ", body)
    for i in range(1, len(epic_blocks), 2):
        num = int(epic_blocks[i])
        block = epic_blocks[i + 1]
        title, rest = block.split("\n", 1)
        story_parts = re.split(r"\n### Story (\d+)\.(\d+): ", rest)
        goal = story_parts[0].strip()
        epics.append({"n": num, "title": title.strip(), "goal": goal})
        for j in range(1, len(story_parts), 3):
            s_epic, s_num = int(story_parts[j]), int(story_parts[j + 1])
            s_block = story_parts[j + 2]
            s_title, s_body = s_block.split("\n", 1)
            key = f"{s_epic}-{s_num}-{kebab(s_title)}"
            stories.append({
                "epic": s_epic, "m": s_num, "title": s_title.strip(),
                "key": key, "body": s_body.strip(),
            })
    return epics, stories


def existing_issue_numbers_by_title() -> dict[str, int]:
    out = run(["gh", "issue", "list", "--repo", REPO, "--state", "all",
               "--limit", "200", "--json", "number,title"])
    return {it["title"]: it["number"] for it in json.loads(out)}


def ensure_labels() -> None:
    have = set(run(["gh", "label", "list", "--repo", REPO, "--json", "name",
                    "-q", ".[].name"]).split("\n"))
    for name, color, desc in LABELS:
        if name not in have:
            run(["gh", "label", "create", name, "--repo", REPO,
                 "--color", color, "--description", desc])
            print(f"label created: {name}")


def ensure_project() -> str:
    out = run(["gh", "project", "list", "--owner", OWNER, "--format", "json"])
    for p in json.loads(out).get("projects", []):
        if p["title"] == PROJECT_TITLE and not p.get("closed"):
            print(f"project exists: #{p['number']}")
            return str(p["number"])
    out = run(["gh", "project", "create", "--owner", OWNER,
               "--title", PROJECT_TITLE, "--format", "json"])
    number = str(json.loads(out)["number"])
    print(f"project created: #{number}")
    run(["gh", "project", "link", number, "--owner", OWNER, "--repo", REPO])
    print("project linked to repo")
    return number


def project_item_urls(number: str) -> set[str]:
    out = run(["gh", "project", "item-list", number, "--owner", OWNER,
               "--format", "json", "--limit", "200"])
    urls = set()
    for it in json.loads(out).get("items", []):
        content = it.get("content") or {}
        if content.get("url"):
            urls.add(content["url"])
    return urls


def main() -> int:
    epics, stories = parse_epics()
    print(f"parsed: {len(epics)} epics, {len(stories)} stories")
    assert len(epics) == 8 and len(stories) == 37, "unexpected counts — aborting"

    ensure_labels()
    project = ensure_project()
    titles = existing_issue_numbers_by_title()
    mapping: dict = {"repo": REPO, "project": {"owner": OWNER, "title": PROJECT_TITLE,
                     "number": int(project)}, "epics": {}, "stories": {}}

    for e in epics:
        title = f"[epic-{e['n']}] Epic {e['n']}: {e['title']}"
        if title in titles:
            num = titles[title]
            print(f"epic exists: #{num} {title}")
        else:
            body = (f"{e['goal']}\n\n**Stories:** _task list added after story issues are created._\n\n"
                    f"---\n**BMAD key:** `epic-{e['n']}` · **Source:** `_bmad-output/planning-artifacts/epics.md` "
                    f"(gitignored planning artifact) · Managed by BMAD sync — edit the source, not this block.")
            url = run(["gh", "issue", "create", "--repo", REPO, "--title", title,
                       "--body", body, "--label", "epic,pivot"])
            num = int(url.rsplit("/", 1)[1])
            print(f"epic created: #{num} {title}")
        mapping["epics"][f"epic-{e['n']}"] = num

    for s in stories:
        title = f"[{s['epic']}-{s['m']}] Story {s['epic']}.{s['m']}: {s['title']}"
        if title in titles:
            num = titles[title]
            print(f"story exists: #{num}")
        else:
            epic_num = mapping["epics"][f"epic-{s['epic']}"]
            body = (f"{s['body']}\n\n---\n**Epic:** #{epic_num} · **BMAD key:** `{s['key']}` · "
                    f"**Sprint status:** backlog · **Source:** `_bmad-output/planning-artifacts/epics.md` "
                    f"(gitignored planning artifact) · Managed by BMAD sync — edit the source, not this block.")
            url = run(["gh", "issue", "create", "--repo", REPO, "--title", title,
                       "--body", body, "--label", "story,pivot,status:backlog"])
            num = int(url.rsplit("/", 1)[1])
            print(f"story created: #{num} {title}")
        mapping["stories"][s["key"]] = num

    in_project = project_item_urls(project)
    for kind in ("epics", "stories"):
        for key, num in mapping[kind].items():
            url = f"https://github.com/{REPO}/issues/{num}"
            if url not in in_project:
                run(["gh", "project", "item-add", project, "--owner", OWNER, "--url", url])
                print(f"added to project: #{num}")

    for e in epics:
        epic_key = f"epic-{e['n']}"
        num = mapping["epics"][epic_key]
        story_lines = "\n".join(
            f"- [ ] #{mapping['stories'][s['key']]} Story {s['epic']}.{s['m']}: {s['title']}"
            for s in stories if s["epic"] == e["n"])
        body = (f"{e['goal']}\n\n**Stories:**\n\n{story_lines}\n\n"
                f"---\n**BMAD key:** `{epic_key}` · **Source:** `_bmad-output/planning-artifacts/epics.md` "
                f"(gitignored planning artifact) · Managed by BMAD sync — edit the source, not this block.")
        run(["gh", "issue", "edit", str(num), "--repo", REPO, "--body", body])
        print(f"epic body updated with story links: #{num}")

    MAP_FILE.write_text(json.dumps(mapping, indent=2) + "\n", encoding="utf-8")
    print(f"map written: {MAP_FILE}")
    print("SYNC COMPLETE")
    return 0


if __name__ == "__main__":
    sys.exit(main())
