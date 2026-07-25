#!/usr/bin/env python3
"""Replay product-path KVM evidence against the admitted P0 model actions."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


MODEL_ACTIONS = {
    "rootd-bootstrap/RootdBootstrap": "CoreServicesReady",
    "dvm-block-startup/DvmBlockStartup": "GenerationBoundDataPlaneProven",
    "input-ingestion-worker/InputIngestionWorker": "AuthenticatedRelayReady",
    "dvm-display-readiness/DvmDisplayReadiness": "GenerationBoundSurfaceReady",
    "network-payload-session/NetworkPayloadSession": "AuthenticatedRoundTrip",
    "ui-frame-budget/UiFrameBudget": "FrameBudgetSatisfied",
}

REQUIRED = {
    "storage-dvm": {
        "rootd-bootstrap/RootdBootstrap",
        "dvm-block-startup/DvmBlockStartup",
    },
    "qemu-commercial": {
        "rootd-bootstrap/RootdBootstrap",
    },
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("trace", type=Path)
    parser.add_argument("--summary", type=Path, required=True)
    args = parser.parse_args()

    failures: list[str] = []
    events: list[dict[str, object]] = []
    for line_number, line in enumerate(
        args.trace.read_text(encoding="utf-8").splitlines(), 1
    ):
        if not line.strip():
            continue
        try:
            event = json.loads(line)
            required = {
                "schema",
                "run_id",
                "topology",
                "sequence",
                "model",
                "action",
                "outcome",
                "elapsed_ms",
            }
            missing = sorted(required - event.keys())
            if missing:
                raise ValueError(f"missing fields {missing}")
            if event["schema"] != "rustos-formal-runtime-event-v1":
                raise ValueError("unsupported schema")
            if event["model"] not in MODEL_ACTIONS:
                raise ValueError("unregistered runtime model")
            if event["action"] != MODEL_ACTIONS[event["model"]]:
                raise ValueError("action does not match the model admission point")
            if event["outcome"] != "success":
                raise ValueError("runtime evidence is not a successful terminal")
            if not isinstance(event["elapsed_ms"], int) or not 0 <= event["elapsed_ms"] <= 30_000:
                raise ValueError("runtime evidence exceeds the 30-second product gate")
            events.append(event)
        except (TypeError, ValueError, json.JSONDecodeError) as error:
            failures.append(f"line {line_number}: {error}")

    if not events:
        failures.append("trace is empty")
        topology = "unknown"
        run_id = None
    else:
        topology = events[0]["topology"]
        run_id = events[0]["run_id"]
        if topology not in REQUIRED:
            failures.append(f"unknown topology {topology}")
        if any(event["topology"] != topology for event in events):
            failures.append("trace mixes topologies")
        if any(event["run_id"] != run_id for event in events):
            failures.append("trace mixes run identities")
        sequences = [event["sequence"] for event in events]
        if sequences != list(range(len(events))):
            failures.append("sequences are not contiguous and ordered")
        models = [event["model"] for event in events]
        if len(models) != len(set(models)):
            failures.append("trace repeats a model admission action")
        if models[0] != "rootd-bootstrap/RootdBootstrap":
            failures.append("rootd bootstrap is not the first admitted action")
        missing_models = REQUIRED.get(str(topology), set()) - set(models)
        if missing_models:
            failures.append(
                "topology lacks required runtime models: "
                + ", ".join(sorted(missing_models))
            )

    summary = {
        "schema": "rustos-kvm-formal-trace-evidence-v1",
        "status": "passed" if not failures else "failed",
        "run_id": run_id,
        "topology": topology,
        "models": sorted({str(event["model"]) for event in events}),
        "event_count": len(events),
        "failures": failures,
    }
    args.summary.parent.mkdir(parents=True, exist_ok=True)
    args.summary.write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    if failures:
        for failure in failures:
            print(failure)
        return 1
    print(f"kvm runtime trace conformance passed events={len(events)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
