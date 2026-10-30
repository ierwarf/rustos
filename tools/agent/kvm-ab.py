#!/usr/bin/env python3
"""Interleaved A/B KVM boot comparison: the working tree against a committed ref.

A boot-failure rate near 4% cannot be compared across hours: host load drifts, and a
`git stash` control makes the working tree unusable while it runs. This tool builds the
control ref in its own `git worktree`, then alternates one boot of the treatment (this
checkout) with one boot of the control, so both arms see the same host conditions, and
reports pass counts, the failure kinds, and a two-sided Fisher exact p-value.

  tools/agent/kvm-ab.py --pairs 40                 # HEAD as control, 16 vCPU
  tools/agent/kvm-ab.py --pairs 20 --vcpus 1       # no seal needed for 1 vCPU
  tools/agent/kvm-ab.py --control abc1234 --pairs 40 --expect "ipcbench: end"
  tools/agent/kvm-ab.py --selftest                 # check the statistics only

Preconditions it checks rather than assumes:
  * a multi-vCPU boot needs a sealed formal run on each tree. The treatment must already
    be sealed (`formal/verify-all.sh --profile pr`); the control is sealed here unless
    `--no-seal-control` is given.
  * the control worktree is created under build/ab/ and reused between runs; remove it
    with `git worktree remove --force build/ab/control` when done.
The run is read-only for the treatment tree: it only invokes `cargo xtask kvm-smoke
--no-build --no-auto-verify`, and builds the control in the control worktree.
"""
import argparse
import math
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONTROL_DIR = ROOT / "build" / "ab" / "control"


def fisher_two_sided(a: int, b: int, c: int, d: int) -> float:
    """Two-sided Fisher exact p for the table [[a, b], [c, d]] (passes/fails per arm)."""
    row1, row2, col1 = a + b, c + d, a + c
    total = row1 + row2

    def prob(x: int) -> float:
        return math.comb(row1, x) * math.comb(row2, col1 - x) / math.comb(total, col1)

    observed = prob(a)
    low, high = max(0, col1 - row2), min(row1, col1)
    return min(1.0, sum(p for p in (prob(x) for x in range(low, high + 1)) if p <= observed * (1 + 1e-9)))


def selftest() -> int:
    cases = [
        ((5, 5, 5, 5), 1.0),
        ((0, 10, 10, 0), 1.0842e-05),
        ((38, 2, 40, 0), 0.4936),  # 2/40 failing against none
    ]
    ok = True
    for table, expected in cases:
        got = fisher_two_sided(*table)
        good = abs(got - expected) < max(1e-3, expected * 0.02)
        ok &= good
        print(f"{'ok' if good else 'not ok'} - fisher{table} = {got:.4g} (expected ~{expected})")
    return 0 if ok else 1


def run(cmd, cwd, check=True, timeout=None, capture=False):
    return subprocess.run(cmd, cwd=cwd, check=check, timeout=timeout, text=True,
                          capture_output=capture)


def provision_control_artifacts() -> None:
    """Copy what a fresh worktree lacks and a boot needs (all of it gitignored).

    * The Linux DVM artifacts: `kvm-smoke` exits at once without the DVM manifest, and
      rebuilding the DVM is never the answer. Only `out/artifacts` (about 150 MB) is
      needed; the rest of `out/` is tens of GB of intermediates. It must be a real
      directory: the seal hashes `git ls-files --others`, and `.gitignore`'s `out/`
      does not match a symlink, which then breaks the hash walk.
    * `build/formal/tools`, which the concurrency-triangle lane of the seal needs for
      its pinned herd7.
    """
    pairs = [
        (ROOT / "driver-domains/linux/out/artifacts", CONTROL_DIR / "driver-domains/linux/out/artifacts"),
        (ROOT / "build/formal/tools", CONTROL_DIR / "build/formal/tools"),
    ]
    for source, target in pairs:
        if target.exists():
            continue
        if not source.is_dir():
            print(f"warning: {source} is missing; the control boot may fail", file=sys.stderr)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(source, target, symlinks=True)
        print(f"provisioned {target.relative_to(ROOT)}", flush=True)
    # `xtask kvm` binds a Unix socket under build/kvm; past ~108 bytes it cannot.
    longest = len(str(CONTROL_DIR / "build/kvm/dvm-input-doorbell.sock"))
    if longest > 100:
        sys.exit(f"control path too long for the KVM sockets ({longest} bytes): shorten the checkout path")


def ensure_control(ref: str, seal: bool, vcpus: int, build: bool) -> None:
    if not CONTROL_DIR.exists():
        CONTROL_DIR.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "worktree", "add", "--detach", str(CONTROL_DIR), ref], ROOT)
    else:
        head = run(["git", "rev-parse", "HEAD"], CONTROL_DIR, capture=True).stdout.strip()
        want = run(["git", "rev-parse", ref], ROOT, capture=True).stdout.strip()
        if head != want:
            run(["git", "checkout", "--detach", want], CONTROL_DIR)
    provision_control_artifacts()
    if build:
        print("building control (first build of a worktree is a full compile)...", flush=True)
        # The first build in a fresh worktree can report a winsys export contract
        # mismatch and still exit 0; the second immediate build passes.
        run(["cargo", "xtask", "build"], CONTROL_DIR)
        run(["cargo", "xtask", "build"], CONTROL_DIR)
    if seal and vcpus > 1:
        print("sealing control...", flush=True)
        run(["bash", "formal/verify-all.sh", "--profile", "pr"], CONTROL_DIR)


def one_boot(cwd: Path, vcpus: int, expect: str, timeout: int, extra: list) -> tuple:
    cmd = ["cargo", "xtask", "kvm-smoke", "--rustos-vcpus", str(vcpus), "--expect", expect,
           "--timeout", str(timeout), "--no-build", "--no-auto-verify", *extra]
    started = time.time()
    result = run(cmd, cwd, check=False, capture=True, timeout=timeout * 6 + 120)
    text = result.stdout + result.stderr
    if result.returncode == 0:
        return True, ""
    missing = re.search(r"missing=\[(.*?)\];", text)
    kind = missing.group(1)[:120] if missing else (text.strip().splitlines() or ["no output"])[-1][:120]
    return False, kind or f"rc={result.returncode} after {time.time() - started:.0f}s"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--control", default="HEAD", help="committed ref for the control arm")
    ap.add_argument("--pairs", type=int, default=20)
    ap.add_argument("--vcpus", type=int, default=16, choices=[1, 2, 4, 8, 16])
    ap.add_argument("--timeout", type=int, default=20)
    ap.add_argument("--expect", default="ipcbench: end")
    ap.add_argument("--smoke-arg", action="append", default=["--gui-dvm-surfaces", "--dvm-block-shmem"],
                    help="extra kvm-smoke argument (repeatable)")
    ap.add_argument("--no-seal-control", action="store_true")
    ap.add_argument("--no-build-control", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    if args.selftest:
        return selftest()
    if args.dry_run:
        print(f"treatment: {ROOT}\ncontrol:   {args.control} in {CONTROL_DIR}")
        print(f"{args.pairs} interleaved pairs, {args.vcpus} vCPU, expect {args.expect!r}, timeout {args.timeout}s")
        return 0

    ensure_control(args.control, not args.no_seal_control, args.vcpus, not args.no_build_control)
    stats = {"treatment": [0, 0, []], "control": [0, 0, []]}
    arms = [("treatment", ROOT), ("control", CONTROL_DIR)]
    for pair in range(1, args.pairs + 1):
        # Alternate which arm goes first so a slow first boot of a pair is not one arm's.
        order = arms if pair % 2 else arms[::-1]
        for name, cwd in order:
            passed, kind = one_boot(cwd, args.vcpus, args.expect, args.timeout, args.smoke_arg)
            stats[name][0 if passed else 1] += 1
            if not passed:
                stats[name][2].append(kind)
            print(f"pair {pair:>3} {name:<9} {'pass' if passed else 'FAIL ' + kind}", flush=True)

    tp, tf, _ = stats["treatment"]
    cp, cf, _ = stats["control"]
    print(f"\ntreatment {tp}/{tp + tf} passed, control {cp}/{cp + cf} passed")
    for name in ("treatment", "control"):
        kinds = {}
        for kind in stats[name][2]:
            kinds[kind] = kinds.get(kind, 0) + 1
        for kind, count in sorted(kinds.items(), key=lambda item: -item[1]):
            print(f"  {name} failure x{count}: {kind}")
    print(f"two-sided Fisher exact p = {fisher_two_sided(tp, tf, cp, cf):.3g} "
          "(small p means the arms differ; a large p means this sample cannot tell)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
