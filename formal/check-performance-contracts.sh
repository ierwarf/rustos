#!/usr/bin/env bash
# Reject source drift at the boot/runtime IPC performance boundaries.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

performance=libs/rustos-user-abi/src/performance.rs
for witness in \
    'BOOT_TO_UI_TARGET_MS: u64 = 3_000' \
    'BOOT_TO_UI_HARD_LIMIT_MS: u64 = 10_000' \
    'UI_FRAME_HARD_LIMIT_US: u64 = 16_667' \
    'UI_FRAME_CPU_TARGET_US: u64 = 8_000' \
    'UI_INPUT_TO_PRESENT_HARD_LIMIT_US: u64 = 50_000' \
    'UI_BOOT_GPU_ACTIVATION_BUDGET_MS: u64 = 750' \
    'IPC_READINESS_QUERY_HARD_LIMIT_MS: u64 = 16' \
    'IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS: u64 = 100' \
    'IPC_CONTROL_DRAIN_BUDGET: usize = 32' \
    'IPC_BOOT_CONTROL_HARD_LIMIT_MS: u64 = 5_000' \
    'DVM_STORAGE_BOOT_READY_HARD_LIMIT_MS: u64 = 4_000' \
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
ipc_reply_diagnostics=kernel/compat/src/user/syscall/linux/ipc_reply_diagnostics.rs
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
service_lookup_body=$(sed -n \
    '/^fn service_endpoint_raw(/,/^fn stable_service_endpoint_snapshot(/p' \
    "$ipc_ops")
if grep -Fq 'SERVICE_ENDPOINT_REGISTRY_MUTATION.lock()' <<<"$service_lookup_body"; then
    echo "stable service endpoint lookup reacquired the global mutation lock" >&2
    exit 1
fi
grep -Fq 'epoch_before == epoch_after' <<<"$service_lookup_body" || {
    echo "service endpoint lookup lost its epoch-stability recheck" >&2
    exit 1
}

rootd=services/rootd/src/main.rs
rootd_drain=services/rootd/src/control_drain.rs
rg -Fq 'const ROOTD_REQUEST_DRAIN_BUDGET: usize = IPC_CONTROL_DRAIN_BUDGET;' "$rootd_drain" || {
    echo "rootd boot control burst drain bound drifted" >&2
    exit 1
}
if [[ "$(rg -Fc 'control_drain::drain_rootd_control_requests' "$rootd")" -ne 2 ]]; then
    echo "rootd early and steady-state loops must share the bounded control drain" >&2
    exit 1
fi
rg -Fq '&& served == 0' "$rootd" || {
    echo "rootd can sleep through an already-progressing boot control burst" >&2
    exit 1
}
rg -Fq 'ROOTD_SUPERVISOR_IDLE_POLL_MS: u64 = 10' "$performance" || {
    echo "rootd steady-state supervisor poll bound drifted" >&2
    exit 1
}
supervisor_idle_body="$(sed -n '/fn supervisor_idle()/,/^}/p' "$rootd")"
grep -Fq 'wait_for_restart_backoff(delay_ms)' <<<"$supervisor_idle_body" || {
    echo "rootd steady-state supervisor stopped using the bounded wait broker" >&2
    exit 1
}
if grep -Fq 'yield_now()' <<<"$supervisor_idle_body"; then
    echo "rootd steady-state supervisor regressed to a runnable yield loop" >&2
    exit 1
fi

runtimed_main=services/runtimed/src/main.rs
rg -Fq 'const SESSION_REQUEST_DRAIN_BUDGET: usize = IPC_CONTROL_DRAIN_BUDGET;' "$runtimed_main" || {
    echo "runtimed session control burst drain bound drifted" >&2
    exit 1
}
rg -Fq 'drain_session_request_burst(session_endpoint, &mut state)' "$runtimed_main" || {
    echo "runtimed main loop stopped draining its ready session dependency burst" >&2
    exit 1
}
rg -Fq 'const POLL_INTERVAL: Duration = Duration::from_millis(10);' services/initd/src/main.rs \
    && rg -Fq 'pub(crate) const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(10);' "$runtimed_main" \
    && rg -Fq 'const INET_READINESS_POLL_INTERVAL: Duration = Duration::from_millis(10);' services/netd/src/main.rs || {
        echo "steady-state Ring3 pollers regained sub-10ms scheduler churn" >&2
        exit 1
    }

endpoint_priority=kernel/ipc-runtime/src/ipc/endpoint_priority.rs
rg -Fq 'super::MAX_ENDPOINT_PENDING_MESSAGES' "$endpoint_priority" \
    && rg -Fq 'IPC_CONTROL_DRAIN_BUDGET.saturating_mul(2)' "$endpoint_priority" || {
        echo "kernel endpoint admission no longer retains two control drain bursts" >&2
        exit 1
    }

typed_ipc=kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs
typed_ipc_diagnostics=kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers_diagnostics.rs
rg -Fq 'static FAILED_SERVICE_CALL_LOG_RATE_STATE' "$typed_ipc_diagnostics" \
    && rg -Fq 'static SLOW_SERVICE_CALL_LOG_RATE_STATE' "$typed_ipc_diagnostics" \
    && rg -Fq 'diagnostics::log_failed_service_call(' "$typed_ipc" || {
        echo "terminal typed-service failures lost their independent bounded diagnostic lane" >&2
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
    if [[ "$service" == inputd ]]; then
        source="services/inputd/src/service_loop.rs"
    fi
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
rg -Fq 'const HOUSEKEEPING_VFS_MAINTENANCE_ATTEMPTS: usize = 1;' "$ipc_helpers" || {
    echo "housekeeping VFS maintenance attempt bound drifted" >&2
    exit 1
}
rg -Fq 'IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS' "$ipc_helpers" || {
    echo "housekeeping VFS maintenance lost its bounded control deadline" >&2
    exit 1
}
if [[ "$(rg -c 'drain_pending_vfs_mutations\(\)' "$ipc_helpers")" != 2 ]]; then
    echo "VFS deferred recovery escaped its sole housekeeping entrypoint" >&2
    exit 1
fi
vfs_socket=kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs
if [[ "$(rg -c 'drain_pending_netd_refs\(\)' "$vfs_socket")" != 2 ]]; then
    echo "netd deferred recovery escaped its sole housekeeping entrypoint" >&2
    exit 1
fi

if rg -Fq 'guest_deadline_reached' tools/xtask/src/kvm/{guest.rs,options.rs}; then
    echo "KVM regained an independent guest boot deadline during the SMP qualification run" >&2
    exit 1
fi
rg -Fq 'rustos_vcpus: 1,' tools/xtask/src/kvm/guest.rs || {
    echo "default KVM topology regained an unrequested RustOS vCPU that contends with the DVM" >&2
    exit 1
}
rg -Fq '.arg(rustos_vcpus.to_string())' tools/xtask/src/kvm/guest.rs || {
    echo "explicit KVM SMP topology is no longer bound to the admitted RustOS vCPU count" >&2
    exit 1
}
rg -Fq 'const SYSTEM_READY_LATENCY_BOUND_MS: u64 = 2;' \
    kernel/ps/src/multitask/scheduler.rs || {
    echo "System dispatch latency rail exceeds the interactive frame budget" >&2
    exit 1
}
if rg -Fq 'USER_READY_LATENCY_BOUND_MS' kernel/ps/src/multitask/scheduler.rs; then
    echo "unadmitted User wall-clock deadline bypasses fair-share accounting" >&2
    exit 1
fi
rg -Fq 'fn reserved_user_pick(&self, current: usize) -> Option<usize>' \
    kernel/ps/src/multitask/scheduler.rs || {
    echo "User reservation is no longer isolated from wall-clock ready age" >&2
    exit 1
}
rg -Fq 'background_probe_rank' services/runtimed/src/catalog.rs || {
    echo "background network probe can block the interactive launch path" >&2
    exit 1
}
rg -Fq 'launch_failure_counts' services/runtimed/src/socket.rs || {
    echo "runtimed launch failures can retry without a consecutive-failure circuit breaker" >&2
    exit 1
}
rg -Fq 'MAX_LAUNCH_RETRY_BACKOFF' services/runtimed/src/socket.rs || {
    echo "runtimed launch retry backoff is not explicitly capped" >&2
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
rg -Fq 'scratch_atlas: Vec<u32>' services/uiserver/src/gpu_runtime.rs || {
    echo "GPU full-scene rebuild lost its reusable scratch atlas" >&2
    exit 1
}
if rg -Fq 'vec![0_u32; self.atlas.len()]' services/uiserver/src/gpu_runtime.rs; then
    echo "GPU full-scene rebuild regained a per-frame atlas allocation" >&2
    exit 1
fi
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
    services/initd/src/boot_order.rs || {
    echo "immutable UI bootstrap is again ordered behind DVM-backed storaged" >&2
    exit 1
}
rg -Fq 'exec == RUNTIMED_EXEC_PATH' services/initd/src/boot_order.rs || {
    echo "runtimed lost its immediate pre-storaged activation boundary" >&2
    exit 1
}
rg -Fq 'const GPU_INITIALIZATION_RETAINS_BOOT_CLASS: bool = true;' \
    services/uiserver/src/gpu_runtime.rs || {
    echo "mandatory GPU initialization can be demoted before its boot result" >&2
    exit 1
}
rg -Fq 'context.weight = (context.weight & LOAD_WEIGHT_MASK).min(NICE_0_LOAD);' \
    kernel/ps/src/multitask/scheduler.rs || {
    echo "scheduler self-demotion no longer caps inherited permanent fair weight" >&2
    exit 1
}
rg -Uq 'let reply = unsafe \{[\s\S]{0,700}rustos_svc_runtime::ipc::reply\([\s\S]{0,700}completion_demotion_due\(reply, handled\.demote_after_reply\)' \
    services/loaderd/src/main.rs || {
    echo "loaderd can demote before its terminal UI spawn reply completes" >&2
    exit 1
}
rg -Uq 'let reply = unsafe \{[\s\S]{0,1400}if ui_bootstrap_snapshot_reply_completed\(' \
    services/vfsd/src/main.rs || {
    echo "vfsd can demote before its terminal UI snapshot reply completes" >&2
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

heap_tracker=kernel/mm/src/memory/heap.rs
record_alloc_body="$(
    sed -n '/^    fn record_alloc(/,/^    fn begin_dealloc(/p' "$heap_tracker"
)"
grep -Fq 'self.insert_active(ptr, layout.size(), layout.align())' <<<"$record_alloc_body" || {
    echo "kernel allocation hot path lost its direct active-table insertion" >&2
    exit 1
}
if grep -Eq 'freed|quarantine|is_recently_freed|for |while ' <<<"$record_alloc_body"; then
    echo "kernel allocation hot path regained a quarantine scan" >&2
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
rg -Fq 'const MAX_REPLY_REJECTION_SUMMARIES_PER_SECOND: u8 = 1;' "$ipc_reply_diagnostics" || {
    echo "late IPC reply rejection summaries are no longer rate-bounded" >&2
    exit 1
}
rg -Fq '"ipc-reply-rejected-summary"' "$ipc_reply_diagnostics" || {
    echo "late IPC reply rejection volume lost its cumulative summary" >&2
    exit 1
}
rg -Fq 'pub struct ReplyFailureDiagnostics' libs/rustos-svc-runtime/src/ipc.rs || {
    echo "service reply failure diagnostics lost their shared bounded owner" >&2
    exit 1
}
for service_group in \
    'services/netd/src/main.rs' \
    'services/inputd/src/main.rs services/inputd/src/service_loop.rs' \
    'services/devmgrd/src/main.rs'; do
    read -r -a service_sources <<<"$service_group"
    service="${service_sources[0]}"
    rg -Fq 'ReplyFailureDiagnostics::new()' "${service_sources[@]}" || {
        echo "$service no longer owns a bounded reply failure lane" >&2
        exit 1
    }
    rg -Fq 'REPLY_FAILURE_DIAGNOSTICS.record(' "${service_sources[@]}" || {
        echo "$service bypasses bounded reply failure reporting" >&2
        exit 1
    }
    if rg -Uq 'writeln!\([^;]{0,200}reply failed' "${service_sources[@]}"; then
        echo "$service regained synchronous per-failure reply logging" >&2
        exit 1
    fi
done
rg -Fq 'const MAX_SERVICE_CALL_LOGS_PER_SECOND: u8 = 1;' \
    "$typed_ipc_diagnostics" || {
    echo "typed service IPC slow logging is no longer rate-bounded" >&2
    exit 1
}
time_hot_path="$(
    sed -n '/^pub fn syscall_linux_nanosleep(/,/^fn rtc_datetime_to_unix_seconds(/p' \
        kernel/compat/src/user/syscall/linux/service_ops/process_time.rs
)"
grep -Fq 'validate_time_hot_path_locally' <<<"$time_hot_path" || {
    echo "Linux time hot path lost local fixed-envelope admission" >&2
    exit 1
}
if grep -Eq 'request_syscalld|with_current_user_process_state(_mut)?' <<<"$time_hot_path"; then
    echo "Linux time hot path regained synchronous policy or process-state latency" >&2
    exit 1
fi
if rg -Fq 'linux_clock_nanosleep_admission_cached' \
    kernel/compat/src/user/syscall/linux/syscalld_ops.rs; then
    echo "retired per-process Linux time admission cache was reintroduced" >&2
    exit 1
fi
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
    'handoff: Mutex<InputQueueHandoff>' \
    'handoff_changed: Condvar' \
    'lock_input_queue_for_ingestion' \
    'queue.handoff_changed.wait(handoff)' \
    'if !handoff.ingestion_waiting'; do
    rg -Fq "$witness" services/inputd/src/main.rs || {
        echo "inputd lost its worker-first queue handoff: $witness" >&2
        exit 1
    }
done
rg -Fq 'difference_bounds(' services/uiserver/src/gpu_runtime.rs || {
    echo "uiserver topology rebuild lost retained-atlas differential damage" >&2
    exit 1
}
loaderd_main=services/loaderd/src/main.rs
rg -Fq 'fn trace_line(message: &str)' "$loaderd_main" &&
    rg -Fq 'option_env!("RUSTOS_LOGGING_BOOT_TRACE_ENABLED") == Some("true")' \
        "$loaderd_main" || {
    echo "loaderd success-path tracing is no longer controlled by the boot-trace contract" >&2
    exit 1
}
for phase in \
    'loaderd: open done' \
    'loaderd: validate begin' \
    'loaderd: prepare begin' \
    'loaderd: commit begin'; do
    rg -Fq "trace_line(&format!(\"$phase" "$loaderd_main" || {
        echo "loaderd synchronous debug output regained a boot hot-path phase: $phase" >&2
        exit 1
    }
done
rg -Uq 'trace_line\(&format!\(\n[[:space:]]*"loaderd: executable snapshot call begin' \
    "$loaderd_main" || {
    echo "loaderd executable snapshot trace regained synchronous console output" >&2
    exit 1
}
rg -Fq 'reply_failure_diagnostic_due(reply_failures)' services/vfsd/src/main.rs || {
    echo "vfsd reply cancellation diagnostics are no longer rate bounded" >&2
    exit 1
}
rg -Fq 'reply_failure_diagnostics_are_first_then_exponentially_rate_limited' \
    services/vfsd/src/lib.rs || {
    echo "vfsd reply diagnostic rate bound lost its executable witness" >&2
    exit 1
}
rg -Fq 'const LOCAL_MEMFD_IO_CHUNK_BYTES: usize = 64 * 1024;' \
    kernel/compat/src/user/syscall/linux/service_ops/local_memfd_io.rs || {
    echo "local memfd I/O regained sub-page lock and reschedule amplification" >&2
    exit 1
}
if rg -Fq 'let mut chunk = [0_u8; 256];' \
    kernel/compat/src/user/syscall/linux/service_ops/local_memfd_io.rs \
    kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs; then
    echo "local memfd write regained its 256-byte scheduler amplification loop" >&2
    exit 1
fi
rg -Fq 'sync_pick_hints: SlotHandoffQueue<MAX_TASK>' \
    kernel/ps/src/multitask/scheduler.rs || {
    echo "synchronous IPC peers no longer have complete bounded FIFO custody" >&2
    exit 1
}
rg -Uq 'let atomic_activation_handoff = self\.take_next_atomic_activation_handoff_ready_slot\(\);[\s\S]{0,500}let sync_handoff = if atomic_activation_handoff\.is_none\(\)[\s\S]{0,500}take_next_synchronous_pick_hint_ready_slot\(\)[\s\S]{0,800}match atomic_activation_handoff[\s\S]{0,800}match sync_handoff[\s\S]{0,500}mandatory_overdue_system_pick' \
    kernel/ps/src/multitask/scheduler.rs || {
    echo "atomic activation or synchronous IPC handoff no longer precedes unrelated overdue work" >&2
    exit 1
}
if [ "$(rg -c 'set_next_synchronous_pick_hint\(task_id\)' \
    kernel/compat/src/user/syscall/linux/ipc_ops.rs)" -lt 2 ]; then
    echo "one IPC reply ABI bypasses terminal caller handoff custody" >&2
    exit 1
fi
rg -Fq 'set_next_synchronous_pick_hint(receiver_task_id)' \
    kernel/compat/src/user/syscall/linux/ipc_ops.rs || {
    echo "IPC call enqueue bypasses exact receiver handoff custody" >&2
    exit 1
}
rg -Fq 'set_next_process_pick_hint(receiver_process_id)' \
    kernel/compat/src/user/syscall/linux/ipc_ops.rs || {
    echo "process-owned IPC call enqueue bypasses runnable receiver custody" >&2
    exit 1
}
rg -Fq 'synchronous_ipc_handoff_is_fifo_deduplicated_and_fairness_bounded' \
    kernel/ps/src/multitask/scheduler/synchronous_handoff_tests.rs || {
    echo "synchronous IPC handoff lost its executable fairness witness" >&2
    exit 1
}
scheduler_profile=kernel/ps/src/multitask/scheduler/runtime_profile.rs
rg -Fq 'const PROFILE_TOP_TASKS: usize = 4;' "$scheduler_profile" \
    && rg -Fq 'self.runtime_profile_ns.fill(0);' "$scheduler_profile" \
    && rg -Fq 'self.runtime_profile_entry_counts.fill(0);' "$scheduler_profile" \
    && rg -Fq 'kernel-scheduler-entry' "$scheduler_profile" \
    && rg -Fq 'runtime_profile_is_windowed_ranked_and_destructive' "$scheduler_profile" || {
        echo "scheduler runtime attribution lost its bounded destructive snapshot" >&2
        exit 1
    }
if [[ "$(rg -Fc 'publish_scheduler_runtime_profile(runtime_profile);' kernel/ps/src/multitask/irq.rs)" -ne 4 ]] \
    || ! rg -Fq 'ps_api::drain_scheduler_runtime_profile();' kernel/executive/src/boot.rs \
    || ! rg -Fq 'pending_runtime_profile_is_single_slot_release_acquire_custody' "$scheduler_profile"; then
    echo "scheduler runtime attribution left its IRQ-to-housekeeping custody path" >&2
    exit 1
fi
rg -Uq 'let gpu_compositor = Some\(GpuCompositorRuntime::new\([\s\S]{0,900}diag_line\("uiserver: init open_input begin"\)' \
    services/uiserver/src/app/bootstrap.rs || {
    echo "mandatory GPU initialization no longer overlaps serial input/console/surface startup" >&2
    exit 1
}
if [ "$(rg -c 'record_runtime_profile_entry' kernel/ps/src/multitask/irq.rs)" -ne 4 ] \
    || ! rg -Fq 'runtime_profile_entry_causes_are_exact_and_destructive' "$scheduler_profile"; then
    echo "scheduler entry-cause attribution is incomplete or lacks a destructive witness" >&2
    exit 1
fi
reply_recv_kernel=kernel/compat/src/user/syscall/linux/ipc_reply_recv.rs
rg -Fq 'SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER' \
    kernel/compat/src/user/syscall/linux/ipc_ops.rs || {
    echo "fused reply-receive syscall left compat dispatch" >&2
    exit 1
}
for witness in \
    'ipc_reply_recv_shape_valid(&args)' \
    'prepare_recv_with_sender(' \
    'complete_endpoint_reply_for_process(' \
    'recv_with_sender_blocking_prepared(' \
    'IPC_REPLY_RECV_COMMITTED_ERROR_BASE + errno'; do
    rg -Fq "$witness" "$reply_recv_kernel" || {
        echo "fused reply-receive lost phase or custody witness: $witness" >&2
        exit 1
    }
done
rg -Uq 'prepare_recv_with_sender\([\s\S]{0,1800}copy_request_from_user\([\s\S]{0,1800}complete_endpoint_reply_for_process\([\s\S]{0,2600}recv_with_sender_blocking_prepared\(' \
    "$reply_recv_kernel" || {
    echo "fused reply-receive no longer preflights before reply commit and receive" >&2
    exit 1
}
rg -Fq 'pub unsafe fn reply_recv_with_sender(' libs/rustos-svc-runtime/src/ipc.rs \
    && rg -Fq 'pub fn reply_recv_committed_errno(result: i64)' \
        libs/rustos-svc-runtime/src/ipc.rs \
    && rg -Fq 'reply_recv_input_request(' services/inputd/src/service_loop.rs \
    && rg -Fq 'let response = malformed_input_response();' services/inputd/src/service_loop.rs || {
    echo "inputd or service runtime bypasses fused reply-receive phase handling" >&2
    exit 1
}

printf 'performance contract source checks passed\n'
