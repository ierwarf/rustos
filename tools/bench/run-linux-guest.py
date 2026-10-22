#!/usr/bin/env python3
"""Run the exact RustOS ipcbench ELF in a minimal same-host Linux KVM guest.

No rootfs/DVM artifact is rewritten. The private archive contains the ELF,
resolved host glibc libraries, minimal static PID1, metadata and complete log.
This runner collects evidence; xtask validates the semantic result transcript.
"""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def archive(entries):
    """Deterministic SVR4 newc, including an actual /dev/console device node."""
    output = bytearray()
    for ino, (name, mode, data, major, minor) in enumerate(entries + [
            ("TRAILER!!!", stat.S_IFREG, b"", 0, 0)], 1):
        encoded = name.encode() + b"\0"
        fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(encoded), 0]
        output.extend(b"070701" + "".join(f"{value:08x}" for value in fields).encode())
        output.extend(encoded)
        output.extend(b"\0" * (-len(output) % 4))
        output.extend(data)
        output.extend(b"\0" * (-len(output) % 4))
    return bytes(output)


def run(args):
    repo = Path(__file__).resolve().parents[2]
    kernel = repo / "driver-domains/linux/out/artifacts/rustos-linux-dvm-x86_64.bzImage"
    compiler = repo / "driver-domains/linux/out/buildroot-output/host/bin/x86_64-buildroot-linux-musl-cc"
    binary = args.binary.resolve(strict=True)
    kernel.resolve(strict=True)
    args.output.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix=f"linux-vcpu-{args.vcpus}-{args.suite}-", dir=args.output.resolve()))
    init = directory / "init"
    archived_kernel = directory / "linux.bzImage"
    archived_kernel.write_bytes(kernel.read_bytes())
    binary_bytes = binary.read_bytes()
    subprocess.run([str(compiler), "-static", "-Os", "-Wall", "-Wextra", "-Werror",
                    str(repo / "tools/bench/linux_init.c"), "-o", str(init)], check=True)
    needed = subprocess.run(["ldd", str(binary)], check=True, text=True, capture_output=True).stdout
    libraries = {}
    for line in needed.splitlines():
        match = re.search(r"(?:=>\s+)?(/[^\s]+)", line)
        if match:
            path = Path(match.group(1))
            libraries[str(path)] = path.resolve(strict=True)
        elif "not found" in line:
            raise RuntimeError(f"unresolved library: {line}")
    # DT_NEEDED may name the loader as a library as well as PT_INTERP.
    loader = Path("/lib64/ld-linux-x86-64.so.2")
    libraries[str(loader)] = loader.resolve(strict=True)
    entries = []
    directories = {"dev", "proc", "tmp"}
    for name in libraries:
        parent = Path(name).parent
        while str(parent) != "/":
            directories.add(str(parent).lstrip("/"))
            parent = parent.parent
    for name in sorted(directories, key=lambda name: (name.count("/"), name)):
        entries.append((name, stat.S_IFDIR | (0o1777 if name == "tmp" else 0o755), b"", 0, 0))
    entries.extend([
        ("dev/console", stat.S_IFCHR | 0o600, b"", 5, 1),
        ("init", stat.S_IFREG | 0o755, init.read_bytes(), 0, 0),
        ("ipcbench", stat.S_IFREG | 0o755, binary_bytes, 0, 0),
        ("suite", stat.S_IFREG | 0o644, args.suite.encode() + b"\n", 0, 0),
    ])
    library_bytes = {name: source.read_bytes() for name, source in libraries.items()}
    for name, payload in sorted(library_bytes.items()):
        entries.append((name.lstrip("/"), stat.S_IFREG | 0o755, payload, 0, 0))
    initramfs = directory / "initramfs.cpio.gz"
    initramfs.write_bytes(gzip.compress(archive(entries), mtime=0))
    command = ["qemu-system-x86_64", "-machine", "q35,accel=kvm,hpet=on",
               "-cpu", "host,+x2apic,+invtsc", "-smp", str(args.vcpus),
               "-m", "2048M,maxmem=3G,slots=2", "-nodefaults", "-display", "none", "-monitor", "none",
               "-serial", "stdio", "-no-reboot", "-kernel", str(archived_kernel), "-initrd", str(initramfs),
               "-append", "console=ttyS0 rdinit=/init panic=-1 quiet"]
    metadata = {"v": 1, "suite": args.suite, "vcpus": args.vcpus, "cpu": "host,+x2apic,+invtsc", "memory_mib": 2048,
                "kernel_sha256": digest(archived_kernel), "ipcbench_sha256": hashlib.sha256(binary_bytes).hexdigest(),
                "init_sha256": digest(init), "initramfs_sha256": digest(initramfs),
                "libraries": {name: hashlib.sha256(payload).hexdigest() for name, payload in library_bytes.items()},
                "command": command, "host_uname": list(os.uname()),
                "host_boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip()}
    # Copy the actual app rather than relying only on a mutable build path.
    (directory / "ipcbench.elf").write_bytes(binary_bytes)
    log = directory / "guest.log"
    try:
        with log.open("wb") as output:
            result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, timeout=120)
        metadata["exit_code"] = result.returncode
    except subprocess.TimeoutExpired:
        metadata["timeout"] = True
    (directory / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"linux-baseline archive={directory}")
    transcript = log.read_text(errors="replace")
    if metadata.get("exit_code") != 0 or "linux-baseline: child-complete v=1" not in transcript:
        raise RuntimeError(f"Linux guest failed; inspect {log}")
    print(f"linux-baseline log={log}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--vcpus", type=int, choices=(1, 8), required=True)
    parser.add_argument("--suite", choices=("core_syscalls", "core_memory", "core_data", "core_state", "core_lifecycle", "core_scheduler"), required=True)
    run(parser.parse_args())
