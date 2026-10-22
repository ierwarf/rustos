#!/usr/bin/env python3
"""Focused, host-only tests for the baseline initramfs encoder (no KVM/build)."""
import importlib.util
from pathlib import Path
import stat
import sys
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("linux_guest", Path(__file__).with_name("run-linux-guest.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def decode(blob):
    offset, entries = 0, []
    while offset < len(blob):
        if blob[offset:offset + 6] != b"070701":
            raise ValueError("newc alignment/magic")
        fields = [int(blob[offset + 6 + i * 8:offset + 14 + i * 8], 16) for i in range(13)]
        offset += 110
        name = blob[offset:offset + fields[11] - 1].decode()
        offset = (offset + fields[11] + 3) & ~3
        data = blob[offset:offset + fields[6]]
        offset = (offset + fields[6] + 3) & ~3
        entries.append((name, fields, data))
    return entries


class ArchiveTests(unittest.TestCase):
    def test_console_has_device_numbers_and_each_payload_is_aligned(self):
        entries = decode(runner.archive([
            ("dev", stat.S_IFDIR | 0o755, b"", 0, 0),
            ("dev/console", stat.S_IFCHR | 0o600, b"", 5, 1),
            ("sample", stat.S_IFREG | 0o644, b"abc", 0, 0),
        ]))
        self.assertEqual(entries[1][1][9:11], [5, 1])
        self.assertEqual(entries[1][1][1], stat.S_IFCHR | 0o600)
        self.assertEqual(entries[2][2], b"abc")
        self.assertEqual(entries[-1][0], "TRAILER!!!")

    def test_encoder_is_deterministic_and_preserves_empty_files(self):
        data = [("empty", stat.S_IFREG | 0o600, b"", 0, 0)]
        self.assertEqual(runner.archive(data), runner.archive(data))
        entries = decode(runner.archive(data))
        self.assertEqual(entries[0][1][6], 0)
        self.assertEqual(entries[0][1][5], 0)  # no wall-clock mtime
        self.assertEqual(entries[0][2], b"")


if __name__ == "__main__":
    unittest.main()
