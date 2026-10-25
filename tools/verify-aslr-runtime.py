#!/usr/bin/env python3
"""Check private ASLR witnesses from >=3 successful, independently booted guests.

Run three bounded RUSTOS_LIFECYCLE_TRACE=true cargo xtask kvm-smoke boots,
saving build/kvm/rustos-debugcon.log after EACH successful boot, then pass those
three distinct saved logs here. --repeat does not archive successful logs.
KVM acceptance is a separate prerequisite: address diagnostics alone do not
prove a healthy boot.
"""
import argparse
import json
from pathlib import Path
import re
import unittest

KERNEL = re.compile(r"aslr: kernel base=(0x[0-9a-f]+) pc=(0x[0-9a-f]+)")
PROCESS = re.compile(r"aslr: process path=(\S+) entry=(0x[0-9a-f]+) stack=(0x[0-9a-f]+) pc=(0x[0-9a-f]+)")
REQUIRED = ("rootd", "syscalld", "vfsd", "loaderd", "procd", "pagerd", "initd")
USER_START, USER_END = 1 << 39, 2 << 39


def verify(logs):
    if len(logs) < 3:
        raise ValueError("at least three independent boot logs are required")
    bases, offsets = set(), set()
    entries = {name: set() for name in REQUIRED}
    stacks = {name: set() for name in REQUIRED}
    initial_pcs = {name: set() for name in REQUIRED}
    for index, text in enumerate(logs, 1):
        kernel = KERNEL.findall(text)
        if len(kernel) != 1:
            raise ValueError(f"boot {index}: expected exactly one executing-kernel witness")
        base, pc = (int(value, 16) for value in kernel[0])
        if not (0xFFFF810000000000 <= base < 0xFFFF818000000000
                and base % (1 << 21) == 0 and 0 <= pc - base < (1 << 28)):
            raise ValueError(f"boot {index}: invalid kernel image placement/PC")
        bases.add(base)
        offsets.add(pc - base)
        seen = set()
        for path, entry, stack, pc in PROCESS.findall(text):
            name = Path(path).stem
            if name not in entries:
                continue
            if name in seen:
                raise ValueError(f"boot {index}: ambiguous repeated initial witness for {name}")
            entry, stack, pc = int(entry, 16), int(stack, 16), int(pc, 16)
            if not (USER_START <= entry < USER_END and USER_START <= pc < USER_END):
                raise ValueError(f"boot {index}: {name}: entry outside process window")
            if not (USER_END - (1 << 30) - (1 << 20) < stack < USER_END
                    and stack % 16 == 0):
                raise ValueError(f"boot {index}: {name}: invalid randomized initial RSP")
            entries[name].add(entry)
            stacks[name].add(stack)
            initial_pcs[name].add(pc)
            seen.add(name)
        missing = set(REQUIRED) - seen
        if missing:
            raise ValueError(f"boot {index}: missing process witnesses: {sorted(missing)}")
    if len(bases) < 2 or len(offsets) != 1:
        raise ValueError("kernel PC must move with the randomized base, at a stable image offset")
    for name in REQUIRED:
        if min(len(entries[name]), len(stacks[name]), len(initial_pcs[name])) < 2:
            raise ValueError(f"{name}: image entry and initial RSP must both vary between boots")
    return {"boots": len(logs), "kernel_bases": len(bases),
            "processes": {name: {"entries": len(entries[name]), "stacks": len(stacks[name]),
                                  "initial_pcs": len(initial_pcs[name])}
                          for name in REQUIRED}}


class Tests(unittest.TestCase):
    def logs(self):
        result = []
        for boot in range(3):
            base = 0xFFFF810000200000 + boot * (1 << 21)
            rows = [f"aslr: kernel base={base:#x} pc={base + 4096:#x}"]
            for i, name in enumerate(REQUIRED):
                entry = USER_START + (boot + 1) * (1 << 21) + i * 4096
                stack = USER_END - (boot + 1) * 4096 - i * 16
                rows.append(f"aslr: process path=services/{name}/{name}.elf entry={entry:#x} stack={stack:#x} pc={entry + 16:#x}")
            result.append("\n".join(rows))
        return result

    def test_accepts_independent_image_stack_and_kernel_movement(self):
        self.assertEqual(verify(self.logs())["boots"], 3)

    def test_rejects_fixed_layout_missing_witness_and_wrong_pc(self):
        logs = self.logs()
        for bad in ([logs[0]] * 3, logs[:2], ["", *logs[1:]],
                    [logs[0].replace("pc=0xffff810000201000", "pc=0x201000"), *logs[1:]]):
            with self.assertRaises(ValueError):
                verify(bad)

    def test_rejects_identical_restart_sequences_in_every_boot(self):
        logs = self.logs()
        sequence = "\n".join(logs[0].splitlines()[1:] + logs[1].splitlines()[1:])
        repeated = [text.splitlines()[0] + "\n" + sequence for text in logs]
        with self.assertRaises(ValueError):
            verify(repeated)

    def test_rejects_fixed_process_entry_or_stack_independently(self):
        for field in ("entry", "stack", "pc"):
            logs = self.logs()
            pattern = re.compile(rf"(path=services/rootd/rootd.elf[^\n]*?{field}=)(0x[0-9a-f]+)")
            fixed = pattern.search(logs[0])[2]
            logs = [pattern.sub(lambda match: match[1] + fixed, text) for text in logs]
            with self.assertRaises(ValueError):
                verify(logs)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("logs", nargs="*", type=Path)
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[__file__])
    else:
        try:
            if len({path.resolve() for path in args.logs}) != len(args.logs):
                raise ValueError("duplicate log paths are not independent boots")
            print(json.dumps(verify([path.read_text() for path in args.logs]), sort_keys=True))
        except (OSError, ValueError) as error:
            parser.exit(1, f"ASLR verification failed: {error}\n")
