#!/usr/bin/env bash
# Reject source drift at the boot/runtime IPC performance boundaries.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

performance=libs/rustos-user-abi/src/performance.rs
for witness in \
    'BOOT_TO_UI_TARGET_MS: u64 = 3_000' \
    'BOOT_TO_UI_HARD_LIMIT_MS: u64 = 5_000' \
    'UI_FRAME_HARD_LIMIT_US: u64 = 16_667' \
    'UI_FRAME_CPU_TARGET_US: u64 = 8_000' \
    'UI_INPUT_TO_PRESENT_HARD_LIMIT_US: u64 = 50_000' \
    'UI_BOOT_GPU_ACTIVATION_BUDGET_MS: u64 = 750' \
    'IPC_FOREGROUND_MAINTENANCE_SLICE_MS: u64 = 1' \
    'IPC_READINESS_QUERY_HARD_LIMIT_MS: u64 = 16' \
    'IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS: u64 = 100' \
    'IPC_BOOT_CONTROL_HARD_LIMIT_MS: u64 = 5_000' \
    'IPC_BULK_DATA_HARD_LIMIT_MS: u64 = 30_000' \
    'UI_FRAME_MAX_SYNCHRONOUS_POLICY_IPC: u32 = 0' \
    'SERVICE_LOOKUP_MAX_IPC_WITH_EXACT_GRANT: u32 = 0' \
    'SERVICE_ENDPOINT_STABLE_LOOKUP_MAX_LOCK_ACQUISITIONS: u32 = 0'
do
    rg -Fq "$witness" "$performance" || {
        echo "missing performance contract witness: $witness" >&2
        exit 1
    }
done

if rg -n 'pub\(super\) fn call_service_endpoint\(' \
    kernel/compat/src/user/syscall/linux >/dev/null; then
    echo "unclassified kernel service IPC helper returned" >&2
    exit 1
fi

mapfile -t raw_timeout_owners < <(
    rg -l 'call_service_endpoint_with_timeout\(' \
        kernel/compat/src/user/syscall/linux -g '*.rs' | sort
)
if [[ "${#raw_timeout_owners[@]}" -ne 1 \
    || "${raw_timeout_owners[0]}" != "kernel/compat/src/user/syscall/linux/ipc_ops.rs" ]]; then
    printf 'raw service timeout escaped typed IPC owner:\n%s\n' \
        "${raw_timeout_owners[*]:-none}" >&2
    exit 1
fi

ipc_ops=kernel/compat/src/user/syscall/linux/ipc_ops.rs
rg -Fq 'const SERVICE_ENDPOINT_STABLE_READ_ATTEMPTS: usize = 3;' "$ipc_ops" || {
    echo "service endpoint stable-read bound drifted" >&2
    exit 1
}
service_lookup_body=$(sed -n '/^fn service_endpoint_raw(/,/^}/p' "$ipc_ops")
if grep -Fq 'SERVICE_ENDPOINT_REGISTRY_MUTATION.lock()' <<<"$service_lookup_body"; then
    echo "stable service endpoint lookup reacquired the global mutation lock" >&2
    exit 1
fi
grep -Fq 'epoch_before == epoch_after' <<<"$service_lookup_body" || {
    echo "service endpoint lookup lost its epoch-stability recheck" >&2
    exit 1
}

ui_hot_paths=(
    services/uiserver/src/render.rs
    services/uiserver/src/render/chrome.rs
    services/uiserver/src/gpu_runtime.rs
    services/uiserver/src/gpu_scene.rs
)
if rg -n \
    'SYS_RUSTOS_IPC_|lookup_service_endpoint|register_service_endpoint|VFS_IPC_OP_|IPC_SERVICE_' \
    "${ui_hot_paths[@]}" >/dev/null; then
    echo "UI frame/present path gained synchronous policy-service IPC" >&2
    exit 1
fi

for service in inputd storaged devmgrd netd; do
    source="services/$service/src/main.rs"
    rg -Fq 'rustos_svc_runtime::ipc::register_service_endpoint' "$source" || {
        echo "$service does not use the single-attempt shared registration path" >&2
        exit 1
    }
    if rg -n 'fn register_service_endpoint|65_536' "$source" >/dev/null; then
        echo "$service reintroduced endpoint-registration retry amplification" >&2
        exit 1
    fi
done

runtimed=services/runtimed/src/session.rs
rg -Fq 'rustos_svc_runtime::ipc::register_service_endpoint' "$runtimed" || {
    echo "runtimed does not use the single-attempt shared registration path" >&2
    exit 1
}
if rg -n 'SERVICE_ENDPOINT_(READY|REGISTER)_ATTEMPTS|fn register_service_endpoint' \
    "$runtimed" >/dev/null; then
    echo "runtimed reintroduced service lookup/registration retry amplification" >&2
    exit 1
fi

ipc_helpers=kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs
rg -Fq 'const FOREGROUND_VFS_MAINTENANCE_ATTEMPTS: usize = 1;' "$ipc_helpers" || {
    echo "foreground VFS maintenance attempt bound drifted" >&2
    exit 1
}
rg -Fq 'IPC_FOREGROUND_MAINTENANCE_SLICE_MS' "$ipc_helpers" || {
    echo "foreground VFS maintenance lost its one-millisecond deadline" >&2
    exit 1
}

rg -Fq 'BOOT_TO_UI_HARD_LIMIT_MS' tools/xtask/src/kvm/guest.rs || {
    echo "KVM interactive boot gate is not bound to the shared hard limit" >&2
    exit 1
}
rg -Fq 'RUSTOS_GPU_ACTIVE_MARKER' tools/xtask/src/kvm/guest.rs || {
    echo "KVM interactive boot gate lost the first completed GPU frame witness" >&2
    exit 1
}

printf 'performance contract source checks passed\n'
