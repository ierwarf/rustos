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
    evidence_profile: str


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


def _tla_code(source: str) -> str:
    """Mask comments/strings without joining tokens or hiding continuations."""
    out = list(source)
    depth = 0
    quoted = False
    index = 0
    while index < len(source):
        pair = source[index:index + 2]
        char = source[index]
        if depth:
            if pair == "(*":
                depth += 1
                out[index:index + 2] = "  "
                index += 2
                continue
            if pair == "*)":
                depth -= 1
                out[index:index + 2] = "  "
                index += 2
                continue
            out[index] = "\n" if char == "\n" else " "
        elif quoted:
            out[index] = "\n" if char == "\n" else " "
            if char == "\\" and index + 1 < len(source):
                out[index + 1] = "\n" if source[index + 1] == "\n" else " "
                index += 2
                continue
            if char == '"':
                quoted = False
        elif pair == "(*":
            depth = 1
            out[index:index + 2] = "  "
            index += 2
            continue
        elif pair == "\\*":
            end = source.find("\n", index)
            end = len(source) if end < 0 else end
            out[index:end] = " " * (end - index)
            index = end
            continue
        elif char == '"':
            quoted = True
            out[index] = " "
        index += 1
    if depth or quoted:
        raise ValueError("individual TLC input binding refuses unfinished lexical context")
    return "".join(out)


def individual_input_fingerprint(root: Path, model: str) -> str:
    """Bind a closed proof's inputs before/after TLC, not merely at reuse time.

    Existing models use single-line EXTENDS of these pinned TLC modules. More
    elaborate imports conservatively run TLC again; this is not a TLA parser.
    """
    if any(os.environ.get(key) for key in (
        "TLA_SPEC_OVERRIDE", "TLA_CONFIG_OVERRIDE", "TLA_ARTIFACT_DIR", "TLA_LIBRARY",
        "JAVA_TOOL_OPTIONS", "JDK_JAVA_OPTIONS", "_JAVA_OPTIONS",
    )) or os.environ.get("FORMAL_MUTATION_MODE", "0") != "0":
        raise ValueError("individual TLC input binding refuses execution overrides")
    _model_deadlock_policy(root, model)
    spec = root / "formal" / f"{model}.tla"
    config = root / "formal" / f"{model}.cfg"
    code = _tla_code(spec.read_text(encoding="utf-8"))
    if re.search(r"\bINSTANCE\b", code):
        raise ValueError("individual TLC reuse cannot bind INSTANCE dependencies")
    standard = {"Naturals", "Integers", "FiniteSets", "Sequences", "TLC"}
    for keyword in re.finditer(r"\bEXTENDS\b", code):
        match = re.match(
            r"\s+([A-Za-z_][A-Za-z_0-9]*(?:\s*,\s*[A-Za-z_][A-Za-z_0-9]*)*)",
            code[keyword.end():],
        )
        # Inspect the complete list, not just the keyword's line: a leading
        # comma may continue it after blank lines or nested comments. Keep the
        # admitted subset deliberately single-line even for standard imports.
        if (not match or "\n" in match[1]
                or not set(re.split(r"\s*,\s*", match[1])) <= standard
                or code[keyword.end() + match.end():].lstrip().startswith(",")):
            raise ValueError("individual TLC reuse requires closed pinned-module imports")
    # A local module can shadow a standard module without editing the model.
    # Do not reuse a proof in any such resolver context.
    directories = [spec.parent, root / "formal"] + [
        root / "build/formal/tlc" / profile / model.replace("/", "__")
        for profile in ("pr", "smp-iteration", "nightly")
    ]
    if any((directory / f"{name}.tla").exists() for directory in directories for name in standard):
        raise ValueError("individual TLC input binding refuses shadowed standard modules")
    inputs = {
        "schema": "rustos-closed-tlc-input-v1",
        "spec": _sha256(spec),
        "config": _sha256(config),
        "tool_lock": _sha256(root / "formal/tla2tools.lock"),
        "runner": _sha256(root / "formal/run-tlc.sh"),
        "validator": _sha256(root / "formal/tlc_cache.py"),
    }
    return hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()


def validate_cached_summary(
    root: Path,
    profile: str,
    model: str,
    *,
    now: float | None = None,
    min_remaining_seconds: int = 0,
    individual: bool = False,
) -> CacheValidation:
    """Return validated cache metadata or raise ValueError on any uncertainty."""

    root = root.resolve()
    contracts = tomllib.loads(
        (root / "formal/contracts.toml").read_text(encoding="utf-8")
    )
    profiles = contracts["profiles"]
    if profile not in profiles:
        raise ValueError(f"unknown formal profile: {profile}")
    requested_profile = profiles[profile]
    max_age_hours = requested_profile.get("tlc_reuse_max_age_hours", 0)
    if not isinstance(max_age_hours, int) or max_age_hours <= 0:
        raise ValueError(f"TLC evidence reuse is disabled for profile {profile}")
    if not isinstance(min_remaining_seconds, int) or min_remaining_seconds < 0:
        raise ValueError("TLC minimum remaining lifetime is invalid")
    max_age_seconds = max_age_hours * 3600
    if min_remaining_seconds >= max_age_seconds:
        raise ValueError("TLC minimum remaining lifetime exhausts the reuse window")
    required_models = requested_profile.get("required_models", [])
    if individual:
        # This only reuses one explicitly selected proof. It does not widen the
        # profile's required-model set or qualify a complete verification seal.
        if profile != "pr":
            raise ValueError("individual TLC reuse is restricted to the PR execution policy")
        if os.environ.get("FORMAL_MUTATION_MODE", "0") != "0" or os.environ.get("TLA_ARTIFACT_DIR"):
            raise ValueError("individual TLC reuse refuses mutation or custom-output runs")
        individual_fingerprint = individual_input_fingerprint(root, model)
    elif model not in required_models:
        raise ValueError(f"model is not selected by profile {profile}: {model}")
    if os.environ.get("TLA_SPEC_OVERRIDE") or os.environ.get("TLA_CONFIG_OVERRIDE"):
        raise ValueError("TLC override inputs are never reusable as baseline evidence")

    compatible = requested_profile.get("tlc_compatible_reuse_profiles", [])
    if not isinstance(compatible, list) or not all(
        isinstance(candidate, str) for candidate in compatible
    ):
        raise ValueError(f"TLC compatible reuse profiles are malformed for {profile}")
    candidate_profiles = [profile]
    for candidate in compatible:
        if candidate not in profiles:
            raise ValueError(f"unknown TLC compatible reuse profile: {candidate}")
        reciprocal = profiles[candidate].get("tlc_compatible_reuse_profiles", [])
        if profile not in reciprocal:
            raise ValueError(
                f"TLC compatible reuse is not reciprocal: {profile} -> {candidate}"
            )
        if model in profiles[candidate].get("required_models", []):
            candidate_profiles.append(candidate)

    errors: list[str] = []
    for evidence_profile in candidate_profiles:
        evidence_max_age_hours = profiles[evidence_profile].get(
            "tlc_reuse_max_age_hours", 0
        )
        if not isinstance(evidence_max_age_hours, int) or evidence_max_age_hours <= 0:
            errors.append(f"{evidence_profile}: TLC evidence reuse is disabled")
            continue
        effective_max_age_hours = min(max_age_hours, evidence_max_age_hours)
        effective_max_age_seconds = effective_max_age_hours * 3600
        if min_remaining_seconds >= effective_max_age_seconds:
            errors.append(
                f"{evidence_profile}: TLC minimum remaining lifetime exhausts the reuse window"
            )
            continue
        summary = (
            root
            / "build/formal/tlc"
            / evidence_profile
            / model.replace("/", "__")
            / "summary.json"
        )
        try:
            result = _validate_candidate(
                root,
                profile,
                evidence_profile,
                model,
                summary,
                max_age_seconds=effective_max_age_seconds,
                max_age_hours=effective_max_age_hours,
                min_remaining_seconds=min_remaining_seconds,
                now=now,
            )
            if individual:
                value = json.loads(summary.read_text(encoding="utf-8"))
                _require_equal(value.get("inputs", {}).get("individual_input_sha256"),
                    individual_fingerprint, "closed proof input binding")
            return result
        except (OSError, ValueError, json.JSONDecodeError) as error:
            errors.append(f"{evidence_profile}: {error}")
    raise ValueError("; ".join(errors))


def _validate_candidate(
    root: Path,
    requested_profile: str,
    evidence_profile: str,
    model: str,
    summary: Path,
    *,
    max_age_seconds: int,
    max_age_hours: int,
    min_remaining_seconds: int,
    now: float | None,
) -> CacheValidation:
    try:
        value = json.loads(summary.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cached TLC summary is unavailable: {summary}") from error

    _require_equal(value.get("schema"), "rustos-formal-evidence-v1", "schema")
    _require_equal(value.get("status"), "passed", "status")
    _require_equal(value.get("model"), model, "model")
    _require_equal(value.get("profile"), evidence_profile, "profile")
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

    expected_policy = _expected_policy(requested_profile)
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
    if age_seconds > max_age_seconds:
        raise ValueError(
            f"cached TLC summary exceeds {max_age_hours} hour reuse limit"
        )
    if age_seconds > max_age_seconds - min_remaining_seconds:
        raise ValueError(
            "cached TLC summary cannot remain valid through the seal reserve"
        )
    return CacheValidation(
        summary=summary,
        age_seconds=max(age_seconds, 0.0),
        evidence_profile=evidence_profile,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--min-remaining-seconds", type=int, default=0)
    parser.add_argument(
        "--individual", action="store_true",
        help="reuse one explicitly selected registered proof, not a profile gate",
    )
    parser.add_argument("--input-fingerprint", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    try:
        if args.input_fingerprint:
            print(individual_input_fingerprint(args.root.resolve(), args.model))
            return 0
        result = validate_cached_summary(
            args.root,
            args.profile,
            args.model,
            min_remaining_seconds=args.min_remaining_seconds,
            individual=args.individual,
        )
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"TLC cache miss model={args.model}: {error}")
        return 1
    print(
        f"TLC {'individual proof reused' if args.individual else 'reused'} model={args.model} "
        f"evidence_profile={result.evidence_profile} age_seconds={int(result.age_seconds)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
