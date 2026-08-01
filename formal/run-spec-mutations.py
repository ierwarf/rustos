#!/usr/bin/env python3
"""Check registered TLA+ mutation adequacy with TLC counterexamples.

The runner never edits a checked-in model.  For each one-site mutant it first
checks the unchanged model, then requires TLC to reject the copied mutant by
the named invariant and to emit a normalized counterexample trace.  A parser
error, a timeout, an unrelated invariant failure, or a surviving mutant is a
verification failure rather than evidence of property quality.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from collections import defaultdict
from pathlib import Path
from typing import Any


KINDS = {
    "property-perturbation",
    "transition-effect",
    "transition-guard-removal",
    "transition-order",
    "transition-revocation",
}
SEVERITIES = {"critical", "high"}
INVARIANT_FAILURE = re.compile(
    r"(?:Error: Invariant |Invariant )(?:([A-Za-z_][A-Za-z0-9_]*) )?(?:is )?violated"
)
MODEL_LINE = re.compile(r"^[^#\t][^\t]*\t")


def root_path() -> Path:
    return Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )


def registered_models(root: Path) -> set[str]:
    models: set[str] = set()
    for raw in (root / "formal/models.tsv").read_text(encoding="utf-8").splitlines():
        if not raw or raw.startswith("#"):
            continue
        if not MODEL_LINE.match(raw):
            raise SystemExit(f"formal/models.tsv: malformed entry {raw!r}")
        model = raw.split("\t", 1)[0]
        if model in models:
            raise SystemExit(f"formal/models.tsv: duplicate model {model}")
        models.add(model)
    return models


def model_bindings(root: Path) -> set[str]:
    bindings: set[str] = set()
    for raw in (root / "formal/model-bindings.tsv").read_text(encoding="utf-8").splitlines():
        if not raw or raw.startswith("#"):
            continue
        fields = raw.split("\t")
        if len(fields) < 3:
            raise SystemExit(f"formal/model-bindings.tsv: malformed entry {raw!r}")
        bindings.add(fields[0])
    return bindings


def flow_models(root: Path) -> dict[str, set[str]]:
    result: dict[str, set[str]] = defaultdict(set)
    for raw in (root / "formal/model-bindings.tsv").read_text(encoding="utf-8").splitlines():
        if not raw or raw.startswith("#"):
            continue
        model, _relation, flow, *_rest = raw.split("\t")
        result[flow].add(model)
    return result


def read_corpus(root: Path) -> tuple[list[dict[str, Any]], list[str], str]:
    path = root / "formal/spec-mutations.toml"
    try:
        document = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as error:
        raise SystemExit(f"{path}: invalid TOML: {error}") from error
    if set(document) != {"schema", "required_models", "mutation"}:
        raise SystemExit(f"{path}: unexpected top-level fields")
    schema = document["schema"]
    required_models = document["required_models"]
    mutations = document["mutation"]
    if schema != "rustos-tla-mutation-corpus-v1":
        raise SystemExit(f"{path}: unsupported schema {schema!r}")
    if not isinstance(required_models, list) or not all(
        isinstance(model, str) for model in required_models
    ):
        raise SystemExit(f"{path}: required_models must be a string list")
    if required_models != sorted(set(required_models)):
        raise SystemExit(f"{path}: required_models must be sorted and unique")
    if not isinstance(mutations, list) or not mutations:
        raise SystemExit(f"{path}: mutation corpus is empty")
    return mutations, required_models, hashlib.sha256(path.read_bytes()).hexdigest()


def validate_corpus(root: Path) -> tuple[list[dict[str, Any]], str]:
    mutations, required, corpus_sha256 = read_corpus(root)
    models = registered_models(root)
    bindings = model_bindings(root)
    flows = flow_models(root)
    ids: set[str] = set()
    mutation_models: set[str] = set()
    exact = {
        "id",
        "kind",
        "severity",
        "model",
        "flow",
        "find",
        "replace",
        "occurrence",
        "invariant",
        "min_counterexample_states",
    }
    for mutation in mutations:
        if not isinstance(mutation, dict) or set(mutation) != exact:
            raise SystemExit("spec mutation entry has missing or unexpected fields")
        identity = mutation["id"]
        if not isinstance(identity, str) or not re.fullmatch(r"[a-z0-9][a-z0-9-]*", identity):
            raise SystemExit(f"invalid spec mutation id {identity!r}")
        if identity in ids:
            raise SystemExit(f"duplicate spec mutation id {identity}")
        ids.add(identity)
        if mutation["kind"] not in KINDS:
            raise SystemExit(f"{identity}: invalid mutation kind {mutation['kind']!r}")
        if mutation["severity"] not in SEVERITIES:
            raise SystemExit(f"{identity}: mutation must be critical or high")
        model = mutation["model"]
        if not isinstance(model, str) or model not in models:
            raise SystemExit(f"{identity}: model is not registered")
        if model not in bindings:
            raise SystemExit(f"{identity}: model lacks a source conformance binding")
        flow = mutation["flow"]
        if not isinstance(flow, str) or model not in flows.get(flow, set()):
            raise SystemExit(f"{identity}: flow does not bind the selected model")
        mutation_models.add(model)
        for field in ("find", "replace", "invariant"):
            if not isinstance(mutation[field], str) or not mutation[field]:
                raise SystemExit(f"{identity}: {field} must be nonempty")
        occurrence = mutation["occurrence"]
        if not isinstance(occurrence, int) or not 1 <= occurrence <= 8:
            raise SystemExit(f"{identity}: occurrence must be 1..=8")
        minimum = mutation["min_counterexample_states"]
        if not isinstance(minimum, int) or not 1 <= minimum <= 64:
            raise SystemExit(f"{identity}: min_counterexample_states must be 1..=64")
        source = root / "formal" / f"{model}.tla"
        text = source.read_text(encoding="utf-8")
        if text.count(mutation["find"]) != occurrence:
            raise SystemExit(
                f"{identity}: expected exactly {occurrence} mutation anchor occurrence(s)"
            )
        config = root / "formal" / f"{model}.cfg"
        if not re.search(
            rf"^\s*(?:(?:INVARIANT|INVARIANTS)\s+)?{re.escape(mutation['invariant'])}\s*$",
            config.read_text(encoding="utf-8"),
            flags=re.MULTILINE,
        ):
            raise SystemExit(f"{identity}: named invariant is not configured")
    if list(mutation["id"] for mutation in mutations) != sorted(ids):
        raise SystemExit("spec mutation ids must be sorted and unique")
    if mutation_models != set(required):
        missing = sorted(set(required) - mutation_models)
        extra = sorted(mutation_models - set(required))
        raise SystemExit(
            f"spec mutation required-model coverage drifted missing={missing} extra={extra}"
        )
    return mutations, corpus_sha256


def run(command: list[str], *, env: dict[str, str], output: Path) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        command,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env=env,
        check=False,
    )
    output.write_text(result.stdout, encoding="utf-8")
    return result


def replace_once(text: str, mutation: dict[str, Any]) -> str:
    find = str(mutation["find"])
    occurrence = int(mutation["occurrence"])
    if text.count(find) != occurrence:
        raise ValueError("mutation anchor changed after corpus validation")
    offset = 0
    for _ in range(occurrence):
        index = text.find(find, offset)
        if index < 0:
            raise ValueError("mutation anchor disappeared")
        offset = index + len(find)
    return text[:index] + str(mutation["replace"]) + text[index + len(find) :]


def has_exact_passed_baseline(root: Path, model: str) -> bool:
    spec = root / "formal" / f"{model}.tla"
    config = root / "formal" / f"{model}.cfg"
    expected_spec = hashlib.sha256(spec.read_bytes()).hexdigest()
    expected_config = hashlib.sha256(config.read_bytes()).hexdigest()
    lock_sha = ""
    for line in (root / "formal/tla2tools.lock").read_text(encoding="utf-8").splitlines():
        if line.startswith("sha256="):
            lock_sha = line.split("=", 1)[1]
            break
    pattern = f"*/{model.replace('/', '__')}/summary.json"
    for evidence in (root / "build/formal/tlc").glob(pattern):
        try:
            summary = json.loads(evidence.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if (
            summary.get("status") == "passed"
            and summary.get("inputs", {}).get("spec_sha256") == expected_spec
            and summary.get("inputs", {}).get("config_sha256") == expected_config
            and summary.get("tool", {}).get("sha256") == lock_sha
        ):
            return True
    return False


def counterexample(root: Path, mutation: dict[str, Any], artifact: Path) -> dict[str, Any]:
    log = artifact / "tlc.log"
    trace = artifact / "counterexample.json"
    if not log.is_file() or not trace.is_file():
        raise SystemExit(f"{mutation['id']}: TLC failed without a counterexample artifact")
    text = log.read_text(encoding="utf-8", errors="replace")
    expected = str(mutation["invariant"])
    invariant_matches = {
        match.group(1)
        for match in INVARIANT_FAILURE.finditer(text)
        if match.group(1) is not None
    }
    if expected not in invariant_matches:
        raise SystemExit(
            f"{mutation['id']}: mutant was not rejected by {expected}; "
            f"observed={sorted(invariant_matches)}"
        )
    payload = json.loads(trace.read_text(encoding="utf-8"))
    states = payload.get("states")
    count = payload.get("state_count")
    if not isinstance(states, list) or count != len(states):
        raise SystemExit(f"{mutation['id']}: malformed normalized TLC trace")
    minimum = int(mutation["min_counterexample_states"])
    if count < minimum:
        raise SystemExit(
            f"{mutation['id']}: counterexample has {count} states; requires {minimum}"
        )
    return {
        "state_count": count,
        "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(),
        "log_sha256": hashlib.sha256(log.read_bytes()).hexdigest(),
    }


def main() -> int:
    root = root_path()
    mutations, corpus_sha256 = validate_corpus(root)
    if sys.argv[1:] == ["--check"]:
        print(
            "TLA+ mutation corpus is valid "
            f"models={len({str(mutation['model']) for mutation in mutations})} "
            f"mutations={len(mutations)}"
        )
        return 0
    selected = mutations
    targeted = False
    if len(sys.argv) == 3 and sys.argv[1] == "--id":
        identity = sys.argv[2]
        selected = [mutation for mutation in mutations if mutation["id"] == identity]
        if not selected:
            raise SystemExit(f"unknown mutation id: {identity}")
        targeted = True
    elif len(sys.argv) != 1:
        raise SystemExit("usage: formal/run-spec-mutations.py [--check|--id <mutation-id>]")
    artifact_root = root / "build/formal/spec-mutations"
    artifact_root.mkdir(parents=True, exist_ok=True)
    base_env = os.environ.copy()
    base_env["FORMAL_MUTATION_MODE"] = "1"
    results: list[dict[str, Any]] = []
    baseline_models = sorted({str(mutation["model"]) for mutation in selected})
    for model in baseline_models:
        if has_exact_passed_baseline(root, model):
            continue
        baseline_log = artifact_root / f"{model.replace('/', '__')}-baseline.log"
        result = run(
            ["bash", str(root / "formal/run-tlc.sh"), "--profile", "pr", model],
            env=base_env,
            output=baseline_log,
        )
        if result.returncode != 0:
            raise SystemExit(f"{model}: baseline TLC run failed; see {baseline_log}")

    with tempfile.TemporaryDirectory(prefix="rustos-tla-mutations-") as temporary:
        temporary_root = Path(temporary)
        for mutation in selected:
            identity = str(mutation["id"])
            model = str(mutation["model"])
            source = root / "formal" / f"{model}.tla"
            config = root / "formal" / f"{model}.cfg"
            mutant_dir = temporary_root / identity
            mutant_dir.mkdir()
            mutant = mutant_dir / source.name
            mutant_config = mutant_dir / config.name
            original = source.read_text(encoding="utf-8")
            mutant.write_text(replace_once(original, mutation), encoding="utf-8")
            shutil.copyfile(config, mutant_config)
            artifact = artifact_root / identity
            if artifact.exists():
                shutil.rmtree(artifact)
            env = base_env | {
                "TLA_SPEC_OVERRIDE": str(mutant),
                "TLA_CONFIG_OVERRIDE": str(mutant_config),
                "TLA_ARTIFACT_DIR": str(artifact),
            }
            result = run(
                ["bash", str(root / "formal/run-tlc.sh"), "--profile", "pr", model],
                env=env,
                output=artifact_root / f"{identity}-runner.log",
            )
            if result.returncode == 0:
                raise SystemExit(f"{identity}: surviving TLA+ mutant")
            summary_path = artifact / "summary.json"
            if not summary_path.is_file():
                raise SystemExit(f"{identity}: TLC did not produce mutation summary")
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
            if summary.get("status") != "failed":
                raise SystemExit(
                    f"{identity}: mutant did not fail an invariant; status={summary.get('status')!r}"
                )
            trace = counterexample(root, mutation, artifact)
            results.append(
                {
                    **mutation,
                    "status": "killed",
                    "source_sha256": hashlib.sha256(original.encode()).hexdigest(),
                    "mutant_sha256": hashlib.sha256(mutant.read_bytes()).hexdigest(),
                    "counterexample": trace,
                }
            )

    summary = {
        "schema": "rustos-tla-mutation-evidence-v1",
        "status": "passed",
        "corpus_sha256": corpus_sha256,
        "mutation_count": len(results),
        "kill_count": len(results),
        "kill_ratio": 1.0,
        "mutations": results,
    }
    summary_path = (
        artifact_root / "summary.json"
        if not targeted
        else artifact_root / str(selected[0]["id"]) / "targeted-result.json"
    )
    summary_path.write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    scope = "targeted" if targeted else "full"
    print(
        "TLA+ specification mutations passed "
        f"scope={scope} killed={len(results)}/{len(results)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
