#!/usr/bin/env bash
# Compare RustOS's compiled dual-ABI constants and layouts with native Linux
# UAPI headers and Microsoft-compatible headers executed under the pinned host
# compatibility environment.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${ABI_DIFFERENTIAL_ARTIFACT_DIR:-$repo_root/build/formal/abi-differential}"
mkdir -p "$artifact_dir"

cc="${CC:-clang}"
mingw_cc="${MINGW_CC:-x86_64-w64-mingw32-gcc}"
wine="${WINE:-wine}"
for tool in "$cc" "$mingw_cc" "$wine" python3; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "missing ABI reference tool: $tool" >&2
        exit 2
    }
done

"$cc" -std=c11 -O2 -Wall -Wextra -Werror \
    formal/abi-reference/linux_probe.c \
    -o "$artifact_dir/linux-reference"
"$artifact_dir/linux-reference" >"$artifact_dir/linux-reference.tsv"

"$mingw_cc" -std=c11 -O2 -Wall -Wextra -Werror \
    formal/abi-reference/windows_probe.c \
    -o "$artifact_dir/windows-reference.exe"
WINEDEBUG=-all WINEPREFIX="$artifact_dir/wine-prefix" \
    "$wine" "$artifact_dir/windows-reference.exe" \
    >"$artifact_dir/windows-reference.tsv" 2>"$artifact_dir/windows-reference.log"

cargo run -q -p contract-tests --bin abi_contract_probe -- linux \
    >"$artifact_dir/linux-rustos.tsv"
cargo run -q -p contract-tests --bin abi_contract_probe -- windows \
    >"$artifact_dir/windows-rustos.tsv"

python3 - "$repo_root" "$artifact_dir" <<'PY'
from __future__ import annotations

import datetime as dt
import hashlib
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
artifacts = Path(sys.argv[2])
divergence_path = root / "formal/abi-divergences.tsv"


def parse_values(path: Path) -> dict[str, int]:
    values: dict[str, int] = {}
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line:
            continue
        if "=" not in line:
            raise SystemExit(f"{path}:{number}: malformed ABI probe line")
        key, raw_value = line.split("=", 1)
        if not key or key in values:
            raise SystemExit(f"{path}:{number}: empty or duplicate key {key!r}")
        try:
            values[key] = int(raw_value, 0)
        except ValueError as error:
            raise SystemExit(f"{path}:{number}: invalid integer {raw_value!r}") from error
    return values


required_keys = {
    "linux": {
        "epoll_ctl_add",
        "map_fixed",
        "offset_epoll_event_data",
        "size_epoll_event",
        "size_msghdr",
        "size_stat",
        "size_statx",
    },
    "windows": {
        "error_invalid_handle",
        "image_file_machine_amd64",
        "image_nt_optional_hdr64_magic",
        "mem_commit",
        "page_execute_readwrite",
        "size_image_nt_headers64",
        "status_invalid_system_service",
    },
}
minimum_key_counts = {"linux": 47, "windows": 32}


divergences: dict[tuple[str, str], tuple[int, int, str, str, dt.date]] = {}
for number, raw in enumerate(
    divergence_path.read_text(encoding="utf-8").splitlines(), 1
):
    if not raw or raw.startswith("#"):
        continue
    fields = raw.split("\t")
    if len(fields) != 7:
        raise SystemExit(f"{divergence_path}:{number}: expected 7 fields")
    abi, key, rustos, reference, reason, owner, expires = fields
    identity = (abi, key)
    if abi not in {"linux", "windows"} or identity in divergences:
        raise SystemExit(f"{divergence_path}:{number}: invalid or duplicate identity")
    expiry = dt.date.fromisoformat(expires)
    if expiry < dt.date.today():
        raise SystemExit(f"{divergence_path}:{number}: expired divergence")
    if not reason or not owner:
        raise SystemExit(f"{divergence_path}:{number}: reason and owner are required")
    divergences[identity] = (int(rustos, 0), int(reference, 0), reason, owner, expiry)

results = []
used_divergences: set[tuple[str, str]] = set()
for abi in ("linux", "windows"):
    rustos = parse_values(artifacts / f"{abi}-rustos.tsv")
    reference = parse_values(artifacts / f"{abi}-reference.tsv")
    if len(reference) < minimum_key_counts[abi]:
        raise SystemExit(
            f"{abi}: reference coverage collapsed "
            f"keys={len(reference)} minimum={minimum_key_counts[abi]}"
        )
    missing_required = sorted(required_keys[abi] - reference.keys())
    if missing_required:
        raise SystemExit(f"{abi}: required reference keys missing={missing_required}")
    if rustos.keys() != reference.keys():
        missing = sorted(reference.keys() - rustos.keys())
        extra = sorted(rustos.keys() - reference.keys())
        raise SystemExit(f"{abi}: key mismatch missing={missing} extra={extra}")
    for key in sorted(rustos):
        actual = rustos[key]
        expected = reference[key]
        if actual == expected:
            results.append(
                {
                    "abi": abi,
                    "key": key,
                    "rustos": actual,
                    "reference": expected,
                    "result": "equal",
                }
            )
            continue
        identity = (abi, key)
        divergence = divergences.get(identity)
        if divergence is None or divergence[0] != actual or divergence[1] != expected:
            raise SystemExit(
                f"{abi}:{key}: RustOS={actual:#x} reference={expected:#x} "
                "without an exact divergence record"
            )
        used_divergences.add(identity)
        results.append(
            {
                "abi": abi,
                "key": key,
                "rustos": actual,
                "reference": expected,
                "result": "declared-divergence",
            }
        )

unused = sorted(set(divergences) - used_divergences)
if unused:
    raise SystemExit(f"stale ABI divergence records: {unused}")

inputs = {}
for relative in (
    "formal/abi-reference/linux_probe.c",
    "formal/abi-reference/windows_probe.c",
    "formal/abi-divergences.tsv",
    "tests/contract-tests/src/bin/abi_contract_probe.rs",
    "libs/rustos-user-abi/src/linux.rs",
    "libs/rustos-user-abi/src/windows.rs",
):
    path = root / relative
    inputs[relative] = hashlib.sha256(path.read_bytes()).hexdigest()

summary = {
    "schema": "rustos-abi-differential-evidence-v1",
    "status": "passed",
    "comparisons": results,
    "comparison_count": len(results),
    "declared_divergence_count": len(used_divergences),
    "inputs": inputs,
}
(artifacts / "summary.json").write_text(
    json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
)
print(
    "ABI differential passed "
    f"comparisons={len(results)} divergences={len(used_divergences)}"
)
PY
