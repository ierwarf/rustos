use core::arch::asm;

#[inline]
pub fn current_rip() -> u64 {
    let rip: u64;
    unsafe {
        asm!(
            "lea {rip}, [rip + 0]",
            rip = out(reg) rip,
            options(nomem, nostack, preserves_flags),
        );
    }
    rip
}

pub unsafe fn enter_higher_half(entry: u64, boot_info_ptr: u64) -> ! {
    unsafe {
        // Pin inputs to fixed registers so the compiler cannot alias the jump
        // target with the ABI argument register during debug builds.
        asm!(
            "add rsp, rcx",
            "test rsp, 8",
            "jnz 2f",
            "sub rsp, 8",
            "2:",
            "jmp rax",
            in("rax") entry,
            in("rdi") boot_info_ptr,
            in("rcx") crate::lowlevel::address::KERNEL_VIRT_OFFSET,
            options(noreturn),
        );
    }
}

pub unsafe fn call_with_stack(entry: u64, arg0: u64, stack_top: u64) -> ! {
    unsafe {
        asm!(
            "mov rsp, rdx",
            "and rsp, -16",
            "call rax",
            in("rax") entry,
            in("rdi") arg0,
            in("rdx") stack_top,
            options(noreturn),
        );
    }
}
