#!/usr/bin/env python3
"""Deterministic classification selftests for the implementation mutant gate."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import select
import signal
import subprocess
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch


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
    """Both mirror implementations retain dirty nested build source, not artifacts."""
    with tempfile.TemporaryDirectory(prefix="rustos-mutation-mirror-") as temporary:
        root = Path(temporary) / "source"
        root.mkdir()
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        subprocess.run(["git", "-C", str(root), "config", "user.email", "test@example.invalid"], check=True)
        subprocess.run(["git", "-C", str(root), "config", "user.name", "Mutation Test"], check=True)
        (root / "tracked.rs").write_text("initial\n", encoding="utf-8")
        (root / "removed.rs").write_text("remove me\n", encoding="utf-8")
        nested_build = root / "tools/xtask/src/build"
        nested_build.mkdir(parents=True)
        (nested_build / "mod.rs").write_text("committed builder\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(root), "add", "."], check=True)
        subprocess.run(["git", "-C", str(root), "commit", "-q", "-m", "initial"], check=True)

        (root / "tracked.rs").write_text("dirty source\n", encoding="utf-8")
        (nested_build / "mod.rs").write_text("dirty builder\n", encoding="utf-8")
        (nested_build / "untracked.rs").write_text("new builder\n", encoding="utf-8")
        (root / "build").mkdir()
        (root / "build/artifact").write_text("ignore\n", encoding="utf-8")
        (root / "removed.rs").unlink()
        (root / "untracked.rs").write_text("live source\n", encoding="utf-8")
        (root / "target").mkdir()
        (root / "target" / "artifact").write_text("ignore\n", encoding="utf-8")
        (root / "driver-domains/linux/out").mkdir(parents=True)
        (root / "driver-domains/linux/out/artifact").write_text("ignore\n", encoding="utf-8")

        original_which = runner.shutil.which
        modes = ["fallback"] + (["rsync"] if original_which("rsync") else [])
        for mode in modes:
            checkout = Path(temporary) / f"checkout-{mode}"
            which = (lambda name: None if name == "rsync" else original_which(name)) \
                if mode == "fallback" else original_which
            with patch.object(runner.shutil, "which", which):
                runner.prepare_checkout(root, checkout)

            assert (checkout / ".git").is_dir()
            assert (checkout / "tracked.rs").read_text(encoding="utf-8") == "dirty source\n"
            assert (checkout / "untracked.rs").read_text(encoding="utf-8") == "live source\n"
            assert (checkout / "tools/xtask/src/build/mod.rs").read_text() == "dirty builder\n"
            assert (checkout / "tools/xtask/src/build/untracked.rs").read_text() == "new builder\n"
            assert not (checkout / "removed.rs").exists()
            assert not (checkout / "target").exists()
            assert not (checkout / "build").exists()
            assert not (checkout / "driver-domains/linux/out").exists()


def assert_process_exited(pid: int) -> None:
    # pidfds signal actual exit, not merely a signal sent; no sleep/poll loop
    # or dependence on how quickly the host's init reaps an orphan zombie.
    try:
        fd = os.pidfd_open(pid)
    except ProcessLookupError:
        return
    try:
        event = select.poll()
        event.register(fd, select.POLLIN)
        assert event.poll(2000), f"witness descendant still running: {pid}"
    finally:
        os.close(fd)


def timeout_and_interruption_kill_real_cargo_descendants() -> None:
    if os.name != "posix" or not hasattr(os, "pidfd_open"):
        return
    for interrupt in [False, True]:
        with tempfile.TemporaryDirectory(prefix="rustos-mutation-process-group-") as temporary:
            root = Path(temporary)
            child_file = root / "child.pid"
            child = "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"
            script = (
                "import os,subprocess,sys,time; from pathlib import Path; "
                f"child=subprocess.Popen([sys.executable,'-c',{child!r}]); "
                f"Path({str(child_file)!r}).write_text(str(child.pid)); "
                "print('exact-witness-started',flush=True); time.sleep(60)"
            )
            original = runner.subprocess.Popen.communicate
            interrupted = False

            def communicate(process, *args, **kwargs):
                nonlocal interrupted
                if interrupt and not interrupted:
                    interrupted = True
                    assert process.stdout.readline().strip() == "exact-witness-started"
                    raise KeyboardInterrupt
                return original(process, *args, **kwargs)

            try:
                with patch.object(runner.subprocess.Popen, "communicate", communicate):
                    try:
                        result, _ = runner.run_cargo_command(root, root / "target",
                            [sys.executable, "-c", script], 1000)
                    except KeyboardInterrupt:
                        assert interrupt
                    else:
                        assert not interrupt
                        assert result.returncode == runner.TIMEOUT_RETURNCODE
                        assert "exact-witness-started" in result.stdout
                        assert "exceeded max_ms=1000" in result.stdout
                assert_process_exited(int(child_file.read_text()))
            finally:
                # A failed regression must not itself leak the exact defect.
                if child_file.exists():
                    try:
                        os.kill(int(child_file.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass


def completed_commands_preserve_status_environment_and_merged_output() -> None:
    with tempfile.TemporaryDirectory(prefix="rustos-mutation-command-") as temporary:
        root = Path(temporary)
        script = ("import os,sys; print(os.environ['CARGO_TARGET_DIR'],flush=True); "
                  "print('stderr-kept',file=sys.stderr,flush=True); sys.exit(7)")
        result, _ = runner.run_cargo_command(root, root / "target", [sys.executable, "-c", script], 5000)
        assert result.returncode == 7
        assert str(root / "target") in result.stdout
        assert "stderr-kept" in result.stdout


def cache_seals_literal_external_witness_inputs() -> None:
    with tempfile.TemporaryDirectory(prefix="rustos-mutation-inputs-") as temporary:
        root = Path(temporary)
        package = root / "sample"
        package.mkdir()
        (package / "Cargo.toml").write_text('[package]\nname="sample"\nversion="0.1.0"\n')
        source = ('#[test]\nfn witness() { let limit = 1; '
                  'let _ = include_str!("probe.S"); '
                  'let _ = include_bytes!("driver.c"); }\n')
        (package / "lib.rs").write_text(source)
        probe = package / "probe.S"
        driver = package / "driver.c"
        probe.write_text("original assembly")
        driver.write_text("original driver")
        policy = root / "runner.py"
        policy.write_text("test policy")
        row = mutation_row("include-input", "sample", "sample/lib.rs", "witness",
                           "limit = 1", "limit = 2", source)
        row.update(anchor_spec="1", anchor_context_sha256="anchor")
        with patch.object(runner, "__file__", str(policy)):
            first = runner.mutation_cache_key(root, row)
            probe.write_text("changed assembly")
            second = runner.mutation_cache_key(root, row)
            assert first != second, "assembly change reused stale witness evidence"
            driver.write_text("changed comparator")
            assert second != runner.mutation_cache_key(root, row)
            driver.unlink()
            try:
                runner.mutation_cache_key(root, row)
            except FileNotFoundError:
                pass
            else:
                raise AssertionError("missing included comparator admitted")
            # A symlink cannot smuggle a host-side witness into repo evidence.
            driver.symlink_to(root.parent / "outside-witness.c")
            try:
                runner.mutation_cache_key(root, row)
            except ValueError as error:
                assert "escapes repository" in str(error)
            else:
                raise AssertionError("external witness include admitted")
        runner.witness_context_index.cache_clear()


def mismatched_cache_key_never_falls_back_to_legacy_source_match() -> None:
    row = mutation_row("cached", "sample", "sample/lib.rs", "witness",
                       "limit = 1", "limit = 2", "let limit = 1;")
    row["anchor_context_sha256"] = "anchor"
    with tempfile.TemporaryDirectory(prefix="rustos-mutation-cache-") as temporary:
        root = Path(temporary)
        summary = root / "summary.json"
        for present, key, expected_reuse in (
            (True, "old-key", False), (True, None, False),
            (True, "new-key", True), (False, None, True),
        ):
            old = {**row, "status": "killed"}
            if present:
                old["cache_key"] = key
            summary.write_text(json.dumps({"registry_sha256": "registry", "mutations": [old]}))
            with patch.object(runner, "mutation_cache_key", return_value="new-key"), \
                    patch.dict(os.environ, {"RUSTOS_IMPLEMENTATION_MUTATION_CACHE": "on"}):
                reused, pending = runner.reusable_mutation_outcomes(
                    root, [dict(row)], summary, "registry")
            assert bool(reused) == expected_reuse, (present, key, reused)
            assert bool(pending) != expected_reuse, (present, key, pending)


def cache_key_ignores_runner_edits_outside_kill_semantics() -> None:
    with tempfile.TemporaryDirectory(prefix="rustos-mutation-semantics-") as temporary:
        root = Path(temporary)
        package = root / "sample"
        package.mkdir()
        (package / "Cargo.toml").write_text('[package]\nname="sample"\nversion="0.1.0"\n')
        source = "#[test]\nfn witness() { let limit = 1; }\n"
        (package / "lib.rs").write_text(source)
        row = mutation_row("semantics", "sample", "sample/lib.rs", "witness",
                           "limit = 1", "limit = 2", source)
        row.update(anchor_spec="1", anchor_context_sha256="anchor")
        policy = root / "runner.py"
        policy.write_text("runner bytes before a logging-only edit")
        with patch.object(runner, "__file__", str(policy)):
            first = runner.mutation_cache_key(root, row)
            policy.write_text("runner bytes after a logging-only edit")
            assert runner.mutation_cache_key(root, row) == first, \
                "a non-semantic runner edit discarded every cached kill"
            runner.kill_semantics_digest.cache_clear()
            with patch.object(runner, "KILL_SEMANTICS",
                              (*runner.KILL_SEMANTICS, "checkpoint_path")):
                assert runner.mutation_cache_key(root, row) != first, \
                    "a kill-semantics change reused stale evidence"
            runner.kill_semantics_digest.cache_clear()
        runner.witness_context_index.cache_clear()
    for name in runner.KILL_SEMANTICS:
        assert callable(getattr(runner, name)), name


def interrupted_lane_resumes_from_exact_key_checkpoint() -> None:
    row = mutation_row("checkpointed", "sample", "sample/lib.rs", "witness",
                       "limit = 1", "limit = 2", "let limit = 1;")
    row["anchor_context_sha256"] = "anchor"
    with tempfile.TemporaryDirectory(prefix="rustos-mutation-checkpoint-") as temporary:
        root = Path(temporary)
        summary = root / "summary.json"
        checkpoint = runner.Checkpoint(runner.checkpoint_path(summary))
        checkpoint.record({**row, "status": "survived", "cache_key": "new-key"})
        checkpoint.record({**row, "status": "killed", "cache_key": "old-key"})
        with patch.dict(os.environ, {"RUSTOS_IMPLEMENTATION_MUTATION_CACHE": "on"}), \
                patch.object(runner, "mutation_cache_key", return_value="new-key"):
            reused, pending = runner.reusable_mutation_outcomes(
                root, [dict(row)], summary, "registry")
            assert not reused and pending, "survivor or stale-key checkpoint reused"
            checkpoint.record({**row, "status": "killed", "cache_key": "new-key"})
            with runner.checkpoint_path(summary).open("a") as stream:
                stream.write('{"id": "torn')
            reused, pending = runner.reusable_mutation_outcomes(
                root, [dict(row)], summary, "registry")
            assert reused and not pending, "exact-key checkpointed kill was not resumed"


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
    timeout_and_interruption_kill_real_cargo_descendants()
    completed_commands_preserve_status_environment_and_merged_output()
    cache_seals_literal_external_witness_inputs()
    mismatched_cache_key_never_falls_back_to_legacy_source_match()
    cache_key_ignores_runner_edits_outside_kill_semantics()
    interrupted_lane_resumes_from_exact_key_checkpoint()

    print("implementation mutation runner selftest passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
