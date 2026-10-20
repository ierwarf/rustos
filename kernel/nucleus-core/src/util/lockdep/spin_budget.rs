//! Duration-based fail-closed budget for raw-spin contention.

/// Conservative wall-clock proxy for early boot, before a calibrated
/// nanosecond clock is available to nucleus-core.
///
/// About a quarter second at 4 GHz. A raw-spin critical section is
/// microseconds long, so this still turns a real deadlock into a prompt
/// fail-stop; the former 62 ms bound could be exceeded by an ordinary
/// host-side deschedule of the vCPU holding the lock on an overcommitted
/// eight-vCPU guest, and turned that into a kernel panic.
pub(super) const RAW_SPIN_CYCLE_LIMIT: u64 = 1_000_000_000;
/// Iteration backstop when the TSC reading cannot advance. Each iteration is
/// at least one `pause`, so this stays above the cycle bound on current parts.
const RAW_SPIN_FALLBACK_LIMIT: usize = 40_000_000;

#[inline]
pub(super) const fn raw_spin_wait_exceeded(wait_cycles: u64, spins: usize) -> bool {
    wait_cycles >= RAW_SPIN_CYCLE_LIMIT || spins >= RAW_SPIN_FALLBACK_LIMIT
}

#[cfg(test)]
mod tests {
    use super::{RAW_SPIN_CYCLE_LIMIT, RAW_SPIN_FALLBACK_LIMIT, raw_spin_wait_exceeded};

    #[test]
    fn duration_is_primary_and_iteration_limit_is_tsc_recovery() {
        assert!(!raw_spin_wait_exceeded(
            RAW_SPIN_CYCLE_LIMIT - 1,
            RAW_SPIN_FALLBACK_LIMIT - 1
        ));
        assert!(raw_spin_wait_exceeded(RAW_SPIN_CYCLE_LIMIT, 1));
        assert!(raw_spin_wait_exceeded(0, RAW_SPIN_FALLBACK_LIMIT));
    }
}
