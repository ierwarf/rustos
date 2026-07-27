#!/usr/bin/env python3
"""Fail closed when proof assumptions or verified-configuration claims drift."""

from __future__ import annotations

import csv
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ASSUMPTIONS = ROOT / "formal/proof-assumptions.tsv"
CONFIGURATIONS = ROOT / "formal/verified-configurations.tsv"

REQUIRED_CATEGORIES = {
    "assembly",
    "hardware",
    "boot",
    "dma",
    "toolchain",
    "external-kernel",
    "observability",
    "hypervisor",
    "physical-hardware",
    "side-channels",
}
ASSUMPTION_STATUSES = {
    "assumed-reviewed",
    "environment-assumption",
    "validated-boundary",
    "assumed-controlled",
    "excluded",
}
CONFIGURATION_STATUSES = {"evidence-complete", "artifact-only", "excluded"}


def rows(path: Path, expected_header: list[str]) -> list[dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        if reader.fieldnames != expected_header:
            raise SystemExit(
                f"{path.relative_to(ROOT)} header drifted: {reader.fieldnames!r}"
            )
        result = list(reader)
    if not result:
        raise SystemExit(f"{path.relative_to(ROOT)} is empty")
    for line, row in enumerate(result, start=2):
        if any(not value.strip() for value in row.values()):
            raise SystemExit(
                f"{path.relative_to(ROOT)}:{line}: empty proof-boundary field"
            )
    return result


def main() -> None:
    assumptions = rows(
        ASSUMPTIONS,
        ["id", "category", "scope", "statement", "validation", "evidence", "status"],
    )
    configurations = rows(
        CONFIGURATIONS,
        [
            "id",
            "platform",
            "topology",
            "evidence_profile",
            "properties",
            "assumptions",
            "status",
        ],
    )

    assumption_ids: set[str] = set()
    categories: set[str] = set()
    for line, row in enumerate(assumptions, start=2):
        assumption_id = row["id"]
        if assumption_id in assumption_ids:
            raise SystemExit(f"formal/proof-assumptions.tsv:{line}: duplicate id")
        assumption_ids.add(assumption_id)
        categories.add(row["category"])
        if row["status"] not in ASSUMPTION_STATUSES:
            raise SystemExit(
                f"formal/proof-assumptions.tsv:{line}: unknown status {row['status']}"
            )
        evidence = ROOT / row["evidence"]
        if not evidence.exists():
            raise SystemExit(
                f"formal/proof-assumptions.tsv:{line}: missing evidence path "
                f"{row['evidence']}"
            )

    missing_categories = sorted(REQUIRED_CATEGORIES - categories)
    if missing_categories:
        raise SystemExit(
            "proof assumption registry misses high-risk categories: "
            + ",".join(missing_categories)
        )

    configuration_ids: set[str] = set()
    evidence_complete = 0
    for line, row in enumerate(configurations, start=2):
        configuration_id = row["id"]
        if configuration_id in configuration_ids:
            raise SystemExit(
                f"formal/verified-configurations.tsv:{line}: duplicate id"
            )
        configuration_ids.add(configuration_id)
        status = row["status"]
        if status not in CONFIGURATION_STATUSES:
            raise SystemExit(
                f"formal/verified-configurations.tsv:{line}: unknown status {status}"
            )
        references = row["assumptions"].split(",")
        if len(references) != len(set(references)):
            raise SystemExit(
                f"formal/verified-configurations.tsv:{line}: duplicate assumption"
            )
        unknown = sorted(set(references) - assumption_ids)
        if unknown:
            raise SystemExit(
                f"formal/verified-configurations.tsv:{line}: unknown assumptions "
                + ",".join(unknown)
            )
        is_physical = "physical" in row["platform"] or "physical" in row["topology"]
        if is_physical and status != "excluded":
            raise SystemExit(
                f"formal/verified-configurations.tsv:{line}: physical topology "
                "cannot inherit QEMU evidence"
            )
        if status == "evidence-complete":
            evidence_complete += 1
            if row["evidence_profile"] not in {"pr", "pr-negative"}:
                raise SystemExit(
                    f"formal/verified-configurations.tsv:{line}: unsealed profile"
                )
            if "TRACE-001" not in references or "QEMU-001" not in references:
                raise SystemExit(
                    f"formal/verified-configurations.tsv:{line}: runtime evidence "
                    "assumptions are incomplete"
                )
            if row["properties"] == "none":
                raise SystemExit(
                    f"formal/verified-configurations.tsv:{line}: empty claim"
                )

    if evidence_complete < 2:
        raise SystemExit("positive and negative QEMU evidence profiles are both required")

    print(
        "proof boundary registry passed "
        f"assumptions={len(assumptions)} configurations={len(configurations)}"
    )


if __name__ == "__main__":
    main()
