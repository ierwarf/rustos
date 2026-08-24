#!/usr/bin/env python3
"""Reuse an exact-tree formal seal after revalidating every artifact digest."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path


def source_tree_sha256(root: Path) -> str:
    output = subprocess.run(
        [
            "git", "ls-files", "-z", "--cached", "--others",
            "--exclude-standard",
        ],
        cwd=root,
        check=True,
        capture_output=True,
    ).stdout
    digest = hashlib.sha256()
    for raw_path in sorted(path for path in output.split(b"\0") if path):
        digest.update(raw_path)
        digest.update(b"\0")
        digest.update((root / raw_path.decode("utf-8")).read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(64 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--profile", required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    seal = root / "build/formal/verification-run" / f"{args.profile}.json"
    try:
        payload = json.loads(seal.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return 1
    if (
        payload.get("schema") != "rustos-formal-verification-run-v1"
        or payload.get("status") != "passed"
        or payload.get("profile") != args.profile
        or payload.get("source_tree_sha256") != source_tree_sha256(root)
    ):
        return 1
    artifacts = payload.get("artifacts")
    if not isinstance(artifacts, list) or not artifacts:
        return 1
    for artifact in artifacts:
        if not isinstance(artifact, dict):
            return 1
        relative = artifact.get("path")
        if not isinstance(relative, str) or Path(relative).is_absolute() or ".." in Path(relative).parts:
            return 1
        path = root / relative
        if (
            not path.is_file()
            or path.stat().st_size != artifact.get("bytes")
            or sha256(path) != artifact.get("sha256")
        ):
            return 1
        try:
            evidence = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return 1
        if evidence.get("status") != "passed":
            return 1
    print(
        f"formal gate reused exact-tree seal profile={args.profile} "
        f"artifacts={len(artifacts)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
