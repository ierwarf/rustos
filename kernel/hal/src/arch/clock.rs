//! Validated monotonic clocksource selection and conversion.
//!
//! - **Owner:** `kernel-hal` owns the one kernel monotonic time domain.
//! - **Boundary:** Firmware frequency/topology and hardware counters are
//!   admitted before they can own scheduler, timeout, or recovery decisions.
//! - **Lifecycle:** A source is uninitialized, validated, then immutable for
//!   the boot; source failure cannot silently switch time domains.
//! - **Concurrency:** Readers use the published source without allocation or
//!   service calls.
//! - **Failure:** Missing invariant-TSC/HPET support fails with an exact
//!   topology result rather than calendar-time substitution.
//! - **Forbidden:** No RTC calendar value, backwards time, or per-caller clock
//!   policy.
//! - **Evidence:** `monotonic-deadline-lifecycle`.
use core::arch::asm;
use core::arch::x86_64::__cpuid;
use core::hint::spin_loop;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

const SOURCE_UNINITIALIZED: u8 = 0;
const SOURCE_HPET: u8 = 1;
const SOURCE_INVARIANT_TSC: u8 = 2;

const HPET_CAPABILITIES_OFFSET: usize = 0x000;
const HPET_CONFIGURATION_OFFSET: usize = 0x010;
const HPET_MAIN_COUNTER_OFFSET: usize = 0x0f0;
const HPET_ENABLE: u64 = 1;
const HPET_COUNTER_64_BIT_CAPABLE: u64 = 1 << 13;
const FEMTOSECONDS_PER_SECOND: u128 = 1_000_000_000_000_000;
const FEMTOSECONDS_PER_NANOSECOND: u128 = 1_000_000;
const TSC_CALIBRATION_NANOS: u64 = 20_000_000;
const MIN_TSC_HZ: u64 = 1_000_000;
const MAX_TSC_HZ: u64 = 10_000_000_000;

static SOURCE: AtomicU8 = AtomicU8::new(SOURCE_UNINITIALIZED);
static HPET_BASE: AtomicU64 = AtomicU64::new(0);
static HPET_PERIOD_FS: AtomicU64 = AtomicU64::new(0);
static HPET_BASE_COUNTER: AtomicU64 = AtomicU64::new(0);
static TSC_BASE: AtomicU64 = AtomicU64::new(0);
static TSC_HZ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockSourceInfo {
    pub name: &'static str,
    pub frequency_hz: u64,
}

/// Installs a non-interrupt-counting monotonic clock source. The legacy RTC
/// periodic interrupt is deliberately not a clock source: virtual machines may
/// coalesce or lose those edges while the vCPU is descheduled. An invariant TSC
/// calibrated against HPET is preferred; a validated 64-bit HPET counter is the
/// bounded fallback.
pub fn init() -> Option<ClockSourceInfo> {
    if SOURCE.load(Ordering::Acquire) != SOURCE_UNINITIALIZED {
        return current_source();
    }

    let hpet = init_hpet();
    let tsc_hz = invariant_tsc_supported()
        .then(|| cpuid_tsc_frequency_hz().or_else(|| hpet.and_then(calibrate_tsc_with_hpet)))
        .flatten()
        .filter(|hz| (MIN_TSC_HZ..=MAX_TSC_HZ).contains(hz));
    if let Some(tsc_hz) = tsc_hz {
        // The calibrated rate remains useful for each CPU's local
        // TSC-deadline clockevent even when HPET owns global monotonic time.
        TSC_HZ.store(tsc_hz, Ordering::Relaxed);
        // Invariant TSC proves rate stability, not cross-CPU offset/skew. Until
        // AP rendezvous admission publishes per-CPU offsets, only a uniprocessor
        // topology may expose raw TSC as the global monotonic source.
        if tsc_clocksource_admitted(super::smp::cpu_count()) {
            TSC_BASE.store(read_tsc_ordered(), Ordering::Relaxed);
            SOURCE.store(SOURCE_INVARIANT_TSC, Ordering::Release);
            return Some(ClockSourceInfo {
                name: "invariant-tsc",
                frequency_hz: tsc_hz,
            });
        }
    }

    if let Some((base, period_fs, counter)) = hpet {
        HPET_BASE.store(base, Ordering::Relaxed);
        HPET_PERIOD_FS.store(period_fs, Ordering::Relaxed);
        HPET_BASE_COUNTER.store(counter, Ordering::Relaxed);
        SOURCE.store(SOURCE_HPET, Ordering::Release);
        return Some(ClockSourceInfo {
            name: "hpet",
            frequency_hz: hpet_frequency_hz(period_fs),
        });
    }

    None
}

pub fn current_source() -> Option<ClockSourceInfo> {
    match SOURCE.load(Ordering::Acquire) {
        SOURCE_INVARIANT_TSC => Some(ClockSourceInfo {
            name: "invariant-tsc",
            frequency_hz: TSC_HZ.load(Ordering::Acquire),
        }),
        SOURCE_HPET => {
            let period_fs = HPET_PERIOD_FS.load(Ordering::Acquire);
            Some(ClockSourceInfo {
                name: "hpet",
                frequency_hz: hpet_frequency_hz(period_fs),
            })
        }
        _ => None,
    }
}

pub fn invariant_tsc_frequency_hz() -> Option<u64> {
    // The rate is separately admitted for CPU-local TSC-deadline clockevents;
    // global monotonic time may deliberately remain on HPET under SMP.
    // ORDERING: Acquire observes the rate stored before clocksource admission;
    // zero remains the explicit not-calibrated sentinel.
    Some(TSC_HZ.load(Ordering::Acquire)).filter(|frequency| *frequency != 0)
}

pub fn monotonic_nanos() -> u64 {
    match SOURCE.load(Ordering::Acquire) {
        SOURCE_INVARIANT_TSC => {
            let hz = TSC_HZ.load(Ordering::Relaxed);
            if hz == 0 {
                return 0;
            }
            let delta = read_tsc_ordered().saturating_sub(TSC_BASE.load(Ordering::Relaxed));
            nanos_from_tsc_delta(delta, hz)
        }
        SOURCE_HPET => {
            let base = HPET_BASE.load(Ordering::Relaxed);
            let period_fs = HPET_PERIOD_FS.load(Ordering::Relaxed);
            let origin = HPET_BASE_COUNTER.load(Ordering::Relaxed);
            if base == 0 || period_fs == 0 {
                return 0;
            }
            let delta = read_hpet_counter(base).saturating_sub(origin);
            nanos_from_hpet_delta(delta, period_fs)
        }
        _ => 0,
    }
}

fn init_hpet() -> Option<(u64, u64, u64)> {
    let base = crate::arch::acpi::hpet_address()?;
    let capabilities =
        unsafe { read_volatile((base as usize + HPET_CAPABILITIES_OFFSET) as *const u64) };
    let period_fs = capabilities >> 32;
    // The HPET specification requires a period no greater than 100 ns. A
    // zero/oversized value or a 32-bit-only counter cannot provide our
    // long-lived monotonic contract without an additional wrap interrupt.
    if capabilities & HPET_COUNTER_64_BIT_CAPABLE == 0 || period_fs == 0 || period_fs > 100_000_000
    {
        return None;
    }
    let configuration = (base as usize + HPET_CONFIGURATION_OFFSET) as *mut u64;
    let current = unsafe { read_volatile(configuration) };
    unsafe { write_volatile(configuration, current | HPET_ENABLE) };
    let first = read_hpet_counter(base);
    let mut advanced = false;
    for _ in 0..100_000 {
        if read_hpet_counter(base) != first {
            advanced = true;
            break;
        }
        spin_loop();
    }
    if !advanced {
        return None;
    }
    Some((base, period_fs, read_hpet_counter(base)))
}

fn calibrate_tsc_with_hpet((base, period_fs, _): (u64, u64, u64)) -> Option<u64> {
    let calibration_ticks = ((u128::from(TSC_CALIBRATION_NANOS) * FEMTOSECONDS_PER_NANOSECOND)
        .div_ceil(u128::from(period_fs)))
    .try_into()
    .ok()?;
    let hpet_start = read_hpet_counter(base);
    let tsc_start = read_tsc_ordered();
    let target = hpet_start.checked_add(calibration_ticks)?;
    while read_hpet_counter(base) < target {
        spin_loop();
    }
    let tsc_end = read_tsc_ordered();
    let hpet_end = read_hpet_counter(base);
    let hpet_delta = hpet_end.saturating_sub(hpet_start);
    let tsc_delta = tsc_end.saturating_sub(tsc_start);
    if hpet_delta == 0 || tsc_delta == 0 {
        return None;
    }
    let elapsed_fs = u128::from(hpet_delta).checked_mul(u128::from(period_fs))?;
    let hz = u128::from(tsc_delta)
        .checked_mul(FEMTOSECONDS_PER_SECOND)?
        .checked_div(elapsed_fs)?;
    u64::try_from(hz).ok()
}

fn invariant_tsc_supported() -> bool {
    let maximum_extended_leaf = __cpuid(0x8000_0000).eax;
    maximum_extended_leaf >= 0x8000_0007 && __cpuid(0x8000_0007).edx & (1 << 8) != 0
}

const fn tsc_clocksource_admitted(cpu_count: usize) -> bool {
    cpu_count == 1
}

fn cpuid_tsc_frequency_hz() -> Option<u64> {
    let maximum_basic_leaf = __cpuid(0).eax;
    if maximum_basic_leaf < 0x15 {
        return None;
    }
    let leaf = __cpuid(0x15);
    if leaf.eax == 0 || leaf.ebx == 0 || leaf.ecx == 0 {
        return None;
    }
    u64::from(leaf.ecx)
        .checked_mul(u64::from(leaf.ebx))?
        .checked_div(u64::from(leaf.eax))
}

fn read_hpet_counter(base: u64) -> u64 {
    unsafe { read_volatile((base as usize + HPET_MAIN_COUNTER_OFFSET) as *const u64) }
}

fn read_tsc_ordered() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!(
            "lfence",
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

fn nanos_from_tsc_delta(delta: u64, hz: u64) -> u64 {
    u64::try_from(
        u128::from(delta)
            .saturating_mul(1_000_000_000)
            .checked_div(u128::from(hz.max(1)))
            .unwrap_or(u128::MAX),
    )
    .unwrap_or(u64::MAX)
}

fn nanos_from_hpet_delta(delta: u64, period_fs: u64) -> u64 {
    u64::try_from(
        u128::from(delta)
            .saturating_mul(u128::from(period_fs))
            .checked_div(FEMTOSECONDS_PER_NANOSECOND)
            .unwrap_or(u128::MAX),
    )
    .unwrap_or(u64::MAX)
}

fn hpet_frequency_hz(period_fs: u64) -> u64 {
    if period_fs == 0 {
        0
    } else {
        u64::try_from(FEMTOSECONDS_PER_SECOND / u128::from(period_fs)).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsc_delta_conversion_is_exact_for_common_frequencies() {
        assert_eq!(nanos_from_tsc_delta(2_500_000, 2_500_000_000), 1_000_000);
        assert_eq!(
            nanos_from_tsc_delta(4_000_000_000, 4_000_000_000),
            1_000_000_000
        );
    }

    #[test]
    fn hpet_delta_conversion_uses_femtosecond_period() {
        assert_eq!(
            nanos_from_hpet_delta(10_000_000, 100_000_000),
            1_000_000_000
        );
        assert_eq!(hpet_frequency_hz(100_000_000), 10_000_000);
    }

    #[test]
    fn raw_tsc_global_clock_is_rejected_until_smp_offsets_are_admitted() {
        assert!(tsc_clocksource_admitted(1));
        assert!(!tsc_clocksource_admitted(2));
        assert!(!tsc_clocksource_admitted(4));
        assert!(!tsc_clocksource_admitted(8));
    }
}
