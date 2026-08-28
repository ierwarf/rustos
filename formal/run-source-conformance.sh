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
exec_stage_body="$(
    sed -n '/^pub fn stage_exec_state(/,/^pub fn finalize_exec_state(/p' \
        kernel/ps/src/multitask/process_table.rs
)"
if ! grep -Fq 'exec_commit_may_transfer(object, reservation)' <<<"$exec_stage_body" \
    || ! grep -Fq 'object.exec_state_staged = true;' <<<"$exec_stage_body"; then
    echo 'exec staging must retain exact reservation and hide the new process state' >&2
    exit 1
fi
exec_transfer_body="$(
    sed -n '/^pub fn exec_current_user_process(/,/^pub fn exec_user_process_by_pid(/p' \
        kernel/ps/src/multitask/current.rs
)"
exec_state_line="$(grep -n -m1 'process_table::stage_exec_state(' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_publish_line="$(grep -n -m1 'scheduler_mut().exec_current_user_process' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_retain_line="$(grep -n -m1 'process_table::finalize_exec_state(staged, published)' <<<"$exec_transfer_body" | cut -d: -f1)"
exec_drop_line="$(grep -n -m1 'drop(old_state);' <<<"$exec_transfer_body" | cut -d: -f1)"
if [[ -z "$exec_state_line" || -z "$exec_publish_line" || -z "$exec_retain_line" || -z "$exec_drop_line" \
    || "$exec_state_line" -ge "$exec_publish_line" || "$exec_publish_line" -ge "$exec_retain_line" \
    || "$exec_retain_line" -ge "$exec_drop_line" ]]; then
    echo 'exec must stage process state, publish scheduler root, finalize generation, then retire the old bundle' >&2
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

# Busy-CPU balancing is driven by loaded opportunities, never by a global RTC
# residue that one CPU's scheduling cadence may fail to visit. Candidate
# selection must also retain the source-migration predicate: it excludes
# current/transition stack owners before the existing owner-CAS/mailbox move.
active_balance_body="$(
    sed -n '/pub(super) fn rebalance_one_from_busy_cpu(/,/pub(super) fn request_runqueue_owner_reschedule/p' \
        kernel/ps/src/multitask/scheduler/runqueue_policy.rs
)"
if ! grep -Fq 'ACTIVE_BALANCE_OPPORTUNITIES[source_cpu].fetch_add(1, Ordering::Relaxed)' \
        <<<"$active_balance_body" \
    || ! grep -Fq 'active_balance_opportunity_due(previous_opportunities)' \
        <<<"$active_balance_body" \
    || ! grep -Fq 'context_is_migratable_from_source(slot, context, source_cpu, target_cpu)' \
        <<<"$active_balance_body" \
    || grep -Fq 'context_is_schedulable(slot, context)' <<<"$active_balance_body"; then
    echo 'active balance must use loaded-opportunity cadence and exclude every execution owner' >&2
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
if ! grep -Fq 'enqueue_executable_snapshot(reply_cap, sender_pid, sender_tid, *request)' <<<"$vfs_receive_body" \
    || grep -Fq 'reply_executable_snapshot(' <<<"$vfs_receive_body" \
    || ! grep -Fq 'reply_executable_snapshot(' <<<"$vfs_snapshot_worker_body" \
    || ! grep -Fq 'SnapshotWorkerAdmission' services/vfsd/src/snapshot_worker.rs \
    || grep -Eq 'sched_yield|SYS_SCHED_YIELD' services/vfsd/src/main.rs services/vfsd/src/snapshot_worker.rs; then
    echo 'vfsd receive owner must hand bulk snapshots to one bounded exact-owner worker slot' >&2
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
# functions in the same file already read the published seqlock record. The
# locked call must remain as the fallback: a `None` publication means "retry
# under authority", never "there is no user task".
current_snapshot_body="$(
    sed -n '/^pub fn current_user_snapshot(/,/^}/p' kernel/ps/src/multitask/current.rs
)"
if ! grep -Fq 'published_current_identity()' <<<"$current_snapshot_body" \
    || ! grep -Fq 'scheduler_ref().current_user_process_binding()' <<<"$current_snapshot_body"; then
    echo 'the current-task identity snapshot must read the published record before the global scheduler lock, and must keep the locked fallback' >&2
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
    sed -n '/^fn recv_with_sender_blocking_prepared(/,/^}/p' \
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
smp_ring3_option_body="$(sed -n '/^    if options.smp_ring3_qualification {/,/^    } else if options.smp_evidence_cohort.is_some() {/p' tools/xtask/src/kvm/options.rs)"
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
dvm_display_cache_source="$(cat kernel/io-manager/src/io/dvm_display.rs)"
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

# Only the four closed qualification names may use qualification-local loss
# counters.  Scheduler records stay one-attempt measurements, while the host
# parser rejects any nonzero local evidence loss before accepting a workload.
debug_qualification_class_failed=0
for debug_qualification_name in smp-qualification-ready smp-qualification-start smp-qualification-finish smp-qualification-complete; do
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
    echo 'qualification debug evidence must use its exact four classes, local fail-closed loss counters, a one-attempt scheduler measurement, Required DVM revoke, and parser loss rejection' >&2
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
    || ! grep -Fq 'authority.qualification_required,' <<<"$proc_activate_body" \
    || ! grep -Fq 'activate_suspended_user_tasks_with_commit(' <<<"$proc_activate_body" \
    || ! rg -Fq 'const fn deferred_authority_is_batch_eligible(qualification_required: bool) -> bool' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq '&& deferred_authority_is_batch_eligible(qualification_required)' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq 'let mut exact_targets = [None; LOADER_ACTIVATE_BATCH_MAX_TARGETS];' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq 'deferred_authority_matches_exact_batch_request(' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq 'assert_eq!(authority.target, exact_target);' kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs \
    || ! rg -Fq 'revoke_smp_qualification_for_process(process_id);' kernel/compat/src/user/syscall/linux/proc_broker_ops.rs \
    || ! rg -Fq 'admit_smp_qualification_milestone(milestone, arg0, arg1, current_cpu)' kernel/compat/src/user/syscall/linux/debug_ops.rs \
    || ! rg -Fq 'debug::write_user_bytes_serialized(&chunk[..chunk_len]);' kernel/compat/src/user/syscall/linux/debug_ops.rs \
    || rg -Fq 'debug::write_bytes(&chunk[..chunk_len]);' kernel/compat/src/user/syscall/linux/debug_ops.rs \
    || ! rg -Fq 'crate::debug::write_user_bytes_serialized(&chunk[..chunk_len]);' kernel/compat/src/user/sysops/console.rs \
    || rg -Fq 'crate::debug::write_bytes(&chunk[..chunk_len]);' kernel/compat/src/user/sysops/console.rs \
    || rg -q 'fn write_bytes' kernel/nucleus-core/src/debug/mod.rs \
    || ! rg -Fq 'let expected_events = usize::from(workers) * 3 + 1;' tools/xtask/src/kvm.rs \
    || ! rg -Fq 'SMP Ring3 qualification requires one terminal completion record' tools/xtask/src/kvm.rs \
    || ! rg -Fq 'schema: "rustos-kvm-smp-correctness-evidence-v6"' tools/xtask/src/kvm/evidence.rs \
    || ! rg -Fq 'predecessor_schema: "rustos-kvm-smp-correctness-evidence-v5"' tools/xtask/src/kvm/evidence.rs \
    || ! rg -Fq 'smp_evidence_cohort: snapshot.run.cohort.clone()' tools/xtask/src/kvm/evidence.rs \
    || ! rg -Fq -- '--smp-ring3-qualification requires --smp-evidence-cohort' tools/xtask/src/kvm/options.rs \
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
    echo 'SMP Ring3 qualification source no longer preserves UI-independent private injection, the closed pre-UI exception, bind-before-activate cleanup, generation-bound kernel FSM admission, escaped user debug, join-before-complete, 3N+1 evidence, v6 pre-spawn DVM and early-system executable attestations, durable snapshot-before-signing order, QEMU readonly ATAPI/header/Linux-relay agreement with built-in sr at exact 2048-byte FLUSH-only media geometry, exact media-barrier authority path, exact successor-generation recovery, publication-time drift rejection, or non-overwriting archive publication' >&2
    exit 1
fi

# A terminal reply crosses the global scheduler catalog exactly once.  That
# transaction revokes donation, wakes the exact task, and mints one opaque
# owner-generation token; publication happens only after the catalog guard is
# gone, and selection revalidates the same custody with no generic fallback.
reply_current_body="$(sed -n '/^pub fn complete_ipc_reply_wake_handoff(/,/^pub fn release_ipc_priorities_for_process/p' kernel/ps/src/multitask/current/ipc.rs)"
reply_custody_body="$(sed -n '/^pub fn settle_ipc_reply_scheduling_context(/,/^pub fn complete_ipc_reply_wake_handoff(/p' kernel/ps/src/multitask/current/ipc.rs)"
reply_scheduler_body="$(sed -n '/^    pub(super) fn complete_ipc_reply_wake_handoff(/,/^    fn wake_task_slot/p' kernel/ps/src/multitask/scheduler.rs)"
reply_enqueue_body="$(sed -n '/^fn enqueue_reply_wake_after_catalog(/,/^pub(super) fn enqueue_reply_wake/p' kernel/ps/src/multitask/scheduler/sync_handoff.rs)"
reply_selection_body="$(sed -n '/^    fn synchronous_handoff_record_is_ready(/,/^    pub(super) fn take_next_synchronous_pick_hint_ready_slot/p' kernel/ps/src/multitask/scheduler/handoffs.rs)"
plain_reply_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_ipc_reply(/,/^pub(super) fn syscall_linux_rustos_ipc_call_with_handles/p' kernel/compat/src/user/syscall/linux/ipc_ops.rs)"
handle_reply_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_ipc_reply_with_handles/,/^pub(super) fn call_linux_syscall_endpoint/p' kernel/compat/src/user/syscall/linux/ipc_ops.rs)"
reply_recv_body="$(sed -n '/^pub(super) fn syscall_linux_rustos_ipc_reply_recv_with_sender/,$p' kernel/compat/src/user/syscall/linux/ipc_reply_recv.rs)"
if ! grep -Fq 'scheduler_mut().complete_ipc_reply_wake_handoff(reply, task_id)' <<<"$reply_current_body" \
    || ! grep -Fq 'token.is_some_and(scheduler::enqueue_reply_wake_handoff)' <<<"$reply_current_body" \
    || ! grep -Fq 'let _ = self.release_ipc_priority(reply);' <<<"$reply_scheduler_body" \
    || ! grep -Fq 'if !self.wake_task_slot(slot)' <<<"$reply_scheduler_body" \
    || ! grep -Fq 'ReplyWakeHandoff::from_owner(slot, task_id, owner)' <<<"$reply_scheduler_body" \
    || ! grep -Fq 'if !owner_still_matches(token)' <<<"$reply_enqueue_body" \
    || ! grep -Fq 'owner_still_matches(token) && retained' <<<"$reply_enqueue_body" \
    || ! grep -Fq 'record.has_current_dispatch_custody()' <<<"$reply_selection_body" \
    || ! grep -Fq 'self.pick_hint_candidate_slot(Some(record.slot())).is_some()' <<<"$reply_selection_body" \
    || ! grep -Fq 'settle_ipc_reply_scheduling_context(reply, custody)' <<<"$reply_custody_body" \
    || ! grep -Fq 'complete_ipc_reply_wake_handoff(reply, completion.caller_task_id)' <<<"$reply_custody_body" \
    || ! grep -Fq 'multitask::complete_ipc_reply_wake_handoff_with_custody(reply, completion)' <<<"$plain_reply_body" \
    || ! grep -Fq 'multitask::complete_ipc_reply_wake_handoff_with_custody(args.reply_cap, completion)' <<<"$handle_reply_body" \
    || ! grep -Fq 'multitask::complete_ipc_reply_wake_handoff_with_custody(args.reply_cap, completion)' <<<"$reply_recv_body" \
    || grep -Eq 'release_ipc_priority|wake_task|set_next_synchronous_pick_hint' <<<"$plain_reply_body" \
    || grep -Eq 'release_ipc_priority|wake_task|set_next_synchronous_pick_hint' <<<"$handle_reply_body" \
    || grep -Eq 'release_ipc_priority|wake_task|set_next_synchronous_pick_hint' <<<"$reply_recv_body"; then
    echo 'terminal IPC replies must return exact scheduling-context custody and retain one exact post-catalog per-CPU handoff token without legacy fallback' >&2
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
process-address-space-lifetime/ProcessAddressSpaceLifetime|kernel-ps|multitask::process_table::tests::process_address_space_and_exec_exit_are_serialized
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|user::handles::transfer_registry::transfer_registry_tests::authority_identity_exhaustion_fails_closed_before_wrap
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::identity_tests::task_identity_exhaustion_never_wraps_to_a_live_id
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ps|multitask::process_table::tests::process_generations_fail_closed_instead_of_aliasing_stale_handles
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-compat|user::syscall::linux::proc_broker_ops::tests::broker_authority_identity_exhaustion_never_wraps
authority-identity-lifecycle/AuthorityIdentityLifecycle|kernel-ipc-runtime|ipc::slab::tests::removed_handle_never_aliases_reused_slot
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
loader-request-authority/LoaderRequestAuthority|rustos-user-abi|syscall::syscall_tests::privileged_loader_operations_have_an_explicit_service_role_matrix
loader-request-authority/LoaderRequestAuthority|initd|tests::init_identity_is_published_before_any_loader_request_and_is_marked_requestless
loader-request-authority/LoaderRequestAuthority|kernel-compat|user::syscall::linux::proc_broker_ops::tests::loader_commit_revalidates_live_requester_role_before_consuming_authority
remote-file-mapping/RemoteFileMapping|rustos-user-abi|syscall::syscall_tests::statx_offload_messages_fit_inline_ipc_v1
remote-file-mapping/RemoteFileMapping|vfsd|tests::early_system_reads_chunk_larger_vfs_buffers_to_the_broker_bound
remote-file-mapping/RemoteFileMapping|kernel-compat|user::syscall::linux::proc_broker_ops::tests::truncated_file_mapping_never_commits_zero_filled_tail
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-compat|user::syscall::tests::the_entry_stub_is_the_whole_of_the_syscall_paths_fpu_custody
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-hal|arch::simd::tests::a_wide_simd_section_covers_every_register_the_entry_stubs_leave_behind
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-ps|multitask::scheduler::tests::scheduler_block_arm_is_exact_race_safe_and_terminally_revoked
syscall-simd-lifecycle/SyscallSimdLifecycle|kernel-ps|multitask::scheduler::tests::raced_wake_never_validates_a_consumed_current_frame
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
clocksource-deadline/ClocksourceDeadline|kernel-compat|user::syscall::linux::service_ops::process_time::tests::a_monotonic_instant_between_two_ticks_keeps_its_own_resolution
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
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::reschedule_observation::tests::notification_may_coalesce_requests_but_consumption_reaches_the_goal
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::reschedule_observation::tests::publication_after_claim_must_create_a_new_pending_edge
smp-reschedule-ipi/SmpRescheduleIpi|kernel-ps|multitask::irq::tests::reschedule_ipi_gate_retains_locked_work_and_dispatches_only_at_safe_point
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-hal|interrupt_stubs::tests::scheduler_commit_call_aligns_and_restores_incoming_rsp
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::cpu_local::tests::current_task_ownership_ignores_offline_slots_and_is_cpu_distinct
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::irq::tests::reschedule_ipi_gate_retains_locked_work_and_dispatches_only_at_safe_point
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::tests::architectural_restore_is_required_exactly_for_a_task_switch
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::tests::wake_transition_publishes_one_owner_before_commit_and_claims_once_after
scheduler-cpu-ownership/SchedulerCpuOwnership|kernel-ps|multitask::scheduler::smp::tests::remote_or_transition_owned_task_is_not_schedulable
scheduler-cpu-ownership/SchedulerCpuOwnership|nucleus-core|util::lockdep::tests::tracked_guard_release_requires_same_cpu_apic_and_positive_depth
scheduler-cpu-ownership/SchedulerCpuOwnership|nucleus-core|util::lockdep::tests::pending_acquire_units_cannot_consume_a_held_guard_pin
tlb-shootdown-lifecycle/TlbShootdownLifecycle|kernel-hal|arch::tlb_shootdown::tests::address_space_shootdown_targets_only_matching_active_roots
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
scheduler-active-balance/SchedulerActiveBalance|kernel-ps|multitask::scheduler::runqueue_policy::tests::active_balance_is_due_first_then_every_eighth_loaded_opportunity
scheduler-active-balance/SchedulerActiveBalance|kernel-ps|multitask::scheduler::smp::tests::source_migration_requires_exact_runnable_local_owner_and_target_affinity
scheduler-thread-demotion/SchedulerThreadDemotion|kernel-ps|multitask::scheduler::tests::self_demotion_removes_base_system_class_and_caps_fair_weight
scheduler-thread-demotion/SchedulerThreadDemotion|vfsd|tests::ui_bootstrap_demotion_requires_successful_terminal_snapshot_reply
scheduler-thread-demotion/SchedulerThreadDemotion|loaderd|tests::ui_bootstrap_demotion_is_custodied_until_terminal_reply
scheduler-thread-demotion/SchedulerThreadDemotion|uiserver|sys::tests::only_bootstrap_gpu_role_retains_inherited_boot_class
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::tests::ipc_admission_exports_only_the_live_bound_scheduling_context
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::tests::nested_passive_server_runtime_is_billed_to_the_root_caller_context
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::tests::deadline_domains_require_per_cpu_utilization_headroom
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::tests::production_user_slot_publication_rejects_an_unbudgeted_context
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::scheduling_context::tests::timeout_fault_is_one_shot_observable_and_never_retried
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_ledger::tests::ordinary_nested_calls_borrow_one_root_context_without_system_promotion
scheduling-context-budget/SchedulingContextBudget|kernel-ps|multitask::scheduler::donation_ledger::tests::multithreaded_server_charge_tokens_restore_the_previous_live_reply
scheduling-context-budget/SchedulingContextBudget|kernel-compat|user::syscall::linux::service_ops::ipc_helpers::tests::direct_bootstrap_consumes_exact_rootd_scheduling_authority_before_spawn
scheduling-context-budget/SchedulingContextBudget|kernel-compat|user::process::tests::production_process_spawn_surface_requires_scheduling_authority
scheduling-context-budget/SchedulingContextBudget|kernel-executive|boot::tests::rootd_bootstrap_is_published_with_a_bounded_scheduling_context
scheduling-context-budget/SchedulingContextBudget|rootd|tests::scheduling_policy_is_owned_by_the_immutable_service_manifest|host-test
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::runqueue::tests::direct_handoff_bypasses_the_fair_runqueue_and_is_cpu_exact
ipc-fast-handoff/IpcFastHandoff|kernel-ipc-runtime|ipc::tests::fast_call_tests::fast_call_uses_fixed_frame_and_exact_receiver_caller_identities
ipc-fast-handoff/IpcFastHandoff|kernel-ipc-runtime|ipc::tests::fast_call_tests::fast_call_rollback_restores_exact_front_waiter_and_custody
ipc-fast-handoff/IpcFastHandoff|kernel-ps|multitask::scheduler::tests::fast_ipc_commit_requires_exact_typed_waits_and_mutates_both_peers_once
ipc-fast-handoff/IpcFastHandoff|syscalld|fast_offload::tests::compact_id_wire_is_fixed_frame_bounded_sender_exact_and_lossless
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::synchronous_handoff_tests::synchronous_ipc_handoff_is_fifo_deduplicated_and_fairness_bounded
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::synchronous_handoff_tests::reply_wake_token_mint_requires_exact_task_and_dispatch_custody
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::synchronous_handoff_tests::terminal_reply_releases_donation_and_wakes_exact_caller_in_one_scheduler_operation
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_wake_token_rejects_stale_owner_generation_migration_and_retirement
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_generation_refresh_replaces_in_place_and_stale_generation_loses_urgency
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::generic_handoff_cannot_downgrade_reply_generation_custody
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::stale_reply_enqueue_has_no_scheduler_or_global_fallback
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_enqueue_rechecks_owner_after_the_exact_target_queue_mutation
synchronous-ipc-handoff/SynchronousIpcHandoff|kernel-ps|multitask::scheduler::runqueue::tests::direct_handoff_predicate_rejects_running_and_migrating_even_if_runnable
synchronous-ipc-handoff-concurrency/SynchronousIpcHandoffConcurrency|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_wake_token_rejects_stale_owner_generation_migration_and_retirement
synchronous-ipc-handoff-concurrency/SynchronousIpcHandoffConcurrency|kernel-ps|multitask::scheduler::sync_handoff::tests::stale_reply_enqueue_has_no_scheduler_or_global_fallback
synchronous-ipc-handoff-concurrency/SynchronousIpcHandoffConcurrency|kernel-ps|multitask::scheduler::sync_handoff::tests::reply_enqueue_rechecks_owner_after_the_exact_target_queue_mutation
synchronous-ipc-handoff-concurrency/SynchronousIpcHandoffConcurrency|kernel-ps|multitask::scheduler::runqueue::tests::direct_handoff_predicate_rejects_running_and_migrating_even_if_runnable
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
page-table-map-transaction/PageTableMapTransaction|kernel-mm|memory::address_space::rollback::tests::intermediate_tables_rollback_in_reverse_publication_order
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
        if [[ "$package" == "kernel-ps" ]]; then
            RUST_TEST_THREADS=1 cargo "${cargo_args[@]}" > "$run_dir/$group_index.out" 2>&1
        else
            cargo "${cargo_args[@]}" > "$run_dir/$group_index.out" 2>&1
        fi
        printf '%s' "$?" > "$run_dir/$group_index.rc"
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
