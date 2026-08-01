#!/usr/bin/env python3
"""Validate reusable TLC evidence against the exact current proof inputs."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class CacheValidation:
    summary: Path
    age_seconds: float


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(64 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _lock_values(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        if not separator:
            raise ValueError(f"malformed TLC tool lock line: {line}")
        values[key] = value
    return values


def _model_deadlock_policy(root: Path, model: str) -> str:
    matches = []
    for line in (root / "formal/models.tsv").read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if fields[0] == model:
            matches.append(fields)
    if len(matches) != 1 or len(matches[0]) < 3:
        raise ValueError(f"model is not uniquely registered: {model}")
    return matches[0][2]


def _expected_policy(profile: str) -> dict[str, Any]:
    if profile in {"pr", "smp-iteration"}:
        workers = os.environ.get("TLC_WORKERS", "auto")
        fingerprint = int(os.environ.get("TLC_FP", "0"))
        seed = int(os.environ.get("TLC_SEED", "1"))
    elif profile == "nightly":
        workers = os.environ.get("TLC_WORKERS", "1")
        fingerprint = int(os.environ.get("TLC_FP", "127"))
        seed = int(os.environ.get("TLC_SEED", "20260721"))
    else:
        raise ValueError(f"unknown TLC profile: {profile}")
    return {"workers": workers, "fingerprint": fingerprint, "seed": seed}


def _require_equal(actual: Any, expected: Any, field: str) -> None:
    if actual != expected:
        raise ValueError(f"cached TLC {field} differs from current input")


def validate_cached_summary(
    root: Path,
    profile: str,
    model: str,
    *,
    now: float | None = None,
) -> CacheValidation:
    """Return validated cache metadata or raise ValueError on any uncertainty."""

    root = root.resolve()
    contracts = tomllib.loads(
        (root / "formal/contracts.toml").read_text(encoding="utf-8")
    )
    profiles = contracts["profiles"]
    if profile not in profiles:
        raise ValueError(f"unknown formal profile: {profile}")
    max_age_hours = profiles[profile].get("tlc_reuse_max_age_hours", 0)
    if not isinstance(max_age_hours, int) or max_age_hours <= 0:
        raise ValueError(f"TLC evidence reuse is disabled for profile {profile}")
    required_models = profiles[profile].get("required_models", [])
    if model not in required_models:
        raise ValueError(f"model is not selected by profile {profile}: {model}")
    if os.environ.get("TLA_SPEC_OVERRIDE") or os.environ.get("TLA_CONFIG_OVERRIDE"):
        raise ValueError("TLC override inputs are never reusable as baseline evidence")

    summary = (
        root
        / "build/formal/tlc"
        / profile
        / model.replace("/", "__")
        / "summary.json"
    )
    try:
        value = json.loads(summary.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cached TLC summary is unavailable: {summary}") from error

    _require_equal(value.get("schema"), "rustos-formal-evidence-v1", "schema")
    _require_equal(value.get("status"), "passed", "status")
    _require_equal(value.get("model"), model, "model")
    _require_equal(value.get("profile"), profile, "profile")
    _require_equal(value.get("exit_code"), 0, "exit code")

    lock = _lock_values(root / "formal/tla2tools.lock")
    if not lock.get("version") or not re.fullmatch(r"[0-9a-f]{64}", lock.get("sha256", "")):
        raise ValueError("pinned TLC tool identity is malformed")
    tool = value.get("tool", {})
    _require_equal(tool.get("name"), "TLC", "tool name")
    _require_equal(tool.get("version"), lock.get("version"), "tool version")
    _require_equal(tool.get("sha256"), lock.get("sha256"), "tool digest")

    spec = root / "formal" / f"{model}.tla"
    config = root / "formal" / f"{model}.cfg"
    inputs = value.get("inputs", {})
    _require_equal(inputs.get("spec_sha256"), _sha256(spec), "specification digest")
    _require_equal(inputs.get("config_sha256"), _sha256(config), "configuration digest")

    expected_policy = _expected_policy(profile)
    expected_policy["deadlock"] = _model_deadlock_policy(root, model)
    _require_equal(value.get("policy"), expected_policy, "execution policy")

    metrics = value.get("metrics")
    if not isinstance(metrics, dict):
        raise ValueError("cached TLC metrics are absent")
    for field in ("generated", "distinct", "depth", "covered_operators"):
        metric = metrics.get(field)
        if not isinstance(metric, int) or isinstance(metric, bool) or metric <= 0:
            raise ValueError(f"cached TLC metric is not positive: {field}")

    observed_now = time.time() if now is None else now
    age_seconds = observed_now - summary.stat().st_mtime
    if age_seconds < -300:
        raise ValueError("cached TLC summary timestamp is implausibly in the future")
    if age_seconds > max_age_hours * 3600:
        raise ValueError(
            f"cached TLC summary exceeds {max_age_hours} hour reuse limit"
        )
    return CacheValidation(summary=summary, age_seconds=max(age_seconds, 0.0))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--model", required=True)
    args = parser.parse_args()
    try:
        result = validate_cached_summary(args.root, args.profile, args.model)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"TLC cache miss model={args.model}: {error}")
        return 1
    print(
        f"TLC reused model={args.model} "
        f"age_seconds={int(result.age_seconds)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
