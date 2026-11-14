//! Extra worker thread for a single-receive-loop service.
//!
//! The caller owns the stack (a `static` it never reuses) and the entry never
//! returns. One `clone` attempt per call: before procd plans thread creation
//! the kernel answers ENOSYS and the caller retries from its own loop. The
//! thread shares the address space and ends with the process.

#[cfg(all(target_arch = "x86_64", not(test)))]
pub unsafe fn spawn(stack_top: u64, entry: extern "C" fn() -> !) -> Result<(), i64> {
    use core::arch::asm;
    const SYS_CLONE: u64 = 56;
    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_FS: u64 = 0x0000_0200;
    const CLONE_FILES: u64 = 0x0000_0400;
    const CLONE_SIGHAND: u64 = 0x0000_0800;
    const CLONE_THREAD: u64 = 0x0001_0000;
    if stack_top == 0 || stack_top % 16 != 0 {
        return Err(22);
    }
    let flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
    let result: i64;
    // SAFETY: the child continues on the caller-provided stack and only calls
    // `entry`, which never returns; the parent resumes with the clone result.
    unsafe {
        asm!(
            "syscall",
            "test rax, rax",
            "jnz 2f",
            "call r12",
            "ud2",
            "2:",
            inlateout("rax") SYS_CLONE as i64 => result,
            in("rdi") flags,
            in("rsi") stack_top,
            in("rdx") 0_u64,
            in("r10") 0_u64,
            in("r8") 0_u64,
            // The entry pointer must survive `syscall`, which clobbers rcx and
            // r11 (and the kernel seeds the child with rcx = user rip). A
            // generic `in(reg)` can be allocated to either, which made the
            // child call its own post-syscall instruction until the stack
            // ran off the text segment. r12 is preserved across clone.
            in("r12") entry,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if result > 0 {
        Ok(())
    } else if result < 0 {
        Err(-result)
    } else {
        Err(11)
    }
}
