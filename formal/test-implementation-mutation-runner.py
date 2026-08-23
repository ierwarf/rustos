#!/usr/bin/env python3
"""Deterministic classification selftests for the implementation mutant gate."""

from __future__ import annotations

import hashlib
import importlib.util
import subprocess
import sys
import tempfile
from pathlib import Path


RUNNER = Path(__file__).with_name("run-implementation-mutations.py")
SPEC = importlib.util.spec_from_file_location("implementation_mutation_runner", RUNNER)
assert SPEC is not None and SPEC.loader is not None
runner = importlib.util.module_from_spec(SPEC)
# Register before executing: the runner defines dataclasses, and
# `dataclasses` resolves a field's type through `sys.modules[cls.__module__]`.
sys.modules[SPEC.name] = runner
SPEC.loader.exec_module(runner)


def sealed_output(witness: str, body: str) -> str:
    return (
        "rustos: exact-witness "
        f"registered={witness.rsplit('::', 1)[-1]} resolved={witness} "
        f"command=cargo test {witness} -- --exact\n"
        + body
    )


def mutation_row(
    identity: str,
    package: str,
    source: str,
    test: str,
    find: str,
    replace: str,
    original: str,
    max_ms: int = 60_000,
) -> dict[str, str | int]:
    return {
        "id": identity,
        "severity": "critical",
        "source": source,
        "find": find,
        "replace": replace,
        "occurrence": 1,
        "package": package,
        "features": "-",
        "target": "lib",
        "test": test,
        "max_ms": max_ms,
        "anchor_offset": original.index(find),
        "source_sha256": hashlib.sha256(original.encode("utf-8")).hexdigest(),
    }


def pristine_work_is_established_once_per_witness() -> None:
    """One listing per Cargo selection and one baseline per registered test."""
    original = "fn a() { let limit = 1; }\nfn b() { let other = 2; }\n"
    rows = [
        mutation_row("m1", "alpha", "alpha/lib.rs", "t_one", "limit = 1", "limit = 9", original),
        mutation_row("m2", "alpha", "alpha/lib.rs", "t_one", "other = 2", "other = 8", original),
        mutation_row("m3", "alpha", "alpha/other.rs", "t_two", "limit = 1", "limit = 7", original),
        # Rows may register different budgets for one witness; the shared
        # pristine work must take the most generous one.
        mutation_row(
            "m4", "beta", "beta/lib.rs", "t_one", "limit = 1", "limit = 5", original, 20_000
        ),
        mutation_row(
            "m5", "beta", "beta/other.rs", "t_one", "other = 2", "other = 4", original, 90_000
        ),
    ]
    expected_listings = len({runner.build_key(row) for row in rows})
    expected_baselines = len({(*runner.build_key(row), row["test"]) for row in rows})

    with tempfile.TemporaryDirectory(prefix="rustos-mutation-selftest-") as temporary:
        checkout = Path(temporary) / "checkout"
        artifact_dir = Path(temporary) / "artifacts"
        artifact_dir.mkdir()
        originals: dict[str, str] = {}
        for row in rows:
            relative = str(row["source"])
            path = checkout / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(original, encoding="utf-8")
            originals[relative] = original

        listings = 0
        witness_runs: list[str] = []
        pristine_budgets: list[tuple[str, int]] = []

        def fake_listing(_checkout, _target, mutation):
            # Cargo lists every test the selected target owns, not just the one
            # the caller happens to be registering.
            nonlocal listings
            listings += 1
            pristine_budgets.append((str(mutation["package"]), int(mutation["max_ms"])))
            listed = "".join(f"tests::{name}: test\n" for name in ("t_one", "t_two"))
            return subprocess.CompletedProcess([], 0, stdout=listed), 1

        def fake_witness(checkout_dir, _target, mutation, witness):
            witness_runs.append(str(mutation["id"]))
            body = (checkout_dir / str(mutation["source"])).read_text(encoding="utf-8")
            mutated = str(mutation["replace"]) in body
            status = "FAILED" if mutated else "ok"
            footer = (
                "test result: FAILED. 0 passed; 1 failed;"
                if mutated
                else "test result: ok. 1 passed;"
            )
            output = sealed_output(
                witness, f"running 1 test\ntest {witness} ... {status}\n{footer}\n"
            )
            return subprocess.CompletedProcess([], 1 if mutated else 0, stdout=output), 1

        original_listing = runner.list_cargo_tests
        original_witness = runner.run_exact_witness
        runner.list_cargo_tests = fake_listing
        runner.run_exact_witness = fake_witness
        try:
            outcomes = runner.run_bucket_mutations(
                checkout, Path(temporary) / "target", artifact_dir, rows, originals
            )
        finally:
            runner.list_cargo_tests = original_listing
            runner.run_exact_witness = original_witness

    assert [outcome["status"] for outcome in outcomes] == ["killed"] * len(rows), outcomes
    assert listings == expected_listings, listings
    # Shared pristine work takes the most generous budget in its group, never
    # the shortest row's.
    assert dict(pristine_budgets)["beta"] == 90_000, pristine_budgets
    assert len(witness_runs) == expected_baselines + len(rows), witness_runs
    # Every pristine baseline must precede the first mutant of its shard.
    assert witness_runs[:expected_baselines] == sorted(witness_runs[:expected_baselines])


def restore_touches_every_registered_source() -> None:
    """An unchanged source is still rewritten, so Cargo cannot answer stale.

    `CARGO_TARGET_DIR` outlives the lane and `rsync -a` preserves the live
    tree's mtimes, so a source whose bytes already match can still be older
    than the mutant binary a previous run cached for it.
    """
    original = "fn a() { let limit = 1; }\n"
    with tempfile.TemporaryDirectory(prefix="rustos-mutation-restore-") as temporary:
        checkout = Path(temporary) / "checkout"
        checkout.mkdir()
        source = checkout / "lib.rs"
        source.write_text(original, encoding="utf-8")
        stale = source.stat().st_mtime_ns
        baselines = runner.PristineBaselines(
            checkout, Path(temporary) / "target", Path(temporary), {"lib.rs": original}
        )
        baselines.restore()
        assert source.read_text(encoding="utf-8") == original
        assert source.stat().st_mtime_ns != stale


def checkout_mirror_has_a_no_rsync_path() -> None:
    """The source seal must not depend on an optional host binary."""
    with tempfile.TemporaryDirectory(prefix="rustos-mutation-mirror-") as temporary:
        root = Path(temporary) / "source"
        checkout = Path(temporary) / "checkout"
        root.mkdir()
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        subprocess.run(["git", "-C", str(root), "config", "user.email", "test@example.invalid"], check=True)
        subprocess.run(["git", "-C", str(root), "config", "user.name", "Mutation Test"], check=True)
        (root / "tracked.rs").write_text("initial\n", encoding="utf-8")
        (root / "removed.rs").write_text("remove me\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(root), "add", "."], check=True)
        subprocess.run(["git", "-C", str(root), "commit", "-q", "-m", "initial"], check=True)

        (root / "tracked.rs").write_text("dirty source\n", encoding="utf-8")
        (root / "removed.rs").unlink()
        (root / "untracked.rs").write_text("live source\n", encoding="utf-8")
        (root / "target").mkdir()
        (root / "target" / "artifact").write_text("ignore\n", encoding="utf-8")
        (root / "driver-domains/linux/out").mkdir(parents=True)
        (root / "driver-domains/linux/out/artifact").write_text("ignore\n", encoding="utf-8")

        original_which = runner.shutil.which
        runner.shutil.which = lambda name: None if name == "rsync" else original_which(name)
        try:
            runner.prepare_checkout(root, checkout)
        finally:
            runner.shutil.which = original_which

        assert (checkout / ".git").is_dir()
        assert (checkout / "tracked.rs").read_text(encoding="utf-8") == "dirty source\n"
        assert (checkout / "untracked.rs").read_text(encoding="utf-8") == "live source\n"
        assert not (checkout / "removed.rs").exists()
        assert not (checkout / "target").exists()
        assert not (checkout / "driver-domains/linux/out").exists()


def main() -> int:
    witness = "ipc::tests::exact_contract"
    started = sealed_output(witness, "running 1 test\n")
    passed = sealed_output(
        witness,
        f"running 1 test\ntest {witness} ... ok\ntest result: ok. 1 passed;\n",
    )
    failed = sealed_output(
        witness,
        f"running 1 test\ntest {witness} ... FAILED\ntest result: FAILED. 0 passed; 1 failed;\n",
    )
    compile_only = sealed_output(witness, "error: could not compile mutant\n")

    assert runner.mutation_was_killed(101, failed, witness)
    assert runner.mutation_was_killed(runner.TIMEOUT_RETURNCODE, started, witness)
    assert not runner.mutation_was_killed(runner.TIMEOUT_RETURNCODE, compile_only, witness)
    assert not runner.mutation_was_killed(101, compile_only, witness)
    assert not runner.mutation_was_killed(0, passed, witness)

    assert runner.parse_occurrence("unique", "1") == (1, None)
    assert runner.parse_occurrence("repeated", "2/3") == (2, 3)
    for invalid in ("0", "2/1", "1/1", "x", "1/0"):
        try:
            runner.parse_occurrence("invalid", invalid)
        except SystemExit:
            pass
        else:
            raise AssertionError(f"invalid occurrence admitted: {invalid}")

    pristine_work_is_established_once_per_witness()
    restore_touches_every_registered_source()
    checkout_mirror_has_a_no_rsync_path()

    print("implementation mutation runner selftest passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
