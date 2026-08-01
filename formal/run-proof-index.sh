#!/usr/bin/env bash
# Check and seal the machine-readable proof retrieval graph before proof runs.
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
python3 formal/check-proof-index.py
artifact_dir="${PROOF_INDEX_ARTIFACT_DIR:-$repo_root/build/formal/proof-index}"
mkdir -p "$artifact_dir"
python3 - "$artifact_dir/summary.json" <<'PY'
import hashlib
import json
import pathlib
import sys
import tomllib

root = pathlib.Path.cwd()
index_path = root / "formal/proof-index.toml"
with index_path.open("rb") as handle:
    index = tomllib.load(handle)

def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()

entries = []
for proof in index["proof"]:
    entry = {
        "id": proof["id"],
        "kind": proof["kind"],
        "formal_model": proof["formal_model"],
        "source": proof["source"],
        "source_sha256": digest(root / proof["source"]),
        "depends_on": proof["depends_on"],
    }
    if proof["kind"] == "kani":
        entry["package"] = proof["package"]
        entry["harnesses"] = proof["harnesses"]
    else:
        entry["proof_file"] = proof["proof_file"]
        entry["proof_file_sha256"] = digest(root / proof["proof_file"])
        entry["lemmas"] = proof["lemmas"]
        entry["counterpart_test"] = proof["counterpart_test"]
    entries.append(entry)

summary = {
    "schema": "rustos-proof-index-evidence-v1",
    "status": "passed",
    "proof_index_sha256": digest(index_path),
    "policy": index["policy"],
    "proofs": entries,
}
pathlib.Path(sys.argv[1]).write_text(json.dumps(summary, sort_keys=True) + "\n", encoding="utf-8")
PY
printf 'Proof index passed entries=%s evidence=%s\n' "$(jq '.proofs | length' "$artifact_dir/summary.json")" "$artifact_dir/summary.json"
