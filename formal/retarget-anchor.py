#!/usr/bin/env python3
"""Rewrite a renamed source string everywhere a formal anchor names it.

Source-conformance scripts, dispatch contracts and the implementation-mutation
registry pin behaviour by literal source text. Renaming one function therefore
breaks anchors in several unrelated files, and finding them by hand was the
slowest part of a rename. This lists every anchor file that contains OLD and,
with --apply, replaces it with NEW; the registry preflight (`--check`) then
re-validates every occurrence count and anchor context.

Only files that exist to carry anchors are touched. Source code and docs are
never rewritten, and a string that occurs in no anchor file is reported rather
than guessed at.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
ANCHOR_GLOBS = (
    "formal/*.sh",
    "formal/*.py",
    "formal/*.toml",
    "formal/*.tsv",
)
SELF = Path(__file__).resolve()


def anchor_files() -> list[Path]:
    files: set[Path] = set()
    for pattern in ANCHOR_GLOBS:
        files.update(ROOT.glob(pattern))
    return sorted(path for path in files if path.is_file() and path.resolve() != SELF)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("old", help="literal text currently pinned by anchors")
    parser.add_argument("new", help="literal text that replaces it")
    parser.add_argument("--apply", action="store_true", help="write the replacement")
    args = parser.parse_args()
    if not args.old or args.old == args.new:
        parser.error("OLD must be non-empty and differ from NEW")

    total = 0
    for path in anchor_files():
        text = path.read_text(encoding="utf-8")
        count = text.count(args.old)
        if count == 0:
            continue
        total += count
        print(f"{path.relative_to(ROOT)}: {count} occurrence(s)")
        if args.apply:
            path.write_text(text.replace(args.old, args.new), encoding="utf-8")
    if total == 0:
        print("no anchor file contains the text; nothing to retarget", file=sys.stderr)
        return 1
    if args.apply:
        print(
            f"rewrote {total} occurrence(s); now run "
            "`python3 formal/run-implementation-mutations.py --check`"
        )
    else:
        print(f"{total} occurrence(s); re-run with --apply to rewrite them")
    return 0


if __name__ == "__main__":
    sys.exit(main())
