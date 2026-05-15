// RING3-MIGRATION-REFERENCE: Linux process-fork clone remains intentionally
// outside the live syscall path. Preserve only the unsupported process-copy
// source material here; thread clone/clone3, wait4, getrandom, ioctl routing,
// and memfd handling have moved to service-owned live paths.
//
// pub(crate) fn fork(frame: LinuxCloneFrame) -> Result<u64, LinuxSysopError> {
//     let retained =
//         multitask::retain_current_user_process_state().ok_or(LinuxSysopError::Unsupported)?;
//     if retained.abi() != UserAbi::Linux {
//         return Err(LinuxSysopError::Unsupported);
//     }
//
//     let parent_pid = retained.process_id();
//     let parent_state = retained.process_state();
//     let child_address_space = parent_state
//         .address_space()
//         .clone_user_space()
//         .map_err(LinuxSysopError::AddressSpace)?;
//     let child_thread_state = clone_fork_thread_state(
//         multitask::current_linux_thread_state().ok_or(LinuxSysopError::Unsupported)?,
//     );
//     let console_session = multitask::current_console_session();
//     let user_stack = multitask::current_user_stack_state();
//
//     let mut bootstrap = multitask::UserTaskBootstrap::new(
//         UserAbi::Linux,
//         VirtAddr::new(frame.user_rip),
//         VirtAddr::new(frame.user_rsp),
//     );
//     bootstrap.registers = frame.registers;
//     bootstrap.registers.rax = 0;
//     bootstrap.registers.rcx = frame.user_rip;
//     bootstrap.registers.r11 = frame.user_rflags;
//     bootstrap.user_stack = user_stack;
//     bootstrap.console_session = console_session;
//     bootstrap.logical_admin = parent_state.security().is_logical_admin();
//     bootstrap.linux_process_state = parent_state.linux_process_state().copied();
//     bootstrap.linux_memory_map = parent_state.linux_memory_map().cloned();
//     bootstrap.linux_runtime_profile = parent_state.linux_runtime_profile().cloned();
//     bootstrap.linux_thread_state = Some(clone_fork_thread_state(child_thread_state));
//     bootstrap.set_exec_path(parent_state.exec_path());
//
//     let child_pid = multitask::spawn_user_process_with_parent(
//         child_address_space,
//         bootstrap,
//         Some(parent_pid),
//         multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
//     )?;
//
//     multitask::with_process_state_by_pid_mut(child_pid, |child_state| {
//         child_state.inherit_fork_process_metadata_from(parent_state);
//     })
//     .ok_or(LinuxSysopError::NoSuchProcess)?;
//     Ok(child_pid)
// }
//
// fn clone_fork_thread_state(mut state: linux_abi::LinuxThreadState) -> linux_abi::LinuxThreadState {
//     state.clear_child_tid = 0;
//     state.robust_list_head = 0;
//     state.robust_list_len = 0;
//     state.rseq_area = 0;
//     state.rseq_len = 0;
//     state.rseq_signature = 0;
//     state.pending_signals = 0;
//     state
// }
