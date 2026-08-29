#!/usr/bin/env python3
"""Reject a #[test] fn added inside a `test = false` [[bin]] target.

A no_std/no_main service binary that sets `test = false` on its [[bin]] entry
never builds or runs that target's #[test] fns under `cargo test`. They read
as coverage and provide none. Each existing case is a registered, counted
debt row in dead-test-debt.tsv; this check fails closed on any new
occurrence, any growth past the registered count, or any stale row whose
binary no longer has a dead test.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEBT = ROOT / "formal/dead-test-debt.tsv"
TEST_RE = re.compile(r"^\s*#\[test\]", re.MULTILINE)
SKIP_DIR_NAMES = {"target", "build"}


def load_debt() -> dict[str, int]:
    debt: dict[str, int] = {}
    for lineno, raw in enumerate(DEBT.read_text().splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) < 2:
            raise SystemExit(f"{DEBT}:{lineno}: expected path\\tlimit\\treason")
        path, limit = fields[0], fields[1]
        debt[path] = int(limit)
    return debt


def main() -> int:
    debt = load_debt()
    errors: list[str] = []
    seen: set[str] = set()

    for manifest in sorted(ROOT.glob("**/Cargo.toml")):
        if SKIP_DIR_NAMES & set(manifest.parts):
            continue
        data = tomllib.loads(manifest.read_text())
        for bin_entry in data.get("bin", []):
            if bin_entry.get("test", True):
                continue
            bin_path = manifest.parent / bin_entry.get("path", "src/main.rs")
            if not bin_path.is_file():
                continue
            count = len(TEST_RE.findall(bin_path.read_text()))
            if count == 0:
                continue
            rel = bin_path.relative_to(ROOT).as_posix()
            seen.add(rel)
            limit = debt.get(rel)
            if limit is None:
                errors.append(
                    f"{rel}: {count} #[test] fn(s) inside a `test = false` "
                    "binary target never run under cargo test; move them "
                    "into lib.rs or add a registered row to "
                    "formal/dead-test-debt.tsv"
                )
            elif count > limit:
                errors.append(
                    f"{rel}: dead-test count grew to {count}, above the "
                    f"registered limit {limit} in formal/dead-test-debt.tsv; "
                    "do not add tests to a dead binary target"
                )

    for rel in sorted(set(debt) - seen):
        errors.append(
            f"{rel}: registered in formal/dead-test-debt.tsv but no longer "
            "has a dead #[test]; remove the stale row"
        )

    if errors:
        for message in errors:
            print(message, file=sys.stderr)
        return 1

    print(
        f"dead-test debt check passed: {len(seen)} registered binaries, "
        f"{sum(debt.values())} known-dead tests"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
