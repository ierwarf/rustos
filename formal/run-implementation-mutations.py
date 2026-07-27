#!/usr/bin/env python3
"""Prove that critical implementation-contract regressions are detected."""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


FIELDS = (
    "id",
    "severity",
    "source",
    "find",
    "replace",
    "occurrence",
    "package",
    "features",
    "target",
    "test",
    "max_ms",
)


def read_registry(root: Path) -> list[dict[str, str | int]]:
    path = root / "formal/implementation-mutations.tsv"
    mutations: list[dict[str, str | int]] = []
    identities: set[str] = set()
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw or raw.startswith("#"):
            continue
        fields = raw.split("\t")
        if len(fields) != len(FIELDS):
            raise SystemExit(f"{path}:{number}: expected {len(FIELDS)} fields")
        mutation: dict[str, str | int] = dict(zip(FIELDS, fields, strict=True))
        identity = str(mutation["id"])
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", identity) or identity in identities:
            raise SystemExit(f"{path}:{number}: invalid or duplicate id {identity!r}")
        identities.add(identity)
        if mutation["severity"] not in {"critical", "high"}:
            raise SystemExit(f"{identity}: mutation is not critical/high")
        if mutation["target"] not in {"all", "lib"}:
            raise SystemExit(f"{identity}: target must be all or lib")
        for field in ("occurrence", "max_ms"):
            try:
                mutation[field] = int(str(mutation[field]))
            except ValueError as error:
                raise SystemExit(f"{identity}: invalid {field}") from error
        if not 1 <= int(mutation["occurrence"]) <= 32:
            raise SystemExit(f"{identity}: occurrence is outside 1..=32")
        if not 1_000 <= int(mutation["max_ms"]) <= 300_000:
            raise SystemExit(f"{identity}: max_ms is outside 1000..=300000")
        source = root / str(mutation["source"])
        if not source.is_file():
            raise SystemExit(f"{identity}: missing source {mutation['source']}")
        source_text = source.read_text(encoding="utf-8")
        if source_text.count(str(mutation["find"])) < int(mutation["occurrence"]):
            raise SystemExit(f"{identity}: mutation anchor occurrence is missing")
        if not re.search(
            rf"\bfn\s+{re.escape(str(mutation['test']))}\s*\(", source_text
        ):
            raise SystemExit(f"{identity}: exact witness test is missing from source")
        mutations.append(mutation)
    if not mutations:
        raise SystemExit("implementation mutation registry is empty")
    if [mutation["id"] for mutation in mutations] != sorted(identities):
        raise SystemExit("implementation mutation ids must be sorted and unique")
    return mutations


def cargo_test_command(mutation: dict[str, str | int]) -> list[str]:
    command = ["cargo", "test", "-q", "-p", str(mutation["package"])]
    if mutation["features"] != "-":
        command.extend(["--features", str(mutation["features"])])
    if mutation["target"] == "lib":
        command.append("--lib")
    command.append(str(mutation["test"]))
    return command


def run_test(
    checkout: Path,
    target_dir: Path,
    mutation: dict[str, str | int],
    timeout_ms: int,
) -> tuple[subprocess.CompletedProcess[str], int]:
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["CARGO_INCREMENTAL"] = "1"
    started = time.monotonic()
    result = subprocess.run(
        cargo_test_command(mutation),
        cwd=checkout,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout_ms / 1000,
        env=env,
        check=False,
    )
    return result, round((time.monotonic() - started) * 1000)


def replace_occurrence(text: str, find: str, replace: str, occurrence: int) -> str:
    start = 0
    for _ in range(occurrence):
        index = text.find(find, start)
        if index < 0:
            raise ValueError("mutation anchor occurrence disappeared")
        start = index + len(find)
    index = start - len(find)
    return text[:index] + replace + text[index + len(find) :]


def prepare_checkout(root: Path, destination: Path) -> None:
    subprocess.run(
        ["git", "clone", "-q", "--shared", "--no-checkout", str(root), str(destination)],
        check=True,
    )
    subprocess.run(["git", "-C", str(destination), "checkout", "-q", "HEAD"], check=True)
    excludes = (
        ".git",
        "target",
        "build",
        "logs",
        "perf.data",
        "driver-domains/linux/out",
    )
    command = ["rsync", "-a", "--delete"]
    command.extend(f"--exclude={value}" for value in excludes)
    command.extend([f"{root}/", f"{destination}/"])
    subprocess.run(command, check=True)


def main() -> int:
    root = Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )
    mutations = read_registry(root)
    artifact_dir = root / "build/formal/implementation-mutations"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    target_dir = artifact_dir / "target"
    results: list[dict[str, str | int]] = []

    if shutil.which("rsync") is None:
        raise SystemExit("implementation mutation runner requires rsync")
    with tempfile.TemporaryDirectory(prefix="rustos-implementation-mutations-") as temp:
        checkout = Path(temp) / "checkout"
        prepare_checkout(root, checkout)
        originals = {
            str(mutation["source"]): (
                checkout / str(mutation["source"])
            ).read_text(encoding="utf-8")
            for mutation in mutations
        }
        for mutation in mutations:
            identity = str(mutation["id"])
            source = checkout / str(mutation["source"])
            source.write_text(originals[str(mutation["source"])], encoding="utf-8")
            baseline, baseline_ms = run_test(
                checkout, target_dir, mutation, int(mutation["max_ms"])
            )
            (artifact_dir / f"{identity}-baseline.log").write_text(
                baseline.stdout, encoding="utf-8"
            )
            if baseline.returncode != 0 or not re.search(
                r"test result: ok\. [1-9][0-9]* passed;", baseline.stdout
            ):
                print("\n".join(baseline.stdout.splitlines()[-100:]), file=sys.stderr)
                raise SystemExit(f"{identity}: baseline witness did not pass")

            mutated = replace_occurrence(
                originals[str(mutation["source"])],
                str(mutation["find"]),
                str(mutation["replace"]),
                int(mutation["occurrence"]),
            )
            source.write_text(mutated, encoding="utf-8")
            mutant, mutant_ms = run_test(
                checkout, target_dir, mutation, int(mutation["max_ms"])
            )
            (artifact_dir / f"{identity}-mutant.log").write_text(
                mutant.stdout, encoding="utf-8"
            )
            killed = mutant.returncode != 0 and bool(
                re.search(r"test result: FAILED", mutant.stdout)
            )
            if not killed:
                print("\n".join(mutant.stdout.splitlines()[-100:]), file=sys.stderr)
                if mutant.returncode == 0:
                    raise SystemExit(f"{identity}: implementation mutant survived")
                raise SystemExit(
                    f"{identity}: mutant was invalid instead of killed by its witness"
                )
            results.append(
                {
                    **mutation,
                    "status": "killed",
                    "baseline_elapsed_ms": baseline_ms,
                    "mutant_elapsed_ms": mutant_ms,
                    "source_sha256": hashlib.sha256(
                        originals[str(mutation["source"])].encode()
                    ).hexdigest(),
                }
            )
            source.write_text(originals[str(mutation["source"])], encoding="utf-8")

    registry = root / "formal/implementation-mutations.tsv"
    summary = {
        "schema": "rustos-implementation-mutation-evidence-v1",
        "status": "passed",
        "registry_sha256": hashlib.sha256(registry.read_bytes()).hexdigest(),
        "mutation_count": len(results),
        "kill_count": len(results),
        "kill_ratio": 1.0,
        "mutations": results,
    }
    (artifact_dir / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(f"implementation mutations passed killed={len(results)}/{len(results)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
