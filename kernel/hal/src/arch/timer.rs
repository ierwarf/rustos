//! Per-CPU local APIC TSC-deadline clockevent.
//!
//! - **Owner:** `kernel-hal` owns local timer hardware programming; kernel-ps
//!   owns scheduling policy after each edge.
//! - **Boundary:** Logical APs admitted by the CPU lifecycle receive one fixed
//!   private vector and a calibrated invariant-TSC deadline.
//! - **Lifecycle:** Program the LVT while interrupts are disabled, arm before
//!   Online publication, rearm after clockevent work and before EOI, and stop
//!   on CPU teardown.
//! - **Concurrency:** Each CPU writes only its local APIC view, deadline MSR,
//!   and indexed next-deadline slot.
//! - **Failure:** Missing TSC deadline, invariant-TSC calibration, local APIC,
//!   or deadline progress fails SMP admission.
//! - **Forbidden:** No shared PIT programming from an AP, interrupt-counting
//!   wall clock, uncalibrated APIC count, or vector shared with MSI.
//! - **Evidence:** `per-cpu-clockevent-lifecycle`.

use core::arch::x86_64::{__cpuid, _mm_mfence, _rdtsc};
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::registers::model_specific::Msr;

use super::acpi::MAX_SUPPORTED_CPUS;

const IA32_TSC_DEADLINE: u32 = 0x6e0;
const CPUID_TSC_DEADLINE: u32 = 1 << 24;
const APIC_LVT_TIMER_OFFSET: usize = 0x320;
const APIC_LVT_TSC_DEADLINE: u32 = 2 << 17;
// Match the BSP fair-class quantum. A private one-millisecond deadline on
// every AP multiplied periodic scheduler entries by the CPU count and made an
// otherwise idle 8-vCPU guest contend on scheduler bookkeeping thousands of
// times per second. Immediate IPC/wake/IPI safe points remain independent of
// this bounded periodic fallback.
const TICK_NANOS: u64 = 4_000_000;

static NEXT_DEADLINE: [AtomicU64; MAX_SUPPORTED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS];

pub fn init_current_cpu() -> bool {
    if x86_64::instructions::interrupts::are_enabled() {
        return false;
    }
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    if logical_index == 0 || logical_index >= MAX_SUPPORTED_CPUS {
        return false;
    }
    if __cpuid(1).ecx & CPUID_TSC_DEADLINE == 0 {
        return false;
    }
    let Some(tsc_hz) = super::clock::invariant_tsc_frequency_hz() else {
        return false;
    };
    let Some(base) = super::msi::local_apic_base_for_current_cpu() else {
        return false;
    };
    let Some(interval) = tick_interval(tsc_hz) else {
        return false;
    };

    // Intel SDM's TSC-deadline programming contract requires the xAPIC LVT
    // write to become visible before the IA32_TSC_DEADLINE WRMSR.  Keeping the
    // LVT masked while arming can lose the only first edge if a heavily
    // oversubscribed vCPU is descheduled until after that deadline.
    // SAFETY: local APIC admission owns this executing CPU's MMIO view.
    unsafe {
        let lvt = (base + APIC_LVT_TIMER_OFFSET) as *mut u32;
        lvt.write_volatile(
            (u32::from(super::idt::LOCAL_TIMER_VECTOR) & 0xff) | APIC_LVT_TSC_DEADLINE,
        );
        // SAFETY: MFENCE is available in the x86_64 baseline. Intel specifies
        // it to serialize the xAPIC MMIO LVT write with the following WRMSR.
        _mm_mfence();
    }
    // SAFETY: `_rdtsc` is available after invariant-TSC and deadline admission.
    let now = unsafe { _rdtsc() };
    let deadline = now
        .checked_add(interval)
        .filter(|deadline| *deadline > now)
        .unwrap_or_else(|| panic!("local timer invariant: initial TSC deadline overflow"));
    // ORDERING: Release publishes the first deadline after the fenced LVT
    // configuration and before this CPU can publish Online.
    NEXT_DEADLINE[logical_index].store(deadline, Ordering::Release);
    write_deadline(deadline);
    true
}

pub fn arm_next_tick() {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        logical_index > 0 && logical_index < MAX_SUPPORTED_CPUS,
        "local timer invariant: BSP or invalid CPU entered AP clockevent"
    );
    let tsc_hz = super::clock::invariant_tsc_frequency_hz()
        .expect("local timer invariant: invariant TSC disappeared");
    let interval = tick_interval(tsc_hz).expect("local timer invariant: invalid TSC frequency");
    // SAFETY: this handler runs only after the admitted TSC-deadline vector.
    let now = unsafe { _rdtsc() };
    // ORDERING: Acquire observes the initial or prior interrupt's deadline.
    let previous = NEXT_DEADLINE[logical_index].load(Ordering::Acquire);
    assert_ne!(
        previous, 0,
        "local timer invariant: interrupt arrived before initial deadline"
    );
    let deadline = next_future_deadline(previous, now, interval)
        .unwrap_or_else(|| panic!("local timer invariant: TSC deadline exhausted"));
    // ORDERING: Release publishes catch-up before the deadline MSR is armed.
    NEXT_DEADLINE[logical_index].store(deadline, Ordering::Release);
    write_deadline(deadline);
}

fn write_deadline(deadline: u64) {
    let mut deadline_msr = Msr::new(IA32_TSC_DEADLINE);
    // SAFETY: CPUID admission proved IA32_TSC_DEADLINE exists on this CPU.
    unsafe {
        deadline_msr.write(deadline);
    }
}

fn tick_interval(tsc_hz: u64) -> Option<u64> {
    let interval = (u128::from(tsc_hz) * u128::from(TICK_NANOS))
        .checked_div(1_000_000_000)?
        .try_into()
        .ok()?;
    (interval != 0).then_some(interval)
}

fn next_future_deadline(previous: u64, now: u64, interval: u64) -> Option<u64> {
    if previous > now {
        return previous.checked_add(interval);
    }
    let missed = now.checked_sub(previous)?.checked_div(interval)?;
    previous.checked_add(missed.checked_add(1)?.checked_mul(interval)?)
}

#[cfg(test)]
mod tests {
    use super::{next_future_deadline, tick_interval};

    #[test]
    fn tsc_deadline_interval_and_catchup_are_strictly_future_bounded() {
        assert_eq!(tick_interval(3_000_000_000), Some(12_000_000));
        assert_eq!(next_future_deadline(100, 99, 10), Some(110));
        assert_eq!(next_future_deadline(100, 100, 10), Some(110));
        assert_eq!(next_future_deadline(100, 125, 10), Some(130));
        assert_eq!(next_future_deadline(u64::MAX, u64::MAX, 1), None);
    }
}
