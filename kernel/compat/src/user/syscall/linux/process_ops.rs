// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this deleted ring0 implementation as source material for userspace services;
// do not restore it to the live kernel path without an explicit privileged-boundary decision.
//
// use super::*;
//
// pub(super) fn syscall_linux_memfd_create(name_ptr: u64, flags: u64) -> u64 {
//     match linux_ops::memfd_create(name_ptr, flags) {
//         Ok(fd) => fd,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_sched_yield() -> u64 {
//     linux_ops::sched_yield()
// }
//
// pub(super) fn syscall_linux_rt_sigaction(
//     signal: u64,
//     action_ptr: u64,
//     old_action_ptr: u64,
//     sigset_size: u64,
// ) -> u64 {
//     match linux_ops::rt_sigaction(signal, action_ptr, old_action_ptr, sigset_size) {
//         Ok(()) => 0,
//         Err(err) => {
//             debug::println!(
//                 "linux rt_sigaction rejected: signal={} action_ptr={:#x} old_action_ptr={:#x} sigset_size={} err={:?}",
//                 signal,
//                 action_ptr,
//                 old_action_ptr,
//                 sigset_size,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_execve(frame: &mut SyscallFrame) -> u64 {
//     match linux_ops::execve(frame.rdi, frame.rsi, frame.rdx) {
//         Ok(transition) => {
//             apply_exec_transition_to_frame(frame, transition);
//             transition.registers.rax
//         }
//         Err(err) => {
//             debug::println!(
//                 "linux execve rejected: path_ptr={:#x} argv_ptr={:#x} envp_ptr={:#x} path={} err={:?}",
//                 frame.rdi,
//                 frame.rsi,
//                 frame.rdx,
//                 debug_user_path(frame.rdi),
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_execveat(frame: &mut SyscallFrame) -> u64 {
//     match linux_ops::execveat(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8) {
//         Ok(transition) => {
//             apply_exec_transition_to_frame(frame, transition);
//             transition.registers.rax
//         }
//         Err(err) => {
//             debug::println!(
//                 "linux execveat rejected: dirfd={} path_ptr={:#x} argv_ptr={:#x} envp_ptr={:#x} flags={:#x} path={} err={:?}",
//                 frame.rdi,
//                 frame.rsi,
//                 frame.rdx,
//                 frame.r10,
//                 frame.r8,
//                 debug_user_path(frame.rsi),
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_rustos_spawn_exec(
//     path_ptr: u64,
//     argv_ptr: u64,
//     envp_ptr: u64,
//     flags: u64,
//     console_session_raw: u64,
//     weight_micros: u64,
// ) -> u64 {
//     if ipc_ops::service_endpoint(linux_abi::IPC_SERVICE_LOADERD).is_some()
//         && !current_process_has_loader_broker_capability()
//     {
//         return linux_errno(LINUX_EACCES);
//     }
//     match linux_ops::spawn_exec(
//         path_ptr,
//         argv_ptr,
//         envp_ptr,
//         flags,
//         console_session_raw,
//         weight_micros,
//     ) {
//         Ok(pid) => pid,
//         Err(err) => {
//             debug::println!(
//                 "linux rustos_spawn_exec rejected: path_ptr={:#x} argv_ptr={:#x} envp_ptr={:#x} flags={:#x} session={:#x} weight={} path={} err={:?}",
//                 path_ptr,
//                 argv_ptr,
//                 envp_ptr,
//                 flags,
//                 console_session_raw,
//                 weight_micros,
//                 debug_user_path(path_ptr),
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// fn current_process_has_loader_broker_capability() -> bool {
//     ipc_ops::current_process_has_service_capability(
//         rustos_user_abi::syscall::IPC_SERVICE_CAP_PROCESS_LOADER,
//     )
// }
//
// fn apply_exec_transition_to_frame(
//     frame: &mut SyscallFrame,
//     transition: linux_ops::LinuxExecTransition,
// ) {
//     frame.user_rip = transition.user_rip;
//     frame.user_rsp = transition.user_rsp;
//     frame.rdi = transition.registers.rdi;
//     frame.rsi = transition.registers.rsi;
//     frame.rdx = transition.registers.rdx;
//     frame.r8 = transition.registers.r8;
//     frame.r9 = transition.registers.r9;
//     frame.r10 = transition.registers.r10;
//     frame.rbx = transition.registers.rbx;
//     frame.rbp = transition.registers.rbp;
//     frame.r12 = transition.registers.r12;
//     frame.r13 = transition.registers.r13;
//     frame.r14 = transition.registers.r14;
//     frame.r15 = transition.registers.r15;
// }
//
// pub(super) fn syscall_linux_rt_sigprocmask(
//     how: u64,
//     set_ptr: u64,
//     oldset_ptr: u64,
//     sigset_size: u64,
// ) -> u64 {
//     match linux_ops::rt_sigprocmask(how, set_ptr, oldset_ptr, sigset_size) {
//         Ok(()) => 0,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_getpid() -> u64 {
//     linux_ops::getpid()
// }
//
// pub(super) fn syscall_linux_fork(frame: &SyscallFrame) -> u64 {
//     let clone_frame = linux_ops::LinuxCloneFrame {
//         user_rip: frame.user_rip,
//         user_rsp: frame.user_rsp,
//         user_rflags: frame.user_rflags,
//         registers: crate::multitask::UserTaskRegisters {
//             rax: frame.rax,
//             rbx: frame.rbx,
//             rcx: frame.user_rip,
//             rdx: frame.rdx,
//             rsi: frame.rsi,
//             rdi: frame.rdi,
//             rbp: frame.rbp,
//             r8: frame.r8,
//             r9: frame.r9,
//             r10: frame.r10,
//             r11: frame.user_rflags,
//             r12: frame.r12,
//             r13: frame.r13,
//             r14: frame.r14,
//             r15: frame.r15,
//         },
//     };
//     match linux_ops::fork(clone_frame) {
//         Ok(pid) => pid,
//         Err(err) => {
//             debug::println!("linux fork rejected: err={:?}", err);
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_clone(frame: &SyscallFrame) -> u64 {
//     let clone_frame = linux_ops::LinuxCloneFrame {
//         user_rip: frame.user_rip,
//         user_rsp: frame.user_rsp,
//         user_rflags: frame.user_rflags,
//         registers: crate::multitask::UserTaskRegisters {
//             rax: frame.rax,
//             rbx: frame.rbx,
//             rcx: frame.user_rip,
//             rdx: frame.rdx,
//             rsi: frame.rsi,
//             rdi: frame.rdi,
//             rbp: frame.rbp,
//             r8: frame.r8,
//             r9: frame.r9,
//             r10: frame.r10,
//             r11: frame.user_rflags,
//             r12: frame.r12,
//             r13: frame.r13,
//             r14: frame.r14,
//             r15: frame.r15,
//         },
//     };
//     match linux_ops::clone(
//         clone_frame,
//         frame.rdi,
//         frame.rsi,
//         frame.rdx,
//         frame.r10,
//         frame.r8,
//     ) {
//         Ok(tid) => tid,
//         Err(err) => {
//             debug::println!(
//                 "linux clone rejected: flags={:#x} child_stack={:#x} parent_tid={:#x} child_tid={:#x} tls={:#x} err={:?}",
//                 frame.rdi,
//                 frame.rsi,
//                 frame.rdx,
//                 frame.r10,
//                 frame.r8,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_clone3(frame: &SyscallFrame) -> u64 {
//     let clone_frame = linux_ops::LinuxCloneFrame {
//         user_rip: frame.user_rip,
//         user_rsp: frame.user_rsp,
//         user_rflags: frame.user_rflags,
//         registers: crate::multitask::UserTaskRegisters {
//             rax: frame.rax,
//             rbx: frame.rbx,
//             rcx: frame.user_rip,
//             rdx: frame.rdx,
//             rsi: frame.rsi,
//             rdi: frame.rdi,
//             rbp: frame.rbp,
//             r8: frame.r8,
//             r9: frame.r9,
//             r10: frame.r10,
//             r11: frame.user_rflags,
//             r12: frame.r12,
//             r13: frame.r13,
//             r14: frame.r14,
//             r15: frame.r15,
//         },
//     };
//     match linux_ops::clone3(clone_frame, frame.rdi, frame.rsi) {
//         Ok(tid) => tid,
//         Err(err) => {
//             debug::println!(
//                 "linux clone3 rejected: args_ptr={:#x} size={} err={:?}",
//                 frame.rdi,
//                 frame.rsi,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_wait4(pid: i64, status_ptr: u64, options: u64, rusage_ptr: u64) -> u64 {
//     match linux_ops::wait4(pid, status_ptr, options, rusage_ptr) {
//         Ok(waited) => waited as u64,
//         Err(linux_ops::LinuxSysopError::NoSuchProcess)
//             if options & linux_abi::WNOHANG as u64 != 0 =>
//         {
//             0
//         }
//         Err(linux_ops::LinuxSysopError::NoSuchProcess) => linux_errno(LINUX_ECHILD),
//         Err(err) => {
//             debug::println!(
//                 "linux wait4 rejected: pid={} status_ptr={:#x} options={:#x} rusage_ptr={:#x} err={:?}",
//                 pid,
//                 status_ptr,
//                 options,
//                 rusage_ptr,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_gettid() -> u64 {
//     linux_ops::gettid()
// }
//
// pub(super) fn syscall_linux_futex(
//     frame: &SyscallFrame,
//     uaddr: u64,
//     op: u64,
//     val: u64,
//     timeout_ptr: u64,
//     uaddr2: u64,
//     val3: u64,
// ) -> u64 {
//     if !multitask::current_console_session().is_system() {
//         let user_rsp = frame.user_rsp;
//         let return_rip = if user_rsp != 0 {
//             let mut bytes = [0_u8; 8];
//             match usermem::copy_from_current_user_exact(user_rsp, &mut bytes) {
//                 Ok(()) => u64::from_le_bytes(bytes),
//                 Err(_) => 0,
//             }
//         } else {
//             0
//         };
//         debug_log_secondary_linux_syscall(|| {
//             alloc::format!(
//                 "futex entry uaddr={:#x} op={:#x} val={:#x} timeout_ptr={:#x} uaddr2={:#x} val3={:#x} user_rsp={:#x} return_rip={:#x}",
//                 uaddr,
//                 op,
//                 val,
//                 timeout_ptr,
//                 uaddr2,
//                 val3,
//                 user_rsp,
//                 return_rip
//             )
//         });
//     }
//
//     match linux_ops::futex(uaddr, op, val, timeout_ptr, uaddr2, val3) {
//         Ok(value) => value,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_arch_prctl(code: u64, arg: u64) -> u64 {
//     match linux_ops::arch_prctl(code, arg) {
//         Ok(value) => value,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_set_tid_address(user_ptr: u64) -> u64 {
//     match linux_ops::set_tid_address(user_ptr) {
//         Ok(pid) => pid,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_clock_gettime(clock_id: u64, timespec_ptr: u64) -> u64 {
//     match linux_ops::clock_gettime(clock_id, timespec_ptr) {
//         Ok(()) => 0,
//         Err(err) => {
//             debug::println!(
//                 "linux clock_gettime rejected: clock_id={} timespec_ptr={:#x} err={:?}",
//                 clock_id,
//                 timespec_ptr,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_clock_nanosleep(
//     clock_id: u64,
//     flags: u64,
//     request_ptr: u64,
//     remaining_ptr: u64,
// ) -> u64 {
//     match linux_ops::clock_nanosleep(clock_id, flags, request_ptr, remaining_ptr) {
//         Ok(()) => 0,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_tgkill(tgid: u64, tid: u64, signal: u64) -> u64 {
//     match linux_ops::tgkill(tgid, tid, signal) {
//         Ok(()) => 0,
//         Err(err) => {
//             debug::println!(
//                 "linux tgkill rejected: tgid={} tid={} signal={} err={:?}",
//                 tgid,
//                 tid,
//                 signal,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_set_robust_list(head_ptr: u64, len: u64) -> u64 {
//     match linux_ops::set_robust_list(head_ptr, len) {
//         Ok(()) => 0,
//         Err(err) => {
//             debug::println!(
//                 "linux set_robust_list rejected: head_ptr={:#x} len={} err={:?}",
//                 head_ptr,
//                 len,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_get_robust_list(pid: u64, head_ptr_ptr: u64, len_ptr: u64) -> u64 {
//     match linux_ops::get_robust_list(pid, head_ptr_ptr, len_ptr) {
//         Ok(()) => 0,
//         Err(err) => {
//             debug::println!(
//                 "linux get_robust_list rejected: pid={} head_ptr_ptr={:#x} len_ptr={:#x} err={:?}",
//                 pid,
//                 head_ptr_ptr,
//                 len_ptr,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// // Linux getrandom moved to offload_ops::syscall_linux_getrandom; syscalld owns
// // the userspace RNG policy. The kernel CSPRNG path stays in linux_ops::getrandom
// // as a bootstrap fallback before syscalld registers.
//
// pub(super) fn syscall_linux_rseq(area_ptr: u64, len: u64, flags: u64, signature: u64) -> u64 {
//     match linux_ops::rseq(area_ptr, len, flags, signature) {
//         Ok(()) => 0,
//         Err(err) => {
//             debug::println!(
//                 "linux rseq rejected: area_ptr={:#x} len={} flags={:#x} signature={:#x} err={:?}",
//                 area_ptr,
//                 len,
//                 flags,
//                 signature,
//                 err,
//             );
//             linux_errno(linux_sysop_error_to_errno(err))
//         }
//     }
// }
//
// pub(super) fn syscall_linux_nanosleep(request_ptr: u64, remaining_ptr: u64) -> u64 {
//     match linux_ops::nanosleep(request_ptr, remaining_ptr) {
//         Ok(()) => 0,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
