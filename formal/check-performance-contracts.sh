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
rg -Fq 'SYS_RUSTOS_IPC_CALL_BOUNDED' "$ipc_ops" || {
    echo "explicit bounded userspace IPC syscall is missing" >&2
    exit 1
}
rg -Fq 'call_bounded(' services/inputd/src/main.rs || {
    echo "inputd-to-netd lifecycle call is not deadline-bounded" >&2
    exit 1
}
rg -Fq 'IPC_READINESS_QUERY_HARD_LIMIT_MS' services/inputd/src/main.rs || {
    echo "inputd-to-netd lifecycle call exceeds the readiness rail" >&2
    exit 1
}
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

rootd=services/rootd/src/main.rs
rg -Fq 'const ROOTD_REQUEST_DRAIN_BUDGET: usize = 32;' "$rootd" || {
    echo "rootd boot control burst drain bound drifted" >&2
    exit 1
}
rg -Fq '&& served == 0' "$rootd" || {
    echo "rootd can sleep through an already-progressing boot control burst" >&2
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
rg -Fq 'RustOS currently schedules all user work on the BSP' \
    tools/xtask/src/kvm/guest.rs || {
    echo "KVM topology regained an idle RustOS vCPU that contends with the DVM" >&2
    exit 1
}
rg -Fq 'const SYSTEM_READY_LATENCY_BOUND_MS: u64 = 2;' \
    kernel/ps/src/multitask/scheduler.rs || {
    echo "System dispatch latency rail exceeds the interactive frame budget" >&2
    exit 1
}
rg -Fq 'const USER_READY_LATENCY_BOUND_MS: u64 = 2;' \
    kernel/ps/src/multitask/scheduler.rs || {
    echo "User dispatch latency rail exceeds the interactive frame budget" >&2
    exit 1
}
rg -Fq 'background_probe_rank' services/runtimed/src/catalog.rs || {
    echo "background network probe can block the interactive launch path" >&2
    exit 1
}
rg -Fq 'RUSTOS_GPU_ACTIVE_MARKER' tools/xtask/src/kvm/guest.rs || {
    echo "KVM interactive boot gate lost the first completed GPU frame witness" >&2
    exit 1
}
rg -Fq 'reserved: 0' services/uiserver/src/sys.rs || {
    echo "GPU ABI v5 commit regained a user pixel pointer" >&2
    exit 1
}
rg -Fq 'copy_atlas_damage_to_slot(' services/uiserver/src/gpu_runtime.rs || {
    echo "uiserver no longer stages bounded damage into the exact DVM slot" >&2
    exit 1
}
rg -Fq 'accumulated[index].overlaps(merged)' services/uiserver/src/gpu_runtime.rs || {
    echo "uiserver damage history can again amplify disjoint damage into a bounding copy" >&2
    exit 1
}
rg -Fq 'reconstruction_damage_within_budget(' services/uiserver/src/gpu_runtime.rs || {
    echo "uiserver can again amplify a small update through an arbitrarily stale atlas slot" >&2
    exit 1
}
rg -Fq 'update_console_gpu_textures(' services/uiserver/src/gpu_runtime.rs || {
    echo "console output can again force a complete GPU atlas rebuild" >&2
    exit 1
}
rg -Fq 'update_gpu_layer_destinations(' services/uiserver/src/gpu_runtime.rs || {
    echo "window movement can again force a complete GPU atlas rebuild" >&2
    exit 1
}
for forbidden in \
    'mix(window.frame.x as u64);' \
    'mix(window.frame.y as u64);'; do
    if rg -Fq "$forbidden" services/uiserver/src/render.rs; then
        echo "window position leaked back into the structural GPU scene signature" >&2
        exit 1
    fi
done
rg -Fq 'layer_index: usize' services/uiserver/src/render.rs || {
    echo "retained GPU movement lost its exact texture-layer binding" >&2
    exit 1
}
rg -Fq 'weight_micros = 1000' apps/wayclick/RUSTOS.package.toml || {
    echo "WayClick lost its ordinary foreground fair-share scheduling contract" >&2
    exit 1
}
rg -Fq 'const WAYLAND_FRAME_CALLBACK_INTERVAL: Duration = Duration::from_millis(15);' \
    services/uiserver/src/main.rs || {
    echo "Wayland callback pacing is no longer phase-locked to DVM presentation" >&2
    exit 1
}
rg -Fq 'sleep_deadline = sleep_deadline.min(next_wayland_callback_pulse);' \
    services/uiserver/src/main.rs || {
    echo "uiserver can sleep past a pending Wayland callback deadline" >&2
    exit 1
}
rg -Fq 'flush_pointer_motion(true)' services/uiserver/src/wayland.rs || {
    echo "Wayland pointer coalescing no longer preserves motion-before-button order" >&2
    exit 1
}
rg -Fq 'validate_ui_bootstrap_metadata(&programs)' services/runtimed/src/catalog.rs || {
    echo "sealed UI bootstrap defaults are no longer checked against the launch catalog" >&2
    exit 1
}
if rg -Fq 'load_desktop_program_entries(DEFAULT_APPLICATIONS_DIR)' \
    services/runtimed/src/session.rs; then
    echo "UI bootstrap again scans the complete applications directory" >&2
    exit 1
fi
rg -Fq 'init_exec_priority(RUNTIMED_EXEC_PATH) < init_exec_priority(STORAGED_EXEC_PATH)' \
    services/initd/src/main.rs || {
    echo "immutable UI bootstrap is again ordered behind DVM-backed storaged" >&2
    exit 1
}
rg -Fq '_mm_stream_si128' services/uiserver/src/gpu_runtime.rs || {
    echo "large DVM atlas reconstruction lost its write-combine streaming store path" >&2
    exit 1
}
rg -Fq 'copy_xrgb_opaque_row(' services/uiserver/src/gpu_scene.rs || {
    echo "GPU topology rebuild regained scalar per-pixel XRGB conversion" >&2
    exit 1
}
rg -Fq '_mm_sfence' services/uiserver/src/gpu_runtime.rs || {
    echo "DVM atlas streaming stores are published without an ordering fence" >&2
    exit 1
}
rg -Fq 'gpu_atlas_slot_mapping' kernel/io-manager/src/io/dvm_display.rs || {
    echo "DVM atlas slots are not exposed as exact service capabilities" >&2
    exit 1
}
rg -Fq 'map_existing_user_pages_at_write_combine' \
    kernel/compat/src/user/syscall/linux/mm_broker_ops.rs || {
    echo "DVM atlas user mapping lost write-combine alias consistency" >&2
    exit 1
}
rg -Fq 'preserve_4k_leaf_pat' kernel/mm/src/memory/address_space.rs || {
    echo "mprotect can erase the 4-KiB PAT selector from DVM atlas mappings" >&2
    exit 1
}
if rg -n 'visit_user_read_spans|source_ptr' \
    kernel/io-manager/src/io/dvm_display.rs >/dev/null; then
    echo "ring0 regained per-frame atlas user-copy ownership" >&2
    exit 1
fi

wayclick=apps/wayclick/src/main.rs
rg -Fq '.include_cursor(self.cursor_x, self.cursor_y)' "$wayclick" || {
    echo "WayClick pointer motion lost its bounded damage accumulator" >&2
    exit 1
}
rg -Fq 'copy_wayland_bgra_row(' services/uiserver/src/wayland.rs || {
    echo "Wayland shm rows regained per-channel scalar reconstruction" >&2
    exit 1
}
rg -Fq 'self.pending_damage.take()' "$wayclick" || {
    echo "WayClick does not consume exact accumulated damage at commit" >&2
    exit 1
}
rg -Fq 'wayclick: initial flush failed' "$wayclick" || {
    echo "WayClick can block before publishing its initial registry request" >&2
    exit 1
}
if rg -Fq 'surface.damage(0, 0, WIDTH as i32, HEIGHT as i32)' "$wayclick"; then
    echo "WayClick reintroduced unconditional full-surface damage" >&2
    exit 1
fi

rg -Fq 'const MAX_SLOW_IPC_LOGS_PER_SECOND: usize = 1;' "$ipc_ops" || {
    echo "generic IPC slow logging is no longer rate-bounded" >&2
    exit 1
}
rg -Fq 'const MAX_SLOW_SERVICE_CALL_LOGS_PER_SECOND: usize = 1;' \
    kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs || {
    echo "typed service IPC slow logging is no longer rate-bounded" >&2
    exit 1
}
rg -Fq 'linux_clock_nanosleep_admission_cached' \
    kernel/compat/src/user/syscall/linux/syscalld_ops.rs || {
    echo "Linux time policy regained per-sleep synchronous syscalld IPC" >&2
    exit 1
}
rg -Fq 'IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY' \
    kernel/compat/src/user/syscall/linux/syscalld_ops.rs || {
    echo "syscalld receive backoff can again synchronously call its own endpoint" >&2
    exit 1
}

storaged_block=services/storaged/src/block.rs
rg -Fq 'const READ_AHEAD_IN_FLIGHT_LIMIT: usize = READ_CACHE_WINDOW_LIMIT;' \
    "$storaged_block" || {
    echo "storaged read-ahead pipeline lost its cache-budget bound" >&2
    exit 1
}
rg -Fq 'let mut in_flight = Vec::with_capacity(windows.len());' \
    "$storaged_block" || {
    echo "storaged sequential read-ahead is serialized before DVM submission" >&2
    exit 1
}
rg -Fq 'miss_continues_read_ahead' "$storaged_block" || {
    echo "storaged random reads can amplify into the full read-ahead pipeline" >&2
    exit 1
}
rg -Fq 'for (_, pending, _) in &in_flight[index + 1..]' \
    "$storaged_block" || {
    echo "storaged read-ahead completion failure no longer cancels pending tickets" >&2
    exit 1
}

for input_wake_source in \
    libs/driver-domain-protocol/src/lib.rs \
    libs/driver-domain-host/src/lib.rs \
    kernel/io-manager/src/input/dvm_ring.rs; do
    rg -Fq 'DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET' "$input_wake_source" || {
        echo "DVM input lost its post-commit consumer wake-generation contract: $input_wake_source" >&2
        exit 1
    }
done
rg -Fq 'arm_consumer_wake()' \
    kernel/compat/src/user/syscall/linux/input_broker_ops.rs || {
    echo "inputd wait broker no longer publishes wake generation before cursor recheck" >&2
    exit 1
}
rg -Fq 'const INPUT_INGESTION_WATCHDOG_MS: u64 = 100;' \
    kernel/compat/src/user/syscall/linux/input_broker_ops.rs || {
    echo "inputd wait broker lost its bounded lost-interrupt watchdog" >&2
    exit 1
}
rg -Fq 'const INPUT_RING_RECOVERY_KICK_RECORDS: u64 = 2;' \
    libs/driver-domain-host/src/lib.rs || {
    echo "DVM input ring lost its bounded lost-edge producer recovery kick" >&2
    exit 1
}
rg -Fq 'arm_sleep_waiter_until_tick(task_id, watchdog_deadline)' \
    kernel/compat/src/user/syscall/linux/input_broker_ops.rs || {
    echo "inputd lost-interrupt watchdog is not part of the armed wait contract" >&2
    exit 1
}
rg -Fq 'write_current_user_bytes(args.out_records_ptr, bytes)' \
    kernel/compat/src/user/syscall/linux/input_broker_ops.rs || {
    echo "input ingestion regressed to per-record user-copy validation" >&2
    exit 1
}
for witness in \
    'ingestion_waiting: AtomicBool' \
    'lock_input_queue_for_ingestion' \
    'queue.queue.try_lock()' \
    'if !queue.ingestion_waiting.load(Ordering::Acquire)'; do
    rg -Fq "$witness" services/inputd/src/main.rs || {
        echo "inputd lost its worker-first queue handoff: $witness" >&2
        exit 1
    }
done
rg -Fq 'difference_bounds(' services/uiserver/src/gpu_runtime.rs || {
    echo "uiserver topology rebuild lost retained-atlas differential damage" >&2
    exit 1
}

printf 'performance contract source checks passed\n'
