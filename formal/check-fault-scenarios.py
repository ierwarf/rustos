#!/usr/bin/env python3
"""Reject phantom, duplicate, or falsely claimed RustOS fault scenarios."""

from __future__ import annotations

import argparse
import json
import re
import tomllib
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--summary", type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    failures: list[str] = []
    package_manifests: dict[str, list[Path]] = {}
    for pattern in (
        "Cargo.toml",
        "kernel/*/Cargo.toml",
        "libs/*/Cargo.toml",
        "services/*/Cargo.toml",
        "apps/*/Cargo.toml",
        "tools/*/Cargo.toml",
    ):
        for manifest in root.glob(pattern):
            package = (
                tomllib.loads(manifest.read_text(encoding="utf-8"))
                .get("package", {})
                .get("name")
            )
            if package:
                package_manifests.setdefault(package, []).append(manifest)

    registry_path = root / "formal/fault-scenarios.tsv"
    scenarios: list[dict[str, str]] = []
    for line_number, line in enumerate(
        registry_path.read_text(encoding="utf-8").splitlines(), 1
    ):
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) != 7:
            failures.append(
                f"{registry_path}:{line_number} has {len(fields)} fields"
            )
            continue
        point, severity, owner, source, expected, evidence, witness = fields
        if severity not in {"critical", "high"}:
            failures.append(f"{point}: fault boundary is not critical/high")
        if evidence not in {"kvm-storage", "source-test"}:
            failures.append(f"{point}: invalid runtime evidence class {evidence}")
        source_path = root / source
        if not source_path.is_file():
            failures.append(f"{point}: missing source {source}")
        elif f'"{point}"' not in source_path.read_text(encoding="utf-8"):
            failures.append(f"{point}: source does not contain the exact fault point")
        if not owner or not expected:
            failures.append(f"{point}: owner or expected failure is empty")
        if ":" not in witness:
            failures.append(f"{point}: source witness must be package:test")
        else:
            package, test = witness.split(":", 1)
            manifests = package_manifests.get(package, [])
            if len(manifests) != 1:
                failures.append(
                    f"{point}: source witness package {package!r} is not unique"
                )
            if source_path.is_file() and not re.search(
                rf"\bfn\s+{re.escape(test)}\s*\(",
                source_path.read_text(encoding="utf-8"),
            ):
                failures.append(
                    f"{point}: source does not define witness test {test!r}"
                )
        scenarios.append(
            {
                "point": point,
                "severity": severity,
                "owner": owner,
                "source": source,
                "expected_failure": expected,
                "runtime_evidence": evidence,
                "source_witness": witness,
            }
        )

    points = [scenario["point"] for scenario in scenarios]
    if points != sorted(set(points)):
        failures.append("fault scenario points are not sorted and unique")

    library = (root / "libs/rustos-fault-injection/src/lib.rs").read_text(
        encoding="utf-8"
    )
    match = re.search(
        r"pub const REGISTERED_FAULT_POINTS: &\[&str\] = &\[(.*?)\];",
        library,
        flags=re.DOTALL,
    )
    if match is None:
        registered: list[str] = []
        failures.append("Rust fault-point registry is missing")
    else:
        registered = re.findall(r'"([^"]+)"', match.group(1))
    if registered != points:
        failures.append(
            f"Rust registry differs from scenarios: rust={registered!r} tsv={points!r}"
        )

    config = tomllib.loads((root / "config/rustos.toml").read_text(encoding="utf-8"))
    fault_config = config.get("fault_injection", {})
    if fault_config.get("enabled") is not False:
        failures.append("normal product config must not enable fault injection")
    configured_rules = fault_config.get("rules", [])
    configured_points: list[str] = []
    for rule in configured_rules:
        if not isinstance(rule, str) or "=" not in rule:
            failures.append(f"invalid configured fault rule {rule!r}")
            continue
        point, action = rule.split("=", 1)
        configured_points.append(point)
        if action != "off":
            failures.append(f"default fault rule is active: {rule}")
    if configured_points != points:
        failures.append(
            "default fault rules do not exactly cover the registered points"
        )

    runtime_claims = [
        scenario["point"]
        for scenario in scenarios
        if scenario["runtime_evidence"] == "kvm-storage"
    ]
    if runtime_claims != ["block.flush"]:
        failures.append("storage KVM negative gate must bind exactly block.flush")
    source_claims = [scenario["point"] for scenario in scenarios]
    source_only_points = [
        scenario["point"]
        for scenario in scenarios
        if scenario["runtime_evidence"] == "source-test"
    ]

    summary = {
        "schema": "rustos-fault-scenario-evidence-v2",
        "status": "passed" if not failures else "failed",
        "registered_points": points,
        "source_claims": source_claims,
        "runtime_claims": runtime_claims,
        "source_only_points": source_only_points,
        "source_witnesses": sorted(
            {scenario["source_witness"] for scenario in scenarios}
        ),
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
    print(
        "fault scenario registry passed "
        f"points={len(points)} runtime_claims={len(runtime_claims)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
