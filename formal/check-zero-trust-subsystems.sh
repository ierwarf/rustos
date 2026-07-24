#!/usr/bin/env bash
# Keep every high-risk external parser, shared-memory consumer, and local
# control socket tied to executable admission and lifecycle evidence.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

registry=formal/zero-trust-subsystems.tsv
models=formal/models.tsv
conformance=formal/run-source-conformance.sh
flows=formal/system-flows.tsv
test -f "$registry" || { echo "missing $registry" >&2; exit 1; }

registered_dvm_sources="$(
    awk -F '\t' '$2 == "dvm-shared-memory" {print $4}' "$registry" | sort -u
)"
implemented_dvm_sources="$(
    {
        find kernel/io-manager/src/io -maxdepth 1 -type f -name 'dvm_*.rs'
        printf '%s\n' kernel/io-manager/src/input/dvm_ring.rs
    } | sort -u
)"
if ! diff -u <(printf '%s\n' "$implemented_dvm_sources") \
    <(printf '%s\n' "$registered_dvm_sources"); then
    echo "DVM shared-memory ingress does not match zero-trust subsystem registry" >&2
    exit 1
fi

registered_socket_sources="$(
    awk -F '\t' '$2 == "local-socket" {print $4}' "$registry" | sort -u
)"
implemented_socket_sources="$(
    rg -l 'accept4\(|UnixListener|libc::bind\(' services --glob '*.rs' | sort -u
)"
if ! diff -u <(printf '%s\n' "$implemented_socket_sources") \
    <(printf '%s\n' "$registered_socket_sources"); then
    echo "local socket ingress does not match zero-trust subsystem registry" >&2
    exit 1
fi

# Host-side DVM vsock and QMP readers cross a stronger trust boundary than a
# service-local socket. Keep their complete source inventory model-bound too,
# so a new hardware-domain control surface cannot arrive as an unreviewed
# parser merely because it lives outside services/.
registered_host_control_sources="$(
    awk -F '\t' '$2 == "host-control" {print $4}' "$registry" | sort -u
)"
implemented_host_control_sources="$(
    rg -l 'AF_VSOCK|VMADDR_CID|sockaddr_vm|read_qmp_message|qmp_capabilities' \
        libs tools --glob '*.rs' | sort -u
)"
if ! diff -u <(printf '%s\n' "$implemented_host_control_sources") \
    <(printf '%s\n' "$registered_host_control_sources"); then
    echo "host control ingress does not match zero-trust subsystem registry" >&2
    exit 1
fi

# A service must name the exact wire contract it emits. Numeric literals make
# unrelated ABI revisions silently strand a producer or consumer, which turns
# fail-closed admission into a boot outage instead of a localized rejection.
if rg -n 'abi_version:[[:space:]]*[0-9]+([,[:space:]]|$)' services --glob '*.rs'; then
    echo "service request uses a numeric ABI version instead of its contract constant" >&2
    exit 1
fi

count=0
while IFS=$'\t' read -r boundary class owner source model shape authority lifecycle flow extra; do
    [[ -z "$boundary" || "$boundary" == \#* ]] && continue
    if [[ -n "${extra:-}" || -z "$flow" ]]; then
        echo "invalid subsystem ingress row: $boundary" >&2
        exit 1
    fi
    [[ -f "$source" ]] || { echo "missing subsystem ingress source: $source" >&2; exit 1; }
    awk -F '\t' -v wanted="$model" '$1 == wanted {found++} END {exit found == 1 ? 0 : 1}' \
        "$models" || {
        echo "subsystem ingress has unregistered model: $boundary ($model)" >&2
        exit 1
    }
    rg -q "$shape" "$source" || {
        echo "subsystem ingress omits shape evidence: $boundary" >&2
        exit 1
    }
    rg -q "$authority" "$source" || {
        echo "subsystem ingress omits authority evidence: $boundary" >&2
        exit 1
    }
    rg -q "$lifecycle" "$source" || {
        echo "subsystem ingress omits lifecycle evidence: $boundary" >&2
        exit 1
    }
    rg -q "^${model//\//\\/}\\|" "$conformance" || {
        echo "subsystem ingress model lacks executable source witness: $boundary" >&2
        exit 1
    }
    awk -F '\t' -v wanted_flow="$flow" -v wanted_model="$model" \
        '$1 == wanted_flow && $12 == wanted_model {found++}
         END {exit found > 0 ? 0 : 1}' "$flows" || {
        echo "subsystem ingress lacks a model-bound end-to-end flow: $boundary" >&2
        exit 1
    }
    count=$((count + 1))
done < "$registry"

(( count > 0 )) || { echo "empty subsystem ingress registry" >&2; exit 1; }
printf 'zero-trust subsystem contract passed: %s boundaries\n' "$count"
