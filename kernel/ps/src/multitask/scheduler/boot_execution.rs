//! Owner: BSP boot execution configuration, not task scheduling policy.
//! Boundary: raw PIT encoding, including zero, stays independent of weights.
//! Lifecycle: BSP publication precedes admission; no runtime replacement API.
//! Concurrency: Release/Acquire publication, no guarded Scheduler data.
//! Failure: preserve the full u16 encoding; no missing-authority sentinel.
//! Forbidden: no lifecycle, CPU execution or billing grants.
//! Evidence: raw zero/full-range native witness and production build.
use core::sync::atomic::{AtomicU16, Ordering};
static TICK_DIVISOR: AtomicU16 = AtomicU16::new(0);
/// BSP boot publication precedes task admission. A raw zero PIT divisor keeps
/// its existing hardware encoding; it is not interpreted as missing authority.
pub(super) fn publish_tick(divisor: u16) {
    // ORDERING: BSP configuration publication precedes boot task admission.
    TICK_DIVISOR.store(divisor, Ordering::Release);
}
pub(super) fn tick_divisor() -> u16 {
    // ORDERING: observe the boot-published raw hardware encoding.
    TICK_DIVISOR.load(Ordering::Acquire)
}

#[cfg(test)]
#[test]
fn boot_tick_roundtrips_raw_zero_and_full_pit_encoding_without_task_weight_state() {
    publish_tick(0);
    assert_eq!(tick_divisor(), 0);
    publish_tick(u16::MAX);
    assert_eq!(tick_divisor(), u16::MAX);
    publish_tick(1);
    assert_eq!(tick_divisor(), 1);
    publish_tick(0);
}
