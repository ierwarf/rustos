#!/usr/bin/env python3
"""Normalize a TLC 1.7.x text counterexample into stable JSON evidence."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path


# Toolbox-style output prefixes snapshots with "State"; TLC's machine-format
# stream uses the bare ordinal.  Both name the same counterexample state and
# must normalize identically for the mutation-evidence hash.
STATE = re.compile(r"^(?:State )?([0-9]+):(?: <(.+)>)?$")
ASSIGNMENT = re.compile(r"^/\\ ([A-Za-z_][A-Za-z0-9_]*) = (.*)$")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--log", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    states: list[dict[str, object]] = []
    current: dict[str, object] | None = None
    for line in args.log.read_text(encoding="utf-8", errors="replace").splitlines():
        match = STATE.match(line)
        if match:
            current = {
                "index": int(match.group(1)),
                "action": match.group(2) or "Initial predicate",
                "assignments": {},
            }
            states.append(current)
            continue
        assignment = ASSIGNMENT.match(line)
        if assignment and current is not None:
            values = current["assignments"]
            assert isinstance(values, dict)
            values[assignment.group(1)] = assignment.group(2)

    payload = {
        "schema": "rustos-tlc-counterexample-v1",
        "model": args.model,
        "state_count": len(states),
        "states": states,
    }
    args.output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
