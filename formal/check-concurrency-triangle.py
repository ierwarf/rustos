#!/usr/bin/env python3
"""Reject stale or vacuous links between concurrency models and RustOS source."""

from __future__ import annotations

import csv
import pathlib
import re
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parent.parent
FORMAL = ROOT / "formal"
REGISTRY = FORMAL / "concurrency-triangle.toml"
LOOM = FORMAL / "loom-proof-kernel" / "src" / "lib.rs"
SHUTTLE = FORMAL / "shuttle-proof-kernel" / "src" / "lib.rs"
FLOWS = FORMAL / "system-flows.tsv"
LOCK = FORMAL / "herdtools.lock"


def fail(message: str) -> None:
    raise ValueError(message)


def require(condition: bool, message: str) -> None:
    if not condition:
        fail(message)


def function_exists(source: str, function: str) -> bool:
    return re.search(rf"\bfn\s+{re.escape(function)}\s*\(", source) is not None


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
        loom_source = LOOM.read_text(encoding="utf-8")
        shuttle_source = SHUTTLE.read_text(encoding="utf-8")
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
            require(function_exists(loom_source, entry["loom_test"]), f"{ident}: missing Loom test {entry['loom_test']}")
            require(function_exists(shuttle_source, entry["shuttle_test"]), f"{ident}: missing Shuttle test {entry['shuttle_test']}")

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
    except (OSError, tomllib.TOMLDecodeError, ValueError) as error:
        print(f"concurrency triangle check failed: {error}", file=sys.stderr)
        raise SystemExit(1)
    print(f"concurrency triangle is valid scenarios={len(scenarios)} herd_litmus={len(herd_seen) // 2}")


if __name__ == "__main__":
    main()
