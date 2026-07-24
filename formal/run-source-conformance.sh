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
root-authority-publication/RootAuthorityPublication|kernel-compat|user::syscall::linux::ipc_ops::tests::root_service_publication_is_boot_owner_sealed_and_epoch_bound
root-authority-publication/RootAuthorityPublication|kernel-ipc-runtime|ipc::tests::process_owned_endpoint_allows_worker_and_rejects_foreign_process
service-call-authority/ServiceCallAuthority|kernel-compat|user::syscall::linux::ipc_ops::tests::service_call_grants_are_exact_epoch_bounded_and_revocable
service-call-authority/ServiceCallAuthority|kernel-ipc-runtime|ipc::tests::process_owned_endpoint_allows_worker_and_rejects_foreign_process
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
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::dvm_ethernet_payload_rejects_bad_checksum_and_fragments
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::dvm_ethernet_payload_accepts_only_bounded_ipv4_or_arp
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::net_contract_has_two_bounded_fixed_rings
dvm-network-ring/DvmNetworkRing|kernel-io-manager|io::dvm_network::tests::control_lease_requires_nonzero_epoch_and_exact_revocation
dvm-network-ring/DvmNetworkRing|kernel-io-manager|io::dvm_network::tests::stale_cleanup_cannot_revoke_replaced_control_lease
dvm-display-readiness/DvmDisplayReadiness|kernel-io-manager|io::dvm_display::tests::damage_bounds_reject_overflow_and_accept_full_frame
dvm-display-readiness/DvmDisplayReadiness|driver-domain-protocol|tests::rejects_unready_or_truncated_regions
dvm-display-readiness/DvmDisplayReadiness|kernel-io-manager|io::dvm_display::tests::exact_predecessor_snapshot_copies_only_declared_damage
dvm-display-readiness/DvmDisplayReadiness|kernel-io-manager|io::dvm_display::tests::missing_gui_dvm_is_unavailable_not_a_fallback_provider
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland::tests::wayland_buffer_layout_rejects_out_of_bounds_and_bad_stride
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland::tests::wayland_buffer_limits_reject_oversized_dimensions
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland::tests::wayland_integer_args_reject_negative_values
wayland-accept-isolation/WaylandAcceptIsolation|uiserver|wayland::tests::wayland_readiness_requires_one_dispatch_before_rearm
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
page-table-lifecycle/PageTableLifecycle|kernel-compat|user::syscall::linux::mm_broker_ops::tests::mapping_range_rejects_noncanonical_and_wrapping_addresses
page-table-lifecycle/PageTableLifecycle|kernel-compat|user::syscall::linux::mm_broker_ops::tests::mapping_cursor_advances_to_the_rounded_region_end
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::validate_user_page_range_rejects_unaligned_or_oob
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::user_page_flags_enforce_wx_and_reject_huge_pages
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::scheduler::tests::rejected_thread_attachment_releases_unpublished_stack
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-hal|arch::idt::handlers::tests::general_exception_bridge_aligns_every_rust_call_boundary
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-compat|user::syscall::tests::only_retired_final_thread_commits_fault_termination
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-ps|multitask::scheduler::tests::retirement_revokes_task_and_process_ipc_authority
futex-waiter-lifecycle/FutexWaiterLifecycle|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::task_identity_cleanup_removes_a_requeued_waiter
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::readiness_generation_requires_a_strict_monotonic_advance
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waiter_capacity_covers_every_scheduler_task_provider_pair
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waitset_provider_authority_maps_to_one_exact_service
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::input_open_description_survives_dup_until_the_final_close
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waiter_removal_before_scheduler_arm_is_detected_by_presence
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::ipc_ops::tests::service_endpoint_epoch_changes_on_every_publication_boundary
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::provider_observations_are_deduplicated_and_keep_the_newest_generation
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::provider_query_timeout_never_exceeds_the_wait_deadline_or_service_cap
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::provider_timeout_never_hides_readiness_found_earlier_in_the_scan
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::provider_revoke_is_reported_per_fd_as_error_and_hup
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
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::vfs_meta::tests::tty_policy_route_requires_an_actual_console_open_description
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::transferred_input_description_keeps_the_waitset_service_reference
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::fork_service_refs_come_from_the_frozen_child_handle_snapshot
service-mutation-recovery/ServiceMutationRecovery|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::remote_vfs_refs_are_local_and_provider_close_is_final_only
service-mutation-recovery/ServiceMutationRecovery|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::foreground_vfs_maintenance_is_one_bounded_replay_turn
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
userspace-wait-set/UserspaceWaitSet|vfsd|tests::epoll_snapshot_rotates_a_persistently_ready_prefix
userspace-wait-set/UserspaceWaitSet|vfsd|tests::provider_restart_updates_epoch_without_duplicating_registration_identity
userspace-wait-set/UserspaceWaitSet|uiserver|wayland::tests::wayland_readiness_requires_one_dispatch_before_rearm
userspace-wait-set/UserspaceWaitSet|netd|packet_provider_state_tests::inet_ingress_publishes_only_socket_state_transitions
userspace-wait-set/UserspaceWaitSet|runtimed|session::tests::console_readiness_generation_advances_only_when_input_becomes_ready
userspace-wait-set/UserspaceWaitSet|runtimed|session::tests::console_close_revokes_readiness_without_resurrecting_the_session
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::pending_slot_reservation_is_global_and_bounded
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::poisoned_deferred_queue_is_drained_for_fail_closed_replies
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::transfer_registry_tests::cancelled_transfer_moves_its_open_description_to_deferred_cleanup
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
msi-vector-lifecycle/MsiVectorLifecycle|kernel-hal|arch::msi::tests::unallocated_vector_has_no_registration_authority
msi-vector-lifecycle/MsiVectorLifecycle|kernel-hal|arch::msi::tests::failed_unpublished_vector_lease_revokes_exact_handler_and_slot
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
ipc-reply-deadline/IpcReplyDeadline|kernel-ipc-runtime|ipc::tests::endpoint_cancel_dequeued_call_invalidates_late_reply
ipc-reply-deadline/IpcReplyDeadline|kernel-ipc-runtime|ipc::tests::endpoint_cancel_rejects_wrong_caller_without_consuming_reply
ipc-reply-deadline/IpcReplyDeadline|rustos-user-abi|tests::performance_limits_are_strictly_layered
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::ipc_ops::tests::public_ipc_calls_share_the_finite_service_deadline
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::ipc_ops::tests::stable_service_endpoint_snapshot_rejects_revoked_owners
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
EOF

jq -s --arg schema rustos-formal-source-conformance-v1 \
    '{schema:$schema,status:"passed",checks:length,models:(map(.model)|unique|length),results:.}' \
    "$records" > "$artifact_dir/summary.json"
printf 'source conformance passed checks=%s models=%s\n' "$checks" \
    "$(jq -r '.models' "$artifact_dir/summary.json")"
