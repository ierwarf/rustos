#!/usr/bin/env python3
"""Import the pinned Ubuntu Noble GNU bc binary as a local non-autostart RustOS app."""
from __future__ import annotations

import hashlib
import os
from pathlib import Path, PurePosixPath
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/xtask/src/stage"))
import noble_runtime  # noqa: E402

NOBLE_PATH = "pool/main/b/bc/bc_1.07.1-3ubuntu4_amd64.deb"
DEFAULT_SHA256 = "cb85d5929476088533b3c5e1db2de5ab4593a69fa54243710f762197dbaea60d"
DEFAULT_SIZE = 85264
TARGET_MEMBER = PurePosixPath("usr/bin/bc")


def extract_bc_binary(deb_path: Path, destination: Path) -> None:
    """Extract only usr/bin/bc from data.tar.* and verify x86-64 ELF header."""
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(
            ["ar", "p", str(deb_path), "data.tar.zst"],
            stdout=subprocess.PIPE,
            stderr=errors,
        )
        zstd_proc = subprocess.Popen(
            ["zstd", "-d", "-q"],
            stdin=process.stdout,
            stdout=subprocess.PIPE,
            stderr=errors,
        )
        assert process.stdout is not None
        process.stdout.close()
        found = False
        try:
            assert zstd_proc.stdout is not None
            with zstd_proc.stdout, tarfile.open(fileobj=zstd_proc.stdout, mode="r|") as archive:
                for member in archive:
                    clean = PurePosixPath(member.name.removeprefix("./"))
                    if clean != TARGET_MEMBER:
                        continue
                    if not member.isfile():
                        raise ValueError(f"non-regular bc member: {member.name}")
                    source = archive.extractfile(member)
                    if source is None:
                        raise ValueError("missing bc binary bytes")
                    target = destination / "bc"
                    with source, target.open("xb") as output:
                        shutil.copyfileobj(source, output, 256 * 1024)
                    target.chmod(0o755)
                    found = True
                    break
            if zstd_proc.wait() != 0 and not found:
                errors.seek(0)
                raise ValueError(f"zstd decompression failed: {errors.read(2048)!r}")
            if process.wait() != 0 and not found:
                errors.seek(0)
                raise ValueError(f"ar extraction failed: {errors.read(2048)!r}")
        finally:
            for proc in (zstd_proc, process):
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()
    if not found:
        raise ValueError(f"{TARGET_MEMBER} not found in {deb_path}")

    executable = destination / "bc"
    header = executable.read_bytes()[:20]
    if (
        len(header) != 20
        or header[:6] != b"\x7fELF\x02\x01"
        or struct.unpack_from("<H", header, 18)[0] != 62
    ):
        raise ValueError("bc executable is not an x86-64 little-endian ELF")


def import_bc(root: Path = ROOT) -> Path:
    app = root / "apps/bc"
    bundle = app / "bundle"
    manifest = app / "RUSTOS.package.toml"
    if bundle.exists() or bundle.is_symlink() or manifest.exists() or manifest.is_symlink():
        raise ValueError("bc is already imported; existing bundle/manifest left untouched")

    cache = root / "build/runtime-libs/noble/cache"
    cache.mkdir(parents=True, exist_ok=True)
    deb_bytes = noble_runtime.fetch(cache, NOBLE_PATH, DEFAULT_SHA256, DEFAULT_SIZE)
    actual = hashlib.sha256(deb_bytes).hexdigest()
    if actual != DEFAULT_SHA256:
        raise ValueError(f"bc package SHA-256 mismatch: {actual}")

    template = (app / "RUSTOS.package.toml.example").read_bytes()
    with tempfile.TemporaryDirectory(prefix=".bc-import-", dir=root / "build") as scratch:
        temporary = Path(scratch)
        deb_file = temporary / "bc.deb"
        deb_file.write_bytes(deb_bytes)
        unpacked = temporary / "bundle"
        unpacked.mkdir()
        extract_bc_binary(deb_file, unpacked)
        (unpacked / "RUSTOS-IMPORT.txt").write_text(
            f"source={Path(NOBLE_PATH).name}\nsha256={DEFAULT_SHA256}\n"
            "host_installed=false\nautostart=false\n"
        )
        staged_manifest = temporary / "RUSTOS.package.toml"
        staged_manifest.write_bytes(template)
        os.rename(unpacked, bundle)
        os.link(staged_manifest, manifest)
    return bundle


if __name__ == "__main__":
    import_bc()
    print("Imported GNU bc into apps/bc/bundle")
