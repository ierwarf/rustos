#!/usr/bin/env python3
"""Replay kernel-timestamped KVM evidence against one product scenario."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
from pathlib import Path


def load_scenario(registry: Path, topology: str) -> list[dict[str, object]]:
    candidates: dict[str, list[dict[str, object]]] = {}
    for line_number, line in enumerate(
        registry.read_text(encoding="utf-8").splitlines(), 1
    ):
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) != 11:
            raise ValueError(
                f"{registry}:{line_number} has {len(fields)} scenario fields"
            )
        if fields[1] != topology:
            continue
        record = {
            "scenario": fields[0],
            "topology": fields[1],
            "sequence": int(fields[2]),
            "step": fields[3],
            "flow": fields[4],
            "transition": fields[5],
            "model": fields[6],
            "log": fields[7],
            "marker": fields[8],
            "requires": [item for item in fields[9].split(",") if item],
            "deadline_ms": int(fields[10]),
        }
        candidates.setdefault(fields[0], []).append(record)
    if len(candidates) != 1:
        raise ValueError(
            f"topology {topology} must select exactly one scenario, found {sorted(candidates)}"
        )
    steps = next(iter(candidates.values()))
    steps.sort(key=lambda step: int(step["sequence"]))
    return steps


def source_tree_sha256(root: Path) -> str:
    output = subprocess.run(
        [
            "git",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        cwd=root,
        check=True,
        capture_output=True,
    ).stdout
    paths = sorted(path for path in output.split(b"\0") if path)
    digest = hashlib.sha256()
    for raw_path in paths:
        relative = raw_path.decode("utf-8")
        digest.update(raw_path)
        digest.update(b"\0")
        digest.update((root / relative).read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("trace", type=Path)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--registry", type=Path, required=True)
    parser.add_argument("--topology", required=True)
    parser.add_argument("--summary", type=Path, required=True)
    parser.add_argument(
        "--classify-stale",
        action="store_true",
        help="return 3, not a validation failure, when only bound inputs are stale",
    )
    args = parser.parse_args()

    failures: list[str] = []
    events: list[dict[str, object]] = []
    try:
        scenario = load_scenario(args.registry, args.topology)
    except (OSError, ValueError) as error:
        scenario = []
        failures.append(f"scenario registry: {error}")

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
                "scenario",
                "sequence",
                "step",
                "flow",
                "transition",
                "model",
                "log",
                "marker",
                "requires",
                "outcome",
                "guest_ts_us",
                "elapsed_ms",
                "deadline_ms",
                "source_line",
                "host_run_elapsed_ms",
                "network_exercised",
                "fps_proof",
                "source_tree_sha256",
                "rustos_boot_image_sha256",
                "dvm_manifest_sha256",
            }
            missing = sorted(required - event.keys())
            if missing:
                raise ValueError(f"missing fields {missing}")
            if event["schema"] != "rustos-formal-runtime-event-v4":
                raise ValueError("unsupported schema")
            if event["topology"] != args.topology:
                raise ValueError("event topology differs from selected topology")
            if event["outcome"] != "success":
                raise ValueError("runtime evidence is not a successful terminal")
            for field in (
                "source_tree_sha256",
                "rustos_boot_image_sha256",
                "dvm_manifest_sha256",
            ):
                if (
                    not isinstance(event[field], str)
                    or len(event[field]) != 64
                    or any(character not in "0123456789abcdef" for character in event[field])
                ):
                    raise ValueError(f"{field} is not a SHA-256 digest")
            for field in (
                "sequence",
                "guest_ts_us",
                "elapsed_ms",
                "deadline_ms",
                "source_line",
                "host_run_elapsed_ms",
            ):
                if not isinstance(event[field], int) or int(event[field]) < 0:
                    raise ValueError(f"{field} is not a nonnegative integer")
            if int(event["source_line"]) == 0:
                raise ValueError("source_line is not one-based")
            if int(event["elapsed_ms"]) != (int(event["guest_ts_us"]) + 999) // 1000:
                raise ValueError("elapsed_ms is not derived from guest_ts_us")
            if int(event["elapsed_ms"]) > int(event["deadline_ms"]):
                raise ValueError("event missed its absolute scenario deadline")
            if (
                not isinstance(event["requires"], list)
                or not event["requires"]
                or any(not isinstance(item, str) or not item for item in event["requires"])
            ):
                raise ValueError("requires is not a nonempty string list")
            events.append(event)
        except (TypeError, ValueError, json.JSONDecodeError) as error:
            failures.append(f"line {line_number}: {error}")

    if not events:
        failures.append("trace is empty")
        run_id = None
        scenario_name = None
    else:
        run_id = events[0]["run_id"]
        scenario_name = events[0]["scenario"]
        if any(event["run_id"] != run_id for event in events):
            failures.append("trace mixes run identities")
        if any(event["scenario"] != scenario_name for event in events):
            failures.append("trace mixes scenarios")
        for field in (
            "source_tree_sha256",
            "rustos_boot_image_sha256",
            "dvm_manifest_sha256",
        ):
            if any(event[field] != events[0][field] for event in events):
                failures.append(f"trace mixes {field} identities")
        observed = {str(event["step"]): event for event in events}
        for event in events:
            for required in event["requires"]:
                if required == "START":
                    continue
                predecessor = observed.get(str(required))
                if predecessor is None:
                    failures.append(
                        f"event {event['step']} requires missing predecessor {required}"
                    )
                elif int(predecessor["sequence"]) >= int(event["sequence"]):
                    failures.append(
                        f"event {event['step']} requires non-prior predecessor {required}"
                    )
                elif int(predecessor["guest_ts_us"]) > int(event["guest_ts_us"]):
                    failures.append(
                        f"event {event['step']} predates prerequisite {required}"
                    )

    if len(events) != len(scenario):
        failures.append(
            f"trace event count {len(events)} differs from scenario {len(scenario)}"
        )
    for index, (event, expected) in enumerate(zip(events, scenario)):
        for field in (
            "scenario",
            "topology",
            "sequence",
            "step",
            "flow",
            "transition",
            "model",
            "log",
            "marker",
            "requires",
            "deadline_ms",
        ):
            if event[field] != expected[field]:
                failures.append(
                    f"event {index} {field}={event[field]!r}, expected {expected[field]!r}"
                )

    trace_sha256 = hashlib.sha256(args.trace.read_bytes()).hexdigest()
    registry_sha256 = hashlib.sha256(args.registry.read_bytes()).hexdigest()
    current_source_sha256 = source_tree_sha256(args.root.resolve())
    rustos_boot_image_sha256 = hashlib.sha256(
        (args.root / "build/rustos-boot.img").read_bytes()
    ).hexdigest()
    dvm_manifest_sha256 = hashlib.sha256(
        (
            args.root
            / "driver-domains/linux/out/artifacts/rustos-linux-dvm-x86_64.manifest"
        ).read_bytes()
    ).hexdigest()
    stale_inputs: list[str] = []
    if events:
        for field, current in (
            ("source_tree_sha256", current_source_sha256),
            ("rustos_boot_image_sha256", rustos_boot_image_sha256),
            ("dvm_manifest_sha256", dvm_manifest_sha256),
        ):
            if events[0][field] != current:
                stale_inputs.append(f"trace {field} does not match current input")
    if stale_inputs and not args.classify_stale:
        failures.extend(stale_inputs)
    summary = {
        "schema": "rustos-kvm-formal-trace-evidence-v4",
        "status": (
            "failed"
            if failures
            else "stale"
            if stale_inputs
            else "passed"
        ),
        "run_id": run_id,
        "topology": args.topology,
        "scenario": scenario_name,
        "models": sorted({str(event["model"]) for event in events}),
        "event_count": len(events),
        "terminal_elapsed_ms": (
            max(int(event["elapsed_ms"]) for event in events) if events else None
        ),
        "trace_sha256": trace_sha256,
        "scenario_registry_sha256": registry_sha256,
        "source_tree_sha256": current_source_sha256,
        "rustos_boot_image_sha256": rustos_boot_image_sha256,
        "dvm_manifest_sha256": dvm_manifest_sha256,
        "failures": failures,
        "stale_inputs": stale_inputs,
    }
    args.summary.parent.mkdir(parents=True, exist_ok=True)
    args.summary.write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    if failures:
        for failure in failures:
            print(failure)
        return 1
    if stale_inputs:
        for stale in stale_inputs:
            print(stale)
        return 3
    print(
        f"kvm runtime trace conformance passed scenario={scenario_name} "
        f"events={len(events)} terminal_ms={events[-1]['elapsed_ms']}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
