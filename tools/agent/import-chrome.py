#!/usr/bin/env python3
"""Import a pinned official Chrome deb as a local, non-autostart RustOS app.

Never installs the package on the host or runs package scripts / executables.
Only opt/google/chrome's regular files are imported. Publication of the manifest
is last, so a failed first import cannot register an incomplete application.
"""
from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path, PurePosixPath
import shutil
import struct
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_PACKAGE = ROOT / "build/external-apps/chrome/google-chrome-stable_current_amd64.deb"
# Official google-chrome-stable 157.0.8103.38-1 (amd64), downloaded via HTTPS.
# This local pin is reproducibility evidence, not an independent vendor signature.
DEFAULT_SHA256 = "a62b277dc03da2cff50cc04589deb66f6875be339c066d46adb49b49eedb629d"
BUNDLE_PREFIX = ("opt", "google", "chrome")
MAX_FILES = 4096
MAX_BYTES = 1024 * 1024 * 1024


def check_digest(package: Path, expected: str) -> None:
    if len(expected) != 64 or any(c not in "0123456789abcdef" for c in expected):
        raise ValueError("expected SHA-256 must be 64 lowercase hexadecimal digits")
    with package.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual != expected:
        raise ValueError(f"Chrome package SHA-256 mismatch: {actual}")


def extract_bundle(stream, destination: Path) -> None:
    """Extract a closed, bounded regular-file tree; reject links/special files."""
    seen: set[PurePosixPath] = set()
    total = 0
    with tarfile.open(fileobj=stream, mode="r|*") as archive:
        for member in archive:
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError(f"unsafe package path: {member.name}")
            if path.parts[:3] != BUNDLE_PREFIX or len(path.parts) <= 3:
                continue
            relative = PurePosixPath(*path.parts[3:])
            target = destination.joinpath(*relative.parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            if not member.isfile():
                raise ValueError(f"non-regular Chrome bundle member: {member.name}")
            if relative in seen:
                raise ValueError(f"duplicate Chrome bundle member: {member.name}")
            seen.add(relative)
            total += member.size
            if member.size < 0 or len(seen) > MAX_FILES or total > MAX_BYTES:
                raise ValueError("Chrome bundle exceeds import budget")
            target.parent.mkdir(parents=True, exist_ok=True)
            source = archive.extractfile(member)
            if source is None:
                raise ValueError(f"missing package member bytes: {member.name}")
            with source, target.open("xb") as output:
                shutil.copyfileobj(source, output, 1024 * 1024)
            # Do not grant host setuid/setgid privileges to chrome-sandbox.
            target.chmod(member.mode & 0o777)
    executable = destination / "chrome"
    with executable.open("rb") as source:
        header = source.read(20)
    if (len(header) != 20 or header[:6] != b"\x7fELF\x02\x01"
            or struct.unpack_from("<H", header, 18)[0] != 62):
        raise ValueError("Chrome executable is not an x86-64 little-endian ELF")


def import_chrome(package: Path, expected: str, root: Path = ROOT) -> Path:
    check_digest(package, expected)
    app = root / "apps/chrome"
    template = (app / "RUSTOS.package.toml.example").read_bytes()
    bundle = app / "bundle"
    manifest = app / "RUSTOS.package.toml"
    # Never replace a prior imported or user-owned tree implicitly.
    if bundle.exists() or bundle.is_symlink() or manifest.exists() or manifest.is_symlink():
        raise ValueError("Chrome is already imported; existing bundle/manifest left untouched")
    with tempfile.TemporaryDirectory(prefix=".chrome-import-", dir=root / "build") as scratch:
        temporary = Path(scratch)
        unpacked = temporary / "bundle"
        unpacked.mkdir()
        with tempfile.TemporaryFile() as errors:
            process = subprocess.Popen(
                ["ar", "p", str(package), "data.tar.xz"],
                stdout=subprocess.PIPE, stderr=errors,
            )
            try:
                assert process.stdout is not None
                with process.stdout:
                    extract_bundle(process.stdout, unpacked)
                if process.wait() != 0:
                    errors.seek(0)
                    raise ValueError(f"deb payload extraction failed: {errors.read(2048)!r}")
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
        (unpacked / "RUSTOS-IMPORT.txt").write_text(
            f"source={package.name}\nsha256={expected}\n"
            "host_installed=false\nautostart=false\ncompatibility_verified=false\n"
            "setuid_setgid_bits_removed=true\n"
        )
        staged_manifest = temporary / "RUSTOS.package.toml"
        staged_manifest.write_bytes(template)
        # hard-link publication refuses an existing manifest, including symlinks.
        os.rename(unpacked, bundle)
        try:
            os.link(staged_manifest, manifest)
        except BaseException:
            shutil.rmtree(bundle)
            raise
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", nargs="?", type=Path, default=DEFAULT_PACKAGE)
    parser.add_argument("--sha256", default=DEFAULT_SHA256,
                        help="explicit reviewed digest for a different package version")
    args = parser.parse_args()
    try:
        manifest = import_chrome(args.package.resolve(), args.sha256)
    except (OSError, ValueError, tarfile.TarError) as error:
        parser.exit(1, f"import-chrome: {error}\n")
    print(f"import-chrome: registered {manifest}; startup=none (not executed)")
    print("Next: cargo xtask build (or build-user followed by stage)")


if __name__ == "__main__":
    main()
