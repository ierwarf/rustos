#!/usr/bin/env python3
"""Pass cache for deterministic cargo-test witness groups.

    witness-cache.py key   --package P [--package Q] [--input F] --salt S
    witness-cache.py hit   KEY [DEST]    exit 0 when KEY passed before; restore its output to DEST
    witness-cache.py store KEY [FILE]    record that KEY passed, with FILE as its output

A group's key covers the local dependency closure of the packages it tests
(see package_closure.py), the exact witness selection (`--salt`) and any
registry/script files the group reads (`--input`). Only passes are stored, so a
failure always re-runs. `RUSTOS_WITNESS_CACHE=off` (the nightly profile) makes
every lookup miss, which re-audits the whole surface and bounds the
approximation: a witness that reads a file outside its closure and declared
inputs is only caught by that audit.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from package_closure import Workspace  # noqa: E402

ROOT = Path(
    subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip()
)
STORE = ROOT / "build/formal/witness-cache"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)
    key = sub.add_parser("key")
    key.add_argument("--package", action="append", required=True)
    key.add_argument("--input", action="append", default=[])
    key.add_argument("--salt", default="")
    hit = sub.add_parser("hit")
    hit.add_argument("key")
    hit.add_argument("dest", nargs="?", help="where to restore the recorded output")
    store = sub.add_parser("store")
    store.add_argument("key")
    store.add_argument("output", nargs="?", help="output file to record with the pass")
    args = parser.parse_args()
    if args.cmd == "key":
        print(Workspace(ROOT).digest(args.package, args.input, args.salt))
        return 0
    marker = STORE / args.key
    if args.cmd == "hit":
        if os.environ.get("RUSTOS_WITNESS_CACHE") == "off":
            return 1
        if not marker.is_file():
            return 1
        if args.dest:
            Path(args.dest).write_bytes(marker.read_bytes())
        return 0
    STORE.mkdir(parents=True, exist_ok=True)
    body = Path(args.output).read_bytes() if args.output else b"passed\n"
    temporary = marker.with_suffix(f".{os.getpid()}.tmp")
    temporary.write_bytes(body)
    temporary.replace(marker)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
