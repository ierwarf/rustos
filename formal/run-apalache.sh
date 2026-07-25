#!/usr/bin/env bash
# Cross-check small typed refinements with a second symbolic model checker.
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
version="$(sed -n 's/^version=//p' "$repo_root/formal/apalache.lock")"
apalache="${RUSTOS_APALACHE:-$HOME/.cache/rustos/apalache/$version/bin/apalache-mc}"
[[ -x "$apalache" ]] || { echo "missing Apalache; run: bash formal/setup-apalache.sh" >&2; exit 2; }
artifact_dir="${APALACHE_ARTIFACT_DIR:-$repo_root/build/formal/apalache}"
mkdir -p "$artifact_dir"

run_model() {
    local model="$1" invariants="$2"
    local name="$(basename "$model" .tla)"
    local log="$artifact_dir/$name.log"
    if ! {
        "$apalache" --out-dir="$artifact_dir/$name-typecheck" typecheck "$model"
        "$apalache" --out-dir="$artifact_dir/$name-check" check \
            --init=Init --next=Next --inv="$invariants" --length=8 --no-deadlock "$model"
    } >"$log" 2>&1; then
        tail -n 100 "$log" >&2
        return 1
    fi
}

run_model "$repo_root/formal/apalache-pilots/ExecTicketPilot.tla" \
    TypeOK,PendingIsExactlyBound,TicketIsOneShot
run_model "$repo_root/formal/apalache-pilots/IpcHandleTransferPilot.tla" \
    TypeOK,RegistryExactlyTracksTransfer,TerminalCannotPinAuthority
run_model "$repo_root/formal/apalache-pilots/UserspaceWaitSetPilot.tla" \
    TypeOK,SleepingHasExactGeneration,NoSleepWithoutWaiter,RevokedCannotSleep
jq -n --arg version "$version" \
    '{schema:"rustos-apalache-evidence-v1",status:"passed",tool:{name:"Apalache",version:$version},models:3,bound:8,claim:"typed bounded symbolic refinement pilots"}' \
    >"$artifact_dir/summary.json"
printf 'Apalache pilots passed models=3 version=%s\n' "$version"
