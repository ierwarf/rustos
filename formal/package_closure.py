"""Content digest of a Cargo package's local dependency closure.

The standard way to cache a build or test result is to key it on exactly the
inputs the task can observe (Bazel's action key, Nx's task hash, cargo-nextest
and Cargo's own fingerprints): the task's own sources plus the transitive
closure of what it depends on, and nothing else. A change outside the closure
cannot change the answer, so it must not invalidate it. This module computes
that key for a workspace package. It is shared by every formal lane cache so
the closure rule exists once.

Local crates contribute their tracked file bytes. Registry and git crates
contribute their pinned identity (name, version, source and the lockfile
checksum), because their bytes are fixed by it.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
import tomllib
from pathlib import Path

POLICY_INPUTS = (
    "Cargo.toml",
    ".cargo/config.toml",
    "rust-toolchain.toml",
)


def _git_files(root: Path, directories: set[Path]) -> set[Path]:
    if not directories:
        return set()
    output = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--"]
        + sorted(d.relative_to(root).as_posix() for d in directories),
        cwd=root,
        check=True,
        capture_output=True,
    ).stdout
    return {root / raw.decode("utf-8") for raw in output.split(b"\0") if raw}


class Workspace:
    def __init__(self, root: Path, manifest_path: Path | None = None) -> None:
        self.root = root.resolve()
        extra = ["--manifest-path", str(manifest_path)] if manifest_path else []
        metadata = json.loads(
            subprocess.run(
                ["cargo", "metadata", "--format-version", "1", "--locked", *extra],
                cwd=self.root,
                check=True,
                capture_output=True,
            ).stdout
        )
        self.packages = {p["id"]: p for p in metadata["packages"]}
        self.nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
        members = set(metadata["workspace_members"])
        self.by_name = {
            self.packages[i]["name"]: i for i in members
        }
        lock = tomllib.loads((self.root / "Cargo.lock").read_text(encoding="utf-8"))
        self.checksums = {
            (p["name"], p["version"]): p.get("checksum", "") for p in lock.get("package", [])
        }

    def package_for_manifest(self, manifest: Path) -> str:
        for pid, package in self.packages.items():
            if Path(package["manifest_path"]).resolve() == manifest.resolve():
                return pid
        raise ValueError(f"no package for {manifest}")

    def closure_ids(self, package_id: str) -> set[str]:
        seen: set[str] = set()
        pending = [package_id]
        while pending:
            current = pending.pop()
            if current in seen:
                continue
            seen.add(current)
            pending.extend(d["pkg"] for d in self.nodes[current]["deps"])
        return seen

    def closure(self, name: str) -> set[str]:
        if name not in self.by_name:
            raise ValueError(f"package {name!r} is not a workspace member")
        seen: set[str] = set()
        pending = [self.by_name[name]]
        while pending:
            current = pending.pop()
            if current in seen:
                continue
            seen.add(current)
            pending.extend(d["pkg"] for d in self.nodes[current]["deps"])
        return seen

    def digest(self, names: list[str], extra_inputs: list[str], salt: str) -> str:
        closure: set[str] = set()
        for name in names:
            closure |= self.closure(name)
        local_dirs: set[Path] = set()
        external: list[str] = []
        for pid in closure:
            package = self.packages[pid]
            manifest = Path(package["manifest_path"]).resolve()
            if package.get("source") is None and manifest.is_relative_to(self.root):
                local_dirs.add(manifest.parent)
            else:
                external.append(
                    f"{package['name']} {package['version']} {package.get('source')} "
                    f"{self.checksums.get((package['name'], package['version']), '')}"
                )
        files = _git_files(self.root, local_dirs)
        for relative in (*POLICY_INPUTS, *extra_inputs):
            path = self.root / relative
            if path.is_file():
                files.add(path)
        digest = hashlib.sha256()
        digest.update(b"rustos-witness-cache-v1\0")
        digest.update(salt.encode("utf-8"))
        digest.update(b"\0")
        for line in sorted(external):
            digest.update(line.encode("utf-8") + b"\0")
        for path in sorted(files):
            if not path.is_file():
                continue
            digest.update(path.relative_to(self.root).as_posix().encode("utf-8") + b"\0")
            digest.update(path.read_bytes() + b"\0")
        return digest.hexdigest()
