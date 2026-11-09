#!/usr/bin/env python3
"""Selftest for the severity proposal and its in-place --apply mode."""

from __future__ import annotations

import importlib.util
import os
import stat
import sys
import tempfile
from pathlib import Path

SCRIPT = Path(__file__).with_name("classify-mutation-severity.py")
SPEC = importlib.util.spec_from_file_location("classify_mutation_severity", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
classify = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = classify
SPEC.loader.exec_module(classify)


def row(ident: str, severity: str, source: str, tail: str = "x\ty\t1\tpkg\t-\tlib\tt\t1000") -> bytes:
    return f"{ident}\t{severity}\t{source}\t{tail}".encode()


def expect_exit(callable_, needle: str) -> None:
    try:
        callable_()
    except SystemExit as error:
        assert needle in str(error), (needle, str(error))
    else:
        raise AssertionError(f"expected SystemExit containing {needle!r}")


def main() -> int:
    # Rubric spot checks: tier comes from the surface and the id's safety token.
    assert classify.classify("image-wx-admitted", "libs/rustos-image-admission/src/lib.rs")[0] == "critical"
    assert classify.classify("plain-thing", "libs/rustos-image-admission/src/lib.rs")[0] == "high"
    assert classify.classify("sched-charge", "kernel/ps/src/a.rs")[0] == "medium"
    assert classify.classify("sched-wake-lost", "kernel/ps/src/a.rs")[0] == "high"
    assert classify.classify("kvm-readiness-label", "tools/xtask/src/kvm/a.rs")[0] == "low"

    with tempfile.TemporaryDirectory(prefix="severity-apply-") as raw:
        tsv = Path(raw) / "implementation-mutations.tsv"
        # Header comment, a CRLF row, a blank line, odd bytes in a later column,
        # and no trailing newline: all must survive byte for byte.
        original = b"\n".join(
            [
                b"# id\tseverity\tsource\t...",
                row("a-one", "critical", "kernel/ps/src/a.rs"),
                row("b-two", "critical", "libs/rustos-image-admission/src/lib.rs") + b"\r",
                b"",
                row("c-three", "high", "tools/xtask/src/kvm/a.rs", "f \u00e9 \\x\t\t1\tpkg\t-\tlib\tt\t1000"),
                row("d-four", "high", "kernel/ps/src/b.rs"),
            ]
        )
        tsv.write_bytes(original)
        os.chmod(tsv, 0o640)
        proposal = {
            "a-one": ("critical", "medium"),
            "b-two": ("critical", "critical"),
            "c-three": ("high", "low"),
        }
        changed, untouched = classify.apply_proposal(tsv, proposal)
        assert (changed, untouched) == (2, 1), (changed, untouched)
        rewritten = tsv.read_bytes()
        old_lines, new_lines = original.split(b"\n"), rewritten.split(b"\n")
        assert len(old_lines) == len(new_lines) and rewritten.endswith(old_lines[-1])
        diffs = [(o, n) for o, n in zip(old_lines, new_lines) if o != n]
        assert len(diffs) == 2
        for old, new in diffs:
            old_f, new_f = old.split(b"\t", 2), new.split(b"\t", 2)
            assert old_f[0] == new_f[0] and old_f[2] == new_f[2]
        assert new_lines[1].split(b"\t")[1] == b"medium"
        assert new_lines[2].endswith(b"\r"), "CRLF byte lost"
        assert new_lines[4].split(b"\t")[1] == b"low"
        assert stat.S_IMODE(tsv.stat().st_mode) == 0o640, "mode not preserved"
        assert not list(Path(raw).glob("*.tmp")), "temporary file left behind"

        # A stale proposal (file changed since) aborts and writes nothing.
        before = tsv.read_bytes()
        expect_exit(lambda: classify.apply_proposal(tsv, {"a-one": ("critical", "low")}), "regenerate")
        # An id that is not in the file aborts and writes nothing.
        expect_exit(lambda: classify.apply_proposal(tsv, {"zz-gone": ("high", "low")}), "absent")
        assert tsv.read_bytes() == before, "failed apply modified the registry"

        # Round trip through the emitted TSV: apply is idempotent for the same proposal.
        text = Path(raw) / "proposal.tsv"
        text.write_text("# h\na-one\tmedium\tlow\twhy\nd-four\thigh\thigh\tkeep\n", encoding="utf-8")
        parsed = classify.read_proposal(text)
        assert classify.apply_proposal(tsv, parsed) == (1, 2)
        assert classify.apply_proposal(tsv, {"a-one": ("low", "low")}) == (0, 3)
        text.write_text("a\tcritical\tbogus\twhy\n", encoding="utf-8")
        expect_exit(lambda: classify.read_proposal(text), "unknown severity")
    print("mutation severity classifier selftest passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
