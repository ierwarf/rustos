// RING3-MIGRATION-REFERENCE START: commercial-max syscalld should own cold Linux ABI
// compatibility tables, errno/default translations, and policy-facing helper logic.
// Ring0 keeps only helpers required by trap entry and privileged broker commits.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum LinuxSyscallSupport {
    Native,
    Partial,
    Stub,
}

const LINUX_SI_MAX_SIZE: usize = 128;
const LINUX_SA_ONSTACK: u64 = 0x0800_0000;
const LINUX_SIGNAL_RED_ZONE_SIZE: u64 = 128;
const LINUX_USER_CS: u16 = 0x33;
const LINUX_USER_SS: u16 = 0x2b;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxStackT {
    sp: u64,
    flags: i32,
    size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSigContext64 {
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rdi: u64,
    rsi: u64,
    rbp: u64,
    rbx: u64,
    rdx: u64,
    rax: u64,
    rcx: u64,
    rsp: u64,
    rip: u64,
    eflags: u64,
    cs: u16,
    gs: u16,
    fs: u16,
    ss: u16,
    err: u64,
    trapno: u64,
    oldmask: u64,
    cr2: u64,
    fpstate: u64,
    reserved1: [u64; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxUContext64 {
    uc_flags: u64,
    uc_link: u64,
    uc_stack: LinuxStackT,
    uc_mcontext: LinuxSigContext64,
    uc_sigmask: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinuxSigInfo {
    bytes: [u8; LINUX_SI_MAX_SIZE],
}

impl LinuxSigInfo {
    fn for_signal(signal: u64) -> Self {
        let mut info = Self::default();
        let signo = (signal as i32).to_le_bytes();
        info.bytes[0..4].copy_from_slice(&signo);
        info
    }
}

impl Default for LinuxSigInfo {
    fn default() -> Self {
        Self {
            bytes: [0; LINUX_SI_MAX_SIZE],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxRtSigFrame {
    pretcode: u64,
    uc: LinuxUContext64,
    info: LinuxSigInfo,
}

pub(super) fn deliver_pending_signals_if_needed(frame: &mut SyscallFrame) -> bool {
    let Some(thread_state) = multitask::current_linux_thread_state() else {
        return false;
    };
    if thread_state.pending_signals == 0 {
        return false;
    }
    let mut request = new_procd_request(PROCD_OP_SELECT_SIGNAL);
    request.arg0 = thread_state.pending_signals;
    let Ok(response) = call_procd(&request) else {
        return false;
    };
    if response.version != rustos_user_abi::syscall::PROCD_IPC_ABI_VERSION || response.status != 0 {
        return false;
    }
    let signal = response.signal as u64;
    match response.action {
        PROCD_SELECT_SIGNAL_NONE => {}
        PROCD_SELECT_SIGNAL_IGNORE => {
            clear_current_pending_signal(signal);
        }
        PROCD_SELECT_SIGNAL_TERMINATE => {
            clear_current_pending_signal(signal);
            if let Some(process_id) = multitask::current_user_process_id() {
                let wait_status = signal as i32;
                let _ = multitask::note_process_exit_status(process_id, wait_status);
            }
            let _ = multitask::exit_current_user_task();
        }
        PROCD_SELECT_SIGNAL_HANDLER => {
            if response.payload_len as usize != LINUX_SIGACTION_SIZE {
                return false;
            }
            let action =
                read_unaligned::<LinuxSigActionWire>(&response.payload[..LINUX_SIGACTION_SIZE]);
            if action.handler == 0 || action.restorer == 0 {
                return false;
            }
            if install_rt_signal_frame(frame, signal, thread_state.signal_mask, action).is_ok() {
                clear_current_pending_signal(signal);
                return true;
            }
        }
        _ => {}
    }
    false
}

pub(super) fn syscall_linux_rt_sigreturn(frame: &mut SyscallFrame) -> u64 {
    let saved = match read_rt_signal_frame(frame.user_rsp) {
        Ok(saved) => saved,
        Err(errno) => return linux_errno(errno),
    };
    let restored_rax = saved.uc.uc_mcontext.rax;
    restore_syscall_frame(frame, saved.uc.uc_mcontext);
    let _ = multitask::with_current_user_process_and_linux_thread_state_mut(
        |_, _, _, _, linux_thread_state| {
            if let Some(state) = linux_thread_state.as_mut() {
                state.signal_mask = saved.uc.uc_sigmask;
                state.signal_stack = linux_signal_stack_from_stack_t(saved.uc.uc_stack);
            }
        },
    );
    restored_rax
}

fn read_rt_signal_frame(rsp: u64) -> Result<LinuxRtSigFrame, i64> {
    if let Ok(saved) = usermem::read_current_user_struct::<LinuxRtSigFrame>(rsp) {
        if validate_rt_signal_frame(&saved) {
            return Ok(saved);
        }
    }
    let Some(frame_addr) = rsp.checked_sub(size_of::<u64>() as u64) else {
        return Err(LINUX_EFAULT);
    };
    match usermem::read_current_user_struct::<LinuxRtSigFrame>(frame_addr) {
        Ok(saved) if validate_rt_signal_frame(&saved) => Ok(saved),
        Ok(_) => Err(LINUX_EINVAL),
        Err(err) => Err(address_space_error_to_linux_errno(err)),
    }
}

fn validate_rt_signal_frame(frame: &LinuxRtSigFrame) -> bool {
    frame.pretcode != 0 && frame.uc.uc_mcontext.rip != 0 && frame.uc.uc_mcontext.rsp != 0
}

fn install_rt_signal_frame(
    frame: &mut SyscallFrame,
    signal: u64,
    saved_mask: u64,
    action: LinuxSigActionWire,
) -> Result<(), i64> {
    let saved_stack = current_signal_stack();
    let using_altstack = should_use_signal_altstack(frame.user_rsp, saved_stack, action.flags);
    let frame_top = signal_frame_top(frame.user_rsp, saved_stack, action.flags)?;
    let signal_frame = LinuxRtSigFrame {
        pretcode: action.restorer,
        uc: LinuxUContext64 {
            uc_flags: 0,
            uc_link: 0,
            uc_stack: stack_t_from_linux_signal_stack(saved_stack),
            uc_mcontext: frame_to_linux_sigcontext(frame, saved_mask),
            uc_sigmask: saved_mask,
        },
        info: LinuxSigInfo::for_signal(signal),
    };
    let frame_size = size_of::<LinuxRtSigFrame>() as u64;
    let aligned_rsp = frame_top
        .checked_sub(LINUX_SIGNAL_RED_ZONE_SIZE)
        .and_then(|rsp| rsp.checked_sub(frame_size))
        .map(|rsp| rsp & !0xf)
        .ok_or(LINUX_EFAULT)?;
    let new_rsp = aligned_rsp
        .checked_sub(size_of::<u64>() as u64)
        .ok_or(LINUX_EFAULT)?;
    usermem::write_current_user_bytes(new_rsp, as_bytes(&signal_frame))
        .map_err(address_space_error_to_linux_errno)?;
    frame.user_rsp = new_rsp;
    frame.user_rip = action.handler;
    frame.rax = 0;
    frame.rdi = signal;
    frame.rsi = new_rsp + linux_rt_sigframe_info_offset() as u64;
    frame.rdx = new_rsp + linux_rt_sigframe_ucontext_offset() as u64;
    let Some(signal_bit) = crate::user::sysops::linux::linux_signal_bit(signal) else {
        return Err(LINUX_EINVAL);
    };
    let _ = multitask::with_current_user_process_and_linux_thread_state_mut(
        |_, _, _, _, linux_thread_state| {
            if let Some(state) = linux_thread_state.as_mut() {
                state.signal_mask = saved_mask | action.mask | signal_bit;
                if using_altstack {
                    state.signal_stack.flags |= linux_abi::SS_ONSTACK;
                }
            }
        },
    );
    Ok(())
}

fn restore_syscall_frame(frame: &mut SyscallFrame, context: LinuxSigContext64) {
    frame.user_rsp = context.rsp;
    frame.user_rip = context.rip;
    frame.user_rflags = context.eflags;
    frame.rax = context.rax;
    frame.rdi = context.rdi;
    frame.rsi = context.rsi;
    frame.rdx = context.rdx;
    frame.r8 = context.r8;
    frame.r9 = context.r9;
    frame.r10 = context.r10;
    frame.rbx = context.rbx;
    frame.rbp = context.rbp;
    frame.r12 = context.r12;
    frame.r13 = context.r13;
    frame.r14 = context.r14;
    frame.r15 = context.r15;
}

fn frame_to_linux_sigcontext(frame: &SyscallFrame, saved_mask: u64) -> LinuxSigContext64 {
    LinuxSigContext64 {
        r8: frame.r8,
        r9: frame.r9,
        r10: frame.r10,
        r11: frame.user_rflags,
        r12: frame.r12,
        r13: frame.r13,
        r14: frame.r14,
        r15: frame.r15,
        rdi: frame.rdi,
        rsi: frame.rsi,
        rbp: frame.rbp,
        rbx: frame.rbx,
        rdx: frame.rdx,
        rax: frame.rax,
        rcx: frame.user_rip,
        rsp: frame.user_rsp,
        rip: frame.user_rip,
        eflags: frame.user_rflags,
        cs: LINUX_USER_CS,
        gs: 0,
        fs: 0,
        ss: LINUX_USER_SS,
        err: 0,
        trapno: 0,
        oldmask: saved_mask,
        cr2: 0,
        fpstate: 0,
        reserved1: [0; 8],
    }
}

fn current_signal_stack() -> linux_abi::LinuxSignalStack {
    multitask::current_linux_thread_state()
        .map(|state| state.signal_stack)
        .unwrap_or_default()
}

fn signal_frame_top(
    current_rsp: u64,
    signal_stack: linux_abi::LinuxSignalStack,
    action_flags: u64,
) -> Result<u64, i64> {
    if should_use_signal_altstack(current_rsp, signal_stack, action_flags) {
        return signal_stack
            .sp
            .checked_add(signal_stack.size)
            .ok_or(LINUX_EFAULT);
    }
    Ok(current_rsp)
}

fn should_use_signal_altstack(
    current_rsp: u64,
    signal_stack: linux_abi::LinuxSignalStack,
    action_flags: u64,
) -> bool {
    action_flags & LINUX_SA_ONSTACK != 0
        && signal_stack.flags & linux_abi::SS_DISABLE == 0
        && signal_stack.sp != 0
        && signal_stack.size != 0
        && !stack_contains(signal_stack, current_rsp)
}

fn stack_contains(signal_stack: linux_abi::LinuxSignalStack, address: u64) -> bool {
    let Some(end) = signal_stack.sp.checked_add(signal_stack.size) else {
        return false;
    };
    address >= signal_stack.sp && address < end
}

fn stack_t_from_linux_signal_stack(stack: linux_abi::LinuxSignalStack) -> LinuxStackT {
    LinuxStackT {
        sp: stack.sp,
        flags: stack.flags as i32,
        size: stack.size,
    }
}

fn linux_signal_stack_from_stack_t(stack: LinuxStackT) -> linux_abi::LinuxSignalStack {
    linux_abi::LinuxSignalStack {
        sp: stack.sp,
        flags: stack.flags as u32,
        _pad: 0,
        size: stack.size,
    }
}

fn linux_rt_sigframe_ucontext_offset() -> usize {
    let base = core::ptr::null::<LinuxRtSigFrame>();
    unsafe { core::ptr::addr_of!((*base).uc) as usize }
}

fn linux_rt_sigframe_info_offset() -> usize {
    let base = core::ptr::null::<LinuxRtSigFrame>();
    unsafe { core::ptr::addr_of!((*base).info) as usize }
}

fn clear_current_pending_signal(signal: u64) {
    let Some(bit) = crate::user::sysops::linux::linux_signal_bit(signal) else {
        return;
    };
    let _ = multitask::with_current_user_process_and_linux_thread_state_mut(
        |_, _, _, _, linux_thread_state| {
            if let Some(state) = linux_thread_state.as_mut() {
                state.pending_signals &= !bit;
            }
        },
    );
}

pub(super) fn syscall_check(frame: &SyscallFrame) -> Result<(), u64> {
    let Some(support) = linux_syscall_support_level(frame.rax) else {
        debug::println!(
            "unsupported linux syscall: nr={} rip={:#x} rdi={:#x} rsi={:#x} rdx={:#x} r10={:#x}",
            frame.rax,
            frame.user_rip,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
        );
        return Err(linux_errno(LINUX_ENOSYS));
    };

    if !super::super::syscall_frame_security_check(frame) {
        super::super::validate_syscall_entry_or_terminate(frame);
    }

    if support == LinuxSyscallSupport::Stub {
        return Err(linux_errno(LINUX_ENOSYS));
    }

    Ok(())
}

fn linux_syscall_support_level(syscall_number: u64) -> Option<LinuxSyscallSupport> {
    if !linux_syscall_number_supported(syscall_number) {
        return None;
    }

    Some(LinuxSyscallSupport::Native)
}

fn linux_syscall_number_supported(syscall_number: u64) -> bool {
    ipc_ops::is_linux_rustos_ipc_syscall(syscall_number)
        || broker_ops::is_linux_rustos_broker_syscall(syscall_number)
        || matches!(
            syscall_number,
            linux_abi::SYS_READ
                | linux_abi::SYS_WRITE
                | linux_abi::SYS_UNLINK
                | linux_abi::SYS_RUSTOS_DEBUG_PRINT
                | linux_abi::SYS_RUSTOS_SPAWN_EXEC
                | linux_abi::SYS_RUSTOS_PROC_PREPARE_BROKER
                | linux_abi::SYS_RUSTOS_PROC_MAP_FILE_BROKER
                | linux_abi::SYS_RUSTOS_PROC_MAP_ZEROED_BROKER
                | linux_abi::SYS_RUSTOS_PROC_MAP_DATA_BROKER
                | SYS_RUSTOS_PROC_SET_IMAGE_BLOB_BROKER
                | SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER
                | linux_abi::SYS_RUSTOS_PROC_COMMIT_BROKER
                | linux_abi::SYS_RUSTOS_PROC_ABORT_BROKER
                | linux_abi::SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER
                | SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER
                | linux_abi::SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER
                | linux_abi::SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER
                | linux_abi::SYS_RUSTOS_PROC_EXEC_TARGET_BROKER
                | linux_abi::SYS_RUSTOS_PROC_FORK_BROKER
                | linux_abi::SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER
                | linux_abi::SYS_RUSTOS_MM_BROKER
                | linux_abi::SYS_RUSTOS_BLOCK_BROKER
                | linux_abi::SYS_CLOSE
                | linux_abi::SYS_SOCKET
                | linux_abi::SYS_SENDTO
                | linux_abi::SYS_ACCEPT
                | linux_abi::SYS_FTRUNCATE
                | linux_abi::SYS_FSTAT
                | linux_abi::SYS_POLL
                | linux_abi::SYS_PPOLL
                | linux_abi::SYS_EPOLL_WAIT
                | linux_abi::SYS_EPOLL_PWAIT
                | linux_abi::SYS_EPOLL_CTL
                | linux_abi::SYS_DUP
                | linux_abi::SYS_DUP2
                | linux_abi::SYS_LSEEK
                | linux_abi::SYS_WRITEV
                | linux_abi::SYS_ACCESS
                | linux_abi::SYS_SCHED_YIELD
                | linux_abi::SYS_MOUNT
                | linux_abi::SYS_GETCWD
                | linux_abi::SYS_MKDIR
                | linux_abi::SYS_CHDIR
                | linux_abi::SYS_MMAP
                | linux_abi::SYS_MPROTECT
                | linux_abi::SYS_MUNMAP
                | linux_abi::SYS_MADVISE
                | linux_abi::SYS_SIGALTSTACK
                | linux_abi::SYS_BRK
                | linux_abi::SYS_RT_SIGACTION
                | linux_abi::SYS_RT_SIGPROCMASK
                | linux_abi::SYS_RT_SIGRETURN
                | linux_abi::SYS_IOCTL
                | linux_abi::SYS_PREAD64
                | linux_abi::SYS_NANOSLEEP
                | linux_abi::SYS_GETPID
                | linux_abi::SYS_FORK
                | linux_abi::SYS_WAIT4
                | linux_abi::SYS_CLONE
                | linux_abi::SYS_UNAME
                | linux_abi::SYS_GETTID
                | linux_abi::SYS_PRLIMIT64
                | linux_abi::SYS_SCHED_GETAFFINITY
                | linux_abi::SYS_GETUID
                | linux_abi::SYS_GETGID
                | linux_abi::SYS_GETEUID
                | linux_abi::SYS_GETEGID
                | linux_abi::SYS_SETUID
                | linux_abi::SYS_SETGID
                | linux_abi::SYS_FCNTL
                | linux_abi::SYS_READLINK
                | linux_abi::SYS_FUTEX
                | linux_abi::SYS_ARCH_PRCTL
                | linux_abi::SYS_SET_TID_ADDRESS
                | linux_abi::SYS_CLOCK_GETTIME
                | linux_abi::SYS_CLOCK_NANOSLEEP
                | linux_abi::SYS_EXECVE
                | linux_abi::SYS_KILL
                | linux_abi::SYS_TKILL
                | linux_abi::SYS_TGKILL
                | linux_abi::SYS_OPENAT
                | linux_abi::SYS_UNLINKAT
                | linux_abi::SYS_SOCKETPAIR
                | linux_abi::SYS_BIND
                | linux_abi::SYS_CONNECT
                | linux_abi::SYS_LISTEN
                | linux_abi::SYS_ACCEPT4
                | linux_abi::SYS_GETSOCKNAME
                | linux_abi::SYS_GETPEERNAME
                | linux_abi::SYS_SETSOCKOPT
                | linux_abi::SYS_GETSOCKOPT
                | linux_abi::SYS_SHUTDOWN
                | linux_abi::SYS_SENDMSG
                | linux_abi::SYS_RECVFROM
                | linux_abi::SYS_RECVMSG
                | linux_abi::SYS_GETDENTS64
                | linux_abi::SYS_EXECVEAT
                | linux_abi::SYS_NEWFSTATAT
                | linux_abi::SYS_UMOUNT2
                | linux_abi::SYS_READLINKAT
                | linux_abi::SYS_FACCESSAT
                | linux_abi::SYS_SET_ROBUST_LIST
                | linux_abi::SYS_GET_ROBUST_LIST
                | linux_abi::SYS_DUP3
                | linux_abi::SYS_EPOLL_CREATE1
                | linux_abi::SYS_UMASK
                | linux_abi::SYS_GETRANDOM
                | linux_abi::SYS_STATX
                | linux_abi::SYS_RSEQ
                | linux_abi::SYS_CLONE3
                | linux_abi::SYS_MEMFD_CREATE
                | linux_abi::SYS_GETPPID
                | linux_abi::SYS_GETPGID
                | linux_abi::SYS_SETPGID
                | linux_abi::SYS_GETSID
                | linux_abi::SYS_SETSID
                | linux_abi::SYS_EXIT
                | linux_abi::SYS_EXIT_GROUP
        )
}
// RING3-MIGRATION-REFERENCE END: commercial-max syscalld-owned Linux syscall support policy.
