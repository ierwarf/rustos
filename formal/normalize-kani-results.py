#!/usr/bin/env python3
"""Create stable Kani evidence and SARIF from pinned 0.67 per-harness output."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path


HARNESS = re.compile(r"^Checking harness (.+)\.\.\.$", re.MULTILINE)
FAILED = re.compile(r"\*\* ([0-9]+) of ([0-9]+) failed")
COVER = re.compile(r"\*\* ([0-9]+) of ([0-9]+) cover properties satisfied")
VERIFICATION = re.compile(r"VERIFICATION:- (SUCCESSFUL|FAILED|UNDETERMINED)")


def harness_sections(text: str) -> list[tuple[str, str]]:
    matches = list(HARNESS.finditer(text))
    return [
        (
            match.group(1),
            text[match.end() : matches[index + 1].start() if index + 1 < len(matches) else len(text)],
        )
        for index, match in enumerate(matches)
    ]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--logs", required=True, type=Path)
    parser.add_argument("--sarif", required=True, type=Path)
    parser.add_argument("--summary", required=True, type=Path)
    args = parser.parse_args()

    harnesses: list[dict[str, object]] = []
    sarif_results: list[dict[str, object]] = []
    for path in sorted(args.logs.glob("*.log")):
        if path.name.endswith("-playback.log"):
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        for harness, section in harness_sections(text):
            failed_match = FAILED.search(section)
            cover_match = COVER.search(section)
            verification_match = VERIFICATION.search(section)
            failed = int(failed_match.group(1)) if failed_match else 1
            checks = int(failed_match.group(2)) if failed_match else 0
            cover_satisfied = int(cover_match.group(1)) if cover_match else 0
            cover_total = int(cover_match.group(2)) if cover_match else 0
            verification = verification_match.group(1) if verification_match else "MISSING"
            status = (
                "passed"
                if verification == "SUCCESSFUL"
                and failed == 0
                and cover_total > 0
                and cover_satisfied == cover_total
                else "failed"
            )
            harnesses.append(
                {
                    "harness": harness,
                    "status": status,
                    "verification": verification.lower(),
                    "checks": checks,
                    "failed_checks": failed,
                    "cover_satisfied": cover_satisfied,
                    "cover_total": cover_total,
                    "output": path.name,
                }
            )
            if verification != "SUCCESSFUL" or failed:
                sarif_results.append(
                    {
                        "ruleId": "kani.verification",
                        "level": "error",
                        "message": {"text": f"Kani verification failed for {harness}"},
                    }
                )
            if cover_total == 0 or cover_satisfied != cover_total:
                sarif_results.append(
                    {
                        "ruleId": "kani.cover",
                        "level": "error",
                        "message": {"text": f"Kani witness coverage incomplete for {harness}: {cover_satisfied}/{cover_total}"},
                    }
                )

    summary = {
        "schema": "rustos-kani-evidence-v1",
        "tool": {"name": "Kani", "version": args.version},
        "status": "passed" if harnesses and all(item["status"] == "passed" for item in harnesses) else "failed",
        "harness_count": len(harnesses),
        "harnesses": harnesses,
    }
    args.summary.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    sarif = {
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "Kani",
                        "version": args.version,
                        "rules": [
                            {"id": "kani.verification", "shortDescription": {"text": "Kani proof failure"}},
                            {"id": "kani.cover", "shortDescription": {"text": "Kani witness coverage failure"}},
                        ],
                    }
                },
                "results": sarif_results,
            }
        ],
    }
    args.sarif.write_text(json.dumps(sarif, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
