#!/usr/bin/env python3
"""Validate source-produced action traces against registered TLA+ pilot semantics."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


REQUEST_OPS = {"snapshot", "launch", "terminate", "ready"}
RESPONSE_OPS = REQUEST_OPS | {"unknown"}


def classify(event: dict[str, object], max_programs: int) -> tuple[str, int]:
    request = event["request_op"]
    response = event["response_op"]
    status = event["status"]
    version = event["version"]
    count = event["count"]
    if not isinstance(count, int) or count < 0:
        raise ValueError("count must be a non-negative integer")
    if version != "current":
        return "protocol", 0
    if status == "server-error":
        return "server-error", 0
    if status != "ok" or response != request:
        return "protocol", 0
    if request == "snapshot":
        return ("success", count) if count <= max_programs else ("overflow", 0)
    return ("success", 0) if count == 0 else ("protocol", 0)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("trace", type=Path)
    parser.add_argument("--max-programs", type=int, default=64)
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()

    failures: list[str] = []
    count = 0
    seen_sequences: set[int] = set()
    for line_number, line in enumerate(args.trace.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        count += 1
        try:
            event = json.loads(line)
            required = {
                "schema", "model", "sequence", "action", "request_op", "response_op",
                "status", "version", "count", "outcome", "payload_count",
            }
            missing = sorted(required - event.keys())
            if missing:
                raise ValueError(f"missing fields {missing}")
            if event["schema"] != "rustos-formal-trace-v1":
                raise ValueError("unsupported schema")
            if event["model"] != "runtime-control-rpc/RuntimeControlRpc":
                raise ValueError("wrong model")
            if event["action"] != "ReceiveResponse":
                raise ValueError("action is not admitted by this pilot")
            if event["request_op"] not in REQUEST_OPS or event["response_op"] not in RESPONSE_OPS:
                raise ValueError("operation is outside the TLA+ state space")
            sequence = event["sequence"]
            if not isinstance(sequence, int) or sequence < 0 or sequence in seen_sequences:
                raise ValueError("sequence must be a unique non-negative integer")
            seen_sequences.add(sequence)
            expected_outcome, expected_payload = classify(event, args.max_programs)
            if (event["outcome"], event["payload_count"]) != (expected_outcome, expected_payload):
                raise ValueError(
                    f"source result {(event['outcome'], event['payload_count'])} != "
                    f"spec result {(expected_outcome, expected_payload)}"
                )
        except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
            failures.append(f"line {line_number}: {error}")

    if count == 0:
        failures.append("trace is empty")
    if seen_sequences and seen_sequences != set(range(len(seen_sequences))):
        failures.append("sequence numbers are not contiguous from zero")
    status = "passed" if not failures else "failed"
    summary = {
        "schema": "rustos-formal-trace-evidence-v1",
        "model": "runtime-control-rpc/RuntimeControlRpc",
        "status": status,
        "event_count": count,
        "failures": failures,
    }
    if args.summary:
        args.summary.parent.mkdir(parents=True, exist_ok=True)
        args.summary.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if failures:
        for failure in failures:
            print(failure)
        return 1
    print(f"runtime trace conformance passed events={count}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
