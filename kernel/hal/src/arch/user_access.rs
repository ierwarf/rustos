//! Fault-recoverable direct access to the running task's own user memory.
//!
//! - **Owner:** `kernel-hal` owns the copy primitives, their fixup table and the
//!   page-fault entry that consults it; `kernel-ps` decides when a direct copy
//!   is admissible and owns the validated fallback.
//! - **Boundary:** Every pointer and length is untrusted. Callers pass ranges
//!   already confined to the canonical user half.
//! - **Lifecycle:** A direct copy runs against the live CR3, which during a
//!   syscall is the calling task's own address space. A fault inside one of the
//!   three routines below resumes at that routine's fixup, which returns failure
//!   with no other effect than the bytes already moved.
//! - **Concurrency:** No lock is held. An unmap racing the copy either completes
//!   first (the copy faults and fails) or is ordered after a TLB shootdown that
//!   waits for this CPU, exactly as for any kernel-mode access.
//! - **Failure:** Any fault reports failure; the caller re-runs the operation on
//!   the validated software-walk path, which also resolves COW and returns the
//!   precise errno. A fault outside the table keeps the ordinary handler.
//! - **Forbidden:** Direct access without CR0.WP (a kernel write would bypass
//!   copy-on-write); use for foreign address spaces; any caller that needs
//!   write permission proven without a write (the probe is an atomic no-op RMW).
//! - **Evidence:** `user-memory-access`; guest `kvm-smoke` exercising IPC and
//!   COW copyout.

#[cfg(rustos_boot_image)]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(rustos_boot_image)]
core::arch::global_asm!(
    r#"
    .text
    .global rustos_user_copy_from
    .type rustos_user_copy_from, @function
rustos_user_copy_from:
    mov rcx, rdx
1:  rep movsb
    xor eax, eax
    ret
2:  mov eax, 1
    ret
    .size rustos_user_copy_from, . - rustos_user_copy_from

    .global rustos_user_copy_to
    .type rustos_user_copy_to, @function
rustos_user_copy_to:
    mov rcx, rdx
3:  rep movsb
    xor eax, eax
    ret
4:  mov eax, 1
    ret
    .size rustos_user_copy_to, . - rustos_user_copy_to

    // An atomic read-modify-write that leaves the byte unchanged: a locked
    // compare-exchange of zero against zero stores zero when the byte is zero
    // and otherwise stores back the value it just read. It faults on a page that
    // is not present or not writable, and loses no concurrent user update.
    .global rustos_user_probe_write
    .type rustos_user_probe_write, @function
rustos_user_probe_write:
    xor eax, eax
    xor ecx, ecx
5:  lock cmpxchg byte ptr [rdi], cl
    xor eax, eax
    ret
6:  mov eax, 1
    ret
    .size rustos_user_probe_write, . - rustos_user_probe_write

    .section .rodata.rustos_user_extable, "a"
    .balign 4
    .global RUSTOS_USER_EXTABLE
RUSTOS_USER_EXTABLE:
    .long 1b - ., 2b - .
    .long 3b - ., 4b - .
    .long 5b - ., 6b - .
    .global RUSTOS_USER_EXTABLE_END
RUSTOS_USER_EXTABLE_END:

    .text
    // Vector 14. Kernel-mode faults whose RIP is a registered copy instruction
    // resume at its fixup; every other fault enters the original handler with
    // all registers intact.
    .global rustos_page_fault_entry
    .type rustos_page_fault_entry, @function
rustos_page_fault_entry:
    test byte ptr [rsp + 16], 3
    jnz 9f
    push rax
    push rcx
    push rdx
    push rsi
    mov rax, [rsp + 40]
    lea rcx, [rip + RUSTOS_USER_EXTABLE]
    lea rdx, [rip + RUSTOS_USER_EXTABLE_END]
7:  cmp rcx, rdx
    jae 8f
    movsxd rsi, dword ptr [rcx]
    add rsi, rcx
    cmp rsi, rax
    je 10f
    add rcx, 8
    jmp 7b
10: movsxd rsi, dword ptr [rcx + 4]
    lea rsi, [rsi + rcx + 4]
    mov [rsp + 40], rsi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    add rsp, 8
    iretq
8:  pop rsi
    pop rdx
    pop rcx
    pop rax
9:  jmp qword ptr [rip + {original}]
    .size rustos_page_fault_entry, . - rustos_page_fault_entry
"#,
    original = sym PF_ORIGINAL,
);

/// The handler installed for vector 14 before the recovery entry replaced it.
/// Written once while the IDT is built, before any CPU can take the fault.
#[cfg(rustos_boot_image)]
static PF_ORIGINAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[cfg(rustos_boot_image)]
unsafe extern "C" {
    fn rustos_user_copy_from(dst: *mut u8, src: *const u8, len: usize) -> usize;
    fn rustos_user_copy_to(dst: *mut u8, src: *const u8, len: usize) -> usize;
    fn rustos_user_probe_write(ptr: *mut u8) -> usize;
    fn rustos_page_fault_entry();
}

/// Set once CR0.WP is proven on this CPU class; direct writes are refused
/// before then because a supervisor write would ignore a read-only COW leaf.
#[cfg(rustos_boot_image)]
static DIRECT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Address of the fault-recovery entry and the registration of the handler it
/// forwards to. Called while the IDT is built, before it is loaded.
#[cfg(rustos_boot_image)]
pub(crate) fn page_fault_entry_addr(original: u64) -> u64 {
    // ORDERING: Release; the IDT is loaded after this store on every CPU.
    PF_ORIGINAL.store(original, Ordering::Release);
    rustos_page_fault_entry as usize as u64
}

/// Enable direct user access on every CPU that has CR0.WP set. The BSP calls
/// this after its CR0 is final; APs set WP themselves before they run tasks.
#[cfg(rustos_boot_image)]
pub fn enable_if_write_protected() {
    use x86_64::registers::control::{Cr0, Cr0Flags};
    if Cr0::read().contains(Cr0Flags::WRITE_PROTECT) {
        DIRECT_ENABLED.store(true, Ordering::Release);
    }
}

/// Make the CPU honour read-only pages for supervisor writes (idempotent).
#[cfg(rustos_boot_image)]
pub fn ensure_write_protect_on_this_cpu() {
    use x86_64::registers::control::{Cr0, Cr0Flags};
    // SAFETY: setting CR0.WP only tightens supervisor write checks.
    unsafe { Cr0::update(|flags| flags.insert(Cr0Flags::WRITE_PROTECT)) };
}

#[cfg(rustos_boot_image)]
#[inline]
pub fn enabled() -> bool {
    DIRECT_ENABLED.load(Ordering::Relaxed)
}

#[cfg(not(rustos_boot_image))]
#[inline]
pub fn enabled() -> bool {
    false
}

/// Copies `dst.len()` bytes from user address `src`. `false` means a fault or a
/// disabled direct path; `dst` may be partly written.
#[cfg(rustos_boot_image)]
#[inline]
pub fn copy_from_user(dst: &mut [u8], src: u64) -> bool {
    // SAFETY: `dst` is a live kernel buffer; a fault in the read is recovered by
    // the fixup table and reported as failure.
    unsafe { rustos_user_copy_from(dst.as_mut_ptr(), src as *const u8, dst.len()) == 0 }
}

#[cfg(not(rustos_boot_image))]
#[inline]
pub fn copy_from_user(_dst: &mut [u8], _src: u64) -> bool {
    false
}

/// Copies `src` to user address `dst`. `false` means a fault; the user range may
/// be partly written (the caller rewrites it on the validated path).
#[cfg(rustos_boot_image)]
#[inline]
pub fn copy_to_user(dst: u64, src: &[u8]) -> bool {
    // SAFETY: `src` is a live kernel buffer; a fault in the write is recovered.
    unsafe { rustos_user_copy_to(dst as *mut u8, src.as_ptr(), src.len()) == 0 }
}

#[cfg(not(rustos_boot_image))]
#[inline]
pub fn copy_to_user(_dst: u64, _src: &[u8]) -> bool {
    false
}

/// Proves every page of `[ptr, ptr + len)` writable without changing it.
#[cfg(rustos_boot_image)]
#[inline]
pub fn probe_write(ptr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let last = ptr + len as u64 - 1;
    let mut page = ptr;
    loop {
        // SAFETY: the routine only issues one atomic RMW on that byte; a fault
        // is recovered and reported.
        if unsafe { rustos_user_probe_write(page as *mut u8) } != 0 {
            return false;
        }
        let next = (page | 0xfff) + 1;
        if next > last {
            return true;
        }
        page = next;
    }
}

#[cfg(not(rustos_boot_image))]
#[inline]
pub fn probe_write(_ptr: u64, _len: usize) -> bool {
    false
}
