//! Scheduler adapters for per-slot fair-share weights.

use super::Scheduler;
#[cfg(not(test))]
use super::runqueue;

// CFS-like fairness constants (mirrors Linux kernel/sched/fair.c).
// NICE_0_LOAD is the nominal weight; vruntime delta = elapsed * NICE_0_LOAD / weight.
// Smaller weight -> larger vruntime per real-time unit -> less CPU share.
pub(in crate::multitask) const NICE_0_LOAD: u32 = 1024;
pub(in crate::multitask) const MIN_LOAD_WEIGHT: u32 = 32;
pub(in crate::multitask) const MAX_LOAD_WEIGHT: u32 = 1_000_000;
pub(in crate::multitask) const SYSTEM_CLASS_WEIGHT_FLAG: u32 = 1 << 31;
pub(in crate::multitask) const LOAD_WEIGHT_MASK: u32 = !SYSTEM_CLASS_WEIGHT_FLAG;
pub(in crate::multitask) const INTERACTIVE_PIT_DIVISOR_FLAG: u16 = 1 << 15;

impl Scheduler {
    /// Linux-style weighted vruntime delta:
    /// `delta_vruntime = delta_exec * NICE_0_LOAD / weight`.
    /// Heavier-weight tasks accrue vruntime more slowly and therefore receive
    /// a proportionally larger share of CPU time.
    pub(super) fn weighted_vruntime_delta(elapsed_ns: u64, weight: u32) -> u64 {
        let w = (weight & LOAD_WEIGHT_MASK).clamp(MIN_LOAD_WEIGHT, MAX_LOAD_WEIGHT) as u64;
        elapsed_ns
            .saturating_mul(NICE_0_LOAD as u64)
            .saturating_div(w)
    }

    /// Maps the per-task PIT divisor (proportional to its `weight_micros`)
    /// onto a CFS load weight. The default user task weight_micros=100 yields
    /// divisor ~119, which we scale to ~952 (close to NICE_0_LOAD=1024).
    /// Heavier services such as `uiserver` (weight_micros=2000) end up around
    /// ~19000 and naturally receive ~20x more CPU when contending.
    pub(super) fn weight_from_pit_divisor(divisor: u16) -> u32 {
        // pit_divisor is BASE_FREQUENCY_HZ * weight_micros / 1_000_000, so it
        // is monotonically increasing in weight_micros. Using `divisor * 8`
        // keeps default-weight tasks near NICE_0_LOAD without arithmetic that
        // requires knowing the PIT base frequency at this layer.
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

    #[inline]
    pub(super) fn slot_weight(&self, slot: usize) -> u32 {
        #[cfg(not(test))]
        {
            runqueue::weight::value(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .expect("test scheduler task lost its fair-share weight")
                .weight
        }
    }

    #[inline]
    pub(super) fn initialize_slot_weight(&mut self, slot: usize, value: u32) {
        #[cfg(not(test))]
        runqueue::weight::initialize(slot, value);
        #[cfg(test)]
        let _ = (slot, value);
    }

    #[inline]
    pub(super) fn set_slot_weight(&mut self, slot: usize, value: u32) {
        #[cfg(not(test))]
        runqueue::weight::replace(slot, value);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.weight = value;
        }
    }
}
