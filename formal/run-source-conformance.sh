#!/usr/bin/env bash
# Run exact source-level witnesses mapped to selected high-risk TLA+ contracts.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${FORMAL_SOURCE_CONFORMANCE_DIR:-$repo_root/build/formal/source-conformance}"
mkdir -p "$artifact_dir"
records="$(mktemp)"
seen="$(mktemp)"
trap 'rm -f "$records" "$seen"' EXIT

# The boot nucleus intentionally uses the hosted x86_64 Cargo target for its
# object format and selects bare-metal behavior with `rustos_boot_image`.
# `target_os = "none"` therefore compiles the host branch into the real image
# and must never gate kernel runtime behavior.
if boot_cfg_misuse="$(rg -n 'target_os[[:space:]]*=[[:space:]]*"none"' kernel --glob '*.rs')" \
    && [[ -n "$boot_cfg_misuse" ]]; then
    printf '%s\n' "$boot_cfg_misuse" >&2
    echo 'kernel boot behavior must use cfg(rustos_boot_image), not target_os = "none"' >&2
    exit 1
fi

# Blocking is one scheduler transition: publishing a public commit-only leaf
# would let callers reintroduce an interruptible commit/yield gap that the
# SchedulerWakeup model deliberately excludes.
if split_block_api="$(rg -n 'pub fn commit_block_current_task\(' kernel/ps/src --glob '*.rs')" \
    && [[ -n "$split_block_api" ]]; then
    printf '%s\n' "$split_block_api" >&2
    echo 'scheduler block commit must not be exported without its atomic reschedule' >&2
    exit 1
fi

# The syscall entry frame remains live across an interruptible scheduler tail.
# Its SYSRET contract must be checked after the last possible resume, not
# before publishing a continuation that may sleep and later be consumed.
syscall_dispatch_body="$(
    sed -n '/^extern "C" fn syscall_dispatch(/,/^fn dispatch_syscall(/p' \
        kernel/compat/src/user/syscall/mod.rs
)"
tail_reschedule_line="$(
    grep -n -m1 'multitask::reschedule_deferred_from_interruptible_syscall();' \
        <<<"$syscall_dispatch_body" | cut -d: -f1
)"
return_validation_line="$(
    grep -n -m1 'let return_abi = validate_syscall_entry_or_terminate(frame);' \
        <<<"$syscall_dispatch_body" | cut -d: -f1
)"
if [[ -z "$tail_reschedule_line" || -z "$return_validation_line" \
    || "$return_validation_line" -le "$tail_reschedule_line" ]]; then
    echo 'syscall SYSRET contract must be validated after the last interruptible tail resume' >&2
    exit 1
fi

# A deadline notification is recovery authority, not proof that the resumed
# syscall completed. Futex waiter-table cleanup must precede timer
# acknowledgement so a stuck resume path remains observable and re-notified.
futex_wait_body="$(
    sed -n '/^fn futex_wait(/,/^fn futex_wait_deadline_tick(/p' \
        kernel/compat/src/user/syscall/linux/service_ops/futex_thread.rs
)"
cleanup_line="$(grep -n -m1 'let still_waiting = take_futex_waiter(task_id);' <<<"$futex_wait_body" | cut -d: -f1)"
timer_ack_line="$(
    grep -n 'crate::arch::rtc::disarm_sleep_waiter(task_id);' <<<"$futex_wait_body" \
        | tail -n1 | cut -d: -f1
)"
if [[ -z "$cleanup_line" || -z "$timer_ack_line" || "$cleanup_line" -ge "$timer_ack_line" ]]; then
    echo 'futex resume cleanup must precede deadline timer acknowledgement' >&2
    exit 1
fi

# Futex wait/wake is scheduler substrate. Supported opcode/flag admission must
# complete locally before waiter/deadline registration; a synchronous syscalld
# round trip here can stall every userspace mutex and can lose an unpark before
# the target has installed its waiter.
futex_impl_body="$(
    sed -n '/^pub fn futex_impl(/,/^fn validate_futex_policy_locally(/p' \
        kernel/compat/src/user/syscall/linux/service_ops/futex_thread.rs
)"
if ! grep -Fq 'validate_futex_policy_locally(op, val3)' <<<"$futex_impl_body"; then
    echo 'futex entry must validate its supported ABI envelope locally' >&2
    exit 1
fi
if grep -Eq 'call_syscalld|SYSCALL_OFFLOAD_OP_LINUX_FUTEX_POLICY' <<<"$futex_impl_body"; then
    echo 'futex scheduler substrate must not synchronously depend on syscalld' >&2
    exit 1
fi
futex_context_body="$(
    sed -n '/^fn current_futex_binding(/,/^fn register_futex_waiter_in(/p' \
        kernel/compat/src/user/syscall/linux/service_ops/futex_thread.rs
)"
if ! grep -Fq 'multitask::current_user_wait_binding()' <<<"$futex_context_body"; then
    echo 'futex key admission must use the scheduler-local current task/MM binding' >&2
    exit 1
fi
if ! grep -Fq 'usermem::current_user_address_space()' <<<"$futex_context_body" \
    || ! grep -Fq 'shared_futex_backing_key(uaddr)' <<<"$futex_context_body"; then
    echo 'shared futex key admission must pin the exact process/VMA backing generation' >&2
    exit 1
fi
if grep -Eq 'with_current_user_process_state(_mut)?' <<<"$futex_context_body"; then
    echo 'futex admission must use its retained generation, not resnapshot current process state' >&2
    exit 1
fi
if ! grep -Fq 'Err(paging::AddressSpaceError::NotMapped) => Ok(private)' <<<"$futex_context_body" \
    || ! grep -Fq 'Some(shared) => [Some(shared), Some(private)]' <<<"$futex_context_body"; then
    echo 'futex keys must fall back for anonymous words and preserve shared cleanup candidates' >&2
    exit 1
fi

stack_layout_body="$(
    sed -n '/^fn release_user_stack_state(/,/^fn prepare_loaded_process_with_launch(/p' \
        kernel/compat/src/user/process/mod.rs
)"
stack_setup_body="$(
    sed -n '/^fn prepare_loaded_process_with_launch(/,/^fn build_process_bootstrap(/p' \
        kernel/compat/src/user/process/mod.rs
)"
if ! grep -Fq 'USER_STACK_INITIAL_COMMIT_PAGES: usize = USER_STACK_RESERVE_PAGES - USER_STACK_GUARD_PAGES' \
        kernel/compat/src/user/process/mod.rs \
    || ! grep -Fq 'let usable_start = reserve_start' <<<"$stack_layout_body" \
    || ! grep -Fq 'release_user_stack_state(reserve_start)' <<<"$stack_setup_body" \
    || ! grep -Fq 'USER_STACK_INITIAL_COMMIT_PAGES,' <<<"$stack_setup_body"; then
    echo 'release user stacks must eagerly map every usable page above one permanent guard' >&2
    exit 1
fi

exec_scheduler_body="$(
    sed -n '/pub(super) fn exec_current_user_process(/,/pub(super) fn linux_thread_snapshot_by_ids(/p' \
        kernel/ps/src/multitask/scheduler.rs
)"
if ! grep -Fq 'exec_slot_admission_valid' <<<"$exec_scheduler_body"; then
    echo 'exec must reject retirement before installing a new address-space root' >&2
    exit 1
fi
if grep -Eq 'retired\[[^]]+\][[:space:]]*=[[:space:]]*false|retirement_cleanup\[[^]]+\][[:space:]]*=[[:space:]]*None|deferred_retire_reasons\[[^]]+\][[:space:]]*=[[:space:]]*None' <<<"$exec_scheduler_body"; then
    echo 'exec must never erase a previously published retirement marker' >&2
    exit 1
fi
exec_transfer_body="$(
    sed -n '/^pub fn replace_for_exec_and_publish(/,/^fn exec_may_replace(/p' \
        kernel/ps/src/multitask/process_table.rs
)"
if ! grep -Fq 'exec_commit_may_transfer(object, reservation)' <<<"$exec_transfer_body" \
    || grep -Fq 'object.exiting' <<<"$exec_transfer_body"; then
    echo 'an installed exec root must transfer ownership through its reservation despite a late exit marker' >&2
    exit 1
fi
exec_state_line="$(grep -n -m1 'state.replace_for_exec(' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_publish_line="$(grep -n -m1 'let published_handle = publish_scheduler()' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_retain_line="$(grep -n -m1 'Some((closed, old_state))' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_drop_line="$(grep -n -m1 'drop(old_state);' <<<"$exec_transfer_body" | cut -d: -f1)"
if [[ -z "$exec_state_line" || -z "$exec_publish_line" || -z "$exec_retain_line" || -z "$exec_drop_line" \
    || "$exec_state_line" -ge "$exec_publish_line" || "$exec_publish_line" -ge "$exec_retain_line" \
    || "$exec_retain_line" -ge "$exec_drop_line" ]]; then
    echo 'exec must commit process generation, publish scheduler generation, then retire the retained old bundle' >&2
    exit 1
fi

reschedule_publish_body="$(
    sed -n '/^fn publish_deferred_reschedule(/,/^}/p' kernel/ps/src/multitask/irq.rs
)"
if ! grep -Fq 'local_request.store(1, Ordering::Release);' <<<"$reschedule_publish_body" \
    || ! grep -Fq 'fanout_pending.store(true, Ordering::Release);' <<<"$reschedule_publish_body" \
    || ! grep -Fq 'super::irq::flush_deferred_reschedule_fanout();' kernel/ps/src/multitask/cpu_local.rs; then
    echo 'lock-held reschedule publication must retain local work and flush remote fanout after raw unlock' >&2
    exit 1
fi

input_drain_body="$(
    sed -n '/^pub(crate) fn service_pending(/,/^pub(crate) fn has_pending_records()/p' \
        kernel/io-manager/src/input/dvm_ring.rs
)"
if ! grep -Fq 'if !try_claim_drain(&DRAIN_IN_PROGRESS)' <<<"$input_drain_body" \
    || ! grep -Fq 'let _drain_guard = DrainGuard;' <<<"$input_drain_body"; then
    echo 'DVM input cursor and reset authority require one exact drain owner' >&2
    exit 1
fi

nmi_body="$(
    sed -n '/fn non_maskable_interrupt_handler(/,/^#\[cfg_attr/p' \
        kernel/hal/src/arch/idt/handlers.rs
)"
if ! grep -Fq 'emergency_exception_marker(2);' <<<"$nmi_body" \
    || grep -Eq 'crate::debug::|hooks::|process_table::|\.lock\(|panic!|println!' <<<"$nmi_body"; then
    echo 'NMI must remain a dedicated-IST lock-free emergency leaf' >&2
    exit 1
fi
if grep -Eq 'stack_frame\.stack_pointer\.as_u64\(\)[[:space:]]+as[[:space:]]+\*const|slice::from_raw_parts' \
        kernel/hal/src/arch/idt/handlers.rs; then
    echo 'user exception diagnostics must never dereference the untrusted saved RSP' >&2
    exit 1
fi

if ! grep -Fq 'IpcTransferTicketWire::decode(bytes)' \
        kernel/compat/src/user/syscall/linux/service_ops/vfs_meta.rs \
    || grep -Eq 'MaybeUninit|assume_init' \
        kernel/compat/src/user/syscall/linux/service_ops/vfs_meta.rs; then
    echo 'SCM_RIGHTS service bytes must use the canonical integer-only ticket parser' >&2
    exit 1
fi

if ! grep -Fq 'cpu_count == 1' kernel/hal/src/arch/clock.rs \
    || ! grep -Fq 'if let Some((base, period_fs, counter)) = hpet' kernel/hal/src/arch/clock.rs; then
    echo 'raw TSC must remain uniprocessor-only and SMP must fail over to validated HPET' >&2
    exit 1
fi

gpu_present_body="$(
    sed -n '/pub(crate) fn present(/,/^    fn capability_for_slot(/p' \
        services/uiserver/src/gpu_runtime.rs
)"
if ! grep -Fq 'let compiler_checkpoint = self.compiler.checkpoint();' <<<"$gpu_present_body" \
    || ! grep -Fq 'self.compiler.restore_rejected_submit(compiler_checkpoint);' <<<"$gpu_present_body" \
    || ! grep -Fq 'self.force_full_snapshot = true;' <<<"$gpu_present_body"; then
    echo 'GPU submit preparation must retain exact rollback and full-replay state' >&2
    exit 1
fi

# Wayland listener and client dispatch are readiness-driven. A nonblocking
# accept/read returning WouldBlock is still a cross-service operation; a fixed
# probe cadence would consume scheduler and VFS/NETD turns while idle.
wayland_accept_body="$(
    sed -n '/^pub(crate) fn start_wayland_acceptor(/,/^#\[cfg(test)\]/p' \
        services/uiserver/src/wayland_accept.rs
)"
if ! grep -Fq 'libc::epoll_wait(' <<<"$wayland_accept_body" \
    || ! grep -Fq 'WAYLAND_ACCEPT_WAIT_TIMEOUT_MS' <<<"$wayland_accept_body" \
    || ! grep -Fq 'worker_pending.fetch_add(1, Ordering::Release);' <<<"$wayland_accept_body" \
    || ! grep -Fq 'ui_wake_sender.signal()' <<<"$wayland_accept_body" \
    || grep -Fq 'thread::sleep' <<<"$wayland_accept_body"; then
    echo 'Wayland accept must block on listener readiness and publish queue ownership before waking UI' >&2
    exit 1
fi
if ! grep -Fq 'wayland_service_required(protocol_input, input.input_events, callback_due)' \
        services/uiserver/src/main.rs; then
    echo 'Wayland client dispatch must require protocol input, server events, or a due callback' >&2
    exit 1
fi

acceptance_body="$(
    sed -n '/^fn exact_contract_enables_profile(/,/^#\[cfg(test)\]/p' \
        services/uiserver/src/acceptance_profile.rs
)"
if ! grep -Fq 'contract && ui_profile == Some(true) && network_exercise.is_some()' <<<"$acceptance_body" \
    || ! grep -Fq 'WATCH_LIMIT' <<<"$acceptance_body" \
    || ! grep -Fq 'require_background_thread_class();' <<<"$acceptance_body" \
    || ! grep -Fq 'read_bounded_config_snapshot(CONTRACT_PATH, CONTRACT_MAX_BYTES)' <<<"$acceptance_body" \
    || grep -Fq 'read_to_string' <<<"$acceptance_body"; then
    echo 'late acceptance profiling must use an exact bounded positioned-read demoted watcher' >&2
    exit 1
fi

runtimed_acceptance_body="$(
    sed -n '/^fn apply_kvm_acceptance_contract(/,/^fn upsert_env(/p' \
        services/runtimed/src/spawn.rs
)"
if ! grep -Fq 'read_bounded_config_snapshot(' <<<"$runtimed_acceptance_body" \
    || ! grep -Fq 'KVM_ACCEPTANCE_CONTRACT_MAX_BYTES' <<<"$runtimed_acceptance_body" \
    || grep -Fq 'read_to_string' <<<"$runtimed_acceptance_body"; then
    echo 'runtimed acceptance injection must use the bounded positioned-read snapshot path' >&2
    exit 1
fi

time_hot_path_body="$(
    sed -n '/^pub fn syscall_linux_nanosleep(/,/^fn rtc_datetime_to_unix_seconds(/p' \
        kernel/compat/src/user/syscall/linux/service_ops/process_time.rs
)"
if ! grep -Fq 'validate_time_hot_path_locally' <<<"$time_hot_path_body"; then
    echo 'clock and sleep hot paths must validate their fixed ABI envelope locally' >&2
    exit 1
fi
if grep -Eq 'request_syscalld|with_current_user_process_state(_mut)?' <<<"$time_hot_path_body"; then
    echo 'clock and sleep hot paths must not depend on process-state or policy-service latency' >&2
    exit 1
fi

checks=0
while IFS='|' read -r model package test_name features; do
    [[ -n "$model" ]] || continue
    if [[ -z "$package" || -z "$test_name" ]]; then
        echo "source conformance row has an empty package or test: $model" >&2
        exit 1
    fi
    witness_key="$model|$package|$test_name|$features"
    if grep -Fqx -- "$witness_key" "$seen"; then
        echo "duplicate source conformance witness: $witness_key" >&2
        exit 1
    fi
    printf '%s\n' "$witness_key" >> "$seen"
    awk -F '\t' -v wanted="$model" '$1 == wanted { found++ } END { exit(found == 1 ? 0 : 1) }' \
        formal/models.tsv || { echo "source conformance model is not registered: $model" >&2; exit 1; }
    cargo_args=(test -q -p "$package")
    if [[ -n "$features" ]]; then
        cargo_args+=(--features "$features")
    fi
    cargo_args+=("$test_name" -- --exact)
    output="$(cargo "${cargo_args[@]}" 2>&1)" || {
        printf '%s\n' "$output" >&2
        echo "source conformance test failed: $model -> $test_name" >&2
        exit 1
    }
    if ! grep -Eq 'test result: ok\. 1 passed; 0 failed' <<< "$output"; then
        printf '%s\n' "$output" >&2
        echo "source conformance test did not execute exactly one witness: $test_name" >&2
        exit 1
    fi
    jq -cn --arg model "$model" --arg package "$package" --arg test "$test_name" \
        --arg features "$features" \
        '{model:$model,package:$package,test:$test,features:$features,status:"passed"}' >> "$records"
    checks=$((checks + 1))
done <<'EOF'
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::process_table::tests::process_address_space_and_exec_exit_are_serialized
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|user::handles::transfer_registry_tests::authority_identity_exhaustion_fails_closed_before_wrap
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::identity_tests::task_identity_exhaustion_never_wraps_to_a_live_id
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::process_table::tests::process_generations_fail_closed_instead_of_aliasing_stale_handles
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-compat|user::syscall::linux::proc_broker_ops::tests::broker_authority_identity_exhaustion_never_wraps
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ipc-runtime|ipc::slab::tests::removed_handle_never_aliases_reused_slot
root-authority-publication/RootAuthorityPublication|kernel-compat|user::syscall::linux::ipc_ops::tests::root_service_publication_is_boot_owner_sealed_and_epoch_bound
root-authority-publication/RootAuthorityPublication|kernel-ipc-runtime|ipc::tests::process_owned_endpoint_allows_worker_and_rejects_foreign_process
service-call-authority/ServiceCallAuthority|kernel-compat|user::syscall::linux::ipc_ops::tests::service_call_grants_are_exact_epoch_bounded_and_revocable
service-call-authority/ServiceCallAuthority|kernel-ipc-runtime|ipc::tests::process_owned_endpoint_allows_worker_and_rejects_foreign_process
service-call-authority/ServiceCallAuthority|nucleus-core|util::lockdep::tests::dependency_walk_detects_transitive_cycle_edge
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::process_table::tests::exiting_process_rejects_new_thread_attachment
early-system-admission/EarlySystemAdmission|boot-protocol|tests::early_system_records_are_fixed_bounded_and_canonical
early-system-admission/EarlySystemAdmission|boot-protocol|tests::rejects_an_all_zero_rng_seed
dual-abi-image-admission/DualAbiImageAdmission|rustos-image-admission|tests::complete_elf64_header_and_program_table_share_the_admission_gate
dual-abi-image-admission/DualAbiImageAdmission|rustos-image-admission|tests::complete_pe64_headers_and_sections_share_the_admission_gate
dual-abi-image-admission/DualAbiImageAdmission|rustos-image-admission|tests::rejects_out_of_range_and_overflowing_regions
dual-abi-image-admission/DualAbiImageAdmission|rustos-image-admission|tests::rejects_writable_executable_region
dvm-input-ring/DvmInputRing|driver-domain-protocol|tests::input_ring_has_separate_cursor_cache_lines_and_rejects_tampering
dvm-input-ring/DvmInputRing|driver-domain-protocol|tests::input_frame_requires_nonzero_provenance_bounds_and_stable_checksum
dvm-input-ring/DvmInputRing|kernel-io-manager|input::dvm_ring::tests::policy_consumer_readiness_requires_transport_and_is_idempotent
dvm-input-ring/DvmInputRing|inputd|dvm_protocol::tests::session_sequence_and_transport_reset_are_service_owned
dvm-input-ring/DvmInputRing|inputd|dvm_protocol::tests::invalid_checksum_and_cross_generation_record_fail_closed
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::dvm_ethernet_payload_rejects_bad_checksum_and_fragments
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::dvm_ethernet_payload_accepts_only_bounded_ipv4_or_arp
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::net_contract_has_two_bounded_fixed_rings
dvm-network-ring/DvmNetworkRing|kernel-io-manager|io::dvm_network::tests::control_lease_requires_nonzero_epoch_and_exact_revocation
dvm-network-ring/DvmNetworkRing|kernel-io-manager|io::dvm_network::tests::stale_cleanup_cannot_revoke_replaced_control_lease
dvm-network-ring/DvmNetworkRing|netd|dvm_session_policy_tests::netd_session_policy_is_exact_idempotent_and_stale_safe
dvm-network-ring/DvmNetworkRing|rootd|tests::inputd_lookup_authority_is_only_the_netd_lifecycle_handoff|host-test
dvm-display-readiness/DvmDisplayReadiness|kernel-io-manager|io::dvm_display::tests::damage_bounds_reject_overflow_and_accept_full_frame
dvm-display-readiness/DvmDisplayReadiness|driver-domain-protocol|tests::rejects_unready_or_truncated_regions
dvm-display-readiness/DvmDisplayReadiness|kernel-io-manager|io::dvm_display::tests::exact_predecessor_snapshot_copies_only_declared_damage
dvm-display-readiness/DvmDisplayReadiness|kernel-io-manager|io::dvm_display::tests::missing_gui_dvm_is_unavailable_not_a_fallback_provider
dvm-display-readiness/DvmDisplayReadiness|uiserver|gpu_runtime::tests::snapshot_damage_keeps_partial_patch_for_exact_slot_predecessor
dvm-display-readiness/DvmDisplayReadiness|uiserver|gpu_runtime::tests::dvm_gpu_admission_waits_without_hiding_behind_software
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland::tests::wayland_buffer_layout_rejects_out_of_bounds_and_bad_stride
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland::tests::wayland_buffer_limits_reject_oversized_dimensions
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland::tests::wayland_integer_args_reject_negative_values
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland::tests::wayland_readiness_requires_one_dispatch_before_rearm
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland_accept::tests::wayland_accept_uses_blocking_readiness_not_probe_cadence
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|main_loop_tests::wayland_dispatch_requires_protocol_input_server_events_or_due_callback
wayland-frame-pacing/WaylandFramePacing|wayclick|damage_tests::cursor_damage_unions_old_and_new_positions_without_full_surface_copy
wayland-frame-pacing/WaylandFramePacing|wayclick|damage_tests::cursor_damage_is_clipped_and_state_changes_force_full_damage
boot-storage-handoff/BootStorageHandoff|rustos-hostd|storage::tests::aperture_epochs_are_clean_monotonic_and_revocable
boot-storage-handoff/BootStorageHandoff|rustos-hostd|storage::tests::idle_validation_covers_every_partition_of_the_whole_device
boot-storage-handoff/BootStorageHandoff|rustos-hostd|runtime::tests::storage_evidence_read_only_mode_must_match_the_signed_aperture
boot-storage-handoff/BootStorageHandoff|rustos-hostd|runtime::tests::storage_supervision_binds_the_exact_signed_epoch_identity
boot-storage-handoff/BootStorageHandoff|rustos-hostd|runtime::tests::runtime_record_rejects_pid_reuse_inputs_and_unknown_keys
boot-storage-handoff/BootStorageHandoff|rustos-hostd|runtime::tests::qmp_powerdown_negotiates_capabilities_before_shutdown
boot-storage-handoff/BootStorageHandoff|xtask|kvm::tests::storage_only_gate_is_independent_of_gpu_and_enables_block_proof
dvm-block-transport/DvmBlockTransport|driver-domain-protocol|block_transport_tests::block_requests_are_address_free_epoch_bound_and_range_checked
dvm-block-transport/DvmBlockTransport|driver-domain-protocol|block_transport_tests::block_completion_binds_request_and_explicit_durability
dvm-control-endpoint/DvmControlEndpoint|rustos-driver-domain-host|tests::control_secret_and_proof_bind_each_session
dvm-control-endpoint/DvmControlEndpoint|rustos-driver-domain-host|tests::control_messages_reject_duplicate_fields
dvm-control-endpoint/DvmControlEndpoint|rustos-driver-domain-host|tests::control_endpoint_is_a_secret_derived_private_port
dvm-block-transport/DvmBlockTransport|kernel-io-manager|io::dvm_block::tests::request_and_completion_bind_exact_slot_epoch_and_durability
dvm-block-transport/DvmBlockTransport|kernel-io-manager|io::dvm_block::tests::stale_completion_revokes_the_transport
dvm-block-transport/DvmBlockTransport|kernel-io-manager|io::dvm_block::tests::revoked_transport_accepts_only_a_signed_newer_epoch
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::startup_not_ready_is_sleepable_not_a_fault_event
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::fixed_nonblock_ivshmem_topology_is_negative_cached_only_after_enumeration
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::readiness_may_arrive_once_but_cannot_be_withdrawn
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::readiness_publication_is_conditional_and_non_mutating_on_mismatch
dvm-block-startup/DvmBlockStartup|storaged|block::tests::startup_wait_slice_is_bounded_and_nonzero
dvm-block-startup/DvmBlockStartup|storaged|block::tests::generation_mismatch_is_stale_not_a_fallback
dvm-block-startup/DvmBlockStartup|storaged|tests::dvm_block_e2e_marker_names_the_complete_authority_path
deferred-process-activation/DeferredProcessActivation|kernel-compat|user::syscall::linux::proc_broker_ops::tests::deferred_activation_authority_is_exact_one_shot_and_nontransferable
loader-request-authority/LoaderRequestAuthority|rustos-user-abi|syscall::syscall_tests::privileged_loader_operations_have_an_explicit_service_role_matrix
loader-request-authority/LoaderRequestAuthority|initd|tests::init_identity_is_published_before_any_loader_request_and_is_marked_requestless
loader-request-authority/LoaderRequestAuthority|kernel-compat|user::syscall::linux::proc_broker_ops::tests::loader_commit_revalidates_live_requester_role_before_consuming_authority
remote-file-mapping/RemoteFileMapping|rustos-user-abi|syscall::syscall_tests::statx_offload_messages_fit_inline_ipc_v1
remote-file-mapping/RemoteFileMapping|vfsd|tests::early_system_reads_chunk_larger_vfs_buffers_to_the_broker_bound
remote-file-mapping/RemoteFileMapping|kernel-compat|user::syscall::linux::proc_broker_ops::tests::truncated_file_mapping_never_commits_zero_filled_tail
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-ps|multitask::scheduler::tests::syscall_user_simd_snapshot_is_disjoint_from_scheduler_continuation
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::sysret_validation_follows_last_interruptible_resume
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::sysret_contract_rejects_forbidden_rflags
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::syscall_entry_preserves_xmm_before_any_rust_dispatch
syscall-scheduler-continuation/SyscallSchedulerContinuation|kernel-ps|multitask::scheduler::tests::scheduler_block_arm_is_exact_race_safe_and_terminally_revoked
syscall-scheduler-continuation/SyscallSchedulerContinuation|kernel-ps|multitask::scheduler::tests::raced_wake_never_validates_a_consumed_current_frame
syscall-scheduler-continuation/SyscallSchedulerContinuation|kernel-compat|user::syscall::tests::sysret_validation_follows_last_interruptible_resume
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::acpi::tests::hpet_gas_requires_memory_qword_zero_offset_and_aligned_range
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::rtc::tests::sleep_deadline_uses_monotonic_ticks_with_ceil_and_saturation
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::rtc::tests::sleep_waiter_update_expiry_and_cancel_preserve_exact_task_ownership
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::rtc::tests::sleep_waiter_clockevent_collision_is_nonblocking_and_retryable
clocksource-deadline/ClocksourceDeadline|kernel-compat|user::syscall::linux::service_ops::process_time::tests::time_hot_path_admission_is_local_and_complete
clocksource-deadline/ClocksourceDeadline|kernel-ps|multitask::scheduler::tests::scheduler_block_arm_is_exact_race_safe_and_terminally_revoked
cpu-topology-admission/CpuTopologyAdmission|kernel-hal|arch::acpi::tests::madt_cpu_topology_is_dense_unique_bounded_and_atomic
cpu-topology-admission/CpuTopologyAdmission|kernel-hal|arch::acpi::tests::madt_rejects_truncation_hot_add_only_and_bad_apic_override
cpu-topology-admission/CpuTopologyAdmission|kernel-hal|arch::acpi::tests::madt_normalizes_the_executing_bsp_to_logical_cpu_zero
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-hal|arch::smp::tests::cpu_lifecycle_publication_is_dense_generation_bound_and_ordered
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-hal|arch::smp::tests::cpu_lifecycle_rejects_skipped_state
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-hal|arch::smp::tests::cpu_lifecycle_rejects_stale_generation
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-hal|arch::smp::tests::ap_bootstrap_stacks_are_aligned_and_disjoint
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-hal|arch::gdt::tests::per_cpu_privilege_and_ist_stacks_are_aligned_and_disjoint
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-hal|arch::msi::tests::startup_ipi_sequence_uses_exact_destination_and_vector
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|user::syscall::tests::cpu_local_records_and_bootstrap_stacks_are_aligned_and_disjoint
cpu-online-lifecycle/CpuOnlineLifecycle|nucleus-core|util::lockdep::tests::dense_apic_identity_map_does_not_index_by_raw_apic_id
cpu-online-lifecycle/CpuOnlineLifecycle|nucleus-core|ap_trampoline::tests::mailbox_layout_and_startup_vector_are_exact
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-mm|memory::phys::tests::fixed_range_claim_is_atomic_exact_and_not_reallocatable
smp-reschedule-ipi/SmpRescheduleIpi|kernel-hal|arch::msi::tests::fixed_reschedule_ipi_uses_exact_destination_and_private_vector
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::cpu_local::tests::current_task_ownership_ignores_offline_slots_and_is_cpu_distinct
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::irq::tests::remote_reschedule_flags_are_cpu_isolated_and_coalesce_without_loss
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::irq::tests::reschedule_ipi_gate_retains_locked_work_and_dispatches_only_at_safe_point
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-hal|interrupt_stubs::tests::scheduler_commit_call_aligns_and_restores_incoming_rsp
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::tests::current_task_ownership_ignores_offline_slots_and_is_cpu_distinct
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::irq::tests::reschedule_ipi_gate_retains_locked_work_and_dispatches_only_at_safe_point
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::tests::architectural_restore_is_required_exactly_for_a_task_switch
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::smp::tests::remote_or_transition_owned_task_is_not_schedulable
scheduler-cpu-ownership/SchedulerCpuOwnership|nucleus-core|util::lockdep::tests::tracked_guard_release_requires_same_cpu_apic_and_positive_depth
scheduler-cpu-ownership/SchedulerCpuOwnership|nucleus-core|util::lockdep::tests::pending_acquire_units_cannot_consume_a_held_guard_pin
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-hal|arch::tlb_shootdown::tests::shootdown_targets_every_eligible_cpu_regardless_of_root
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-hal|arch::tlb_shootdown::tests::same_root_activation_preserves_tlb_but_root_change_reloads_cr3
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-hal|arch::tlb_shootdown::tests::reclaim_requires_every_target_to_acknowledge_the_exact_generation
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-mm|memory::address_space::tests::unmap_region_plan_is_complete_before_metadata_commit
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::process_table::tests::exec_seal_rejects_thread_attachment_until_cancel
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::process_table::tests::process_address_space_and_exec_exit_are_serialized
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::smp::tests::remote_retirement_waits_only_for_another_cpus_running_slot
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-hal|arch::tlb_shootdown::tests::reclaim_requires_every_target_to_acknowledge_the_exact_generation
robust-futex-owner-death/RobustFutexOwnerDeath|kernel-mm|memory::address_space::atomic_user::tests::atomic_user_u32_requires_aligned_complete_user_word
robust-futex-owner-death/RobustFutexOwnerDeath|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::robust_owner_death_preserves_waiters_and_rejects_foreign_owner
robust-futex-owner-death/RobustFutexOwnerDeath|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::retired_task_cleanup_is_exact_and_idempotent
per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle|kernel-hal|arch::timer::tests::tsc_deadline_interval_and_catchup_are_strictly_future_bounded
per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle|kernel-hal|arch::smp::tests::cpu_lifecycle_publication_is_dense_generation_bound_and_ordered
per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle|kernel-ps|multitask::irq::tests::syscall_tail_consumes_every_deferred_or_handoff_request_exactly_once
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::tests::scheduler_block_arm_is_exact_race_safe_and_terminally_revoked
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::tests::raced_wake_never_validates_a_consumed_current_frame
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::tests::live_noncurrent_task_must_retain_one_scheduler_state_owner
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::cpu_local::tests::current_task_ownership_ignores_offline_slots_and_is_cpu_distinct
scheduler-wakeup/SchedulerWakeup|kernel-hal|hooks::tests::scheduler_callback_runs_after_hook_registry_read_guard_is_released
scheduler-wakeup/SchedulerWakeup|kernel-compat|user::syscall::linux::broker_ops::input_broker_ops::tests::ingestion_watchdog_is_bounded_below_ring_exhaustion_time
scheduler-wakeup/SchedulerWakeup|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::task_identity_cleanup_removes_a_requeued_waiter
smp-release-admission/SmpReleaseAdmission|xtask|kvm::tests::rustos_smp_topology_is_machine_gated_on_complete_prerequisites
smp-release-admission/SmpReleaseAdmission|xtask|kvm::tests::rustos_smp_runtime_requires_every_requested_cpu_event_class
scheduler-admission/SchedulerAdmission|runtimed|spawn::tests::catalog_weight_cannot_promote_an_untrusted_program
scheduler-admission/SchedulerAdmission|runtimed|spawn::tests::only_the_exact_ui_server_path_receives_system_weight
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::tests::bounded_system_burst_reserves_a_ready_user_turn
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::tests::user_reservation_obeys_vruntime_without_a_wall_clock_bypass
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::tests::fair_locality_is_bounded_by_class_and_vruntime_lag
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::tests::event_wait_handoff_is_fifo_deduplicated_and_burst_bounded
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::tests::overdue_system_continuation_precedes_a_fresh_latency_handoff
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::irq::tests::syscall_tail_consumes_every_deferred_or_handoff_request_exactly_once
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::tests::runtime_profile_distinguishes_switches_roots_and_migrations
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::tests::runtime_profile_lock_totals_and_maxima_are_destructive
scheduler-thread-demotion/SchedulerThreadDemotion|kernel-ps|multitask::scheduler::tests::self_demotion_removes_base_system_class_and_caps_fair_weight
scheduler-thread-demotion/SchedulerThreadDemotion|vfsd|tests::ui_bootstrap_demotion_requires_successful_terminal_snapshot_reply
scheduler-thread-demotion/SchedulerThreadDemotion|loaderd|tests::ui_bootstrap_demotion_is_custodied_until_terminal_reply
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::synchronous_handoff_tests::synchronous_ipc_handoff_is_fifo_deduplicated_and_fairness_bounded
ipc-priority-inheritance/IpcPriorityInheritance|kernel-ps|multitask::scheduler::tests::synchronous_ipc_donation_promotes_and_revokes_a_transitive_user_chain
ipc-priority-queue/IpcPriorityQueue|kernel-ipc-runtime|ipc::tests::receiver_waiter_tests::endpoint_system_calls_bypass_backlog_without_starving_ordinary_lane
pci-bar-discovery/PciBarDiscovery|kernel-hal|arch::pci::tests::mem64_bar_size_uses_the_lowest_implemented_mask_bit
dvm-volume-io/DvmVolumeIo|vfsd|tests::dvm_block_range_rejects_empty_overflow_and_end_overrun
dvm-volume-io/DvmVolumeIo|vfsd|tests::storage_geometry_rejects_provider_overflow_unknown_flags_and_foreign_binding
dvm-volume-io/DvmVolumeIo|storage-fat|tests::fat_disk_rejects_untrusted_or_overflowing_geometry_before_allocation
dvm-volume-io/DvmVolumeIo|storage-fat|tests::malformed_fat_boot_sector_fails_without_mounting
dvm-volume-io/DvmVolumeIo|vfsd|tests::broker_status_preserves_recoverable_storage_failures
dvm-volume-io/DvmVolumeIo|vfsd|tests::transient_metadata_failures_never_enter_the_negative_cache
dvm-volume-io/DvmVolumeIo|rustos-user-abi|syscall::syscall_tests::storaged_bulk_read_response_fills_one_exact_inline_message
dvm-volume-io/DvmVolumeIo|rustos-user-abi|syscall::syscall_tests::storaged_bulk_read_response_binds_the_complete_request_header
dvm-volume-io/DvmVolumeIo|storaged|tests::bulk_read_reuses_read_authority_instead_of_minting_a_new_right
dvm-volume-io/DvmVolumeIo|kernel-io-manager|io::dvm_block::tests::request_and_completion_bind_exact_slot_epoch_and_durability
dvm-volume-io/DvmVolumeIo|kernel-io-manager|io::dvm_block::tests::stale_completion_revokes_the_transport
dvm-volume-io/DvmVolumeIo|kernel-io-manager|io::dvm_block::tests::fault_points_cover_reads_mutations_and_durability
dvm-volume-io/DvmVolumeIo|xtask|kvm::tests::storage_flush_fault_gate_requires_one_exact_fail_rule_and_rejects_success
dvm-read-cache/DvmReadCache|storaged|block::tests::read_ahead_cache_is_generation_and_range_bound
dvm-read-cache/DvmReadCache|storaged|block::tests::read_ahead_cache_set_is_bounded_lru_and_generation_atomic
dvm-read-cache/DvmReadCache|storaged|block::tests::overlapping_read_ahead_windows_replace_instead_of_aliasing
dvm-read-cache/DvmReadCache|storaged|block::tests::read_ahead_plan_pipelines_bounded_transport_windows
dvm-read-cache/DvmReadCache|storaged|block::tests::read_ahead_plan_stops_at_device_end_and_rejects_oversize_requests
dvm-read-cache/DvmReadCache|storaged|block::tests::random_miss_stays_one_window_until_a_contiguous_boundary_miss
page-table-lifecycle/PageTableLifecycle|kernel-compat|user::syscall::linux::mm_broker_ops::tests::mapping_range_rejects_noncanonical_and_wrapping_addresses
page-table-lifecycle/PageTableLifecycle|kernel-compat|user::syscall::linux::mm_broker_ops::tests::mapping_cursor_advances_to_the_rounded_region_end
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::validate_user_page_range_rejects_unaligned_or_oob
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::user_page_flags_enforce_wx_and_reject_huge_pages
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::protection_span_preflight_rejects_a_hole_before_commit
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::unmap_region_plan_is_complete_before_metadata_commit
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::kernel_vm::tests::direct_map_update_bounds_are_aligned_nonempty_and_nonwrapping
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::kernel_vm::tests::kernel_segment_protection_rejects_writable_executable_authority
page-table-lifecycle/PageTableLifecycle|syscalld|mmap_policy::tests::invalid_backing_is_rejected_before_a_fixed_replace_plan_exists
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::usable_region_spans_filter_and_trim_to_direct_map
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::bitmap_allocator_reuses_freed_frames
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::bounded_allocator_stays_under_limit
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::reserve_phys_range_removes_kernel_image_from_free_set
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::fixed_range_claim_is_atomic_exact_and_not_reallocatable
service-heap-lifecycle/ServiceHeapLifecycle|rustos-svc-runtime|allocator::tests::freed_large_allocation_is_reused_without_growth
service-heap-lifecycle/ServiceHeapLifecycle|rustos-svc-runtime|allocator::tests::allocator_honors_large_alignment
service-heap-lifecycle/ServiceHeapLifecycle|rustos-svc-runtime|allocator::tests::adjacent_frees_coalesce_for_a_larger_request
service-heap-lifecycle/ServiceHeapLifecycle|rustos-svc-runtime|allocator::tests::growth_is_page_aligned_and_bounded_by_request
service-heap-lifecycle/ServiceHeapLifecycle|rustos-svc-runtime|allocator::tests::cumulative_transient_traffic_is_bounded_by_peak_live_memory
service-heap-lifecycle/ServiceHeapLifecycle|rustos-svc-runtime|allocator::tests::growth_callback_runs_without_allocator_lock
service-heap-lifecycle/ServiceHeapLifecycle|rustos-svc-runtime|allocator::tests::duplicate_release_is_rejected_without_free_list_overlap
service-heap-lifecycle/ServiceHeapLifecycle|rustos-svc-runtime|allocator::tests::bootstrap_region_is_installed_once
service-heap-lifecycle/ServiceHeapLifecycle|syscalld|vma_policy::tests::next_fit_wraps_cursor_and_reuses_a_freed_gap
service-heap-lifecycle/ServiceHeapLifecycle|xtask|kvm::tests::ui_runtime_health_rejects_allocator_and_core_service_failure_markers
service-heap-lifecycle/ServiceHeapLifecycle|rootd|tests::production_root_installs_reclaiming_heap_before_first_allocation|host-test
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::scheduler::tests::rejected_thread_attachment_releases_unpublished_stack
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|user::sysops::usermem::tests::user_virt_addr_rejects_out_of_range_without_panicking
process-signal-delivery/ProcessSignalDelivery|kernel-ps|multitask::scheduler::tests::process_stop_is_scheduler_wide_and_sigcont_resumes_before_delivery
process-signal-delivery/ProcessSignalDelivery|kernel-ps|multitask::process_table::tests::child_stop_and_continue_status_require_exact_wait_options
sigchld-notification/SigchldNotification|kernel-ps|multitask::scheduler::tests::process_sigchld_prefers_leader_and_retains_exact_coalesced_causes
sigchld-notification/SigchldNotification|rustos-user-abi|syscall::syscall_tests::nocldstop_suppresses_only_nonterminal_child_state_changes
sigchld-notification/SigchldNotification|kernel-compat|user::syscall::linux::support::tests::sigchld_selection_cannot_clear_unselected_or_future_causes
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-hal|arch::idt::handlers::tests::general_exception_bridge_aligns_every_rust_call_boundary
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-compat|user::syscall::tests::only_retired_final_thread_commits_fault_termination
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-ps|multitask::scheduler::tests::retirement_revokes_task_and_process_ipc_authority
futex-waiter-lifecycle/FutexWaiterLifecycle|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::task_identity_cleanup_removes_a_requeued_waiter
futex-waiter-lifecycle/FutexWaiterLifecycle|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::supported_futex_admission_is_local_and_complete
futex-waiter-lifecycle/FutexWaiterLifecycle|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::retired_task_cleanup_is_exact_and_idempotent
futex-waiter-lifecycle/FutexWaiterLifecycle|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::robust_owner_death_preserves_waiters_and_rejects_foreign_owner
futex-waiter-lifecycle/FutexWaiterLifecycle|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::robust_futex_offset_is_checked_before_user_access
futex-waiter-lifecycle/FutexWaiterLifecycle|kernel-ps|multitask::scheduler::tests::retired_user_slot_waits_for_exact_runtime_cleanup_ack
kernel-resource-accounting/KernelResourceAccounting|kernel-ipc-runtime|ipc::tests::process_endpoint_quota_is_bounded_and_returned_on_exit
kernel-resource-accounting/KernelResourceAccounting|kernel-ipc-runtime|ipc::tests::process_shared_region_quota_is_bounded_until_reclaim_completes
kernel-resource-accounting/KernelResourceAccounting|kernel-ps|multitask::process_table::tests::one_process_cannot_consume_the_global_task_table
input-ingestion-worker/InputIngestionWorker|inputd|tests::ingestion_handoff_prevents_hot_reader_mutex_barging
input-ingestion-worker/InputIngestionWorker|inputd|tests::full_dvm_ingest_batch_retries_without_requiring_another_irq
input-ingestion-worker/InputIngestionWorker|inputd|tests::readiness_generation_closes_empty_queue_lost_wake_window
input-ingestion-worker/InputIngestionWorker|inputd|dvm_session_sync::tests::session_authority_sync_never_holds_the_policy_queue_lock
input-ingestion-worker/InputIngestionWorker|inputd|dvm_session_sync::tests::failed_session_authority_sync_resets_without_killing_ring_progress
input-ingestion-worker/InputIngestionWorker|inputd|dvm_session_sync::tests::failed_session_grant_is_retryable_without_losing_following_input
input-ingestion-worker/InputIngestionWorker|inputd|dvm_session_sync::tests::session_authority_retry_deadline_is_bounded
input-ingestion-worker/InputIngestionWorker|kernel-compat|user::syscall::linux::ipc_ops::tests::inputd_owner_exit_withdraws_the_separate_ring_policy_lease
input-ingestion-worker/InputIngestionWorker|kernel-io-manager|input::dvm_ring::tests::policy_consumer_withdrawal_preserves_transport_but_stops_production
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::readiness_generation_requires_a_strict_monotonic_advance
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waiter_capacity_covers_every_scheduler_task_provider_pair
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waitset_provider_authority_maps_to_one_exact_service
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::input_open_description_survives_dup_until_the_final_close
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waiter_removal_before_scheduler_arm_is_detected_by_presence
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::ipc_ops::tests::service_endpoint_epoch_changes_on_every_publication_boundary
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::provider_observations_are_deduplicated_and_keep_the_newest_generation
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::provider_query_timeout_never_exceeds_the_wait_deadline_or_service_cap
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::control::tests::persistent_epoll_mutation_uses_the_interactive_deadline
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::provider_timeout_never_hides_readiness_found_earlier_in_the_scan
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::provider_revoke_is_reported_per_fd_as_error_and_hup
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::transient_vfs_reply_break_is_retried_inside_epoll_wait
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::epoll_snapshot_reads_are_retry_safe
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::epoll_delete_does_not_require_a_live_provider_epoch
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::epoll_ctl_guard_pins_console_across_concurrent_final_close
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::console_output_is_writable_only_while_its_session_is_live
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::empty_nonblocking_console_read_returns_eagain_without_retry
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::temporary_wait_mask_cannot_block_kill_or_stop
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::handles::table::tests::lifecycle_snapshot_is_descriptor_exact_and_filters_cloexec
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::handles::table::tests::standard_descriptors_are_real_unique_open_descriptions
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::handles::table::tests::close_and_dup_reuse_standard_slots_with_one_open_description
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::handles::table::tests::close_cloexec_removes_only_flagged_entries
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::handles::table::tests::console_last_close_ignores_transient_handle_snapshot
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::handles::table::tests::duplicate_exact_replaces_target_and_applies_cloexec_flag
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::handles::transfer_registry_tests::console_token_liveness_tracks_descriptor_references_not_snapshots
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::epoll::tests::descriptor_references_are_explicit_and_transient_clones_do_not_count
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::vfs_meta::tests::tty_policy_route_requires_an_actual_console_open_description
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::transferred_input_description_keeps_the_waitset_service_reference
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::fork_service_refs_come_from_the_frozen_child_handle_snapshot
service-mutation-recovery/ServiceMutationRecovery|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::remote_vfs_refs_are_local_and_provider_close_is_final_only
service-mutation-recovery/ServiceMutationRecovery|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::netd_reference_mutation_owns_the_complete_interactive_deadline
service-mutation-recovery/ServiceMutationRecovery|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::deadline::tests::netd_reference_mutations_use_interactive_control_deadline
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::diagnostics::tests::terminal_failure_diagnostic_has_an_independent_bounded_lane
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::deadline::tests::vfs_timeout_diagnostic_identifies_the_exact_epoll_control_operation
service-mutation-recovery/ServiceMutationRecovery|kernel-ps|user::epoll::tests::descriptor_references_are_explicit_and_transient_clones_do_not_count
service-mutation-recovery/ServiceMutationRecovery|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::housekeeping_vfs_maintenance_is_one_bounded_replay_turn
service-mutation-recovery/ServiceMutationRecovery|kernel-compat|user::syscall::linux::service_ops::poll_epoll::control::tests::persistent_epoll_mutation_uses_the_interactive_deadline
service-mutation-recovery/ServiceMutationRecovery|netd|ref_replay_tests::close_retry_replays_exact_result_and_rejects_operation_alias
service-mutation-recovery/ServiceMutationRecovery|rootd|service_checkpoint::tests::exact_retry_is_idempotent_and_stale_retry_cannot_rollback|host-test
service-mutation-recovery/ServiceMutationRecovery|rootd|service_checkpoint::tests::parent_tombstone_atomically_revokes_children|host-test
service-mutation-recovery/ServiceMutationRecovery|rootd|tests::service_lookup_uses_the_declared_dependency_edge_not_generic_liveness|host-test
service-mutation-recovery/ServiceMutationRecovery|vfsd|tests::checkpoint_wire_rejects_unknown_or_noncanonical_state
vfs-open-description-recovery/VfsOpenDescriptionRecovery|vfsd|tests::open_description_wire_is_one_checkpoint_value_and_strictly_bounded
vfs-open-description-recovery/VfsOpenDescriptionRecovery|vfsd|tests::seek_position_never_wraps_signed_linux_off_t
vfs-open-description-recovery/VfsOpenDescriptionRecovery|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::remote_vfs_refs_are_local_and_provider_close_is_final_only
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::exit_service_refs_come_from_the_exact_closed_handle_set
userspace-wait-set/UserspaceWaitSet|inputd|tests::readiness_generation_closes_empty_queue_lost_wake_window
userspace-wait-set/UserspaceWaitSet|vfsd|tests::epoll_membership_binds_open_description_and_purges_last_close
userspace-wait-set/UserspaceWaitSet|vfsd|tests::epoll_registry_has_one_service_lifetime_until_final_retire
userspace-wait-set/UserspaceWaitSet|vfsd|tests::epoll_snapshot_rotates_a_persistently_ready_prefix
userspace-wait-set/UserspaceWaitSet|vfsd|tests::provider_restart_updates_epoch_without_duplicating_registration_identity
userspace-wait-set/UserspaceWaitSet|uiserver|wayland::tests::wayland_readiness_requires_one_dispatch_before_rearm
userspace-wait-set/UserspaceWaitSet|uiserver|wayland::tests::wayland_readiness_retries_only_transient_transport_failures
userspace-wait-set/UserspaceWaitSet|uiserver|wayland_accept::tests::wayland_accept_uses_blocking_readiness_not_probe_cadence
userspace-wait-set/UserspaceWaitSet|uiserver|input_loop::tests::input_reader_uses_blocking_epoll_readiness_not_probe_cadence
userspace-wait-set/UserspaceWaitSet|netd|packet_provider_state_tests::inet_ingress_publishes_only_socket_state_transitions
userspace-wait-set/UserspaceWaitSet|runtimed|session::tests::console_readiness_generation_advances_only_when_input_becomes_ready
userspace-wait-set/UserspaceWaitSet|runtimed|session::tests::console_close_revokes_readiness_without_resurrecting_the_session
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::pending_slot_reservation_is_global_and_bounded
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::poisoned_deferred_queue_is_drained_for_fail_closed_replies
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::local_poll_wait_budget_matches_readiness_service_cap
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::transfer_registry_tests::cancelled_transfer_moves_its_open_description_to_deferred_cleanup
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::transfer_registry_tests::opaque_transfer_ticket_is_exact_one_shot_and_nonce_bound
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::table::tests::receive_reservations_are_invisible_and_publish_atomically
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::table::tests::cancelled_receive_reservation_is_reusable
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::table::tests::stale_reservation_cannot_cancel_or_commit_after_exec_boundary
ipc-handle-transfer/IpcHandleTransfer|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::transferred_input_description_keeps_the_waitset_service_reference
process-signal-delivery/ProcessSignalDelivery|kernel-compat|user::syscall::linux::support::tests::signal_selection_revalidates_pending_mask_and_uncatchable_policy
process-signal-delivery/ProcessSignalDelivery|kernel-compat|user::syscall::linux::support::tests::restored_signal_mask_cannot_block_kill_or_stop
process-signal-delivery/ProcessSignalDelivery|kernel-compat|user::syscall::linux::process_termination_tests::x86_user_faults_have_linux_wait_signal_status
process-signal-delivery/ProcessSignalDelivery|kernel-compat|user::syscall::tests::only_retired_final_thread_commits_fault_termination
process-signal-delivery/ProcessSignalDelivery|kernel-executive|hal_hooks::tests::linux_fault_policy_is_not_applied_to_windows_abi
memfd-seal-lifecycle/MemfdSealLifecycle|kernel-ps|user::memfd::tests::memfd_seals_reject_growth_and_mapping_counter_overflow
dvm-gpu-compositor/DvmGpuCompositor|driver-domain-protocol|tests::gpu_render_contract_is_fixed_bounded_and_address_free
dvm-gpu-compositor/DvmGpuCompositor|driver-domain-protocol|tests::gpu_batch_admission_binds_one_atlas_to_a_physical_pool_slot
dvm-gpu-compositor/DvmGpuCompositor|driver-domain-protocol|tests::gpu_timeline_requires_prime_and_acquire_and_retires_outputs_in_fence_order
dvm-gpu-compositor/DvmGpuCompositor|driver-domain-protocol|tests::gpu_timeline_is_monotonic_bounded_and_reset_by_epoch
dvm-gpu-compositor/DvmGpuCompositor|uiserver|gpu_scene::tests::scene_compiler_normalizes_atlas_subrect_and_rejects_escape
dvm-gpu-compositor/DvmGpuCompositor|uiserver|gpu_runtime::tests::slot_reconstruction_budget_rejects_atlas_amplification
dvm-gpu-compositor/DvmGpuCompositor|uiserver|gpu_runtime::tests::frame_deadline_skips_missed_slots_without_drift_or_burst
dvm-gpu-compositor/DvmGpuCompositor|uiserver|gpu_runtime::tests::completion_timeout_separates_activation_from_steady_state
dvm-gpu-admission/DvmGpuAdmission|uiserver|gpu_runtime::tests::completion_timeout_separates_activation_from_steady_state
msi-vector-lifecycle/MsiVectorLifecycle|kernel-hal|arch::msi::tests::unallocated_vector_has_no_registration_authority
msi-vector-lifecycle/MsiVectorLifecycle|kernel-hal|arch::msi::tests::failed_unpublished_vector_lease_revokes_exact_handler_and_slot
msi-vector-lifecycle/MsiVectorLifecycle|kernel-hal|arch::msi::tests::committed_vector_remains_revocable_until_permanent_publication
acpi-table-admission/AcpiTableAdmission|kernel-hal|arch::acpi::tests::root_sdt_requires_exact_signature_width_and_entry_alignment
acpi-table-admission/AcpiTableAdmission|kernel-hal|arch::acpi::tests::mcfg_admission_is_atomic_bounded_aligned_and_nonoverlapping
acpi-table-admission/AcpiTableAdmission|kernel-hal|arch::acpi::tests::ecam_region_range_and_config_address_are_checked_end_to_end
acpi-table-admission/AcpiTableAdmission|kernel-hal|arch::acpi::tests::hpet_gas_requires_memory_qword_zero_offset_and_aligned_range
persistent-mutation-admission/PersistentMutationAdmission|vfsd|tests::persistent_mutation_admission_remains_read_only
deferred-start/DeferredStart|runtimed|spawn::tests::failed_spawn_cleanup_accepts_only_exact_retirement_or_esrch
deferred-start/DeferredStart|initd|tests::failed_service_cleanup_accepts_only_exact_retirement_or_esrch
ipc-endpoint-ownership/IpcEndpointOwnership|kernel-ps|user::handles::table::tests::dynamic_install_never_exceeds_descriptor_ceiling
ipc-endpoint-ownership/IpcEndpointOwnership|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::vfs_response_envelope_rejects_oversized_payload_before_slice_use
ipc-endpoint-ownership/IpcEndpointOwnership|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::descriptor_exhaustion_is_not_reported_as_a_bad_source_fd
ipc-endpoint-ownership/IpcEndpointOwnership|kernel-ps|multitask::scheduler::tests::retirement_revokes_task_and_process_ipc_authority
ipc-endpoint-ownership/IpcEndpointOwnership|kernel-ps|multitask::process_table::tests::leader_thread_retirement_does_not_mark_live_process_exited
endpoint-receiver-wakeup/EndpointReceiverWakeup|kernel-ipc-runtime|ipc::tests::receiver_waiter_tests::endpoint_pending_message_does_not_publish_stale_receiver_waiter
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|multitask::scheduler::tests::retirement_revokes_task_and_process_ipc_authority
proc-broker-session/ProcBrokerSession|kernel-compat|user::syscall::linux::proc_broker_ops::tests::exited_prepare_owner_cannot_republish_after_cleanup
rootd-restart-backoff/RootdRestartBackoff|rootd|tests::failed_restart_activation_retires_exact_suspended_child|host-test
rootd-restart-backoff/RootdRestartBackoff|rootd|tests::core_readiness_budget_is_bounded_and_resets_only_on_readiness|host-test
rootd-restart-backoff/RootdRestartBackoff|kernel-compat|user::syscall::linux::offload_ops::tests::full_lifecycle_queue_rejects_loss_instead_of_dropping_oldest_exit
rootd-restart-backoff/RootdRestartBackoff|kernel-compat|user::syscall::linux::offload_ops::tests::lifecycle_drain_snapshot_preserves_events_appended_during_copyout
rootd-restart-backoff/RootdRestartBackoff|kernel-compat|user::syscall::linux::offload_ops::tests::lifecycle_fanout_consumers_drain_independently
rootd-restart-backoff/RootdRestartBackoff|kernel-compat|user::syscall::linux::broker_ops::lifecycle_broker_ops::tests::lifecycle_drain_requires_exact_version_zero_reserved_envelope
rootd-bootstrap/RootdBootstrap|rootd|tests::core_readiness_budget_is_bounded_and_resets_only_on_readiness|host-test
service-bootstrap-lifecycle/ServiceBootstrapLifecycle|rootd|tests::raw_entry_aligns_stack_before_calling_rust|host-test
service-bootstrap-lifecycle/ServiceBootstrapLifecycle|rootd|tests::loader_worker_completion_is_same_process_and_exact_state_only|host-test
service-bootstrap-lifecycle/ServiceBootstrapLifecycle|rootd|tests::initd_lookup_authority_includes_every_declared_bootstrap_dependency|host-test
service-bootstrap-lifecycle/ServiceBootstrapLifecycle|kernel-compat|user::syscall::linux::process_termination_tests::single_thread_exit_is_never_invented_from_missing_process_state
service-bootstrap-lifecycle/ServiceBootstrapLifecycle|initd|tests::service_readiness_retries_only_an_unpublished_endpoint
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|bootstrap_barrier::tests::independent_bootstrap_activation_overlaps_only_before_consumer_barriers
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|bootstrap_barrier::tests::dependency_packages_exclude_spawned_but_unadmitted_endpoints
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|bootstrap_barrier::tests::bootstrap_barrier_requires_every_exact_endpoint_admission
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|boot_order::tests::runtimed_bootstrap_does_not_wait_for_storage_dvm_publication
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|tests::endpoint_barrier_wait_is_exact_pid_bound_and_bounded
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::activation_batch_tests::spawn_handoff_is_fifo_deduplicated_and_precedes_ipc_handoff
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::tests::overdue_system_continuation_precedes_a_fresh_latency_handoff
atomic-process-activation-batch/AtomicProcessActivationBatch|initd|activation::tests::activation_batch_is_exact_bounded_and_zero_tailed
atomic-process-activation-batch/AtomicProcessActivationBatch|rustos-user-abi|syscall::activation_batch::tests::requester_identity_is_bound_to_the_kernel_sender
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-compat|user::syscall::linux::proc_broker_ops::activation_batch::tests::activation_batch_keeps_preflight_and_commit_under_registry_lock
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-compat|user::syscall::linux::proc_broker_ops::tests::deferred_activation_authority_is_exact_one_shot_and_nontransferable
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation_batch_tests::spawn_handoff_is_fifo_deduplicated_and_precedes_ipc_handoff
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation_batch_tests::authority_commit_is_checked_while_the_complete_cohort_is_still_suspended
cpu-affinity-observation/CpuAffinityObservation|kernel-hal|arch::smp::tests::online_mask_contains_exact_dense_online_set
cpu-affinity-observation/CpuAffinityObservation|kernel-compat|user::syscall::linux::syscalld_ops::tests::affinity_topology_stamp_is_versioned_exact_and_reserved_zero
cpu-affinity-observation/CpuAffinityObservation|syscalld|affinity_policy::tests::sched_getaffinity_returns_exact_kernel_stamped_task_mask
cpu-affinity-observation/CpuAffinityObservation|syscalld|affinity_policy::tests::sched_getaffinity_rejects_invalid_topology_observations
cpu-affinity-observation/CpuAffinityObservation|kernel-compat|user::syscall::windows::dispatch::tests::windows_topology_stamp_is_versioned_exact_and_reserved_zero
cpu-affinity-observation/CpuAffinityObservation|syscalld|affinity_policy::tests::windows_basic_system_information_uses_exact_kernel_topology_stamp
cpu-affinity-observation/CpuAffinityObservation|syscalld|affinity_policy::tests::windows_basic_information_rejects_class_pointer_and_length_before_publish
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-ps|multitask::scheduler::affinity::tests::task_affinity_snapshot_is_exact_and_online_bounded
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-ps|multitask::scheduler::affinity::tests::linux_thread_affinity_commits_exact_mask_and_previous_value
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-ps|multitask::scheduler::affinity::tests::invalid_affinity_changes_leave_all_authority_unchanged
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-ps|multitask::scheduler::affinity::tests::excluded_running_cpu_requires_remote_reschedule
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-ps|multitask::scheduler::affinity::tests::child_task_inherits_effective_parent_affinity
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-ps|multitask::scheduler::affinity::tests::exec_preserves_task_and_process_affinity
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-ps|multitask::scheduler::affinity::tests::windows_process_affinity_updates_every_live_thread_atomically
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-ps|multitask::scheduler::affinity::tests::windows_thread_affinity_returns_previous_and_rejects_process_escape
task-affinity-lifecycle/TaskAffinityLifecycle|syscalld|affinity_policy::tests::sched_getaffinity_returns_exact_kernel_stamped_task_mask
task-affinity-lifecycle/TaskAffinityLifecycle|syscalld|affinity_policy::tests::windows_affinity_admission_is_handle_exact_and_online_bounded
task-affinity-lifecycle/TaskAffinityLifecycle|syscalld|affinity_policy::tests::windows_process_affinity_query_binds_both_output_pointers_and_process_mask
task-affinity-lifecycle/TaskAffinityLifecycle|kernel-compat|user::syscall::windows::dispatch::tests::windows_current_processor_number_is_exact_and_online_bounded
filesystem-content-integrity/FilesystemContentIntegrity|kernel-io-manager|storage::boot_volume::tests::early_system_lookup_verifies_exact_path_and_payload_digest
post-init-leases/PostInitLeases|rootd|tests::reporter_exit_cascades_and_capability_requires_live_reporter_chain|host-test
post-init-leases/PostInitLeases|rootd|tests::post_init_lease_requires_the_exact_declared_executable_path|host-test
post-init-leases/PostInitLeases|kernel-compat|user::syscall::linux::proc_broker_ops::tests::deferred_activation_authority_is_exact_one_shot_and_nontransferable
zero-trust-service-flow/ZeroTrustServiceFlow|rootd|tests::root_supervisor_requests_require_exact_sender_and_canonical_unused_fields|host-test
endpoint-registry/EndpointRegistry|kernel-ps|multitask::process_table::tests::leader_thread_retirement_does_not_mark_live_process_exited
endpoint-publication/EndpointPublication|kernel-compat|user::syscall::linux::ipc_ops::tests::service_endpoint_epoch_changes_on_every_publication_boundary
runtime-control-rpc/RuntimeControlRpc|runtime-control|tests::successful_response_must_echo_the_request_opcode
runtime-control-rpc/RuntimeControlRpc|runtime-control|tests::malformed_status_and_oversized_snapshot_fail_closed
runtime-control-authority/RuntimeControlAuthority|runtimed|socket::tests::runtime_control_mutations_require_live_uiserver_or_logical_admin
runtime-control-authority/RuntimeControlAuthority|runtimed|socket::tests::partial_background_client_never_busy_waits_the_policy_loop
ipc-reply-deadline/IpcReplyDeadline|kernel-ipc-runtime|ipc::tests::endpoint_cancel_dequeued_call_invalidates_late_reply
ipc-reply-deadline/IpcReplyDeadline|kernel-ipc-runtime|ipc::tests::endpoint_cancel_rejects_wrong_caller_without_consuming_reply
ipc-reply-deadline/IpcReplyDeadline|kernel-ipc-runtime|ipc::tests::retiring_caller_may_consume_the_exact_global_message_capacity
ipc-reply-deadline/IpcReplyDeadline|rustos-user-abi|tests::performance_limits_are_strictly_layered
ipc-reply-deadline/IpcReplyDeadline|rootd|control_drain::tests::root_control_drain_services_a_bounded_ready_burst|host-test
ipc-reply-deadline/IpcReplyDeadline|runtimed|tests::session_control_drain_services_a_bounded_ready_burst
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::ipc_ops::tests::public_ipc_calls_share_the_finite_service_deadline
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::ipc_ops::tests::stable_service_endpoint_snapshot_rejects_revoked_owners
ipc-reply-recv-transaction/IpcReplyRecvTransaction|rustos-user-abi|syscall::ipc_reply_recv::tests::reply_recv_wire_shape_and_error_partition_are_stable
ipc-reply-recv-transaction/IpcReplyRecvTransaction|rustos-svc-runtime|ipc::tests::reply_recv_phase_tag_cannot_alias_linux_errno
ipc-reply-recv-transaction/IpcReplyRecvTransaction|kernel-compat|user::syscall::linux::ipc_ops::ipc_reply_recv::tests::reply_recv_precommit_shape_is_exact_and_versioned
ipc-reply-recv-transaction/IpcReplyRecvTransaction|kernel-compat|user::syscall::linux::ipc_ops::ipc_reply_recv::tests::reply_recv_post_commit_error_is_outside_linux_errno_space
ipc-reply-recv-transaction/IpcReplyRecvTransaction|inputd|service_loop::tests::malformed_dequeued_request_has_terminal_error_reply
ipc-reply-recv-transaction/IpcReplyRecvTransaction|inputd|service_loop::tests::reply_recv_recovery_retries_only_a_proven_live_reply
ipc-reply-recv-transaction/IpcReplyRecvTransaction|loaderd|tests::zero_length_request_is_malformed_not_idle
ipc-reply-recv-transaction/IpcReplyRecvTransaction|loaderd|tests::fused_reply_never_delays_cleanup_or_bootstrap_demotion
ipc-reply-recv-transaction/IpcReplyRecvTransaction|loaderd|tests::reply_recv_recovery_retries_only_a_proven_live_reply
commercial-service-envelope/CommercialServiceEnvelope|rustos-user-abi|syscall::syscall_tests::commercial_request_envelope_rejects_reserved_flags_and_oversized_lengths
commercial-service-envelope/CommercialServiceEnvelope|rustos-user-abi|syscall::syscall_tests::commercial_response_envelope_matches_exact_request_and_bounds_nested_fields
commercial-service-envelope/CommercialServiceEnvelope|kernel-compat|user::syscall::linux::ipc_ops::tests::commercial_response_envelope_is_bound_to_request_and_bounded
zero-trust-service-flow/ZeroTrustServiceFlow|rustos-user-abi|syscall::syscall_tests::service_subject_identity_is_never_a_zero_or_foreign_wildcard
zero-trust-service-flow/ZeroTrustServiceFlow|rustos-user-abi|syscall::syscall_tests::commercial_request_envelope_rejects_reserved_flags_and_oversized_lengths
zero-trust-service-flow/ZeroTrustServiceFlow|rustos-user-abi|syscall::syscall_tests::loader_requester_identity_is_bound_to_the_kernel_sender
zero-trust-service-flow/ZeroTrustServiceFlow|rustos-user-abi|syscall::syscall_tests::commercial_response_envelope_matches_exact_request_and_bounds_nested_fields
zero-trust-service-flow/ZeroTrustServiceFlow|runtimed|session::tests::session_ingress_requires_exact_sender_or_narrow_devmgrd_delegation
entropy-broker-boundary/EntropyBrokerBoundary|boot-protocol|tests::rejects_an_all_zero_rng_seed
entropy-broker-boundary/EntropyBrokerBoundary|boot-random|tests::child_streams_are_derived_from_private_master_output
entropy-broker-boundary/EntropyBrokerBoundary|kernel-compat|user::syscall::linux::broker_ops::entropy_broker_ops::tests::entropy_copyout_is_zero_safe_and_strictly_bounded
devmgrd-sessiond-isolation/DevmgrdSessiondIsolation|runtimed|session::tests::session_ingress_requires_exact_sender_or_narrow_devmgrd_delegation
dma-iommu-isolation/DmaIommuIsolation|rustos-driver-domain-host|tests::launch_plan_requires_the_complete_iommu_group
driver-domain-fleet/DriverDomainFleet|rustos-driver-domain-host|tests::fleet_policy_requires_disjoint_domain_cid_group_and_pci_authority
dual-abi-byte-parser/DualAbiByteParser|rustos-image-admission|tests::complete_elf64_header_and_program_table_share_the_admission_gate
dual-abi-byte-parser/DualAbiByteParser|rustos-image-admission|tests::complete_pe64_headers_and_sections_share_the_admission_gate
dvm-absolute-pointer/DvmAbsolutePointer|driver-domain-protocol|tests::absolute_pointer_frame_is_bounded_and_keeps_position_semantics
dvm-agent-readiness/DvmAgentReadiness|xtask|kvm::tests::dvm_agent_local_readiness_is_process_owned_and_atomic
dvm-amdgpu-supply/DvmAmdgpuSupply|rustos-driver-domain-host|tests::physical_display_assignment_is_bound_to_exact_amdgpu_identity
dvm-atomic-scanout/DvmAtomicScanout|driver-domain-protocol|tests::gpu_timeline_requires_prime_and_acquire_and_retires_outputs_in_fence_order
dvm-commercial-lifecycle/DvmCommercialLifecycle|rustos-hostd|runtime::tests::storage_supervision_binds_the_exact_signed_epoch_identity
dvm-control-relay/DvmControlRelay|rustos-driver-domain-host|tests::relay_epochs_are_monotonic_and_fail_closed_before_reuse
dvm-display-driver-supply/DvmDisplayDriverSupply|rustos-driver-domain-host|tests::display_evidence_is_exact_fresh_and_zero_copy
dvm-display-scheduler/DvmDisplayScheduler|xtask|kvm::tests::dvm_display_relay_has_bounded_authenticated_scheduler_admission
dvm-gpu-admission/DvmGpuAdmission|uiserver|gpu_runtime::tests::dvm_gpu_admission_waits_without_hiding_behind_software
dvm-gpu-atlas-transport/DvmGpuAtlasTransport|driver-domain-protocol|tests::gpu_atlas_transport_separates_immutable_sources_from_completions
dvm-input-revocation/DvmInputRevocation|kernel-io-manager|input::dvm_ring::tests::policy_consumer_withdrawal_preserves_transport_but_stops_production
dvm-network-control/DvmNetworkControl|kernel-io-manager|io::dvm_network::tests::control_lease_requires_nonzero_epoch_and_exact_revocation
exec-ticket/ExecTicket|kernel-compat|user::syscall::linux::proc_broker_ops::tests::deferred_activation_authority_is_exact_one_shot_and_nontransferable
gui-dvm-pixel-authority/GuiDvmPixelAuthority|driver-domain-protocol|tests::gpu_timeline_is_monotonic_bounded_and_reset_by_epoch
gui-dvm-surface/GuiDvmSurface|driver-domain-protocol|tests::gui_surface_control_is_fixed_and_capability_bounded
input-readiness/InputReadiness|inputd|tests::readiness_generation_closes_empty_queue_lost_wake_window
ivshmem-pairing/IvshmemPairing|rustos-driver-domain-host|tests::control_secret_and_proof_bind_each_session
network-payload-session/NetworkPayloadSession|driver-domain-protocol|tests::dvm_ethernet_payload_accepts_only_bounded_ipv4_or_arp
post-init-supervisor-recovery/PostInitSupervisorRecovery|rootd|tests::reporter_exit_cascades_and_capability_requires_live_reporter_chain|host-test
trusted-ui-boundary/TrustedUiBoundary|uiserver|sys::tests::trusted_ui_status_fails_closed_for_every_current_scanout
ui-frame-budget/UiFrameBudget|uiserver|gpu_runtime::tests::frame_deadline_skips_missed_slots_without_drift_or_burst
ui-main-loop-wakeup/UiMainLoopWakeup|uiserver|input_loop::tests::prequeued_wake_never_commits_a_timeout_sleep
ui-main-loop-wakeup/UiMainLoopWakeup|uiserver|input_loop::tests::coalesced_notification_tokens_still_advance_readiness_generation
ui-main-loop-wakeup/UiMainLoopWakeup|kernel-hal|arch::rtc::tests::sleep_waiter_update_expiry_and_cancel_preserve_exact_task_ownership
ui-main-loop-wakeup/UiMainLoopWakeup|wayclick|damage_tests::first_frame_marker_is_the_user_visible_boot_terminal
ui-input-motion/UiInputMotion|uiserver|input_loop::tests::input_reader_batch_coalesces_relative_motion
vfio-release-authorization/VfioReleaseAuthorization|rustos-driver-domain-host|tests::release_authorization_binds_artifacts_policy_and_complete_iommu_group
product-boot/ProductBoot|vfsd|tests::executable_snapshot_marker_binds_path_and_exact_length
product-boot/ProductBoot|rootd|tests::core_readiness_budget_is_bounded_and_resets_only_on_readiness|host-test
product-boot/ProductBoot|kernel-io-manager|input::dvm_ring::tests::policy_consumer_readiness_requires_transport_and_is_idempotent
product-boot/ProductBoot|uiserver|gpu_runtime::tests::dvm_gpu_admission_waits_without_hiding_behind_software
product-boot/ProductBoot|storaged|tests::dvm_block_e2e_marker_names_the_complete_authority_path
product-boot/ProductBoot|uiserver|gpu_runtime::tests::frame_deadline_skips_missed_slots_without_drift_or_burst
ui-frame-budget/UiFrameBudget|wayclick|damage_tests::first_frame_marker_is_the_user_visible_boot_terminal
input-ingestion-worker/InputIngestionWorker|kernel-io-manager|input::dvm_ring::tests::policy_consumer_readiness_requires_transport_and_is_idempotent
dvm-input-ring/DvmInputRing|kernel-io-manager|input::dvm_ring::tests::concurrent_broker_callers_have_exactly_one_drain_owner
product-boot/ProductBoot|kernel-compat|user::syscall::linux::debug_ops::product_milestone_tests::product_milestones_are_a_closed_fixed_name_vocabulary
user-stack-growth/UserStackGrowth|kernel-compat|user::process::tests::release_stack_maps_every_usable_page_above_one_guard
user-stack-growth/UserStackGrowth|kernel-ps|multitask::process_table::tests::exception_process_state_try_lock_never_waits_on_contention
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::process_table::tests::process_address_space_and_exec_exit_are_serialized
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::process_table::tests::exec_seal_rejects_thread_attachment_until_cancel
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::irq::tests::lock_held_reschedule_publication_retains_local_and_fanout_work
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::tests::ready_scanner_never_reads_a_frame_owned_by_any_cpu
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::clock::tests::raw_tsc_global_clock_is_rejected_until_smp_offsets_are_admitted
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-hal|arch::gdt::tests::per_cpu_privilege_and_ist_stacks_are_aligned_and_disjoint
ipc-handle-transfer/IpcHandleTransfer|rustos-user-abi|tests::ipc_transfer_ticket_wire_is_canonical_and_rejects_zero_authority
robust-futex-owner-death/RobustFutexOwnerDeath|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::kernel_generated_wake_uses_shared_then_exact_private_fallback
gpu-submit-transaction/GpuSubmitTransaction|uiserver|gpu_scene::tests::rejected_transport_submit_restores_exact_compiler_timeline
acceptance-profile-publication/AcceptanceProfilePublication|uiserver|acceptance_profile::tests::late_acceptance_profile_requires_the_exact_complete_contract
EOF

jq -s --arg schema rustos-formal-source-conformance-v1 \
    '{schema:$schema,status:"passed",checks:length,models:(map(.model)|unique|length),results:.}' \
    "$records" > "$artifact_dir/summary.json"
printf 'source conformance passed checks=%s models=%s\n' "$checks" \
    "$(jq -r '.models' "$artifact_dir/summary.json")"
