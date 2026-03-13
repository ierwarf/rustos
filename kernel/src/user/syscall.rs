use core::arch::global_asm;
use core::cmp::min;
use core::convert::TryFrom;

use x86_64::VirtAddr;
use x86_64::registers::control::{Efer, EferFlags};
use x86_64::registers::model_specific::{FsBase, GsBase, KernelGsBase, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::paging::PageTableFlags;

use crate::user::abi::UserAbi;
use crate::user::linux;
use crate::{debug, gdt, multitask, paging, rtc, tty};

const SYSCALL_STACK_SIZE: usize = 16 * 1024;
const CONSOLE_IO_CHUNK_LEN: usize = 256;
const SYSCALL_ERR_INVALID: u64 = u64::MAX;
const PAGE_SIZE: u64 = 4096;
const LINUX_ENOMEM: i64 = 12;
const LINUX_EBADF: i64 = 9;
const LINUX_EFAULT: i64 = 14;
const LINUX_EINVAL: i64 = 22;
const LINUX_ENOTTY: i64 = 25;
const LINUX_ENOSYS: i64 = 38;
const LINUX_SIGSET_SIZE: u64 = 8;

#[repr(C, align(16))]
struct SyscallCpuLocal {
    kernel_stack_top: u64,
    user_rsp: u64,
}

#[repr(align(16))]
struct SyscallFallbackStack([u8; SYSCALL_STACK_SIZE]);

#[repr(C)]
struct SyscallFrame {
    user_rsp: u64,
    user_rip: u64,
    user_rflags: u64,
    rax: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}

#[repr(C)]
struct LinuxTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

const _: [(); 128] = [(); core::mem::size_of::<SyscallFrame>()];

static mut SYSCALL_CPU_LOCAL: SyscallCpuLocal = SyscallCpuLocal {
    kernel_stack_top: 0,
    user_rsp: 0,
};
static mut SYSCALL_FALLBACK_STACK: SyscallFallbackStack =
    SyscallFallbackStack([0; SYSCALL_STACK_SIZE]);

const USER_GS_BASE_DEFAULT: u64 = 0;

global_asm!(
    r#"
    .global syscall_entry
    .type syscall_entry, @function
    syscall_entry:
        swapgs
        mov gs:[8], rsp
        mov rsp, gs:[0]
        sub rsp, 128

        mov [rsp + 24], rax
        mov rax, gs:[8]
        mov [rsp + 0], rax
        mov [rsp + 8], rcx
        mov [rsp + 16], r11
        mov [rsp + 32], rdi
        mov [rsp + 40], rsi
        mov [rsp + 48], rdx
        mov [rsp + 56], r8
        mov [rsp + 64], r9
        mov [rsp + 72], r10
        mov [rsp + 80], rbx
        mov [rsp + 88], rbp
        mov [rsp + 96], r12
        mov [rsp + 104], r13
        mov [rsp + 112], r14
        mov [rsp + 120], r15

        cld
        mov rdi, rsp
        call syscall_dispatch

        mov rdi, [rsp + 32]
        mov rsi, [rsp + 40]
        mov rdx, [rsp + 48]
        mov r8, [rsp + 56]
        mov r9, [rsp + 64]
        mov r10, [rsp + 72]
        mov rbx, [rsp + 80]
        mov rbp, [rsp + 88]
        mov r12, [rsp + 96]
        mov r13, [rsp + 104]
        mov r14, [rsp + 112]
        mov r15, [rsp + 120]
        mov r11, [rsp + 16]
        mov rcx, [rsp + 8]
        mov rsp, [rsp + 0]
        swapgs
        sysretq
    .size syscall_entry, . - syscall_entry
"#
);

pub fn init() {
    let (_cpu_local_addr, stack_base) = unsafe {
        (
            syscall_cpu_local_addr(),
            paging::higher_half_addr(core::ptr::addr_of!(SYSCALL_FALLBACK_STACK.0) as u64),
        )
    };

    unsafe {
        SYSCALL_CPU_LOCAL.kernel_stack_top = stack_base + SYSCALL_STACK_SIZE as u64;
    }

    let entry_addr = paging::higher_half_addr(syscall_entry as *const () as usize as u64);
    let syscall_mask = RFlags::INTERRUPT_FLAG | RFlags::TRAP_FLAG | RFlags::DIRECTION_FLAG;

    unsafe {
        Efer::write(
            Efer::read() | EferFlags::SYSTEM_CALL_EXTENSIONS | EferFlags::NO_EXECUTE_ENABLE,
        );
        Star::write(
            gdt::user_code_selector(),
            gdt::user_data_selector(),
            gdt::kernel_code_selector(),
            gdt::kernel_data_selector(),
        )
        .expect("syscall STAR selectors must be valid");
        LStar::write(VirtAddr::new(entry_addr));
    }
    SFMask::write(syscall_mask);
    prepare_for_context_return(false);
}

pub fn set_kernel_stack_top(kernel_stack_top: u64) {
    if kernel_stack_top == 0 {
        return;
    }

    unsafe {
        SYSCALL_CPU_LOCAL.kernel_stack_top = kernel_stack_top;
    }
}

pub fn prepare_for_context_return(returning_to_user: bool) {
    let kernel_gs_base = VirtAddr::new(unsafe { syscall_cpu_local_addr() });
    let user_gs_base = VirtAddr::new(USER_GS_BASE_DEFAULT);
    if returning_to_user {
        GsBase::write(user_gs_base);
        KernelGsBase::write(kernel_gs_base);
    } else {
        GsBase::write(kernel_gs_base);
        KernelGsBase::write(user_gs_base);
    }
}

unsafe fn syscall_cpu_local_addr() -> u64 {
    paging::higher_half_addr(core::ptr::addr_of!(SYSCALL_CPU_LOCAL) as u64)
}

unsafe extern "C" {
    fn syscall_entry();
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) -> u64 {
    let frame = unsafe { &mut *frame };
    multitask::save_current_fx_state();
    let result = dispatch_syscall(frame);
    multitask::restore_current_fx_state();
    result
}

fn dispatch_syscall(frame: &SyscallFrame) -> u64 {
    match multitask::current_user_abi() {
        Some(UserAbi::Linux) => dispatch_linux_syscall(frame),
        Some(UserAbi::Windows) => crate::win32::dispatch_syscall(
            frame.rax, frame.rdi, frame.rsi, frame.rdx, frame.r8, frame.r9, frame.r10,
        )
        .unwrap_or(SYSCALL_ERR_INVALID),
        None => SYSCALL_ERR_INVALID,
    }
}

fn dispatch_linux_syscall(frame: &SyscallFrame) -> u64 {
    match frame.rax {
        linux::SYS_READ => syscall_linux_read(frame.rdi, frame.rsi, frame.rdx),
        linux::SYS_WRITE => syscall_linux_write(frame.rdi, frame.rsi, frame.rdx),
        linux::SYS_MMAP => syscall_linux_mmap(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux::SYS_MPROTECT => syscall_linux_mprotect(frame.rdi, frame.rsi, frame.rdx),
        linux::SYS_BRK => syscall_linux_brk(frame.rdi),
        linux::SYS_RT_SIGPROCMASK => {
            syscall_linux_rt_sigprocmask(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux::SYS_IOCTL => syscall_linux_ioctl(frame.rdi, frame.rsi, frame.rdx),
        linux::SYS_NANOSLEEP => syscall_linux_nanosleep(frame.rdi, frame.rsi),
        linux::SYS_GETPID => syscall_linux_getpid(),
        linux::SYS_ARCH_PRCTL => syscall_linux_arch_prctl(frame.rdi, frame.rsi),
        linux::SYS_SET_TID_ADDRESS => syscall_linux_set_tid_address(frame.rdi),
        linux::SYS_EXIT | linux::SYS_EXIT_GROUP => syscall_process_exit(frame.rdi),
        _ => linux_errno(LINUX_ENOSYS),
    }
}

fn syscall_process_exit(status: u64) -> u64 {
    let _ = status;
    let _ = multitask::with_current_user_process_mut(|_, abi, address_space, linux_state| {
        if abi != UserAbi::Linux {
            return;
        }

        let Some(state) = linux_state.as_mut() else {
            return;
        };
        if state.clear_child_tid == 0 {
            return;
        }

        let _ = address_space
            .copy_into_user(VirtAddr::new(state.clear_child_tid), &0_u32.to_le_bytes());
        state.clear_child_tid = 0;
    });
    multitask::exit_current_user_task()
}

fn syscall_linux_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if !matches!(fd, 1 | 2) {
        return linux_errno(LINUX_EBADF);
    }

    let Ok(len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if len == 0 {
        return 0;
    }

    match console_write_from_user(user_ptr, len) {
        Ok(written) => written as u64,
        Err(err) => {
            debug::println!(
                "linux write fault: fd={} user_ptr={:#x} len={} err={:?}",
                fd,
                user_ptr,
                len,
                err,
            );
            linux_errno(address_space_error_to_linux_errno(err))
        }
    }
}

fn syscall_linux_read(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if fd != 0 {
        return linux_errno(LINUX_EBADF);
    }

    let Ok(len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if len == 0 {
        return 0;
    }

    match console_read_into_user(user_ptr, len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux read fault: fd={} user_ptr={:#x} len={} err={:?}",
                fd,
                user_ptr,
                len,
                err,
            );
            linux_errno(address_space_error_to_linux_errno(err))
        }
    }
}

fn syscall_linux_mmap(
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> u64 {
    let supported_prot = linux::PROT_READ | linux::PROT_WRITE | linux::PROT_EXEC;
    if prot & !supported_prot != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if requested_addr != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if fd != u64::MAX || offset != 0 {
        return linux_errno(LINUX_EBADF);
    }
    if flags & linux::MAP_PRIVATE == 0 || flags & linux::MAP_ANONYMOUS == 0 {
        return linux_errno(LINUX_EINVAL);
    }

    let Ok(len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if len == 0 {
        return linux_errno(LINUX_EINVAL);
    }

    let page_count = len.div_ceil(PAGE_SIZE as usize);
    let page_flags = linux_mmap_page_flags(prot);

    let Some(result) =
        multitask::with_current_user_process_mut(|_, abi, address_space, linux_state| {
            if abi != UserAbi::Linux {
                return linux_errno(LINUX_ENOSYS);
            }

            let Some(state) = linux_state.as_mut() else {
                return linux_errno(LINUX_ENOSYS);
            };

            let start = align_up(state.mmap_next, PAGE_SIZE);
            let span = match (page_count as u64).checked_mul(PAGE_SIZE) {
                Some(value) => value,
                None => return linux_errno(LINUX_ENOMEM),
            };
            let end = match start.checked_add(span) {
                Some(value) => value,
                None => return linux_errno(LINUX_ENOMEM),
            };
            if end > state.brk_limit() || end <= state.brk_mapped_end {
                return linux_errno(LINUX_ENOMEM);
            }

            match address_space.map_zeroed_user_pages_at(
                VirtAddr::new(start),
                page_count,
                page_flags,
            ) {
                Ok(region) => {
                    state.mmap_next = align_up(region.end().as_u64(), PAGE_SIZE);
                    region.start.as_u64()
                }
                Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
            }
        })
    else {
        return linux_errno(LINUX_ENOSYS);
    };

    result
}

fn syscall_linux_mprotect(start: u64, user_len: u64, prot: u64) -> u64 {
    let supported_prot = linux::PROT_READ | linux::PROT_WRITE | linux::PROT_EXEC;
    if prot & !supported_prot != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let Ok(len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if len == 0 {
        return 0;
    }

    let Some(result) = multitask::with_current_user_process_mut(|_, abi, address_space, _| {
        if abi != UserAbi::Linux {
            return linux_errno(LINUX_ENOSYS);
        }

        let validation = if prot & linux::PROT_WRITE != 0 {
            address_space.validate_user_write_buffer(VirtAddr::new(start), len)
        } else {
            address_space.validate_user_read_buffer(VirtAddr::new(start), len)
        };

        match validation {
            Ok(()) => 0,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        }
    }) else {
        return linux_errno(LINUX_ENOSYS);
    };

    result
}

fn syscall_linux_brk(addr: u64) -> u64 {
    let Some(result) =
        multitask::with_current_user_process_mut(|_, abi, address_space, linux_state| {
            if abi != UserAbi::Linux {
                return 0;
            }

            let Some(state) = linux_state.as_mut() else {
                return 0;
            };
            if addr == 0 {
                return state.brk_current;
            }
            if addr < state.brk_start {
                return state.brk_current;
            }

            let requested_mapped_end = align_up(addr, PAGE_SIZE);
            if !state.can_grow_brk_to(requested_mapped_end) {
                return state.brk_current;
            }

            if requested_mapped_end > state.brk_mapped_end {
                let delta = requested_mapped_end - state.brk_mapped_end;
                let page_count = (delta / PAGE_SIZE) as usize;
                let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
                if address_space
                    .map_zeroed_user_pages_at(
                        VirtAddr::new(state.brk_mapped_end),
                        page_count,
                        flags,
                    )
                    .is_err()
                {
                    return state.brk_current;
                }
                state.brk_mapped_end = requested_mapped_end;
            }

            state.brk_current = addr;
            addr
        })
    else {
        return 0;
    };

    result
}

fn syscall_linux_rt_sigprocmask(how: u64, set_ptr: u64, oldset_ptr: u64, sigset_size: u64) -> u64 {
    let _ = how;
    if sigset_size != LINUX_SIGSET_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    if set_ptr != 0 {
        let mut incoming = [0_u8; LINUX_SIGSET_SIZE as usize];
        if copy_from_user_exact(set_ptr, &mut incoming).is_err() {
            return linux_errno(LINUX_EFAULT);
        }
    }
    if oldset_ptr != 0 && write_user_bytes(oldset_ptr, &0_u64.to_le_bytes()).is_err() {
        return linux_errno(LINUX_EFAULT);
    }
    0
}

fn syscall_linux_ioctl(fd: u64, _request: u64, _arg: u64) -> u64 {
    if !matches!(fd, 0 | 1 | 2) {
        return linux_errno(LINUX_EBADF);
    }

    linux_errno(LINUX_ENOTTY)
}

fn syscall_linux_getpid() -> u64 {
    multitask::current_user_id().unwrap_or(0)
}

fn syscall_linux_arch_prctl(code: u64, arg: u64) -> u64 {
    match code {
        linux::ARCH_SET_FS => {
            if arg != 0
                && !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&arg)
            {
                return linux_errno(LINUX_EINVAL);
            }

            let Some(result) =
                multitask::with_current_user_process_mut(|_, abi, _, linux_state| {
                    if abi != UserAbi::Linux {
                        return linux_errno(LINUX_ENOSYS);
                    }

                    let Some(state) = linux_state.as_mut() else {
                        return linux_errno(LINUX_ENOSYS);
                    };
                    state.fs_base = arg;
                    FsBase::write(VirtAddr::new(arg));
                    0
                })
            else {
                return linux_errno(LINUX_ENOSYS);
            };
            result
        }
        linux::ARCH_GET_FS => match write_user_bytes(arg, &FsBase::read().as_u64().to_le_bytes()) {
            Ok(()) => 0,
            Err(_) => linux_errno(LINUX_EFAULT),
        },
        _ => linux_errno(LINUX_EINVAL),
    }
}

fn syscall_linux_set_tid_address(user_ptr: u64) -> u64 {
    let Some(result) = multitask::with_current_user_process_mut(|pid, abi, _, linux_state| {
        if abi != UserAbi::Linux {
            return linux_errno(LINUX_ENOSYS);
        }

        let Some(state) = linux_state.as_mut() else {
            return linux_errno(LINUX_ENOSYS);
        };
        state.clear_child_tid = user_ptr;
        pid
    }) else {
        return linux_errno(LINUX_ENOSYS);
    };

    result
}

fn syscall_linux_nanosleep(request_ptr: u64, remaining_ptr: u64) -> u64 {
    let mut request = LinuxTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let request_bytes = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(request) as *mut u8,
            core::mem::size_of::<LinuxTimespec>(),
        )
    };
    if copy_from_user_exact(request_ptr, request_bytes).is_err() {
        return linux_errno(LINUX_EFAULT);
    }

    if request.tv_sec < 0 || !(0..1_000_000_000).contains(&request.tv_nsec) {
        return linux_errno(LINUX_EINVAL);
    }

    let Ok(seconds) = u64::try_from(request.tv_sec) else {
        return linux_errno(LINUX_EINVAL);
    };
    let Ok(nanoseconds) = u64::try_from(request.tv_nsec) else {
        return linux_errno(LINUX_EINVAL);
    };
    let milliseconds = seconds
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(nanoseconds.div_ceil(1_000_000)))
        .unwrap_or(u64::MAX);

    sleep_ms(milliseconds);

    if remaining_ptr != 0 {
        let remaining = LinuxTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let remaining_bytes = unsafe {
            core::slice::from_raw_parts(
                core::ptr::addr_of!(remaining) as *const u8,
                core::mem::size_of::<LinuxTimespec>(),
            )
        };
        if write_user_bytes(remaining_ptr, remaining_bytes).is_err() {
            return linux_errno(LINUX_EFAULT);
        }
    }

    0
}

fn linux_errno(errno: i64) -> u64 {
    (-errno) as u64
}

fn address_space_error_to_linux_errno(err: paging::AddressSpaceError) -> i64 {
    match err {
        paging::AddressSpaceError::ProtectionViolation
        | paging::AddressSpaceError::NotMapped
        | paging::AddressSpaceError::HugePageConflict => LINUX_EFAULT,
        paging::AddressSpaceError::ZeroSizedAllocation
        | paging::AddressSpaceError::AddressOverflow
        | paging::AddressSpaceError::AddressOutOfRange
        | paging::AddressSpaceError::AddressNotPageAligned
        | paging::AddressSpaceError::AlreadyMapped
        | paging::AddressSpaceError::OutOfFrames => LINUX_ENOMEM,
    }
}

fn linux_mmap_page_flags(prot: u64) -> PageTableFlags {
    let mut flags = PageTableFlags::empty();
    if prot & linux::PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if prot & linux::PROT_EXEC == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value.saturating_add(align - 1) & !(align - 1)
}

pub(crate) fn console_write_from_user(
    user_ptr: u64,
    len: usize,
) -> Result<usize, paging::AddressSpaceError> {
    let Some(address_space) = multitask::current_user_address_space() else {
        return Err(paging::AddressSpaceError::NotMapped);
    };

    address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), len)?;

    let mut copied = 0usize;
    let mut total_written = 0usize;
    let mut chunk = [0_u8; CONSOLE_IO_CHUNK_LEN];
    let session = multitask::current_console_session();

    while copied < len {
        let chunk_len = min(len - copied, chunk.len());
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        address_space.copy_from_user(VirtAddr::new(chunk_ptr), &mut chunk[..chunk_len])?;
        total_written += tty::write_to_session(session, &chunk[..chunk_len]);
        copied += chunk_len;
    }

    Ok(total_written)
}

pub(crate) fn console_read_into_user(
    user_ptr: u64,
    len: usize,
) -> Result<usize, paging::AddressSpaceError> {
    let Some(address_space) = multitask::current_user_address_space() else {
        return Err(paging::AddressSpaceError::NotMapped);
    };

    address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), len)?;

    let mut total_read = 0usize;
    let mut chunk = [0_u8; CONSOLE_IO_CHUNK_LEN];
    let session = multitask::current_console_session();

    while total_read < len {
        let chunk_len = min(len - total_read, chunk.len());
        let read = if total_read == 0 {
            tty::read_input_blocking_for_session(session, &mut chunk[..chunk_len])
        } else {
            tty::read_input_for_session(session, &mut chunk[..chunk_len])
        };
        if read == 0 {
            break;
        }

        let chunk_ptr = user_ptr
            .checked_add(total_read as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        address_space.copy_into_user(VirtAddr::new(chunk_ptr), &chunk[..read])?;
        total_read += read;
    }

    Ok(total_read)
}

pub(crate) fn copy_from_user_exact(
    user_ptr: u64,
    dest: &mut [u8],
) -> Result<(), paging::AddressSpaceError> {
    let Some(address_space) = multitask::current_user_address_space() else {
        return Err(paging::AddressSpaceError::NotMapped);
    };

    address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), dest.len())?;
    address_space.copy_from_user(VirtAddr::new(user_ptr), dest)
}

pub(crate) fn write_user_bytes(
    user_ptr: u64,
    bytes: &[u8],
) -> Result<(), paging::AddressSpaceError> {
    let Some(address_space) = multitask::current_user_address_space() else {
        return Err(paging::AddressSpaceError::NotMapped);
    };

    address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), bytes.len())?;
    address_space.copy_into_user(VirtAddr::new(user_ptr), bytes)
}

pub(crate) fn write_user_u32(user_ptr: u64, value: u32) -> Result<(), paging::AddressSpaceError> {
    write_user_bytes(user_ptr, &value.to_le_bytes())
}

pub(crate) fn sleep_ms(milliseconds: u64) {
    rtc::sleep(milliseconds);
}
