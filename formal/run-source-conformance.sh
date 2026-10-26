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

# `println_emergency` deliberately bypasses the shared debug-output lock so a
# nested panic can still report why the machine stopped. Letting any ordinary
# diagnostic use it can splice bytes into a mandatory SMP milestone and turn a
# completed CPU transition into false-negative (or substring-based false
# positive) KVM evidence. All healthy-runtime writers must take the bounded
# serialized line path.
while IFS= read -r emergency_source; do
    case "$emergency_source" in
        # `debug/tests.rs` is included only by `#[cfg(test)] mod tests`; its
        # function-pointer binding proves the panic-only API remains distinct
        # and cannot become a healthy-runtime writer.
        kernel/nucleus-core/src/debug/mod.rs|kernel/nucleus-core/src/debug/tests.rs|kernel/executive/src/boot.rs) ;;
        *)
            printf '%s\n' "$emergency_source" >&2
            echo 'non-panic debug output must not bypass milestone serialization' >&2
            exit 1
            ;;
    esac
done < <(rg -l 'println_emergency' kernel --glob '*.rs' | sort)

# Blocking is one scheduler transition: publishing a public commit-only leaf
# would let callers reintroduce an interruptible commit/yield gap that the
# SchedulerWakeup model deliberately excludes.
if split_block_api="$(rg -n 'pub fn commit_block_current_task\(' kernel/ps/src --glob '*.rs')" \
    && [[ -n "$split_block_api" ]]; then
    printf '%s\n' "$split_block_api" >&2
    echo 'scheduler block commit must not be exported without its atomic reschedule' >&2
    exit 1
fi

# The syscall entry frame remains live across interruptible kernel work and can
# become the scheduler's outgoing user context at the common tail. Its return
# contract must be checked after claiming that tail and before publishing the
# normalized context to the scheduler.
syscall_dispatch_body="$(
    sed -n '/^extern "C" fn syscall_dispatch(/,/^fn dispatch_syscall(/p' \
        kernel/compat/src/user/syscall/mod.rs
)"
tail_reschedule_line="$(
    grep -n -m1 'multitask::claim_interruptible_syscall_reschedule();' \
        <<<"$syscall_dispatch_body" | cut -d: -f1
)"
return_validation_line="$(
    grep -n -m1 'let return_abi = if register_ipc {' \
        <<<"$syscall_dispatch_body" | cut -d: -f1
)"
context_publication_line="$(
    grep -n -m1 'kernel_hal::api::dispatch_saved_syscall_user_context(' \
        <<<"$syscall_dispatch_body" | cut -d: -f1
)"
if [[ -z "$tail_reschedule_line" || -z "$return_validation_line" \
    || -z "$context_publication_line" \
    || "$return_validation_line" -le "$tail_reschedule_line" \
    || "$context_publication_line" -le "$return_validation_line" ]]; then
    echo 'scheduled syscall return must be validated after claim and before context publication' >&2
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
    sed -n '/^pub(in crate::multitask::scheduler) fn commit(/,/^fn install_current_architecture(/p' \
        kernel/ps/src/multitask/scheduler/exec_transaction/publication.rs
)"
exec_retire_line="$(grep -n -m1 'if lifecycle.retired' <<<"$exec_scheduler_body" | cut -d: -f1)"
exec_root_line="$(grep -n -m1 'runqueue::address_space::replace(slot, lease.root)' <<<"$exec_scheduler_body" | cut -d: -f1)"
if [[ -z "$exec_retire_line" || -z "$exec_root_line" || "$exec_retire_line" -ge "$exec_root_line" ]] \
    || ! grep -Fq 'retirement_cleanup::deferred(slot).is_some()' <<<"$exec_scheduler_body" \
    || ! grep -Fq 'retirement_cleanup::pending(slot)' <<<"$exec_scheduler_body" \
    || ! grep -Fq 'retirement_cleanup::kernel_pending(slot)' <<<"$exec_scheduler_body" \
    || ! grep -Fq 'retirement_cleanup::reason(slot).is_some()' <<<"$exec_scheduler_body"; then
    echo 'exec must reject retirement before installing a new address-space root' >&2
    exit 1
fi
if grep -Eq 'retired\[[^]]+\][[:space:]]*=[[:space:]]*false|retirement_cleanup\[[^]]+\][[:space:]]*=[[:space:]]*None|deferred_retire_reasons\[[^]]+\][[:space:]]*=[[:space:]]*None' <<<"$exec_scheduler_body"; then
    echo 'exec must never erase a previously published retirement marker' >&2
    exit 1
fi
exec_stage_body="$(
    sed -n '/^pub fn stage_exec_state(/,/^pub(super) fn finalize_exec_state(/p' \
        kernel/ps/src/multitask/process_table.rs
)"
if ! grep -Fq 'exec_commit_may_transfer(object, reservation)' <<<"$exec_stage_body" \
    || ! grep -Fq 'object.exec_state_staged = true;' <<<"$exec_stage_body"; then
    echo 'exec staging must retain exact reservation and hide the new process state' >&2
    exit 1
fi
exec_transfer_body="$(
    sed -n '/^pub fn exec_current_user_process(/,/^pub fn exec_user_process_by_pid(/p' \
        kernel/ps/src/multitask/current/exec.rs
)"
exec_image_line="$(grep -n -m1 'exec_transaction::with_census_image(' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_state_line="$(grep -n -m1 'process_table::stage_exec_state(' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_publish_line="$(grep -n -m1 'lease.commit_image(&mut bootstrap, None)' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_retain_line="$(grep -n -m1 'process_table::finalize_exec_state(' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_drop_line="$(grep -n -m1 'old_state.retire_execution_state();' <<<"$exec_transfer_body" | cut -d: -f1)"
if [[ -z "$exec_image_line" || -z "$exec_state_line" || -z "$exec_publish_line" || -z "$exec_retain_line" || -z "$exec_drop_line" \
    || "$exec_image_line" -ge "$exec_state_line" || "$exec_state_line" -ge "$exec_publish_line" || "$exec_publish_line" -ge "$exec_retain_line" \
    || "$exec_retain_line" -ge "$exec_drop_line" ]]; then
    echo 'exec must acquire image custody before staging, publish root with receipt, finalize generation, then retire the old bundle' >&2
    exit 1
fi

# Exception-side table installation has no allocator lock or heap admission.
# Its explicit descriptor must therefore be claimed before the directory-entry
# CAS, linked only by the winner, and cleared before a losing supplied frame
# returns to `phys`.  Retirement drains that root ledger only after the
# cross-CPU barrier and before reusing the table frames.
fault_table_body="$(sed -n '/^pub fn ensure_current_fault_tables_at(/,$p' \
    kernel/mm/src/memory/address_space/pager_fault_mapping.rs)"
fault_claim_line="$(grep -n -m1 'claim_lazy_table_record' <<<"$fault_table_body" | cut -d: -f1)"
fault_entry_line="$(grep -n -m1 'publish_user_table_entry' <<<"$fault_table_body" | cut -d: -f1)"
fault_publish_line="$(grep -n -m1 'publish_lazy_table_record' <<<"$fault_table_body" | cut -d: -f1)"
fault_loser_body="$(sed -n '/Ok((child, false)) => {/,/^                    }/p' <<<"$fault_table_body")"
fault_error_body="$(sed -n '/Err(error) => {/,/^                    }/p' <<<"$fault_table_body")"
retirement_body="$(sed -n '/^pub(super) fn reclaim_root(/,/^}/p' \
    kernel/mm/src/memory/address_space/retirement.rs)"
retire_barrier_line="$(grep -n -m1 'begin_address_space_retirement' <<<"$retirement_body" | cut -d: -f1)"
retire_drain_line="$(grep -n -m1 'drain_lazy_table_records' <<<"$retirement_body" | cut -d: -f1)"
retire_free_line="$(grep -n -m1 'explicit_tables.into_iter' <<<"$retirement_body" | cut -d: -f1)"
if [[ -z "$fault_claim_line" || -z "$fault_entry_line" || -z "$fault_publish_line" \
    || "$fault_claim_line" -ge "$fault_entry_line" || "$fault_entry_line" -ge "$fault_publish_line" \
    || -z "$retire_barrier_line" || -z "$retire_drain_line" || -z "$retire_free_line" \
    || "$retire_barrier_line" -ge "$retire_drain_line" || "$retire_drain_line" -ge "$retire_free_line" ]]; then
    echo 'lazy-table ledger must claim before CAS, resolve each loser, and drain after retirement' >&2
    exit 1
fi
if ! grep -Fq 'cancel_lazy_table_record' <<<"$fault_loser_body" \
    || ! grep -Fq 'release(frame);' <<<"$fault_loser_body" \
    || ! grep -Fq 'cancel_lazy_table_record' <<<"$fault_error_body" \
    || ! grep -Fq 'release(frame);' <<<"$fault_error_body"; then
    echo 'lazy-table ledger must claim before CAS, resolve each loser, and drain after retirement' >&2
    exit 1
fi

# Normal-time mapping must not retain intermediate table frames in the data
# leaf ledger. It uses the identical claim -> directory-CAS -> publish/cancel
# protocol and directory witness as the IRQ-off installer, so retirement has
# one root-owned table descriptor list to reconcile.
normal_table_body="$(sed -n '/^fn ensure_next_table(/,/^}/p' \
    kernel/mm/src/memory/address_space.rs)"
normal_claim_line="$(grep -n -m1 'claim_lazy_table_record' <<<"$normal_table_body" | cut -d: -f1)"
normal_entry_line="$(grep -n -m1 'publish_user_table_entry' <<<"$normal_table_body" | cut -d: -f1)"
normal_publish_line="$(grep -n -m1 'publish_lazy_table_record' <<<"$normal_table_body" | cut -d: -f1)"
if [[ -z "$normal_claim_line" || -z "$normal_entry_line" || -z "$normal_publish_line" \
    || "$normal_claim_line" -ge "$normal_entry_line" || "$normal_entry_line" -ge "$normal_publish_line" \
    || ! "$normal_table_body" == *'ROOT_OWNED_USER_TABLE'* \
    || ! "$normal_table_body" == *'cancel_lazy_table_record'* \
    || "$normal_table_body" == *'track_owned_frame'* ]]; then
    echo 'normal table installation must use the unified frame-descriptor claim/CAS protocol' >&2
    exit 1
fi

reschedule_publish_body="$(
    sed -n '/^fn set_local_deferred_reschedule(/,/^}/p' kernel/ps/src/multitask/irq.rs
)"
if ! grep -Fq 'super::reschedule_observation::publish_request(' <<<"$reschedule_publish_body" \
    || grep -Eq 'send_reschedule_ipi|send_target_reschedule_ipi|fanout' <<<"$reschedule_publish_body" \
    || ! grep -Fq 'super::irq::flush_deferred_target_reschedules();' kernel/ps/src/multitask/cpu_local.rs; then
    echo 'local reschedule publication must stay local and exact target custody must flush after raw unlock' >&2
    exit 1
fi

# Busy/idle migration now retains an ORIGINAL physical reader through exact
# Local CAS and mailbox. Loaded-opportunity cadence and Current/Transition
# exclusion remain obligations of the actual independent implementation.
python3 formal/check-dispatch-source-contracts.py
python3 formal/test-dispatch-namespace-contract.py

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

# Every production uiserver thread must cross the role-typed spawn boundary.
# The sole direct Builder call is the wrapper implementation itself; allowing
# a second call would restore inherited scheduling authority implicitly.
ui_direct_spawns="$(
    rg -n 'thread::spawn|thread::Builder::new' services/uiserver/src --glob '*.rs' || true
)"
if [[ "$(wc -l <<<"$ui_direct_spawns")" -ne 1 ]] \
    || ! grep -Fq 'services/uiserver/src/sys.rs:' <<<"$ui_direct_spawns" \
    || ! grep -Fq 'thread::Builder::new().name(name.into()).spawn' <<<"$ui_direct_spawns"; then
    echo 'uiserver production threads must use the single role-typed spawn boundary' >&2
    exit 1
fi

acceptance_body="$(
    sed -n '/^fn exact_contract_enables_profile(/,/^#\[cfg(test)\]/p' \
        services/uiserver/src/acceptance_profile.rs
)"
if ! grep -Fq 'contract && ui_profile == Some(true) && network_exercise.is_some()' <<<"$acceptance_body" \
    || ! grep -Fq 'WATCH_LIMIT' <<<"$acceptance_body" \
    || ! grep -Fq 'spawn_ui_thread(UiThreadRole::Background' <<<"$acceptance_body" \
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

vfs_receive_body="$(
    sed -n '/^fn serve(/,/^fn reply_executable_snapshot(/p' services/vfsd/src/main.rs | sed '$d'
)"
vfs_snapshot_worker_body="$(
    cat services/vfsd/src/snapshot_worker.rs
)"
if ! grep -Fq 'state.resolve_executable_snapshot_request(*request)' <<<"$vfs_receive_body" \
    || ! grep -Fq 'enqueue_executable_snapshot(reply_cap, sender_pid, sender_tid, resolved)' <<<"$vfs_receive_body" \
    || grep -Fq 'enqueue_executable_snapshot(reply_cap, sender_pid, sender_tid, *request)' <<<"$vfs_receive_body" \
    || grep -Fq 'reply_executable_snapshot(' <<<"$vfs_receive_body" \
    || ! grep -Fq 'reply_executable_snapshot(' <<<"$vfs_snapshot_worker_body" \
    || ! grep -Fq 'SnapshotWorkerAdmission' services/vfsd/src/snapshot_worker.rs \
    || grep -Eq 'sched_yield|SYS_SCHED_YIELD' services/vfsd/src/main.rs services/vfsd/src/snapshot_worker.rs; then
    echo 'vfsd receive owner must hand bulk snapshots to one bounded exact-owner worker slot' >&2
    exit 1
fi

proc_prepare_authority="kernel/compat/src/user/syscall/linux/proc_broker_ops/authority.rs"
proc_prepare_body="$(
    sed -n '/^pub(super) fn syscall_linux_rustos_proc_prepare_broker(/,/^pub(super) fn syscall_linux_rustos_proc_map_file_broker(/p' \
        kernel/compat/src/user/syscall/linux/proc_broker_ops.rs | sed '$d'
)"
if grep -Eq 'call_service_endpoint|IPC_SERVICE_PROCD|procd_process_prepare_policy' \
        "$proc_prepare_authority" \
    || grep -Eq 'call_service_endpoint|IPC_SERVICE_PROCD|procd_process_prepare_policy' \
        <<<"$proc_prepare_body" \
    || ! grep -Fq 'let owner_pid = loader_pid;' <<<"$proc_prepare_body"; then
    echo 'loader prepare must consume PROCESS_LOADER authority without a procd reverse IPC cycle' >&2
    exit 1
fi

# VFS geometry admission is an authority equality check.  Treating the signed
# flag word as an allowed-bit mask would admit flags=0 (writable) when the
# caller requires the exact READ_ONLY authority.
vfs_geometry_body="$(
    sed -n '/^pub fn admit_dvm_block_geometry(/,/^pub fn storage_error_from_linux_status/p' \
        services/vfsd/src/lib.rs
)"
if ! grep -Fq '|| info.flags != expected_flags' <<<"$vfs_geometry_body" \
    || grep -Fq 'info.flags & !expected_flags' <<<"$vfs_geometry_body"; then
    echo 'vfsd DVM geometry must require the exact expected READ_ONLY flag authority' >&2
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
# REQ-CLK-013. The tick domain numbers the deadline wheel; it is not the
# resolution an instant is reported at. Deriving the reported timespec from
# `ticks()` rounded every `CLOCK_MONOTONIC` answer to 1/1024 s, which is coarser
# than most intervals ring 3 measures and made its own timings unreadable.
monotonic_report_body="$(
    sed -n '/^pub fn monotonic_timespec(/,/^}/p' \
        kernel/compat/src/user/syscall/linux/service_ops/process_time.rs
)"
if ! grep -Fq 'crate::arch::rtc::monotonic_nanos()' <<<"$monotonic_report_body" \
    || grep -Eq 'ticks\(\)|ticks_per_second' <<<"$monotonic_report_body"; then
    echo 'the reported monotonic instant must come from the clocksource, not the tick domain' >&2
    exit 1
fi
# The calendar chip is read under `without_interrupts` and answers with
# one-second resolution; under virtualization each of its port accesses is an
# exit. Serving every `CLOCK_REALTIME` query from it is what the latched epoch
# exists to prevent.
realtime_report_body="$(
    sed -n '/^pub fn realtime_timespec(/,/^}/p' \
        kernel/compat/src/user/syscall/linux/service_ops/process_time.rs
)"
if grep -Fq 'crate::arch::rtc::now()' <<<"$realtime_report_body"; then
    echo 'CLOCK_REALTIME must not read the calendar chip on the query path' >&2
    exit 1
fi
# Asking for the identity of the task already running on the asking CPU is a
# per-CPU question. Answering it through the exclusive global scheduler lock
# cost 7,197 acquisitions/s at 8 vCPU from this one site while two sibling
# functions in the same file already read the published seqlock record. An
# unavailable publication now requires an independently joined Current/context
# owner fallback; it cannot recover authority from a legacy directory.
current_snapshot_body="$(
    sed -n '/^pub fn current_user_snapshot(/,/^}/p' kernel/ps/src/multitask/current.rs
)"
if ! grep -Fq 'published_current_identity()' <<<"$current_snapshot_body" \
    || ! grep -Fq 'owned_current_user_binding()' <<<"$current_snapshot_body"; then
    echo 'current-task snapshot must retain published identity and exact owned Current fallback' >&2
    exit 1
fi
python3 - <<'PY'
from pathlib import Path
source = Path('kernel/ps/src/multitask/current.rs').read_text()
body = source.split('pub fn current_user_snapshot()', 1)[1].split('\n}', 1)[0]
assert body.index('published_current_identity()') < body.index('owned_current_user_binding()')
owned = Path('kernel/ps/src/multitask/current/owner_bindings.rs').read_text()
assert 'super::super::scheduler::stack_resources::context::current_user_binding()' in owned
PY
# Call authorization already reads and validates the exact service publication
# epoch. The reply-deadline path must reuse the service identity captured by
# that transaction. Looking the endpoint up again both adds a second registry
# read to every synchronous call and can classify a replacement publication
# rather than the publication that granted authority.
ipc_authorize_body="$(
    sed -n '/^fn authorize_ipc_call_for_process(/,/^fn /p' \
        kernel/compat/src/user/syscall/linux/ipc_ops.rs
)"
ipc_fast_call_body="$(
    sed -n '/^fn try_fast_ipc_call_bytes[<(]/,/^fn /p' \
        kernel/compat/src/user/syscall/linux/ipc_ops.rs
)"
ipc_deadline_body="$(
    sed -n '/^fn service_reply_deadline_tick_for_authorized_service(/,/^fn /p' \
        kernel/compat/src/user/syscall/linux/ipc_ops.rs
)"
if ! grep -Fq 'Ok(Some(index as u64))' <<<"$ipc_authorize_body" \
    || ! grep -Fq '.map(|_| None)' <<<"$ipc_authorize_body" \
    || ! grep -Fq 'authorize_ipc_call_for_process(endpoint, retained_mm.process_id())?' kernel/compat/src/user/syscall/linux/ipc_ops.rs \
    || ! grep -Fq 'authorize_ipc_call_for_process(raw, identity.process_id())?' kernel/compat/src/user/syscall/linux/ipc_ops/registers.rs \
    || ! grep -Fq 'service_reply_deadline_tick_for_authorized_service(' <<<"$ipc_fast_call_body" \
    || ! grep -Fq 'authorized_service_id,' <<<"$ipc_fast_call_body" \
    || ! grep -Fq 'service_reply_deadline_tick_for_service(service_id, request, timeout_ms)' <<<"$ipc_deadline_body" \
    || grep -Fq 'service_endpoint(' <<<"$ipc_deadline_body" \
    || rg -Fq 'service_reply_deadline_tick_for_endpoint' \
        kernel/compat/src/user/syscall/linux/ipc_ops.rs; then
    echo 'synchronous IPC must derive its reply deadline from the exact service identity captured by authorization without a second publication lookup' >&2
    exit 1
fi
# A synchronous receiver is already blocked and the caller will block in the
# same transaction. If all dispatch constraints admit the caller's CPU, moving
# receiver custody there avoids a remote wake plus its reverse reply IPI. The
# previous CPU remains the exact fallback for affinity, budget, idle-owner, or
# lifecycle rejection; the ordinary cross-CPU publication must stay present.
ipc_fast_handoff_body="$(cat kernel/ps/src/multitask/scheduler/ipc_publication.rs)"
if ! grep -Fq 'let previous = receiver_owner.cpu.unwrap_or_else' <<<"$ipc_fast_handoff_body" \
    || ! grep -Fq 'runqueue::affinity_payload::last_cpu(binding.slot())' <<<"$ipc_fast_handoff_body" \
    || ! grep -Fq 'if eligible(&binding, cpu) { cpu } else { previous }' <<<"$ipc_fast_handoff_body" \
    || ! grep -Fq 'if target != cpu && !eligible(&binding, target)' <<<"$ipc_fast_handoff_body" \
    || ! grep -Fq 'runqueue::claim_blocked_handoff(slot, expected, target, local)' <<<"$ipc_fast_handoff_body" \
    || ! grep -Fq 'runqueue::publish_handoff_mailbox(slot, original,' <<<"$ipc_fast_handoff_body" \
    || ! grep -Fq 'request_target_reschedule(target)' <<<"$ipc_fast_handoff_body"; then
    echo 'synchronous IPC must prefer an eligible caller CPU while retaining the admitted previous-CPU remote wake fallback' >&2
    exit 1
fi
# A bounded receive can leave the block by timer instead of by sender. A sender
# pops the receiver it wakes; a timer does not, so a task that resumes on its
# deadline is still published as this endpoint's next receiver. Leaving it there
# sends the endpoint's next request to a task that is not waiting for it and
# wakes nobody else - a lost wakeup that looks like a hung service. The exact
# endpoint withdrawal and the timer must therefore stay together; using the
# task-retirement whole-slab scan here is both unnecessary and a tail-latency
# regression.
bounded_recv_body="$(
    sed -n '/^fn recv_with_sender_blocking_prepared_inner[<(]/,/^}/p' \
        kernel/compat/src/user/syscall/linux/ipc_ops.rs
)"
if ! grep -Fq 'arm_sleep_waiter_until_tick' <<<"$bounded_recv_body" \
    || ! grep -Fq 'remove_endpoint_waiter_for_task(endpoint, task_id)' <<<"$bounded_recv_body"; then
    echo 'the bounded endpoint receive must arm a deadline waiter and must withdraw its endpoint receiver waiter when it resumes' >&2
    exit 1
fi
# The supervisor idle wait is the whole point of the bounded receive: a flat
# sleep here puts the entire idle interval in front of every synchronous caller,
# which is how a 10 ms sleep became the dominant term in keystroke latency.
runtimed_idle_body="$(
    sed -n '/^        let idle_delay =$/,/^    }$/p' \
        services/runtimed/src/main.rs
)"
if ! grep -Fq 'service_session_endpoint(session_endpoint, &mut state, Some(idle_delay))' \
    <<<"$runtimed_idle_body"; then
    echo 'the runtimed idle wait must block on the session endpoint with a bounded deadline, not on a timer' >&2
    exit 1
fi
# A parked console read is answered by the same loop that idles. If its deadline
# is not folded into the idle budget, the broker can sleep straight through it
# whenever it has nothing else to do, and the read outlives the budget it
# promised its caller.
runtimed_delay_body="$(
    sed -n '/^pub(super) fn next_idle_delay(/,/^}/p' services/runtimed/src/spawn.rs
)"
if ! grep -Fq 'earliest_console_read_deadline()' <<<"$runtimed_delay_body" \
    || ! grep -Fq '.min(parked_read_delay)' <<<"$runtimed_delay_body" \
    || ! grep -Fq 'earliest_watch_deadline' <<<"$runtimed_delay_body" \
    || ! grep -Fq '.min(parked_watch_delay)' <<<"$runtimed_delay_body"; then
    echo 'the runtimed idle budget must include every reply deadline it is holding' >&2
    exit 1
fi
# devmgrd answers the ioctl routing question with a pure function of the request
# number - no fd, pid, credentials, or session reach it - so re-asking per call
# spends a broker round trip deriving a constant. It must be memoized, and the
# memo must be keyed by devmgrd's registration epoch, because the table is
# compiled into that service. Authorization is the opposite and must never join
# it: it reads the caller, and the forwarded path pays it every call.
route_memo_body="$(
    sed -n '/^pub fn ioctl_route_via_devmgrd(/,/^}/p' \
        kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs
)"
if ! grep -Fq 'memoized_ioctl_route(request_number, epoch)' <<<"$route_memo_body" \
    || ! grep -Fq 'ipc_ops::service_endpoint_epoch(linux_abi::IPC_SERVICE_DEVMGRD)' \
        <<<"$route_memo_body"; then
    echo 'the devmgrd ioctl route must be memoized against its registration epoch, not re-asked per call' >&2
    exit 1
fi
if rg -Fq 'memoized_ioctl_route' \
    <<<"$(sed -n '/^pub fn ioctl_device_via_devmgrd(/,/^}/p' \
        kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs)"; then
    echo 'ioctl authorization reads the caller and must never be served from the routing memo' >&2
    exit 1
fi

# The console has two observers with opposite interests: a shell waiting to read
# its own session, and a compositor waiting for anything it draws to change.
# Only the first had a readiness subject, so the second ran a timer. Every
# mutation the compositor can see must move the one token it waits on, and the
# edge must be published where the token moves rather than at each call site -
# a forgotten publication is silent and strands a waiter that stopped polling.
graph_advance_body="$(
    sed -n '/^    fn advance_graph_generation(/,/^    }/p' services/runtimed/src/session.rs
)"
if ! grep -Fq 'publish_console_graph_readiness(self.output_generation)' <<<"$graph_advance_body"; then
    echo 'the console graph token must publish its wait-set edge where the token moves' >&2
    exit 1
fi
for mutation in create_session remove_session write_to_session handle_input_event; do
    body="$(sed -n "/^    \(pub(crate) \)\?fn ${mutation}(/,/^    }/p" services/runtimed/src/session.rs)"
    if ! grep -Fq 'advance_graph_generation()' <<<"$body"; then
        echo "console mutation ${mutation} must advance the graph token the compositor waits on" >&2
        exit 1
    fi
done
# A parked graph wait is answered by the broker pass, so the pass has to visit
# it, and its deadline has to be inside the idle budget or the broker can sleep
# through a promise it made.
if ! rg -Fq 'session::service_console_graph_waiters(&mut state)' services/runtimed/src/main.rs; then
    echo 'the runtimed loop must answer parked console-graph waits every pass' >&2
    exit 1
fi
if ! grep -Fq '.min(parked_graph_delay)' <<<"$runtimed_delay_body"; then
    echo 'the runtimed idle budget must include the soonest parked console-graph deadline' >&2
    exit 1
fi
# The compositor must wait on that edge instead of a timer, and the wait must
# not be routed through devmgrd: devmgrd serves from a single loop, so a held
# reply there stalls every unrelated device ioctl for the whole park.
console_refresh_body="$(
    sed -n '/^pub(crate) fn start_console_refresh_worker(/,/^}/p' services/uiserver/src/app/runtime.rs
)"
if ! grep -Fq 'console_wait_graph(' <<<"$console_refresh_body" \
    || grep -Fq 'wait_for_edge' <<<"$console_refresh_body"; then
    echo 'the uiserver console refresh must block on the console graph edge, not on an interval' >&2
    exit 1
fi
if ! rg -Fq 'if request_number == rustos_user_abi::console::CONSOLE_IOCTL_WAIT_GRAPH {' \
    kernel/compat/src/user/syscall/linux/service_ops/vfs_meta.rs; then
    echo 'the console graph wait must take the direct broker rail, never the devmgrd forward' >&2
    exit 1
fi

# A change token that changes when it is read tells every caller that everything
# changed, every time. The console snapshot once raised its reported generation
# to a counter the handler itself incremented, so uiserver's refresh worker
# re-fetched every session's output on every pass, woke the render loop each
# time, and never reached its idle wait - a spin whose traffic lands on the same
# endpoint carrying shell keystrokes.
session_graph_body="$(
    sed -n '/^fn handle_session_graph_request(/,/^fn /p' services/runtimed/src/session.rs
)"
if ! grep -Fq 'let generation = state.session_runtime.output_generation();' \
    <<<"$session_graph_body" \
    || grep -Eq 'fetch_add|SESSION_GRAPH_GENERATION' <<<"$session_graph_body"; then
    echo 'reporting the console generation must not advance it; observing a change token may not be a change' >&2
    exit 1
fi
# The runtimed control socket is deliberately non-blocking so a dead peer cannot
# hang connect. That makes a short read/write spin unless it waits on the
# descriptor: uiserver and sessiond issue these RPCs on their own hot paths, and
# a yield loop here burns a core against the very service it is waiting for.
runtime_rpc_body="$(
    sed -n '/^fn write_all_retry_until(/,/^}/p;/^fn read_exact_retry_until(/,/^}/p' \
        libs/runtime-control/src/lib.rs
)"
if grep -Fq 'thread::yield_now()' <<<"$runtime_rpc_body" \
    || ! grep -Fq 'wait_for_socket_ready(stream, deadline, libc::POLLOUT)' <<<"$runtime_rpc_body" \
    || ! grep -Fq 'wait_for_socket_ready(stream, deadline, libc::POLLIN)' <<<"$runtime_rpc_body"; then
    echo 'the runtimed control RPC must wait on the socket for readiness, never spin yielding' >&2
    exit 1
fi
# The control wire had two definitions - one per crate - and nothing checked
# them against each other. A silent divergence in an opcode or a frame field is
# not a build error, so the server must consume the client's protocol module
# rather than redeclare any part of it.
if runtimed_protocol_copy="$(rg -n '^pub\(crate\) const (PROTOCOL_VERSION|OP_[A-Z_]+|LAUNCH_TARGET_[A-Z_]+|TERMINATE_TARGET_[A-Z_]+|READY_COMPONENT_[A-Z_]+|MAX_REQUEST_PATH_BYTES|MAX_RUNTIME_PROGRAMS)\b|^pub\(crate\) struct Runtime(Request|Response)\b' services/runtimed/src)" \
    && [[ -n "$runtimed_protocol_copy" ]]; then
    printf '%s\n' "$runtimed_protocol_copy" >&2
    echo 'the runtimed control protocol must have one definition, not a private server copy' >&2
    exit 1
fi
if ! rg -Fq 'pub(crate) use runtime_control::protocol::{' services/runtimed/src/main.rs; then
    echo 'runtimed must consume the shared runtime-control protocol module' >&2
    exit 1
fi
# The change edge is defined by the bytes a reply would carry, not by a counter
# the server bumps at each mutation site. A counter that misses one site parks a
# watcher through the very change it asked about, and nothing fails until a
# taskbar silently stops updating.
watch_service_body="$(
    sed -n '/^pub(super) fn service_running_program_watchers(/,/^}/p' \
        services/runtimed/src/socket.rs
)"
if ! grep -Fq 'let programs = running_program_snapshot(state);' <<<"$watch_service_body" \
    || ! grep -Fq 'let digest = running_programs_digest(&programs);' <<<"$watch_service_body" \
    || ! grep -Fq 'now < watcher.deadline' <<<"$watch_service_body"; then
    echo 'a parked watch must be judged by the digest of the reply itself and re-armed at its deadline' >&2
    exit 1
fi
# Watchers are answered from the broker pass, so the pass has to visit them.
# Without this call a held reply is owed forever and the callers that stopped
# polling never hear about a launch or an exit.
if ! rg -Fq 'socket::service_running_program_watchers(&mut runtime_connections, &state)' \
    services/runtimed/src/main.rs; then
    echo 'the runtimed loop must publish the running-set edge to parked watchers every pass' >&2
    exit 1
fi
# uiserver and sessiond were the two permanent pollers of that set. Each paid a
# full snapshot round trip per interval, forever, to be told nothing changed.
uiserver_sync_body="$(
    sed -n '/^fn runtime_sync_worker(/,/^}/p' services/uiserver/src/runtime_sync.rs
)"
if ! grep -Fq 'runtime.watch_running_programs(observed_digest, RUNTIME_WATCH_WAIT)' \
    <<<"$uiserver_sync_body" \
    || grep -Fq 'snapshot_running_programs()' <<<"$uiserver_sync_body"; then
    echo 'the uiserver runtime sync must park on the running-set change edge, not poll a snapshot' >&2
    exit 1
fi
sessiond_observe_body="$(
    sed -n '/^fn observe_running_programs(/,/^}/p' services/sessiond/src/main.rs
)"
if ! grep -Fq 'runtime.watch_running_programs(*observed_digest, idle_watch_wait(retry_after))' \
    <<<"$sessiond_observe_body" \
    || ! grep -Fq 'if launch_pending {' <<<"$sessiond_observe_body"; then
    echo 'an idle sessiond must park on the running-set change edge and keep the tight cadence only while a launch is pending' >&2
    exit 1
fi
# Rootd has the same lifecycle/control split as runtimed: its bounded drain may
# not block, but the post-init idle turn must wake on a control message or the
# shared supervisor budget. Keeping the old timer-only helper here would put a
# full interval in front of every restart-policy caller.
rootd_source="$(cat services/rootd/src/main.rs)"
if ! grep -Fq 'SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED' <<<"$rootd_source" \
    || ! grep -Fq 'ROOTD_SUPERVISOR_IDLE_POLL_MS' <<<"$rootd_source" \
    || grep -Eq '^fn supervisor_idle\(\)' <<<"$rootd_source"; then
    echo 'rootd post-init supervision must use bounded message-or-timeout receive, not timer-only supervisor idle' >&2
    exit 1
fi

join_line="$(rg -n 'pthread_join\(threads\[index\]' apps/smpqual/smpqual.c | head -n 1 | cut -d: -f1)"
complete_line="$(rg -n 'emit_milestone\(PRODUCT_MILESTONE_SMPQUAL_COMPLETE' apps/smpqual/smpqual.c | head -n 1 | cut -d: -f1)"
smp_bind_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_smp_qualification_bind/,/^pub(super) fn prepare_smp_qualification_activation/p' kernel/compat/src/user/syscall/linux/smp_qualification_ops.rs)"
smp_activation_body="$(sed -n '/^pub(super) fn prepare_smp_qualification_activation/,/^pub(super) fn abort_smp_qualification_activation/p' kernel/compat/src/user/syscall/linux/smp_qualification_ops.rs)"
smp_phase_body="$(sed -n '/^pub(super) fn admit_smp_qualification_milestone/,/^pub(super) fn revoke_smp_qualification_for_process/p' kernel/compat/src/user/syscall/linux/smp_qualification_ops.rs)"
proc_activate_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_proc_activate_broker/,/^pub(super) fn syscall_linux_rustos_proc_validate_deferred_spawn_broker/p' kernel/compat/src/user/syscall/linux/proc_broker_ops.rs)"
smp_ring3_option_body="$(sed -n '/^    if options.smp_ring3_qualification {/,/^    } else if options.smp_evidence_cohort.is_some() {/p' tools/xtask/src/kvm/smoke_args.rs)"
ordinary_catalog_body="$(sed -n '/^pub(super) fn load_launch_catalog/,/^\/\/\/ Reconciles the private/p' services/runtimed/src/catalog.rs)"
qualification_reconcile_body="$(sed -n '/^pub(super) fn reconcile_kvm_smp_qualification_into_state/,/^fn defer_qualification_catalog_retry/p' services/runtimed/src/catalog.rs)"
qualification_candidate_body="$(sed -n '/^fn qualification_catalog_candidate/,/^fn validate_ui_bootstrap_metadata/p' services/runtimed/src/catalog.rs)"
dvm_read_only_header_body="$(sed -n '/^fn dvm_read_only_block_header(/,/^fn create_dvm_block_aperture/p' tools/xtask/src/kvm/layout/block_transport.rs)"
dvm_snapshot_sync_body="$(sed -n '/^fn sync_private_dvm_block_snapshot(/,/^fn create_dvm_block_aperture/p' tools/xtask/src/kvm/layout/block_transport.rs)"
dvm_block_create_body="$(sed -n '/^fn create_dvm_block_aperture(/,/^fn rotate_dvm_block_epoch/p' tools/xtask/src/kvm/layout/block_transport.rs)"
dvm_block_rotate_body="$(sed -n '/^fn rotate_dvm_block_epoch(/,$p' tools/xtask/src/kvm/layout/block_transport.rs)"
dvm_virtual_storage_body="$(sed -n '/^fn append_dvm_virtual_storage(/,/^fn append_dvm_display_pixels/p' tools/xtask/src/kvm/guest.rs)"
dvm_recovery_harness_body="$(sed -n '/^impl RecoveryHarness/,/^fn wait_for_rustos_reboot_recovery/p' tools/xtask/src/kvm/guest.rs)"
dvm_restart_recovery_body="$(sed -n '/^fn wait_for_dvm_restart_recovery(/,$p' tools/xtask/src/kvm/guest.rs)"
dvm_ready_generation_body="$(sed -n '/^fn dvm_block_header_matches_ready_generation(/,/^fn verify_dvm_block_ready(/p' tools/xtask/src/kvm/layout.rs)"
dvm_block_ready_body="$(sed -n '/^fn verify_dvm_block_ready_generation(/,/^fn render_private_acceptance_contract/p' tools/xtask/src/kvm/layout.rs)"
dvm_block_revoke_body="$(sed -n '/^impl DvmBlockState {/,/^#\[cfg(not(test))\]/p' kernel/io-manager/src/io/dvm_block/revoke.rs)"
dvm_block_revoke_report_body="$(sed -n '/^fn report_transport_revoke(/,/^#\[cfg(test)\]/p' kernel/io-manager/src/io/dvm_block/revoke.rs)"
dvm_block_revoke_reason_body="$(sed -n '/^pub(super) enum DvmBlockRevokeReason {/,/^    #\[cfg(test)\]/p' kernel/io-manager/src/io/dvm_block/revoke.rs)"
dvm_block_flush_read_body="$(sed -n '/^fn valid_flush_completion_keeps_transport_live_for_first_64kib_read(/,/^#\[test\]/p' kernel/io-manager/src/io/dvm_block/tests.rs)"
dvm_block_cache_source="$(cat kernel/io-manager/src/io/dvm_block.rs)"
dvm_input_cache_source="$(cat kernel/io-manager/src/input/dvm_ring.rs)"
dvm_network_cache_source="$(cat kernel/io-manager/src/io/dvm_network.rs)"
dvm_display_cache_source="$(cat kernel/io-manager/src/io/dvm_display.rs kernel/io-manager/src/io/dvm_display/{installation,peer}.rs)"
kernel_vm_cache_source="$(cat kernel/mm/src/memory/kernel_vm.rs)"
input_ring_atomic_production="$(sed '/^#\[cfg(test)\]/,$d' kernel/io-manager/src/input/dvm_ring.rs)"
host_input_ring_atomic_production="$(sed '/^#\[cfg(test)\]/,$d' libs/driver-domain-host/src/lib.rs)"
network_ring_atomic_production="$(sed '/^#\[cfg(test)\]/,$d' kernel/io-manager/src/io/dvm_network.rs)"
input_ring_snapshot_body="$(sed -n '/^fn copy_immutable_header_bytes(/,/^fn write_control_words_to_header_bytes(/p' kernel/io-manager/src/input/dvm_ring.rs)"
host_input_ring_snapshot_body="$(sed -n '/^fn copy_immutable_input_header_bytes(/,/^fn write_control_words_to_input_header_bytes(/p' libs/driver-domain-host/src/lib.rs)"
input_ring_load_order_body="$(sed -n '/^const fn shared_control_load_order(/,/^}/p' kernel/io-manager/src/input/dvm_ring.rs)"
input_ring_publish_order_body="$(sed -n '/^const fn shared_control_publish_order(/,/^}/p' kernel/io-manager/src/input/dvm_ring.rs)"
input_ring_update_order_body="$(sed -n '/^const fn shared_control_update_order(/,/^}/p' kernel/io-manager/src/input/dvm_ring.rs)"
input_ring_update_failure_order_body="$(sed -n '/^const fn shared_control_update_failure_order(/,/^}/p' kernel/io-manager/src/input/dvm_ring.rs)"
host_input_ring_load_order_body="$(sed -n '/^const fn shared_control_load_order(/,/^}/p' libs/driver-domain-host/src/lib.rs)"
host_input_ring_publish_order_body="$(sed -n '/^const fn shared_control_publish_order(/,/^}/p' libs/driver-domain-host/src/lib.rs)"
mmio_mapping_body="$(sed -n '/^fn map_with_cache_mode(/,/^pub(crate) fn unmap(/p' kernel/io-manager/src/driver/mmio.rs)"
mmio_direct_override_body="$(sed -n '/^fn apply_direct_map_cache_mode(/,/^fn restore_direct_map_cache_mode(/p' kernel/io-manager/src/driver/mmio.rs)"
permanent_boot_mmio_body="$(sed -n '/^pub fn map_permanent_boot_mmio_uncached(/,/^pub fn unmap_mmio_range(/p' kernel/mm/src/memory/kernel_vm.rs)"
high_window_mapping_body="$(sed -n '/^fn map_physical_range_internal(/,/^fn physical_mapping_cache_flags(/p' kernel/mm/src/memory/kernel_vm.rs)"
cache_attributes_source="$(cat kernel/mm/src/memory/cache_attributes.rs)"
bsp_cache_capture_body="$(sed -n '/^pub(super) fn capture_boot_cpu_cache_attributes()/,/^fn restore_mtrr_and_pat_baseline(/p' kernel/mm/src/memory/cache_attributes.rs)"
ap_cache_initialize_body="$(sed -n '/^pub(super) fn initialize_application_processor_cache_attributes()/,/^#\[cfg(test)\]/p' kernel/mm/src/memory/cache_attributes.rs)"
pat_contract_body="$(sed -n '/^const fn pat_with_kernel_cache_contract(/,/^fn read_msr(/p' kernel/mm/src/memory/cache_attributes.rs)"
ap_trampoline_source="$(cat kernel/nucleus-core/src/multiboot2_entry.S)"
ap_entry_body="$(sed -n '/^extern "C" fn rustos_ap_entry(/,/^    loop {/p' kernel/executive/src/boot.rs)"
boot_initialize_body="$(sed -n '/^pub unsafe fn initialize_kernel(/,/^fn initialize_application_processors(/p' kernel/executive/src/boot.rs)"
mmio_conflict_line="$(grep -n -m1 -F 'physical_ranges_overlap(mapping.phys_start, mapping.size, phys_start, size)' <<<"$mmio_mapping_body" | cut -d: -f1)"
mmio_straddle_guard_line="$(grep -n -m1 -F 'if physical_range_straddles_direct_map_limit(phys_start, phys_end) {' <<<"$mmio_mapping_body" | cut -d: -f1)"
mmio_registry_reserve_line="$(grep -n -m1 -F 'if registry.mappings.try_reserve(1).is_err() {' <<<"$mmio_mapping_body" | cut -d: -f1)"
mmio_direct_map_line="$(grep -n -m1 -F 'direct_map_mapping(phys_start, size)' <<<"$mmio_mapping_body" | cut -d: -f1)"
mmio_window_map_line="$(grep -n -m1 -F 'crate::memory::paging::map_shared_memory_range(phys_start, size)' <<<"$mmio_mapping_body" | cut -d: -f1)"
mmio_override_reserve_line="$(grep -n -m1 -F 'if overrides.try_reserve(page_count).is_err()' <<<"$mmio_direct_override_body" | cut -d: -f1)"
mmio_direct_map_update_line="$(grep -n -m1 -F 'crate::memory::paging::update_direct_map_range_flags(' <<<"$mmio_direct_override_body" | cut -d: -f1)"
ap_memory_type_init_line="$(grep -n -m1 -F 'mm_api::boot::initialize_application_processor_cache_attributes()' <<<"$ap_entry_body" | cut -d: -f1)"
ap_online_parked_line="$(grep -n -m1 -F 'CpuLifecycleState::OnlineParked' <<<"$ap_entry_body" | cut -d: -f1)"
ap_private_ready_line="$(grep -n -m1 -F '"smp-ap-private-ready"' <<<"$ap_entry_body" | cut -d: -f1)"
ap_no_fill_cr0_line="$(grep -n -m1 -F 'and $0xdfffffff, %eax' <<<"$ap_trampoline_source" | cut -d: -f1)"
ap_no_fill_wbinvd_line="$(grep -n -m1 -F 'wbinvd' <<<"$ap_trampoline_source" | cut -d: -f1)"
apic_permanent_map_line="$(grep -n -m1 -F 'mm_api::paging::map_permanent_boot_mmio_uncached(local_apic_phys, 4096)' <<<"$boot_initialize_body" | cut -d: -f1)"
apic_configure_line="$(grep -n -m1 -F 'hal_api::cpu::configure_local_apic_mmio(local_apic_phys, local_apic_virt)' <<<"$boot_initialize_body" | cut -d: -f1)"
apic_pic_line="$(grep -n -m1 -F 'hal_api::init_pic();' <<<"$boot_initialize_body" | cut -d: -f1)"
raw_high_window_callers="$(rg -l -F 'map_physical_range_internal(' --glob '*.rs' kernel | LC_ALL=C sort)"
raw_direct_map_update_callers="$(rg -l -F 'update_direct_map_range_flags(' --glob '*.rs' kernel | LC_ALL=C sort)"
direct_map_cache_flag_callers="$(rg -l -F 'direct_map_cache_flags_for_phys(' --glob '*.rs' kernel | LC_ALL=C sort)"
debug_milestone_class_body="$(sed -n '/^pub(super) fn milestone_output_class(/,/^}$/p' kernel/nucleus-core/src/debug/milestone_class.rs)"
debug_milestone_loss_snapshot_body="$(sed -n '/^pub(super) fn milestone_loss_snapshot(/,/^}$/p' kernel/nucleus-core/src/debug/milestone_class.rs)"
debug_milestone_drop_body="$(sed -n '/^fn record_milestone_output_drop_to(/,/^}$/p' kernel/nucleus-core/src/debug/mod.rs)"
dvm_revoke_reason_count="$(grep -Ec '^[[:space:]]{4}[A-Za-z][A-Za-z0-9]* = [1-9][0-9]*,$' <<<"$dvm_block_revoke_reason_body")"
dvm_revoke_guard_line="$(grep -n -m1 -F 'if self.revoked {' <<<"$dvm_block_revoke_body" | cut -d: -f1)"
dvm_revoke_observation_line="$(grep -n -m1 -F 'let observation = DvmBlockRevokeObservation {' <<<"$dvm_block_revoke_body" | cut -d: -f1)"
dvm_revoke_terminal_line="$(grep -n -m1 -F 'self.revoked = true;' <<<"$dvm_block_revoke_body" | cut -d: -f1)"
dvm_revoke_observer_line="$(grep -n -m1 -F 'observer(observation);' <<<"$dvm_block_revoke_body" | cut -d: -f1)"
dvm_revoke_pending_clear_line="$(grep -n -m1 -F 'self.pending = [None; QUEUE_DEPTH];' <<<"$dvm_block_revoke_body" | cut -d: -f1)"
dvm_revoke_flags_clear_line="$(grep -n -m1 -F 'fetch_and_u32(' <<<"$dvm_block_revoke_body" | cut -d: -f1)"
dvm_snapshot_file_open_line="$(grep -n -m1 -F 'std::fs::File::open(disk)' <<<"$dvm_snapshot_sync_body" | cut -d: -f1)"
dvm_snapshot_first_sync_line="$(grep -n -m1 -F '.sync_all()' <<<"$dvm_snapshot_sync_body" | cut -d: -f1)"
dvm_snapshot_directory_open_line="$(grep -n -m1 -F 'std::fs::File::open(directory)' <<<"$dvm_snapshot_sync_body" | cut -d: -f1)"
dvm_snapshot_last_sync_line="$(grep -n -F '.sync_all()' <<<"$dvm_snapshot_sync_body" | tail -n 1 | cut -d: -f1)"
dvm_snapshot_copy_line="$(rg -n -m1 -F 'fs::copy(&runtime_disk, &disk)' tools/xtask/src/kvm/layout.rs | cut -d: -f1)"
dvm_snapshot_permissions_line="$(rg -n -m1 -F 'fs::set_permissions(&disk, std::fs::Permissions::from_mode(0o600))?' tools/xtask/src/kvm/layout.rs | cut -d: -f1)"
dvm_snapshot_sync_call_line="$(rg -n -m1 -F 'sync_private_dvm_block_snapshot(&disk, &run_dir)?' tools/xtask/src/kvm/layout.rs | cut -d: -f1)"
dvm_snapshot_aperture_line="$(rg -n -m1 -F 'create_dvm_block_aperture(&aperture, &disk, &config.storage_epoch_signing_key)?' tools/xtask/src/kvm/layout.rs | cut -d: -f1)"
# The v6 SMP evidence snapshot is an admission boundary, not a post-hoc
# report.  Keep both calls in the *smoke* orchestration body: a matching call
# elsewhere (notably kvm-run) cannot establish that the bytes were captured
# before this pair of QEMU processes was spawned.
kvm_smoke_body="$(sed -n '/^pub(crate) fn kvm_smoke_command<I>(/,/^fn smoke_guest_display(/p' tools/xtask/src/kvm/options.rs)"
kvm_launch_capture_line="$(grep -n -m1 'smp_qualification::capture_kvm_launch_evidence' <<<"$kvm_smoke_body" | cut -d: -f1)"
kvm_bounded_input_relay_line="$(grep -n -m1 'let control_relay = start_dvm_input_relay(' <<<"$kvm_smoke_body" | cut -d: -f1)"
kvm_guest_spawn_line="$(grep -n -m1 'let (mut rustos, mut dvm) = spawn_guests(' <<<"$kvm_smoke_body" | cut -d: -f1)"
kvm_boot_started_line="$(grep -n -m1 'let boot_started = Instant::now();' <<<"$kvm_smoke_body" | cut -d: -f1)"
kvm_deadline_line="$(grep -n -m1 'let deadline = boot_started + options.timeout;' <<<"$kvm_smoke_body" | cut -d: -f1)"
kvm_precapture_body="$(sed '/smp_qualification::capture_kvm_launch_evidence/,$d' <<<"$kvm_smoke_body")"
kvm_interactive_body="$(sed -n '/^pub(crate) fn kvm_run_command(/,/^fn log_kvm_start_phase(/p' tools/xtask/src/kvm/options.rs)"

# Block/input/network BAR2 is coherent shared RAM, not a framebuffer or controller
# register window. The physical-interval guard runs before either direct-map or
# high-window installation, so a cache-mode mismatch cannot become an alias.
if ! grep -Fq '!is_io && prefetchable && size == DVM_BLOCK_APERTURE_BYTES' <<<"$dvm_block_cache_source" \
    || ! grep -Fq 'crate::driver::mmio::map_shared_write_back(resource.start, resource_len).cast::<u8>();' <<<"$dvm_block_cache_source" \
    || ! grep -Fq 'shared_start={:#x} shared_size={:#x} shared_prefetchable={} shared_64={} cache=wb' <<<"$dvm_block_cache_source" \
    || ! grep -Fq '!is_io && prefetchable && size == DVM_INPUT_RING_APERTURE_BYTES' <<<"$dvm_input_cache_source" \
    || ! grep -Fq 'crate::driver::mmio::map_shared_write_back(resource.start, resource_len).cast::<u8>();' <<<"$dvm_input_cache_source" \
    || ! grep -Fq '!is_io && prefetchable && size == DVM_NET_APERTURE_BYTES' <<<"$dvm_network_cache_source" \
    || ! grep -Fq 'crate::driver::mmio::map_shared_write_back(resource.start, resource_len).cast::<u8>();' <<<"$dvm_network_cache_source" \
    || ! grep -Fq 'PhysicalMappingCacheMode::WriteBack => PageTableFlags::empty(),' kernel/mm/src/memory/kernel_vm.rs \
    || ! grep -Fq '!(shared->flags & IORESOURCE_PREFETCH)' driver-domains/linux/package/rustos-dvm-block/src/rustos_dvm_block_uio.c \
    || ! grep -Fq 'mapped = ioremap_cache(shared->start, sizeof(bytes));' driver-domains/linux/package/rustos-dvm-block/src/rustos_dvm_block_uio.c \
    || ! grep -Fq '~_PAGE_CACHE_MASK' driver-domains/linux/package/rustos-dvm-block/src/rustos_dvm_block_uio.c \
    || ! grep -Fq 'remap_pfn_range' driver-domains/linux/package/rustos-dvm-block/src/rustos_dvm_block_uio.c \
    || ! grep -Fq '!(shared->flags & IORESOURCE_PREFETCH)' driver-domains/linux/package/rustos-dvm-net/src/rustos_dvm_net_uio.c \
    || ! grep -Fq 'mapped = ioremap_cache(shared->start, sizeof(bytes));' driver-domains/linux/package/rustos-dvm-net/src/rustos_dvm_net_uio.c \
    || ! grep -Fq '~_PAGE_CACHE_MASK' driver-domains/linux/package/rustos-dvm-net/src/rustos_dvm_net_uio.c \
    || ! grep -Fq 'UIO_IRQ_NONE' driver-domains/linux/package/rustos-dvm-net/src/rustos_dvm_net_uio.c \
    || ! grep -Fq 'modprobe rustos_dvm_net_uio' driver-domains/linux/board/overlay/etc/init.d/S48rustos-dvm-net \
    || ! grep -Fq '"/sys/class/uio"' driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c \
    || ! grep -Fq '#define UIO_NAME "rustos-dvm-net"' driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c \
    || grep -Fq '/sys/bus/pci/devices' driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c \
    || grep -Fq 'resource2' driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c \
    || ! grep -Fq 'libc::MAP_SHARED' libs/driver-domain-host/src/lib.rs \
    || rg -q -e 'fn[[:space:]]+map[[:space:]]*\(' kernel/io-manager/src/driver/mmio.rs; then
    echo 'DVM block/input/network shared RAM must be exact prefetchable WB, Linux BAR2 must clear PAT cache flags, raw network resource2 access must remain absent, host input must MAP_SHARED, and ambiguous mmio::map must remain absent' >&2
    exit 1
fi
if ! grep -Fq 'use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};' <<<"$network_ring_atomic_production" \
    || ! grep -Fq ').load(Ordering::Acquire)' <<<"$network_ring_atomic_production" \
    || ! grep -Fq ').store(u32::from_le(value), Ordering::Release)' <<<"$network_ring_atomic_production" \
    || ! grep -Fq 'for index in 0..36' <<<"$network_ring_atomic_production" \
    || ! grep -Fq 'read_u32(mapped, 36)' <<<"$network_ring_atomic_production" \
    || ! grep -Fq 'for index in 56..DVM_NET_RECORD_BYTES' <<<"$network_ring_atomic_production" \
    || grep -Fq 'bytes.iter_mut().enumerate()' <<<"$network_ring_atomic_production" \
    || ! grep -Fq '__ATOMIC_ACQUIRE' driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c \
    || ! grep -Fq '__ATOMIC_RELEASE' driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c \
    || ! grep -Fq '__ATOMIC_ACQ_REL' driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c; then
    echo 'DVM network control words must use aligned Acquire/Release atomics and immutable-only byte snapshots on both sides' >&2
    exit 1
fi
if ! grep -Fq 'const fn shared_control_load_order() -> Ordering {' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'const fn shared_control_publish_order() -> Ordering {' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'const fn shared_control_update_order() -> Ordering {' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'const fn shared_control_update_failure_order() -> Ordering {' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'Ordering::Acquire' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'Ordering::Release' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'Ordering::AcqRel' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'AtomicU32::from_ptr' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'AtomicU64::from_ptr' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'word.load(shared_control_load_order())' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'word.compare_exchange_weak(' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'shared_control_update_order()' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'shared_control_update_failure_order()' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'word.store(value.to_le(), ordering);' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'DVM_INPUT_RING_FLAGS_OFFSET + size_of::<u32>()..DVM_INPUT_RING_PRODUCER_OFFSET' <<<"$input_ring_snapshot_body" \
    || ! grep -Fq 'DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET + size_of::<u64>()..bytes.len()' <<<"$input_ring_snapshot_body" \
    || ! grep -Fq 'fn write_control_words_to_header_bytes' <<<"$input_ring_atomic_production" \
    || ! grep -Fq 'const fn shared_control_load_order() -> Ordering {' <<<"$host_input_ring_atomic_production" \
    || ! grep -Fq 'const fn shared_control_publish_order() -> Ordering {' <<<"$host_input_ring_atomic_production" \
    || ! grep -Fq 'AtomicU32::from_ptr' <<<"$host_input_ring_atomic_production" \
    || ! grep -Fq 'AtomicU64::from_ptr' <<<"$host_input_ring_atomic_production" \
    || ! grep -Fq 'word.store(value.to_le(), shared_control_publish_order());' <<<"$host_input_ring_atomic_production" \
    || ! grep -Fq 'let wake_generation =' <<<"$host_input_ring_atomic_production" \
    || ! grep -Fq 'fn write_control_words_to_input_header_bytes' <<<"$host_input_ring_atomic_production" \
    || ! grep -Fq 'DVM_INPUT_RING_FLAGS_OFFSET + size_of::<u32>()..DVM_INPUT_RING_PRODUCER_OFFSET' <<<"$host_input_ring_snapshot_body" \
    || ! grep -Fq 'DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET + size_of::<u64>()..bytes.len()' <<<"$host_input_ring_snapshot_body"; then
    echo 'DVM input control words must use aligned acquire/release atomics, AcqRel flag updates, and immutable-only volatile header snapshots on both RustOS and L0' >&2
    exit 1
fi
if [[ -z "$mmio_straddle_guard_line" || -z "$mmio_conflict_line" || -z "$mmio_registry_reserve_line" || -z "$mmio_direct_map_line" || -z "$mmio_window_map_line" ]] \
    || (( mmio_straddle_guard_line >= mmio_conflict_line || mmio_conflict_line >= mmio_registry_reserve_line || mmio_registry_reserve_line >= mmio_direct_map_line || mmio_registry_reserve_line >= mmio_window_map_line )) \
    || ! grep -Fq 'physical_ranges_overlap(mapping.phys_start, mapping.size, phys_start, size)' <<<"$mmio_mapping_body" \
    || ! grep -Fq 'left_start < right_end && right_start < left_end' kernel/io-manager/src/driver/mmio.rs \
    || ! grep -Fq 'start < crate::memory::kernel_vm::DIRECT_MAP_PHYS_LIMIT' kernel/io-manager/src/driver/mmio.rs \
    || ! grep -Fq 'end_exclusive > crate::memory::kernel_vm::DIRECT_MAP_PHYS_LIMIT' kernel/io-manager/src/driver/mmio.rs \
    || [[ -z "$mmio_override_reserve_line" || -z "$mmio_direct_map_update_line" || "$mmio_override_reserve_line" -ge "$mmio_direct_map_update_line" ]] \
    || ! grep -Fq 'new_pages.try_reserve(page_count).is_err()' <<<"$mmio_direct_override_body"; then
    echo 'global physical overlap, direct-map-boundary, and reserve-before-PTE-mutation guards must fail before an alias or partial mapping can publish' >&2
    exit 1
fi
if [[ "$raw_high_window_callers" != "kernel/mm/src/memory/kernel_vm.rs" ]] \
    || [[ "$raw_direct_map_update_callers" != $'kernel/io-manager/src/driver/mmio.rs\nkernel/mm/src/memory/kernel_vm.rs\nkernel/mm/src/memory/paging.rs' ]] \
    || [[ "$direct_map_cache_flag_callers" != $'kernel/io-manager/src/driver/mmio.rs\nkernel/mm/src/memory/kernel_vm.rs\nkernel/mm/src/memory/paging.rs' ]] \
    || rg -q -F 'direct_map_flags_for_phys' kernel \
    || ! grep -Fq 'if !high_window_physical_range_is_admissible(phys_addr, size) {' <<<"$high_window_mapping_body" \
    || ! grep -Fq '&& phys_addr >= DIRECT_MAP_PHYS_LIMIT' kernel/mm/src/memory/kernel_vm.rs \
    || ! grep -Fq 'Some(end) => end <= limit,' kernel/mm/src/memory/kernel_vm.rs \
    || ! grep -Fq 'let limit = super::cache_attributes::max_physical_address();' kernel/mm/src/memory/kernel_vm.rs \
    || ! grep -Fq 'if size == 0 || end > DIRECT_MAP_PHYS_LIMIT {' <<<"$permanent_boot_mmio_body" \
    || ! grep -Fq 'update_direct_map_range_flags_batched(' <<<"$permanent_boot_mmio_body" \
    || ! grep -Fq 'crate::memory::paging::direct_map_cache_flags_for_phys(phys_page)' <<<"$mmio_direct_override_body" \
    || ! grep -Fq 'if existing_mode != MmioCacheMode::SharedWriteBack && existing_mode != cache_mode {' <<<"$mmio_direct_override_body" \
    || ! grep -Fq 'crate::driver::mmio::map_write_combining(phys_start, len).cast::<u8>();' <<<"$dvm_display_cache_source" \
    || grep -Fq 'update_direct_map_range_flags' <<<"$dvm_display_cache_source" \
    || [[ -z "$apic_permanent_map_line" || -z "$apic_configure_line" || -z "$apic_pic_line" ]] \
    || (( apic_permanent_map_line >= apic_configure_line || apic_configure_line >= apic_pic_line )); then
    echo 'direct-map cache aliases must use only the whitelisted owners: permanent APIC UC retyping precedes APIC/PIC use, high-window maps reject direct-map physical addresses, and display WC routes through io-manager' >&2
    exit 1
fi
if ! grep -Fq 'compare_exchange(' <<<"$bsp_cache_capture_body" \
    || ! grep -Fq 'cpu_memory_type_features_are_admissible(features)' <<<"$bsp_cache_capture_body" \
    || ! grep -Fq '!cache_is_enabled(read_cr0())' <<<"$bsp_cache_capture_body" \
    || ! grep -Fq 'let cap = read_msr(IA32_MTRR_CAP_MSR);' <<<"$bsp_cache_capture_body" \
    || ! grep -Fq 'let expected_pat = pat_with_kernel_cache_contract(initial_pat);' <<<"$bsp_cache_capture_body" \
    || ! grep -Fq 'BSP_MTRR_CAP.store(cap, Ordering::Relaxed);' <<<"$bsp_cache_capture_body" \
    || ! grep -Fq 'BSP_MTRR_DEF_TYPE.store(read_msr(IA32_MTRR_DEF_TYPE_MSR), Ordering::Relaxed);' <<<"$bsp_cache_capture_body" \
    || ! grep -Fq 'BASELINE_STATE.store(BASELINE_READY, Ordering::Release);' <<<"$bsp_cache_capture_body" \
    || ! grep -Fq 'BASELINE_STATE.load(Ordering::Acquire) != BASELINE_READY' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'cap != BSP_MTRR_CAP.load(Ordering::Relaxed)' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'read_cr4() & CR4_PAGE_GLOBAL_ENABLE != 0' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'write_cr0(no_fill_cache_state(read_cr0()));' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'writeback_and_invalidate_caches();' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'flush_tlb_without_global_pages();' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'restore_mtrr_and_pat_baseline(cap);' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'write_cr0(read_cr0() & !CR0_CACHE_CONTROL_MASK);' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'cache_is_enabled(read_cr0()) && current_cpu_matches_sealed_baseline(cap)' <<<"$ap_cache_initialize_body" \
    || ! grep -Fq 'const fn pat_initial_write_back_selector_is_admissible(pat: u64) -> bool {' <<<"$pat_contract_body" \
    || ! grep -Fq 'pat_entry(pat, PAT_SLOT0_SHIFT) == PAT_WRITE_BACK' <<<"$pat_contract_body" \
    || ! grep -Fq 'observed == expected' <<<"$pat_contract_body" \
    || ! grep -Fq 'pat_entry(observed, PAT_SLOT0_SHIFT) == PAT_WRITE_BACK' <<<"$pat_contract_body" \
    || ! grep -Fq 'pat_entry(observed, PAT_SLOT2_SHIFT) == PAT_UNCACHEABLE' <<<"$pat_contract_body" \
    || ! grep -Fq 'pat_entry(observed, PAT_SLOT4_SHIFT) == PAT_WRITE_COMBINING' <<<"$pat_contract_body" \
    || [[ -z "$ap_memory_type_init_line" || -z "$ap_online_parked_line" || -z "$ap_private_ready_line" ]] \
    || (( ap_memory_type_init_line >= ap_online_parked_line || ap_memory_type_init_line >= ap_private_ready_line )) \
    || [[ -z "$ap_no_fill_cr0_line" || -z "$ap_no_fill_wbinvd_line" ]] \
    || (( ap_no_fill_cr0_line >= ap_no_fill_wbinvd_line )); then
    echo 'the BSP must seal an exact MTRR/PAT baseline before SIPI; every AP must enter reset no-fill, restore and read back that baseline, then enable caches before OnlineParked or private readiness publication' >&2
    exit 1
fi

# DVM block revoke is a terminal evidence boundary.  Keep its closed production
# reason vocabulary, immutable pre-clear snapshot, and milestone ABI pinned to
# the source even though the runtime reporter is excluded from unit-test cfg.
if [[ "$dvm_revoke_reason_count" != 12 ]] \
    || ! grep -Fq 'reason: DvmBlockRevokeReason,' <<<"$dvm_block_revoke_body" \
    || ! grep -Fq 'generation: self.geometry.generation,' <<<"$dvm_block_revoke_body" \
    || ! grep -Fq 'flags: load_u32(self.base, FLAGS_OFFSET, Ordering::Acquire),' <<<"$dvm_block_revoke_body" \
    || ! grep -Fq 'expected_fixed_flags: self.geometry.flags & !DVM_BLOCK_FLAG_DVM_READY,' <<<"$dvm_block_revoke_body" \
    || ! grep -Fq 'request_producer: load_u64(self.base, REQUEST_PRODUCER_OFFSET, Ordering::Acquire),' <<<"$dvm_block_revoke_body" \
    || ! grep -Fq 'request_consumer: load_u64(self.base, REQUEST_CONSUMER_OFFSET, Ordering::Acquire),' <<<"$dvm_block_revoke_body" \
    || ! grep -Fq 'completion_producer: load_u64(self.base, COMPLETION_PRODUCER_OFFSET, Ordering::Acquire),' <<<"$dvm_block_revoke_body" \
    || ! grep -Fq 'completion_consumer: load_u64(self.base, COMPLETION_CONSUMER_OFFSET, Ordering::Acquire),' <<<"$dvm_block_revoke_body" \
    || ! grep -Fq '"dvm-block-transport-revoked"' <<<"$dvm_block_revoke_report_body" \
    || ! grep -Fq 'observation.reason as u64,' <<<"$dvm_block_revoke_report_body" \
    || ! grep -Fq 'observation.generation,' <<<"$dvm_block_revoke_report_body" \
    || ! grep -Fq 'DvmBlockOperation::Flush' <<<"$dvm_block_flush_read_body" \
    || ! grep -Fq '.finish(flush)' <<<"$dvm_block_flush_read_body" \
    || ! grep -Fq 'DvmBlockOperation::Read' <<<"$dvm_block_flush_read_body" \
    || ! grep -Fq 'DVM_BLOCK_DATA_SLOT_BYTES' <<<"$dvm_block_flush_read_body" \
    || ! grep -Fq 'assert!(!state.revoked);' <<<"$dvm_block_flush_read_body"; then
    echo 'DVM block revoke must retain 12 production reasons, one pre-clear snapshot, the reason/generation milestone ABI, and valid FLUSH-to-first-64KiB-READ liveness' >&2
    exit 1
fi
if [[ -z "$dvm_revoke_guard_line" || -z "$dvm_revoke_observation_line" || -z "$dvm_revoke_terminal_line" \
    || -z "$dvm_revoke_observer_line" || -z "$dvm_revoke_pending_clear_line" || -z "$dvm_revoke_flags_clear_line" ]] \
    || (( dvm_revoke_guard_line >= dvm_revoke_observation_line \
        || dvm_revoke_observation_line >= dvm_revoke_terminal_line \
        || dvm_revoke_terminal_line >= dvm_revoke_observer_line \
        || dvm_revoke_observer_line >= dvm_revoke_pending_clear_line \
        || dvm_revoke_pending_clear_line >= dvm_revoke_flags_clear_line )); then
    echo 'DVM block revoke must reject a second report and snapshot before terminal clear' >&2
    exit 1
fi

# Only the six closed SMP/HECS names may use qualification-local loss
# counters.  Scheduler records stay one-attempt measurements, while the host
# parser rejects any nonzero local evidence loss before accepting a workload.
debug_qualification_class_failed=0
for debug_qualification_name in smp-qualification-ready smp-qualification-start smp-qualification-finish smp-qualification-complete hecs-registers hecs-rsp-arrivals; do
    grep -Fq "\"$debug_qualification_name\"" <<<"$debug_milestone_class_body" || debug_qualification_class_failed=1
done
if [[ "$debug_qualification_class_failed" != 0 ]] \
    || ! grep -Fq 'MilestoneOutputClass::QualificationCritical' <<<"$debug_milestone_class_body" \
    || ! grep -Fq '_ if name.starts_with("kernel-scheduler-") => MilestoneOutputClass::Measurement,' <<<"$debug_milestone_class_body" \
    || ! grep -Fq '|| name == "dvm-block-transport-revoked"' <<<"$debug_milestone_class_body" \
    || ! grep -Fq 'MilestoneOutputClass::Required' <<<"$debug_milestone_class_body" \
    || ! grep -Fq 'MilestoneOutputClass::QualificationCritical => (' <<<"$debug_milestone_loss_snapshot_body" \
    || ! grep -Fq 'qualification_milestones_dropped,' <<<"$debug_milestone_loss_snapshot_body" \
    || ! grep -Fq 'qualification_discarded_bytes,' <<<"$debug_milestone_loss_snapshot_body" \
    || ! grep -Fq 'milestones_dropped.fetch_add(1, Ordering::Relaxed);' <<<"$debug_milestone_drop_body" \
    || ! grep -Fq 'qualification_milestones_dropped.fetch_add(1, Ordering::Relaxed);' <<<"$debug_milestone_drop_body" \
    || ! grep -Fq 'qualification_discarded_bytes.fetch_add(discarded_bytes, Ordering::Relaxed);' <<<"$debug_milestone_drop_body" \
    || ! rg -Fq 'event.milestones_dropped != 0' tools/xtask/src/kvm.rs \
    || ! rg -Fq '|| event.debug_bytes_discarded != 0' tools/xtask/src/kvm.rs; then
    echo 'qualification debug evidence must use its exact six SMP/HECS classes, local fail-closed loss counters, a one-attempt scheduler measurement, Required DVM revoke, and parser loss rejection' >&2
    exit 1
fi

if [[ -z "$join_line" || -z "$complete_line" || "$complete_line" -le "$join_line" ]] \
    || [[ -z "$kvm_launch_capture_line" || -z "$kvm_bounded_input_relay_line" || -z "$kvm_guest_spawn_line" || -z "$kvm_boot_started_line" || -z "$kvm_deadline_line" ]] \
    || [[ "$kvm_launch_capture_line" -ge "$kvm_bounded_input_relay_line" || "$kvm_bounded_input_relay_line" -ge "$kvm_guest_spawn_line" || "$kvm_guest_spawn_line" -ge "$kvm_boot_started_line" || "$kvm_boot_started_line" -ge "$kvm_deadline_line" ]] \
    || grep -Fq 'start_dvm_input_relay(' <<<"$kvm_precapture_body" \
    || grep -Fq 'start_dvm_input_relay_unbounded(' <<<"$kvm_smoke_body" \
    || ! grep -Fq 'start_dvm_input_relay_unbounded(' <<<"$kvm_interactive_body" \
    || ! rg -Fq 'atomic_load_explicit(&shared.completed_workers, memory_order_acquire) != shared.config.workers' apps/smpqual/smpqual.c \
    || ! rg -Fq 'private_smp_qualification:' services/runtimed/src/main.rs \
    || ! rg -Fq 'private_smp_qualification: Some(contract)' services/runtimed/src/kvm_smp_qualification.rs \
    || ! rg -Fq 'Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None)' services/runtimed/src/kvm_smp_qualification.rs \
    || ! rg -Fq 'Err(error) => Err(stable_snapshot_errno(error)),' services/runtimed/src/kvm_smp_qualification.rs \
    || ! rg -Fq 'map_err(|_| libc::EINVAL),' services/runtimed/src/kvm_smp_qualification.rs \
    || ! rg -Fq 'if policy_catalog_load_due(state.launch_catalog_loaded)' services/runtimed/src/main.rs \
    || rg -Fq 'state.ui_ready && policy_catalog_load_due' services/runtimed/src/main.rs \
    || grep -Fq 'load_kvm_smp_qualification_contract()' <<<"$ordinary_catalog_body" \
    || ! grep -Fq 'if !state.launch_catalog_loaded || state.qualification_catalog_resolved {' <<<"$qualification_reconcile_body" \
    || ! grep -Fq 'Err(errno) => return defer_qualification_catalog_retry(state, errno),' <<<"$qualification_reconcile_body" \
    || ! grep -Fq 'state.launch_entries = candidate;' <<<"$qualification_reconcile_body" \
    || ! grep -Fq 'let contract = contract?;' <<<"$qualification_candidate_body" \
    || ! rg -Fq 'super::STORAGE_NOT_READY_RETRY_BACKOFF' services/runtimed/src/catalog.rs \
    || ! rg -Fq '.min(super::MAX_LAUNCH_RETRY_BACKOFF)' services/runtimed/src/catalog.rs \
    || ! rg -Fq 'if endpoint == 0 || endpoint == -(ENOENT as i64) {' services/vfsd/src/lib.rs \
    || ! rg -Fq '        EAGAIN' services/vfsd/src/lib.rs \
    || [[ "$(rg -F -c 'let errno = storage_service_lookup_errno(endpoint);' services/vfsd/src/block.rs)" != 2 ]] \
    || ! rg -Fq '    "apps/smpqual/smpqual.elf",' tools/xtask/src/stage/mod.rs \
    || ! rg -Fq 'entry.exec == UI_SERVER_EXEC_PATH' services/runtimed/src/socket.rs \
    || ! rg -Fq 'super::kvm_smp_qualification::qualification_contract_for_launch(entry)' services/runtimed/src/socket.rs \
    || ! rg -Fq 'Ok(Some(_))' services/runtimed/src/socket.rs \
    || rg -Fq 'entry.private_smp_qualification.is_some()' services/runtimed/src/socket.rs \
    || ! rg -Fq 'if !state.ui_ready && !may_precede_ui_ready' services/runtimed/src/socket.rs \
    || ! rg -Fq 'bind_then_activate_spawned_process(' services/runtimed/src/spawn.rs \
    || ! rg -Fq 'retire_failed_spawn_or_abort(pid, stage.cleanup_stage())' services/runtimed/src/spawn.rs \
    || ! grep -Fq 'smp_qualification_bind_shape_valid(&args)' <<<"$smp_bind_body" \
    || ! grep -Fq 'live_user_process_identity_with_exact_exec_path(' <<<"$smp_bind_body" \
    || ! grep -Fq 'live_sessiond_endpoint_matches(owner_identity, endpoint_epoch)' <<<"$smp_bind_body" \
    || ! grep -Fq 'with_deferred_activation_authority_for_smp_bind(' <<<"$smp_bind_body" \
    || ! grep -Fq 'if !qualification_required' <<<"$smp_activation_body" \
    || ! grep -Fq 'published_service_endpoint_owner_and_epoch(IPC_SERVICE_SESSIOND)' <<<"$smp_activation_body" \
    || grep -Fq 'live_user_process_identity_by_pid' <<<"$smp_activation_body" \
    || ! grep -Fq 'let now_tick = crate::arch::rtc::ticks();' <<<"$smp_activation_body" \
    || ! grep -Fq 'binding.activate(owner, endpoint_epoch, target, now_tick, ticks_per_second)?;' <<<"$smp_activation_body" \
    || ! grep -Fq 'let now_tick = crate::arch::rtc::ticks();' <<<"$smp_phase_body" \
    || ! grep -Fq 'binding.admit_phase(' <<<"$smp_phase_body" \
    || ! grep -Fq 'terminalize_binding_after_endpoint_revalidation(binding_id, target);' <<<"$smp_phase_body" \
    || ! grep -Fq 'activations.get(&args.target_pid).unwrap().qualification_required' <<<"$proc_activate_body" \
    || ! grep -Fq 'activate_suspended_user_tasks_with_commit(' <<<"$proc_activate_body" \
    || ! rg -Fq 'const fn deferred_authority_is_batch_eligible(qualification_required: bool) -> bool' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq '&& deferred_authority_is_batch_eligible(qualification_required)' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq 'let mut exact_targets = [None; LOADER_ACTIVATE_BATCH_MAX_TARGETS];' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq 'deferred_authority_matches_exact_batch_request(' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq '&authority.owner, &authority.target, authority.qualification_required,' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq 'revoke_smp_qualification_for_process(process_id);' kernel/compat/src/user/syscall/linux/proc_broker_ops.rs \
    || ! rg -Fq 'admit_smp_qualification_milestone(milestone, arg0, arg1, current_cpu)' kernel/compat/src/user/syscall/linux/debug_ops.rs \
    || ! rg -Fq 'debug::write_user_bytes_serialized(&chunk[..chunk_len]);' kernel/compat/src/user/syscall/linux/debug_ops.rs \
    || rg -Fq 'debug::write_bytes(&chunk[..chunk_len]);' kernel/compat/src/user/syscall/linux/debug_ops.rs \
    || ! rg -Fq 'crate::debug::write_user_bytes_serialized(&chunk[..chunk_len]);' kernel/compat/src/user/sysops/console.rs \
    || rg -Fq 'crate::debug::write_bytes(&chunk[..chunk_len]);' kernel/compat/src/user/sysops/console.rs \
    || rg -q 'fn write_bytes' kernel/nucleus-core/src/debug/mod.rs \
    || ! rg -Fq 'let expected_events = usize::from(workers) * 3 + 1;' tools/xtask/src/kvm.rs \
    || ! rg -Fq 'SMP Ring3 qualification requires one terminal completion record' tools/xtask/src/kvm.rs \
    || ! rg -Fq 'schema: "rustos-kvm-smp-correctness-evidence-v7"' tools/xtask/src/kvm/evidence.rs \
    || ! rg -Fq 'predecessor_schema: "rustos-kvm-smp-correctness-evidence-v6"' tools/xtask/src/kvm/evidence.rs \
    || ! rg -Fq 'hecs::is_complete(&rustos_log, options.rustos_vcpus)' tools/xtask/src/kvm/guest.rs \
    || ! rg -Fq 'hecs::evidence(&rustos_log, options.rustos_vcpus)' tools/xtask/src/kvm/evidence.rs \
    || ! rg -Fq 'hardware_execution,' tools/xtask/src/kvm/evidence.rs \
    || ! rg -Fq 'multitask::hardware_rsp_arrivals(current_cpu as usize)' kernel/compat/src/user/syscall/linux/smp_qualification_ops.rs \
    || ! rg -Fq 'run_register_probes(shared, worker_id, observed_cpu)' apps/smpqual/smpqual.c \
    || ! rg -Fq 'smp_evidence_cohort: snapshot.run.cohort.clone()' tools/xtask/src/kvm/evidence.rs \
    || ! rg -Fq -- '--smp-ring3-qualification requires --smp-evidence-cohort' tools/xtask/src/kvm/smoke_args.rs \
    || ! grep -Fq 'options.dvm_block_shmem = true;' <<<"$smp_ring3_option_body" \
    || ! grep -Fq '"file={},format=raw,if=none,id=dvm-storage-disk,cache=none,aio=threads,readonly=on",' <<<"$dvm_virtual_storage_body" \
    || ! grep -Fq 'ide-cd,drive=dvm-storage-disk,bus=ide.0,unit=0,id=dvm-storage-disk-device' <<<"$dvm_virtual_storage_body" \
    || grep -Fq 'ide-hd,drive=dvm-storage-disk' <<<"$dvm_virtual_storage_body" \
    || grep -Eq 'logical_block_size|physical_block_size' <<<"$dvm_virtual_storage_body" \
    || ! rg -Fq 'const DVM_BLOCK_MEDIA_BLOCK_BYTES: u32 = 2048;' tools/xtask/src/kvm.rs \
    || ! rg -Fq 'const DVM_BLOCK_MEDIA_FEATURES: u64 = DVM_BLOCK_FEATURE_FLUSH;' tools/xtask/src/kvm.rs \
    || ! rg -Fxq 'CONFIG_BLK_DEV_SR=y' driver-domains/linux/board/linux.fragment \
    || ! rg -Fq 'grep -qx "${1}=y" "$config"' driver-domains/linux/scripts/verify-kernel-config.sh \
    || ! rg -Fxq 'require_builtin CONFIG_BLK_DEV_SR' driver-domains/linux/scripts/verify-kernel-config.sh \
    || ! rg -Fq 'built-in sr owns the immutable ATAPI' driver-domains/linux/board/overlay/etc/init.d/S12rustos-dvm-block \
    || ! rg -Fq 'Signed sd_mod owns writable disk' driver-domains/linux/board/overlay/etc/init.d/S12rustos-dvm-block \
    || rg -Fq 'modprobe sr_mod' driver-domains/linux/board/overlay/etc/init.d/S12rustos-dvm-block \
    || [[ -z "$dvm_snapshot_file_open_line" || -z "$dvm_snapshot_first_sync_line" || -z "$dvm_snapshot_directory_open_line" || -z "$dvm_snapshot_last_sync_line" ]] \
    || [[ "$dvm_snapshot_file_open_line" -ge "$dvm_snapshot_first_sync_line" || "$dvm_snapshot_first_sync_line" -ge "$dvm_snapshot_directory_open_line" || "$dvm_snapshot_directory_open_line" -ge "$dvm_snapshot_last_sync_line" ]] \
    || [[ "$(grep -F -c '.sync_all()' <<<"$dvm_snapshot_sync_body")" != 2 ]] \
    || [[ -z "$dvm_snapshot_copy_line" || -z "$dvm_snapshot_permissions_line" || -z "$dvm_snapshot_sync_call_line" || -z "$dvm_snapshot_aperture_line" ]] \
    || [[ "$dvm_snapshot_copy_line" -ge "$dvm_snapshot_permissions_line" || "$dvm_snapshot_permissions_line" -ge "$dvm_snapshot_sync_call_line" || "$dvm_snapshot_sync_call_line" -ge "$dvm_snapshot_aperture_line" ]] \
    || ! grep -Fq 'header.flags = DVM_BLOCK_FLAG_READ_ONLY;' <<<"$dvm_read_only_header_body" \
    || ! grep -Fq 'dvm_read_only_block_header(' <<<"$dvm_block_create_body" \
    || ! grep -Fq 'disk_bytes.is_multiple_of(u64::from(DVM_BLOCK_MEDIA_BLOCK_BYTES))' <<<"$dvm_block_create_body" \
    || [[ "$(grep -F -c 'DVM_BLOCK_MEDIA_BLOCK_BYTES,' <<<"$dvm_block_create_body")" != 2 ]] \
    || ! grep -Fq 'predecessor.flags & DVM_BLOCK_FLAG_READ_ONLY == 0' <<<"$dvm_block_rotate_body" \
    || ! grep -Fq 'dvm_read_only_block_header(' <<<"$dvm_block_rotate_body" \
    || ! rg -Fq 'let ready = DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY | DVM_BLOCK_FLAG_READ_ONLY;' tools/xtask/src/kvm/layout.rs \
    || ! grep -Fq 'header.flags & ready == ready && header.generation == expected_generation' <<<"$dvm_ready_generation_body" \
    || ! grep -Fq 'let successor_block_generation = if self.options.dvm_block_shmem {' <<<"$dvm_recovery_harness_body" \
    || ! grep -Fq 'rotate_dvm_block_epoch(aperture, disk, &self.config.storage_epoch_signing_key)' <<<"$dvm_recovery_harness_body" \
    || ! grep -Fq 'successor_block_generation,' <<<"$dvm_recovery_harness_body" \
    || ! grep -Fq 'expected_block_generation: Option<u64>,' <<<"$dvm_restart_recovery_body" \
    || ! grep -Fq 'rustos_log.contains("dvm-block: signed transport epoch rebound generation=")' <<<"$dvm_restart_recovery_body" \
    || ! grep -Fq 'verify_dvm_block_ready_generation(layout, generation).is_ok()' <<<"$dvm_restart_recovery_body" \
    || ! grep -Fq 'disk_bytes.is_multiple_of(u64::from(DVM_BLOCK_MEDIA_BLOCK_BYTES))' <<<"$dvm_block_ready_body" \
    || ! grep -Fq 'header.logical_block_size != DVM_BLOCK_MEDIA_BLOCK_BYTES' <<<"$dvm_block_ready_body" \
    || ! grep -Fq 'header.physical_block_size != DVM_BLOCK_MEDIA_BLOCK_BYTES' <<<"$dvm_block_ready_body" \
    || ! rg -Fq 'ioctl(device->fd, BLKROGET, &read_only) != 0' driver-domains/linux/package/rustos-dvm-block/src/rustos-dvm-block.c \
    || ! rg -Fq 'device->read_only !=' driver-domains/linux/package/rustos-dvm-block/src/rustos-dvm-block.c \
    || ! rg -Fq '((header->flags & DVM_BLOCK_FLAG_READ_ONLY) != 0U) ||' driver-domains/linux/package/rustos-dvm-block/src/rustos-dvm-block.c \
    || ! rg -Fq 'errno = EPROTO;' driver-domains/linux/package/rustos-dvm-block/src/rustos-dvm-block.c \
    || ! rg -Fq 'storaged: dvm-block e2e media barrier completed generation=' services/storaged/src/main.rs \
    || ! rg -Fq 'path=vfs-policy->block-broker->shared-ring->linux-dvm->media-barrier' services/storaged/src/main.rs \
    || ! rg -Fq 'let rustos_runtime_image = binary_artifact(&config.root_dir, &layout.runtime_disk)?;' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'dvm_attached_block_disk: Option<KvmSuccessArtifact>,' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'smpqual_early_system_executable: Option<KvmSuccessArtifact>,' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'let dvm_attached_block_disk = capture_dvm_attached_block_disk(' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'then(|| capture_smpqual_early_system_executable(&layout.runtime_disk))' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'path: "system/boot/early-system.img#apps/smpqual/smpqual.elf".to_owned(),' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'verify_prelaunch_snapshot(&archive.root, snapshot)?' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'verify_dvm_attached_block_disk_matches_runtime(snapshot)?;' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'let observed = capture_smpqual_early_system_executable(&runtime_disk)?;' tools/xtask/src/kvm/evidence/smp_qualification.rs \
    || ! rg -Fq 'fs::hard_link(&self.temporary_path, &self.final_path)' tools/xtask/src/kvm/evidence/smp_qualification.rs; then
    echo 'SMP Ring3 qualification source no longer preserves UI-independent private injection, the closed pre-UI exception, bind-before-activate cleanup, generation-bound kernel FSM admission, escaped user debug, join-before-complete, 3N+1 lifecycle plus 2N HECS records, v7 actual-execution and pre-spawn DVM/early-system executable attestations, durable snapshot-before-signing order, QEMU readonly ATAPI/header/Linux-relay agreement with built-in sr at exact 2048-byte FLUSH-only media geometry, exact media-barrier authority path, exact successor-generation recovery, publication-time drift rejection, or non-overwriting archive publication' >&2
    exit 1
fi

# The old catalog dispatch/fallback body is deleted. Qualify the ACTUAL
# independent fair, strict overdue, due User reservation, incomplete fallback
# and exact Direct FIFO path together; original native/mutation witnesses below
# remain required and are not replaced by these source anchors (checked above).

# Under Scheduler, a borrowed worker is admitted only against the exact
# donor publication and its still-current reply/owner token; the worker's
# own stamp cannot silently stand in for a missing donor publication.
borrowed_local_body="$(sed -n '/^pub(super) fn effective_charge_stamp(/,/^const fn ordinary_voluntary_pick/p' kernel/ps/src/multitask/scheduler/local_dispatch.rs)"
if ! grep -Fq 'let borrowed = token.map(|(owner_slot, _)| current_identity::read_dispatch(owner_slot));' <<<"$borrowed_local_body" \
    || ! grep -Fq 'donation_ledger::borrowed_context_charge_token(slot) == token' <<<"$borrowed_local_body" \
    || ! grep -Fq '&& effective_charge_is_eligible(slot, publication.pager_charge, cpu, now_ns)' <<<"$borrowed_local_body" \
    || ! grep -Fq 'match effective_charge_admission(slot, publication.pager_charge, cpu, now_ns) {' <<<"$borrowed_local_body" \
    || ! grep -Fq 'Some(false) => Ok(false),' <<<"$borrowed_local_body" \
    || ! grep -Fq 'None => Err(LocalFallbackReason::LifecycleIneligible),' <<<"$borrowed_local_body" \
    || ! grep -Fq 'if !published_candidate_admission(slot, cpu, now_ns)? {' <<<"$borrowed_local_body" \
    || ! grep -Fq 'scheduling_runtime::published_stamp_admission(stamp, cpu, now_ns)' <<<"$borrowed_local_body" \
    || ! grep -Fq 'rustos_scheduler_frame_gate::budget_admission_after_token_check(' <<<"$borrowed_local_body" \
    || ! grep -Fq 'policy.cpu_mask & cpu_bit != 0,' kernel/ps/src/multitask/scheduler/scheduling_runtime.rs \
    || ! grep -Fq '&& cpu_allowed' libs/rustos-scheduler-frame-gate/src/lib.rs; then
    echo 'borrowed local admission must validate donor charge and exact live reply token' >&2
    exit 1
fi

# Production per-CPU handoff and reservation queues must live outside the
# lifecycle catalog. No host duplicate backend remains either; actual production
# callers select the same static policy owner on every CPU.
python3 - <<'PY'
from pathlib import Path

scheduler = Path('kernel/ps/src/multitask/scheduler.rs').read_text()
policy = Path('kernel/ps/src/multitask/scheduler/dispatch_policy.rs').read_text()
initial = Path('kernel/ps/src/multitask/scheduler/bootstrap/initial_tasks.rs').read_text()
dispatcher = Path('kernel/ps/src/multitask/scheduler/dispatcher.rs').read_text()
if ('cpu_dispatch:' in scheduler
    or 'self.cpu_dispatch' in scheduler
    or 'dispatch_policy::lock(cpu)' not in dispatcher
    or 'dispatch_policy::reset();' not in initial
    or 'fn reset(' in scheduler
    or 'static CPU_DISPATCH_POLICY: [CpuDispatchLock; MAX_TRACKED_CPUS]' not in policy
    or 'CPU_DISPATCH_POLICY[cpu].lock()' not in policy):
    raise SystemExit('production CPU dispatch policy must be stored outside Scheduler catalog')
PY

# Current terminal reply owns original masked CPU and actual wake binding
# through donation withdrawal, wake and exact FIFO publication. IRQ fallback
# borrows its SAME original CPU owner; full fast settlement/dispatch is separate.
# Both paths retain generation-qualified tokens; selection never gains a grant.
reply_current_body="$(sed -n '/^pub fn complete_ipc_reply_wake_handoff(/,/^}/p' kernel/ps/src/multitask/current/ipc.rs)"
reply_custody_body="$(sed -n '/^pub fn settle_ipc_reply_scheduling_context(/,/^pub fn complete_ipc_reply_wake_handoff(/p' kernel/ps/src/multitask/current/ipc.rs)"
reply_scheduler_body="$(cat kernel/ps/src/multitask/scheduler/ipc_publication.rs)"
reply_enqueue_body="$(sed -n '/^fn enqueue_reply_wake_after_catalog(/,/^pub(super) fn enqueue_reply_wake/p' kernel/ps/src/multitask/scheduler/sync_handoff.rs)"
reply_selection_body="$(sed -n '/^fn synchronous(/,/^fn latency(/p' kernel/ps/src/multitask/scheduler/dispatcher.rs)"
plain_reply_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_ipc_reply(/,/^pub(super) fn syscall_linux_rustos_ipc_call_with_handles/p' kernel/compat/src/user/syscall/linux/ipc_ops.rs)"
handle_reply_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_ipc_reply_with_handles/,/^pub(super) fn call_linux_syscall_endpoint/p' kernel/compat/src/user/syscall/linux/ipc_ops.rs)"
reply_recv_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_ipc_reply_recv_with_sender/,$p' kernel/compat/src/user/syscall/linux/ipc_reply_recv.rs)"
reply_owned_body="$(cat kernel/ps/src/multitask/scheduler/wake_publication.rs)"
if ! grep -Fq 'scheduler::wake_publication::reply(reply, task_id)' <<<"$reply_current_body" \
    || grep -Eq 'scheduler_(mut|ref)' <<<"$reply_current_body" \
    || ! grep -Fq 'Some((runqueue::wait::REASON_ENDPOINT_REPLY, reply))' <<<"$reply_owned_body" \
    || ! grep -Fq 'sync_handoff::enqueue_reply_wake(token)' <<<"$reply_owned_body" \
    || ! grep -Fq 'dispatch_priority::Publication::begin(scope, token.target_cpu())' <<<"$reply_owned_body" \
    || ! grep -Fq 'donation_ledger::release_reply(reply, ipc_donation::DonationNamespace::IpcReply)' <<<"$reply_owned_body" \
    || grep -Fq 'fn complete_ipc_reply_wake_handoff(' <<<"$reply_scheduler_body" \
    || ! grep -Fq 'wake_publication::reply_switching(scope, reply, caller, eligible_cpus, switch)' <<<"$reply_scheduler_body" \
    || ! grep -Fq 'if !owner_still_matches(token)' <<<"$reply_enqueue_body" \
    || ! grep -Fq 'owner_still_matches(token) && retained' <<<"$reply_enqueue_body" \
    || ! grep -Fq 'record.has_current_dispatch_custody()' <<<"$reply_selection_body" \
    || ! grep -Fq 'record.bind_selected_local_owner(runqueue::owner(record.slot()), cpu)' <<<"$reply_selection_body" \
    || ! grep -Fq 'settle_terminal_ipc_reply_scheduling_context(reply, custody)' <<<"$reply_custody_body" \
    || ! grep -Fq 'complete_ipc_reply_wake_handoff(reply, completion.caller_task_id)' <<<"$reply_custody_body" \
    || ! grep -Fq 'multitask::complete_ipc_reply_wake_handoff_with_custody(reply, completion)' <<<"$plain_reply_body" \
    || ! grep -Fq 'multitask::complete_ipc_reply_wake_handoff_with_custody(args.reply_cap, completion)' <<<"$handle_reply_body" \
    || ! grep -Fq 'multitask::complete_ipc_reply_wake_handoff_with_custody(args.reply_cap, completion)' <<<"$reply_recv_body" \
    || grep -Eq 'release_ipc_priority|wake_task|set_next_synchronous_pick_hint' <<<"$plain_reply_body" \
    || grep -Eq 'release_ipc_priority|wake_task|set_next_synchronous_pick_hint' <<<"$handle_reply_body" \
    || grep -Eq 'release_ipc_priority|wake_task|set_next_synchronous_pick_hint' <<<"$reply_recv_body"; then
    echo 'Current terminal IPC replies must return exact scheduling-context custody and retain original binding through exact FIFO publication without legacy fallback' >&2
    exit 1
fi

# ReplyObject is the sole transport owner of caller scheduling-context custody.
# Every terminal class must extract the field with `take` or remove the exact
# reply object and return it; legacy reply/cancel helpers reject custody-bearing
# calls so no internal caller can accidentally discard the token.
ipc_runtime_source=kernel/ipc-runtime/src/ipc/mod.rs
if ! rg -Uq 'struct ReplyObject \{[^}]*scheduling_context: Option<ReplySchedulingContextCustody>' "$ipc_runtime_source" \
    || [ "$(rg -c 'reply_object\.scheduling_context\.take\(\)|reply\.scheduling_context\.take\(\)' "$ipc_runtime_source")" -ne 7 ] \
    || ! rg -Fq 'pub scheduling_context: Option<ReplySchedulingContextCustody>' "$ipc_runtime_source" \
    || ! rg -Fq 'ensure_reply_has_no_scheduling_context(reply)?;' "$ipc_runtime_source" \
    || ! rg -Fq 'enqueue_endpoint_call_with_handles_priority_and_custody(' kernel/compat/src/user/syscall/linux/ipc_ops.rs \
    || ! rg -Fq 'settle_endpoint_scheduling_contexts(&wake_set);' kernel/ps/src/multitask/scheduler/reclaim.rs; then
    echo 'reply-owned scheduling-context custody no longer covers reply, cancel, owner failure, and retirement exactly once' >&2
    exit 1
fi

# Fork publishes a copied reservation over resident leaves, while ordinary
# mmap reserves an empty range. The entrypoints must stay distinct, and the
# pre-publication child-TID copyout must receive a private writable leaf rather
# than writing through a read-only COW alias. This binding catches the failure
# class that first appeared as an undifferentiated fork ENOMEM.
pager_current_source=kernel/ps/src/multitask/current/pager.rs
ordinary_vma_publish_body="$(sed -n '/^pub fn publish_pager_vma_for_process(/,/^\/\/\/ Publishes one inherited VMA/p' "$pager_current_source")"
inherited_vma_publish_body="$(sed -n '/^pub fn publish_inherited_pager_vma_for_process(/,/^\/\/\/ Resolve one current-task fault/p' "$pager_current_source")"
fork_broker_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_proc_fork_broker(/,/^pub(super) fn valid_process_fork_plan_locally/p' kernel/compat/src/user/syscall/linux/proc_broker_ops/fork.rs)"
pager_template_body="$(sed -n '/^fn template_is_canonical(/,/^fn stamped_region/p' kernel/ps/src/multitask/pager_vma.rs)"
pager_admission_source=kernel/compat/src/user/syscall/linux/mm_broker_ops/pager_admission.rs
if ! grep -Fq '.user_range_is_unmapped(' <<<"$ordinary_vma_publish_body" \
    || grep -Fq '.user_range_is_unmapped(' <<<"$inherited_vma_publish_body" \
    || ! grep -Fq 'publish_inherited_pager_vma_for_process' <<<"$inherited_vma_publish_body" \
    || ! grep -Fq 'writable_committed_regions_cover(regions, args.ctid_ptr, byte_len)' <<<"$fork_broker_body" \
    || ! grep -Fq '.validate_user_read_buffer(VirtAddr::new(args.ctid_ptr), byte_len)?' <<<"$fork_broker_body" \
    || ! grep -Fq 'clone_user_space_cow(&cow_ranges, &eager_private_ranges)' <<<"$fork_broker_body" \
    || grep -Fq 'region.prot != 0' <<<"$pager_template_body" \
    || ! rg -Uq '(?s)existing\.process_handle != process\.slot\(\).*existing\.process_generation != process_generation.*existing\.mm_generation != mm_generation.*slot\.publish\(None\)\?' kernel/ps/src/multitask/pager_vma.rs \
    || ! rg -Uq '(?s)PagerVmaError::Malformed => 1,.*PagerVmaError::Overlap => 2,.*PagerVmaError::Pressure => 3,.*PagerVmaError::Stale => 4,.*PagerVmaError::Unstable => 5,.*PagerVmaError::Denied => 6,' "$pager_admission_source" \
    || ! grep -Fq 'count.saturating_mul(1 << 8) | pager_vma_error_class(error)' "$pager_admission_source"; then
    echo 'fork inheritance must preserve VMA authority, separate resident publication from ordinary mmap, private pre-publication copyout, reclaim stale generations, accept PROT_NONE, and retain exact failure classes' >&2
    exit 1
fi

# Kernel copyout is a write in the VMA/COW model even though the byte mover
# reaches frames through the direct map. Every usermem writer must enter the
# exact-generation wrapper; the slow path checks logical VMA authority, splits
# against the retained root, then retranslates before constructing its proof.
# The direct fast path (kernel-hal `user_access`) writes through the live CR3 with
# CR0.WP set, so a read-only COW leaf faults and re-enters this same wrapper.
copyout_ps_source=kernel/ps/src/multitask/user_copy.rs
copyout_mm_source=kernel/mm/src/memory/address_space/user_copy.rs
copyout_cow_source=kernel/mm/src/memory/address_space/pager_fault_mapping.rs
usermem_source=kernel/ps/src/user/sysops/usermem.rs
cow_probe_source=apps/ipcbench/src/cow_probe.rs
if ! rg -Uq 'fn with_current_write_address_space<R>\([^)]*UserCopyAddressSpace' "$usermem_source" \
    || [ "$(rg -c 'with_current_write_address_space' "$usermem_source")" -ne 5 ] \
    || [ "$(rg -c 'try_with_user_copy_address_space' "$usermem_source")" -ne 5 ] \
    || ! rg -Fq 'validate_user_write_resolving_cow' "$copyout_ps_source" \
    || ! rg -Fq 'region.commit_state != VM_COMMIT_COMMITTED' "$copyout_ps_source" \
    || ! rg -Fq 'region.prot & VM_PROT_WRITE == 0' "$copyout_ps_source" \
    || ! rg -Fq 'region.sharing != VM_SHARING_PRIVATE' "$copyout_ps_source" \
    || ! rg -Uq 'resolve_cow_write_under_mutation\(\s*self\.pml4_frame_phys,\s*start,\s*frame\.as_u64\(\),\s*kind,\s*mutation' "$copyout_cow_source" \
    || [ "$(rg -c 'translate_user_with_flags\(VirtAddr::new\(cursor\)\)' "$copyout_mm_source")" -lt 2 ] \
    || ! rg -Uq 'libc::pread\(stamp\.fd, copyout_page\.cast\(\), 16, 0\)' "$cow_probe_source" \
    || [ "$(rg -c 'ptr::read_volatile\(copyout_page' "$cow_probe_source")" -lt 4 ]; then
    echo 'kernel copyout must resolve COW through exact VMA authority and retained root, retranslate, and retain the untouched-page KVM witness' >&2
    exit 1
fi

checks=0
# The registry is read first and executed second. One `cargo test` per witness
# spent almost all of its time re-entering Cargo for a test binary it had
# already built, and the same test can witness several models, so it was also
# rebuilt and rerun once per model it appears under. Collecting the witnesses
# per Cargo selection turns 575 invocations into one per package/feature pair
# without weakening the claim: `--exact` still admits only a full-path match,
# every registered witness must still print its own passing line, and the
# executed count must still equal the exact set that was asked for.
declare -A group_tests=()
declare -A group_rows=()
declare -a group_order=()
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
    group="$package|$features"
    if [[ -z "${group_rows[$group]+set}" ]]; then
        group_order+=("$group")
        group_tests["$group"]=$'\n'
        group_rows["$group"]=""
    fi
    if [[ "${group_tests[$group]}" != *$'\n'"$test_name"$'\n'* ]]; then
        group_tests["$group"]+="$test_name"$'\n'
    fi
    group_rows["$group"]+="$witness_key"$'\n'
done <<'EOF'
pager-fault-slot-lifecycle/PagerFaultSlotLifecycle|kernel-ps|multitask::scheduler::pager_handoff::tests::actual_pager_full_fifo_preserves_original_worker_wait_and_faulting_current
pager-fault-slot-lifecycle/PagerFaultSlotLifecycle|kernel-ps|multitask::scheduler::pager_handoff::tests::actual_pager_does_not_clear_an_unrelated_rearmed_worker_wait
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::pager_handoff::tests::actual_pager_last_floor_effect_keeps_original_reader_binding_and_typed_phase
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::pager_handoff::tests::actual_pager_precommit_current_worker_wake_never_reads_consumed_frame_or_reblocks
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::pager_handoff::tests::actual_pager_resume_borrows_original_current_mm_without_claim_or_frame_read
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::pager_handoff::tests::actual_pager_remote_worker_publication_binds_exact_fifo_and_mailbox_generation
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::irq_entry_tests::actual_irq_control_probe_keeps_original_current_bank_without_directory
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::irq_entry_tests::actual_irq_unmasked_scope_refuses_before_current_or_frame_effects
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::irq_entry_tests::actual_irq_dispatch_uses_original_claim_interval_and_ignores_return_scratch
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::irq_entry_tests::actual_irq_fast_producer_claim_and_changed_return_keep_both_born_banks_until_rsp_ack
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::irq_entry_tests::actual_irq_diagnostic_compares_original_physical_bank_and_uid_permit
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_call_reverse_wake_before_original_sender_commit_cannot_be_overwritten
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_call_unmasked_or_closed_receiver_refuses_before_any_wait_or_fifo_effect
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_reply_foreign_context_and_wrong_wait_refuse_without_generic_fallback
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_cross_cpu_call_keeps_original_funding_wait_fifo_and_mailbox_generation
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_reverse_reply_uses_original_typed_wait_fair_owner_and_exact_fifo
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_full_fifo_refuses_original_call_before_owner_wait_or_sender_commit
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_raced_owner_cannot_be_reacquired_by_original_typed_handoff_cas
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_cold_call_keeps_sender_unarmed_and_original_loans_through_receiver_fifo
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_withdrawn_receive_retains_exact_queued_reply_without_wake_or_early_worker_bind
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_cold_publication_raced_receive_preserves_exact_bound_reply_and_newer_foreign_wait
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_cold_call_own_funding_bind_cannot_certify_a_refused_receiver_wake
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_cold_call_cross_cpu_keeps_exact_receive_funding_and_remote_queue
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_cold_call_full_fifo_keeps_typed_wake_and_original_unarmed_sender
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::ipc_publication::tests::actual_cold_call_foreign_endpoint_or_sender_wait_refuses_before_bind_and_wake
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::group_exit::tests::actual_group_worker_transfers_original_member_loan_to_terminal_and_acks_without_catalog
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::group_exit::tests::actual_competing_terminal_reservation_suspends_original_census_and_resumes_same_request
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::group_exit::tests::actual_group_desired_ack_does_not_complete_terminal_consumers_before_original_borrower_drains
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::queued::explicit_suspension_retains_original_census_nonce_effects_and_pin_without_reopening_join
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::queued::suspended_effects_retain_captured_detached_uid_instead_of_recollecting_recycled_live_member
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::dispatch_priority::tests::actual_publication_borrows_original_masked_scope_through_release
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::dispatcher::tests::actual_direct_fifo_selection_retains_consumed_generation_through_original_claim
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::dispatcher::tests::actual_dispatch_owns_choice_frame_funding_interval_and_return_without_catalog
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::dispatcher::tests::actual_failed_fast_probe_reuses_one_original_choice_and_outgoing_interval
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::dispatcher::tests::actual_earlier_claim_discharges_only_its_exact_spawn_first_turn
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatcher::tests::actual_incomplete_first_turn_retains_fifo_and_pending_while_original_reader_excludes_claim
scheduler-active-balance/SchedulerActiveBalance|kernel-ps|multitask::scheduler::dispatcher::maintenance::tests::original_migration_reader_spans_exact_owner_cas_and_target_mailbox
scheduler-active-balance/SchedulerActiveBalance|kernel-ps|multitask::scheduler::dispatcher::maintenance::tests::raced_block_refuses_migration_without_reacquiring_generation_or_waking
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_claim_opens_original_interval_before_return_without_scalar_identity_reacquisition
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_prepared_original_funder_cannot_be_rebound_from_a_later_scalar_charge_publication
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_unused_original_funding_refusal_releases_final_closing_funder_outside_cpu_leaf
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_claimed_funding_survives_terminal_until_original_interval_settlement
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::return_architecture::tests::actual_settled_original_grant_cannot_admit_architecture_with_retained_frame_banks
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::typed_tests::actual_typed_concurrent_arm_and_capture_have_one_winner_without_runowner_generation_change
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::wake_publication::typed_tests::actual_typed_terminal_after_capture_cannot_clear_or_resurrect_and_keeps_original_reuse_fence
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::typed_tests::actual_typed_phase_excludes_same_payload_rearm_until_original_publication_releases
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::typed_tests::actual_typed_cancel_invalidates_original_match_without_clearing_or_rearming_a_new_wait
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::typed_tests::actual_typed_original_phase_survives_current_wake_through_last_effect_not_just_clear
pager-fault-slot-lifecycle/PagerFaultSlotLifecycle|kernel-ps|multitask::scheduler::wake_publication::typed_tests::actual_typed_pager_matches_exact_key_and_keeps_original_loan_through_real_fifo
pager-fault-slot-lifecycle/PagerFaultSlotLifecycle|kernel-ps|multitask::scheduler::wake_publication::typed_tests::actual_typed_service_requires_original_process_and_wait_kind_and_rejects_foreign_scope
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::typed_tests::actual_typed_refused_running_arm_preserves_original_blocked_payload_and_current_return_wake
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::runqueue::wait::epoch::tests::actual_exhausted_wait_epoch_refuses_capture_and_arm_before_payload_or_pending_effects
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::process_table::tests::process_address_space_and_exec_exit_are_serialized
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::authority_identity_exhaustion_fails_closed_before_wrap
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::identity_tests::task_identity_exhaustion_never_wraps_to_a_live_id
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::current_identity::tests::dispatch_lifecycle_is_committed_with_identity
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::current_identity::tests::actual_current_pager_fault_keeps_original_bank_identity_during_metadata_publication
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::current_identity::owner_tests::metadata_republication_cannot_erase_newer_lifecycle_intent
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::current_identity::owner_tests::absent_partial_metadata_preserves_pending_start_and_stop_intent
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::current_identity::owner_tests::stale_live_command_cannot_stop_or_resume_a_recycled_identity
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::current_identity::owner_tests::concurrent_metadata_and_gate_writers_do_not_lose_each_others_fields
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::process_table::tests::process_generations_fail_closed_instead_of_aliasing_stale_handles
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-compat|user::syscall::linux::proc_broker_ops::tests::broker_authority_identity_exhaustion_never_wraps
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ipc-runtime|ipc::slab::tests::removed_handle_never_aliases_reused_slot
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ipc-runtime|ipc::slab::tests::hinted_reuse_changes_generation_without_touching_global_cursor
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ipc-runtime|ipc::slab::tests::hinted_collisions_use_full_capacity_and_preserve_unpublished_value
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ipc-runtime|ipc::slab::tests::hinted_allocation_skips_permanently_exhausted_generation
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ipc-runtime|ipc::slab::tests::concurrent_hinted_allocations_never_share_live_authority
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-object|identity::tests::identity_rejects_zero_slot_or_generation
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-object|identity::tests::capability_epochs_keep_lease_and_revoke_distinct
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-object|handle::tests::typed_rights_attenuation_rejects_widening_and_kind_substitution
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-ps|user::handles::table::tests::nonreusable_console_descriptors_carry_the_open_description_identity_adapter
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-ipc-runtime|ipc::tests::endpoint_and_reply_handles_decode_only_in_range_generational_identities
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-ps|multitask::process_table::tests::identity_tests::process_handle_adapts_table_slot_and_generation_to_typed_identity
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-ps|multitask::process_table::tests::identity_tests::exec_reservation_binds_process_generation_and_unique_transaction_token
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::lifecycle_transaction_ids_are_nonzero_unique_and_fail_closed_at_exhaustion
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-mm|memory::phys::tests::partial_batch_fault_returns_every_acquired_frame_exactly_once
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::retained_ref_delays_reclaim_until_drop
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::exec_seal_rejects_thread_attachment_until_cancel
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::stale_exec_transaction_id_cannot_authorize_or_cancel_live_reservation
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::process_address_space_and_exec_exit_are_serialized
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::exit_pending_wins_when_exec_is_cancelled
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::exiting_process_rejects_new_thread_attachment
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::parent_wait_is_required_before_child_reap
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::exited_child_is_consumed_once_even_while_pinned
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::concurrent_parent_waiters_cannot_replay_a_pinned_exit
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-compat|user::syscall::linux::syscalld_ops::tests::settled_child_status_is_delivered_before_eintr
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::tests::process_generations_fail_closed_instead_of_aliasing_stale_handles
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::spawn::tests::process_state_spawn_has_no_unreserved_production_alias
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-ipc-runtime|ipc::tests::kernel_transfer_ticket_binds_the_nonzero_transfer_object_generation
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-ipc-runtime|ipc::tests::transferred_handle_derivation_only_attenuates_typed_rights
capability-derivation-lifecycle/CapabilityDerivationLifecycle|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::opaque_transfer_ticket_is_exact_one_shot_and_nonce_bound
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
dvm-transport-lifecycle/DvmTransportLifecycle|kernel-io-manager|transport_lifecycle::tests::drain_closes_admission_and_waits_for_exact_claim
dvm-input-ring/DvmInputRing|inputd|dvm_protocol::tests::session_sequence_and_transport_reset_are_service_owned
dvm-input-ring/DvmInputRing|inputd|dvm_protocol::tests::invalid_checksum_and_cross_generation_record_fail_closed
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::dvm_ethernet_payload_rejects_bad_checksum_and_fragments
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::dvm_ethernet_payload_accepts_only_bounded_ipv4_or_arp
dvm-network-ring/DvmNetworkRing|driver-domain-protocol|tests::net_contract_has_two_bounded_fixed_rings
dvm-network-ring/DvmNetworkRing|kernel-io-manager|io::dvm_network::tests::control_lease_requires_nonzero_epoch_and_exact_revocation
dvm-network-ring/DvmNetworkRing|kernel-io-manager|io::dvm_network::tests::network_header_snapshot_excludes_live_atomic_cursor_bytes
dvm-network-ring/DvmNetworkRing|kernel-io-manager|io::dvm_network::tests::network_shared_ring_requires_exact_prefetchable_write_back_memory
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
dvm-block-transport/DvmBlockTransport|kernel-io-manager|io::dvm_block::tests::revoke_reports_once_before_clearing_and_is_terminal
dvm-block-transport/DvmBlockTransport|kernel-io-manager|io::dvm_block::tests::valid_flush_completion_keeps_transport_live_for_first_64kib_read
dvm-block-transport/DvmBlockTransport|xtask|kvm::tests::dvm_block_recovery_readiness_tracks_the_exact_successor_generation
dvm-block-transport/DvmBlockTransport|xtask|kvm::tests::dvm_attached_block_disk_requires_qemu_read_only_backing
dvm-block-transport/DvmBlockTransport|xtask|kvm::tests::dvm_block_read_only_media_driver_closure_is_explicit
dvm-block-transport/DvmBlockTransport|xtask|kvm::tests::dvm_block_read_only_media_geometry_matches_atapi_capacity
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::startup_not_ready_is_sleepable_not_a_fault_event
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::fixed_nonblock_ivshmem_topology_is_negative_cached_only_after_enumeration
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::readiness_may_arrive_once_but_cannot_be_withdrawn
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::readiness_publication_is_conditional_and_non_mutating_on_mismatch
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::revoke_reports_once_before_clearing_and_is_terminal
dvm-block-startup/DvmBlockStartup|storaged|block::tests::startup_wait_slice_is_bounded_and_nonzero
dvm-block-startup/DvmBlockStartup|storaged|block::tests::generation_mismatch_is_stale_not_a_fallback
dvm-block-startup/DvmBlockStartup|storaged|tests::dvm_block_e2e_marker_names_the_complete_authority_path
deferred-process-activation/DeferredProcessActivation|kernel-compat|user::syscall::linux::proc_broker_ops::tests::deferred_activation_authority_is_exact_one_shot_and_nontransferable
deferred-process-activation/DeferredProcessActivation|kernel-compat|user::syscall::linux::proc_broker_ops::tests::single_activation_resolves_claimed_requester_identity_not_loaderd_context
deferred-process-activation/DeferredProcessActivation|kernel-compat|user::syscall::linux::proc_broker_ops::deferred_activation::tests::original_grants_move_once_and_whole_batch_refusals_preserve_every_record
deferred-process-activation/DeferredProcessActivation|kernel-compat|user::syscall::linux::proc_broker_ops::deferred_activation::tests::cleanup_winning_before_the_transfer_refuses_without_consuming_the_survivor
deferred-process-activation/DeferredProcessActivation|kernel-ps|multitask::scheduler::activation::publication::tests::actual_fallible_original_grant_refuses_before_effects_and_survives_final_batch_publication
loader-request-authority/LoaderRequestAuthority|rustos-user-abi|syscall::syscall_tests::privileged_loader_operations_have_an_explicit_service_role_matrix
loader-request-authority/LoaderRequestAuthority|initd|tests::init_identity_is_published_before_any_loader_request_and_is_marked_requestless
loader-request-authority/LoaderRequestAuthority|kernel-compat|user::syscall::linux::proc_broker_ops::tests::loader_commit_revalidates_live_requester_role_before_consuming_authority
remote-file-mapping/RemoteFileMapping|rustos-user-abi|syscall::syscall_tests::statx_offload_messages_fit_inline_ipc_v1
remote-file-mapping/RemoteFileMapping|vfsd|tests::early_system_reads_chunk_larger_vfs_buffers_to_the_broker_bound
remote-file-mapping/RemoteFileMapping|kernel-compat|user::syscall::linux::proc_broker_ops::tests::truncated_file_mapping_never_commits_zero_filled_tail
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::the_entry_stub_is_the_whole_of_the_syscall_paths_fpu_custody
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::register_ipc_entry_elision_requires_exact_linux_operation
syscall-simd-lifecycle/SyscallSimdLifecycle|rustos-user-abi|syscall::ipc_fast_buffer::tests::buffer_mode_is_closed_to_data_plane_and_exact_entry_family
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::linux::ipc_ops::registers::buffer::tests::buffer_mode_rejects_foreign_family_recursion_and_non_ipc_before_dispatch
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::linux::ipc_ops::registers::buffer::tests::buffer_mode_forwards_all_six_words_once_and_preserves_commit_result
syscall-simd-lifecycle/SyscallSimdLifecycle|rustos-svc-runtime|ipc::fastcall::tests::short_selection_never_narrows_or_widens_advertised_receive_capacity
syscall-simd-lifecycle/SyscallSimdLifecycle|rustos-svc-runtime|ipc::fastcall::tests::short_receive_copies_exact_bytes_and_authenticated_metadata
syscall-simd-lifecycle/SyscallSimdLifecycle|rustos-svc-runtime|ipc::fastcall::tests::short_results_preserve_pre_and_post_commit_errors_without_retry_or_copy
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-ps|user::syscall::tests::syscall_abi_selector_is_installed_and_reset_with_the_original_stack
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::register_ipc_finalizes_before_both_user_return_routes
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-ps|multitask::scheduler::runqueue::simd_tls::tests::register_ipc_mode_is_exact_task_paired_and_cleared_on_every_replacement
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-hal|arch::simd::tests::a_wide_simd_section_covers_every_register_the_entry_stubs_leave_behind
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-ps|multitask::scheduler::tests::scheduler_block_arm_is_exact_race_safe_and_terminally_revoked
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-ps|multitask::scheduler::tests::raced_wake_never_validates_a_consumed_current_frame
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::scheduled_user_return_is_validated_before_context_publication
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::sysret_contract_rejects_forbidden_rflags
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::syscall_entry_preserves_xmm_before_any_rust_dispatch
syscall-scheduler-continuation/SyscallSchedulerContinuation|kernel-ps|multitask::scheduler::tests::scheduler_block_arm_is_exact_race_safe_and_terminally_revoked
syscall-scheduler-continuation/SyscallSchedulerContinuation|kernel-ps|multitask::scheduler::tests::raced_wake_never_validates_a_consumed_current_frame
syscall-scheduler-continuation/SyscallSchedulerContinuation|kernel-compat|user::syscall::tests::scheduled_user_return_is_validated_before_context_publication
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::acpi::tests::hpet_gas_requires_memory_qword_zero_offset_and_aligned_range
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::rtc::tests::sleep_deadline_uses_monotonic_ticks_with_ceil_and_saturation
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::rtc::tests::sleep_waiter_update_expiry_and_cancel_preserve_exact_task_ownership
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::rtc::tests::sleep_waiter_clockevent_collision_is_nonblocking_and_retryable
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::rtc::tests::actual_clockevent_refusal_and_notification_keep_deadline_until_exact_ack
clocksource-deadline/ClocksourceDeadline|kernel-hal|hooks::tests::deadline_wake_hook_forwards_exact_task_outside_registry_guard
clocksource-deadline/ClocksourceDeadline|kernel-ps|multitask::scheduler::wake_publication::tests::actual_generic_deadline_wake_admits_original_kernel_wait_without_user_reclassification
clocksource-deadline/ClocksourceDeadline|kernel-compat|user::syscall::linux::service_ops::process_time::tests::time_hot_path_admission_is_local_and_complete
clocksource-deadline/ClocksourceDeadline|kernel-compat|user::syscall::linux::service_ops::process_time::tests::a_monotonic_instant_between_two_ticks_keeps_its_own_resolution
clocksource-deadline/ClocksourceDeadline|kernel-ps|multitask::scheduler::tests::scheduler_block_arm_is_exact_race_safe_and_terminally_revoked
cpu-topology-admission/CpuTopologyAdmission|kernel-hal|arch::acpi::tests::madt_cpu_topology_is_dense_unique_bounded_and_atomic
cpu-topology-admission/CpuTopologyAdmission|kernel-hal|arch::acpi::tests::madt_rejects_truncation_and_bad_apic_override
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
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::reschedule_observation::tests::notification_may_coalesce_requests_but_consumption_reaches_the_goal
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::reschedule_observation::tests::publication_after_claim_must_create_a_new_pending_edge
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::irq::tests::reschedule_ipi_gate_retains_locked_work_and_dispatches_only_at_safe_point
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-hal|interrupt_stubs::tests::scheduler_commit_call_aligns_and_restores_incoming_rsp
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::dispatch_turn::tests::unique_turn_refuses_overlap_and_commit_cannot_release_a_live_producer
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::dispatch_turn::tests::switched_return_stays_closed_until_rsp_commit_and_duplicate_cannot_spend_next_turn
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::dispatch_turn::tests::abandoned_or_partial_publication_poison_cannot_be_reopened_by_commit
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::dispatch_turn::tests::concurrent_cpu_entry_has_one_unique_winner_not_a_global_writer_hint
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::dispatch_turn::tests::production_return_publishes_under_local_lease_and_waits_for_assembly_ack
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::dispatch_turn::tests::return_lease_refuses_replaced_current_and_remote_custody_before_publication
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::tests::current_task_ownership_ignores_offline_slots_and_is_cpu_distinct
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::irq::tests::reschedule_ipi_gate_retains_locked_work_and_dispatches_only_at_safe_point
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::frame_publication::tests::executing_or_transition_stack_owners_reject_a_matching_local_frame
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::tests::architectural_restore_is_required_exactly_for_a_task_switch
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::tests::wake_transition_publishes_one_owner_before_commit_and_claims_once_after
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::smp::tests::secondary_idle_slot_layout_is_cpu_exact_and_outside_the_root
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::smp::tests::remote_or_transition_owned_task_is_not_schedulable
scheduler-cpu-ownership/SchedulerCpuOwnership|nucleus-core|util::lockdep::tests::tracked_guard_release_requires_same_cpu_apic_and_positive_depth
scheduler-cpu-ownership/SchedulerCpuOwnership|nucleus-core|util::lockdep::tests::pending_acquire_units_cannot_consume_a_held_guard_pin
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-hal|arch::tlb_shootdown::tests::address_space_shootdown_targets_only_matching_active_roots
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-hal|arch::tlb_shootdown::tests::same_root_activation_preserves_tlb_but_root_change_reloads_cr3
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-hal|arch::tlb_shootdown::tests::reclaim_requires_every_target_to_acknowledge_the_exact_generation
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-mm|memory::address_space::tests::unmap_descriptor_plan_is_complete_before_first_pte_remove
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::process_table::tests::exec_seal_rejects_thread_attachment_until_cancel
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::process_table::tests::process_address_space_and_exec_exit_are_serialized
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::context::tests::frame_and_descriptor_custody_keep_process_backing_after_terminal_detach
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::pin_owner::tests::closing_keeps_old_readers_and_refuses_rebind_until_last_release
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::pin_owner::tests::metadata_republication_never_reopens_closing_or_resets_readers
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::pin_owner::tests::foreign_generation_saturation_and_duplicate_release_fail_closed
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::pin_owner::tests::concurrent_close_cannot_observe_zero_then_admit_a_new_reader
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::pin_owner::tests::descriptor_and_syscall_pins_delay_real_reap_and_notify_without_table_release
process-signal-delivery/ProcessSignalDelivery|kernel-ps|multitask::scheduler::linux_thread_state::owner_tests::static_thread_binding_rejects_recycled_owner_before_any_callback
process-signal-delivery/ProcessSignalDelivery|kernel-ps|multitask::scheduler::linux_thread_state::owner_tests::pending_hint_tracks_bounded_mutation_under_the_same_thread_leaf
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::context::tests::pinned_context_cannot_be_cleared_replaced_or_detached_with_its_allocation
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::context::tests::terminal_detach_is_exact_and_cannot_be_replayed_from_an_old_snapshot
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::context::tests::metadata_cannot_install_a_context_identity_from_a_foreign_slot
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::context::tests::start_identity_must_match_the_installed_funding_handle
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::frame_access::tests::unpublished_writer_owns_bytes_outside_leaf_and_blocks_detach_and_reentry
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::frame_access::tests::writer_unwind_releases_admission_without_losing_allocation_custody
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::frame_access::tests::closing_defers_reap_until_the_exclusive_writer_releases
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::frame_access::tests::remote_close_and_reader_cannot_cross_an_active_byte_writer
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::exec_transaction::publication::tests::actual_peer_exec_publishes_owned_frame_metadata_tls_and_staged_mm_before_finalization
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::exec_transaction::publication::tests::actual_peer_exec_foreign_pid_tid_and_mode_refuse_before_original_bytes_and_authority
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::exec_transaction::publication::tests::actual_exec_copied_current_without_original_bank_refuses_before_frame_and_mm_effects
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::exec_transaction::publication::tests::actual_self_exec_never_rewrites_consumed_frame_or_publishes_ready_and_retains_original_bank
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::exec_transaction::tests::image_custody_spans_staging_root_publication_and_finalization_not_just_directory_commit
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::exec_transaction::tests::image_lease_credentials_cannot_publish_a_foreign_slot_identity_handle_root_or_mode
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::exec_transaction::tests::incomplete_joined_target_refuses_before_process_staging_callback
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::scheduler::exec_transaction::tests::publication_receipt_rejects_old_mm_transaction_foreign_process_generation_and_wrong_root
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::process_table::exec_tests::real_staging_hides_new_mm_until_exact_receipt_finalizes_and_preserves_pending_exit
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::process_table::exec_tests::real_finalization_refuses_changed_live_transaction_before_visibility_or_old_mm_release
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::frame_access::tests::self_exec_requires_sole_exact_current_before_any_image_side_effect
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::frame_access::tests::self_exec_cannot_spend_prepared_custody_as_current_or_leak_writer_after_unwind
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::frame_observation::tests::copied_local_observation_never_escapes_backing_or_classifies_unknown_as_zero
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::frame_observation::tests::executing_observation_needs_exact_cpu_current_or_prepared_and_refuses_foreign_current
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::frame_observation::tests::observed_epoch_and_identity_cannot_recover_replaced_or_writing_frame_authority
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::cancellation_retains_slot_through_external_release_and_stale_generation_cannot_spend_reuse
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::ordinary_allocation_and_detachment_cannot_cross_a_reserved_slot
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|multitask::scheduler::bootstrap::tests::boot_image_refuses_empty_zero_generation_capacity_and_competing_bsp_without_reopening_poison
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|multitask::scheduler::bootstrap::tests::every_cpu_bank_ack_precedes_one_ready_release_and_incomplete_prefix_preserves_other_producer
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::bootstrap::tests::initial_producer_binds_cpu_slot_uid_generation_and_wrong_alias_refuses_before_effects
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::bootstrap::tests::duplicate_publication_cannot_spend_bank_or_ack_and_abandonment_retains_poisoned_image
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|multitask::scheduler::bootstrap::tests::ap_entry_is_generation_exact_unique_and_poisoned_entry_never_reuses_next_cpu
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|multitask::scheduler::bootstrap::tests::concurrent_bsp_claims_have_one_winner_without_global_directory_or_callback_lock
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|multitask::scheduler::bootstrap::initial_tasks::tests::actual_initial_image_ack_joins_hidden_descriptor_running_interval_and_original_backing
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|multitask::scheduler::bootstrap::initial_tasks::tests::actual_ap_initial_image_owns_only_its_cpu_and_never_reads_a_synthetic_saved_rsp
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|multitask::scheduler::bootstrap::initial_tasks::tests::actual_boot_architecture_borrows_original_image_current_and_interval_not_scalar_rsp
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-ps|multitask::scheduler::bootstrap::initial_tasks::tests::actual_boot_owner_reset_refuses_existing_physical_lifetime_instead_of_tearing_it_down
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::bootstrap::tests::duplicate_task_uid_cannot_bind_two_physical_cpu_banks
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::execution_image::tests::reserved_kernel_entry_frame_keeps_exact_rsp_trailer_registers_flags_and_canary_under_producer
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_creation::tests::actual_root_construct_owns_pinned_process_and_complete_blocked_slot_ack_before_wake
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_creation::tests::suspended_root_finishes_both_real_acks_without_wake_or_fresh_dispatch_permission
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::root_creation::tests::terminal_after_root_ack_cannot_reuse_child_before_wake_or_resurrect_retired_owner
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_creation::tests::prepared_root_is_invisible_and_copied_spawn_receipt_cannot_publish_or_cancel_real_seal
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_creation::tests::exhausted_root_nonce_refuses_before_pid_visibility_and_returns_exact_reserved_slot_for_cancel
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_creation::tests::actual_windows_materialization_error_cancels_all_unpublished_process_and_slot_custody
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_creation::tests::newly_visible_root_abandonment_retains_actual_process_pin_and_poisoned_join_without_registration
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::root_creation::tests::actual_fork_parent_cohort_retains_original_policy_and_mask_through_child_root_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_creation::tests::same_uid_root_reuse_requires_new_process_transaction_and_actual_ticket_epoch
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::root_creation::tests::original_user_actor_fork_retains_two_real_parent_loans_without_double_cohort_capture
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_creation::tests::actual_orphan_runtime_refusal_does_not_publish_pid_or_release_foreign_funding
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_creation::tests::actual_kernel_actor_does_not_grant_unbudgeted_or_missing_parent_admission
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::kernel_creation::tests::actual_kernel_publication_keeps_physical_child_through_blocked_ack_and_runnable_frame_publication
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::kernel_creation::tests::exact_kernel_zero_uid_is_complete_publication_not_absence_or_resume_permission
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::kernel_creation::tests::stop_after_slot_ack_cannot_reuse_child_before_wake_and_never_resurrects_terminal_owner
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::kernel_creation::tests::uncommitted_ticket_cannot_create_publication_backing_loan
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::execution_image::tests::reserved_kernel_writer_refuses_unqualified_boot_writer_and_nested_producer_without_byte_effect
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::execution_image::tests::complete_kernel_uid_zero_descriptor_is_not_admission_until_reserved_blocked_ack
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_slots::tests::root_reservation_owns_initialized_canary_before_bytes_and_second_creator_cannot_spend_it
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_slots::tests::root_descriptor_prefix_stays_closed_until_exact_blocked_ack_without_directory_authority
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_slots::tests::root_publication_refuses_reserved_phase_before_queue_effect_and_returns_original_producer
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_slots::tests::root_cancellation_cannot_cross_commit_or_destroy_published_binding_before_ack
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_slots::tests::competing_root_reservations_have_one_real_stack_producer_without_global_scheduler_guard
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::cancellation_ack_cannot_reopen_before_storage_release
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::concurrent_reservations_have_one_slot_owner_without_a_directory_guard
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::abandoned_reservation_poison_is_not_implicit_rollback_or_slot_reopening
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::context_and_descriptor_installation_require_the_exact_committing_ticket
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::publication_ack_refuses_a_foreign_published_identity_even_after_blocked_admission
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::publication_ack_follows_exact_context_descriptor_and_blocked_identity_admission
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::admission::tests::partial_commit_cannot_ack_or_reopen_before_runqueue_publication
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::scheduling_runtime::domain_tests::domain_descriptor_tracks_the_budget_owner_not_a_stale_catalog_copy
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::scheduling_runtime::domain_tests::captured_interval_without_member_or_catalog_still_refuses_domain_replacement
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::scheduling_runtime::domain_tests::incomplete_domain_budget_cannot_be_projected_as_reusable_absence
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-mm|memory::address_space::execution_lifetime::tests::sealing_refuses_new_execution_but_captured_refs_retain_the_root_until_cold_drain
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-mm|memory::address_space::execution_lifetime::tests::owner_drop_and_pin_release_never_run_finalization_inline
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-mm|memory::address_space::execution_lifetime::tests::old_mapping_backing_payload_survives_until_last_execution_ref_and_cold_finalizer
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-mm|memory::address_space::execution_lifetime::tests::concurrent_last_releases_enqueue_once_and_pin_cannot_cross_close
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-mm|memory::address_space::execution_lifetime::tests::bounded_single_consumer_drain_survives_reentry_and_concurrent_producers
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-mm|memory::address_space::execution_lifetime::tests::checked_reference_saturation_refuses_without_spending_owner_custody
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::stack_resources::frame_readers::tests::local_reader_refusal_preserves_owner_queue_and_membership_until_last_read_release
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::stack_resources::frame_readers::tests::direct_reader_refusal_retains_exact_handoff_and_cannot_spend_a_republished_generation
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::stack_resources::frame_readers::tests::concurrent_reader_and_owner_cas_cannot_both_acquire_saved_header_authority
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::stack_resources::frame_readers::tests::running_reader_requires_own_current_or_prepared_bank_not_merely_a_running_owner
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::stack_resources::frame_readers::tests::reader_saturation_and_writer_conflict_refuse_without_spending_any_custody
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::stack_resources::frame_readers::tests::reader_unwind_releases_only_its_borrow_and_reopens_exact_claim_admission
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::irq_entry_tests::actual_dispatch_return_terminal_and_rsp_ack_keep_original_banks_until_real_reuse_seals
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::irq_entry_tests::actual_native_resource_publication_refuses_missing_architecture_or_settled_original_interval
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::irq_entry_tests::foreign_shared::actual_foreign_shared_mm_returns_drain_frozen_exec_census_before_self_exec_and_terminal_reuse
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::irq_entry_tests::cross_root::actual_cross_root_ipc_roundtrip_and_terminal_returns_withdraw_every_born_bank_before_real_reuse
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::tests::all_lifecycle_gates_refuse_before_claim_callback_and_resume_preserves_existing_holder
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::tests::captured_execution_blocks_clear_and_absent_metadata_cannot_rebind_its_uid
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::tests::writer_and_claim_have_one_lifecycle_linearization_point_without_directory
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::tests::saturation_refuses_callback_and_unwind_leaves_no_unparked_permit
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::runqueue::affinity_payload::admission_tests::each_mask_uid_and_cpu_bound_refuses_before_callback_including_root_uid_zero
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::runqueue::affinity_payload::admission_tests::concurrent_affinity_writer_waits_until_admitted_cas_callback_has_left_leaf
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::runqueue::affinity_payload::admission_tests::overlapping_writers_publish_only_complete_commits_and_unwind_releases_admission
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::runqueue::affinity_payload::admission_tests::reset_is_empty_authority_not_unrestricted_permission_and_old_uid_cannot_rebind
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::runqueue::affinity_payload::admission_tests::invalid_mask_writer_cannot_modify_binding_version_or_previous_commit
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::affinity_change_refuses_real_local_claim_without_directory_and_preserves_queue_generation
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::affinity_change_refuses_real_direct_claim_without_consuming_handoff_custody
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::affinity_reset_cannot_discard_captured_execution_and_same_uid_exec_preserves_binding
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::runqueue::affinity_payload::admission_tests::exhausted_affinity_epoch_refuses_before_any_mask_or_migration_effect
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::captured_placement_survives_new_exclusion_and_unchanged_ack_cannot_erase_new_migration
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::current_only_placement_capture_acknowledges_only_its_exact_migration_version
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::changed_rsp_ack_keeps_late_affinity_request_and_releases_only_outgoing_identity
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::placement_owner_republication_refuses_return_before_any_placement_ack
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::staged_self_exec_keeps_captured_placement_through_receipt_and_late_migration_until_cpu_ack
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::stack_resources::context::mm_tests::unpublished_thread_mm_capture_retains_old_root_and_closed_exec_cannot_reactivate_it
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::stack_resources::context::mm_tests::unpublished_thread_commit_mm_admission_excludes_exec_staging_until_commit_releases
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_slots::tests::sleepable_windows_thread_commit_joins_new_process_mask_not_obsolete_inheritance
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::current_identity::exec_intent::tests::exact_cancel_then_new_reservation_rejects_old_image_cancel_and_drop_without_reopening
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::current_identity::exec_intent::tests::unqualified_gate_and_identity_reset_cannot_spend_owned_cold_intent
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::current_identity::exec_intent::tests::image_refusal_returns_to_same_sealed_owner_but_abandonment_and_publication_never_cancel
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::current_identity::exec_intent::tests::full_qualifiers_refuse_before_any_gate_effect_and_generation_survives_uid_reset
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::self_cancellation_closes_before_process_wait_and_pins_cpu_until_matching_receipt
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::owned_peer_image_retains_gate_through_real_receipt_finalization_and_writer_release
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::owned_intent_refuses_unqualified_writer_and_resource_teardown_before_any_effect
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::failed_image_admission_can_cancel_exact_process_but_old_credential_cannot_enter_next_exec
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::pending_exit_cancel_receipt_never_reopens_target_and_preserves_unrelated_stop
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::real_finalization_pending_exit_cannot_reopen_dispatch_after_owned_image_completion
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::self_intent_does_not_suspend_reschedulable_sibling_barrier_and_only_image_closes_gate
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::completed_self_image_opens_only_after_writer_drop_inside_completion_scope
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::tests::root_uid_zero_is_bound_authority_not_an_empty_sentinel
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::cpu_local::dispatch_turn::tests::assembly_ack_refusal_cannot_clear_a_new_open_producers_transition
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::cpu_local::dispatch_turn::tests::abandoned_admitted_rsp_consumer_quarantines_instead_of_reopening
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::cpu_local::dispatch_turn::tests::rsp_consumer_is_admitted_before_bank_ack_and_stays_closed_until_final_release
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::tests::user_identity_requires_each_abi_handle_and_pid_qualifier_before_claim
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::banks::tests::incoming_and_outgoing_identity_cannot_reset_until_exact_rsp_release
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::banks::tests::same_task_refresh_and_replaced_selection_never_release_current_identity
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::execution::banks::tests::desired_stop_denies_new_admission_but_cannot_spend_captured_current_or_outgoing
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::initial_execution_and_physical_pins_refuse_every_constructor_phase_independently_before_bank_effects
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::initial_execution_and_physical_pins_refuse_writer_and_close_separately_without_spending_next_owner
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::lifecycle_stop_and_resume_gate_real_local_cas_without_directory_and_preserve_membership
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::direct_handoff_refusal_preserves_exact_owner_and_failed_resource_admission_releases_permit
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::claimed_lifecycle_excludes_peer_writer_even_before_stack_selection_is_parked
mm-activation-close/MmActivationClose|kernel-mm|memory::address_space::execution_lifetime::activation_tests::activation_refuses_close_without_partial_sealing_until_last_grant
mm-activation-close/MmActivationClose|kernel-mm|memory::address_space::execution_lifetime::activation_tests::concurrent_activation_and_close_have_one_admission_winner
mm-activation-close/MmActivationClose|kernel-mm|memory::address_space::execution_lifetime::activation_tests::owner_drop_retains_activation_backing_and_hot_release_only_enqueues
mm-activation-close/MmActivationClose|kernel-mm|memory::address_space::execution_lifetime::activation_tests::activation_saturation_and_unwind_preserve_exact_existing_custody
mm-activation-close/MmActivationClose|kernel-mm|memory::address_space::execution_lifetime::activation_tests::closed_activation_gate_refuses_even_before_physical_close_publication
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::stack_resources::context::activation_tests::local_and_direct_mm_grant_block_real_staging_through_publication_until_rsp_ack
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::stack_resources::context::activation_tests::architecture_only_capture_blocks_mm_close_without_a_fresh_runqueue_claim
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::stack_resources::context::activation_tests::failed_cas_releases_provisional_mm_grant_without_committing_owner_or_membership
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::stack_resources::context::activation_tests::republished_owner_cannot_spend_same_uid_mm_grant_and_poison_keeps_close_excluded
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::stack_resources::context::activation_tests::replaced_uid_cannot_spend_unchanged_owner_and_mm_activation_identity
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::stack_resources::context::activation_tests::staged_exec_activation_spans_direct_hardware_receipt_and_cpu_return_completion
cpu-dispatch-preemption/CpuDispatchPreemption|nucleus-core|util::lockdep::preemption::dispatch::tests::independent_dispatch_unit_is_unique_and_refusal_cannot_release_its_owner
cpu-dispatch-preemption/CpuDispatchPreemption|nucleus-core|util::lockdep::preemption::dispatch::tests::switched_dispatch_cannot_restore_outgoing_irq_flags
cpu-dispatch-preemption/CpuDispatchPreemption|nucleus-core|util::lockdep::preemption::dispatch::tests::scoped_unwind_releases_one_unit_and_duplicate_release_refuses
cpu-dispatch-preemption/CpuDispatchPreemption|nucleus-core|util::lockdep::preemption::dispatch::tests::concurrent_dispatch_admission_has_one_non_lock_preemption_owner
cpu-dispatch-preemption/CpuDispatchPreemption|nucleus-core|util::lockdep::preemption::dispatch::tests::effective_preemption_counts_dispatch_without_inventing_a_raw_guard
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::cpu_local::dispatch_turn::preemption_tests::independent_preemption_stays_owned_across_directory_wait_not_after_raw_acquire
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::cpu_local::dispatch_turn::preemption_tests::completion_keeps_preemption_until_rsp_and_duplicate_cannot_spend_next_turn
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::cpu_local::dispatch_turn::preemption_tests::abandoned_completion_poison_preserves_published_current_and_releases_only_its_scope
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::cpu_local::dispatch_turn::preemption_tests::fixture_reset_acknowledges_parked_scope_rather_than_erasing_its_ownership
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::context::mm_tests::staged_mm_closes_old_selection_but_cpu_bank_retains_old_root_until_context_release
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::context::mm_tests::exec_receipt_moves_mm_generation_after_root_publication_and_before_old_state_release
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::frame_access::tests::exec_writer_requires_exact_quiesced_identity_and_drained_readers
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::frame_access::tests::exec_revokes_old_frame_epoch_before_bytes_and_keeps_readers_out_through_publication
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::runqueue::simd_tls::tests::simd_binding_requires_explicit_terminal_release_before_uid_reuse
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::runqueue::simd_tls::tests::only_restore_may_use_prepared_custody_and_outgoing_is_never_current
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::runqueue::simd_tls::tests::exclusive_token_refuses_overlap_before_any_raw_image_is_borrowed
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::runqueue::simd_tls::tests::exclusive_simd_guard_survives_refusal_and_unwind_without_leaking_access
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::runqueue::simd_tls::tests::remote_simd_access_never_aliases_a_live_mutable_image
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::stack_resources::cpu_leases::tests::simd_replacement_requires_exclusive_current_or_terminal_custody
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::stack_resources::context::tests::current_scalar_snapshot_requires_joined_descriptor_and_current_custody
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::context::tests::exec_patch_requires_exact_identity_and_preserves_non_exec_fields
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::context::tests::exec_cannot_use_a_process_label_to_bypass_the_task_binding
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::stack_resources::context::tests::concurrent_readers_observe_complete_exec_metadata_without_catalog_borrows
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::task_start::tests::running_entry_requires_current_custody_and_exact_complete_identity
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::task_start::tests::execution_pin_blocks_descriptor_clear_and_recycled_identity
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::task_start::tests::same_identity_exec_changes_entry_without_spending_a_reference
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::task_start::tests::live_descriptor_and_allocation_binding_cannot_be_rebound_without_reset
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::task_start::tests::descriptor_identity_is_immutable_even_before_frame_binding
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::task_start::tests::concurrent_entry_refresh_never_changes_the_monotonic_task_binding
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::tests::exec_cannot_rebind_a_live_thread_to_its_process_identity
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::cpu_leases::tests::claim_to_assembly_commit_retains_both_allocations
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::cpu_leases::tests::replaced_selection_releases_only_its_prepared_reference
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::cpu_leases::tests::retained_current_refresh_never_releases_its_final_reference
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::cpu_leases::tests::foreign_selection_cannot_replace_current_custody
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::cpu_leases::tests::publication_and_remote_close_cannot_free_the_outgoing_rsp_early
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::tests::allocation_pin_prevents_reset_release_and_identity_reuse
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::tests::exact_geometry_and_owned_canary_do_not_follow_foreign_publications
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::tests::concurrent_readers_keep_allocation_until_the_last_pin_is_released
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::tests::closing_cannot_be_undone_by_metadata_republication
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::smp::tests::remote_retirement_waits_only_for_another_cpus_running_slot
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::publication::tests::actual_activation_holds_original_frame_and_binding_through_callback_before_queue_and_ack
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::activation::publication::tests::actual_terminal_during_callback_keeps_hidden_original_retired_member_and_wakes_only_peer
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::activation::publication::tests::actual_callback_unwind_leaves_all_suspended_and_releases_frame_loans_for_exact_retry
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::activation::publication::tests::actual_invalid_suspended_frame_panics_before_consuming_authority_without_releasing_suspension
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::publication::tests::actual_activation_exact_capacity_commits_all_first_turn_records_without_overflow_or_replay
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::publication::tests::actual_priority_saturation_degrades_to_original_spawn_fifo_not_atomic_count_overflow
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::activation::publication::tests::actual_remote_activation_publishes_original_mailbox_before_complete_ack_and_bound_frame
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::activation::publication::tests::remote_activation_notifies_after_all_claim_gates_release_even_with_existing_mailbox_edge
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::publication::tests::actual_activation_preserves_strict_class_sleeper_floor_without_priority_donation_shortcut
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::activation::publication::tests::actual_activation_refuses_copied_current_slot_without_original_physical_cpu_bank_before_callback
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::publication::tests::actual_single_activation_recomputes_same_band_floor_after_wake_for_original_spawn_donation
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::publication::tests::actual_stop_gates_remain_ineligible_after_owned_activation_without_queue_or_priority_records
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::tests::exact_activation_capture_denies_bounds_uid_alias_duplicate_replay_and_unsuspended_binding_before_callback
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::tests::shared_commit_retains_every_binding_and_suspend_through_authority_callback_and_one_release
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::tests::terminal_stop_and_hidden_metadata_during_callback_never_reawaken_or_consume_next_binding
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::tests::callback_unwind_before_task_effects_keeps_targets_suspended_and_releases_only_captured_refs
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::tests::concurrent_task_affinity_refuses_under_same_identity_seal_without_spending_mask_epoch_or_callback
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::tests::hint_capacity_duplicate_and_counter_exhaustion_refuse_before_spending_queue_or_remaining
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::activation::tests::competing_disjoint_cohorts_cannot_overflow_one_cpu_priority_queue_after_same_empty_observation
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::reuse::tests::sealed_terminal_row_denies_foreign_unowned_reset_and_deferred_producer_until_complete_ack
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::reuse::tests::each_terminal_custody_independently_refuses_seal_without_spending_nonce
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::reuse::tests::abandoned_effects_keep_terminal_row_quarantined_and_nonce_exhaustion_never_wraps
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::reuse::tests::before_effect_rollback_is_exact_and_old_seal_cannot_release_next_producer_nonce
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::reuse::tests::detached_metadata_prefix_keeps_new_creator_closed_until_exact_reuse_ack_and_clears_hidden_binding
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::reuse::tests::foreign_pristine_or_old_uid_cannot_close_live_slot_and_epoch_exhaustion_has_no_partial_gate
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::reuse::tests::unowned_storage_detach_and_early_ack_cannot_spend_owned_reclaim_or_reopen_constructor
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::reuse::tests::abandoned_detached_reclaim_quarantines_even_empty_storage_and_next_nonce_cannot_be_spent_by_old_key
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::reuse::tests::close_refuses_all_constructor_phases_without_partial_close_or_spending_creator
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::stack_resources::reuse::tests::pristine_descriptor_absence_cannot_discard_unowned_close_or_reader_custody_on_reservation
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::tests::cleanup_requires_exact_claim_and_ack_before_slot_reset_or_reuse
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::tests::concurrent_consumers_claim_exactly_once_without_directory_serialization
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::tests::claimed_cleanup_cannot_be_reset_before_ack
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::tests::pending_cleanup_cannot_be_reset_before_claim
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::tests::acknowledged_cleanup_must_be_reset_before_new_publication
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-hal|arch::tlb_shootdown::tests::reclaim_requires_every_target_to_acknowledge_the_exact_generation
robust-futex-owner-death/RobustFutexOwnerDeath|kernel-mm|memory::address_space::atomic_user::tests::atomic_user_u32_requires_aligned_complete_user_word
robust-futex-owner-death/RobustFutexOwnerDeath|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::robust_owner_death_preserves_waiters_and_rejects_foreign_owner
robust-futex-owner-death/RobustFutexOwnerDeath|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::retired_task_cleanup_is_exact_and_idempotent
per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle|kernel-hal|arch::timer::tests::tsc_deadline_interval_and_catchup_are_strictly_future_bounded
per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle|kernel-hal|arch::smp::tests::cpu_lifecycle_publication_is_dense_generation_bound_and_ordered
per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle|kernel-ps|multitask::irq::tests::syscall_tail_consumes_every_deferred_or_handoff_request_exactly_once
per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle|kernel-ps|multitask::irq::tests::periodic_idle_tick_stays_local_only_without_any_queue_or_request
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::tests::scheduler_block_arm_is_exact_race_safe_and_terminally_revoked
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::tests::raced_wake_never_validates_a_consumed_current_frame
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::tests::live_noncurrent_task_must_retain_one_scheduler_state_owner
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::runqueue::tests::wake_runnable_predicate_uses_owner_bit_but_respects_wait_lifecycle
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::runqueue::tests::owner_run_intent_keeps_running_queued_and_migrating_states_distinct_from_handoff
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
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::tests::dispatch_fairness_and_handoff_state_is_cpu_isolated
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runqueue::tests::idle_steal_uses_single_owner_mailbox_transfer
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runqueue::tests::affinity_rehome_invalidates_and_coalesces_old_mailbox_generations
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runqueue_policy::tests::active_balance_requires_more_than_one_excess_runnable
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::tests::overdue_system_continuation_precedes_a_fresh_latency_handoff
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::irq::tests::syscall_tail_consumes_every_deferred_or_handoff_request_exactly_once
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::tests::runtime_profile_distinguishes_switches_roots_and_migrations
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::tests::runtime_profile_lock_totals_and_maxima_are_destructive
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::eligible_user_reservation_preempts_the_ordinary_system_candidate
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::borrowed_context_selects_published_owner_stamp_or_fails_closed
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::local_affinity_admits_only_the_intersection_and_a_representable_cpu
scheduler-local-authority/SchedulerLocalAuthority|rustos-scheduler-frame-gate|tests::only_stable_negative_publications_can_be_skipped
scheduler-local-authority/SchedulerLocalAuthority|rustos-scheduler-frame-gate|tests::budget_denial_requires_exact_unreplaced_reply_custody
scheduler-local-authority/SchedulerLocalAuthority|rustos-scheduler-frame-gate|tests::selected_frame_must_match_both_owner_and_saved_stack
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::final_local_frame_rechecks_exact_generation_rsp_and_validated_payload
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::empty_fair_scan_claims_only_the_exact_local_idle_generation
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::overdue_system_prefers_oldest_age_then_vruntime_without_reordering_ties
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::deferred_catalog_fair_scan_runs_only_for_incomplete_local_observations
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::fair_retry::tests::torn_fair_observations_retry_without_authorizing_a_partial_candidate
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::fair_retry::tests::incomplete_early_policy_never_means_no_due_peer
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::hint_priority::tests::incomplete_system_presence_cannot_release_a_user_hint
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::hint_priority::tests::latency_class_requires_a_stable_user_publication
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::dispatch_policy::tests::latency_hint_does_not_transfer_user_budget_to_a_recycled_slot
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::frame_publication::tests::retained_current_frame_requires_exact_local_and_execution_generation
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::current_frame::tests::retained_current_frame_rejects_foreign_cpu_changed_rsp_and_republished_owner
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::complete_yield_prefers_an_eligible_peer_before_reusing_current
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::unstable_epoch_rejects_before_candidate_scan
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::fair_shadow_uses_vruntime_then_requested_tie_break
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::local_dispatch::tests::three_candidate_fair_order_is_permutation_invariant
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::runqueue::tests::activation_token_cannot_claim_a_republished_local_generation
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::activation_batch_tests::spawn_handoff_is_fifo_deduplicated_and_precedes_ipc_handoff
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::handoffs::published_identity_tests::first_turn_cannot_steal_identity_after_a_slot_reuse
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::sync_handoff::tests::consumed_generic_fifo_binds_only_the_exact_local_owner
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::runqueue::tests::local_dispatch_claim_rejects_republished_generation_without_consuming_queue
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::runqueue::tests::local_claim_losing_lock_free_owner_update_leaves_queue_and_fallback_custody_intact
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::runqueue_policy::tests::a_failed_current_claim_never_resolves_back_to_the_current_slot
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::scheduling_context::tests::abandoned_dispatch_preparation_only_matures_due_refills
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::donation_ledger::tests::ordinary_nested_calls_borrow_one_root_context_without_system_promotion
scheduler-active-balance/SchedulerActiveBalance|kernel-ps|multitask::scheduler::runqueue_policy::tests::active_balance_is_due_first_then_every_eighth_loaded_opportunity
scheduler-active-balance/SchedulerActiveBalance|kernel-ps|multitask::scheduler::smp::tests::source_migration_requires_exact_runnable_local_owner_and_target_affinity
scheduler-thread-demotion/SchedulerThreadDemotion|kernel-ps|multitask::scheduler::current_class::tests::actual_current_self_demotion_caps_base_weight_without_revoking_original_reply_class
scheduler-thread-demotion/SchedulerThreadDemotion|vfsd|tests::ui_bootstrap_demotion_requires_successful_terminal_snapshot_reply
scheduler-thread-demotion/SchedulerThreadDemotion|loaderd|tests::ui_bootstrap_demotion_is_custodied_until_terminal_reply
scheduler-thread-demotion/SchedulerThreadDemotion|uiserver|sys::tests::only_bounded_bootstrap_roles_retain_inherited_boot_class
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::tests::ipc_admission_exports_only_the_live_bound_scheduling_context
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::tests::nested_passive_server_runtime_is_billed_to_the_root_caller_context
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::tests::deadline_domains_require_per_cpu_utilization_headroom
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::domain_admission::tests::captured_domain_selection_excludes_replacement_before_context_and_duplicate_grants_drain_once
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::domain_admission::tests::live_context_member_blocks_policy_reuse_after_capture_drop_and_close_until_exact_release
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::domain_admission::tests::context_admission_refuses_foreign_domain_generation_policy_and_identity_without_spending_members
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::domain_admission::tests::deadline_quota_sums_exact_overlapping_cpu_masks_and_rounds_up_without_partial_replacement
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::domain_admission::tests::competing_deadline_domains_cannot_both_spend_one_cpu_utilization_capacity
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::domain_admission::tests::exhausted_domain_generation_refuses_before_descriptor_publication_and_never_wraps
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::domain_admission::tests::full_registry_refuses_new_domain_without_spending_existing_capture_or_policy_generation
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::tests::production_user_slot_publication_rejects_an_unbudgeted_context
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::scheduling_context::tests::timeout_fault_is_one_shot_observable_and_never_retried
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::scheduling_context::tests::immutable_budget_refill_cannot_reverse_an_earlier_and_later_admission
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::scheduling_context::tests::published_budget_stamp_rejects_a_cpu_outside_its_live_policy_mask
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::return_cancels_unused_grant_and_notifies_final_closing_funder
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::cancellation_cannot_settle_active_funding_or_clear_a_foreign_cpu_grant
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::changed_return_requires_exact_running_identity_generation_and_active_interval
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::bootstrap_running_work_has_an_interval_before_it_becomes_idle
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::captured_funding_survives_close_and_blocks_context_and_domain_reuse
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::prepared_grant_keeps_its_funder_after_donation_publication_changes
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::pinned_exhaustion_keeps_the_captured_reply_not_a_replacement_donation
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::a_cpu_interval_is_consumed_once_and_uses_only_its_own_clock_domain
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::grant_pin_cannot_authorize_credit_drained_after_preparation
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::execution_interval::tests::funding_pin_rejects_wrong_owner_cpu_and_identity_without_leaking_custody
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_ledger::tests::ordinary_nested_calls_borrow_one_root_context_without_system_promotion
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_ledger::tests::multithreaded_server_charge_tokens_restore_the_previous_live_reply
scheduling-context-budget/SchedulingContextBudget|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::direct_bootstrap_consumes_exact_rootd_scheduling_authority_before_spawn
scheduling-context-budget/SchedulingContextBudget|kernel-compat|user::process::tests::production_process_spawn_surface_requires_scheduling_authority
scheduling-context-budget/SchedulingContextBudget|kernel-executive|boot::tests::rootd_bootstrap_is_published_with_a_bounded_scheduling_context
scheduling-context-budget/SchedulingContextBudget|rootd|tests::scheduling_policy_is_owned_by_the_immutable_service_manifest|host-test
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::runqueue::tests::direct_handoff_bypasses_the_fair_runqueue_and_is_cpu_exact
ipc-fast-handoff/IpcFastHandoff|kernel-ipc-runtime|ipc::tests::fast_call_tests::fast_call_uses_fixed_frame_and_exact_receiver_caller_identities
ipc-fast-handoff/IpcFastHandoff|kernel-ipc-runtime|ipc::tests::fast_call_tests::fast_calls_reuse_caller_reply_slot_without_reusing_authority
ipc-fast-handoff/IpcFastHandoff|kernel-ipc-runtime|ipc::tests::fast_call_tests::alternating_queued_and_fast_replies_preserve_exact_caller_custody
ipc-fast-handoff/IpcFastHandoff|kernel-ipc-runtime|ipc::tests::fast_call_tests::fast_call_rollback_restores_exact_front_waiter_and_custody
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::tests::fast_ipc_commit_requires_exact_typed_waits_and_mutates_both_peers_once
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::runqueue::tests::budget_refusal_after_consuming_a_direct_handoff_retains_claimable_custody
ipc-fast-handoff/IpcFastHandoff|syscalld|fast_offload::tests::compact_id_wire_is_fixed_frame_bounded_sender_exact_and_lossless
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::synchronous_handoff_tests::synchronous_ipc_handoff_is_fifo_deduplicated_and_fairness_bounded
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::synchronous_handoff_tests::reply_wake_token_mint_requires_exact_task_and_dispatch_custody
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::donation_publication::tests::actual_normal_reply_withdraws_only_original_reply_namespace_before_wake_or_refusal
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_wake_token_rejects_stale_owner_generation_migration_and_retirement
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_generation_refresh_replaces_in_place_and_stale_generation_loses_urgency
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::generic_handoff_cannot_downgrade_reply_generation_custody
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::fresh_direct_owner_replaces_only_a_stale_reply_token_at_the_same_fifo_position
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::stale_reply_enqueue_has_no_scheduler_or_global_fallback
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_enqueue_rechecks_owner_after_the_exact_target_queue_mutation
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::ordering_publication::tests::actual_fifo_qualification_releases_ordering_leaf_before_original_binding_and_replenishment
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::scheduler::dispatcher::fifo_witnesses::actual_remote_activation_mailbox_is_admitted_before_atomic_first_turn_qualification
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::runqueue::tests::direct_handoff_predicate_rejects_running_and_migrating_even_if_runnable
synchronous-ipc-handoff-concurrency/SynchronousIpcHandoffConcurrency|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_wake_token_rejects_stale_owner_generation_migration_and_retirement
synchronous-ipc-handoff-concurrency/SynchronousIpcHandoffConcurrency|kernel-ps|multitask::scheduler::sync_handoff::tests::stale_reply_enqueue_has_no_scheduler_or_global_fallback
synchronous-ipc-handoff-concurrency/SynchronousIpcHandoffConcurrency|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_enqueue_rechecks_owner_after_the_exact_target_queue_mutation
synchronous-ipc-handoff-concurrency/SynchronousIpcHandoffConcurrency|kernel-ps|multitask::scheduler::runqueue::tests::direct_handoff_predicate_rejects_running_and_migrating_even_if_runnable
ipc-priority-inheritance/IpcPriorityInheritance|kernel-ps|multitask::scheduler::tests::synchronous_ipc_donation_promotes_and_revokes_a_transitive_user_chain
ipc-priority-inheritance/IpcPriorityInheritance|kernel-ps|multitask::scheduler::donation_publication::tests::actual_queued_receive_moves_original_reply_funding_from_wake_candidate_to_consumer
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
page-cache-lifecycle/PageCacheLifecycle|pagerd|page_cache::tests::concurrent_miss_has_one_load_owner_and_exact_backing_generation
page-cache-lifecycle/PageCacheLifecycle|pagerd|page_cache::tests::private_cow_never_changes_the_shared_clean_source
page-cache-lifecycle/PageCacheLifecycle|pagerd|page_cache::tests::dirty_page_requires_exact_writeback_before_reclaim
page-cache-lifecycle/PageCacheLifecycle|pagerd|page_cache::tests::reclaim_releases_frame_only_after_exact_tlb_ack_generation
page-cache-lifecycle/PageCacheLifecycle|pagerd|page_cache::tests::provider_restart_revokes_only_the_exact_old_backing_generation
page-cache-lifecycle/PageCacheLifecycle|pagerd|page_cache::tests::provider_revoke_quarantines_mapped_frame_until_release_and_exact_tlb_ack
page-cache-lifecycle/PageCacheLifecycle|pagerd|page_cache::tests::dirty_revoked_page_requires_exact_new_backing_reauthorization
page-cache-lifecycle/PageCacheLifecycle|pagerd|page_cache::tests::revoked_loading_owner_cannot_republish_or_reload_stale_generation
pager-frame-grant-lifecycle/PagerFrameGrantLifecycle|kernel-mm|memory::frame_capability::tests::exact_binding_consumes_frame_grant_once
pager-frame-grant-lifecycle/PagerFrameGrantLifecycle|kernel-mm|memory::frame_capability::tests::reused_slot_never_accepts_stale_frame_handle
pager-frame-grant-lifecycle/PagerFrameGrantLifecycle|kernel-mm|memory::frame_capability::tests::rights_are_subset_checked_and_wx_is_never_published
pager-fault-slot-lifecycle/PagerFaultSlotLifecycle|kernel-ps|multitask::pager_fault::tests::fault_slot_is_exact_and_consumed_once
pager-fault-slot-lifecycle/PagerFaultSlotLifecycle|kernel-ps|multitask::scheduler::tests::pager_fault_handoff_requires_exact_waits_and_donates_after_worker_wake
pager-fault-slot-lifecycle/PagerFaultSlotLifecycle|kernel-ps|multitask::pager_fault::tests::cancellation_wins_over_late_reply_claim
pager-region-agreement/PagerRegionAgreement|rustos-user-abi|pager::region_edit::tests::an_interior_unmap_splits_into_two_mappings_like_linux
pager-region-agreement/PagerRegionAgreement|rustos-user-abi|pager::region_edit::tests::surviving_fragments_are_exactly_the_region_minus_the_edit
pager-region-agreement/PagerRegionAgreement|rustos-user-abi|pager::region_edit::tests::prot_none_is_a_ring0_vma_and_a_pager_removal
pager-region-agreement/PagerRegionAgreement|kernel-ps|multitask::pager_vma::tests::ring0_rewrite_matches_the_shared_range_edit_rule
pager-region-agreement/PagerRegionAgreement|kernel-ps|multitask::pager_vma::tests::a_split_with_no_free_vma_slot_refuses_and_keeps_every_region
pager-region-agreement/PagerRegionAgreement|kernel-ps|multitask::pager_vma::tests::sparse_unmap_is_idempotent_and_protect_commit_still_require_coverage
pager-region-agreement/PagerRegionAgreement|kernel-ps|multitask::pager_vma::tests::unmap_partially_initialized_range_and_touching_only_preserve_exact_authority
pager-region-agreement/PagerRegionAgreement|pagerd|tests::an_interior_release_keeps_both_remainders_and_they_still_fault
pager-region-agreement/PagerRegionAgreement|pagerd|tests::a_split_that_cannot_fit_refuses_and_keeps_the_region_whole
pager-region-agreement/PagerRegionAgreement|pagerd|tests::an_interior_protect_narrows_only_the_edited_span
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::actual_range_rewrite_gap_is_retryable_until_commit_or_original_rollback
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::actual_lookup_rechecks_whole_rewrite_epoch_after_the_scan
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::actual_exhausted_range_rewrite_epoch_refuses_before_withdrawal_or_pte_mutation
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::publication_stamps_exact_process_mm_and_nonzero_vma_generation
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::overlap_and_permission_escalation_fail_closed
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::exec_generation_change_and_revoked_region_never_match
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::reservation_commit_state_splits_and_gates_fault_access
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::prot_none_reserved_and_committed_regions_publish_but_never_admit_access
pager-vma-publication/PagerVmaPublication|kernel-ps|multitask::pager_vma::tests::publication_reclaims_stale_process_generation_before_slot_reuse
pager-vma-publication/PagerVmaPublication|kernel-compat|user::syscall::linux::mm_broker_ops::pager_admission::tests::pager_vma_failure_telemetry_has_stable_distinct_classes
cow-frame-lifecycle/CowFrameLifecycle|kernel-ps|multitask::pager_vma::tests::fork_hold_makes_fault_readers_retry_without_publishing_a_hole
cow-frame-lifecycle/CowFrameLifecycle|kernel-compat|user::syscall::linux::mm_broker_ops::pager_admission::tests::inherited_file_private_reservation_preserves_dual_abi_policy
cow-frame-lifecycle/CowFrameLifecycle|kernel-compat|user::syscall::linux::mm_broker_ops::pager_admission::tests::inherited_anonymous_private_gets_fresh_identity_and_rejects_shared
cow-frame-lifecycle/CowFrameLifecycle|kernel-mm|memory::phys::frame_descriptor_ledger::tests::anonymous_cow_removes_one_alias_then_promotes_the_survivor_in_place
cow-frame-lifecycle/CowFrameLifecycle|kernel-mm|memory::phys::frame_descriptor_ledger::tests::failed_fork_alias_rolls_back_to_the_exact_exclusive_owner
cow-frame-lifecycle/CowFrameLifecycle|kernel-ps|multitask::user_copy::tests::copyout_cow_requires_committed_private_write_authority_for_both_abis
page-table-lifecycle/PageTableLifecycle|kernel-compat|user::syscall::linux::mm_broker_ops::tests::mapping_range_rejects_noncanonical_and_wrapping_addresses
page-table-lifecycle/PageTableLifecycle|kernel-compat|user::syscall::linux::mm_broker_ops::tests::mapping_cursor_advances_to_the_rounded_region_end
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::validate_user_page_range_rejects_unaligned_or_oob
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::user_page_flags_enforce_wx_and_reject_huge_pages
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::protection_span_preflight_rejects_a_hole_before_commit
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::tests::unmap_descriptor_plan_is_complete_before_first_pte_remove
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::pager_fault_mapping::tests::user_unmap_preflights_owned_pages_separated_by_holes
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::pager_fault_mapping::tests::user_unmap_preflights_owned_and_borrowed_mix
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::pager_fault_mapping::tests::user_unmap_repeated_no_present_plans_are_empty
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::pager_fault_mapping::tests::user_unmap_ownership_refusals_leave_all_entries_intact
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::pager_fault_mapping::tests::user_unmap_invalid_leaf_refusal_leaves_all_entries_intact
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::pager_fault_mapping::tests::user_unmap_late_invalid_directory_refuses_before_mutation
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::pager_fault_mapping::tests::user_unmap_capacity_refusal_precedes_mutation
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::address_space::pager_fault_mapping::tests::user_unmap_external_segments_validate_geometry_and_half_open_membership
page-table-map-transaction/PageTableMapTransaction|kernel-mm|memory::address_space::rollback::tests::leaf_rollback_runs_in_reverse_publication_order
page-table-map-transaction/PageTableMapTransaction|kernel-mm|memory::address_space::pager_fault_mapping::tests::a_published_table_entry_must_be_a_present_non_huge_user_table
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::kernel_vm::tests::direct_map_update_bounds_are_aligned_nonempty_and_nonwrapping
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::kernel_vm::tests::kernel_segment_protection_rejects_writable_executable_authority
page-table-lifecycle/PageTableLifecycle|syscalld|mmap_policy::tests::invalid_backing_is_rejected_before_a_fixed_replace_plan_exists
page-table-lifecycle/PageTableLifecycle|syscalld|vma_policy::tests::sparse_munmap_is_one_transaction_and_commits_policy_only_on_success
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::usable_region_spans_filter_and_trim_to_direct_map
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::bitmap_allocator_reuses_freed_frames
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::bounded_allocator_stays_under_limit
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::reserve_phys_range_removes_kernel_image_from_free_set
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::tests::fixed_range_claim_is_atomic_exact_and_not_reallocatable
physical-frame-lifecycle/PhysicalFrameLifecycle|kernel-mm|memory::phys::frame_descriptor_ledger::tests::one_catalog_tracks_tables_and_exact_data_leaves
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
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-compat|user::syscall::linux::broker_ops::lifecycle_broker_ops::tests::only_original_final_fault_token_commits_process_termination
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
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waiter_capacity_admits_one_maximal_arm_and_bounded_concurrency
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waitset_provider_authority_maps_to_one_exact_service
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::input_open_description_survives_dup_until_the_final_close
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::waiter_removal_before_scheduler_arm_is_detected_by_presence
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::ipc_ops::tests::service_endpoint_epoch_changes_on_every_publication_boundary
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::service_ops::poll_epoll::tests::object_observations_are_deduplicated_and_keep_the_newest_generation
userspace-wait-set/UserspaceWaitSet|kernel-compat|user::syscall::linux::broker_ops::waitset_broker_ops::tests::exact_object_publication_never_removes_a_foreign_wait_set
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
userspace-wait-set/UserspaceWaitSet|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::console_token_liveness_tracks_descriptor_references_not_snapshots
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
userspace-wait-set/WaitSetRegistry|vfsd|tests::epoll_object_cap_admits_the_boundary_and_rejects_one_more_unchanged
userspace-wait-set/WaitSetRegistry|vfsd|tests::checkpoint_restore_over_capacity_is_atomic
userspace-wait-set/UserspaceWaitSet|uiserver|wayland::tests::wayland_readiness_requires_one_dispatch_before_rearm
userspace-wait-set/UserspaceWaitSet|uiserver|wayland::tests::wayland_readiness_retries_only_transient_transport_failures
userspace-wait-set/UserspaceWaitSet|uiserver|wayland_accept::tests::wayland_accept_uses_blocking_readiness_not_probe_cadence
userspace-wait-set/UserspaceWaitSet|uiserver|input_loop::tests::input_reader_uses_blocking_epoll_readiness_not_probe_cadence
userspace-wait-set/UserspaceWaitSet|netd|packet_provider_state_tests::inet_ingress_publishes_only_socket_state_transitions
userspace-wait-set/UserspaceWaitSet|runtimed|session::tests::console_readiness_generation_advances_only_when_input_becomes_ready
userspace-wait-set/UserspaceWaitSet|runtimed|session::tests::console_close_revokes_readiness_without_resurrecting_the_session
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::pending_slot_reservation_is_global_and_bounded
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::poisoned_deferred_queue_is_drained_for_fail_closed_replies
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::admission_clamps_each_operation_class_once_without_freshening
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::expired_work_is_rejected_before_any_queue_reservation
netd-deferred-reply/NetdDeferredReply|netd|local_socket_poll_tests::expired_detached_local_poll_replies_and_releases_exactly_once
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::cancelled_transfer_moves_its_open_description_to_deferred_cleanup
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::opaque_transfer_ticket_is_exact_one_shot_and_nonce_bound
ipc-transfer-authority/IpcTransferAuthority|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::opaque_transfer_ticket_is_exact_one_shot_and_nonce_bound
ipc-transfer-authority/IpcTransferAuthority|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::unbound_stream_transfer_requires_exact_receive_time_process_binding
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::table::tests::receive_reservations_are_invisible_and_publish_atomically
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::table::tests::cancelled_receive_reservation_is_reusable
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::table::tests::stale_reservation_cannot_cancel_or_commit_after_exec_boundary
ipc-handle-transfer/IpcHandleTransfer|kernel-compat|user::syscall::linux::service_ops::vfs_socket::tests::transferred_input_description_keeps_the_waitset_service_reference
process-signal-delivery/ProcessSignalDelivery|kernel-compat|user::syscall::linux::support::tests::signal_selection_revalidates_pending_mask_and_uncatchable_policy
process-signal-delivery/ProcessSignalDelivery|kernel-compat|user::syscall::linux::support::tests::restored_signal_mask_cannot_block_kill_or_stop
process-signal-delivery/ProcessSignalDelivery|kernel-compat|user::syscall::linux::process_termination_tests::x86_user_faults_have_linux_wait_signal_status
process-signal-delivery/ProcessSignalDelivery|kernel-compat|user::syscall::linux::broker_ops::lifecycle_broker_ops::tests::only_original_final_fault_token_commits_process_termination
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
endpoint-receiver-wakeup/EndpointReceiverWakeup|kernel-ipc-runtime|ipc::tests::receive_transaction_tests::receive_or_wait_is_one_endpoint_transaction_for_empty_queued_and_fast
endpoint-receiver-wakeup/EndpointReceiverWakeup|kernel-ipc-runtime|ipc::tests::receive_transaction_tests::concurrent_enqueue_and_receive_have_no_unowned_empty_window
endpoint-receiver-wakeup/EndpointReceiverWakeup|kernel-ipc-runtime|ipc::tests::receive_transaction_tests::receive_rejection_preserves_exact_fast_frame_and_publishes_no_waiter
endpoint-receiver-wakeup/EndpointReceiverWakeup|kernel-ipc-runtime|ipc::tests::receive_transaction_tests::receive_expiry_still_delivers_queued_request_without_registering_waiter
endpoint-receiver-wakeup/EndpointReceiverWakeup|kernel-ipc-runtime|ipc::tests::receive_transaction_tests::receiver_registration_is_bounded_deduplicated_and_refreshes_capacity
endpoint-receiver-wakeup/EndpointReceiverWakeup|kernel-ipc-runtime|ipc::tests::receive_transaction_tests::received_request_custody_observation_requires_actual_dequeue_and_exact_live_reply
endpoint-receiver-wakeup/EndpointReceiverWakeup|kernel-compat|user::syscall::linux::ipc_ops::ipc_call_admission::tests::actual_received_call_survives_a_refused_sender_wake_without_certifying_queued_or_uncustodied_requests
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
service-bootstrap-lifecycle/ServiceBootstrapLifecycle|initd|tests::libc_errno_is_not_misreported_as_eperm_at_the_readiness_or_rpc_boundary
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|bootstrap_barrier::tests::independent_bootstrap_activation_overlaps_only_before_consumer_barriers
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|bootstrap_barrier::tests::dependency_packages_exclude_spawned_but_unadmitted_endpoints
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|bootstrap_barrier::tests::bootstrap_barrier_requires_every_exact_endpoint_admission
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|boot_order::tests::runtimed_bootstrap_does_not_wait_for_storage_dvm_publication
post-init-bootstrap-barrier/PostInitBootstrapBarrier|initd|tests::endpoint_barrier_wait_is_exact_pid_bound_and_bounded
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::activation_batch_tests::spawn_handoff_is_fifo_deduplicated_and_precedes_ipc_handoff
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::tests::overdue_system_continuation_precedes_a_fresh_latency_handoff
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::current_identity::cohort::tests::cohort_admission_is_closed_through_every_partial_publication_and_one_release_opens_all
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::current_identity::cohort::tests::complete_release_opens_every_member_before_slow_per_slot_unseal_without_a_global_claim_lock
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::cohort::tests::failed_member_and_duplicate_preflight_roll_back_only_exact_binding_pins_and_epoch_never_replays
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::current_identity::cohort::tests::callback_failure_before_effects_can_abort_but_partial_effects_quarantine_binding_and_admission
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::owned_cohort_gate_refuses_real_local_and_direct_cas_until_commit_preserving_exact_custody
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::cold_binding_pin_refuses_active_writer_closing_and_foreign_resource_uid_before_any_seal
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::tests::creation_grant_freezes_process_mask_and_registers_child_before_writer_can_capture_membership
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::process_placement::tests::committed_query_stays_old_during_partial_masks_and_joins_open_only_after_published_unseal
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-ps|multitask::process_placement::tests::activation_credential_cannot_publish_process_mask_or_spend_pending_affinity_transaction
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::tests::process_membership_and_mask_survive_same_uid_exec_abi_change_without_old_abi_creation_permission
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::tests::process_and_creator_generations_abi_terminal_and_duplicate_members_refuse_without_spending_join
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::process_placement::tests::exhausted_transaction_and_partial_failure_never_reopen_or_alias_next_process_generation
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exec_membership::tests::exec_census_freezes_new_join_root_and_mask_change_before_member_publication_until_exact_finish
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exec_membership::tests::pending_creation_reader_refuses_exec_census_without_spending_epoch_and_drained_child_is_in_snapshot
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::process_placement::exec_membership::tests::retired_member_removal_preserves_captured_uid_and_seal_while_recycled_slot_is_not_new_member
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::process_placement::exec_membership::tests::missing_target_foreign_handle_and_exhausted_epoch_refuse_before_partial_exec_gate
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::process_placement::exec_membership::tests::before_image_drop_releases_only_exact_census_and_old_epoch_cannot_spend_next_owner
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::process_placement::exec_membership::tests::staged_abandonment_retains_process_census_quarantine_instead_of_reopening_creators
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exec_membership::tests::census_retains_actual_process_object_pin_until_release_outside_leaf_and_cancel_keeps_masks
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::exec_transaction::tests::census::retired_captured_sibling_with_execution_refs_is_not_quiescent_and_is_never_retired_twice
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::exec_transaction::tests::census::recycled_catalog_uid_cannot_be_retired_by_old_census_and_peer_request_cannot_retire_own_live_caller
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::independent_exec_candidate_claim_rechecks_exact_uid_handle_pid_tid_and_mode_before_producer
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::terminal_turn_freezes_root_join_mask_and_competing_vote_before_effects_and_untouched_drop_releases_only_owner
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::waiting_seal_closes_new_creation_but_original_join_finishes_and_late_child_is_captured_only_after_final_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::cancelled_captured_join_leaves_no_phantom_child_and_waiting_rollback_preserves_live_members
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::reserved_exit_committed_detach_retains_exact_negative_through_reuse_until_owned_finish
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::waiting_exit_retains_committed_detached_uid_through_binding_clear_and_recycled_slot_without_spending_new_task
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::waiting_rollback_prunes_only_committed_detached_tombstones_not_live_or_stopped_attached_members
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::waiting_seal_rejects_foreign_target_and_nonce_exhaustion_and_stale_row_record_phase_independently
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::captured_join_cannot_register_into_reserved_census_even_if_counter_is_forged_to_look_active
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::queued::queued_exit_waits_for_real_join_ack_not_visible_membership_prefix
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::process_placement::exit_membership::tests::queued::durable_request_outlives_requester_binding_detach_and_reuse_without_spending_replacement
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::queued::duplicate_receipt_keeps_original_reason_and_only_exact_effect_ack_completes
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::process_placement::exit_membership::tests::queued::terminal_turn_drains_before_claim_and_seal_publication_does_not_steal_its_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::queued::exec_owner_and_process_exec_refusal_preserve_durable_work_until_actual_cancel
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::process_placement::exit_membership::tests::queued::actual_exec_census_release_precedes_cancel_and_durable_exit_claim_after_pending_exit
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::queued::placement_busy_or_poison_never_grants_exit_claim_from_process_marker_alone
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::process_placement::exit_membership::tests::queued::abandoned_queued_claim_even_before_effects_returns_process_pin_to_poison_custody
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::process_placement::exit_membership::tests::queued::abandoned_queued_effects_preserve_work_pin_and_never_ack_stopped_metadata
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::queued::completion_watermark_survives_row_release_but_absence_and_next_unfinished_nonce_do_not_ack
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::ordering_publication::tests::actual_original_ordering_preserves_spawn_fifo_and_current_floor_without_funding_transfer
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::ordering_publication::tests::actual_latency_and_process_ordering_use_exact_live_original_targets_without_waking
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::ordering_publication::tests::actual_missing_current_cannot_supply_latency_floor_and_foreign_uid_cannot_publish
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::wake_publication::tests::actual_user_qualified_wake_preserves_kernel_wait_and_wakes_exact_original_user
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::activation::publication::tests::actual_single_activation_uses_spawn_fifo_and_rejects_foreign_duplicate_or_retired_members
bootstrap-activation-handoff/BootstrapActivationHandoff|kernel-ps|multitask::scheduler::activation::publication::tests::actual_over_capacity_activation_refuses_before_callback_with_all_real_members_suspended
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_observation::tests::actual_current_runtime_retains_original_pair_through_close_and_last_snapshot
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_observation::tests::actual_current_tls_requires_same_banks_linux_cell_and_accepted_value
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_observation::tests::actual_target_snapshot_retains_original_uid_and_refuses_foreign_pid_or_closed_lookup
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::bootstrap::initial_tasks::tests::actual_root_idle_requires_original_initial_current_banks_and_unconsumed_interval
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::execution_interval::tests::original_current_snapshot_retains_exact_timeout_reply_and_pair_after_shared_exhaustion
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::execution_interval::tests::prepared_grant_keeps_its_funder_after_donation_publication_changes
sigchld-notification/SigchldNotification|kernel-ps|multitask::scheduler::linux_thread_state::signals::tests::actual_group_job_control_seals_all_members_and_resumes_before_signal_delivery
sigchld-notification/SigchldNotification|kernel-ps|multitask::scheduler::linux_thread_state::signals::tests::actual_group_contention_returns_retry_without_signal_or_partial_stop_effects
sigchld-notification/SigchldNotification|kernel-ps|multitask::scheduler::linux_thread_state::signals::tests::actual_signal_original_recipient_revokes_block_arm_without_consumed_frame_read
sigchld-notification/SigchldNotification|kernel-ps|multitask::scheduler::linux_thread_state::signals::tests::actual_process_sigchld_prefers_leader_then_transfers_and_qualifies_leaderless_original
sigchld-notification/SigchldNotification|kernel-ps|multitask::scheduler::linux_thread_state::signals::tests::actual_signal_wrong_pid_generation_and_mask_refuse_or_preserve_exact_wait
sigchld-notification/SigchldNotification|kernel-ps|multitask::scheduler::linux_thread_state::signals::tests::actual_signal_retained_target_delays_terminal_effect_and_reuse_until_original_drop
sigchld-notification/SigchldNotification|kernel-ps|multitask::current_identity::cohort::tests::job_control_original_cohort_excludes_dispatch_through_each_gate_and_cannot_spend_affinity
sigchld-notification/SigchldNotification|kernel-ps|multitask::process_placement::tests::job_control_and_affinity_credentials_cannot_spend_each_others_original_process_turn
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_terminal::tests::actual_current_terminal_keeps_original_mm_uid_and_funding_until_last_effect
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_terminal::tests::actual_current_fault_preserves_first_reason_and_terminal_current_without_fresh_admission
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_terminal::tests::actual_current_terminal_refuses_missing_uid_alias_and_settled_interval
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_terminal::tests::actual_current_terminal_excludes_overlapping_effect_borrow_until_release
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_terminal::tests::actual_target_terminal_transfers_original_binding_and_marks_last_process
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_terminal::tests::actual_group_request_marks_exit_before_original_census_worker_effects
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_terminal::tests::actual_target_terminal_leaves_original_sibling_alive_until_separate_group_request
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::kernel_executor_refuses_terminal_and_every_stop_gate_but_retains_real_dispatch_admission
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::kernel_executor_is_not_an_ordinary_backing_pin_and_last_scope_release_reopens_exact_stop
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::active_kernel_executor_refuses_metadata_withdrawal_unowned_stop_and_qualified_cohort_seal
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::kernel_executor_refuses_user_metadata_user_context_foreign_uid_and_unpublished_context_independently
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::kernel_executor_refuses_constructor_writer_and_closing_without_spending_a_backing_reference
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::exact_kernel_uid_zero_is_executor_authority_not_an_empty_slot_or_user_projection
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_terminal_census_refuses_stopped_member_reclaim_and_logical_detach_until_original_turn_release
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_reclaim_first_refuses_terminal_census_until_original_physical_unregistration
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_overlapping_exec_and_terminal_censuses_keep_logical_tombstone_until_both_original_releases
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_concurrent_terminal_census_and_reclaim_admission_have_exactly_one_winner
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_final_terminal_withdraws_original_stopped_member_donation_without_directory_scan
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_frozen_census_refuses_zero_borrower_reclaim_without_partial_close_or_nonce_and_denies_physical_clear
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_reclaim_first_refuses_census_until_physical_unregistration_while_original_reuse_stays_closed
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_logical_unregister_cannot_erase_frozen_member_reclaim_exclusion
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_abandoned_reclaim_keeps_census_closed_without_new_execution_refs
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_concurrent_census_and_reclaim_admission_have_exactly_one_winner
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_exhausted_resource_nonce_does_not_install_a_process_reclaim_reservation
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::reuse::census_tests::actual_physical_clear_requires_original_reclaim_nonce_before_releasing_process_or_mm
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::return_architecture::tests::actual_copied_dispatch_without_successful_claim_refuses_architecture
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::return_architecture::tests::actual_same_current_retains_exact_claim_and_mm_without_architectural_replay
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::return_architecture::tests::actual_local_peer_plan_uses_original_frame_mm_and_placement_banks
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::return_architecture::tests::actual_direct_peer_plan_requires_original_fifo_claim_not_result_scalar
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::return_architecture::tests::actual_foreign_return_rsp_prior_or_selected_slot_refuses_original_claim
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::return_architecture::tests::actual_claimed_frame_republication_refuses_without_reading_a_new_header
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::return_architecture::tests::actual_claimed_return_requires_original_physical_prepared_bank
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::return_architecture::tests::actual_claimed_return_refuses_replaced_scalar_current_with_retained_banks
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::return_architecture::tests::actual_terminal_after_claim_cannot_reopen_architecture_or_drop_original_backing
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::return_architecture::tests::actual_retained_architecture_refuses_foreign_uid_and_frame_geometry
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::return_architecture::tests::actual_original_architecture_phase_admits_once_and_ready_is_not_a_fresh_claim
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_current_claim_retains_original_funding_and_moves_allocation_once
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_same_generation_frame_republication_refuses_final_leaf_cas_and_releases_funding
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_republished_owner_refuses_without_membership_or_bank_changes
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_other_reader_blocks_claim_until_reader_releases_without_funding_stage
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_affinity_exclusion_refuses_real_claim_and_original_funding_releases
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_terminal_after_funding_refuses_and_drops_original_grant
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_custody_and_saved_rsp_are_not_interchangeable_grants
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_current_fallback_uses_same_frame_funding_and_claim_engine
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_local_peer_uses_real_allocation_mm_placement_and_funding_banks
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_direct_peer_claims_exact_fifo_generation_without_fair_membership
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_direct_same_generation_frame_republication_refuses_final_leaf_cas
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_original_scope_replacement_refuses_under_final_leaf_before_cas
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_budget_close_refuses_without_prepared_cpu_grant
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_original_funding_close_after_preparation_retains_pin_but_refuses_new_execution
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_original_funding_exhausted_after_preparation_refuses_before_queue_cas
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_foreign_expected_uid_refuses_before_frame_and_funding_admission
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_selected_live_guard_corruption_refuses_old_published_canary_snapshot
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_joint_refused_placement_returns_original_mm_and_uid_loans_before_any_drop
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_joint_fixed_idle_borrows_real_kernel_frame_without_late_class_or_funding_authority
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_joint_local_claim_keeps_original_budget_and_class_leaves_through_owner_cas
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_joint_direct_claim_keeps_original_budget_and_class_leaves_through_owner_cas
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_joint_base_class_change_after_preparation_refuses_without_queue_or_bank_effects
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_joint_original_selection_class_is_not_recaptured_after_frame_admission
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::dispatch_claim::tests::actual_joint_original_donation_producer_invalidates_selected_peer_before_claim
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_failed_peer_fallback_reuses_interval_frame_and_queue_receipt_once
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_raced_completed_wait_is_not_cleared_and_publishes_local
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_terminal_after_capture_keeps_original_bank_and_refuses_resurrection
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_already_terminal_borrows_old_bank_not_a_new_post_stop_pin
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_scalar_running_without_physical_bank_refuses_before_effects
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_physical_bank_without_running_or_terminal_custody_refuses
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_replaced_scalar_current_refuses_even_with_retained_original_bank
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_foreign_frame_geometry_refuses_before_any_byte_read_or_effect
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::current_return::tests::actual_original_return_blocked_frame_refresh_observes_live_guard_not_old_snapshot
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::current_wait::tests::actual_current_wait_arm_cancel_retains_original_bank_uid_and_exact_reason
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::current_wait::tests::actual_current_wait_pager_commit_rollback_uses_same_original_loan_and_exact_token
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::current_wait::tests::actual_current_wait_raced_wake_withdraws_arm_and_refuses_commit_without_catalog
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::current_wait::tests::actual_current_wait_terminal_after_capture_refuses_arm_and_acknowledges_cancelled_pager_without_wake
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::current_wait::tests::actual_current_wait_scalar_running_without_original_physical_bank_refuses_before_arm
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::current_wait::tests::actual_current_wait_physical_bank_without_running_queue_custody_refuses_before_arm
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::current_wait::tests::actual_current_wait_already_committed_wait_refuses_another_arm_or_cancel
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::tests::actual_wake_blocked_return_publishes_queue_while_original_current_bank_retains_consumed_frame
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::runqueue::return_publication::tests::actual_return_raced_wake_recomputes_run_intent_and_publishes_once
scheduler-local-authority/SchedulerLocalAuthority|kernel-ps|multitask::scheduler::runqueue::return_publication::tests::actual_return_raced_wait_rolls_back_only_its_provisional_membership
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::runqueue::return_publication::tests::actual_return_terminal_wins_without_resurrection_or_neighbour_withdrawal
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::runqueue::return_publication::tests::actual_blocked_wake_terminal_wins_and_rolls_back_same_queue_turn
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::exec_transaction::quiescence::tests::actual_exec_census_busy_terminal_election_is_not_quiescent_even_with_zero_execution_refs
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::exec_transaction::quiescence::tests::actual_exec_census_unmasked_or_foreign_cpu_scope_refuses_before_original_sibling_effects
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::wake_publication::tests::actual_wake_unmasked_or_foreign_cpu_scope_refuses_before_binding_and_wait_effects
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::tests::actual_wake_stale_directory_hint_is_only_candidate_and_never_uid_authority
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_normal_reply_withdraws_only_original_reply_namespace_before_wake_or_refusal
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::wake_publication::tests::actual_reply_wake_retains_original_binding_through_fifo_and_rejects_replaced_generation
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::wake_publication::tests::actual_reply_wake_scope_and_foreign_identity_refuse_without_wait_or_fifo_effects
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::current_class::tests::actual_self_demotion_refuses_unmasked_foreign_cpu_and_sealed_or_missing_identity
scheduler-thread-demotion/SchedulerThreadDemotion|kernel-ps|multitask::scheduler::current_class::tests::actual_self_demotion_refuses_kernel_and_scalar_current_without_original_physical_bank
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_reply_attachment_never_selects_a_runnable_process_worker_before_exact_receive
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::terminal_publication::tests::actual_terminal_process_donations_retire_by_original_members_without_catalog_rescan
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_noncurrent_donor_requires_original_bound_resource_and_full_context
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_class_admission_uses_trusted_bit_and_preserves_ordinary_custody
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_participant_loans_survive_capture_and_terminal_seal_denies_new_admission
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_cancelled_original_source_refuses_after_capture_before_reservation_insert
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_changed_still_live_source_refuses_stale_new_reservation_basis
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_reservation_retains_original_source_when_a_newer_reply_changes_current_funding
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_root_cancellation_cannot_revive_a_reserved_nested_edge_or_replace_it_with_self_funding
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_publication::tests::actual_pager_and_reply_equal_numbers_have_distinct_owned_edges_and_exact_receivers
cpu-dispatch-preemption/CpuDispatchPreemption|kernel-ps|multitask::scheduler::donation_publication::tests::actual_unmasked_or_foreign_cpu_scope_refuses_all_donation_effects
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::task_directory::tests::actual_reply_custody_exact_resource_object_and_foreign_identity_are_one_scalar_result
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::task_directory::tests::actual_reply_custody_stale_or_absent_hint_cannot_substitute_another_resource_binding
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::task_directory::tests::actual_reply_custody_retained_terminal_object_returns_only_exact_custody_not_live_authority
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::current::ipc::tests::actual_slow_reply_to_reclaimed_original_context_is_terminal_not_kernel_panic
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::exec_transaction::quiescence::tests::actual_exec_census_retired_borrower_defers_same_detach_then_retries_to_both_consumer_publications
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::exec_transaction::quiescence::tests::actual_exec_census_cannot_retire_its_live_caller_before_spending_any_sibling_producer
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::scheduler::exec_transaction::quiescence::tests::actual_exec_census_foreign_mode_refuses_without_queue_stop_or_terminal_payload
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::tests::actual_wake_current_ignores_consumed_frame_and_retains_original_bank_and_uid
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::wake_publication::tests::actual_wake_foreign_uid_and_original_terminal_reservation_refuse_before_effects
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::wake_publication::tests::actual_wake_binding_pins_before_stop_and_excludes_reuse_until_original_drop
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::wake_publication::tests::actual_wake_nonexecuting_validates_real_frame_and_publishes_queue_without_catalog
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::terminal_publication::tests::actual_terminal_effects_publish_both_consumers_last_and_retain_current_backing_until_ack
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::terminal_publication::tests::actual_terminal_preexisting_wake_borrower_defers_original_reason_before_signal_and_consumer_effects
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::terminal_publication::tests::actual_terminal_busy_process_election_defers_without_spending_original_detach_then_retries
scheduler-wakeup/SchedulerWakeup|kernel-ps|multitask::scheduler::terminal_publication::tests::actual_terminal_sigchld_transfers_exact_sibling_events_to_leader_with_mask_and_consumed_current_frame
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::terminal_publication::tests::actual_invalid_wake_transfers_same_producer_after_reader_release_and_publishes_terminal_cleanup
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::terminal_quarantine::tests::actual_stop_retained_owner_closes_all_fresh_binding_and_cohort_admission_without_claim_gap
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::terminal_quarantine::tests::actual_stop_withdraws_queue_and_wait_before_deferred_reason_without_detaching_resources
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::terminal_quarantine::tests::actual_stop_foreign_uid_refuses_before_lifecycle_queue_or_terminal_reservation
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::terminal_quarantine::tests::actual_stop_preserves_first_reason_and_detach_through_repeat_and_claim_abandonment
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::kernel_executor_and_admitted_terminal_producer_exclude_each_other_before_effects
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::queued_kernel_stop_preserves_original_request_dispatch_and_nested_executor_until_last_release
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::stack_resources::kernel_executor_tests::last_executor_release_between_terminal_refusal_and_wait_publication_never_loses_exact_stop
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_slots::tests::membership::actual_root_capsule_publishes_membership_only_with_final_slot_ack_then_opens_child_creation
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_slots::tests::membership::root_join_waits_through_visible_prefix_and_blocked_slot_ack_until_actual_membership_release
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_slots::tests::membership::exec_intent_after_slot_ack_cannot_capture_root_census_before_owned_membership_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_slots::tests::membership::root_factory_early_mask_refusal_returns_original_ticket_without_process_gate_or_resource_effect
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_slots::tests::membership::untouched_root_join_rollback_preserves_actual_ticket_and_only_owned_empty_process_gate_reopens
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_slots::tests::membership::root_factory_refuses_committing_producer_without_spending_or_cancelling_original_binding
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::root_slots::tests::membership::registered_root_join_abandonment_keeps_process_sealed_even_after_independent_slot_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::root_slots::tests::membership::exhausted_root_nonce_returns_real_ticket_without_partial_membership_or_epoch_wrap
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::actual_detached_prefix_keeps_both_seals_and_hidden_uid_until_external_completion_returns
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::physical_retired_grant_independently_requires_desired_stop_and_exact_terminal_run_owner
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::pending_external_terminal_ack_refuses_reaper_before_metadata_or_resource_effects
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::reaper::tests::captured_backing_and_lifecycle_custody_refuse_then_release_into_same_exact_retired_grant
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::hidden_metadata_keeps_exact_retired_binding_without_using_absence_as_drain_evidence
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::two_real_retired_slots_own_disjoint_seals_and_first_ack_cannot_reopen_second
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::funded_interval_is_closed_not_discarded_and_last_charge_release_precedes_retired_detachment
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::abandoned_external_completion_keeps_terminal_and_physical_reuse_quarantined_even_after_empty_metadata
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::saved_frame_backing_without_lifecycle_count_independently_delays_the_real_reaper
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::reaper::tests::physical_all_cpu_policy_withdrawal_removes_only_the_owned_slot_before_external_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::process_affinity::tests::actual_group_capture_owns_every_live_binding_and_excludes_reuse_until_queue_and_membership_ack
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::process_affinity::tests::queued_rehome_keeps_old_process_mask_and_closed_admission_until_actual_owner_effect_returns
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::process_affinity::tests::running_affinity_never_steals_current_custody_but_marks_exclusion_and_requests_reschedule
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::process_affinity::tests::untouched_grant_rollback_releases_all_exact_pins_without_masks_queue_or_membership_effects
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::process_affinity::tests::post_pin_physical_abi_refusal_rolls_back_complete_group_before_any_effect
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::process_affinity::tests::invalid_requested_mask_and_foreign_full_handle_or_caller_uid_cannot_spend_real_membership
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::process_affinity::tests::desired_stop_without_terminal_ack_refuses_instead_of_treating_missing_member_as_drained
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::process_affinity::tests::exact_terminal_membership_ack_excludes_stopped_peer_without_rewriting_or_waking_its_slot
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::process_affinity::tests::foreign_physical_process_binding_refuses_after_exact_cohort_capture_without_spending_either_process
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::process_affinity::tests::terminal_stop_during_queue_effect_cannot_reuse_or_reanimate_captured_peer_and_survives_final_affinity_ack
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::process_affinity::tests::peer_block_between_affinity_masks_and_queue_effect_never_spends_affinity_as_a_wake
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::runqueue::tests::terminal_stop_between_migrating_cas_and_mailbox_publication_refuses_without_reanimation
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::thread_creation::tests::actual_reservation_and_cancel_retain_ticket_mm_and_attachment_until_complete_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_creation::tests::retired_creator_cannot_capture_physical_reservation_input
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::thread_creation::tests::actual_publication_keeps_parent_mm_and_ticket_through_blocked_frame_membership_and_slot_ack
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::thread_creation::tests::publication_inherits_new_complete_process_mask_after_reservation_not_old_snapshot
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_creation::tests::final_retired_creator_or_wrong_abi_refusal_cancels_real_unpublished_ticket_and_attachment
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::thread_creation::work::tests::original_mm_qualification_refuses_wrong_root_and_retains_actual_activation_through_bytes
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::thread_creation::work::tests::equal_root_different_original_mm_generation_refuses_materialization_and_final_membership
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_creation::work::tests::exhausted_durable_nonce_cannot_transfer_or_drop_actual_reserved_ticket
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::thread_creation::work::tests::abandoned_actual_kernel_claim_quarantines_capsule_without_reopen_or_implied_ack
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::thread_creation::work::tests::real_kernel_worker_publishes_slot_before_durable_ack_and_duplicate_claim_or_stale_receipt_refuses
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::thread_creation::work::tests::same_uid_slot_reuse_spends_new_ticket_and_durable_nonce_and_old_receipt_cannot_observe_next_ack
mm-activation-close/MmActivationClose|kernel-ps|multitask::scheduler::thread_creation::work::tests::actual_windows_materialization_error_cancels_reserved_storage_and_attachment_before_receipt_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_creation::work::tests::dying_original_user_cannot_orphan_real_capsule_and_kernel_cancels_before_failed_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_affinity::tests::linux_query_and_peer_set_preserve_process_mask_and_other_thread
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_affinity::tests::windows_self_alias_owns_two_physical_refs_and_changes_only_thread_mask
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::thread_affinity::tests::caller_and_target_pins_exclude_reuse_and_target_admission_until_untouched_release
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::context::tests::actual_current_bank_owns_entry_log_and_wait_namespace_until_physical_pin_release
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::context::tests::prepared_or_foreign_current_bank_cannot_authorize_scalar_fallback
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_affinity::tests::invalid_online_or_empty_mask_refuses_before_any_effect
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_affinity::tests::process_mask_escape_refuses_after_actual_complete_group_change_without_effects
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_affinity::tests::foreign_physical_process_refuses_after_pin_without_spending_either_binding
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::thread_affinity::tests::physical_peer_abi_and_caller_abi_mismatch_cannot_spend_target_masks
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::thread_affinity::tests::wrong_uid_missing_target_and_retired_caller_or_target_refuse_exact_physical_capture
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::thread_affinity::tests::queued_transfer_borrows_exact_target_cohort_until_real_queue_ack_without_process_publication
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::thread_affinity::tests::actual_running_target_keeps_current_custody_and_notifies_owner_cpu
affinity-claim-admission/AffinityClaimAdmission|kernel-ps|multitask::scheduler::thread_affinity::tests::block_during_owned_thread_queue_effect_is_not_an_affinity_wake
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::thread_affinity::tests::terminal_overlap_remains_retired_and_pinned_through_thread_affinity_ack
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::executor::tests::captured_kernel_fence_waits_for_exact_ack_not_claim_or_empty_payload
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::executor::tests::kernel_fence_completion_persists_through_reset_but_identical_reuse_needs_a_new_ack
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::executor::tests::copied_fence_drop_cannot_abandon_claim_and_foreign_slot_ack_cannot_complete_it
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::queued::exact_process_exit_marker_refuses_foreign_pid_without_setting_exit_on_original_object
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::commit_grant_captures_real_ticket_epoch_parent_membership_and_original_mm_before_effects_and_stale_ticket_cannot_spend_it
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::waiting_binding_retains_backing_but_terminal_stop_denies_fresh_admission_and_cannot_prove_drain_progress
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::group_freeze_denies_duplicate_root_join_and_mask_before_effects_and_reopens_only_exact_untouched_owner
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::pending_join_refuses_census_before_nonce_spend_then_registered_child_is_in_frozen_membership
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::wrong_target_foreign_ledger_generation_and_exhausted_nonce_refuse_without_partial_membership_gate
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::exact_retired_uid_not_hidden_metadata_absence_or_recycled_slot_is_required_for_withdrawal_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::captured_members_survive_stop_and_removal_while_effect_abandonment_never_reopens_creation
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::exit_scope_does_not_spend_or_reopen_existing_exec_epoch_on_pending_exit_rollback
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::original_process_object_is_retained_until_owner_drops_outside_leaf_even_after_last_attach_detaches
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::exit_membership::tests::stale_credential_cannot_begin_effects_for_next_same_uid_owner_and_row_record_epochs_are_independent
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::independent_thread_creator_capture_requires_exact_context_start_and_live_unsealed_binding_before_facade
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::process_placement::exec_membership::tests::group_membership_freeze_blocks_new_exec_but_pending_exit_does_not_erase_existing_census_epoch_or_reopen_join
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::pending_creation_excludes_vote_without_spending_epoch_and_registered_child_is_live_before_creator_ack
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::committed_stop_election_has_one_last_member_while_desired_retired_peer_is_not_final_election_or_absence
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::hidden_exact_retired_binding_counts_for_exit_authorization_but_missing_or_recycled_uid_never_counts_negative
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::missing_target_foreign_process_generation_and_nonce_exhaustion_refuse_before_partial_creation_gate
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::exact_lifecycle_stop_is_required_before_committed_vote_and_before_real_producer_publication
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::actual_dual_publication_receipt_not_fixture_or_copied_uid_releases_stopped_member_and_creation_gate
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::detached_stopped_member_keeps_exact_handle_epoch_and_turn_until_producer_ack_then_releases_empty_row_without_nonce_reset
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::live_peer_unregister_cannot_discard_potential_last_member_during_another_turn_but_exact_stopped_removal_can
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::abandoned_stopped_turn_quarantines_even_after_member_detach_instead_of_reopening_creator_or_next_process_generation
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::old_reserved_turn_credential_cannot_publish_next_same_uid_turn_despite_valid_class50_owner_and_stopped_lifecycle
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::terminal_turn_holds_real_process_object_until_exact_leaf_release_and_disjoint_process_votes_are_independent
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_placement::terminal::tests::committed_stop_cannot_admit_creation_or_mask_replacement_even_if_other_metadata_looks_live_again
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::process_placement::exec_membership::tests::terminal_turn_excludes_new_exec_census_but_frozen_exec_allows_exact_sibling_stop_without_reopening_creators
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::process_placement::exec_membership::tests::committed_terminal_target_refuses_exec_census_independently_of_live_metadata_intent_and_open_placement_phase
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::producer_reserves_before_effects_excludes_duplicates_consumers_and_reset_without_reclaiming_uid
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::payload_visibility_is_one_last_producer_operation_and_both_exact_consumer_acks_precede_reuse
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::deferred_loan_keeps_original_reason_detach_and_uid_and_two_claimants_cannot_spend_one_effect_owner
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::checked_reserve_generation_and_nonce_and_deferred_claim_nonce_exhaust_independently_without_effects
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::abandoned_effects_quarantine_even_with_empty_deferred_queue_and_no_consumer_payload
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::stale_claim_rollback_cannot_begin_effects_or_erase_next_claim_nonce_despite_same_uid_and_lifetime_generation
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::producer_key_independently_blocks_reuse_pending_and_consumers_when_compatibility_bits_or_payloads_are_misleading
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::unowned_terminal_deferred_take_reset_and_abort_cannot_spend_real_producer
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::unqualified_publication_rolls_back_but_foreign_payload_quarantines_effects_before_consumer_visibility
task-retirement-cleanup-owner/TaskRetirementCleanupOwner|kernel-ps|multitask::scheduler::retirement_cleanup::producer::tests::kernel_uid_zero_is_an_exact_producer_not_absence_and_reset_never_rewinds_next_admission_epoch
cross-cpu-task-retirement/CrossCpuTaskRetirement|kernel-ps|multitask::scheduler::stack_resources::context::intent_tests::terminal_producer_binding_retains_hidden_exact_uid_and_independently_denies_writer_close_and_physical_reuse_receipt
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::root_slots::tests::terminal_binding_admission_refuses_complete_constructor_prefix_until_exact_blocked_ack
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::irq_leaf::tests::failed_try_and_unwind_restore_only_the_original_mask_after_unlock
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::irq_leaf::tests::shared_leaf_preserves_outer_cpu_owner_and_original_irq_state_through_nested_release
scheduler-admission/SchedulerAdmission|kernel-ps|multitask::scheduler::scheduling_runtime::namespace_tests::original_birth_holds_the_same_namespace_as_standalone_identity_witnesses
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::process_table::pin_owner::tests::process_placement_transaction_retains_real_object_until_refusal_releases_without_rebinding
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::owner::tests::cpu_cells_drain_independently_and_merge_saturates_without_spending_next_window
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::owner::tests::remote_window_drain_preserves_open_turn_phase_baseline_and_new_window_segments
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::owner::tests::worst_turn_merge_keeps_caller_and_attribution_with_exact_maximum_not_other_cpu
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::owner::tests::per_slot_migration_history_distinguishes_root_uid_zero_reuse_and_cpus
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::owner::tests::busy_reporter_refuses_without_consuming_cpu_counters_or_reopening_custody
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runtime_profile::owner::tests::report_epoch_admits_one_due_consumer_and_preserves_partial_period_on_every_cpu
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::runqueue::idle::admitted_tag_binds_fixed_ap_cpu_and_clear_removes_mapping_without_turning_root_into_ap
scheduler-cpu-distribution/SchedulerCpuDistribution|kernel-ps|multitask::scheduler::boot_execution::boot_tick_roundtrips_raw_zero_and_full_pit_encoding_without_task_weight_state
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::admission::tests::reservation_blocks_cold_binding_in_every_phase_even_when_descriptor_and_identity_are_complete
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::admission::tests::borrowed_binding_and_exec_intent_refuse_reservation_before_any_nonce_or_storage_effect
process-lifecycle-transaction/ProcessLifecycleTransaction|kernel-ps|multitask::scheduler::stack_resources::admission::tests::reserved_frame_bytes_require_exact_live_ticket_and_cannot_reenter_or_replay_generation
cold-exec-intent/ColdExecIntent|kernel-ps|multitask::scheduler::stack_resources::admission::tests::reserved_descriptor_cannot_acquire_exec_seal_or_spend_next_exec_generation_before_ack
lifecycle-execution-claim/LifecycleExecutionClaim|kernel-ps|multitask::scheduler::stack_resources::lifecycle_tests::reserved_slot_refuses_real_local_and_direct_cas_in_every_phase_without_spending_queue_or_uid
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::local_dispatch::tests::actual_selection_refuses_published_frame_without_original_allocation
frame-reader-claim/FrameReaderClaim|kernel-ps|multitask::scheduler::stack_resources::frame_readers::tests::reserved_descriptor_never_becomes_frame_observation_authority_before_publication_ack
atomic-process-activation-batch/AtomicProcessActivationBatch|initd|activation::tests::activation_batch_is_exact_bounded_and_zero_tailed
atomic-process-activation-batch/AtomicProcessActivationBatch|rustos-user-abi|syscall::activation_batch::tests::requester_identity_is_bound_to_the_kernel_sender
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-compat|user::syscall::linux::proc_broker_ops::activation_batch::tests::activation_batch_keeps_preflight_and_commit_under_registry_lock
atomic-process-activation-batch/AtomicProcessActivationBatch|kernel-compat|user::syscall::linux::proc_broker_ops::activation_batch::tests::exact_batch_authority_rejects_pid_equal_generation_or_mm_replacement
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
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::ipc_ops::reply_wait::tests::the_wait_polls_twice_per_turn_and_arms_only_after_the_first_poll
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::ipc_ops::tests::stable_service_endpoint_snapshot_rejects_revoked_owners
ipc-reply-deadline/IpcReplyDeadline|kernel-ipc-runtime|ipc::tests::prepared_reply_bind_rejects_foreign_and_duplicate_owner_without_mutation
ipc-reply-deadline/IpcReplyDeadline|kernel-ipc-runtime|ipc::tests::cancelling_before_reply_returns_prepared_descriptor_once
ipc-reply-deadline/IpcReplyDeadline|kernel-ipc-runtime|ipc::tests::endpoint_failure_returns_prepared_descriptor_in_error_cleanup
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::timely_netd_socket_decode_installs_one_exact_entry_and_returns_that_fd
ipc-reply-deadline/IpcReplyDeadline|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::only_inet_stream_socket_creation_requires_a_prepared_reply_entry
ipc-handle-transfer/IpcHandleTransfer|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::failed_reply_binding_reclaims_initial_inet_descriptor_without_deferred_release
netd-deferred-reply/NetdDeferredReply|netd|packet_provider_state_tests::rejected_prepared_inet_publication_discards_the_unpublished_token_once
netd-deferred-reply/NetdDeferredReply|netd|packet_provider_state_tests::inet_provider_start_deadline_and_token_gate_precede_the_start_closure
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
commercial-service-envelope/CommercialServiceEnvelope|netd|commercial_protocol::tests::commercial_netd_requests_are_closed_and_canonical
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
product-boot/ProductBoot|vfsd|tests::executable_snapshot_marker_binds_source_and_mount_identity
product-boot/ProductBoot|vfsd|tests::snapshot_worker_admission_is_single_slot_and_exact_owner
product-boot/ProductBoot|rootd|tests::core_readiness_budget_is_bounded_and_resets_only_on_readiness|host-test
product-boot/ProductBoot|kernel-io-manager|input::dvm_ring::tests::policy_consumer_readiness_requires_transport_and_is_idempotent
product-boot/ProductBoot|uiserver|gpu_runtime::tests::dvm_gpu_admission_waits_without_hiding_behind_software
product-boot/ProductBoot|storaged|tests::dvm_block_e2e_marker_names_the_complete_authority_path
product-boot/ProductBoot|uiserver|gpu_runtime::tests::frame_deadline_skips_missed_slots_without_drift_or_burst
product-boot/ProductBoot|uiserver|input_loop::tests::input_waitset_startup_retries_only_transient_control_failures
product-boot/ProductBoot|xtask|kvm::tests::dvm_display_mode_requires_the_observed_display_contract
dvm-gpu-compositor/DvmGpuCompositor|uiserver|gpu_runtime::tests::mandatory_gpu_wait_never_admits_cpu_present_as_retry
ui-frame-budget/UiFrameBudget|wayclick|damage_tests::first_frame_marker_is_the_user_visible_boot_terminal
input-ingestion-worker/InputIngestionWorker|kernel-io-manager|input::dvm_ring::tests::policy_consumer_readiness_requires_transport_and_is_idempotent
dvm-input-ring/DvmInputRing|kernel-io-manager|input::dvm_ring::tests::concurrent_broker_callers_have_exactly_one_drain_owner
product-boot/ProductBoot|kernel-compat|user::syscall::linux::debug_ops::product_milestone_tests::product_milestones_are_a_closed_fixed_name_vocabulary
user-stack-growth/UserStackGrowth|kernel-compat|user::process::tests::release_stack_maps_every_usable_page_above_one_guard
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::process_table::tests::process_address_space_and_exec_exit_are_serialized
exec-address-space-transaction/ExecAddressSpaceTransaction|kernel-ps|multitask::process_table::tests::exec_seal_rejects_thread_attachment_until_cancel
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::tests::ready_scanner_never_reads_a_frame_owned_by_any_cpu
scheduler-cpu-distribution/SchedulerCpuDistribution|nucleus-core|debug::tests::milestone_frame_is_complete_self_framed_and_checksum_verified
scheduler-cpu-distribution/SchedulerCpuDistribution|nucleus-core|debug::tests::milestone_render_overflow_is_an_explicit_failure_before_publication
scheduler-cpu-distribution/SchedulerCpuDistribution|xtask|kvm::tests::smp_runtime_rejects_interleaved_or_route_only_substrings_as_evidence
clocksource-deadline/ClocksourceDeadline|kernel-hal|arch::clock::tests::raw_tsc_global_clock_is_rejected_until_smp_offsets_are_admitted
exception-retirement-lifecycle/ExceptionRetirementLifecycle|kernel-hal|arch::gdt::tests::per_cpu_privilege_and_ist_stacks_are_aligned_and_disjoint
ipc-handle-transfer/IpcHandleTransfer|rustos-user-abi|tests::ipc_transfer_ticket_wire_is_canonical_and_rejects_zero_authority
robust-futex-owner-death/RobustFutexOwnerDeath|kernel-compat|user::syscall::linux::service_ops::futex_thread::tests::kernel_generated_wake_uses_shared_then_exact_private_fallback
gpu-submit-transaction/GpuSubmitTransaction|uiserver|gpu_scene::tests::rejected_transport_submit_restores_exact_compiler_timeline
acceptance-profile-publication/AcceptanceProfilePublication|uiserver|acceptance_profile::tests::late_acceptance_profile_requires_the_exact_complete_contract
smp-ring3-qualification/SmpRing3Qualification|rustos-user-abi|syscall::syscall_tests::smp_qualification_worker_shape_is_exact_and_bounded
smp-ring3-qualification/SmpRing3Qualification|rustos-user-abi|syscall::syscall_tests::smp_qualification_bind_shape_is_closed_and_bounded
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::debug_ops::product_milestone_tests::smp_qualification_milestone_binds_worker_to_the_kernel_observed_cpu
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::debug_ops::product_milestone_tests::smp_qualification_milestone_rejects_unbounded_workers_and_work
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::debug_ops::product_milestone_tests::product_milestones_are_a_closed_fixed_name_vocabulary
smp-ring3-qualification/SmpRing3Qualification|nucleus-core|debug::tests::ring3_debug_bytes_are_bounded_and_cannot_open_a_milestone_frame
smp-ring3-qualification/SmpRing3Qualification|nucleus-core|debug::tests::qualification_output_class_is_exact_and_scheduler_is_measurement
smp-ring3-qualification/SmpRing3Qualification|nucleus-core|debug::tests::qualification_loss_snapshot_isolated_and_fail_closed
smp-ring3-qualification/SmpRing3Qualification|nucleus-core|debug::tests::qualification_drop_is_visible_only_to_following_critical_evidence
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::debug_ops::product_milestone_tests::user_debug_syscall_has_no_raw_debugcon_payload_path
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::sysops::console::tests::system_console_debug_mirror_never_writes_raw_user_bytes
smp-ring3-qualification/SmpRing3Qualification|runtimed|kvm_smp_qualification::tests::parser_accepts_only_the_canonical_ordered_contract
smp-ring3-qualification/SmpRing3Qualification|runtimed|kvm_smp_qualification::tests::parser_rejects_worker_set_and_safe_bound_violations
smp-ring3-qualification/SmpRing3Qualification|runtimed|kvm_smp_qualification::tests::absent_or_invalid_contract_injects_nothing
smp-ring3-qualification/SmpRing3Qualification|runtimed|kvm_smp_qualification::tests::exact_contract_injects_one_private_nonprivileged_nonrestarting_launch
smp-ring3-qualification/SmpRing3Qualification|runtimed|kvm_smp_qualification::tests::missing_contract_is_the_only_normal_no_qualification_result
smp-ring3-qualification/SmpRing3Qualification|runtimed|kvm_smp_qualification::tests::snapshot_failures_preserve_raw_errno_or_use_stable_errno
smp-ring3-qualification/SmpRing3Qualification|runtimed|kvm_smp_qualification::tests::malformed_contract_is_an_einval_catalog_failure
smp-ring3-qualification/SmpRing3Qualification|runtimed|catalog::tests::qualification_load_failure_leaves_published_ordinary_entries_unchanged
smp-ring3-qualification/SmpRing3Qualification|runtimed|catalog::tests::qualification_retry_backoff_is_bounded_and_monotonic
smp-ring3-qualification/SmpRing3Qualification|vfsd|tests::unpublished_storaged_is_retryable_not_a_missing_file
smp-ring3-qualification/SmpRing3Qualification|vfsd|tests::storage_geometry_rejects_provider_overflow_unknown_flags_and_foreign_binding
smp-ring3-qualification/SmpRing3Qualification|runtimed|tests::policy_catalog_load_is_not_gated_by_ui_readiness
smp-ring3-qualification/SmpRing3Qualification|runtimed|socket::tests::only_ui_bootstrap_or_private_smp_qualification_may_launch_before_ui_ready
smp-ring3-qualification/SmpRing3Qualification|runtimed|spawn::tests::qualification_bind_is_exact_and_precedes_activation
smp-ring3-qualification/SmpRing3Qualification|runtimed|spawn::tests::qualification_bind_failure_never_activates_the_child
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::private_exec_and_missing_binding_activation_are_fail_closed
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::exact_worker_topologies_bind_and_complete_once
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::ready_barrier_identity_and_phase_rejections_are_atomic
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::immutable_work_deadline_and_endpoint_generation_fail_closed
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::pid_generation_and_mm_generation_substitution_terminally_revoke
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::post_admission_endpoint_revalidation_rejects_revoke_and_terminalizes
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::complete_cannot_precede_every_worker_finish
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::bind_registration_is_linearized_with_deferred_activation_authority
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::smp_qualification_ops::tests::process_cleanup_revokes_qualification_before_deferred_authority_reuse
smp-ring3-qualification/SmpRing3Qualification|kernel-compat|user::syscall::linux::proc_broker_ops::activation_batch::tests::private_qualification_authority_is_never_batch_eligible
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::smoke_readiness_budget_starts_only_after_both_guests_spawn
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::smp_ring3_qualification_accepts_complete_exact_worker_sets
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::smp_ring3_qualification_rejects_missing_duplicate_and_replayed_phases
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::smp_ring3_qualification_rejects_process_and_thread_substitution
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::smp_ring3_qualification_rejects_loss_wrong_cpu_and_work
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::smp_ring3_qualification_rejects_phase_order_and_deadline
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::smp_ring3_qualification_rejects_interleaved_tampered_and_plain_frames
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::smp_ring3_qualification_has_exact_private_kvm_admission
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::dvm_attached_block_disk_requires_qemu_read_only_backing
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::dvm_block_transport_header_matches_read_only_qemu_backing
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::dvm_block_read_only_media_driver_closure_is_explicit
smp-ring3-qualification/SmpRing3Qualification|xtask|kvm::tests::dvm_block_read_only_media_geometry_matches_atapi_capacity
smp-ring3-qualification/SmpRing3Qualification|xtask|stage::tests::early_system_allowlist_contains_the_minimal_dynamic_runtime_closure
dvm-block-startup/DvmBlockStartup|kernel-io-manager|io::dvm_block::tests::block_shared_aperture_requires_prefetchable_write_back_atomic_memory
dvm-input-ring/DvmInputRing|kernel-io-manager|input::dvm_ring::tests::input_shared_ring_requires_prefetchable_write_back_atomic_memory
page-table-lifecycle/PageTableLifecycle|kernel-io-manager|driver::mmio::tests::overlapping_physical_ranges_reject_mixed_cache_modes
page-table-lifecycle/PageTableLifecycle|kernel-mm|memory::kernel_vm::tests::shared_memory_mapping_is_write_back_not_mmio_or_write_combining
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-mm|memory::cache_attributes::tests::pat_cache_contract_update_is_exact_idempotent_and_cpu_local
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-mm|memory::cache_attributes::tests::ap_memory_type_admission_requires_features_capacity_and_no_fill_state
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-mm|memory::cache_attributes::tests::ap_restore_sequence_is_before_cache_enable_and_private_readback
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-mm|memory::cache_attributes::tests::ap_restore_requires_the_sealed_bsp_baseline_and_exact_capability
cpu-online-lifecycle/CpuOnlineLifecycle|nucleus-core|ap_trampoline::tests::reset_cache_state_enters_no_fill_before_mailbox_or_paging
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-executive|boot::tests::ap_cache_attributes_are_verified_before_private_ready_publication
cpu-online-lifecycle/CpuOnlineLifecycle|kernel-executive|boot::tests::local_apic_uses_one_permanent_uncached_direct_map_alias
EOF

# Every group is an independent Cargo selection over an already-built target
# directory, and there are fewer groups than cores. Re-entering Cargo once per
# package/feature pair was this lane's dominant cost, and running the groups
# concurrently turns that sum into a maximum. Only the execution moves:
# verification below still walks `group_order` in registry order, so a failure
# names the same group, witness, and count it always did.
run_dir="$(mktemp -d)"
trap 'rm -f "$records" "$seen"; rm -rf "$run_dir"' EXIT
group_index=0
for group in "${group_order[@]}"; do
    package="${group%%|*}"
    features="${group#*|}"
    mapfile -t names < <(printf '%s' "${group_tests[$group]}" | sed '/^$/d')
    cargo_args=(test -p "$package")
    if [[ -n "$features" ]]; then
        cargo_args+=(--features "$features")
    fi
    # `-q` would collapse libtest to progress dots, and the per-witness pass
    # line is the evidence that each registered name really ran.
    cargo_args+=(-- --exact "${names[@]}")
    # `kernel-ps` witnesses share architecture-test publication state (GDT,
    # runqueue and process-table reset fixtures). Their production protocol is
    # concurrent, but these host fixtures are intentionally single-owner; keep
    # that group's own threads at one rather than letting one witness reset
    # another's synthetic scheduler while it is allocating a slot. Running it
    # beside *other* packages is unaffected: the fixtures are process-local.
    (
        # `set -e` is inherited here, so a failing cargo would abort this
        # subshell before it could record the exit code, and verification below
        # would then report a missing `.rc` file instead of the real failure.
        # Capture the status explicitly so a failed group names its own cause.
        rc=0
        if [[ "$package" == "kernel-ps" ]]; then
            RUST_TEST_THREADS=1 cargo "${cargo_args[@]}" \
                > "$run_dir/$group_index.out" 2>&1 || rc=$?
        else
            cargo "${cargo_args[@]}" > "$run_dir/$group_index.out" 2>&1 || rc=$?
        fi
        printf '%s' "$rc" > "$run_dir/$group_index.rc"
    ) &
    group_index=$((group_index + 1))
done
wait

group_index=0
for group in "${group_order[@]}"; do
    package="${group%%|*}"
    features="${group#*|}"
    mapfile -t names < <(printf '%s' "${group_tests[$group]}" | sed '/^$/d')
    output="$(cat "$run_dir/$group_index.out")"
    group_rc="$(cat "$run_dir/$group_index.rc")"
    group_index=$((group_index + 1))
    if [[ "$group_rc" != "0" ]]; then
        printf '%s\n' "$output" >&2
        echo "source conformance witnesses failed for $package${features:+ [$features]}" >&2
        exit 1
    fi
    for name in "${names[@]}"; do
        # libtest decorates a `#[should_panic]` witness, and a registered test
        # name is compared whole so one witness cannot be satisfied by another
        # whose name merely extends it.
        if ! awk -v plain="test $name ... ok" \
            -v panics="test $name - should panic ... ok" \
            '$0 == plain || $0 == panics { found = 1 } END { exit(found ? 0 : 1) }' \
            <<<"$output"; then
            printf '%s\n' "$output" >&2
            echo "source conformance witness did not execute and pass: $package -> $name" >&2
            exit 1
        fi
    done
    executed="$(
        sed -n 's/^test result: ok\. \([0-9][0-9]*\) passed; 0 failed.*$/\1/p' <<<"$output" \
            | awk '{ total += $1 } END { print total + 0 }'
    )"
    if [[ "$executed" -ne "${#names[@]}" ]]; then
        printf '%s\n' "$output" >&2
        echo "source conformance executed $executed witnesses for $package, expected ${#names[@]}" >&2
        exit 1
    fi
    while IFS='|' read -r model row_package row_test row_features; do
        [[ -n "$model" ]] || continue
        # One `jq` per witness spawned it 619 times to build 619 one-line
        # objects. The row is already the exact `|`-separated witness key that
        # was validated on the way in, so it is recorded verbatim and converted
        # once below.
        printf '%s|%s|%s|%s\n' "$model" "$row_package" "$row_test" "$row_features" >> "$records"
        checks=$((checks + 1))
    done <<<"${group_rows[$group]}"
done

jq -R -s --arg schema rustos-formal-source-conformance-v1 \
    'split("\n")
     | map(select(length > 0) | split("|")
         | {model:.[0],package:.[1],test:.[2],features:(.[3] // ""),status:"passed"})
     | {schema:$schema,status:"passed",checks:length,models:(map(.model)|unique|length),results:.}' \
    "$records" > "$artifact_dir/summary.json"
printf 'source conformance passed checks=%s models=%s\n' "$checks" \
    "$(jq -r '.models' "$artifact_dir/summary.json")"
