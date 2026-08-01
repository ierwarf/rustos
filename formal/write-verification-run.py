#!/usr/bin/env python3
"""Seal one completed formal profile to the exact source tree and gate outputs."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import tomllib
from pathlib import Path

from tlc_cache import validate_cached_summary


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
    digest = hashlib.sha256()
    for raw_path in sorted(path for path in output.split(b"\0") if path):
        relative = raw_path.decode("utf-8")
        digest.update(raw_path)
        digest.update(b"\0")
        digest.update((root / relative).read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(64 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def require_passed(path: Path) -> None:
    value = json.loads(path.read_text(encoding="utf-8"))
    if value.get("status") != "passed":
        raise ValueError(f"evidence is not passed: {path}")


def require_fresh(path: Path, marker_mtime_ns: int) -> None:
    if path.stat().st_mtime_ns < marker_mtime_ns:
        raise ValueError(f"evidence predates this verification run: {path}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--not-before", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    contracts = tomllib.loads(
        (root / "formal/contracts.toml").read_text(encoding="utf-8")
    )
    profiles = contracts["profiles"]
    if args.profile not in profiles:
        raise ValueError(f"unknown formal verification profile: {args.profile}")
    profile = profiles[args.profile]
    required = [
        root / path
        for path in profile["required_evidence"]
        if not path.endswith("/kvm-p0-summary.json")
    ]
    models = profile.get("required_models")
    if models is None:
        models = []
        for line in (root / contracts["models"]).read_text(
            encoding="utf-8"
        ).splitlines():
            if line and not line.startswith("#"):
                models.append(line.split("\t", 1)[0])
    tlc_paths = {
        root
        / "build/formal/tlc"
        / args.profile
        / model.replace("/", "__")
        / "summary.json": model
        for model in models
    }
    required = sorted(set(required) | set(tlc_paths))
    not_before = args.not_before.resolve()
    if not_before.parent != (root / "build/formal/verification-run").resolve():
        raise ValueError("verification run marker is outside the evidence directory")
    marker_mtime_ns = not_before.stat().st_mtime_ns
    artifacts = []
    for path in required:
        if path in tlc_paths and profile.get("tlc_reuse_max_age_hours", 0) > 0:
            validate_cached_summary(root, args.profile, tlc_paths[path])
        else:
            require_fresh(path, marker_mtime_ns)
        require_passed(path)
        artifacts.append(
            {
                "path": path.relative_to(root).as_posix(),
                "sha256": sha256(path),
                "bytes": path.stat().st_size,
            }
        )
    summary = {
        "schema": "rustos-formal-verification-run-v1",
        "status": "passed",
        "profile": args.profile,
        "source_tree_sha256": source_tree_sha256(root),
        "artifacts": artifacts,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(
        f"formal verification run sealed profile={args.profile} "
        f"artifacts={len(artifacts)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
