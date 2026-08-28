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


# "Immediately before, in the same critical section" as a number. Wide enough
# for the intervening lines these sites actually have, narrow enough that a
# precondition moved to another function or another lock hold fails.
AUDITED_FAIL_STOP_MAX_DISTANCE = 1200


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

    # An admission predicate and the commit that spends what it admitted read
    # the same state at two different times, so they may disagree. A caller that
    # fail-stops on that disagreement turns a policy outcome into a panic; both
    # 8-vCPU scheduler panics fixed in this class had exactly that shape. The
    # rule is therefore mechanical: a registered commit may not appear inside an
    # `assert!`, an `expect`, or a `panic!` guard at any call site in the tree.
    admit_commit_pairs = 0
    for entry in manifest.get("admit_commit_pairs", []):
        admit_commit_pairs += 1
        checks += 1
        name = entry["name"]
        for role in ("admit", "commit"):
            relative = entry[f"{role}_path"]
            path = by_relative.get(relative)
            if path is None:
                errors.append(f"admit/commit pair {name}: {role} source is absent: {relative}")
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            if entry[f"{role}_token"] not in text:
                errors.append(
                    f"admit/commit pair {name}: {role} token is missing from "
                    f"{relative}: {entry[f'{role}_token']!r}"
                )
        fail_stop = re.compile(
            r"(?:assert!|panic!|\.expect)\s*\([^;]{0,400}?"
            + re.escape(entry["commit_call"])
        )
        for relative, path in by_relative.items():
            text = path.read_text(encoding="utf-8", errors="replace")
            for match in fail_stop.finditer(text):
                line = text.count("\n", 0, match.start()) + 1
                errors.append(
                    f"{relative}:{line}: admit/commit pair {name} is fail-stopped "
                    f"at its call site; a refused commit is a policy outcome and "
                    f"the caller must fall back instead"
                )

    # The other half of the rule above. A fail-stop on a commit is sound when
    # the precondition it asserts is established inside the same critical
    # section, immediately before it, leaving no window for the two to disagree
    # across. Recording that keeps a completed audit from being redone, and
    # pinning both statements plus their distance makes the entry go stale --
    # rather than silently becoming the defect -- if anyone moves the
    # establishing statement away from the commit it guards.
    audited_fail_stops = 0
    for entry in manifest.get("audited_fail_stops", []):
        audited_fail_stops += 1
        checks += 1
        name = entry["name"]
        relative = entry["path"]
        path = by_relative.get(relative)
        if path is None:
            errors.append(f"audited fail-stop {name}: source is absent: {relative}")
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        establishes = text.find(entry["establishes"])
        fail_stop = text.find(entry["fail_stop"], max(establishes, 0))
        if establishes < 0:
            errors.append(
                f"audited fail-stop {name}: the establishing statement is gone "
                f"from {relative}: {entry['establishes']!r}"
            )
            continue
        if fail_stop < 0:
            errors.append(
                f"audited fail-stop {name}: the fail-stop no longer follows its "
                f"establishing statement in {relative}: {entry['fail_stop']!r}"
            )
            continue
        distance = fail_stop - establishes
        if distance > AUDITED_FAIL_STOP_MAX_DISTANCE:
            errors.append(
                f"{relative}: audited fail-stop {name} is now {distance} characters "
                f"after the statement that establishes it; re-audit it as an "
                f"admit/commit pair or move them back together"
            )

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
        f"checks={checks} files={len(paths)} per_cpu_statics={per_cpu_statics} "
        f"admit_commit_pairs={admit_commit_pairs} "
        f"audited_fail_stops={audited_fail_stops}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
