//! Read-modify-write of a counter written only by its owning CPU.
//!
//! - **Owner:** the CPU whose index selects the counter; no other CPU writes
//!   it, and no other CPU reads it to make a decision.
//! - **Boundary:** a single non-`lock` `xadd`. One instruction cannot be split
//!   by an interrupt on the same CPU, which is the only other writer, so it is
//!   exactly as atomic as the `lock` form for this ownership while skipping its
//!   full fence and cache-line lock.
//! - **Failure:** using this on a counter another CPU writes loses updates;
//!   every caller indexes by the CPU it is running on, with preemption or
//!   interrupts already preventing migration. Until the CPU map is published
//!   all CPUs share index zero, so the `lock` form is kept until then.
//! - **Evidence:** guest `perf` of the eight-vCPU IPC benchmark: the `lock`
//!   prefixed depth updates on every lock acquire/release were the hottest
//!   instructions in the kernel.

use core::sync::atomic::AtomicU32;
#[cfg(rustos_boot_image)]
use core::sync::atomic::AtomicUsize;

/// Before the dense CPU map is published every CPU derives logical index
/// zero, so one counter row is genuinely shared during AP bring-up; only after
/// publication does "indexed by the running CPU" mean single-writer.
#[cfg(rustos_boot_image)]
#[inline(always)]
fn indices_exact() -> bool {
    // ORDERING: Acquire pairs with topology publication; it is set once and
    // never cleared, so a stale `false` only selects the `lock` form.
    super::CPU_IDENTITIES_PUBLISHED.load(core::sync::atomic::Ordering::Acquire)
}

/// Adds `delta` (wrapping) and returns the previous value.
#[cfg(rustos_boot_image)]
#[inline(always)]
pub(super) fn add_usize(counter: &AtomicUsize, delta: usize) -> usize {
    if !indices_exact() {
        return counter.fetch_add(delta, core::sync::atomic::Ordering::AcqRel);
    }
    let mut value = delta;
    // SAFETY: `counter` is a live, aligned atomic; the owning CPU is its only
    // writer and one instruction is indivisible against its own interrupts.
    // No `nomem`: the asm is a compiler barrier like the atomic it replaces.
    unsafe {
        core::arch::asm!(
            "xadd qword ptr [{ptr}], {value}",
            ptr = in(reg) counter.as_ptr(),
            value = inout(reg) value,
            options(nostack),
        );
    }
    value
}

/// `add_usize` for a 32-bit counter.
#[cfg(rustos_boot_image)]
#[inline(always)]
pub(super) fn add_u32(counter: &AtomicU32, delta: u32) -> u32 {
    if !indices_exact() {
        return counter.fetch_add(delta, core::sync::atomic::Ordering::AcqRel);
    }
    let mut value = delta;
    // SAFETY: as in `add_usize`.
    unsafe {
        core::arch::asm!(
            "xadd dword ptr [{ptr}], {value:e}",
            ptr = in(reg) counter.as_ptr(),
            value = inout(reg) value,
            options(nostack),
        );
    }
    value
}

/// Native models run several host threads as one modeled CPU, so the counter
/// is genuinely shared there and keeps the `lock` form. (The `usize` counters
/// it would cover are all boot-image-only.)
#[cfg(not(rustos_boot_image))]
pub(super) fn add_u32(counter: &AtomicU32, delta: u32) -> u32 {
    counter.fetch_add(delta, core::sync::atomic::Ordering::AcqRel)
}

/// Decrements (wrapping) and returns the previous value.
#[cfg(rustos_boot_image)]
#[inline(always)]
pub(super) fn dec(counter: &AtomicUsize) -> usize {
    add_usize(counter, usize::MAX)
}
