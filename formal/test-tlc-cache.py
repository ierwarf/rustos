#!/usr/bin/env python3
"""Negative tests for exact-input TLC evidence reuse."""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
import time
from pathlib import Path

from tlc_cache import validate_cached_summary


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def expect_rejected(root: Path, reason: str) -> None:
    try:
        validate_cached_summary(root, "pr", "sample/Sample")
    except ValueError:
        return
    raise AssertionError(f"TLC cache accepted {reason}")


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="rustos-tlc-cache-") as temporary:
        root = Path(temporary)
        formal = root / "formal"
        formal.mkdir()
        (formal / "sample").mkdir()
        spec = formal / "sample/Sample.tla"
        config = formal / "sample/Sample.cfg"
        spec.write_text("---- MODULE Sample ----\n====\n", encoding="utf-8")
        config.write_text("INVARIANT Safety\n", encoding="utf-8")
        (formal / "contracts.toml").write_text(
            "[profiles.pr]\n"
            "tlc_reuse_max_age_hours = 24\n"
            'required_models = ["sample/Sample"]\n',
            encoding="utf-8",
        )
        (formal / "models.tsv").write_text(
            "sample/Sample\tsafety\tcheck\treason\t30\t60\texhaustive\tno\tno\tno\n",
            encoding="utf-8",
        )
        (formal / "tla2tools.lock").write_text(
            "version=1.7.4\nsha256=" + "a" * 64 + "\n",
            encoding="utf-8",
        )
        summary = root / "build/formal/tlc/pr/sample__Sample/summary.json"
        summary.parent.mkdir(parents=True)

        def write_summary() -> None:
            summary.write_text(
                json.dumps(
                    {
                        "schema": "rustos-formal-evidence-v1",
                        "model": "sample/Sample",
                        "profile": "pr",
                        "status": "passed",
                        "tool": {
                            "name": "TLC",
                            "version": "1.7.4",
                            "sha256": "a" * 64,
                        },
                        "inputs": {
                            "spec_sha256": sha256(spec),
                            "config_sha256": sha256(config),
                        },
                        "policy": {
                            "deadlock": "check",
                            "workers": "auto",
                            "fingerprint": 0,
                            "seed": 1,
                        },
                        "metrics": {
                            "generated": 2,
                            "distinct": 2,
                            "depth": 2,
                            "covered_operators": 1,
                        },
                        "exit_code": 0,
                    }
                ),
                encoding="utf-8",
            )

        write_summary()
        validate_cached_summary(root, "pr", "sample/Sample")

        near_expiry = time.time() - 24 * 3600 + 60
        os.utime(summary, (near_expiry, near_expiry))
        validate_cached_summary(root, "pr", "sample/Sample")
        try:
            validate_cached_summary(
                root,
                "pr",
                "sample/Sample",
                min_remaining_seconds=120,
            )
        except ValueError:
            pass
        else:
            raise AssertionError("TLC cache accepted evidence that expires before sealing")

        spec.write_text("---- MODULE Sample ----\nVARIABLE changed\n====\n", encoding="utf-8")
        expect_rejected(root, "a changed specification")
        spec.write_text("---- MODULE Sample ----\n====\n", encoding="utf-8")

        write_summary()
        value = json.loads(summary.read_text(encoding="utf-8"))
        value["policy"]["deadlock"] = "intentional-terminal"
        summary.write_text(json.dumps(value), encoding="utf-8")
        expect_rejected(root, "a changed deadlock policy")

        write_summary()
        old = summary.stat().st_mtime - 25 * 3600
        os.utime(summary, (old, old))
        expect_rejected(root, "expired evidence")

    print("TLC exact-input cache selftest passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
