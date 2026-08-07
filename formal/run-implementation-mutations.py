#!/usr/bin/env python3
"""Prove that critical implementation-contract regressions are detected."""

from __future__ import annotations

import argparse
import concurrent.futures
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
        witness_pattern = rf"\bfn\s+{re.escape(str(mutation['test']))}\s*\("
        witness_sources = [source_text]
        # Large kernel owners keep their exact unit witnesses in an explicit
        # path module. Resolve only modules that the mutated source itself
        # declares, so an unrelated same-named test cannot satisfy this gate.
        for relative in re.findall(
            r'#\[path\s*=\s*"([^"]+)"\]\s*\n\s*mod\s+[A-Za-z_][A-Za-z0-9_]*\s*;',
            source_text,
        ):
            witness_path = source.parent / relative
            if not witness_path.is_file():
                raise SystemExit(
                    f"{identity}: declared witness module is missing: {witness_path}"
                )
            witness_sources.append(witness_path.read_text(encoding="utf-8"))
        if not any(re.search(witness_pattern, text) for text in witness_sources):
            # The production owner may be a child module while its white-box
            # witness remains in the parent test module. Accept that layout
            # only when the exact function name is globally unique. The cargo
            # baseline below still requires this exact filter to execute one
            # or more tests in the declared package, preventing an unrelated
            # same-named function from satisfying mutation evidence.
            matches = [
                candidate
                for candidate in root.rglob("*.rs")
                if not any(part in {"target", "build", "vendor"} for part in candidate.parts)
                and re.search(
                    witness_pattern, candidate.read_text(encoding="utf-8", errors="strict")
                )
            ]
            if len(matches) != 1:
                raise SystemExit(
                    f"{identity}: exact witness test must resolve uniquely; matches={len(matches)}"
                )
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


def evaluate_mutation(
    checkout: Path,
    target_dir: Path,
    artifact_dir: Path,
    mutation: dict[str, str | int],
    originals: dict[str, str],
) -> dict[str, str | int]:
    """Run one mutation's baseline and mutant in an already-prepared checkout."""
    identity = str(mutation["id"])
    source = checkout / str(mutation["source"])
    original = originals[str(mutation["source"])]
    source.write_text(original, encoding="utf-8")
    baseline, baseline_ms = run_test(
        checkout, target_dir, mutation, int(mutation["max_ms"])
    )
    (artifact_dir / f"{identity}-baseline.log").write_text(
        baseline.stdout, encoding="utf-8"
    )
    if baseline.returncode != 0 or not re.search(
        r"test result: ok\. [1-9][0-9]* passed;", baseline.stdout
    ):
        return {**mutation, "status": "baseline-failed", "detail": "baseline witness did not pass"}

    mutated = replace_occurrence(
        original,
        str(mutation["find"]),
        str(mutation["replace"]),
        int(mutation["occurrence"]),
    )
    source.write_text(mutated, encoding="utf-8")
    mutant, mutant_ms = run_test(
        checkout, target_dir, mutation, int(mutation["max_ms"])
    )
    (artifact_dir / f"{identity}-mutant.log").write_text(mutant.stdout, encoding="utf-8")
    source.write_text(original, encoding="utf-8")
    # Some freestanding service profiles use panic=abort even in host tests, so
    # Cargo can terminate after `running N tests` without a libtest `FAILED`
    # footer. The exact filter already passed in the baseline; observing it
    # start here proves compilation succeeded and makes a nonzero exit an
    # execution-time mutant kill rather than an invalid mutant.
    exact_test_started = bool(re.search(r"running [1-9][0-9]* tests?", mutant.stdout))
    killed = mutant.returncode != 0 and (
        bool(re.search(r"test result: FAILED", mutant.stdout)) or exact_test_started
    )
    if not killed:
        detail = (
            "implementation mutant survived"
            if mutant.returncode == 0
            else "mutant was invalid instead of killed by its witness"
        )
        return {**mutation, "status": "survived", "detail": detail}
    return {
        **mutation,
        "status": "killed",
        "baseline_elapsed_ms": baseline_ms,
        "mutant_elapsed_ms": mutant_ms,
        "source_sha256": hashlib.sha256(original.encode()).hexdigest(),
    }


# A shard's own `CARGO_TARGET_DIR` measured ~1.4 GiB warm, plus its checkout.
SHARD_DISK_BUDGET_BYTES = 3 * 1024 * 1024 * 1024


def shard_count(mutation_count: int, artifact_dir: Path) -> int:
    """How many checkouts to run concurrently.

    Each shard needs its own checkout and its own `CARGO_TARGET_DIR`, because a
    mutation rewrites source in place and cargo serializes on a target lock.
    That makes a shard expensive in disk, and the first attempt at four shards
    filled the device mid-run - which is strictly worse than running
    sequentially, because it fails the lane and leaves the target trees behind.
    So free space decides, and the default is one shard on a full disk.
    """
    override = os.environ.get("RUSTOS_MUTATION_SHARDS")
    if override:
        return max(1, min(int(override), mutation_count))
    try:
        free = shutil.disk_usage(artifact_dir).free
    except OSError:
        return 1
    affordable = 1 + int(free // SHARD_DISK_BUDGET_BYTES)
    return max(1, min(4, (os.cpu_count() or 1) // 4, mutation_count, affordable))


def run_shards(
    checkout: Path,
    target_dir: Path,
    artifact_dir: Path,
    mutations: list[dict[str, str | int]],
    originals: dict[str, str],
    temp: str,
) -> list[dict[str, str | int]]:
    # Keep every mutation of one source file in the same shard and adjacent to
    # its neighbours: cargo then rebuilds one crate incrementally instead of
    # alternating between crates on every run.
    ordered = sorted(
        mutations, key=lambda entry: (str(entry["package"]), str(entry["source"]))
    )
    shards = shard_count(len(ordered), artifact_dir)
    if shards == 1:
        return [
            evaluate_mutation(checkout, target_dir, artifact_dir, mutation, originals)
            for mutation in ordered
        ]

    buckets: list[list[dict[str, str | int]]] = [[] for _ in range(shards)]
    by_source: dict[str, list[dict[str, str | int]]] = {}
    for mutation in ordered:
        by_source.setdefault(str(mutation["source"]), []).append(mutation)
    for index, group in enumerate(by_source.values()):
        buckets[index % shards].extend(group)

    def run_bucket(index: int) -> list[dict[str, str | int]]:
        bucket = buckets[index]
        if not bucket:
            return []
        if index == 0:
            shard_checkout, shard_target = checkout, target_dir
        else:
            shard_checkout = Path(temp) / f"checkout-{index}"
            prepare_checkout(checkout, shard_checkout)
            shard_target = artifact_dir / f"target-{index}"
        return [
            evaluate_mutation(
                shard_checkout, shard_target, artifact_dir, mutation, originals
            )
            for mutation in bucket
        ]

    outcomes: list[dict[str, str | int]] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=shards) as pool:
        for bucket_outcomes in pool.map(run_bucket, range(shards)):
            outcomes.extend(bucket_outcomes)
    return outcomes


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Kill registered implementation mutants with their exact witnesses."
    )
    parser.add_argument(
        "--only",
        action="append",
        default=[],
        metavar="ID",
        help="run one registered mutation; repeat for a focused change set",
    )
    args = parser.parse_args()
    root = Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )
    mutations = read_registry(root)
    focused_ids = set(args.only)
    if focused_ids:
        known_ids = {str(mutation["id"]) for mutation in mutations}
        unknown_ids = focused_ids - known_ids
        if unknown_ids:
            raise SystemExit(
                "unknown implementation mutation ids: " + ", ".join(sorted(unknown_ids))
            )
        mutations = [
            mutation
            for mutation in mutations
            if str(mutation["id"]) in focused_ids
        ]
    artifact_dir = root / "build/formal/implementation-mutations"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    target_dir = artifact_dir / "target"

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
        outcomes = run_shards(checkout, target_dir, artifact_dir, mutations, originals, temp)

    failures = [outcome for outcome in outcomes if outcome.get("status") != "killed"]
    if failures:
        for outcome in failures:
            print(
                f"{outcome['id']}: {outcome['detail']}",
                file=sys.stderr,
            )
        # Report every survivor in one run. A registry this size is a survey of
        # what the witnesses actually cover, and stopping at the first gap
        # turns one pass into one finding.
        raise SystemExit(
            f"implementation mutations failed: {len(failures)} of {len(outcomes)}"
        )
    results = [
        {key: value for key, value in outcome.items() if key != "detail"}
        for outcome in outcomes
    ]

    registry = root / "formal/implementation-mutations.tsv"
    summary = {
        "schema": "rustos-implementation-mutation-evidence-v1",
        "status": "passed",
        "registry_sha256": hashlib.sha256(registry.read_bytes()).hexdigest(),
        "mutation_count": len(results),
        "kill_count": len(results),
        "kill_ratio": 1.0,
        "mutations": results,
        "scope": "focused" if focused_ids else "complete",
    }
    summary_name = "focused-summary.json" if focused_ids else "summary.json"
    (artifact_dir / summary_name).write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    scope = "focused" if focused_ids else "complete"
    print(
        f"implementation mutations passed scope={scope} "
        f"killed={len(results)}/{len(results)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
