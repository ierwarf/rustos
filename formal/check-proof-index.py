#!/usr/bin/env python3
"""Validate the closed proof-retrieval graph and reject unsound ghost shortcuts."""

from __future__ import annotations

import pathlib
import re
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parent.parent
FORMAL = ROOT / "formal"
INDEX = FORMAL / "proof-index.toml"
RUN_KANI = FORMAL / "run-kani.sh"
VERUS_DIR = FORMAL / "verus-proof-kernel"
MODELS = FORMAL / "models.tsv"
FORBIDDEN_VERUS = (
    r"\badmit\s*!?\s*\(",
    r"\bassume\s*!?\s*\(",
    r"\baxiom\b",
    r"#\[verifier::(?:external|external_body|external_fn_specification)",
)


def fail(message: str) -> None:
    raise ValueError(message)


def require(condition: bool, message: str) -> None:
    if not condition:
        fail(message)


def has_function(source: str, name: str, prefix: str = "fn") -> bool:
    return re.search(rf"\b{prefix}\s+{re.escape(name)}\s*\(", source) is not None


def kani_harness_has_cover(source: str, name: str) -> bool:
    match = re.search(
        rf"#\[kani::proof\](?:(?!#\[kani::proof\])[\s\S])*?\bfn\s+{re.escape(name)}\s*\(",
        source,
    )
    if match is None:
        return False
    next_harness = source.find("#[kani::proof]", match.end())
    body = source[match.start() : next_harness if next_harness >= 0 else len(source)]
    return "kani::cover!" in body


def registered_models() -> set[str]:
    models: set[str] = set()
    for line in MODELS.read_text(encoding="utf-8").splitlines():
        if line and not line.startswith("#"):
            models.add(line.split("\t", 1)[0])
    return models


def main() -> None:
    try:
        with INDEX.open("rb") as handle:
            data = tomllib.load(handle)
        require(data.get("schema") == "rustos-proof-index-v1", "unsupported proof-index schema")
        policy = data.get("policy")
        require(isinstance(policy, dict), "missing proof-index policy")
        require(isinstance(policy.get("max_verus_files"), int) and 1 <= policy["max_verus_files"] <= 10, "invalid max_verus_files")
        require(isinstance(policy.get("max_dependency_depth"), int) and 1 <= policy["max_dependency_depth"] <= 16, "invalid max_dependency_depth")
        proofs = data.get("proof")
        require(isinstance(proofs, list) and proofs, "empty proof index")
        ids = [proof.get("id") for proof in proofs if isinstance(proof, dict)]
        require(len(ids) == len(proofs) and all(isinstance(ident, str) and ident for ident in ids), "missing proof id")
        require(ids == sorted(ids) and len(set(ids)) == len(ids), "proof ids must be unique and sorted")
        by_id = {proof["id"]: proof for proof in proofs}
        models = registered_models()

        kani_packages: set[str] = set()
        verus_files: set[str] = set()
        for proof in proofs:
            ident = proof["id"]
            kind = proof.get("kind")
            require(kind in {"kani", "verus"}, f"{ident}: invalid proof kind")
            for field in ("source", "symbol", "formal_model", "scope"):
                require(isinstance(proof.get(field), str) and proof[field], f"{ident}: missing {field}")
            require(proof["formal_model"] in models, f"{ident}: unknown formal model {proof['formal_model']}")
            source_path = ROOT / proof["source"]
            require(source_path.is_file(), f"{ident}: missing source {proof['source']}")
            source = source_path.read_text(encoding="utf-8")
            require(proof["symbol"].split("::")[-1] in source, f"{ident}: stale source symbol {proof['symbol']}")
            dependencies = proof.get("depends_on")
            require(isinstance(dependencies, list) and all(isinstance(item, str) and item in by_id and item != ident for item in dependencies), f"{ident}: invalid dependency")
            if kind == "kani":
                package = proof.get("package")
                harnesses = proof.get("harnesses")
                require(isinstance(package, str) and package, f"{ident}: missing Kani package")
                require(isinstance(harnesses, list) and harnesses and all(isinstance(name, str) and name for name in harnesses), f"{ident}: missing Kani harnesses")
                require(len(set(harnesses)) == len(harnesses), f"{ident}: duplicate Kani harness")
                for harness in harnesses:
                    require(kani_harness_has_cover(source, harness), f"{ident}: Kani harness lacks proof/cover pair: {harness}")
                kani_packages.add(package)
            else:
                proof_file = proof.get("proof_file")
                lemmas = proof.get("lemmas")
                counterpart = proof.get("counterpart_test")
                require(isinstance(proof_file, str) and proof_file, f"{ident}: missing Verus proof_file")
                require(isinstance(lemmas, list) and lemmas and all(isinstance(name, str) and name for name in lemmas), f"{ident}: missing Verus lemmas")
                require(isinstance(counterpart, str) and counterpart, f"{ident}: missing counterpart_test")
                file_path = ROOT / proof_file
                require(file_path.is_file(), f"{ident}: missing Verus file {proof_file}")
                verus_source = file_path.read_text(encoding="utf-8")
                for forbidden in FORBIDDEN_VERUS:
                    require(re.search(forbidden, verus_source) is None, f"{ident}: forbidden trusted shortcut matches {forbidden}")
                for lemma in lemmas:
                    require(has_function(verus_source, lemma, "proof fn"), f"{ident}: missing Verus lemma {lemma}")
                require(counterpart in source, f"{ident}: missing executable counterpart test {counterpart}")
                verus_files.add(proof_file)

        package_match = re.search(r"packages=\(([^)]*)\)", RUN_KANI.read_text(encoding="utf-8"))
        require(package_match is not None, "cannot read Kani package list")
        runner_packages = set(package_match.group(1).split())
        require(runner_packages == kani_packages, f"Kani runner/index package mismatch: runner={sorted(runner_packages)} index={sorted(kani_packages)}")
        actual_verus_files = {str(path.relative_to(ROOT)) for path in VERUS_DIR.glob("*.rs")}
        require(verus_files == actual_verus_files, f"Verus runner/index file mismatch: files={sorted(actual_verus_files)} index={sorted(verus_files)}")
        require(len(verus_files) <= policy["max_verus_files"], "Verus proof-file budget exceeded")

        def depth(ident: str, visiting: set[str]) -> int:
            require(ident not in visiting, f"cyclic proof dependency at {ident}")
            dependencies = by_id[ident]["depends_on"]
            if not dependencies:
                return 1
            return 1 + max(depth(dep, visiting | {ident}) for dep in dependencies)

        require(max(depth(ident, set()) for ident in by_id) <= policy["max_dependency_depth"], "proof dependency depth exceeded")
    except (OSError, tomllib.TOMLDecodeError, ValueError) as error:
        print(f"proof index check failed: {error}", file=sys.stderr)
        raise SystemExit(1)
    print(f"proof index is valid entries={len(proofs)} kani_packages={len(kani_packages)} verus_files={len(verus_files)}")


if __name__ == "__main__":
    main()
