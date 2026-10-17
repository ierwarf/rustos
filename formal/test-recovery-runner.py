#!/usr/bin/env python3
"""Regression checks for compile-once recovery witnesses and fail-closed execution."""

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

runner = Path(__file__).with_name("run-recovery-scenarios.py")
spec = importlib.util.spec_from_file_location("recovery_runner", runner)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class RecoveryRunnerTests(unittest.TestCase):
    def test_compile_artifacts_are_cargo_test_binaries_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "witness"
            binary.touch()
            scenario = {"id": "sample", "package": "vfsd", "features": "-", "target": "lib"}
            message = json.dumps({"reason": "compiler-artifact", "profile": {"test": True},
                                  "executable": str(binary)})
            with patch.object(module.subprocess, "run", return_value=subprocess.CompletedProcess(
                [], 0, message + "\n", ""
            )) as run:
                self.assertEqual(module.build_witnesses(root, scenario), [binary])
                self.assertEqual(run.call_args.args[0][-2:], ["--no-run", "--message-format=json"])
            with patch.object(module.subprocess, "run", return_value=subprocess.CompletedProcess(
                [], 0, json.dumps({"reason": "build-finished", "success": True}) + "\n", ""
            )):
                with self.assertRaisesRegex(SystemExit, "no complete Cargo test artifact"):
                    module.build_witnesses(root, scenario)

    def test_zero_witness_and_timeout_do_not_count_as_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scenario = {"id": "sample", "test": "missing", "max_ms": "30000"}
            with patch.object(module.subprocess, "run", return_value=subprocess.CompletedProcess(
                [], 0, "test result: ok. 0 passed; 0 failed;\n", ""
            )):
                with self.assertRaisesRegex(SystemExit, "no witness"):
                    module.run_scenario(root, root, scenario, [root / "witness"])
            with patch.object(module.subprocess, "run", side_effect=subprocess.TimeoutExpired(
                ["witness"], 30
            )):
                with self.assertRaisesRegex(SystemExit, "timed out"):
                    module.run_scenario(root, root, scenario, [root / "witness"])


if __name__ == "__main__":
    unittest.main()
