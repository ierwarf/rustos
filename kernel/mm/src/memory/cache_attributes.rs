//! Per-CPU x86 memory-type baseline capture and AP restoration.
//!
//! - **Owner:** `kernel-mm` owns the BSP-derived MTRR/PAT/cache contract used
//!   by every logical CPU that shares the kernel page tables.
//! - **Boundary:** The BSP captures one immutable architectural baseline before
//!   any SIPI; an AP restores and exactly reads it back before private-ready.
//!   The CPU's reported physical-address width published here is the bound
//!   every untrusted device-derived range is admitted against.
//! - **Lifecycle:** Empty -> Capturing -> Ready is one-way. A rejected capture
//!   is terminal, and AP restoration never mutates the sealed baseline.
//! - **Concurrency:** Atomic Release publication makes all immutable MSR words
//!   visible to any number of Acquire readers without a boot-time global lock.
//! - **Failure:** Unsupported features, malformed MTRR capacity, cache-enabled
//!   AP entry, capability drift, or any readback mismatch fails closed.
//! - **Forbidden:** No AP dispatch, shared-WB access, retry, or partial baseline
//!   publication before exact MTRR, PAT, CR0, and TLB/cache sequencing.
//! - **Evidence:** `cpu-online-lifecycle`.

use core::arch::{asm, x86_64::__cpuid};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use x86_64::registers::model_specific::Msr;

const IA32_MTRR_CAP_MSR: u32 = 0x0fe;
const IA32_MTRR_PHYSBASE0_MSR: u32 = 0x200;
const IA32_MTRR_DEF_TYPE_MSR: u32 = 0x2ff;
const IA32_PAT_MSR: u32 = 0x277;

const MTRR_CAP_VARIABLE_COUNT_MASK: u64 = 0xff;
const MTRR_CAP_FIXED_SUPPORTED: u64 = 1 << 8;
const MTRR_DEF_FIXED_ENABLE: u64 = 1 << 10;
const MTRR_DEF_ENABLE: u64 = 1 << 11;
const MTRR_DEF_ENABLE_MASK: u64 = MTRR_DEF_FIXED_ENABLE | MTRR_DEF_ENABLE;
const MAX_VARIABLE_MTRRS: usize = 255;
const FIXED_MTRR_MSRS: [u32; 11] = [
    0x250, 0x258, 0x259, 0x268, 0x269, 0x26a, 0x26b, 0x26c, 0x26d, 0x26e, 0x26f,
];

const CR0_NOT_WRITE_THROUGH: u64 = 1 << 29;
const CR0_CACHE_DISABLE: u64 = 1 << 30;
const CR0_CACHE_CONTROL_MASK: u64 = CR0_NOT_WRITE_THROUGH | CR0_CACHE_DISABLE;
const CR4_PAGE_GLOBAL_ENABLE: u64 = 1 << 7;

const CPUID_FEATURE_MTRR: u32 = 1 << 12;
const CPUID_FEATURE_PAT: u32 = 1 << 16;

const PAT_SLOT0_SHIFT: u32 = 0;
const PAT_SLOT2_SHIFT: u32 = 16;
const PAT_SLOT4_SHIFT: u32 = 32;
const PAT_ENTRY_MASK: u64 = 0xff;
const PAT_WRITE_BACK: u64 = 0x06;
const PAT_UNCACHEABLE: u64 = 0x00;
const PAT_WRITE_COMBINING: u64 = 0x01;
const PAT_KERNEL_CACHE_SLOT_MASK: u64 = (PAT_ENTRY_MASK << PAT_SLOT0_SHIFT)
    | (PAT_ENTRY_MASK << PAT_SLOT2_SHIFT)
    | (PAT_ENTRY_MASK << PAT_SLOT4_SHIFT);

const BASELINE_EMPTY: u8 = 0;
const BASELINE_CAPTURING: u8 = 1;
const BASELINE_READY: u8 = 2;
const BASELINE_REJECTED: u8 = 3;

static BASELINE_STATE: AtomicU8 = AtomicU8::new(BASELINE_EMPTY);
static BSP_MTRR_CAP: AtomicU64 = AtomicU64::new(0);
static BSP_MTRR_DEF_TYPE: AtomicU64 = AtomicU64::new(0);
static BSP_PAT: AtomicU64 = AtomicU64::new(0);
static BSP_FIXED_MTRRS: [AtomicU64; FIXED_MTRR_MSRS.len()] =
    [const { AtomicU64::new(0) }; FIXED_MTRR_MSRS.len()];
static BSP_VARIABLE_MTRR_BASES: [AtomicU64; MAX_VARIABLE_MTRRS] =
    [const { AtomicU64::new(0) }; MAX_VARIABLE_MTRRS];
static BSP_VARIABLE_MTRR_MASKS: [AtomicU64; MAX_VARIABLE_MTRRS] =
    [const { AtomicU64::new(0) }; MAX_VARIABLE_MTRRS];

const fn pat_with_kernel_cache_contract(pat: u64) -> u64 {
    (pat & !PAT_KERNEL_CACHE_SLOT_MASK)
        | (PAT_WRITE_BACK << PAT_SLOT0_SHIFT)
        | (PAT_UNCACHEABLE << PAT_SLOT2_SHIFT)
        | (PAT_WRITE_COMBINING << PAT_SLOT4_SHIFT)
}

const fn pat_entry(pat: u64, shift: u32) -> u64 {
    (pat >> shift) & PAT_ENTRY_MASK
}

const fn pat_kernel_cache_contract_is_exact(expected: u64, observed: u64) -> bool {
    observed == expected
        && pat_entry(observed, PAT_SLOT0_SHIFT) == PAT_WRITE_BACK
        && pat_entry(observed, PAT_SLOT2_SHIFT) == PAT_UNCACHEABLE
        && pat_entry(observed, PAT_SLOT4_SHIFT) == PAT_WRITE_COMBINING
}

const fn pat_initial_write_back_selector_is_admissible(pat: u64) -> bool {
    pat_entry(pat, PAT_SLOT0_SHIFT) == PAT_WRITE_BACK
}

const fn cpu_memory_type_features_are_admissible(cpuid1_edx: u32) -> bool {
    cpuid1_edx & (CPUID_FEATURE_MTRR | CPUID_FEATURE_PAT) == CPUID_FEATURE_MTRR | CPUID_FEATURE_PAT
}

/// Highest physical address this CPU can address, exclusive.
///
/// A PCI BAR, an ivshmem aperture, and every other device-derived range is
/// untrusted input: the value may be a probe mask a peer never restored, or a
/// field a driver domain populated wrongly. Constructing a page-table entry
/// from it must fail closed, so callers bound the range against the CPU's
/// reported width. The architectural page-table limit is 52 bits and CPUID
/// leaf 0x8000_0008 reports no more than that on any supported part; a
/// missing leaf falls back to the architectural minimum rather than trusting
/// the address.
pub(super) fn max_physical_address() -> u64 {
    // ORDERING: Relaxed. An idempotent cache of an immutable CPUID answer;
    // a concurrent recompute produces the identical value.
    let cached = MAX_PHYSICAL_ADDRESS.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let width = if __cpuid(0x8000_0000).eax >= 0x8000_0008 {
        (__cpuid(0x8000_0008).eax & 0xff) as u64
    } else {
        0
    };
    let width = width.clamp(
        MINIMUM_PHYSICAL_ADDRESS_BITS,
        ARCHITECTURAL_PHYSICAL_ADDRESS_BITS,
    );
    let limit = 1_u64 << width;
    MAX_PHYSICAL_ADDRESS.store(limit, Ordering::Relaxed);
    limit
}

/// One past the last physical address a 4-level page-table entry can encode.
const ARCHITECTURAL_PHYSICAL_ADDRESS_BITS: u64 = 52;
/// Long mode requires at least this much; a CPU reporting less is reporting
/// nonsense and must not be allowed to shrink the admitted range.
const MINIMUM_PHYSICAL_ADDRESS_BITS: u64 = 36;
static MAX_PHYSICAL_ADDRESS: AtomicU64 = AtomicU64::new(0);

const fn mtrr_variable_count(cap: u64) -> usize {
    (cap & MTRR_CAP_VARIABLE_COUNT_MASK) as usize
}

const fn mtrr_capacity_is_admissible(cap: u64) -> bool {
    mtrr_variable_count(cap) <= MAX_VARIABLE_MTRRS
}

const fn cache_is_enabled(cr0: u64) -> bool {
    cr0 & CR0_CACHE_CONTROL_MASK == 0
}

const fn no_fill_cache_state(cr0: u64) -> u64 {
    (cr0 | CR0_CACHE_DISABLE) & !CR0_NOT_WRITE_THROUGH
}

fn read_msr(index: u32) -> u64 {
    // SAFETY: every call is gated by CPUID and MTRR capability enumeration.
    unsafe { Msr::new(index).read() }
}

fn write_msr(index: u32, value: u64) {
    // SAFETY: callers own private AP architectural initialization, keep caches
    // in no-fill mode, and only write enumerated MTRR/PAT registers.
    unsafe { Msr::new(index).write(value) }
}

fn read_cr0() -> u64 {
    let value: u64;
    // SAFETY: reading CR0 has no side effect at CPL0.
    unsafe {
        asm!("mov {value}, cr0", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn write_cr0(value: u64) {
    // SAFETY: callers preserve every CR0 bit except the architected CD/NW pair.
    unsafe {
        asm!("mov cr0, {value}", value = in(reg) value, options(nostack, preserves_flags));
    }
}

fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: reading CR4 has no side effect at CPL0.
    unsafe {
        asm!("mov {value}, cr4", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn flush_tlb_without_global_pages() {
    let cr3: u64;
    // SAFETY: AP private initialization has interrupts disabled, PCID and PGE
    // are not admitted, and reloading the active CR3 is the architectural TLB
    // invalidation required around MTRR changes.
    unsafe {
        asm!("mov {cr3}, cr3", cr3 = out(reg) cr3, options(nomem, nostack, preserves_flags));
        asm!("mov cr3, {cr3}", cr3 = in(reg) cr3, options(nostack, preserves_flags));
    }
}

fn writeback_and_invalidate_caches() {
    // SAFETY: WBINVD is executed at CPL0 with interrupts disabled during the
    // boot-private cache transition.
    unsafe {
        asm!("wbinvd", options(nostack, preserves_flags));
    }
}

fn reject_capture() -> bool {
    // ORDERING: Release makes the terminal rejection visible to every AP
    // before it can read any incompletely captured baseline word.
    BASELINE_STATE.store(BASELINE_REJECTED, Ordering::Release);
    false
}

/// Capture and seal the firmware/virtualization memory-type state on the BSP.
///
/// This must run once, with caches enabled, before the first INIT/SIPI.
pub(super) fn capture_boot_cpu_cache_attributes() -> bool {
    // ORDERING: one AcqRel transition owns BSP capture; Acquire observes an
    // already sealed or rejected record without permitting a second writer.
    if BASELINE_STATE
        .compare_exchange(
            BASELINE_EMPTY,
            BASELINE_CAPTURING,
            // ORDERING: AcqRel claims BSP capture; Acquire observes a sealed
            // or rejected record without permitting a second writer.
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return false;
    }

    let features = __cpuid(1).edx;
    if !cpu_memory_type_features_are_admissible(features) || !cache_is_enabled(read_cr0()) {
        return reject_capture();
    }
    let cap = read_msr(IA32_MTRR_CAP_MSR);
    if !mtrr_capacity_is_admissible(cap) {
        return reject_capture();
    }
    let initial_pat = read_msr(IA32_PAT_MSR);
    if !pat_initial_write_back_selector_is_admissible(initial_pat) {
        return reject_capture();
    }
    let expected_pat = pat_with_kernel_cache_contract(initial_pat);
    if initial_pat != expected_pat {
        write_msr(IA32_PAT_MSR, expected_pat);
    }
    if !pat_kernel_cache_contract_is_exact(expected_pat, read_msr(IA32_PAT_MSR)) {
        return reject_capture();
    }

    BSP_MTRR_CAP.store(cap, Ordering::Relaxed);
    BSP_MTRR_DEF_TYPE.store(read_msr(IA32_MTRR_DEF_TYPE_MSR), Ordering::Relaxed);
    BSP_PAT.store(expected_pat, Ordering::Relaxed);
    if cap & MTRR_CAP_FIXED_SUPPORTED != 0 {
        for (slot, msr) in FIXED_MTRR_MSRS.iter().copied().enumerate() {
            BSP_FIXED_MTRRS[slot].store(read_msr(msr), Ordering::Relaxed);
        }
    }
    for slot in 0..mtrr_variable_count(cap) {
        let base_msr = IA32_MTRR_PHYSBASE0_MSR + (slot as u32) * 2;
        BSP_VARIABLE_MTRR_BASES[slot].store(read_msr(base_msr), Ordering::Relaxed);
        BSP_VARIABLE_MTRR_MASKS[slot].store(read_msr(base_msr + 1), Ordering::Relaxed);
    }

    // ORDERING: Release publishes every Relaxed baseline word as one immutable
    // record after PAT readback and before the BSP can send the first SIPI.
    BASELINE_STATE.store(BASELINE_READY, Ordering::Release);
    true
}

fn restore_mtrr_and_pat_baseline(cap: u64) {
    let expected_def_type = BSP_MTRR_DEF_TYPE.load(Ordering::Relaxed);
    write_msr(
        IA32_MTRR_DEF_TYPE_MSR,
        expected_def_type & !MTRR_DEF_ENABLE_MASK,
    );
    if cap & MTRR_CAP_FIXED_SUPPORTED != 0 {
        for (slot, msr) in FIXED_MTRR_MSRS.iter().copied().enumerate() {
            write_msr(msr, BSP_FIXED_MTRRS[slot].load(Ordering::Relaxed));
        }
    }
    for slot in 0..mtrr_variable_count(cap) {
        let base_msr = IA32_MTRR_PHYSBASE0_MSR + (slot as u32) * 2;
        write_msr(
            base_msr,
            BSP_VARIABLE_MTRR_BASES[slot].load(Ordering::Relaxed),
        );
        write_msr(
            base_msr + 1,
            BSP_VARIABLE_MTRR_MASKS[slot].load(Ordering::Relaxed),
        );
    }
    write_msr(IA32_PAT_MSR, BSP_PAT.load(Ordering::Relaxed));
    write_msr(IA32_MTRR_DEF_TYPE_MSR, expected_def_type);
}

fn current_cpu_matches_sealed_baseline(cap: u64) -> bool {
    if read_msr(IA32_MTRR_CAP_MSR) != cap
        || read_msr(IA32_MTRR_DEF_TYPE_MSR) != BSP_MTRR_DEF_TYPE.load(Ordering::Relaxed)
        || !pat_kernel_cache_contract_is_exact(
            BSP_PAT.load(Ordering::Relaxed),
            read_msr(IA32_PAT_MSR),
        )
    {
        return false;
    }
    if cap & MTRR_CAP_FIXED_SUPPORTED != 0 {
        for (slot, msr) in FIXED_MTRR_MSRS.iter().copied().enumerate() {
            if read_msr(msr) != BSP_FIXED_MTRRS[slot].load(Ordering::Relaxed) {
                return false;
            }
        }
    }
    for slot in 0..mtrr_variable_count(cap) {
        let base_msr = IA32_MTRR_PHYSBASE0_MSR + (slot as u32) * 2;
        if read_msr(base_msr) != BSP_VARIABLE_MTRR_BASES[slot].load(Ordering::Relaxed)
            || read_msr(base_msr + 1) != BSP_VARIABLE_MTRR_MASKS[slot].load(Ordering::Relaxed)
        {
            return false;
        }
    }
    true
}

/// Restore the sealed BSP memory-type state and enable caching on one AP.
///
/// The AP is not scheduler-visible, interrupts are disabled, and the
/// trampoline left it in CD=1/NW=0 no-fill mode. The function performs the
/// architectural cache/TLB/MTRR sequence and only returns after exact readback.
pub(super) fn initialize_application_processor_cache_attributes() -> bool {
    // ORDERING: Acquire observes the complete immutable baseline or rejects a
    // partially captured/terminal record before touching any AP MSR.
    if BASELINE_STATE.load(Ordering::Acquire) != BASELINE_READY {
        return false;
    }
    let features = __cpuid(1).edx;
    let cap = read_msr(IA32_MTRR_CAP_MSR);
    if !cpu_memory_type_features_are_admissible(features)
        || cap != BSP_MTRR_CAP.load(Ordering::Relaxed)
        || !mtrr_capacity_is_admissible(cap)
        || read_cr4() & CR4_PAGE_GLOBAL_ENABLE != 0
    {
        return false;
    }

    write_cr0(no_fill_cache_state(read_cr0()));
    writeback_and_invalidate_caches();
    flush_tlb_without_global_pages();
    restore_mtrr_and_pat_baseline(cap);
    flush_tlb_without_global_pages();
    writeback_and_invalidate_caches();
    write_cr0(read_cr0() & !CR0_CACHE_CONTROL_MASK);

    cache_is_enabled(read_cr0()) && current_cpu_matches_sealed_baseline(cap)
}

#[cfg(test)]
fn pat_without_kernel_cache_slots(value: u64) -> u64 {
    value & !PAT_KERNEL_CACHE_SLOT_MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_cache_contract_update_is_exact_idempotent_and_cpu_local() {
        let original = 0x1234_5678_9abc_def0;
        let updated = pat_with_kernel_cache_contract(original);
        assert_eq!(pat_entry(updated, PAT_SLOT0_SHIFT), PAT_WRITE_BACK);
        assert_eq!(pat_entry(updated, PAT_SLOT2_SHIFT), PAT_UNCACHEABLE);
        assert_eq!(pat_entry(updated, PAT_SLOT4_SHIFT), PAT_WRITE_COMBINING);
        assert_eq!(
            pat_without_kernel_cache_slots(updated),
            pat_without_kernel_cache_slots(original)
        );
        assert_eq!(pat_with_kernel_cache_contract(updated), updated);
        assert!(pat_initial_write_back_selector_is_admissible(updated));
        assert!(!pat_initial_write_back_selector_is_admissible(
            updated ^ PAT_ENTRY_MASK
        ));
        assert!(pat_kernel_cache_contract_is_exact(updated, updated));
        assert!(!pat_kernel_cache_contract_is_exact(
            updated,
            updated ^ (PAT_ENTRY_MASK << PAT_SLOT4_SHIFT)
        ));
        assert!(!pat_kernel_cache_contract_is_exact(
            updated,
            updated ^ (PAT_ENTRY_MASK << 8)
        ));
    }

    #[test]
    fn ap_memory_type_admission_requires_features_capacity_and_no_fill_state() {
        assert!(cpu_memory_type_features_are_admissible(
            CPUID_FEATURE_MTRR | CPUID_FEATURE_PAT
        ));
        assert!(!cpu_memory_type_features_are_admissible(CPUID_FEATURE_PAT));
        assert!(mtrr_capacity_is_admissible(8 | MTRR_CAP_FIXED_SUPPORTED));
        assert_eq!(mtrr_variable_count(8 | MTRR_CAP_FIXED_SUPPORTED), 8);

        let reset = CR0_CACHE_DISABLE | CR0_NOT_WRITE_THROUGH | 0x11;
        let no_fill = no_fill_cache_state(reset);
        assert_eq!(no_fill & CR0_CACHE_DISABLE, CR0_CACHE_DISABLE);
        assert_eq!(no_fill & CR0_NOT_WRITE_THROUGH, 0);
        assert!(!cache_is_enabled(no_fill));
        assert!(cache_is_enabled(no_fill & !CR0_CACHE_CONTROL_MASK));
    }

    #[test]
    fn ap_restore_sequence_is_before_cache_enable_and_private_readback() {
        let source = include_str!("cache_attributes.rs");
        let body = source
            .split_once("pub(super) fn initialize_application_processor_cache_attributes()")
            .expect("AP cache initializer must remain source-visible")
            .1
            .split_once("#[cfg(test)]")
            .expect("AP cache initializer must precede tests")
            .0;
        let no_fill = body
            .find("write_cr0(no_fill_cache_state(read_cr0()))")
            .unwrap();
        let first_flush = body.find("writeback_and_invalidate_caches()").unwrap();
        let restore = body.find("restore_mtrr_and_pat_baseline(cap)").unwrap();
        let last_flush = body.rfind("writeback_and_invalidate_caches()").unwrap();
        let cache_enable = body
            .find("write_cr0(read_cr0() & !CR0_CACHE_CONTROL_MASK)")
            .unwrap();
        let readback = body
            .find("current_cpu_matches_sealed_baseline(cap)")
            .unwrap();
        assert!(no_fill < first_flush);
        assert!(first_flush < restore);
        assert!(restore < last_flush);
        assert!(last_flush < cache_enable);
        assert!(cache_enable < readback);
    }

    #[test]
    fn ap_restore_requires_the_sealed_bsp_baseline_and_exact_capability() {
        // ORDERING: this source-visible assertion pins the AP's acquire
        // admission ahead of every MTRR/PAT restore action.
        let source = include_str!("cache_attributes.rs");
        let body = source
            .split_once("pub(super) fn initialize_application_processor_cache_attributes()")
            .expect("AP cache initializer must remain source-visible")
            .1
            .split_once("#[cfg(test)]")
            .expect("AP cache initializer must precede tests")
            .0;
        // ORDERING: this is the source-visible AP acquire admission check.
        let acquire = body
            .find("BASELINE_STATE.load(Ordering::Acquire) != BASELINE_READY")
            .expect("AP must acquire one completely sealed BSP baseline");
        let exact_cap = body
            .find("cap != BSP_MTRR_CAP.load(Ordering::Relaxed)")
            .expect("AP MTRR capability must exactly match the BSP");
        let no_global_tlb = body
            .find("read_cr4() & CR4_PAGE_GLOBAL_ENABLE != 0")
            .expect("AP restore must reject a global-page TLB context");
        let restore = body
            .find("restore_mtrr_and_pat_baseline(cap)")
            .expect("AP must restore the complete sealed MTRR/PAT record");
        assert!(acquire < exact_cap);
        assert!(exact_cap < no_global_tlb);
        assert!(no_global_tlb < restore);
    }
}
