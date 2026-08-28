#!/usr/bin/env python3
"""Prove that every binding-exempt path is not a verification input.

The verification-run binding hashes the source tree so a sealed result names the
exact tree it was produced from. `formal/binding-exempt-paths.txt` removes a few
prose documents from that hash. This checker is what keeps that sound: an exempt
path must exist, must be tracked, and must be mentioned by nothing under
`formal/` or `tools/`. The moment a document becomes an input -- parsed by a
checker, cited as evidence, or read by xtask -- this fails until the exemption is
withdrawn.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(
    subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip()
)
EXEMPT_LIST = ROOT / "formal/binding-exempt-paths.txt"
SCANNED_ROOTS = ("formal", "tools")


def exempt_paths() -> list[str]:
    lines = EXEMPT_LIST.read_text(encoding="utf-8").splitlines()
    return [line.strip() for line in lines if line.strip() and not line.startswith("#")]


def tracked_files() -> set[str]:
    output = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
        text=True,
    )
    return {entry for entry in output.split("\0") if entry}


def main() -> int:
    errors: list[str] = []
    tracked = tracked_files()
    paths = exempt_paths()
    if not paths:
        print("binding exemptions passed count=0")
        return 0
    if paths != sorted(set(paths), key=paths.index) or len(paths) != len(set(paths)):
        errors.append("binding-exempt-paths.txt contains a duplicate entry")

    # One pass over the scanned roots rather than one grep per exempt path: the
    # question is symmetric and the roots are small.
    haystack: list[tuple[str, str]] = []
    for scanned in SCANNED_ROOTS:
        for path in sorted((ROOT / scanned).rglob("*")):
            relative = path.relative_to(ROOT).as_posix()
            # The list itself necessarily names every exempt path; it is the
            # declaration, not a use of one.
            if not path.is_file() or relative not in tracked or path == EXEMPT_LIST:
                continue
            try:
                haystack.append((relative, path.read_text(encoding="utf-8")))
            except (UnicodeDecodeError, OSError):
                continue

    for exempt in paths:
        if exempt not in tracked:
            errors.append(f"binding-exempt path is not a tracked source file: {exempt}")
            continue
        basename = exempt.rsplit("/", 1)[-1]
        for relative, text in haystack:
            if exempt in text:
                errors.append(
                    f"{relative}: mentions binding-exempt path {exempt}; it is a "
                    "verification input and must not be exempt from the binding"
                )
            elif basename in text:
                errors.append(
                    f"{relative}: mentions {basename}, which may be the exempt "
                    f"path {exempt}; resolve the reference or withdraw the exemption"
                )

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print(f"binding exemptions passed count={len(paths)} scanned={len(haystack)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
