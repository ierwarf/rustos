//! Linux fork broker transaction.
//!
//! The child process-table generation and lifecycle transaction are reserved
//! before the address-space clone. The reservation is invisible until the
//! scheduler publishes a suspended task; every failure before that point
//! returns its exact token.

use super::*;

pub(super) fn syscall_linux_rustos_proc_fork_broker(args_ptr: u64) -> u64 {
    if !current_process_can_policy() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcForkBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || args.source_pid == 0
        || args.source_tid == 0
        || !valid_process_fork_plan_locally(&args)
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(thread_snapshot) =
        multitask::linux_thread_snapshot_by_ids(args.source_pid, args.source_tid)
    else {
        return linux_errno(LINUX_ESRCH);
    };
    let spawn_reservation = match multitask::reserve_process_spawn() {
        Some(reservation) => reservation,
        None => return linux_errno(LINUX_EAGAIN),
    };
    let child_state = match multitask::with_process_state_by_pid(args.source_pid, |parent| {
        let address_space = parent.address_space().clone_user_space()?;
        if args.clone_flags & linux_abi::CLONE_CHILD_SETTID != 0 {
            address_space.validate_user_write_buffer(
                VirtAddr::new(args.ctid_ptr),
                core::mem::size_of::<u32>(),
            )?;
        }
        Ok::<_, crate::memory::paging::AddressSpaceError>(parent.fork_clone(address_space, None))
    }) {
        Some(Ok(state)) => state,
        Some(Err(err)) => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        None => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            return linux_errno(LINUX_ESRCH);
        }
    };
    let mut child_thread_state = thread_snapshot.thread_state;
    child_thread_state.clear_child_tid = if args.clone_flags & linux_abi::CLONE_CHILD_CLEARTID != 0
    {
        args.ctid_ptr
    } else {
        0
    };
    child_thread_state.robust_list_head = 0;
    child_thread_state.robust_list_len = 0;
    child_thread_state.rseq_area = 0;
    child_thread_state.rseq_len = 0;
    child_thread_state.rseq_signature = 0;
    child_thread_state.pending_signals = 0;
    child_thread_state.pending_sigchld_events = 0;

    let mut bootstrap = multitask::UserTaskBootstrap::new(
        crate::user::abi::UserAbi::Linux,
        VirtAddr::new(args.registers.rip),
        VirtAddr::new(if args.stack_ptr != 0 {
            args.stack_ptr
        } else {
            args.registers.rsp
        }),
    );
    bootstrap.registers = user_registers_to_task_registers(args.registers);
    bootstrap.registers.rax = 0;
    bootstrap.registers.rcx = args.registers.rip;
    bootstrap.registers.r11 = args.registers.rflags;
    bootstrap.user_stack = thread_snapshot.user_stack;
    bootstrap.console_session = thread_snapshot.console_session;
    bootstrap.logical_admin = child_state.security().is_logical_admin();
    bootstrap.linux_process_state = child_state.linux_process_state().copied();
    bootstrap.linux_memory_map = child_state.linux_memory_map().cloned();
    bootstrap.linux_runtime_profile = child_state.linux_runtime_profile().cloned();
    bootstrap.linux_thread_state = Some(child_thread_state);
    bootstrap.set_exec_path(child_state.exec_path());

    let inherited_service_refs = match acquire_cloned_service_handle_refs(&child_state) {
        Ok(refs) => refs,
        Err(errno) => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            return linux_errno(errno);
        }
    };
    let child_pid = match multitask::spawn_user_process_state_suspended_with_parent_reservation(
        child_state,
        bootstrap,
        Some(args.source_pid),
        multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
        spawn_reservation,
    ) {
        Ok(pid) => pid,
        Err(err) => {
            release_service_handle_refs(&inherited_service_refs);
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            return linux_errno(process_spawn_error_to_linux_errno(err));
        }
    };
    if args.clone_flags & linux_abi::CLONE_CHILD_SETTID != 0 {
        let child_tid = (child_pid as u32).to_le_bytes();
        let write_result = multitask::with_process_state_by_pid_mut(child_pid, |child| {
            child
                .address_space()
                .copy_into_user(VirtAddr::new(args.ctid_ptr), &child_tid)
        });
        match write_result {
            Some(Ok(())) => {}
            Some(Err(err)) => {
                let _ = multitask::terminate_user_task(child_pid);
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            None => {
                let _ = multitask::terminate_user_task(child_pid);
                return linux_errno(LINUX_EAGAIN);
            }
        }
    }
    if !multitask::activate_suspended_user_task(child_pid) {
        let _ = multitask::terminate_user_task(child_pid);
        return linux_errno(LINUX_EAGAIN);
    }
    multitask::set_next_spawn_pick_hint(child_pid);
    multitask::request_deferred_reschedule();
    child_pid
}

pub(super) fn valid_process_fork_plan_locally(args: &RustosProcForkBrokerArgs) -> bool {
    let supported =
        linux_abi::CSIGNAL | linux_abi::CLONE_CHILD_SETTID | linux_abi::CLONE_CHILD_CLEARTID;
    let exit_signal = args.clone_flags & linux_abi::CSIGNAL;
    args.clone_flags & !supported == 0
        && (exit_signal == 0 || exit_signal == linux_abi::SIGCHLD)
        && args.ptid_ptr == 0
        && args.tls == 0
        && (args.clone_flags & (linux_abi::CLONE_CHILD_SETTID | linux_abi::CLONE_CHILD_CLEARTID)
            == 0
            || (PROC_BROKER_USER_SPACE_BASE..PROC_BROKER_USER_SPACE_END_EXCLUSIVE)
                .contains(&args.ctid_ptr))
}
