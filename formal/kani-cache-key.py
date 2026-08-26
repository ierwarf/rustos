#!/usr/bin/env python3
"""Compute content-addressed Kani keys for local dependency closures."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
from pathlib import Path


POLICY_INPUTS = (
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    "rust-toolchain.toml",
    "formal/kani.lock",
    "formal/proof-index.toml",
    "formal/check-proof-index.py",
    "formal/run-kani.sh",
    "formal/normalize-kani-results.py",
)


def tracked_files(root: Path, directories: set[Path]) -> set[Path]:
    arguments = [
        "git",
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
        "--",
    ]
    arguments.extend(sorted(path.relative_to(root).as_posix() for path in directories))
    output = subprocess.run(
        arguments, cwd=root, check=True, capture_output=True
    ).stdout
    return {
        root / raw.decode("utf-8")
        for raw in output.split(b"\0")
        if raw
    }


def update_file(digest: "hashlib._Hash", root: Path, path: Path) -> None:
    relative = path.relative_to(root).as_posix().encode("utf-8")
    digest.update(relative)
    digest.update(b"\0")
    digest.update(path.read_bytes())
    digest.update(b"\0")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--package", action="append", required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            cwd=root,
            check=True,
            capture_output=True,
        ).stdout
    )
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    workspace_members = set(metadata["workspace_members"])

    selected: dict[str, str] = {}
    for name in args.package:
        matches = [
            package_id
            for package_id in workspace_members
            if packages[package_id]["name"] == name
        ]
        if len(matches) != 1:
            raise ValueError(f"Kani package {name!r} resolved {len(matches)} workspace members")
        selected[name] = matches[0]

    policy_paths = {root / relative for relative in POLICY_INPUTS}
    missing = sorted(path for path in policy_paths if not path.is_file())
    if missing:
        raise ValueError(f"missing Kani cache policy input: {missing[0]}")

    result: dict[str, str] = {}
    for name, package_id in selected.items():
        closure: set[str] = set()
        pending = [package_id]
        while pending:
            current = pending.pop()
            if current in closure:
                continue
            closure.add(current)
            pending.extend(dependency["pkg"] for dependency in nodes[current]["deps"])

        local_directories: set[Path] = set()
        for current in closure:
            package = packages[current]
            manifest = Path(package["manifest_path"]).resolve()
            if package.get("source") is None and manifest.is_relative_to(root):
                local_directories.add(manifest.parent)
        inputs = tracked_files(root, local_directories) | policy_paths

        digest = hashlib.sha256()
        digest.update(b"rustos-kani-package-cache-v1\0")
        digest.update(args.version.encode("utf-8"))
        digest.update(b"\0cargo-kani terse timeout=180 sanity-checks\0")
        for path in sorted(inputs):
            update_file(digest, root, path)
        result[name] = digest.hexdigest()

    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
