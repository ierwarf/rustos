#!/usr/bin/env bash
# Ensure every published service endpoint uses kernel-stamped sender identity
# and has an explicit owner-side authority contract.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

registry=formal/trust-boundaries.tsv
test -f "$registry" || { echo "missing $registry" >&2; exit 1; }
test -f formal/trust-identities.tsv || {
    echo "missing formal/trust-identities.tsv" >&2
    exit 1
}

declared_sources="$(
    {
        awk -F '\t' '!/^#/ && NF {print $3}' "$registry"
        awk -F '\t' '!/^#/ && NF {print $3}' formal/trust-identities.tsv
    } | sort -u
)"
published_sources="$(
    rg -l 'register_service_endpoint|register_linux_syscall_endpoint|SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT' \
        services --glob '*.rs' | sort -u
)"
if ! diff -u <(printf '%s\n' "$published_sources") <(printf '%s\n' "$declared_sources"); then
    echo "published service ingress does not match zero-trust registry" >&2
    exit 1
fi

if raw_receive="$(
    rg -n \
        '\bSYS_RUSTOS_IPC_(TRY_)?RECV\b|rustos_svc_runtime::ipc::(try_)?recv\(' \
        services --glob '*.rs' || true
)" && [[ -n "$raw_receive" ]]; then
    printf '%s\n' "$raw_receive" >&2
    echo "service ingress uses identity-blind IPC receive" >&2
    exit 1
fi

count=0
while IFS=$'\t' read -r boundary owner source identity delegation object response extra; do
    [[ -z "$boundary" || "$boundary" == \#* ]] && continue
    if [[ -n "${extra:-}" || -z "$response" ]]; then
        echo "invalid zero-trust registry row: $boundary" >&2
        exit 1
    fi
    [[ -f "$source" ]] || { echo "missing ingress source: $source" >&2; exit 1; }
    rg -q 'recv_with_sender|RECV_WITH_SENDER' "$source" || {
        echo "service ingress omits kernel-stamped sender: $boundary ($source)" >&2
        exit 1
    }
    rg -q "$identity" "$source" || {
        echo "service ingress omits identity admission evidence: $boundary" >&2
        exit 1
    }
    if [[ "$delegation" != "-" ]]; then
        rg -q "$delegation" "$source" || {
            echo "delegated ingress omits live service-owner proof: $boundary" >&2
            exit 1
        }
    fi
    [[ "$object" != "-" && "$response" != "-" ]] || {
        echo "service ingress omits object or response authority: $boundary" >&2
        exit 1
    }
    count=$((count + 1))
done < "$registry"

identity_count=0
while IFS=$'\t' read -r boundary owner source service authority lifecycle extra; do
    [[ -z "$boundary" || "$boundary" == \#* ]] && continue
    if [[ -n "${extra:-}" || -z "$lifecycle" ]]; then
        echo "invalid zero-trust identity row: $boundary" >&2
        exit 1
    fi
    [[ -f "$source" ]] || { echo "missing identity source: $source" >&2; exit 1; }
    rg -q "$service" "$source" || {
        echo "identity publication omits exact service id: $boundary" >&2
        exit 1
    }
    rg -q "$authority" "$source" || {
        echo "identity publication omits no-call authority marker: $boundary" >&2
        exit 1
    }
    if rg -q 'recv_with_sender|RECV_WITH_SENDER|\bSYS_RUSTOS_IPC_(TRY_)?RECV\b' "$source"; then
        echo "identity-only endpoint unexpectedly has a receive surface: $boundary" >&2
        exit 1
    fi
    identity_count=$((identity_count + 1))
done < formal/trust-identities.tsv

(( count > 0 )) || { echo "empty zero-trust registry" >&2; exit 1; }
(( identity_count > 0 )) || { echo "empty zero-trust identity registry" >&2; exit 1; }
printf 'zero-trust ingress contract passed: %s boundaries %s identity-only\n' \
    "$count" "$identity_count"
