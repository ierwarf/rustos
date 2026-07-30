#!/usr/bin/env python3
"""Reject undocumented high-risk Rust boundaries and unbounded source debt."""

from __future__ import annotations

import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
CONTRACTS = ROOT / "formal/contracts.toml"
DEBT = ROOT / "formal/rust-source-debt.tsv"
LARGE_FILES = ROOT / "formal/rust-large-files.tsv"
LARGE_FILE_THRESHOLD = 1300
HEADER_SCAN_LINES = 100

HEADER_FIELDS = (
    "owner",
    "boundary",
    "lifecycle",
    "concurrency",
    "failure",
    "forbidden",
    "evidence",
)
DEAD_CODE_RATIONALES = (
    "ABI:",
    "ASSEMBLY:",
    "GENERATED:",
    "DIAGNOSTIC:",
    "LAYOUT:",
    "TEST-HARNESS:",
)
ORDERING_RE = re.compile(
    r"\bOrdering::(?:Acquire|Release|AcqRel|SeqCst)\b|"
    r"\b(?:compiler_)?fence\s*\("
)
UNSAFE_RE = re.compile(r"\bunsafe\s*\{")


@dataclass(frozen=True)
class Debt:
    undocumented_unsafe: int
    undocumented_ordering: int


def fail(errors: list[str], message: str) -> None:
    errors.append(message)


def tracked_rust_files() -> list[Path]:
    output = subprocess.check_output(
        [
            "git",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.rs",
        ],
        cwd=ROOT,
        text=True,
    )
    return [
        ROOT / relative
        for relative in output.splitlines()
        if relative and (ROOT / relative).is_file()
    ]


def risk_surface_paths() -> list[Path]:
    with CONTRACTS.open("rb") as source:
        contract = tomllib.load(source)
    return [ROOT / entry["path"] for entry in contract.get("risk_surfaces", [])]


def read_tsv(path: Path, columns: int) -> dict[str, list[str]]:
    rows: dict[str, list[str]] = {}
    for line_number, raw in enumerate(path.read_text().splitlines(), 1):
        if not raw or raw.startswith("#"):
            continue
        fields = raw.split("\t")
        if len(fields) != columns:
            raise ValueError(
                f"{path.relative_to(ROOT)}:{line_number}: expected "
                f"{columns} tab-separated columns, found {len(fields)}"
            )
        key = fields[0]
        if key in rows:
            raise ValueError(
                f"{path.relative_to(ROOT)}:{line_number}: duplicate {key}"
            )
        rows[key] = fields[1:]
    return rows


def preceding_has(lines: list[str], index: int, tag: str, window: int = 4) -> bool:
    start = max(0, index - window)
    return any(tag in line for line in lines[start:index])


def current_debt(lines: list[str]) -> Debt:
    undocumented_unsafe = 0
    undocumented_ordering = 0
    for index, line in enumerate(lines):
        if UNSAFE_RE.search(line) and not preceding_has(lines, index, "SAFETY:"):
            undocumented_unsafe += 1
        if ORDERING_RE.search(line) and not preceding_has(lines, index, "ORDERING:"):
            undocumented_ordering += 1
    return Debt(undocumented_unsafe, undocumented_ordering)


def check_high_risk_headers(
    paths: list[Path], errors: list[str]
) -> dict[str, Debt]:
    debts: dict[str, Debt] = {}
    for path in paths:
        relative = path.relative_to(ROOT).as_posix()
        if not path.is_file():
            fail(errors, f"critical/high source is absent: {relative}")
            continue
        lines = path.read_text(errors="replace").splitlines()
        header = "\n".join(lines[:HEADER_SCAN_LINES]).lower()
        if "//!" not in header:
            fail(errors, f"{relative}: missing leading module contract")
        for field in HEADER_FIELDS:
            if field not in header:
                fail(errors, f"{relative}: module contract omits {field}")
        debts[relative] = current_debt(lines)
    return debts


def check_debt_ledger(
    actual: dict[str, Debt], errors: list[str]
) -> None:
    try:
        registered = read_tsv(DEBT, 3)
    except (OSError, ValueError) as error:
        fail(errors, str(error))
        return

    if set(registered) != set(actual):
        missing = sorted(set(actual) - set(registered))
        stale = sorted(set(registered) - set(actual))
        for path in missing:
            fail(errors, f"{DEBT.relative_to(ROOT)}: missing risk surface {path}")
        for path in stale:
            fail(errors, f"{DEBT.relative_to(ROOT)}: stale risk surface {path}")

    for path, debt in actual.items():
        row = registered.get(path)
        if row is None:
            continue
        try:
            unsafe_limit, ordering_limit = map(int, row)
        except ValueError:
            fail(errors, f"{DEBT.relative_to(ROOT)}: non-integer debt for {path}")
            continue
        if debt.undocumented_unsafe > unsafe_limit:
            fail(
                errors,
                f"{path}: undocumented unsafe debt grew "
                f"{unsafe_limit} -> {debt.undocumented_unsafe}",
            )
        if debt.undocumented_ordering > ordering_limit:
            fail(
                errors,
                f"{path}: undocumented ordering debt grew "
                f"{ordering_limit} -> {debt.undocumented_ordering}",
            )


def check_large_files(files: list[Path], errors: list[str]) -> None:
    try:
        registered = read_tsv(LARGE_FILES, 4)
    except (OSError, ValueError) as error:
        fail(errors, str(error))
        return

    actual: dict[str, int] = {}
    for path in files:
        lines = path.read_text(errors="replace").splitlines()
        if len(lines) > LARGE_FILE_THRESHOLD:
            actual[path.relative_to(ROOT).as_posix()] = len(lines)

    for path, line_count in actual.items():
        row = registered.get(path)
        if row is None:
            fail(
                errors,
                f"{path}: {line_count} lines exceeds {LARGE_FILE_THRESHOLD} "
                "without a split-debt registry entry",
            )
            continue
        limit_text, owner, split_plan = row
        try:
            limit = int(limit_text)
        except ValueError:
            fail(errors, f"{LARGE_FILES.relative_to(ROOT)}: bad limit for {path}")
            continue
        if line_count > limit:
            fail(errors, f"{path}: large-file debt grew {limit} -> {line_count}")
        if not owner.strip() or not split_plan.strip():
            fail(errors, f"{path}: owner and split plan are mandatory")

    for path in sorted(set(registered) - set(actual)):
        fail(
            errors,
            f"{LARGE_FILES.relative_to(ROOT)}: remove resolved/stale row {path}",
        )


def check_production_markers(files: list[Path], errors: list[str]) -> None:
    forbidden = re.compile(r"\b(?:TODO|FIXME)\b|todo!\s*\(|unimplemented!\s*\(")
    for path in files:
        relative = path.relative_to(ROOT).as_posix()
        lines = path.read_text(errors="replace").splitlines()
        for index, line in enumerate(lines):
            if forbidden.search(line):
                fail(errors, f"{relative}:{index + 1}: unresolved production marker")
            if "allow(dead_code)" in line or "allow(dead_code," in line:
                context = "\n".join(lines[max(0, index - 3) : index])
                if not any(tag in context for tag in DEAD_CODE_RATIONALES):
                    fail(
                        errors,
                        f"{relative}:{index + 1}: dead_code needs a stable rationale",
                    )


def main() -> int:
    errors: list[str] = []
    files = tracked_rust_files()
    risks = risk_surface_paths()
    debt = check_high_risk_headers(risks, errors)
    check_debt_ledger(debt, errors)
    check_large_files(files, errors)
    check_production_markers(files, errors)

    if errors:
        for error in errors:
            print(f"rust source contract: {error}", file=sys.stderr)
        return 1
    print(
        "rust source contracts passed: "
        f"{len(files)} Rust files, {len(risks)} critical/high surfaces"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
