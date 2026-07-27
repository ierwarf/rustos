#!/usr/bin/env python3
"""Regression test for stale-artifact rejection in the profile sealer."""

from __future__ import annotations

import importlib.util
import os
import tempfile
from pathlib import Path


def load_sealer(root: Path):
    path = root / "formal/write-verification-run.py"
    spec = importlib.util.spec_from_file_location("rustos_verification_sealer", path)
    if spec is None or spec.loader is None:
        raise SystemExit("could not load verification sealer")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    sealer = load_sealer(root)
    with tempfile.TemporaryDirectory(prefix="rustos-sealer-selftest-") as temporary:
        directory = Path(temporary)
        stale = directory / "stale.json"
        marker = directory / "marker"
        current = directory / "current.json"
        stale.write_text("{}\n", encoding="utf-8")
        marker.write_text("\n", encoding="utf-8")
        current.write_text("{}\n", encoding="utf-8")
        marker_ns = marker.stat().st_mtime_ns
        os.utime(stale, ns=(marker_ns - 1_000_000, marker_ns - 1_000_000))
        os.utime(current, ns=(marker_ns + 1_000_000, marker_ns + 1_000_000))
        try:
            sealer.require_fresh(stale, marker_ns)
        except ValueError:
            pass
        else:
            raise SystemExit("stale verification artifact was accepted")
        sealer.require_fresh(current, marker_ns)
    print("verification-run freshness selftest passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
