#!/usr/bin/env python3
"""Execute the bounded restart and crash-consistency witness matrix."""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
import time
import tomllib
from pathlib import Path


FIELDS = (
    "id",
    "severity",
    "class",
    "owner",
    "disruption",
    "expected_terminal",
    "max_ms",
    "package",
    "features",
    "target",
    "test",
    "source",
)

REQUIRED_DISRUPTIONS = {
    "checkpoint": {"before-commit", "after-commit", "partial-record", "stale-replay"},
    "service-restart": {
        "failed-activation",
        "forced-termination",
        "restart",
        "stale-replay",
    },
    "storage": {"before-commit", "after-commit", "restart", "stale-replay", "timeout"},
}

ALLOWED_TERMINALS = {
    ("checkpoint", "before-commit"): {"no-visible-child-state"},
    ("checkpoint", "after-commit"): {"idempotent-replay"},
    ("checkpoint", "partial-record"): {"rejected-without-restore"},
    ("checkpoint", "stale-replay"): {"no-key-reuse-reclamation"},
    ("service-restart", "restart"): {
        "new-owner-epoch",
        "same-registration-new-epoch",
    },
    ("service-restart", "failed-activation"): {
        "exact-child-retired-before-retry",
    },
    ("service-restart", "forced-termination"): {
        "dependents-revoked-and-terminated",
    },
    ("service-restart", "stale-replay"): {"rejected-owner-snapshot"},
    ("storage", "before-commit"): {"no-ring-publication"},
    ("storage", "after-commit"): {"exact-durability-completion"},
    ("storage", "restart"): {"stale-completion-revokes"},
    ("storage", "stale-replay"): {"rejected-before-submit"},
    ("storage", "timeout"): {"slot-retained-until-completion"},
}


def package_names(root: Path) -> set[str]:
    names: set[str] = set()
    patterns = (
        "Cargo.toml",
        "kernel/*/Cargo.toml",
        "libs/*/Cargo.toml",
        "services/*/Cargo.toml",
        "tests/*/Cargo.toml",
        "tools/*/Cargo.toml",
    )
    for pattern in patterns:
        for manifest in root.glob(pattern):
            package = tomllib.loads(manifest.read_text(encoding="utf-8")).get(
                "package", {}
            )
            if name := package.get("name"):
                names.add(name)
    return names


def read_registry(root: Path) -> list[dict[str, str]]:
    path = root / "formal/recovery-scenarios.tsv"
    packages = package_names(root)
    scenarios: list[dict[str, str]] = []
    identities: set[str] = set()
    witnesses: set[tuple[str, str]] = set()
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw or raw.startswith("#"):
            continue
        fields = raw.split("\t")
        if len(fields) != len(FIELDS):
            raise SystemExit(f"{path}:{number}: expected {len(FIELDS)} fields")
        scenario = dict(zip(FIELDS, fields, strict=True))
        identity = scenario["id"]
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", identity) or identity in identities:
            raise SystemExit(f"{path}:{number}: invalid or duplicate id {identity!r}")
        identities.add(identity)
        if scenario["severity"] not in {"critical", "high"}:
            raise SystemExit(f"{identity}: scenario is not critical/high")
        if scenario["class"] not in REQUIRED_DISRUPTIONS:
            raise SystemExit(f"{identity}: unknown recovery class")
        if scenario["disruption"] not in REQUIRED_DISRUPTIONS[scenario["class"]]:
            raise SystemExit(f"{identity}: invalid disruption for class")
        allowed_terminals = ALLOWED_TERMINALS[
            (scenario["class"], scenario["disruption"])
        ]
        if scenario["expected_terminal"] not in allowed_terminals:
            raise SystemExit(
                f"{identity}: invalid terminal {scenario['expected_terminal']!r}"
            )
        if scenario["package"] not in packages:
            raise SystemExit(f"{identity}: unknown package {scenario['package']}")
        witness = (scenario["package"], scenario["test"])
        if witness in witnesses:
            raise SystemExit(f"{identity}: duplicate recovery witness {witness!r}")
        witnesses.add(witness)
        if scenario["target"] not in {"all", "lib"}:
            raise SystemExit(f"{identity}: target must be all or lib")
        try:
            max_ms = int(scenario["max_ms"])
        except ValueError as error:
            raise SystemExit(f"{identity}: invalid max_ms") from error
        if not 1 <= max_ms <= 30_000:
            raise SystemExit(f"{identity}: max_ms is outside 1..=30000")
        source = root / scenario["source"]
        if not source.is_file():
            raise SystemExit(f"{identity}: missing source {scenario['source']}")
        if not re.search(
            rf"\bfn\s+{re.escape(scenario['test'])}\s*\(",
            source.read_text(encoding="utf-8"),
        ):
            raise SystemExit(f"{identity}: source witness test is missing")
        scenarios.append(scenario)

    if [scenario["id"] for scenario in scenarios] != sorted(identities):
        raise SystemExit("recovery scenario ids must be sorted and unique")
    for class_name, required in REQUIRED_DISRUPTIONS.items():
        observed = {
            scenario["disruption"]
            for scenario in scenarios
            if scenario["class"] == class_name
        }
        if not required <= observed:
            raise SystemExit(
                f"{class_name}: missing disruption classes {sorted(required - observed)}"
            )
    return scenarios


def run_scenario(root: Path, artifact_dir: Path, scenario: dict[str, str]) -> dict:
    command = ["cargo", "test", "-q", "-p", scenario["package"]]
    if scenario["features"] != "-":
        command.extend(["--features", scenario["features"]])
    if scenario["target"] == "lib":
        command.append("--lib")
    command.append(scenario["test"])
    started = time.monotonic()
    result = subprocess.run(
        command,
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=int(scenario["max_ms"]) / 1000,
        env=os.environ.copy(),
        check=False,
    )
    elapsed_ms = round((time.monotonic() - started) * 1000)
    log = artifact_dir / f"{scenario['id']}.log"
    log.write_text(result.stdout, encoding="utf-8")
    if result.returncode != 0:
        print("\n".join(result.stdout.splitlines()[-100:]), file=sys.stderr)
        raise SystemExit(f"{scenario['id']}: witness command failed")
    if not re.search(r"test result: ok\. [1-9][0-9]* passed;", result.stdout):
        raise SystemExit(f"{scenario['id']}: test filter executed no witness")
    return {
        **scenario,
        "max_ms": int(scenario["max_ms"]),
        "elapsed_ms": elapsed_ms,
        "source_sha256": hashlib.sha256(
            (root / scenario["source"]).read_bytes()
        ).hexdigest(),
    }


def main() -> int:
    root = Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )
    scenarios = read_registry(root)
    artifact_dir = root / "build/formal/recovery-scenarios"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    results = [run_scenario(root, artifact_dir, scenario) for scenario in scenarios]
    registry = root / "formal/recovery-scenarios.tsv"
    summary = {
        "schema": "rustos-recovery-scenario-evidence-v1",
        "status": "passed",
        "registry_sha256": hashlib.sha256(registry.read_bytes()).hexdigest(),
        "scenario_count": len(results),
        "scenarios": results,
    }
    (artifact_dir / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(f"recovery scenarios passed count={len(results)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
