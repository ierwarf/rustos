use core::arch::global_asm;
use core::cmp::min;
use core::convert::TryFrom;

use x86_64::VirtAddr;
use x86_64::registers::control::{Efer, EferFlags};
use x86_64::registers::model_specific::{KernelGsBase, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use crate::{debug, gdt, multitask, paging, rtc, tty};

const SYSCALL_CONSOLE_WRITE: u64 = 1;
const SYSCALL_CONSOLE_READ: u64 = 2;
const SYSCALL_CONSOLE_POLL_INPUT: u64 = 3;
const SYSCALL_SLEEP_MS: u64 = 4;
const SYSCALL_STACK_SIZE: usize = 16 * 1024;
const MAX_CONSOLE_IO_LEN: usize = 256;
const CONSOLE_IO_CHUNK_LEN: usize = 256;
const SYSCALL_ERR_INVALID: u64 = u64::MAX;
const SYSCALL_ERR_FAULT: u64 = u64::MAX - 1;

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

const _: [(); 128] = [(); core::mem::size_of::<SyscallFrame>()];

static mut SYSCALL_CPU_LOCAL: SyscallCpuLocal = SyscallCpuLocal {
    kernel_stack_top: 0,
    user_rsp: 0,
};
static mut SYSCALL_FALLBACK_STACK: SyscallFallbackStack = SyscallFallbackStack([0; SYSCALL_STACK_SIZE]);

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
    let (cpu_local_addr, stack_base) = unsafe {
        (
            paging::higher_half_addr(core::ptr::addr_of!(SYSCALL_CPU_LOCAL) as u64),
            paging::higher_half_addr(core::ptr::addr_of!(SYSCALL_FALLBACK_STACK.0) as u64),
        )
    };

    unsafe {
        SYSCALL_CPU_LOCAL.kernel_stack_top = stack_base + SYSCALL_STACK_SIZE as u64;
    }

    let entry_addr =
        paging::higher_half_addr(syscall_entry as *const () as usize as u64);
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
    KernelGsBase::write(VirtAddr::new(cpu_local_addr));
}

pub fn set_kernel_stack_top(kernel_stack_top: u64) {
    if kernel_stack_top == 0 {
        return;
    }

    unsafe {
        SYSCALL_CPU_LOCAL.kernel_stack_top = kernel_stack_top;
    }
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
    if let Some(result) = crate::win32::dispatch_syscall(
        frame.rax,
        frame.rdi,
        frame.rsi,
        frame.rdx,
        frame.r8,
        frame.r9,
        frame.r10,
    ) {
        return result;
    }

    match frame.rax {
        SYSCALL_CONSOLE_WRITE => syscall_console_write(frame.rdi, frame.rsi),
        SYSCALL_CONSOLE_READ => syscall_console_read(frame.rdi, frame.rsi),
        SYSCALL_CONSOLE_POLL_INPUT => tty::pending_input_len() as u64,
        SYSCALL_SLEEP_MS => syscall_sleep_ms(frame.rdi),
        _ => SYSCALL_ERR_INVALID,
    }
}

fn syscall_console_write(user_ptr: u64, user_len: u64) -> u64 {
    let Ok(len) = usize::try_from(user_len) else {
        return SYSCALL_ERR_INVALID;
    };
    if len > MAX_CONSOLE_IO_LEN {
        return SYSCALL_ERR_INVALID;
    }
    if len == 0 {
        return 0;
    }

    if let Err(err) = console_write_from_user(user_ptr, len) {
        debug::println!(
            "syscall console_write fault: user_ptr={:#x} len={} err={:?}",
            user_ptr,
            len,
            err,
        );
        return SYSCALL_ERR_FAULT;
    }

    len as u64
}

fn syscall_console_read(user_ptr: u64, user_len: u64) -> u64 {
    let Ok(len) = usize::try_from(user_len) else {
        return SYSCALL_ERR_INVALID;
    };
    if len > MAX_CONSOLE_IO_LEN {
        return SYSCALL_ERR_INVALID;
    }
    if len == 0 {
        return 0;
    }

    match console_read_into_user(user_ptr, len) {
        Ok(read) => read as u64,
        Err(err) => {
        debug::println!(
            "syscall console_read fault: user_ptr={:#x} len={} err={:?}",
            user_ptr,
                len,
            err,
        );
        return SYSCALL_ERR_FAULT;
        }
    }
}

fn syscall_sleep_ms(milliseconds: u64) -> u64 {
    sleep_ms(milliseconds);
    0
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

    while copied < len {
        let chunk_len = min(len - copied, chunk.len());
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        address_space.copy_from_user(VirtAddr::new(chunk_ptr), &mut chunk[..chunk_len])?;
        total_written += tty::write(&chunk[..chunk_len]);
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

    while total_read < len {
        let chunk_len = min(len - total_read, chunk.len());
        let read = if total_read == 0 {
            tty::read_input_blocking(&mut chunk[..chunk_len])
        } else {
            tty::read_input(&mut chunk[..chunk_len])
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

pub(crate) fn write_user_u32(
    user_ptr: u64,
    value: u32,
) -> Result<(), paging::AddressSpaceError> {
    let Some(address_space) = multitask::current_user_address_space() else {
        return Err(paging::AddressSpaceError::NotMapped);
    };

    address_space.copy_into_user(VirtAddr::new(user_ptr), &value.to_le_bytes())
}

pub(crate) fn sleep_ms(milliseconds: u64) {
    rtc::sleep(milliseconds);
}
