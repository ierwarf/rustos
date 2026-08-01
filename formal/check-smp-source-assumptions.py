#!/usr/bin/env python3
"""Reject mechanically detectable BSP-only state and ownership regressions."""

from __future__ import annotations

import fnmatch
import hashlib
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
CONTRACT = ROOT / "formal/smp-source-contracts.toml"
SUMMARY = ROOT / "build/formal/smp-source-assumptions/summary.json"
UNSAFE_IMPL = re.compile(r"\bunsafe\s+impl(?:\s*<[^;{]*?>)?\s+(?:Send|Sync)\b")


def tracked_kernel_rust() -> list[Path]:
    output = subprocess.check_output(
        [
            "git",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "kernel/**/*.rs",
            "kernel/*.rs",
        ],
        cwd=ROOT,
        text=True,
    )
    return sorted(
        ROOT / relative
        for relative in output.splitlines()
        if relative and (ROOT / relative).is_file()
    )


def has_preceding_safety(lines: list[str], index: int) -> bool:
    start = max(0, index - 5)
    return any("SAFETY:" in line for line in lines[start:index])


def source_digest(paths: list[Path]) -> str:
    digest = hashlib.sha256()
    for path in [CONTRACT, *paths]:
        relative = path.relative_to(ROOT).as_posix().encode()
        digest.update(relative)
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def main() -> int:
    manifest = tomllib.loads(CONTRACT.read_text(encoding="utf-8"))
    if manifest.get("schema") != "rustos-smp-source-contracts-v1":
        raise ValueError("unsupported SMP source-contract schema")
    paths = tracked_kernel_rust()
    by_relative = {path.relative_to(ROOT).as_posix(): path for path in paths}
    errors: list[str] = []
    checks = 0

    for entry in manifest.get("forbidden_patterns", []):
        scope = entry["scope"]
        pattern = re.compile(entry["regex"])
        matched_scope = False
        for relative, path in by_relative.items():
            if not fnmatch.fnmatchcase(relative, scope):
                continue
            matched_scope = True
            text = path.read_text(encoding="utf-8", errors="replace")
            for match in pattern.finditer(text):
                line = text.count("\n", 0, match.start()) + 1
                errors.append(
                    f"{relative}:{line}: {entry['reason']}: {match.group(0)!r}"
                )
            checks += 1
        if not matched_scope:
            errors.append(f"SMP source rule scope matched no files: {scope}")

    for entry in manifest.get("required_sequences", []):
        relative = entry["path"]
        path = by_relative.get(relative)
        checks += 1
        if path is None:
            errors.append(f"registered SMP sequence source is absent: {relative}")
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        cursor = 0
        for token in entry["tokens"]:
            position = text.find(token, cursor)
            if position < 0:
                errors.append(
                    f"{relative}: required SMP sequence is missing/out of order: {token!r}"
                )
                break
            cursor = position + len(token)

    unsafe_impls = 0
    for relative, path in by_relative.items():
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        for index, line in enumerate(lines):
            if not UNSAFE_IMPL.search(line):
                continue
            unsafe_impls += 1
            checks += 1
            if not has_preceding_safety(lines, index):
                errors.append(
                    f"{relative}:{index + 1}: unsafe Send/Sync impl lacks a "
                    "nearby SAFETY ownership argument"
                )

    per_cpu_statics = 0
    observed_static_keys: set[tuple[str, str]] = set()
    for entry in manifest.get("per_cpu_statics", []):
        relative = entry["path"]
        path = by_relative.get(relative)
        if path is None:
            errors.append(f"registered per-CPU source is absent: {relative}")
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        capacity = entry["capacity"]
        for name in entry["names"]:
            key = (relative, name)
            if key in observed_static_keys:
                errors.append(f"duplicate per-CPU static contract: {relative}:{name}")
                continue
            observed_static_keys.add(key)
            declaration = re.compile(
                rf"\bstatic\s+{re.escape(name)}\s*:\s*(?P<type>.*?)=",
                re.DOTALL,
            )
            matches = list(declaration.finditer(text))
            checks += 1
            per_cpu_statics += 1
            if len(matches) != 1:
                errors.append(
                    f"{relative}: expected exactly one static {name}, found {len(matches)}"
                )
                continue
            declared_type = matches[0].group("type")
            if "[" not in declared_type or "]" not in declared_type:
                errors.append(f"{relative}:{name}: per-CPU authority regressed to a scalar")
            if capacity not in declared_type:
                errors.append(
                    f"{relative}:{name}: declaration is not bounded by {capacity}"
                )

    SUMMARY.parent.mkdir(parents=True, exist_ok=True)
    summary = {
        "schema": "rustos-smp-source-assumption-evidence-v1",
        "status": "passed" if not errors else "failed",
        "source_sha256": source_digest(paths),
        "checks": checks,
        "kernel_rust_files": len(paths),
        "unsafe_send_sync_impls": unsafe_impls,
        "per_cpu_statics": per_cpu_statics,
        "errors": errors,
    }
    SUMMARY.write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print(
        "SMP source assumptions passed "
        f"checks={checks} files={len(paths)} per_cpu_statics={per_cpu_statics}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
