#!/usr/bin/env python3
"""Check the validator registry (formal/validators.tsv).

A validator replaces a TLA+ model for a sequential, single-actor admission
check whose executable test is the complete specification
(docs/ai/formal-scope-policy.md). Retiring the model removes the model checker
from the evidence chain, so this script keeps the replacement honest:

  * ids are unique and well formed, and the retired models are really gone
    from the model registry and from disk;
  * every validator has source-conformance witnesses that exist in the owning
    crate, and flows cite the validator (no orphan validator);
  * every validator is adversarially audited: at least MIN_KILLS
    implementation-mutation rows target its source and are designated to be
    killed by one of its own witnesses.

Mutation rows are read from formal/implementation-mutations.tsv plus any
--extra TSV (used to check proposed rows before they are merged).
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FORMAL = ROOT / "formal"
MIN_KILLS = 5


def rows(path: Path):
    for raw in path.read_text(encoding="utf-8").splitlines():
        if raw and not raw.startswith("#"):
            yield raw.split("\t")


def crate_root(source: str) -> Path:
    path = (ROOT / source).parent
    while path != ROOT and not (path / "Cargo.toml").is_file():
        path = path.parent
    return path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--extra", action="append", default=[], help="extra mutation rows TSV")
    args = parser.parse_args()
    errors: list[str] = []

    validators: dict[str, dict] = {}
    for fields in rows(FORMAL / "validators.tsv"):
        if len(fields) != 5:
            errors.append(f"validators.tsv: expected 5 columns: {fields[0]!r}")
            continue
        ident, package, sources, replaces, summary = fields
        if not re.fullmatch(r"validator:[a-z0-9]+(-[a-z0-9]+)*", ident):
            errors.append(f"malformed validator id {ident}")
        if ident in validators:
            errors.append(f"duplicate validator {ident}")
        validators[ident] = {
            "package": package,
            "sources": sources.split(","),
            "replaces": [model for model in replaces.split(",") if model],
            "summary": summary,
        }

    models = {fields[0].split("/")[0] for fields in rows(FORMAL / "models.tsv")}
    for ident, validator in validators.items():
        for source in validator["sources"]:
            if not (ROOT / source).is_file():
                errors.append(f"{ident}: missing source {source}")
        for model in validator["replaces"]:
            if model in models or (FORMAL / model).exists():
                errors.append(f"{ident}: retired model {model} is still registered or on disk")

    witnesses: dict[str, set[tuple[str, str]]] = {ident: set() for ident in validators}
    catalogue = (FORMAL / "run-source-conformance.sh").read_text(encoding="utf-8")
    for line in catalogue.splitlines():
        parts = line.split("|")
        if len(parts) >= 3 and parts[0].startswith("validator:"):
            if parts[0] not in validators:
                errors.append(f"source conformance names unknown validator {parts[0]}")
            else:
                witnesses[parts[0]].add((parts[1], parts[2]))
    for ident, validator in validators.items():
        if not witnesses[ident]:
            errors.append(f"{ident}: no source-conformance witness")
        root = crate_root(validator["sources"][0])
        text = "\n".join(
            path.read_text(encoding="utf-8", errors="replace") for path in root.rglob("*.rs")
        )
        for package, test in witnesses[ident]:
            if package != validator["package"]:
                errors.append(f"{ident}: witness package {package} != {validator['package']}")
            name = test.rsplit("::", 1)[-1]
            if not re.search(rf"\bfn {re.escape(name)}\b", text):
                errors.append(f"{ident}: witness {test} has no test function")

    cited = {fields[11] for fields in rows(FORMAL / "system-flows.tsv") if len(fields) > 11}
    for ident in validators:
        if ident not in cited:
            errors.append(f"{ident}: no system flow cites this validator")

    mutation_paths = [FORMAL / "implementation-mutations.tsv", *map(Path, args.extra)]
    kills = {ident: set() for ident in validators}
    for path in mutation_paths:
        for fields in rows(path):
            if len(fields) < 10:
                continue
            ident_row, _sev, source, _f, _r, _occ, package, _feat, _target, test = fields[:10]
            for ident, validator in validators.items():
                if (
                    source in validator["sources"]
                    and package == validator["package"]
                    and any(test == witness or test.endswith("::" + witness.rsplit("::", 1)[-1])
                            for _pkg, witness in witnesses[ident])
                ):
                    kills[ident].add(ident_row)
    for ident, killed in kills.items():
        if len(killed) < MIN_KILLS:
            errors.append(
                f"{ident}: only {len(killed)} implementation mutations are designated to be "
                f"killed by its witnesses (need {MIN_KILLS})"
            )

    for error in errors:
        print(f"validators: {error}", file=sys.stderr)
    if errors:
        return 1
    print(
        "validators: "
        + ", ".join(f"{ident}={len(kills[ident])} mutants/{len(witnesses[ident])} witnesses" for ident in validators)
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
