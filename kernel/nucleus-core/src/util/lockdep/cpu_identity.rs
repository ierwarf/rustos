//! Dense logical-CPU to architectural APIC identity publication.
//!
//! - **Owner:** nucleus lockdep owns the diagnostic CPU identity map; HAL owns
//!   firmware topology admission.
//! - **Boundary:** logical indexes and APIC IDs arrive only from the complete
//!   admitted topology and are untrusted until dense uniqueness checks pass.
//! - **Lifecycle:** stage each exact association, release-publish the complete
//!   map once, then resolve every lock operation against that immutable map.
//! - **Concurrency:** BSP-only staging precedes one Release publication;
//!   readers Acquire the publication flag before reading relaxed payloads.
//! - **Failure:** duplicate, sparse, late, over-capacity, or unknown current
//!   CPU identity panics before lock ownership can alias.
//! - **Forbidden:** no raw APIC ID indexes a per-CPU array and no map mutation
//!   follows publication.
//! - **Evidence:** `cpu-online-lifecycle`, `scheduler-cpu-ownership`, and the
//!   dense APIC identity unit witness.

#[cfg(rustos_boot_image)]
use super::{CPU_APIC_IDENTITIES, CPU_IDENTITIES_PUBLISHED, CPU_IDENTITY_COUNT, MAX_TRACKED_CPUS};
#[cfg(rustos_boot_image)]
use core::sync::atomic::{AtomicBool, Ordering};
#[cfg(rustos_boot_image)]
use x86_64::registers::model_specific::Msr;

#[cfg(rustos_boot_image)]
const IA32_TSC_AUX: u32 = 0xC000_0103;
#[cfg(rustos_boot_image)]
static RDTSCP_CPU_TOKEN_ADMITTED: AtomicBool = AtomicBool::new(false);

#[cfg(rustos_boot_image)]
pub fn current_cpu_index() -> usize {
    // ORDERING: Acquire makes the complete dense APIC-ID map visible after HAL
    // finishes ACPI topology publication. Earlier BSP boot remains index zero.
    if !CPU_IDENTITIES_PUBLISHED.load(Ordering::Acquire) {
        return 0;
    }
    let count = CPU_IDENTITY_COUNT.load(Ordering::Relaxed);
    // ORDERING: Acquire observes token admission after immutable topology
    // publication. TSC_AUX itself is CPU-local architectural state.
    if RDTSCP_CPU_TOKEN_ADMITTED.load(Ordering::Acquire) {
        let mut aux = 0_u32;
        // SAFETY: admission proved RDTSCP support; the instruction only reads
        // the timestamp and this CPU's kernel-owned TSC_AUX token.
        let _ = unsafe { core::arch::x86_64::__rdtscp(&mut aux) };
        if let Some(index) = decode_cpu_token(aux, count) {
            return index;
        }
        if aux != 0 {
            panic!("lockdep invariant: current TSC_AUX token {aux:#x} is outside topology");
        }
    }
    let apic_id = hardware_apic_id();
    let index = select_cpu_index(
        CPU_APIC_IDENTITIES[..count]
            .iter()
            .map(|identity| identity.load(Ordering::Relaxed)),
        apic_id,
    );
    if let Some(index) = index {
        return index;
    }
    panic!("lockdep invariant: current APIC ID {apic_id:#x} lacks a logical CPU index");
}

#[cfg(not(rustos_boot_image))]
pub fn current_cpu_index() -> usize {
    0
}

/// Returns the calling CPU's admitted architectural APIC identity.
///
/// `hardware_apic_id` derives the identity with `CPUID`, which is an
/// unconditional VM exit on every virtualized product topology. The raw-guard
/// ownership check captured it on acquisition, on failed acquisition, and again
/// on release, so each tracked lock put several exits inside the critical
/// section it protects. That made nested-lock cost scale with CPU count rather
/// than with the protected work, and on the serialized dispatch path it
/// dominated every scheduling decision.
///
/// The dense identity map is immutable after topology publication, so this
/// reads the admitted identity for the logical index instead. The check stays
/// exactly as strong: the logical index comes from CPU-local architectural
/// state and is rejected outright when it falls outside the admitted topology.
/// Before publication there is no map to read and the `CPUID` derivation
/// remains the only source.
#[cfg(rustos_boot_image)]
pub fn current_apic_id() -> u32 {
    // ORDERING: Acquire observes the complete dense map published by topology
    // admission before any entry is read.
    if !CPU_IDENTITIES_PUBLISHED.load(Ordering::Acquire) {
        return hardware_apic_id();
    }
    let index = current_cpu_index();
    let count = CPU_IDENTITY_COUNT.load(Ordering::Relaxed);
    if index >= count {
        return hardware_apic_id();
    }
    // ORDERING: the publication flag acquired above covers this payload; the
    // map is immutable afterwards. Zero is the never-published sentinel, so the
    // stored value is the identity biased by one.
    let encoded = CPU_APIC_IDENTITIES[index].load(Ordering::Relaxed);
    match encoded {
        0 => hardware_apic_id(),
        encoded => u32::try_from(encoded - 1)
            .expect("lockdep invariant: admitted APIC identity exceeds u32 capacity"),
    }
}

#[cfg(not(rustos_boot_image))]
pub fn current_apic_id() -> u32 {
    0
}

#[cfg(any(rustos_boot_image, test))]
pub(super) fn select_cpu_index(
    identities: impl IntoIterator<Item = u64>,
    current_apic_id: u32,
) -> Option<usize> {
    let encoded = u64::from(current_apic_id) + 1;
    identities
        .into_iter()
        .position(|identity| identity == encoded)
}

#[cfg(any(rustos_boot_image, test))]
pub(super) const fn decode_cpu_token(token: u32, cpu_count: usize) -> Option<usize> {
    let Some(index) = token.checked_sub(1) else {
        return None;
    };
    let index = index as usize;
    if index < cpu_count { Some(index) } else { None }
}

/// Stage one dense logical-index/APIC-ID association during BSP admission.
pub fn register_cpu_identity(logical_index: u8, apic_id: u32) {
    #[cfg(rustos_boot_image)]
    {
        // ORDERING: Acquire rejects mutation after the Release publication of
        // the complete dense identity map.
        assert!(
            !CPU_IDENTITIES_PUBLISHED.load(Ordering::Acquire),
            "lockdep invariant: CPU identity registered after publication"
        );
        let index = usize::from(logical_index);
        assert!(
            index < MAX_TRACKED_CPUS,
            "lockdep invariant: logical CPU index exceeds capacity"
        );
        let encoded = u64::from(apic_id) + 1;
        assert!(
            CPU_APIC_IDENTITIES
                .iter()
                .all(|current| current.load(Ordering::Relaxed) != encoded),
            "lockdep invariant: duplicate APIC identity {apic_id:#x}"
        );
        // ORDERING: AcqRel exclusively stages this slot; the later Release
        // publication flag makes every staged identity visible to readers.
        assert!(
            CPU_APIC_IDENTITIES[index]
                .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "lockdep invariant: logical CPU identity registered twice"
        );
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = (logical_index, apic_id);
    }
}

/// Release-publish the complete dense CPU identity map.
pub fn finalize_cpu_identities(cpu_count: usize) {
    #[cfg(rustos_boot_image)]
    {
        assert!(
            (1..=MAX_TRACKED_CPUS).contains(&cpu_count),
            "lockdep invariant: invalid CPU identity count {cpu_count}"
        );
        assert!(
            CPU_APIC_IDENTITIES[..cpu_count]
                .iter()
                .all(|identity| identity.load(Ordering::Relaxed) != 0),
            "lockdep invariant: CPU identity map is not dense"
        );
        assert!(
            CPU_APIC_IDENTITIES[cpu_count..]
                .iter()
                .all(|identity| identity.load(Ordering::Relaxed) == 0),
            "lockdep invariant: CPU identity exists beyond published count"
        );
        CPU_IDENTITY_COUNT.store(cpu_count, Ordering::Relaxed);
        // ORDERING: Release publishes the dense count and every encoded APIC
        // identity to lock operations that Acquire the publication flag.
        assert!(
            CPU_IDENTITIES_PUBLISHED
                .compare_exchange(false, true, Ordering::Release, Ordering::Acquire)
                .is_ok(),
            "lockdep invariant: CPU identity map published twice"
        );
        if rdtscp_supported() {
            // ORDERING: Release admits the CPU-local token only after the
            // immutable dense topology is visible.
            RDTSCP_CPU_TOKEN_ADMITTED.store(true, Ordering::Release);
        }
        bind_current_cpu_identity(0, hardware_apic_id());
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = cpu_count;
    }
}

/// Bind the validated current CPU to an O(1) lockdep lookup token.
pub fn bind_current_cpu_identity(logical_index: u8, expected_apic_id: u32) {
    #[cfg(rustos_boot_image)]
    {
        assert!(
            CPU_IDENTITIES_PUBLISHED.load(Ordering::Acquire),
            "lockdep invariant: CPU token bound before topology publication"
        );
        let index = usize::from(logical_index);
        let count = CPU_IDENTITY_COUNT.load(Ordering::Relaxed);
        assert!(
            index < count,
            "lockdep invariant: CPU token index is absent"
        );
        assert_eq!(
            CPU_APIC_IDENTITIES[index].load(Ordering::Relaxed),
            u64::from(expected_apic_id) + 1,
            "lockdep invariant: CPU token disagrees with admitted topology"
        );
        assert_eq!(
            hardware_apic_id(),
            expected_apic_id,
            "lockdep invariant: CPU token bound on the wrong APIC"
        );
        if !RDTSCP_CPU_TOKEN_ADMITTED.load(Ordering::Acquire) {
            return;
        }
        let token = u32::from(logical_index) + 1;
        // SAFETY: IA32_TSC_AUX is CPU-local, topology is immutable, and only
        // this CPU writes its token during BSP/AP private initialization.
        unsafe {
            Msr::new(IA32_TSC_AUX).write(u64::from(token));
        }
        let mut observed = 0_u32;
        // SAFETY: RDTSCP support was admitted before the write above.
        let _ = unsafe { core::arch::x86_64::__rdtscp(&mut observed) };
        assert_eq!(
            observed, token,
            "lockdep invariant: CPU-local TSC_AUX token did not read back"
        );
    }
    #[cfg(not(rustos_boot_image))]
    {
        let _ = (logical_index, expected_apic_id);
    }
}

#[cfg(rustos_boot_image)]
fn rdtscp_supported() -> bool {
    use core::arch::x86_64::__cpuid;

    // CPUID is unprivileged and the extended maximum leaf gates the feature
    // word query.
    let maximum = __cpuid(0x8000_0000).eax;
    maximum >= 0x8000_0001 && __cpuid(0x8000_0001).edx & (1 << 27) != 0
}

#[cfg(rustos_boot_image)]
pub fn hardware_apic_id() -> u32 {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    // Charged so the sample count answers whether the dense map is actually
    // serving. Any nonzero count on a steady-state path means acquisitions are
    // still paying a VM exit apiece.
    let profile_entry = super::lock_profile::now();
    let _charge = ChargeHardwareApicId(profile_entry);

    // CPUID is unprivileged on x86_64 and these leaves are queried only after
    // checking the maximum supported basic leaf.
    let maximum = __cpuid(0).eax;
    for leaf in [0x1f, 0x0b] {
        if maximum >= leaf {
            // The maximum basic leaf above admits this topology query.
            let topology = __cpuid_count(leaf, 0);
            if topology.ebx != 0 {
                return topology.edx;
            }
        }
    }
    // SAFETY: CPUID leaf one is mandatory on x86_64.
    __cpuid(1).ebx >> 24
}

#[cfg(not(rustos_boot_image))]
pub fn hardware_apic_id() -> u32 {
    0
}

/// Charges the `CPUID` derivation on every exit path, including the early
/// returns inside the topology-leaf loop.
#[cfg(rustos_boot_image)]
struct ChargeHardwareApicId(u64);

#[cfg(rustos_boot_image)]
impl Drop for ChargeHardwareApicId {
    fn drop(&mut self) {
        super::lock_profile::charge(super::lock_profile::LockPhase::HardwareApicId, self.0);
    }
}
