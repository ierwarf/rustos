#!/usr/bin/env python3
"""Reject colliding or non-canonical native RustOS syscall numbers."""

from __future__ import annotations

import re
import sys
from collections import defaultdict
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOURCES = [ROOT / "libs/rustos-user-abi/src/syscall.rs"]
SOURCES.extend(sorted((ROOT / "libs/rustos-user-abi/src/syscall").glob("*.rs")))
DECLARATION = re.compile(
    r"pub const\s+(SYS_RUSTOS_[A-Z0-9_]+)\s*:\s*u64\s*=\s*([^;]+);",
    re.MULTILINE,
)
LITERAL = re.compile(r"0x[0-9a-fA-F_]+")
NATIVE_PREFIX = 0x5255_0000


def main() -> int:
    by_name: dict[str, tuple[int, Path]] = {}
    by_number: defaultdict[int, list[tuple[str, Path]]] = defaultdict(list)
    invalid: list[str] = []

    for source in SOURCES:
        text = source.read_text(encoding="utf-8")
        for name, expression in DECLARATION.findall(text):
            expression = expression.strip()
            if not LITERAL.fullmatch(expression):
                invalid.append(f"{source.relative_to(ROOT)}: {name} is not one exact hex literal")
                continue
            number = int(expression.replace("_", ""), 16)
            if name in by_name:
                invalid.append(f"duplicate native syscall declaration: {name}")
                continue
            if number & 0xFFFF_0000 != NATIVE_PREFIX:
                invalid.append(
                    f"{source.relative_to(ROOT)}: {name}={number:#x} is outside 0x5255_XXXX"
                )
            by_name[name] = (number, source)
            by_number[number].append((name, source))

    if not by_name:
        invalid.append("no native syscall declarations found")
    for number, owners in sorted(by_number.items()):
        if len(owners) > 1:
            rendered = ", ".join(
                f"{name} ({source.relative_to(ROOT)})" for name, source in owners
            )
            invalid.append(f"native syscall collision {number:#x}: {rendered}")

    if invalid:
        print("\n".join(invalid), file=sys.stderr)
        return 1
    print(f"native syscall numbers are unique count={len(by_name)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
