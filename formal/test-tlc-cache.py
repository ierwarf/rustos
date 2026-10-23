#!/usr/bin/env python3
"""Negative tests for exact-input TLC evidence reuse."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

from tlc_cache import individual_input_fingerprint, validate_cached_summary


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def expect_rejected(root: Path, reason: str) -> None:
    for individual in (False, True):
        try:
            validate_cached_summary(root, "pr", "sample/Sample", individual=individual)
        except ValueError:
            continue
        raise AssertionError(f"TLC cache accepted {reason}, individual={individual}")


def runner_input_binding_test() -> None:
    """Exercise the shipping runner with a fake pinned tool, not a model proof."""
    source = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix="rustos-tlc-binding-") as temporary:
        root = Path(temporary)
        formal = root / "formal"
        (formal / "sample").mkdir(parents=True)
        for name in ("run-tlc.sh", "tlc_cache.py"):
            shutil.copyfile(source / name, formal / name)
        subprocess.run(["git", "init", "--quiet", str(root)], check=True)
        spec = formal / "sample/Sample.tla"
        config = formal / "sample/Sample.cfg"
        spec.write_text("---- MODULE Sample ----\nEXTENDS Naturals\n====\n", encoding="utf-8")
        config.write_text("INVARIANT Safety\n", encoding="utf-8")
        (formal / "models.tsv").write_text(
            "sample/Sample\tsafety\tcheck\treason\t30\t60\texhaustive\tno\tno\tno\n", encoding="utf-8")
        (formal / "contracts.toml").write_text(
            '[profiles.pr]\ntlc_reuse_max_age_hours = 24\nrequired_models = ["sample/Sample"]\n', encoding="utf-8")
        cache = root / "cache"
        cache.mkdir()
        jar = cache / "tla2tools-fixture.jar"
        jar.write_bytes(b"pinned fixture, not an actual TLC jar")
        (formal / "tla2tools.lock").write_text(
            f"version=fixture\nurl=https://invalid.example/unused\nsha256={sha256(jar)}\n", encoding="utf-8")
        binary = root / "bin"
        binary.mkdir()
        java = binary / "java"
        java.write_text(
            '#!/bin/sh\n'
            'if [ -n "${MUTATE_TLC_INPUT:-}" ]; then echo "\\* changed" >> "$MUTATE_TLC_INPUT"; fi\n'
            'echo "<Spec>: 1:1"\n'
            'echo "2 states generated, 2 distinct states found, 0 states left on queue."\n'
            'echo "The depth of the complete state graph search is 2."\n', encoding="utf-8")
        java.chmod(0o755)
        env = os.environ.copy()
        for key in ("TLA_ARTIFACT_DIR", "TLA_SPEC_OVERRIDE", "TLA_CONFIG_OVERRIDE", "FORMAL_MUTATION_MODE"):
            env.pop(key, None)
        env.update(PATH=f"{binary}:{env['PATH']}", TLA_CACHE_DIR=str(cache))
        summary = root / "build/formal/tlc/pr/sample__Sample/summary.json"
        # The tool runs from its artifact directory: mutation targets must be
        # absolute and restored after each orchestration case.
        targets = [config, spec, formal / "run-tlc.sh", formal / "tlc_cache.py",
                   formal / "tla2tools.lock", spec.parent / "Naturals.tla"]
        for target in [None, *targets]:
            original = target.read_bytes() if target and target.exists() else None
            env["MUTATE_TLC_INPUT"] = str(target) if target else ""
            try:
                result = subprocess.run(["bash", "formal/run-tlc.sh", "sample/Sample"],
                    cwd=root, env=env, capture_output=True, text=True)
                value = json.loads(summary.read_text(encoding="utf-8"))
                if target:
                    assert result.returncode != 0, (target, result.stdout + result.stderr)
                    assert value["status"] == "input-changed", (target, value)
                else:
                    assert result.returncode == 0, result.stdout + result.stderr
                    assert len(value["inputs"]["individual_input_sha256"]) == 64
            finally:
                if target and original is not None:
                    target.write_bytes(original)
                elif target:
                    target.unlink()


def main() -> int:
    runner_input_binding_test()
    with tempfile.TemporaryDirectory(prefix="rustos-tlc-cache-") as temporary:
        root = Path(temporary)
        formal = root / "formal"
        formal.mkdir()
        (formal / "sample").mkdir()
        spec = formal / "sample/Sample.tla"
        config = formal / "sample/Sample.cfg"
        spec.write_text("---- MODULE Sample ----\n====\n", encoding="utf-8")
        config.write_text("INVARIANT Safety\n", encoding="utf-8")
        (formal / "contracts.toml").write_text(
            "[profiles.pr]\n"
            "tlc_reuse_max_age_hours = 24\n"
            'tlc_compatible_reuse_profiles = ["smp-iteration"]\n'
            'required_models = ["sample/Sample"]\n'
            "[profiles.smp-iteration]\n"
            "tlc_reuse_max_age_hours = 24\n"
            'tlc_compatible_reuse_profiles = ["pr"]\n'
            'required_models = ["sample/Sample"]\n',
            encoding="utf-8",
        )
        (formal / "models.tsv").write_text(
            "sample/Sample\tsafety\tcheck\treason\t30\t60\texhaustive\tno\tno\tno\n",
            encoding="utf-8",
        )
        (formal / "tla2tools.lock").write_text(
            "version=1.7.4\nsha256=" + "a" * 64 + "\n",
            encoding="utf-8",
        )
        (formal / "run-tlc.sh").write_text("#!/bin/sh\n# fixture runner\n", encoding="utf-8")
        shutil.copyfile(Path(__file__).with_name("tlc_cache.py"), formal / "tlc_cache.py")
        summary = root / "build/formal/tlc/pr/sample__Sample/summary.json"
        summary.parent.mkdir(parents=True)

        def write_summary() -> None:
            try:
                individual = individual_input_fingerprint(root, "sample/Sample")
            except ValueError:
                individual = ""
            summary.write_text(
                json.dumps(
                    {
                        "schema": "rustos-formal-evidence-v1",
                        "model": "sample/Sample",
                        "profile": "pr",
                        "status": "passed",
                        "tool": {
                            "name": "TLC",
                            "version": "1.7.4",
                            "sha256": "a" * 64,
                        },
                        "inputs": {
                            "spec_sha256": sha256(spec),
                            "config_sha256": sha256(config),
                            "individual_input_sha256": individual,
                        },
                        "policy": {
                            "deadlock": "check",
                            "workers": "auto",
                            "fingerprint": 0,
                            "seed": 1,
                        },
                        "metrics": {
                            "generated": 2,
                            "distinct": 2,
                            "depth": 2,
                            "covered_operators": 1,
                        },
                        "exit_code": 0,
                    }
                ),
                encoding="utf-8",
            )

        write_summary()
        validate_cached_summary(root, "pr", "sample/Sample")
        validate_cached_summary(root, "pr", "sample/Sample", individual=True)
        for invalidate in ("missing-marker", "runner", "validator", "lock", "shadow",
                           "root-shadow", "pr-shadow", "smp-iteration-shadow", "nightly-shadow"):
            write_summary()
            if invalidate == "missing-marker":
                value = json.loads(summary.read_text(encoding="utf-8"))
                del value["inputs"]["individual_input_sha256"]
                summary.write_text(json.dumps(value), encoding="utf-8")
            elif invalidate in ("runner", "validator", "lock"):
                name = {"runner": "run-tlc.sh", "validator": "tlc_cache.py",
                        "lock": "tla2tools.lock"}[invalidate]
                with (formal / name).open("a", encoding="utf-8") as changed:
                    changed.write("\n# changed input\n")
            else:
                if invalidate == "shadow":
                    shadow = spec.parent / "Naturals.tla"
                elif invalidate == "root-shadow":
                    shadow = formal / "Naturals.tla"
                else:
                    profile = invalidate.removesuffix("-shadow")
                    shadow = root / f"build/formal/tlc/{profile}/sample__Sample/Naturals.tla"
                shadow.parent.mkdir(parents=True, exist_ok=True)
                shadow.write_text("---- MODULE Naturals ----\n====\n", encoding="utf-8")
            try:
                validate_cached_summary(root, "pr", "sample/Sample", individual=True)
            except ValueError:
                pass
            else:
                raise AssertionError(f"individual proof accepted {invalidate}")
            if invalidate.endswith("shadow"):
                shadow.unlink()
        write_summary()

        # Individual impacted proofs may be outside the curated PR model set,
        # but cannot become evidence for the whole profile by that route.
        contracts = formal / "contracts.toml"
        original_contracts = contracts.read_text(encoding="utf-8")
        contracts.write_text(original_contracts.replace(
            'required_models = ["sample/Sample"]', 'required_models = []'
        ), encoding="utf-8")
        try:
            validate_cached_summary(root, "pr", "sample/Sample")
        except ValueError:
            pass
        else:
            raise AssertionError("individual proof widened the profile gate")
        validate_cached_summary(root, "pr", "sample/Sample", individual=True)
        contracts.write_text(original_contracts, encoding="utf-8")

        # No source import can escape the input digest. Unknown, multiline or
        # instantiated modules conservatively require a fresh TLC execution.
        original_spec = spec.read_text(encoding="utf-8")
        for imports in (
            "EXTENDS Local\n", "EXTENDS Naturals,\n Local\n",
            "EXTENDS Naturals\n    , Local\n",
            "EXTENDS Naturals\n\n\\* comment\n    , Local\n",
            "EXTENDS Naturals (* outer (* nested *) comment *)\n    , Local\n",
            "EXTENDS Naturals\n (* outer (* nested *) comment *) , Local\n",
            "EXTENDS Naturals\n    , FiniteSets\n",
            "LOCAL M == INSTANCE Local\n",
        ):
            spec.write_text("---- MODULE Sample ----\n" + imports + "====\n", encoding="utf-8")
            write_summary()
            try:
                validate_cached_summary(root, "pr", "sample/Sample", individual=True)
            except ValueError:
                pass
            else:
                raise AssertionError(f"unbound dependency accepted: {imports}")
        spec.write_text("---- MODULE Sample ----\nEXTENDS Naturals, FiniteSets\n====\n", encoding="utf-8")
        write_summary()
        validate_cached_summary(root, "pr", "sample/Sample", individual=True)
        spec.write_text(original_spec, encoding="utf-8")
        write_summary()
        for key in ("FORMAL_MUTATION_MODE", "TLA_ARTIFACT_DIR", "TLA_SPEC_OVERRIDE", "TLA_CONFIG_OVERRIDE"):
            previous = os.environ.get(key)
            os.environ[key] = "1"
            try:
                try:
                    validate_cached_summary(root, "pr", "sample/Sample", individual=True)
                except ValueError:
                    pass
                else:
                    raise AssertionError(f"individual reuse accepted override {key}")
            finally:
                if previous is None:
                    del os.environ[key]
                else:
                    os.environ[key] = previous

        near_expiry = time.time() - 24 * 3600 + 60
        os.utime(summary, (near_expiry, near_expiry))
        validate_cached_summary(root, "pr", "sample/Sample")
        try:
            validate_cached_summary(
                root,
                "pr",
                "sample/Sample",
                min_remaining_seconds=120,
            )
        except ValueError:
            pass
        else:
            raise AssertionError("TLC cache accepted evidence that expires before sealing")

        spec.write_text("---- MODULE Sample ----\nVARIABLE changed\n====\n", encoding="utf-8")
        expect_rejected(root, "a changed specification")
        spec.write_text("---- MODULE Sample ----\n====\n", encoding="utf-8")

        config.write_text("INVARIANT Other!\n", encoding="utf-8")
        expect_rejected(root, "a changed configuration")
        config.write_text("INVARIANT Safety\n", encoding="utf-8")
        for field, value in (("schema", "other"), ("status", "failed"), ("model", "other/Other"), ("exit_code", 1)):
            write_summary()
            changed = json.loads(summary.read_text(encoding="utf-8"))
            changed[field] = value
            summary.write_text(json.dumps(changed), encoding="utf-8")
            expect_rejected(root, f"a changed {field}")

        write_summary()
        value = json.loads(summary.read_text(encoding="utf-8"))
        value["policy"]["deadlock"] = "intentional-terminal"
        summary.write_text(json.dumps(value), encoding="utf-8")
        expect_rejected(root, "a changed deadlock policy")

        write_summary()
        old = summary.stat().st_mtime - 25 * 3600
        os.utime(summary, (old, old))
        expect_rejected(root, "expired evidence")

        shared = root / "build/formal/tlc/smp-iteration/sample__Sample/summary.json"
        shared.parent.mkdir(parents=True, exist_ok=True)
        write_summary()
        shared_value = json.loads(summary.read_text(encoding="utf-8"))
        shared_value["profile"] = "smp-iteration"
        shared.write_text(json.dumps(shared_value), encoding="utf-8")
        old = summary.stat().st_mtime - 25 * 3600
        os.utime(summary, (old, old))
        reused = validate_cached_summary(root, "pr", "sample/Sample")
        assert reused.summary == shared
        assert reused.evidence_profile == "smp-iteration"

        value = json.loads(shared.read_text(encoding="utf-8"))
        value["policy"]["seed"] = 2
        shared.write_text(json.dumps(value), encoding="utf-8")
        expect_rejected(root, "a cross-profile result with a changed execution policy")

    print("TLC exact-input cache selftest passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
