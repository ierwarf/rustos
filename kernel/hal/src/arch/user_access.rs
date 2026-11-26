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
    endbr64
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
    endbr64
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
    endbr64
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
    // Vector 14 (#PF, running on IST2 so faults on uncommitted/guard stack
    // pages never escalate to #DF while vectoring).
    // 1) Kernel-mode direct user-copy faults resume at their fixups immediately.
    // 2) Kernel-mode demand-committed stack faults commit on IST2 and `iretq`.
    // 3) User faults and all other kernel faults pivot off IST2 onto `TSS.RSP0`
    //    or `(old_rsp & -16) - 48` before entering `{original}` so IST2 stays
    //    free across blocking user page faults.
    .global rustos_page_fault_entry
    .type rustos_page_fault_entry, @function
rustos_page_fault_entry:
    endbr64
    test byte ptr [rsp + 16], 3
    jnz 20f
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
8:  push rdi
    push r8
    push r9
    push r10
    push r11
    sub rsp, 8
    mov rdi, [rsp + 80]
    mov rsi, cr2
    mov rdx, [rsp + 88]
    mov rcx, [rsp + 112]
    call {try_kernel_pf}
    test al, al
    jz 21f
    add rsp, 8
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    add rsp, 8
    iretq
21: mov rax, [rsp + 112]
    jmp 22f
20: push rax
    push rcx
    push rdx
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    sub rsp, 8
    call {priv_stack_top}
22: mov rcx, rax
    and rcx, -16
    sub rcx, 48
    mov rdx, [rsp + 80]
    mov [rcx + 0], rdx
    mov rdx, [rsp + 88]
    mov [rcx + 8], rdx
    mov rdx, [rsp + 96]
    mov [rcx + 16], rdx
    mov rdx, [rsp + 104]
    mov [rcx + 24], rdx
    mov rdx, [rsp + 112]
    mov [rcx + 32], rdx
    mov rdx, [rsp + 120]
    mov [rcx + 40], rdx
    mov [rsp + 80], rcx
    add rsp, 8
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    mov rsp, [rsp]
    jmp qword ptr [rip + {original}]
    .size rustos_page_fault_entry, . - rustos_page_fault_entry
"#,
    original = sym PF_ORIGINAL,
    try_kernel_pf = sym rustos_try_kernel_stack_page_fault,
    priv_stack_top = sym rustos_hal_pf_privilege_stack_top,
);

/// Handles kernel-stack demand commit or guard diagnostics directly on IST2.
/// Returns `1` if the page fault was resolved by demand commit (resume via `iretq`),
/// or `0` after ensuring the target pivot frame on `old_rsp` is backed.
#[cfg(rustos_boot_image)]
extern "C" fn rustos_try_kernel_stack_page_fault(
    error_code: u64,
    cr2: u64,
    rip: u64,
    old_rsp: u64,
) -> u64 {
    if crate::hooks::try_handle_kernel_page_fault(Some(error_code), cr2, rip, old_rsp) {
        return 1;
    }
    let target_low = (old_rsp & !0xf).wrapping_sub(48);
    let _ = crate::hooks::try_handle_kernel_page_fault(None, target_low, rip, old_rsp);
    0
}

#[cfg(rustos_boot_image)]
extern "C" fn rustos_hal_pf_privilege_stack_top() -> u64 {
    crate::arch::gdt::privilege_stack_top_for_current_cpu()
}

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
    rustos_page_fault_entry as *const () as usize as u64
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
/// Defense in depth behind the caller's own admission: a direct copy runs
/// with supervisor rights, so a range that is not wholly inside the canonical
/// user half (or wraps) must never reach `rep movsb`.
#[cfg(any(rustos_boot_image, test))]
#[inline]
const fn user_range_is_canonical(addr: u64, len: usize) -> bool {
    const USER_HALF_END: u64 = 1 << 47;
    match addr.checked_add(len as u64) {
        Some(end) => end <= USER_HALF_END,
        None => false,
    }
}

/// Element types a user read may fill: initialized bytes, or storage that is
/// still uninitialized (a freshly reserved buffer that will be wholly
/// overwritten). The copy only ever stores initialized bytes.
pub trait UserReadByte: sealed::Sealed {}
impl UserReadByte for u8 {}
impl UserReadByte for core::mem::MaybeUninit<u8> {}

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for core::mem::MaybeUninit<u8> {}
}

/// Copies `dst.len()` bytes from user address `src`. On `true` every byte of
/// `dst` is initialized. `false` means a fault or a disabled direct path; `dst`
/// may be partly written, and for `MaybeUninit` storage none of it may be
/// assumed initialized.
#[cfg(rustos_boot_image)]
#[inline]
pub fn copy_from_user<T: UserReadByte>(dst: &mut [T], src: u64) -> bool {
    if !user_range_is_canonical(src, dst.len()) {
        return false;
    }
    // SAFETY: `T` is `u8` or `MaybeUninit<u8>`, both one byte; `dst` is a live
    // kernel buffer; a fault in the read is recovered by the fixup table and
    // reported as failure. The routine only stores bytes.
    unsafe { rustos_user_copy_from(dst.as_mut_ptr().cast::<u8>(), src as *const u8, dst.len()) == 0 }
}

#[cfg(not(rustos_boot_image))]
#[inline]
pub fn copy_from_user<T: UserReadByte>(_dst: &mut [T], _src: u64) -> bool {
    false
}

/// Copies `src` to user address `dst`. `false` means a fault; the user range may
/// be partly written (the caller rewrites it on the validated path).
#[cfg(rustos_boot_image)]
#[inline]
pub fn copy_to_user(dst: u64, src: &[u8]) -> bool {
    if !user_range_is_canonical(dst, src.len()) {
        return false;
    }
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
    if !user_range_is_canonical(ptr, len) {
        return false;
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

#[cfg(test)]
mod tests {
    use super::user_range_is_canonical;

    #[test]
    fn direct_copy_ranges_must_stay_in_the_canonical_user_half() {
        assert!(user_range_is_canonical(0x1000, 0x1000));
        assert!(user_range_is_canonical((1 << 47) - 1, 1));
        assert!(!user_range_is_canonical((1 << 47) - 1, 2));
        assert!(!user_range_is_canonical(0xffff_8000_0000_0000, 1));
        assert!(!user_range_is_canonical(u64::MAX, 2));
    }
}
