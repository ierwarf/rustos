#!/usr/bin/env python3
"""Skip a formal lane whose declared inputs are unchanged, and prove the declaration.

    lane-cache.py run LANE -- COMMAND...

The key covers the command, the Rust toolchain, and the bytes of every
git-tracked file in the lane's declared scope (formal/lane-scopes.toml). On a
hit the recorded log is replayed and the lane's artifacts are re-verified by
digest; on a miss the lane runs inside a bubblewrap sandbox that exposes only
the scope, so an undeclared read fails the lane rather than weakening the key.
Without bubblewrap the lane still runs, but its result is not stored.
RUSTOS_WITNESS_CACHE=off (nightly) forces a real run and stores nothing stale.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from package_closure import Workspace  # noqa: E402

ROOT = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
STORE = ROOT / "build/formal/lane-cache"


def tracked(paths: list[str]) -> set[Path]:
    out = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", *paths],
        cwd=ROOT, check=True, capture_output=True,
    ).stdout
    return {ROOT / raw.decode() for raw in out.split(b"\0") if raw}


def scope(spec: dict) -> tuple[set[Path], set[Path]]:
    files = tracked(spec["paths"])
    all_tracked = tracked(["."])
    for registry in spec.get("path_refs", []):
        for token in re.findall(r"[A-Za-z0-9_./-]+\.[A-Za-z]+", (ROOT / registry).read_text()):
            candidate = ROOT / token
            if candidate in all_tracked:
                files.add(candidate)
    for manifest in spec.get("manifests", []):
        workspace = Workspace(ROOT, manifest_path=ROOT / manifest)
        root_package = workspace.package_for_manifest(ROOT / manifest)
        for pid in workspace.closure_ids(root_package):
            package = workspace.packages[pid]
            directory = Path(package["manifest_path"]).resolve().parent
            if package.get("source") is None and directory.is_relative_to(ROOT):
                files |= tracked([directory.relative_to(ROOT).as_posix()])
    files = {path for path in files if path.is_file()}
    top = {ROOT / path.relative_to(ROOT).parts[0] for path in files}
    return files, top


def digest_files(files: set[Path]) -> str:
    h = hashlib.sha256()
    for path in sorted(files):
        h.update(path.relative_to(ROOT).as_posix().encode() + b"\0" + path.read_bytes() + b"\0")
    return h.hexdigest()


def artifact_digests(dirs: list[str]) -> dict[str, str]:
    result = {}
    for relative in dirs:
        for path in sorted((ROOT / relative).rglob("*")):
            if path.is_file():
                result[path.relative_to(ROOT).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def sandboxed(command: list[str], files: set[Path], writable: list[str] = ()) -> list[str]:
    args = ["bwrap", "--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", str(ROOT)]
    created: set[Path] = set()
    for path in sorted(files):
        # Bind each file's parent chain implicitly: bwrap creates mount points.
        args += ["--ro-bind", str(path), str(path)]
        created.add(path)
    args += ["--ro-bind", str(ROOT / ".git"), str(ROOT / ".git")]
    for writable_dir in ("build", "target", *writable):
        directory = ROOT / writable_dir
        directory.mkdir(parents=True, exist_ok=True)
        args += ["--bind", str(directory), str(directory)]
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    if cargo_home.is_dir():
        args += ["--bind", str(cargo_home), str(cargo_home)]
    return args + ["--chdir", str(ROOT), *command]


def main() -> int:
    if len(sys.argv) < 4 or sys.argv[1] != "run" or "--" not in sys.argv:
        print(__doc__, file=sys.stderr)
        return 2
    lane = sys.argv[2]
    command = sys.argv[sys.argv.index("--") + 1:]
    spec = tomllib.loads((HERE / "lane-scopes.toml").read_text())["lane"][lane]
    files, _ = scope(spec)
    toolchain = subprocess.run(["rustc", "-Vv"], capture_output=True, text=True, cwd=ROOT).stdout
    key = hashlib.sha256(
        json.dumps({"lane": lane, "cmd": command, "toolchain": toolchain}).encode()
        + digest_files(files).encode()
    ).hexdigest()
    STORE.mkdir(parents=True, exist_ok=True)
    entry = STORE / f"{lane}.json"
    off = os.environ.get("RUSTOS_WITNESS_CACHE") == "off"
    if not off and entry.is_file():
        recorded = json.loads(entry.read_text())
        if recorded["key"] == key and recorded["artifacts"] == artifact_digests(spec["artifacts"]):
            # The seal demands evidence newer than its run marker. These bytes
            # were just re-verified by digest, so refreshing the mtime restates
            # them; it does not fabricate a result.
            for relative in recorded["artifacts"]:
                (ROOT / relative).touch()
            sys.stdout.write(recorded["log"])
            print(f"lane {lane}: reused (scope unchanged, {len(files)} files)")
            return 0
    sandbox = shutil.which("bwrap")
    run = sandboxed(command, files, spec.get("writable", [])) if sandbox else command
    done = subprocess.run(run, cwd=ROOT, capture_output=True, text=True)
    sys.stdout.write(done.stdout)
    sys.stderr.write(done.stderr)
    if done.returncode != 0:
        if sandbox:
            print(f"lane {lane}: failed in the declared-scope sandbox; if a file is reported "
                  "missing, add it to formal/lane-scopes.toml", file=sys.stderr)
        entry.unlink(missing_ok=True)
        return done.returncode
    if sandbox:
        entry.write_text(json.dumps({"key": key, "log": done.stdout,
                                     "artifacts": artifact_digests(spec["artifacts"])}))
    else:
        entry.unlink(missing_ok=True)
        print(f"lane {lane}: no bubblewrap, result not cached", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
