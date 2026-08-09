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
import shlex
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path


TIMEOUT_RETURNCODE = -1024
OCCURRENCE_SPEC = re.compile(r"(?:(?P<selected>[1-9][0-9]*)/)?(?P<total>[1-9][0-9]*)$")
LISTED_TEST = re.compile(r"(?m)^(?P<name>[^\s].*): test$")


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


def source_path(root: Path, identity: str, raw_source: str) -> Path:
    """Resolve one registry source without permitting checkout escape."""
    if Path(raw_source).is_absolute() or ".." in Path(raw_source).parts:
        raise SystemExit(f"{identity}: source must be a repository-relative path")
    candidate = root / raw_source
    try:
        candidate.resolve().relative_to(root.resolve())
    except ValueError as error:
        raise SystemExit(f"{identity}: source escapes the repository: {raw_source!r}") from error
    if not candidate.is_file():
        raise SystemExit(f"{identity}: missing source {raw_source}")
    return candidate


def package_name_for_source(root: Path, identity: str, source: Path) -> str:
    """Read only ancestor manifests; mutation preflight must not walk the tree."""
    current = source.parent
    while current != root:
        manifest = current / "Cargo.toml"
        if manifest.is_file():
            try:
                package = tomllib.loads(manifest.read_text(encoding="utf-8")).get(
                    "package", {}
                )
            except tomllib.TOMLDecodeError as error:
                raise SystemExit(f"{identity}: invalid package manifest {manifest}") from error
            name = package.get("name")
            if isinstance(name, str):
                return name
            raise SystemExit(f"{identity}: source manifest has no package name: {manifest}")
        current = current.parent
    raise SystemExit(f"{identity}: no package manifest owns {source.relative_to(root)}")


class AnchorResolutionError(ValueError):
    """One registry row cannot identify a stable source mutation location."""


def parse_occurrence(identity: str, raw_value: str) -> tuple[int, int | None]:
    """Return the selected occurrence and exact expected anchor total.

    ``N`` is permitted only for a unique source anchor. ``N/M`` is required
    for a genuinely ambiguous anchor and selects N of exactly M occurrences.
    The total is part of the registry contract, so inserting an earlier
    identical string fails the preflight instead of silently retargeting a
    later mutation.
    """
    match = OCCURRENCE_SPEC.fullmatch(raw_value)
    if match is None:
        raise SystemExit(
            f"{identity}: invalid occurrence {raw_value!r}; expected N or N/M"
        )
    selected = int(match.group("selected") or match.group("total"))
    expected_total = (
        int(match.group("total")) if match.group("selected") is not None else None
    )
    if expected_total is not None and selected > expected_total:
        raise SystemExit(
            f"{identity}: occurrence selects {selected} of only {expected_total}"
        )
    if expected_total == 1:
        raise SystemExit(
            f"{identity}: explicit occurrence is forbidden for a unique anchor"
        )
    return selected, expected_total


def anchor_offsets(text: str, find: str) -> list[int]:
    offsets: list[int] = []
    start = 0
    while True:
        offset = text.find(find, start)
        if offset < 0:
            return offsets
        offsets.append(offset)
        start = offset + len(find)


def resolve_anchor(
    identity: str,
    source: Path,
    source_text: str,
    find: str,
    selected: int,
    expected_total: int | None,
) -> dict[str, int | str]:
    """Resolve one immutable source location and reject anchor drift."""
    offsets = anchor_offsets(source_text, find)
    actual_total = len(offsets)
    if expected_total is None:
        if actual_total != 1:
            if selected <= actual_total:
                required = f"{selected}/{actual_total}"
                raise AnchorResolutionError(
                    f"{identity}: ambiguous mutation anchor in {source}: "
                    f"legacy occurrence={selected} found total={actual_total}; "
                    f"use occurrence={required}"
                )
            raise AnchorResolutionError(
                f"{identity}: stale mutation anchor in {source}: "
                f"legacy occurrence={selected} exceeds found total={actual_total}"
            )
        if selected != 1:
            raise AnchorResolutionError(
                f"{identity}: stale mutation anchor in {source}: "
                f"legacy occurrence={selected} exceeds found total=1"
            )
    elif actual_total != expected_total:
        raise AnchorResolutionError(
            f"{identity}: stale or ambiguous mutation anchor in {source}: "
            f"expected total={expected_total}, found={actual_total}, "
            f"selected={selected}"
        )
    offset = offsets[selected - 1]
    line = source_text.count("\n", 0, offset) + 1
    # Record a small selected-context seal as evidence and recheck the full
    # source snapshot before rewriting the isolated checkout.
    context_start = max(0, offset - 96)
    context_end = min(len(source_text), offset + len(find) + 96)
    return {
        "anchor_offset": offset,
        "anchor_line": line,
        "anchor_total": actual_total,
        "anchor_context_sha256": hashlib.sha256(
            source_text[context_start:context_end].encode("utf-8")
        ).hexdigest(),
        "source_sha256": hashlib.sha256(source_text.encode("utf-8")).hexdigest(),
    }


def read_registry(
    root: Path, anchor_issues: list[str] | None = None
) -> list[dict[str, str | int]]:
    path = root / "formal/implementation-mutations.tsv"
    mutations: list[dict[str, str | int]] = []
    identities: set[str] = set()
    semantic_rows: dict[tuple[str, ...], tuple[str, int]] = {}
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
        # IDs, severity labels, and time budgets must not let one source
        # mutation/witness pair inflate the adequacy count more than once.
        semantic = tuple(
            str(mutation[field])
            for field in (
                "source",
                "find",
                "replace",
                "occurrence",
                "package",
                "features",
                "target",
                "test",
            )
        )
        if previous := semantic_rows.get(semantic):
            previous_id, previous_line = previous
            message = (
                f"{path}:{number}: duplicate semantic mutation row with "
                f"{previous_id} at line {previous_line}"
            )
            if anchor_issues is None:
                raise SystemExit(message)
            anchor_issues.append(message)
        else:
            semantic_rows[semantic] = (identity, number)
        if mutation["severity"] not in {"critical", "high"}:
            raise SystemExit(f"{identity}: mutation is not critical/high")
        if mutation["target"] not in {"all", "lib"}:
            raise SystemExit(f"{identity}: target must be all or lib")
        raw_occurrence = str(mutation["occurrence"])
        selected, expected_total = parse_occurrence(identity, raw_occurrence)
        mutation["occurrence"] = selected
        mutation["anchor_spec"] = raw_occurrence
        try:
            mutation["max_ms"] = int(str(mutation["max_ms"]))
        except ValueError as error:
            raise SystemExit(f"{identity}: invalid max_ms") from error
        if not 1_000 <= int(mutation["max_ms"]) <= 300_000:
            raise SystemExit(f"{identity}: max_ms is outside 1000..=300000")
        if not str(mutation["find"]):
            raise SystemExit(f"{identity}: mutation anchor must not be empty")
        if mutation["find"] == mutation["replace"]:
            raise SystemExit(f"{identity}: mutation replacement must change the anchor")
        source = source_path(root, identity, str(mutation["source"]))
        package = package_name_for_source(root, identity, source)
        if package != mutation["package"]:
            raise SystemExit(
                f"{identity}: source package is {package!r}, not registered package "
                f"{mutation['package']!r}"
            )
        source_text = source.read_text(encoding="utf-8")
        try:
            mutation.update(
                resolve_anchor(
                    identity,
                    source,
                    source_text,
                    str(mutation["find"]),
                    selected,
                    expected_total,
                )
            )
        except AnchorResolutionError as error:
            if anchor_issues is None:
                raise SystemExit(str(error)) from error
            anchor_issues.append(f"{path}:{number}: {error}")
            continue
        mutations.append(mutation)
    if anchor_issues:
        raise SystemExit(
            "implementation mutation registry preflight failed:\n"
            + "\n".join(anchor_issues)
        )
    if not mutations:
        raise SystemExit("implementation mutation registry is empty")
    if [mutation["id"] for mutation in mutations] != sorted(identities):
        raise SystemExit("implementation mutation ids must be sorted and unique")
    return mutations


def cargo_test_base_command(mutation: dict[str, str | int]) -> list[str]:
    command = ["cargo", "test", "-q", "-p", str(mutation["package"])]
    if mutation["features"] != "-":
        command.extend(["--features", str(mutation["features"])])
    if mutation["target"] == "lib":
        command.append("--lib")
    return command


def run_cargo_command(
    checkout: Path,
    target_dir: Path,
    command: list[str],
    timeout_ms: int,
) -> tuple[subprocess.CompletedProcess[str], int]:
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["CARGO_INCREMENTAL"] = "1"
    started = time.monotonic()
    try:
        result = subprocess.run(
            command,
            cwd=checkout,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=timeout_ms / 1000,
            env=env,
            check=False,
        )
    except subprocess.TimeoutExpired as expired:
        # Preserve timeouts as an explicit result. Evaluation treats a timeout
        # from the mutant's exact witness as a kill, while a baseline/listing
        # timeout remains a failed precondition.
        captured = expired.output or b""
        if isinstance(captured, bytes):
            captured = captured.decode("utf-8", "replace")
        result = subprocess.CompletedProcess(
            expired.cmd,
            returncode=TIMEOUT_RETURNCODE,
            stdout=captured + f"\nrustos: witness exceeded max_ms={timeout_ms}\n",
        )
    return result, round((time.monotonic() - started) * 1000)


def resolve_cargo_witness(
    checkout: Path,
    target_dir: Path,
    mutation: dict[str, str | int],
) -> tuple[str | None, subprocess.CompletedProcess[str], int, str | None]:
    """Resolve the test binary's full libtest name without executing tests."""
    listing, elapsed_ms = run_cargo_command(
        checkout,
        target_dir,
        [*cargo_test_base_command(mutation), "--", "--list"],
        int(mutation["max_ms"]),
    )
    registered = str(mutation["test"])
    if listing.returncode != 0:
        return None, listing, elapsed_ms, "could not list the registered witness"
    listed = [match.group("name") for match in LISTED_TEST.finditer(listing.stdout)]
    if "::" in registered:
        matches = [name for name in listed if name == registered]
    else:
        matches = [name for name in listed if name.rsplit("::", 1)[-1] == registered]
    if len(matches) != 1:
        return (
            None,
            listing,
            elapsed_ms,
            f"registered witness must resolve to exactly one libtest name; matches={len(matches)}",
        )
    return matches[0], listing, elapsed_ms, None


def run_exact_witness(
    checkout: Path,
    target_dir: Path,
    mutation: dict[str, str | int],
    witness: str,
) -> tuple[subprocess.CompletedProcess[str], int]:
    command = [*cargo_test_base_command(mutation), witness, "--", "--exact"]
    result, elapsed_ms = run_cargo_command(
        checkout, target_dir, command, int(mutation["max_ms"])
    )
    proof = (
        "rustos: exact-witness "
        f"registered={mutation['test']} resolved={witness} "
        f"command={shlex.join(command)}\n"
    )
    return (
        subprocess.CompletedProcess(
            result.args,
            result.returncode,
            stdout=proof + result.stdout,
        ),
        elapsed_ms,
    )


def exact_witness_executed(output: str, witness: str) -> bool:
    """Require an exact command seal and one libtest execution, not a filter hit."""
    proof = f"resolved={witness} " in output and " --exact" in output
    completed = bool(
        re.search(rf"(?m)^test {re.escape(witness)} \.\.\. (?:ok|FAILED|ignored)$", output)
    )
    # panic=abort profiles can terminate before libtest prints the completed
    # test line.  The exact command plus one selected test still proves the
    # named witness began, whereas a compile-only rejection prints neither.
    started = bool(re.search(r"(?m)^running 1 test$", output))
    return proof and (completed or started)


def exact_witness_failed(output: str, witness: str) -> bool:
    """Distinguish a failing selected witness from another Cargo target error."""
    proof = f"resolved={witness} " in output and " --exact" in output
    completed = re.search(
        rf"(?m)^test {re.escape(witness)} \.\.\. (?P<status>ok|FAILED|ignored)$",
        output,
    )
    if completed is not None:
        return proof and completed.group("status") == "FAILED"
    # In panic=abort profiles libtest may never print the result line. The
    # resolved --exact command can execute only this test, so its one-test
    # start is sufficient evidence of a witness-triggered abort.
    return proof and bool(re.search(r"(?m)^running 1 test$", output))


def mutation_was_killed(returncode: int, output: str, witness: str) -> bool:
    """Credit only a failure or timeout reached by the selected witness.

    Cargo may spend most of a mutation budget compiling or linking. A timeout
    before libtest announces the one resolved test is therefore invalid
    evidence, just like any other compile-only rejection.
    """
    if returncode == TIMEOUT_RETURNCODE:
        return exact_witness_executed(output, witness)
    return returncode != 0 and exact_witness_failed(output, witness)


def replace_resolved_anchor(text: str, mutation: dict[str, str | int]) -> str:
    expected_sha256 = str(mutation["source_sha256"])
    actual_sha256 = hashlib.sha256(text.encode("utf-8")).hexdigest()
    if actual_sha256 != expected_sha256:
        raise ValueError("source changed after mutation preflight; refusing to retarget")
    offset = int(mutation["anchor_offset"])
    find = str(mutation["find"])
    if text[offset : offset + len(find)] != find:
        raise ValueError("resolved mutation anchor disappeared")
    return text[:offset] + str(mutation["replace"]) + text[offset + len(find) :]


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
    witness, witness_listing, witness_listing_ms, resolution_error = resolve_cargo_witness(
        checkout, target_dir, mutation
    )
    (artifact_dir / f"{identity}-witness-list.log").write_text(
        f"rustos: registered-witness={mutation['test']}\n" + witness_listing.stdout,
        encoding="utf-8",
    )
    if resolution_error is not None:
        return {
            **mutation,
            "status": "baseline-failed",
            "detail": resolution_error,
        }
    assert witness is not None
    baseline, baseline_ms = run_exact_witness(checkout, target_dir, mutation, witness)
    (artifact_dir / f"{identity}-baseline.log").write_text(
        baseline.stdout, encoding="utf-8"
    )
    if (
        baseline.returncode != 0
        or not exact_witness_executed(baseline.stdout, witness)
        or not re.search(r"test result: ok\. 1 passed;", baseline.stdout)
    ):
        return {
            **mutation,
            "status": "baseline-failed",
            "detail": "baseline did not execute exactly the registered witness",
        }

    mutated = replace_resolved_anchor(original, mutation)
    source.write_text(mutated, encoding="utf-8")
    mutant, mutant_ms = run_exact_witness(checkout, target_dir, mutation, witness)
    (artifact_dir / f"{identity}-mutant.log").write_text(mutant.stdout, encoding="utf-8")
    source.write_text(original, encoding="utf-8")
    # Some freestanding service profiles use panic=abort even in host tests, so
    # Cargo can terminate after `running N tests` without a libtest `FAILED`
    # footer. The resolved --exact filter and its execution seal distinguish a
    # witness kill from a compile-only failure or an unrelated filtered test.
    killed = mutation_was_killed(mutant.returncode, mutant.stdout, witness)
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
        "resolved_witness": witness,
        "witness_listing_elapsed_ms": witness_listing_ms,
        "baseline_elapsed_ms": baseline_ms,
        "mutant_elapsed_ms": mutant_ms,
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
    parser.add_argument(
        "--check",
        action="store_true",
        help="validate registry anchors and duplicate semantics without running Cargo",
    )
    args = parser.parse_args()
    if args.check and args.only:
        parser.error("--check validates the complete registry and cannot be combined with --only")
    root = Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )
    mutations = read_registry(root, [] if args.check else None)
    if args.check:
        print(f"implementation mutation preflight passed count={len(mutations)}")
        return 0
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
        for mutation in mutations:
            source_name = str(mutation["source"])
            original = originals[source_name]
            if hashlib.sha256(original.encode("utf-8")).hexdigest() != mutation[
                "source_sha256"
            ]:
                raise SystemExit(
                    f"{mutation['id']}: source changed after mutation preflight; rerun the lane"
                )
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
