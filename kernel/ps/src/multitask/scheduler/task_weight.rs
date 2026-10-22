//! Scheduler adapters for per-slot fair-share weights.

// CFS-like fairness constants (mirrors Linux kernel/sched/fair.c).
// NICE_0_LOAD is the nominal weight; vruntime delta = elapsed * NICE_0_LOAD / weight.
// Smaller weight -> larger vruntime per real-time unit -> less CPU share.
pub(in crate::multitask) const NICE_0_LOAD: u32 = 1024;
pub(in crate::multitask) const MIN_LOAD_WEIGHT: u32 = 32;
pub(in crate::multitask) const MAX_LOAD_WEIGHT: u32 = 1_000_000;
pub(in crate::multitask) const SYSTEM_CLASS_WEIGHT_FLAG: u32 = 1 << 31;
pub(in crate::multitask) const LOAD_WEIGHT_MASK: u32 = !SYSTEM_CLASS_WEIGHT_FLAG;
pub(in crate::multitask) const INTERACTIVE_PIT_DIVISOR_FLAG: u16 = 1 << 15;

/// Pure PIT encoding to load-weight conversion; not scheduling admission.
pub(super) fn weight_from_pit_divisor(divisor: u16) -> u32 {
    let interactive = divisor & INTERACTIVE_PIT_DIVISOR_FLAG != 0;
    let raw_divisor = divisor & !INTERACTIVE_PIT_DIVISOR_FLAG;
    let scaled = (raw_divisor.max(1) as u32).saturating_mul(8);
    let load = scaled.clamp(MIN_LOAD_WEIGHT, MAX_LOAD_WEIGHT);
    load | if interactive {
        SYSTEM_CLASS_WEIGHT_FLAG
    } else {
        0
    }
}
/// Pure fairness arithmetic; callers supply independently owned interval time.
pub(super) fn weighted_vruntime_delta(elapsed_ns: u64, weight: u32) -> u64 {
    let w = (weight & LOAD_WEIGHT_MASK).clamp(MIN_LOAD_WEIGHT, MAX_LOAD_WEIGHT) as u64;
    elapsed_ns.saturating_mul(NICE_0_LOAD as u64).saturating_div(w)
}
