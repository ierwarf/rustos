//! Narrow x86 MSI/MSI-X receive substrate.
//!
//! Device policy stays with the owning driver domain. This module only owns a
//! bounded interrupt-vector allocator, IDT dispatch, and the local-APIC EOI
//! required for a PCI device to signal a fixed event queue. It deliberately
//! exposes function pointers rather than a general IRQ object graph so an IRQ
//! handler cannot allocate, block, or acquire a policy-service lock.
//!
//! - **Owner:** `kernel-hal` owns vector reservation, handler publication, and
//!   local-APIC acknowledgement mechanics; the driver domain owns device policy.
//! - **Boundary:** CPUID/MSR state, MMIO addresses, vector numbers, and handler
//!   publication are admitted before an interrupt can become observable.
//! - **Lifecycle:** Reserve an unpublished vector, program the masked device,
//!   publish the handler, unmask, then mask/revoke before returning the slot.
//! - **Concurrency:** Allocation and handler slots are atomic; IRQ dispatch is
//!   non-blocking and allocation-free.
//! - **Failure:** Unsupported APIC modes, exhausted vectors, invalid vectors,
//!   and partial setup fail closed without a live handler.
//! - **Forbidden:** No policy call, heap allocation, blocking, or vector reuse
//!   while the prior generation can still signal.
//! - **Evidence:** `msi-vector-lifecycle`, `irq-resource-accounting`, and
//!   `interrupt-return`.

use core::arch::x86_64::__cpuid;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use x86_64::instructions::interrupts;
use x86_64::registers::model_specific::Msr;

use super::acpi::MAX_SUPPORTED_CPUS;

pub const MSI_VECTOR_FIRST: u8 = 0x40;
pub const MSI_VECTOR_LAST: u8 = 0xdf;
const MSI_VECTOR_COUNT: usize = (MSI_VECTOR_LAST - MSI_VECTOR_FIRST + 1) as usize;
const IA32_APIC_BASE: u32 = 0x1b;
const APIC_BASE_ADDRESS_MASK: u64 = 0xffff_f000;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_SPURIOUS_VECTOR_OFFSET: usize = 0x0f0;
const APIC_EOI_OFFSET: usize = 0x0b0;
const APIC_ICR_LOW_OFFSET: usize = 0x300;
const APIC_ICR_HIGH_OFFSET: usize = 0x310;
const APIC_SPURIOUS_ENABLE: u32 = 1 << 8;
const APIC_SPURIOUS_VECTOR: u32 = 0xff;
const APIC_ICR_DELIVERY_STATUS: u32 = 1 << 12;
const APIC_ICR_LEVEL_ASSERT: u32 = 1 << 14;
const APIC_ICR_TRIGGER_LEVEL: u32 = 1 << 15;
const APIC_DELIVERY_MODE_FIXED: u32 = 0b000 << 8;
const APIC_DELIVERY_MODE_INIT: u32 = 0b101 << 8;
const APIC_DELIVERY_MODE_STARTUP: u32 = 0b110 << 8;
const CPUID_FEATURE_APIC: u32 = 1 << 9;
const INIT_SETTLE_NS: u64 = 10_000_000;
const SIPI_SETTLE_NS: u64 = 200_000;
const ICR_DELIVERY_TIMEOUT_NS: u64 = 100_000_000;

pub type MsiHandler = fn(u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsiMessage {
    pub address: u64,
    pub data: u32,
}

static HANDLERS: [AtomicUsize; MSI_VECTOR_COUNT] =
    [const { AtomicUsize::new(0) }; MSI_VECTOR_COUNT];
static ALLOCATED: [AtomicBool; MSI_VECTOR_COUNT] =
    [const { AtomicBool::new(false) }; MSI_VECTOR_COUNT];
static ALLOCATION_CURSOR: AtomicUsize = AtomicUsize::new(0);
static LOCAL_APIC_BASE: AtomicUsize = AtomicUsize::new(0);
static LOCAL_APIC_PHYSICAL_BASE: AtomicU64 = AtomicU64::new(0);
static LOCAL_APIC_READY: [AtomicBool; MAX_SUPPORTED_CPUS] =
    [const { AtomicBool::new(false) }; MAX_SUPPORTED_CPUS];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupIpiError {
    InterruptsEnabled,
    LocalApicUnavailable,
    UnsupportedDestination,
    InvalidVector,
    DeliveryTimeout,
}

/// A not-yet-published interrupt vector. Failed MSI-X setup drops the lease,
/// clears its exact handler, and returns the slot to the bounded pool. Only a
/// fully programmed, still-masked device may commit the reservation.
pub struct MsiVectorLease {
    vector: u8,
    handler: usize,
    committed: bool,
}

/// Revocable ownership of one published vector and its exact handler.
///
/// Device drivers keep this guard until every later transport publication has
/// succeeded. Dropping it revokes handler authority and returns the bounded
/// vector; `retain_permanent` is the final one-way commit for boot-lifetime
/// devices.
pub struct CommittedMsiVector {
    vector: u8,
    handler: usize,
    retained: bool,
}

pub fn physical_base() -> Option<u64> {
    let leaf1 = __cpuid(1);
    if leaf1.edx & CPUID_FEATURE_APIC == 0 {
        return None;
    }
    let apic_base = unsafe { Msr::new(IA32_APIC_BASE).read() };
    if apic_base & APIC_BASE_X2APIC != 0 {
        return None;
    }
    let physical_base = apic_base & APIC_BASE_ADDRESS_MASK;
    (physical_base != 0).then_some(physical_base)
}

/// Bind the admitted local-APIC physical page to one kernel-mm-owned,
/// uncacheable MMIO mapping before any CPU accesses APIC registers.
pub fn configure_mmio(expected_physical_base: u64, virtual_base: u64) -> bool {
    if expected_physical_base == 0
        || virtual_base == 0
        || !expected_physical_base.is_multiple_of(4096)
        || !virtual_base.is_multiple_of(4096)
        || physical_base() != Some(expected_physical_base)
    {
        return false;
    }
    let Ok(virtual_base) = usize::try_from(virtual_base) else {
        return false;
    };
    // ORDERING: Acquire observes the complete one-time physical/virtual
    // mapping publication before comparing an idempotent configuration.
    let published_phys = LOCAL_APIC_PHYSICAL_BASE.load(Ordering::Acquire);
    let published_virt = LOCAL_APIC_BASE.load(Ordering::Acquire);
    if published_phys != 0 || published_virt != 0 {
        return published_phys == expected_physical_base && published_virt == virtual_base;
    }
    if LOCAL_APIC_PHYSICAL_BASE
        .compare_exchange(
            0,
            expected_physical_base,
            // ORDERING: AcqRel claims the unique physical mapping identity.
            Ordering::AcqRel,
            // ORDERING: Acquire observes the winning mapping identity.
            Ordering::Acquire,
        )
        .is_err()
    {
        return false;
    }
    // ORDERING: Release publishes the kernel-mm mapping only after the exact
    // physical APIC page has been claimed above.
    LOCAL_APIC_BASE.store(virtual_base, Ordering::Release);
    true
}

/// Enable the local xAPIC path on the executing logical CPU. Extended x2APIC
/// destinations remain outside the release envelope until interrupt remapping
/// and MSI destination ownership are introduced together.
pub fn init() -> bool {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    if logical_index >= MAX_SUPPORTED_CPUS {
        return false;
    }
    // ORDERING: Acquire pairs with this CPU's completed SVR publication.
    if LOCAL_APIC_READY[logical_index].load(Ordering::Acquire) {
        return true;
    }
    let leaf1 = __cpuid(1);
    if leaf1.edx & CPUID_FEATURE_APIC == 0 {
        return false;
    }

    let mut apic_base_msr = Msr::new(IA32_APIC_BASE);
    let mut apic_base = unsafe { apic_base_msr.read() };
    if apic_base & APIC_BASE_X2APIC != 0 {
        return false;
    }
    if apic_base & APIC_BASE_ENABLE == 0 {
        apic_base |= APIC_BASE_ENABLE;
        unsafe {
            apic_base_msr.write(apic_base);
        }
    }
    let physical_base = apic_base & APIC_BASE_ADDRESS_MASK;
    // ORDERING: Acquire observes the admitted physical/virtual mapping pair
    // before this CPU accesses the local register page.
    if physical_base == 0 || LOCAL_APIC_PHYSICAL_BASE.load(Ordering::Acquire) != physical_base {
        return false;
    }
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    if base == 0 {
        return false;
    }
    // SAFETY: kernel-mm admitted this exact uncacheable page, and this CPU has
    // architectural ownership of its local APIC register view.
    unsafe {
        let spurious = (base + APIC_SPURIOUS_VECTOR_OFFSET) as *mut u32;
        let current = spurious.read_volatile();
        spurious.write_volatile((current & !0xff) | APIC_SPURIOUS_ENABLE | APIC_SPURIOUS_VECTOR);
    }
    // ORDERING: Release publishes completed SVR programming for this CPU.
    LOCAL_APIC_READY[logical_index].store(true, Ordering::Release);
    true
}

pub(super) fn local_apic_base_for_current_cpu() -> Option<usize> {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    if logical_index >= MAX_SUPPORTED_CPUS
        || !LOCAL_APIC_READY[logical_index].load(Ordering::Acquire)
    {
        return None;
    }
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    (base != 0).then_some(base)
}

/// Send the architected INIT-SIPI-SIPI sequence from the BSP to one admitted
/// xAPIC destination. The single mailbox is safe because callers wait for the
/// exact AP generation to acknowledge before targeting another CPU.
pub fn start_application_processor(
    apic_id: u32,
    startup_vector: u8,
) -> Result<(), StartupIpiError> {
    if interrupts::are_enabled() {
        return Err(StartupIpiError::InterruptsEnabled);
    }
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    // ORDERING: Acquire forbids ICR writes until this BSP's complete local
    // APIC programming is visible.
    if logical_index >= MAX_SUPPORTED_CPUS
        || !LOCAL_APIC_READY[logical_index].load(Ordering::Acquire)
    {
        return Err(StartupIpiError::LocalApicUnavailable);
    }
    if apic_id > u32::from(u8::MAX) || apic_id == nucleus_core::util::lockdep::hardware_apic_id() {
        return Err(StartupIpiError::UnsupportedDestination);
    }
    if startup_vector == 0 {
        return Err(StartupIpiError::InvalidVector);
    }

    let (destination, init_assert, init_deassert, startup_command) =
        startup_icr_words(apic_id, startup_vector).expect("validated xAPIC startup tuple");
    crate::debug::record_milestone(
        crate::debug::LogCategory::Boot,
        "smp-init-assert-begin",
        u64::from(apic_id),
        u64::from(init_assert),
    );
    write_icr(destination, init_assert)?;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Boot,
        "smp-init-assert-complete",
        u64::from(apic_id),
        0,
    );
    write_icr(destination, init_deassert)?;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Boot,
        "smp-init-deassert-complete",
        u64::from(apic_id),
        0,
    );
    busy_wait_ns(INIT_SETTLE_NS);
    crate::debug::record_milestone(
        crate::debug::LogCategory::Boot,
        "smp-init-settled",
        u64::from(apic_id),
        0,
    );
    write_icr(destination, startup_command)?;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Boot,
        "smp-first-sipi-complete",
        u64::from(apic_id),
        u64::from(startup_vector),
    );
    busy_wait_ns(SIPI_SETTLE_NS);
    write_icr(destination, startup_command)?;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Boot,
        "smp-second-sipi-complete",
        u64::from(apic_id),
        u64::from(startup_vector),
    );
    Ok(())
}

/// Deliver the private reschedule vector to one already-online xAPIC CPU.
///
/// Runtime callers may invoke this with interrupts enabled. The ICR high/low
/// pair is serialized locally with interrupts excluded; the scheduler's
/// per-CPU request flag provides the cross-CPU work publication.
pub fn send_reschedule_ipi(apic_id: u32) -> Result<(), StartupIpiError> {
    send_private_fixed_ipi(apic_id, super::idt::RESCHEDULE_IPI_VECTOR)
}

pub fn send_tlb_shootdown_ipi(apic_id: u32) -> Result<(), StartupIpiError> {
    send_private_fixed_ipi(apic_id, super::idt::TLB_SHOOTDOWN_IPI_VECTOR)
}

fn send_private_fixed_ipi(apic_id: u32, vector: u8) -> Result<(), StartupIpiError> {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    // ORDERING: Acquire forbids an ICR write until this CPU's local APIC has
    // completed SVR programming.
    if logical_index >= MAX_SUPPORTED_CPUS
        || !LOCAL_APIC_READY[logical_index].load(Ordering::Acquire)
    {
        return Err(StartupIpiError::LocalApicUnavailable);
    }
    if apic_id > u32::from(u8::MAX) || apic_id == nucleus_core::util::lockdep::hardware_apic_id() {
        return Err(StartupIpiError::UnsupportedDestination);
    }

    let (destination, command) =
        fixed_icr_words(apic_id, vector).ok_or(StartupIpiError::InvalidVector)?;
    interrupts::without_interrupts(|| write_icr(destination, command))
}

fn startup_icr_words(apic_id: u32, startup_vector: u8) -> Option<(u32, u32, u32, u32)> {
    if apic_id > u32::from(u8::MAX) || startup_vector == 0 {
        return None;
    }
    Some((
        apic_id << 24,
        APIC_DELIVERY_MODE_INIT | APIC_ICR_LEVEL_ASSERT | APIC_ICR_TRIGGER_LEVEL,
        APIC_DELIVERY_MODE_INIT | APIC_ICR_TRIGGER_LEVEL,
        APIC_DELIVERY_MODE_STARTUP | u32::from(startup_vector),
    ))
}

fn fixed_icr_words(apic_id: u32, vector: u8) -> Option<(u32, u32)> {
    if apic_id > u32::from(u8::MAX)
        || vector < 32
        || u32::from(vector) == APIC_SPURIOUS_VECTOR
        || (MSI_VECTOR_FIRST..=MSI_VECTOR_LAST).contains(&vector)
    {
        return None;
    }
    Some((
        apic_id << 24,
        APIC_DELIVERY_MODE_FIXED | APIC_ICR_LEVEL_ASSERT | u32::from(vector),
    ))
}

fn write_icr(destination: u32, command: u32) -> Result<(), StartupIpiError> {
    wait_for_icr_idle()?;
    // ORDERING: Acquire observes the immutable admitted MMIO base before the
    // volatile high/low ICR transaction.
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    if base == 0 {
        return Err(StartupIpiError::LocalApicUnavailable);
    }
    // SAFETY: only the BSP boot owner sends startup IPIs with interrupts
    // excluded, and both pointers address the admitted local-APIC MMIO page.
    unsafe {
        ((base + APIC_ICR_HIGH_OFFSET) as *mut u32).write_volatile(destination);
        ((base + APIC_ICR_LOW_OFFSET) as *mut u32).write_volatile(command);
    }
    wait_for_icr_idle()
}

fn wait_for_icr_idle() -> Result<(), StartupIpiError> {
    // ORDERING: Acquire observes the immutable admitted MMIO base before
    // polling hardware delivery state.
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    if base == 0 {
        return Err(StartupIpiError::LocalApicUnavailable);
    }
    // Interrupt delivery is idle on the first observation for every ordinary
    // reschedule, shootdown, and device IPI. Start the bounded deadline only
    // after an actual busy observation so the common path reads no clocksource
    // at all, and sample it in batches once it does exist.
    let mut deadline: Option<super::clock::SpinDeadline> = None;
    loop {
        // SAFETY: the configured mapping owns the complete local-APIC page.
        let command = unsafe { ((base + APIC_ICR_LOW_OFFSET) as *const u32).read_volatile() };
        if command & APIC_ICR_DELIVERY_STATUS == 0 {
            return Ok(());
        }
        if deadline
            .get_or_insert_with(super::clock::SpinDeadline::start)
            .elapsed_nanos()
            >= ICR_DELIVERY_TIMEOUT_NS
        {
            return Err(StartupIpiError::DeliveryTimeout);
        }
        spin_loop();
    }
}

fn busy_wait_ns(duration_ns: u64) {
    let start = super::clock::monotonic_nanos();
    while super::clock::monotonic_nanos().saturating_sub(start) < duration_ns {
        spin_loop();
    }
}

pub const fn vector_is_valid(vector: u8) -> bool {
    vector >= MSI_VECTOR_FIRST && vector <= MSI_VECTOR_LAST
}

impl MsiVectorLease {
    /// Reserve one vector without publishing any registration authority.
    pub fn allocate() -> Option<Self> {
        let start = ALLOCATION_CURSOR.fetch_add(1, Ordering::AcqRel) % MSI_VECTOR_COUNT;
        for offset in 0..MSI_VECTOR_COUNT {
            let index = (start + offset) % MSI_VECTOR_COUNT;
            if ALLOCATED[index]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(Self {
                    vector: MSI_VECTOR_FIRST + index as u8,
                    handler: 0,
                    committed: false,
                });
            }
        }
        None
    }

    pub const fn vector(&self) -> u8 {
        self.vector
    }

    /// Bind exactly one leaf handler to this unpublished lease.
    pub fn register_handler(&mut self, handler: MsiHandler) -> bool {
        if self.handler != 0 {
            return false;
        }
        let Some(index) = vector_index(self.vector) else {
            return false;
        };
        if !vector_has_registration_authority(self.vector, ALLOCATED[index].load(Ordering::Acquire))
        {
            return false;
        }
        let raw = handler as usize;
        if HANDLERS[index]
            .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.handler = raw;
        true
    }

    /// Return the exact xAPIC MSI tuple only for this lease's live handler.
    pub fn message(&self) -> Option<MsiMessage> {
        let index = vector_index(self.vector)?;
        if self.handler == 0
            || !LOCAL_APIC_READY[nucleus_core::util::lockdep::current_cpu_index()]
                .load(Ordering::Acquire)
            || !ALLOCATED[index].load(Ordering::Acquire)
            || HANDLERS[index].load(Ordering::Acquire) != self.handler
        {
            return None;
        }
        let leaf1 = __cpuid(1);
        let apic_id = (leaf1.ebx >> 24) as u64;
        Some(MsiMessage {
            address: 0xfee0_0000 | (apic_id << 12),
            data: u32::from(self.vector),
        })
    }

    /// Transfer the reservation into revocable committed ownership.
    pub fn commit(mut self) -> CommittedMsiVector {
        self.committed = true;
        CommittedMsiVector {
            vector: self.vector,
            handler: self.handler,
            retained: false,
        }
    }
}

impl CommittedMsiVector {
    pub const fn vector(&self) -> u8 {
        self.vector
    }

    /// Permanently retain the vector only after the complete device and
    /// transport transaction is externally visible and cannot fail.
    pub fn retain_permanent(mut self) -> u8 {
        self.retained = true;
        self.vector
    }
}

impl Drop for CommittedMsiVector {
    fn drop(&mut self) {
        if !self.retained {
            release_vector_handler(self.vector, self.handler);
        }
    }
}

impl Drop for MsiVectorLease {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        release_vector_handler(self.vector, self.handler);
    }
}

fn release_vector_handler(vector: u8, handler: usize) {
    let Some(index) = vector_index(vector) else {
        return;
    };
    // ORDERING: AcqRel removes the exact handler publication and observes any
    // prior owner initialization before making its allocation reusable.
    let handler_released = if handler == 0 {
        HANDLERS[index].load(Ordering::Acquire) == 0
    } else {
        HANDLERS[index]
            .compare_exchange(handler, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    };
    if handler_released {
        // ORDERING: release makes handler revocation visible before another
        // allocator may successfully reserve this vector.
        ALLOCATED[index].store(false, Ordering::Release);
    }
}

/// Called only from the generic MSI IDT entries. The callback must only set a
/// lock-free pending flag or acknowledge a bounded device register; policy and
/// buffer work run later in the owning service/broker turn.
pub fn dispatch(vector: u8) {
    if let Some(index) = vector_index(vector) {
        let raw = HANDLERS[index].load(Ordering::Acquire);
        if raw != 0 {
            let handler: MsiHandler = unsafe { core::mem::transmute(raw) };
            handler(vector);
        }
    }
    end_of_interrupt();
}

fn vector_index(vector: u8) -> Option<usize> {
    if !vector_is_valid(vector) {
        return None;
    }
    Some((vector - MSI_VECTOR_FIRST) as usize)
}

const fn vector_has_registration_authority(vector: u8, allocated: bool) -> bool {
    vector_is_valid(vector) && allocated
}

pub fn local_apic_eoi() {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    // ORDERING: Acquire proves this CPU completed SVR initialization before
    // acknowledging any published MSI handler.
    assert!(
        logical_index < MAX_SUPPORTED_CPUS
            && LOCAL_APIC_READY[logical_index].load(Ordering::Acquire),
        "local APIC EOI attempted before per-CPU initialization"
    );
    // ORDERING: Acquire observes the immutable MMIO mapping paired with the
    // per-CPU readiness publication.
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    assert_ne!(base, 0, "local APIC EOI has no admitted MMIO mapping");
    // SAFETY: the executing CPU initialized its local APIC against the admitted
    // mapping before any MSI vector could be published.
    unsafe {
        ((base + APIC_EOI_OFFSET) as *mut u32).write_volatile(0);
    }
}

fn end_of_interrupt() {
    local_apic_eoi();
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;

    use super::{
        ALLOCATED, APIC_DELIVERY_MODE_FIXED, APIC_DELIVERY_MODE_INIT, APIC_DELIVERY_MODE_STARTUP,
        APIC_ICR_LEVEL_ASSERT, APIC_ICR_TRIGGER_LEVEL, HANDLERS, MSI_VECTOR_FIRST, MSI_VECTOR_LAST,
        MsiVectorLease, fixed_icr_words, startup_icr_words, vector_has_registration_authority,
        vector_index, vector_is_valid,
    };

    #[test]
    fn msi_vector_pool_excludes_exceptions_pic_and_spurious_vectors() {
        assert!(!vector_is_valid(0x1f));
        assert_eq!(vector_index(0x1f), None);
        assert!(vector_is_valid(MSI_VECTOR_FIRST));
        assert!(vector_is_valid(MSI_VECTOR_LAST));
        assert!(!vector_is_valid(0xe0));
        assert_eq!(vector_index(0xe0), None);
    }

    #[test]
    fn unallocated_vector_has_no_registration_authority() {
        assert!(!vector_has_registration_authority(MSI_VECTOR_FIRST, false));
        assert!(vector_has_registration_authority(MSI_VECTOR_FIRST, true));
        assert!(!vector_has_registration_authority(0x20, true));
    }

    #[test]
    fn failed_unpublished_vector_lease_revokes_exact_handler_and_slot() {
        fn handler(_vector: u8) {}

        let mut lease = MsiVectorLease::allocate().expect("bounded test vector");
        let index = vector_index(lease.vector()).expect("allocated vector index");
        assert!(ALLOCATED[index].load(Ordering::Acquire));
        assert!(lease.register_handler(handler));
        assert_eq!(
            HANDLERS[index].load(Ordering::Acquire),
            handler as *const () as usize
        );

        drop(lease);
        assert_eq!(HANDLERS[index].load(Ordering::Acquire), 0);
        assert!(!ALLOCATED[index].load(Ordering::Acquire));
    }

    #[test]
    fn committed_vector_remains_revocable_until_permanent_publication() {
        fn handler(_vector: u8) {}

        let mut lease = MsiVectorLease::allocate().expect("bounded test vector");
        let index = vector_index(lease.vector()).expect("allocated vector index");
        assert!(lease.register_handler(handler));
        let committed = lease.commit();
        assert_eq!(committed.vector(), MSI_VECTOR_FIRST + index as u8);
        assert!(ALLOCATED[index].load(Ordering::Acquire));

        drop(committed);
        assert_eq!(HANDLERS[index].load(Ordering::Acquire), 0);
        assert!(!ALLOCATED[index].load(Ordering::Acquire));
    }

    #[test]
    fn startup_ipi_sequence_uses_exact_destination_and_vector() {
        let (destination, init_assert, init_deassert, sipi) =
            startup_icr_words(7, 8).expect("valid xAPIC startup");
        assert_eq!(destination, 7 << 24);
        assert_eq!(
            init_assert,
            APIC_DELIVERY_MODE_INIT | APIC_ICR_LEVEL_ASSERT | APIC_ICR_TRIGGER_LEVEL
        );
        assert_eq!(
            init_deassert,
            APIC_DELIVERY_MODE_INIT | APIC_ICR_TRIGGER_LEVEL
        );
        assert_eq!(sipi, APIC_DELIVERY_MODE_STARTUP | 8);
        assert!(startup_icr_words(256, 8).is_none());
        assert!(startup_icr_words(7, 0).is_none());
    }

    #[test]
    fn fixed_reschedule_ipi_uses_exact_destination_and_private_vector() {
        let vector = super::super::idt::RESCHEDULE_IPI_VECTOR;
        let (destination, command) = fixed_icr_words(0x5a, vector).expect("valid reschedule IPI");
        assert_eq!(destination, 0x5a00_0000);
        assert_eq!(
            command,
            APIC_DELIVERY_MODE_FIXED | APIC_ICR_LEVEL_ASSERT | u32::from(vector)
        );
        assert!(fixed_icr_words(0x100, vector).is_none());
        assert!(fixed_icr_words(1, MSI_VECTOR_FIRST).is_none());
        assert!(fixed_icr_words(1, 0xff).is_none());
        assert!(fixed_icr_words(1, super::super::idt::TLB_SHOOTDOWN_IPI_VECTOR).is_some());
    }
}
