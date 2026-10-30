#!/usr/bin/env python3
"""Regression witnesses for checkout-relative dead-test discovery."""
from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    "dead_test_check", Path(__file__).with_name("check-dead-tests.py")
)
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class CheckoutRelativeDiscovery(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.original_root, self.original_debt = CHECK.ROOT, CHECK.DEBT
        self.addCleanup(self.restore)

    def restore(self):
        CHECK.ROOT, CHECK.DEBT = self.original_root, self.original_debt

    def checkout(self, ancestor):
        root = Path(self.temporary.name) / ancestor / "worktree"
        (root / "formal").mkdir(parents=True)
        debt = root / "formal/dead-test-debt.tsv"
        debt.write_text("")
        CHECK.ROOT, CHECK.DEBT = root, debt
        return root, debt

    def binary(self, root, relative):
        package = root / relative
        (package / "src").mkdir(parents=True)
        (package / "Cargo.toml").write_text(
            '[[bin]]\nname = "fixture"\npath = "src/main.rs"\ntest = false\n'
        )
        (package / "src/main.rs").write_text("#[test]\nfn dead() {}\n")

    def run_check(self):
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            status = CHECK.main()
        return status, stdout.getvalue(), stderr.getvalue()

    def test_registered_binary_is_found_under_build_or_target_ancestor(self):
        for ancestor in ("build", "target"):
            with self.subTest(ancestor=ancestor):
                root, debt = self.checkout(ancestor)
                self.binary(root, "services/fixture")
                debt.write_text("services/fixture/src/main.rs\t1\tfixture\n")
                status, stdout, stderr = self.run_check()
                self.assertEqual(status, 0, stderr)
                self.assertIn("1 registered binaries", stdout)

    def test_unknown_dead_test_is_not_hidden_by_build_ancestor(self):
        root, _ = self.checkout("build")
        self.binary(root, "services/fixture")
        status, _, stderr = self.run_check()
        self.assertEqual(status, 1)
        self.assertIn("services/fixture/src/main.rs: 1 #[test]", stderr)

    def test_checkout_local_build_and_target_caches_remain_excluded(self):
        root, _ = self.checkout("ordinary")
        self.binary(root, "build/cache")
        self.binary(root, "target/cache")
        status, stdout, stderr = self.run_check()
        self.assertEqual(status, 0, stderr)
        self.assertIn("0 registered binaries", stdout)

    def test_genuinely_stale_debt_still_fails(self):
        _, debt = self.checkout("ordinary")
        debt.write_text("services/absent/src/main.rs\t1\tfixture\n")
        status, _, stderr = self.run_check()
        self.assertEqual(status, 1)
        self.assertIn("remove the stale row", stderr)


if __name__ == "__main__":
    unittest.main()
