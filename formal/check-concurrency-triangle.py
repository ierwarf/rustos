#!/usr/bin/env python3
"""Reject stale or vacuous links between concurrency models and RustOS source."""

from __future__ import annotations

import csv
import pathlib
import re
import subprocess
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parent.parent
FORMAL = ROOT / "formal"
REGISTRY = FORMAL / "concurrency-triangle.toml"
LOOM_ROOT = FORMAL / "loom-proof-kernel" / "src"
SHUTTLE_ROOT = FORMAL / "shuttle-proof-kernel" / "src"
LOOM_MANIFEST = FORMAL / "loom-proof-kernel" / "Cargo.toml"
SHUTTLE_MANIFEST = FORMAL / "shuttle-proof-kernel" / "Cargo.toml"
FLOWS = FORMAL / "system-flows.tsv"
LOCK = FORMAL / "herdtools.lock"


def fail(message: str) -> None:
    raise ValueError(message)


def require(condition: bool, message: str) -> None:
    if not condition:
        fail(message)


def function_exists(source: str, function: str) -> bool:
    return re.search(rf"\bfn\s+{re.escape(function)}\s*\(", source) is not None


def proof_function_count(source: str, function: str) -> int:
    return len(re.findall(rf"\bfn\s+{re.escape(function)}\s*\(", source))


def rust_tree_source(root: pathlib.Path) -> str:
    sources = sorted(root.rglob("*.rs"))
    require(bool(sources), f"missing Rust proof sources under {root.relative_to(ROOT)}")
    return "\n".join(source.read_text(encoding="utf-8") for source in sources)


def compiled_tests(manifest: pathlib.Path) -> str:
    return subprocess.check_output(
        [
            "cargo",
            "test",
            "-q",
            "--manifest-path",
            str(manifest),
            "--",
            "--list",
        ],
        cwd=ROOT,
        text=True,
        stderr=subprocess.STDOUT,
    )


def compiled_test_exists(test_list: str, function: str) -> bool:
    return f"tests::{function}: test" in test_list.splitlines()


def main() -> None:
    try:
        with REGISTRY.open("rb") as handle:
            data = tomllib.load(handle)
        require(data.get("schema") == "rustos-concurrency-triangle-v1", "unsupported concurrency triangle schema")
        budget = data.get("budget")
        require(isinstance(budget, dict), "missing concurrency triangle budget")
        for key, minimum, maximum in (
            ("loom_max_branches", 1, 10_000),
            ("shuttle_iterations", 16, 2_048),
            ("shuttle_pct_depth", 1, 4),
            ("shuttle_max_seconds", 1, 120),
            ("herd_max_seconds", 1, 60),
        ):
            value = budget.get(key)
            require(isinstance(value, int) and minimum <= value <= maximum, f"invalid bounded budget {key}: {value!r}")

        scenarios = data.get("scenario")
        require(isinstance(scenarios, list) and scenarios, "empty concurrency triangle registry")
        ids = [entry.get("id") for entry in scenarios if isinstance(entry, dict)]
        require(len(ids) == len(scenarios) and all(isinstance(value, str) and value for value in ids), "missing scenario id")
        require(ids == sorted(ids) and len(set(ids)) == len(ids), "scenario ids must be unique and sorted")

        with FLOWS.open(newline="", encoding="utf-8") as handle:
            flow_rows = [
                {key.lstrip("# ").strip(): value for key, value in row.items()}
                for row in csv.DictReader(handle, delimiter="\t")
            ]
        loom_source = rust_tree_source(LOOM_ROOT)
        shuttle_source = rust_tree_source(SHUTTLE_ROOT)
        loom_tests = compiled_tests(LOOM_MANIFEST)
        shuttle_tests = compiled_tests(SHUTTLE_MANIFEST)
        herd_seen: set[str] = set()
        for entry in scenarios:
            ident = entry["id"]
            require(entry.get("severity") in {"critical", "high"}, f"{ident}: severity must be critical/high")
            for field in ("flow", "model", "source", "source_symbol", "invariant", "loom_test", "shuttle_test"):
                require(isinstance(entry.get(field), str) and entry[field], f"{ident}: missing {field}")
            source_path = ROOT / entry["source"]
            require(source_path.is_file(), f"{ident}: missing source {entry['source']}")
            source_text = source_path.read_text(encoding="utf-8")
            require(function_exists(source_text, entry["source_symbol"]), f"{ident}: missing source symbol {entry['source_symbol']}")
            anchors = entry.get("source_anchors")
            require(isinstance(anchors, list) and anchors and all(isinstance(anchor, str) and anchor for anchor in anchors), f"{ident}: invalid source_anchors")
            for anchor in anchors:
                require(anchor in source_text, f"{ident}: stale source ordering anchor: {anchor}")
            require(
                any(row.get("flow_id") == entry["flow"] and row.get("model") == entry["model"] and row.get("source") == entry["source"] for row in flow_rows),
                f"{ident}: no matching system-flow row",
            )
            require(proof_function_count(loom_source, entry["loom_test"]) == 1, f"{ident}: Loom test must have one exact source definition {entry['loom_test']}")
            require(proof_function_count(shuttle_source, entry["shuttle_test"]) == 1, f"{ident}: Shuttle test must have one exact source definition {entry['shuttle_test']}")
            require(compiled_test_exists(loom_tests, entry["loom_test"]), f"{ident}: Loom test is not compiled {entry['loom_test']}")
            require(compiled_test_exists(shuttle_tests, entry["shuttle_test"]), f"{ident}: Shuttle test is not compiled {entry['shuttle_test']}")

            herd_test = entry.get("herd_test")
            herd_mutant = entry.get("herd_mutant")
            if herd_test is None and herd_mutant is None:
                reason = entry.get("herd_not_applicable")
                require(isinstance(reason, str) and len(reason) >= 24, f"{ident}: herd omission needs a precise non-applicability reason")
                continue
            require(isinstance(herd_test, str) and isinstance(herd_mutant, str), f"{ident}: herd test/mutant must be paired")
            require(entry.get("herd_cat") == "x86tso-mixed.cat", f"{ident}: only pinned x86tso-mixed.cat is admissible")
            for path_string in (herd_test, herd_mutant):
                path = ROOT / path_string
                require(path.is_file(), f"{ident}: missing herd litmus {path_string}")
                content = path.read_text(encoding="utf-8")
                require(content.startswith("X86_64 "), f"{ident}: litmus must use x86_64 architecture")
                require("exists (" in content, f"{ident}: litmus must assert an explicit forbidden outcome")
                require(path_string not in herd_seen, f"{ident}: duplicate herd litmus {path_string}")
                herd_seen.add(path_string)
            require(herd_test != herd_mutant, f"{ident}: herd mutant must differ from baseline")

        with LOCK.open("rb") as handle:
            lock = tomllib.load(handle)
        require(lock.get("version") == "7.58", "herdtools lock must pin 7.58")
        digest = lock.get("source_sha256")
        require(isinstance(digest, str) and re.fullmatch(r"[0-9a-f]{64}", digest) is not None, "invalid herdtools source SHA-256")
        package_digest = lock.get("package_sha256")
        require(lock.get("package_version") == "7.58-1", "herdtools package must pin 7.58-1")
        require(isinstance(package_digest, str) and re.fullmatch(r"[0-9a-f]{64}", package_digest) is not None, "invalid herdtools package SHA-256")
    except (OSError, subprocess.CalledProcessError, tomllib.TOMLDecodeError, ValueError) as error:
        print(f"concurrency triangle check failed: {error}", file=sys.stderr)
        raise SystemExit(1)
    print(f"concurrency triangle is valid scenarios={len(scenarios)} herd_litmus={len(herd_seen) // 2}")


if __name__ == "__main__":
    main()
