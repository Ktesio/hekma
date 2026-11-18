#!/usr/bin/env python3
"""The AD-17 lock-debt ratchet (AD-18 Step 2).

Every bare `.lock()`, and every unordered supervisor lock (`.alone()`), in
`crates/hekma-engine/src/engine.rs` must sit in a function listed in
`.ad17-debt.txt` with exactly that many sites. Sites are
anchored by SYMBOL (`Type::function`), never by line number, so unrelated
edits never trip it. The ratchet only turns one way:

- a new bare `.lock()` in an unlisted function fails;
- more sites than listed fails;
- fewer sites than listed fails too, so the debt file is lowered in the same
  change that pays it down.

The named scopes in `domain/bounded.rs` (`critical`, `after`, `cell_scope`)
are not debt: they are where the engine's locks are meant to be taken.

Usage: python3 scripts/check_ad17_debt.py [--write]
"""

from __future__ import annotations

import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "crates/hekma-engine/src/engine.rs"
DEBT = ROOT / ".ad17-debt.txt"

IMPL = re.compile(r"^impl(?:<[^>]*>)?\s+(?:[\w:]+\s+for\s+)?([A-Za-z_][\w]*)")
FN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_]\w*)")
MOD_TESTS = re.compile(r"^\s*mod\s+tests\b")
BARE_LOCK = re.compile(r"\.lock\(\)|\.alone\(\)")


def sites(text: str) -> Counter[str]:
    """Bare `.lock()` sites per `Type::function` (`fn` for free functions;
    `tests::fn` inside a test module)."""
    counts: Counter[str] = Counter()
    owner = ""
    function = ""
    for line in text.splitlines():
        stripped = line.split("//", 1)[0]
        # Top-level items only: an `impl` opens an owner, a column-0 `}`
        # closes it, and a test module owns what is inside it.
        if line.startswith("impl"):
            match = IMPL.match(line)
            owner = match.group(1) if match else ""
        elif line.startswith("}"):
            owner = ""
        elif MOD_TESTS.match(line) and not line.startswith(" "):
            owner = "tests"
        fn = FN.match(line)
        if fn:
            function = fn.group(1)
        if BARE_LOCK.search(stripped):
            counts[f"{owner}::{function}" if owner else function] += 1
    return counts


def read_debt(path: Path) -> Counter[str]:
    debt: Counter[str] = Counter()
    for raw in path.read_text().splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        symbol, count = line.rsplit(maxsplit=1)
        debt[symbol] = int(count)
    return debt


def check(found: Counter[str], debt: Counter[str]) -> list[str]:
    errors = []
    for symbol in sorted(set(found) | set(debt)):
        have, allowed = found.get(symbol, 0), debt.get(symbol, 0)
        if have > allowed:
            errors.append(
                f"{symbol}: {have} bare .lock() site(s), {allowed} allowed; take the lock "
                "through a named scope in domain/bounded.rs (critical / after / cell_scope)"
            )
        elif have < allowed:
            errors.append(
                f"{symbol}: {have} bare .lock() site(s), {allowed} listed; lower "
                ".ad17-debt.txt in this change (the ratchet only turns down)"
            )
    return errors


def render(found: Counter[str]) -> str:
    lines = [
        "# The AD-17 lock debt: bare `.lock()` sites left in",
        "# crates/hekma-engine/src/engine.rs, per symbol. Checked by",
        "# scripts/check_ad17_debt.py; it may only go down.",
    ]
    lines += [f"{symbol} {count}" for symbol, count in sorted(found.items())]
    return "\n".join(lines) + "\n"


SAMPLE = """\
impl Engine {
    pub fn start(&self) {
        let registry = self.inner.registry.critical();
        let supervisor = self.inner.supervisor.after(&registry);
    }

    fn peek(&self) {
        self.inner.supervisor.alone().mode();
    }
}

fn free() {
    let _ = CELL.lock();
}
"""


def self_test() -> int:
    """The ratchet's mutation cases: it must catch each of these."""
    found = sites(SAMPLE)
    assert found == Counter({"Engine::peek": 1, "free": 1}), found
    debt = Counter({"Engine::peek": 1, "free": 1})
    assert check(found, debt) == [], "the listed debt passes"
    # A new unordered lock in an unlisted function fails.
    grown = sites(SAMPLE.replace("let registry = self.inner.registry.critical();",
                                 "let registry = self.inner.registry.lock();"))
    assert any("Engine::start" in e for e in check(grown, debt)), grown
    # A second site in a listed function fails.
    doubled = sites(SAMPLE.replace(".mode();", ".mode();\n        X.lock();"))
    assert any("Engine::peek: 2" in e for e in check(doubled, debt)), doubled
    # A paid-down site must be removed from the file.
    paid = sites(SAMPLE.replace("self.inner.supervisor.alone().mode();", ""))
    assert any("lower .ad17-debt.txt" in e for e in check(paid, debt)), paid
    # Anchored by symbol: shifting every line changes nothing.
    assert sites("\n\n\n" + SAMPLE) == found
    # Comments are not sites.
    assert sites(SAMPLE + "// x.lock()\n") == found
    print("ad17 debt self-test: ok")
    return 0


def main(argv: list[str]) -> int:
    if "--self-test" in argv:
        return self_test()
    found = sites(SOURCE.read_text())
    if "--write" in argv:
        DEBT.write_text(render(found))
        print(f"wrote {DEBT.name}: {sum(found.values())} site(s) in {len(found)} symbol(s)")
        return 0
    errors = check(found, read_debt(DEBT))
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print(f"ad17 debt: ok ({sum(found.values())} bare site(s) in {len(found)} symbol(s))")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
