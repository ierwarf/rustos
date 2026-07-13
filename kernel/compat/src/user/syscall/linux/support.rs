use super::*;

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

// RING3-MIGRATION-REFERENCE START: signal-frame-substrate exception: procd owns
// pending-signal selection and disposition policy. Ring0 keeps rt_sigframe
// construction, current-thread pending-bit clearing, and final register
// mutation substrate.
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
                let _ = multitask::mark_user_process_exiting(process_id);
                ipc_ops::cleanup_service_endpoints_for_process(process_id);
                super::cleanup_proc_broker_state_for_process(process_id);
                let _ = multitask::note_process_exit_status(process_id, wait_status);
                let parent = multitask::parent_process_id_of(process_id).unwrap_or(0);
                if parent != 0 {
                    multitask::queue_linux_signal(parent, parent, linux_abi::SIGCHLD as u64);
                }
            }
            multitask::exit_current_user_process();
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
// RING3-MIGRATION-REFERENCE END: procd-owned signal-frame substrate exception.

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
    if !super::super::syscall_frame_security_check(frame) {
        super::super::validate_syscall_entry_or_terminate(frame);
    }

    Ok(())
}
