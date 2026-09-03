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
//!   topology result rather than calendar-time substitution. A multiprocessor
//!   TSC upgrade requires a proven zero cross-CPU warp and otherwise remains
//!   fail-closed on the validated HPET counter.
//! - **Forbidden:** No RTC calendar value, backwards time, or per-caller clock
//!   policy.
//! - **Evidence:** `monotonic-deadline-lifecycle`.
use core::arch::asm;
use core::arch::x86_64::__cpuid;
use core::hint::spin_loop;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

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

/// Fixed-point scale for every counter-to-nanosecond conversion:
/// `nanos = (delta * mult) >> NANOS_SHIFT`.
///
/// A `u128` division is a call into `__udivti3`, and LLVM does not strength
/// reduce one even when the divisor is a literal -- checked against the
/// generated assembly, not assumed. Reading the clock went through two of them:
/// `monotonic_nanos` divided by the rate, and `rtc::ticks` divided the result
/// again. There are 55 call sites of the first and about 90 of the second.
///
/// The multiplier is derived once, when the rate is admitted. 48 bits keeps it
/// inside a `u64` across the whole admitted rate range and bounds the truncation
/// at `delta / 2^48` -- under a nanosecond per hour of uptime at 4 GHz.
pub(crate) const NANOS_SHIFT: u32 = 48;
static TSC_NANOS_MULT: AtomicU64 = AtomicU64::new(0);
static HPET_NANOS_MULT: AtomicU64 = AtomicU64::new(0);

// Cross-CPU TSC warp rendezvous. A multiprocessor topology may publish the raw
// TSC as the global monotonic source only after every application processor has
// proven, against the boot processor, that no CPU ever observes a timestamp
// earlier than one already published by another CPU. This is the admission
// test used by Linux (`check_tsc_warp`) and FreeBSD (`comp_smp_tsc`): it
// measures the property the kernel actually depends on — no backwards time —
// instead of assuming hypervisor or firmware synchronization.
//
// The alternative is not free. Without this admission the SMP monotonic source
// stays on the HPET main counter, whose every read is an MMIO access. Under a
// hardware-virtualized product topology that is a VM exit serialized against
// the other virtual CPUs, so scheduler, timeout, and IPC paths pay a
// cross-CPU-serialized exit for each timestamp.
const TSC_SYNC_NO_CPU: u32 = u32::MAX;
const TSC_SYNC_WINDOW_NANOS: u64 = 2_000_000;
const TSC_SYNC_RENDEZVOUS_TIMEOUT_NANOS: u64 = 200_000_000;
const TSC_SYNC_DEADLINE_POLL_SPINS: u32 = 1_024;
static TSC_SYNC_ACTIVE_CPU: AtomicU32 = AtomicU32::new(TSC_SYNC_NO_CPU);
static TSC_SYNC_RUNNING: AtomicBool = AtomicBool::new(false);
static TSC_SYNC_TARGET_PRESENT: AtomicBool = AtomicBool::new(false);
static TSC_SYNC_LAST: AtomicU64 = AtomicU64::new(0);
static TSC_SYNC_MAX_WARP: AtomicU64 = AtomicU64::new(0);
static TSC_SMP_ADMITTED_SKEW_NANOS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockSourceInfo {
    pub name: &'static str,
    pub frequency_hz: u64,
}

/// Why a TSC was or was not admitted, recorded during `init`.
///
/// The three fields are independent failure causes and a caller that sees only
/// "no clocksource" cannot tell them apart: a CPU without invariant TSC, an
/// invariant TSC whose rate no enumeration reports, and a rate that was
/// measured but fell outside the admitted range.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TscAdmissionFacts {
    pub invariant_supported: bool,
    /// CPUID leaf `0x15`. Intel-only in practice; AMD reports none.
    pub cpuid_frequency_hz: Option<u64>,
    /// Measured against another counter when CPUID cannot report a rate.
    pub calibrated_frequency_hz: Option<u64>,
}

static TSC_FACTS: Mutex<TscAdmissionFacts> = Mutex::new(TscAdmissionFacts {
    invariant_supported: false,
    cpuid_frequency_hz: None,
    calibrated_frequency_hz: None,
});

/// What `init` observed about this CPU's TSC.
pub fn tsc_admission_facts() -> TscAdmissionFacts {
    *TSC_FACTS.lock()
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
    let invariant_supported = invariant_tsc_supported();
    let cpuid_frequency_hz = invariant_supported.then(cpuid_tsc_frequency_hz).flatten();
    let calibrated_frequency_hz = (invariant_supported && cpuid_frequency_hz.is_none())
        .then(|| hpet.and_then(calibrate_tsc_with_hpet))
        .flatten();
    *TSC_FACTS.lock() = TscAdmissionFacts {
        invariant_supported,
        cpuid_frequency_hz,
        calibrated_frequency_hz,
    };
    let tsc_hz = cpuid_frequency_hz
        .or(calibrated_frequency_hz)
        .filter(|hz| (MIN_TSC_HZ..=MAX_TSC_HZ).contains(hz));
    if let Some(tsc_hz) = tsc_hz {
        // The calibrated rate remains useful for each CPU's local
        // TSC-deadline clockevent even when HPET owns global monotonic time.
        TSC_HZ.store(tsc_hz, Ordering::Relaxed);
        TSC_NANOS_MULT.store(nanos_mult_from_hz(tsc_hz), Ordering::Relaxed);
        // Invariant TSC proves rate stability, not cross-CPU offset/skew. Only
        // a uniprocessor topology may expose the raw TSC here. A multiprocessor
        // topology starts on HPET and is upgraded by
        // `promote_smp_tsc_clocksource` once every application processor has
        // completed the bounded zero-warp rendezvous.
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
        HPET_NANOS_MULT.store(nanos_mult_from_period_fs(period_fs), Ordering::Relaxed);
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

/// Reports the cross-CPU skew, in nanoseconds, that the multiprocessor TSC
/// admission actually proved. Zero is the only admitted value; the accessor
/// exists so boot evidence records a measurement rather than an assumption.
pub fn admitted_smp_tsc_skew_nanos() -> Option<u64> {
    // ORDERING: Acquire on the published source observes the measured skew
    // stored before the one-way promotion; a source that is not the promoted
    // TSC has no admitted measurement to report.
    (SOURCE.load(Ordering::Acquire) == SOURCE_INVARIANT_TSC)
        .then(|| TSC_SMP_ADMITTED_SKEW_NANOS.load(Ordering::Acquire))
}

/// One lock-free cross-CPU warp observation.
///
/// `TSC_SYNC_LAST` only ever advances, so it holds the largest timestamp any
/// participating CPU has published so far. Observing a local timestamp below
/// that published value proves this CPU's counter genuinely trails another
/// CPU's; a delay between the two reads can only make the local sample larger,
/// so the test has no false positives.
fn tsc_sync_step() {
    // ORDERING: Acquire observes the peer's published sample before this CPU
    // takes the timestamp it will compare against.
    let published = TSC_SYNC_LAST.load(Ordering::Acquire);
    let local = read_tsc_ordered();
    if local < published {
        TSC_SYNC_MAX_WARP.fetch_max(published - local, Ordering::Relaxed);
    }
    // ORDERING: AcqRel keeps the shared publication monotone and makes this
    // sample visible to the peer's next acquire load.
    TSC_SYNC_LAST.fetch_max(local, Ordering::AcqRel);
}

/// Application-processor participation in the bounded TSC warp rendezvous.
///
/// The caller is parked in its `OnlineParked` admission loop and owns no lock,
/// no allocation, and no interrupt state. It returns as soon as the boot
/// processor closes the window, so a rejected or absent rendezvous cannot
/// delay CPU admission beyond the boot processor's own bounded deadline.
pub fn tsc_sync_participate(logical_index: u32) {
    // ORDERING: Acquire pairs with the boot processor's target publication.
    if TSC_SYNC_ACTIVE_CPU.load(Ordering::Acquire) != logical_index {
        return;
    }
    // ORDERING: Release tells the boot processor this exact CPU joined before
    // any sample is contributed.
    TSC_SYNC_TARGET_PRESENT.store(true, Ordering::Release);
    // ORDERING: Acquire observes the window close published by the source.
    while TSC_SYNC_RUNNING.load(Ordering::Acquire) {
        tsc_sync_step();
    }
    // ORDERING: Release lets the source prove the target left the window
    // before the shared rendezvous state is reused for the next CPU.
    TSC_SYNC_TARGET_PRESENT.store(false, Ordering::Release);
}

/// Boot-processor side of the bounded TSC warp rendezvous with one exact
/// application processor.
///
/// Returns the largest cross-CPU backwards observation in nanoseconds, or
/// `None` when the rendezvous could not complete. Both outcomes are
/// fail-closed: only an exact zero-warp measurement can later admit the raw
/// TSC as the multiprocessor monotonic source.
pub fn measure_ap_tsc_warp_nanos(logical_index: u32) -> Option<u64> {
    // ORDERING: Acquire observes the calibrated rate published by clocksource
    // initialization before any rendezvous state is reused.
    let hz = TSC_HZ.load(Ordering::Acquire);
    if hz == 0 || logical_index == TSC_SYNC_NO_CPU {
        return None;
    }
    TSC_SYNC_MAX_WARP.store(0, Ordering::Relaxed);
    TSC_SYNC_LAST.store(read_tsc_ordered(), Ordering::Relaxed);
    TSC_SYNC_TARGET_PRESENT.store(false, Ordering::Relaxed);
    // ORDERING: Release opens the window before the target is named, so a CPU
    // that observes its own index never sees a stale closed window.
    TSC_SYNC_RUNNING.store(true, Ordering::Release);
    TSC_SYNC_ACTIVE_CPU.store(logical_index, Ordering::Release);

    let joined = wait_for_tsc_sync_target(true);
    if joined {
        let window_ticks = ticks_from_nanos(TSC_SYNC_WINDOW_NANOS, hz);
        let started_ticks = read_tsc_ordered();
        while read_tsc_ordered().saturating_sub(started_ticks) < window_ticks {
            tsc_sync_step();
        }
    }
    // ORDERING: Release closes the window before the target publication is
    // withdrawn, so the target always observes a terminating condition.
    TSC_SYNC_RUNNING.store(false, Ordering::Release);
    let left = wait_for_tsc_sync_target(false);
    TSC_SYNC_ACTIVE_CPU.store(TSC_SYNC_NO_CPU, Ordering::Release);
    if !joined || !left {
        return None;
    }
    Some(nanos_from_tsc_delta(
        TSC_SYNC_MAX_WARP.load(Ordering::Relaxed),
        hz,
    ))
}

/// Waits for the rendezvous target to reach `expected` presence within the
/// bounded rendezvous deadline. The deadline is sampled once per fixed spin
/// batch because the current monotonic source is still the MMIO counter this
/// admission exists to retire.
fn wait_for_tsc_sync_target(expected: bool) -> bool {
    let started_at = monotonic_nanos();
    let mut spins: u32 = 0;
    // ORDERING: Acquire pairs with the target's presence publication.
    while TSC_SYNC_TARGET_PRESENT.load(Ordering::Acquire) != expected {
        spins = spins.wrapping_add(1);
        if spins.is_multiple_of(TSC_SYNC_DEADLINE_POLL_SPINS)
            && monotonic_nanos().saturating_sub(started_at) >= TSC_SYNC_RENDEZVOUS_TIMEOUT_NANOS
        {
            return false;
        }
        spin_loop();
    }
    true
}

/// Promotes the calibrated invariant TSC to the global monotonic source after
/// every application processor proved a zero cross-CPU warp.
///
/// The promotion is a one-way upgrade performed by the boot processor while
/// every other CPU is still parked before `SchedulerReady`, so no CPU can
/// observe the two time domains out of order. The new origin is chosen so the
/// first TSC-derived reading is not earlier than the last HPET-derived one.
pub fn promote_smp_tsc_clocksource(worst_warp_nanos: u64) -> Option<ClockSourceInfo> {
    if worst_warp_nanos != 0 {
        return None;
    }
    // ORDERING: Acquire observes the calibrated rate and the current source
    // publication. The upgrade is one-way and only ever replaces the validated
    // MMIO fallback, so a repeated promotion is rejected rather than retried.
    let hz = TSC_HZ.load(Ordering::Acquire);
    if hz == 0 || SOURCE.load(Ordering::Acquire) != SOURCE_HPET {
        return None;
    }
    let continuation_nanos = monotonic_nanos();
    let now = read_tsc_ordered();
    TSC_BASE.store(
        now.saturating_sub(ticks_from_nanos(continuation_nanos, hz)),
        Ordering::Relaxed,
    );
    TSC_SMP_ADMITTED_SKEW_NANOS.store(worst_warp_nanos, Ordering::Relaxed);
    // ORDERING: Release publishes the new origin before the source switch that
    // makes readers use it.
    SOURCE.store(SOURCE_INVARIANT_TSC, Ordering::Release);
    Some(ClockSourceInfo {
        name: "invariant-tsc",
        frequency_hz: hz,
    })
}

/// Bounded wall-clock budget for a spin loop, sampled once per fixed batch of
/// iterations.
///
/// A spin loop must never read the monotonic source on every iteration. Until
/// the multiprocessor TSC admission completes, that source is the HPET main
/// counter, and under the virtualized product topology each read is an exit
/// serialized against the other CPUs: a waiter polling it per iteration
/// actively slows the owner it is waiting for, which converts a bounded
/// dead-owner watchdog into the dominant cost of the path it protects.
///
/// Batching costs no useful resolution because every caller's budget is orders
/// of magnitude larger than one batch of spins.
pub struct SpinDeadline {
    started_at_nanos: u64,
    elapsed_nanos: u64,
    spins: u32,
}

impl SpinDeadline {
    const SAMPLE_INTERVAL_SPINS: u32 = 1_024;

    pub fn start() -> Self {
        Self {
            started_at_nanos: monotonic_nanos(),
            elapsed_nanos: 0,
            spins: 0,
        }
    }

    /// Returns the nanoseconds elapsed since the deadline started, refreshing
    /// the monotonic sample on the first call and once per batch afterwards.
    pub fn elapsed_nanos(&mut self) -> u64 {
        if self.spins.is_multiple_of(Self::SAMPLE_INTERVAL_SPINS) {
            self.elapsed_nanos = monotonic_nanos().saturating_sub(self.started_at_nanos);
        }
        self.spins = self.spins.wrapping_add(1);
        self.elapsed_nanos
    }

    /// Returns an exact, unbatched elapsed measurement. Use this only on a
    /// terminal path such as a watchdog panic message.
    pub fn exact_elapsed_nanos(&self) -> u64 {
        monotonic_nanos().saturating_sub(self.started_at_nanos)
    }
}

fn ticks_from_nanos(nanos: u64, hz: u64) -> u64 {
    u64::try_from(
        u128::from(nanos)
            .saturating_mul(u128::from(hz))
            .checked_div(1_000_000_000)
            .unwrap_or(u128::MAX),
    )
    .unwrap_or(u64::MAX)
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

/// `1e9 / hz` in `NANOS_SHIFT` fixed point, rounded up.
///
/// Rounding up is the whole reason the conversions still round-trip. The
/// product is truncated by the shift, so a multiplier rounded *down* truncates
/// twice: at 2.5 GHz, one millisecond of counter came back as 999,999 ns and
/// the promotion-continuity witness caught it. A multiplier at or above the
/// true ratio makes the result at or above the division's, and the shift can
/// then only bring it back down to it.
fn nanos_mult_from_hz(hz: u64) -> u64 {
    let hz = u128::from(hz.max(1));
    u64::try_from((1_000_000_000_u128 << NANOS_SHIFT).div_ceil(hz)).unwrap_or(u64::MAX)
}

/// `period_fs / 1e6` in `NANOS_SHIFT` fixed point, rounded up for the same
/// reason as [`nanos_mult_from_hz`].
fn nanos_mult_from_period_fs(period_fs: u64) -> u64 {
    u64::try_from((u128::from(period_fs) << NANOS_SHIFT).div_ceil(FEMTOSECONDS_PER_NANOSECOND))
        .unwrap_or(u64::MAX)
}

/// One `mul` and one shift, where a division used to be a libcall.
#[inline]
pub(crate) fn scale_by_nanos_mult(delta: u64, mult: u64) -> u64 {
    u64::try_from((u128::from(delta) * u128::from(mult)) >> NANOS_SHIFT).unwrap_or(u64::MAX)
}

fn nanos_from_tsc_delta(delta: u64, hz: u64) -> u64 {
    // The multiplier is published with the rate, so a zero here means a caller
    // reached this before admission. Deriving it now keeps the answer exact
    // rather than reporting zero elapsed time.
    let mult = match TSC_NANOS_MULT.load(Ordering::Relaxed) {
        0 => nanos_mult_from_hz(hz),
        mult => mult,
    };
    scale_by_nanos_mult(delta, mult)
}

fn nanos_from_hpet_delta(delta: u64, period_fs: u64) -> u64 {
    let mult = match HPET_NANOS_MULT.load(Ordering::Relaxed) {
        0 => nanos_mult_from_period_fs(period_fs),
        mult => mult,
    };
    scale_by_nanos_mult(delta, mult)
}

/// The division this file used to perform, kept as the reference the fixed-point
/// conversions are checked against.
#[cfg(test)]
fn nanos_from_hpet_delta_by_division(delta: u64, period_fs: u64) -> u64 {
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

    #[test]
    fn smp_tsc_promotion_requires_an_exact_zero_cross_cpu_warp() {
        // A promotion attempt is admissible only for a measured zero warp. Any
        // nonzero observation, and any rendezvous that failed to complete, must
        // leave the validated MMIO counter as the multiprocessor source.
        assert!(promote_smp_tsc_clocksource(1).is_none());
        assert!(promote_smp_tsc_clocksource(u64::MAX).is_none());
        // Zero warp is necessary but not sufficient: an uncalibrated rate or a
        // source that is not the HPET fallback still refuses the upgrade, so
        // the host-test build (no clocksource initialized) stays rejected.
        assert!(promote_smp_tsc_clocksource(0).is_none());
        assert!(admitted_smp_tsc_skew_nanos().is_none());
    }

    #[test]
    fn tsc_origin_conversion_round_trips_the_promotion_continuity_math() {
        // The promotion picks an origin so the first TSC-derived reading is not
        // earlier than the last HPET-derived one; both conversions must agree.
        let hz = 2_500_000_000_u64;
        assert_eq!(ticks_from_nanos(1_000_000_000, hz), hz);
        assert_eq!(nanos_from_tsc_delta(ticks_from_nanos(4_000, hz), hz), 4_000);
        assert_eq!(ticks_from_nanos(0, hz), 0);
    }

    /// The fixed-point conversion replaced a `__udivti3` call. It has to give
    /// the same answer as the division it replaced, across the whole admitted
    /// rate range and out to a realistic uptime, or it traded a real cost for a
    /// real bug.
    #[test]
    fn the_fixed_point_conversion_matches_the_division_it_replaced() {
        fn by_division(delta: u64, hz: u64) -> u64 {
            u64::try_from(
                u128::from(delta)
                    .saturating_mul(1_000_000_000)
                    .checked_div(u128::from(hz.max(1)))
                    .unwrap_or(u128::MAX),
            )
            .unwrap_or(u64::MAX)
        }

        for hz in [
            MIN_TSC_HZ,
            1_000_000_000,
            2_400_000_000,
            3_991_222_000,
            MAX_TSC_HZ,
        ] {
            let mult = nanos_mult_from_hz(hz);
            // One second, one hour, and one day of counter, plus the edges.
            for delta in [0, 1, 1_000, hz, hz * 3_600, hz.saturating_mul(86_400)] {
                let exact = by_division(delta, hz);
                let scaled = scale_by_nanos_mult(delta, mult);
                // The multiplier rounds up and the shift truncates, so the
                // result is never below the division's and never more than one
                // part in 2^48 of the counter delta above it.
                let slack = (u128::from(delta) >> NANOS_SHIFT) as u64 + 1;
                assert!(
                    scaled >= exact && scaled - exact <= slack,
                    "hz={hz} delta={delta}: fixed point {scaled} vs division {exact}"
                );
            }
        }
    }

    /// Same obligation for the HPET path, against the division still in this
    /// file as the reference.
    #[test]
    fn the_hpet_fixed_point_conversion_matches_its_division() {
        for period_fs in [69_841_279_u64, 100_000_000, 10_000_000] {
            let mult = nanos_mult_from_period_fs(period_fs);
            for delta in [0_u64, 1, 1_000, 14_318_180, 14_318_180 * 3_600] {
                let exact = nanos_from_hpet_delta_by_division(delta, period_fs);
                let scaled = scale_by_nanos_mult(delta, mult);
                let slack = (u128::from(delta) >> NANOS_SHIFT) as u64 + 1;
                assert!(
                    scaled >= exact && scaled - exact <= slack,
                    "period_fs={period_fs} delta={delta}: {scaled} vs {exact}"
                );
            }
        }
    }
}
