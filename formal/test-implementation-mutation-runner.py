#!/usr/bin/env python3
"""Deterministic classification selftests for the implementation mutant gate."""

from __future__ import annotations

import importlib.util
from pathlib import Path


RUNNER = Path(__file__).with_name("run-implementation-mutations.py")
SPEC = importlib.util.spec_from_file_location("implementation_mutation_runner", RUNNER)
assert SPEC is not None and SPEC.loader is not None
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def sealed_output(witness: str, body: str) -> str:
    return (
        "rustos: exact-witness "
        f"registered={witness.rsplit('::', 1)[-1]} resolved={witness} "
        f"command=cargo test {witness} -- --exact\n"
        + body
    )


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

    print("implementation mutation runner selftest passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
