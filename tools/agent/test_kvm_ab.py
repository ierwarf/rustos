#!/usr/bin/env python3
"""Host-only tests for bounded A/B subprocess cleanup."""
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import MagicMock, patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("kvm_ab", Path(__file__).with_name("kvm-ab.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class RunTests(unittest.TestCase):
    def test_timeout_kills_process_group_and_reaps(self):
        process = MagicMock(pid=123)
        process.communicate.side_effect = [subprocess.TimeoutExpired(["cargo"], 5), ("", "")]
        with patch.object(runner.subprocess, "Popen") as popen, \
             patch.object(runner.os, "killpg") as killpg:
            popen.return_value.__enter__.return_value = process
            with self.assertRaises(subprocess.TimeoutExpired):
                runner.run(["cargo"], Path("."), timeout=5, capture=True)
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        killpg.assert_called_once_with(123, signal.SIGKILL)
        self.assertEqual(process.communicate.call_count, 2)

    @unittest.skipUnless(sys.platform.startswith("linux"), "requires Linux /proc")
    def test_timeout_stops_real_child_and_grandchild(self):
        # The intermediate process and its child stay in the new session,
        # just as cargo, xtask and QEMU normally do.
        with tempfile.TemporaryDirectory() as directory:
            pidfile = Path(directory) / "pids"
            script = (
                "import os, subprocess, sys, time; "
                "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)']); "
                "open(sys.argv[1], 'w').write(f'{os.getpid()} {child.pid}'); "
                "time.sleep(60)"
            )
            try:
                with self.assertRaises(subprocess.TimeoutExpired):
                    runner.run([sys.executable, "-c", script, str(pidfile)],
                               Path(directory), timeout=3, capture=True)
                pids = [int(pid) for pid in pidfile.read_text().split()]
                self.assertEqual(len(pids), 2)
                deadline = time.monotonic() + 3
                while True:
                    # A killed orphan may briefly remain as a zombie until its
                    # adopter reaps it; it no longer executes or holds resources.
                    alive = []
                    for pid in pids:
                        try:
                            state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
                        except FileNotFoundError:
                            continue
                        if state != "Z":
                            alive.append(pid)
                    if not alive or time.monotonic() >= deadline:
                        break
                    time.sleep(0.05)
                self.assertEqual(alive, [])
            finally:
                # Bound damage if a regression leaves the group alive.
                if pidfile.exists():
                    for pid in pidfile.read_text().split():
                        try:
                            os.kill(int(pid), signal.SIGKILL)
                        except ProcessLookupError:
                            pass

    def test_bounded_success_propagates_output_and_failure(self):
        process = MagicMock(returncode=1)
        process.communicate.return_value = ("out", "err")
        with patch.object(runner.subprocess, "Popen") as popen:
            popen.return_value.__enter__.return_value = process
            result = runner.run(["cargo"], Path("."), check=False, timeout=5, capture=True)
            self.assertEqual((result.returncode, result.stdout, result.stderr), (1, "out", "err"))
            with self.assertRaises(subprocess.CalledProcessError):
                runner.run(["cargo"], Path("."), timeout=5, capture=True)


if __name__ == "__main__":
    unittest.main()
