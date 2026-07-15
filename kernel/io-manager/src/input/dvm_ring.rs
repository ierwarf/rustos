// RING3-MIGRATION-REFERENCE START: DVM input transport substrate.
// L0 is the sole producer of this fixed ivshmem ring. Ring0 maps the exact
// launch-created aperture, arms one MSI-X wake vector, and drains bounded
// records only for inputd's capability-gated broker. inputd retains all input
// policy, translation, modifier state, and client-read ownership.
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering, fence};

use driver_domain_protocol::{
    DVM_INPUT_RING_APERTURE_BYTES, DVM_INPUT_RING_CONSUMER_OFFSET,
    DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY, DVM_INPUT_RING_FLAG_RUSTOS_READY,
    DVM_INPUT_RING_FLAGS_OFFSET, DVM_INPUT_RING_PRODUCER_OFFSET, DVM_INPUT_RING_RECORD_BYTES,
    DVM_INPUT_RING_SLOT_COUNT, DvmInputRingHeader,
};

const IVSHMEM_VENDOR_ID: u16 = 0x1af4;
const IVSHMEM_DEVICE_ID: u16 = 0x1110;
const IVSHMEM_REGISTERS_BAR: usize = 0;
const IVSHMEM_SHARED_MEMORY_BAR: usize = 2;
const INPUT_RING_MSIX_VECTOR_COUNT: u16 = 1;
const MSIX_ENTRY_BYTES: usize = 16;
const MSIX_ENTRY_ADDRESS_LOW_OFFSET: usize = 0;
const MSIX_ENTRY_ADDRESS_HIGH_OFFSET: usize = 4;
const MSIX_ENTRY_DATA_OFFSET: usize = 8;
const MSIX_ENTRY_VECTOR_CONTROL_OFFSET: usize = 12;
const MSIX_ENTRY_VECTOR_MASKED: u32 = 1;
const MAX_RECORDS_PER_BROKER_TURN: u64 = 256;
const MAX_ATTACH_ATTEMPTS_PER_BOOT: u8 = 8;

static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Serialize first attachment and recovery attachment. A concurrent init and
/// inputd broker turn must never reserve two permanent MSI vectors.
static INSTALLING: AtomicBool = AtomicBool::new(false);
// MSI vectors are deliberately permanent once allocated. If a discovered ring
// fails exact MSI-X arming, retrying the same malformed topology would only
// leak vectors; hold it fail-closed until the next boot instead.
static INSTALL_REJECTED: AtomicBool = AtomicBool::new(false);
static INSTALL_REJECTION: AtomicU8 = AtomicU8::new(INSTALL_REJECTION_NONE);
static INSTALL_REJECTION_REPORTED: AtomicBool = AtomicBool::new(false);
static DISCOVERY_REJECTION: AtomicU8 = AtomicU8::new(DISCOVERY_REJECTION_NONE);
static DISCOVERY_IVSHMEM_CANDIDATES: AtomicUsize = AtomicUsize::new(0);
static DISCOVERY_EXACT_APERTURES: AtomicUsize = AtomicUsize::new(0);
static DISCOVERY_EXACT_APERTURE_START: AtomicU64 = AtomicU64::new(0);
/// Both first attach and post-revoke recovery are bounded per boot. A missing
/// or malformed provider must not turn every input poll into a PCI/MMIO probe.
static ATTACH_ATTEMPTS: AtomicU8 = AtomicU8::new(0);
/// A successful revoke may reattach only to the same launch-created PCI
/// aperture. Its fixed leaf vector remains registered and is reused; a new
/// exact-looking ivshmem function is not allowed to inherit that authority.
static MSIX_ARMED: AtomicBool = AtomicBool::new(false);
static ARMED_RESOURCE_START: AtomicU64 = AtomicU64::new(0);
static IRQ_PENDING: AtomicBool = AtomicBool::new(false);
static SHARED_ADDR: AtomicUsize = AtomicUsize::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static CONSUMER: AtomicU64 = AtomicU64::new(0);

const INSTALL_REJECTION_NONE: u8 = 0;
const INSTALL_REJECTION_ATTACH_BUDGET: u8 = 1;
const INSTALL_REJECTION_APERTURE_CHANGED: u8 = 2;
const INSTALL_REJECTION_MSIX_CAPABILITY: u8 = 3;
const INSTALL_REJECTION_MSIX_VECTOR_COUNT: u8 = 4;
const INSTALL_REJECTION_MSIX_TABLE_RESOURCE: u8 = 5;
const INSTALL_REJECTION_MSIX_TABLE_LENGTH: u8 = 6;
const INSTALL_REJECTION_MSIX_TABLE_OFFSET: u8 = 7;
const INSTALL_REJECTION_MSIX_TABLE_BOUNDS: u8 = 8;
const INSTALL_REJECTION_VECTOR_ALLOCATION: u8 = 9;
const INSTALL_REJECTION_HANDLER_REGISTRATION: u8 = 10;
const INSTALL_REJECTION_MESSAGE: u8 = 11;
const INSTALL_REJECTION_TABLE_MAPPING: u8 = 12;

const DISCOVERY_REJECTION_NONE: u8 = 0;
const DISCOVERY_REJECTION_NO_IVSHMEM: u8 = 1;
const DISCOVERY_REJECTION_SHARED_BAR: u8 = 2;
const DISCOVERY_REJECTION_REGISTER_BAR: u8 = 3;
const DISCOVERY_REJECTION_APERTURE_GEOMETRY: u8 = 4;
const DISCOVERY_REJECTION_REGISTER_GEOMETRY: u8 = 5;
const DISCOVERY_REJECTION_LENGTH: u8 = 6;
const DISCOVERY_REJECTION_MAPPING: u8 = 7;
const DISCOVERY_REJECTION_HEADER: u8 = 8;
const DISCOVERY_REJECTION_REGION: u8 = 9;

struct MappedInputRing {
    device: crate::arch::pci::PciDevice,
    resource_start: u64,
    mapped: *mut u8,
    header: DvmInputRingHeader,
}

pub(crate) fn init() {
    let _ = try_install();
}

fn try_install() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    if INSTALL_REJECTED.load(Ordering::Acquire) {
        report_install_rejection_once();
        return false;
    }
    if INSTALLING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let installed = try_install_serialized();
    INSTALLING.store(false, Ordering::Release);
    installed
}

fn try_install_serialized() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    if INSTALL_REJECTED.load(Ordering::Acquire) {
        report_install_rejection_once();
        return false;
    }
    if !consume_attach_attempt() {
        reject_install(INSTALL_REJECTION_ATTACH_BUDGET);
        return false;
    }
    let Some(ring) = find_input_ring() else {
        return false;
    };
    if MSIX_ARMED.load(Ordering::Acquire) {
        if ARMED_RESOURCE_START.load(Ordering::Acquire) != ring.resource_start {
            release_mapping(ring.mapped);
            reject_install(INSTALL_REJECTION_APERTURE_CHANGED);
            return false;
        }
    } else {
        if let Err(rejection) = arm_input_ring_interrupt(ring.device) {
            release_mapping(ring.mapped);
            reject_install(rejection);
            return false;
        }
        ARMED_RESOURCE_START.store(ring.resource_start, Ordering::Release);
        MSIX_ARMED.store(true, Ordering::Release);
    }
    let flags = read_u32(ring.mapped, DVM_INPUT_RING_FLAGS_OFFSET)
        & !DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY;
    write_u32(
        ring.mapped,
        DVM_INPUT_RING_FLAGS_OFFSET,
        flags | DVM_INPUT_RING_FLAG_RUSTOS_READY,
    );
    fence(Ordering::SeqCst);
    SHARED_ADDR.store(ring.mapped as usize, Ordering::Release);
    GENERATION.store(ring.header.generation, Ordering::Release);
    CONSUMER.store(ring.header.consumer, Ordering::Release);
    INSTALLED.store(true, Ordering::Release);
    crate::debug::info!(
        input,
        "DVM input ring ready: fixed {}-slot MSI-X ingress",
        DVM_INPUT_RING_SLOT_COUNT
    );
    true
}

/// Admit DVM production only after a real input client has successfully
/// reached `inputd` through the policy-backed poll path. The transport-ready
/// flag above proves only that MSI-X is armed; it does not prove that anyone
/// will advance the consumer cursor.
pub(crate) fn mark_policy_consumer_ready() -> bool {
    if !INSTALLED.load(Ordering::Acquire) {
        return false;
    }
    let mapped = SHARED_ADDR.load(Ordering::Acquire) as *mut u8;
    if mapped.is_null() {
        return false;
    }
    let flags = read_u32(mapped, DVM_INPUT_RING_FLAGS_OFFSET);
    if flags & DVM_INPUT_RING_FLAG_RUSTOS_READY == 0 {
        return false;
    }
    write_u32(
        mapped,
        DVM_INPUT_RING_FLAGS_OFFSET,
        flags | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY,
    );
    fence(Ordering::SeqCst);
    true
}

/// Drain a bounded batch only on inputd's broker call. The interrupt callback
/// merely wakes a sleeping poll waiter; it cannot parse a DVM frame or acquire
/// the decoder lock.
pub(crate) fn service_pending() -> usize {
    if !INSTALLED.load(Ordering::Acquire) && !try_install() {
        return 0;
    }
    let mapped = SHARED_ADDR.load(Ordering::Acquire) as *mut u8;
    if mapped.is_null() {
        return 0;
    }
    let _ = IRQ_PENDING.swap(false, Ordering::AcqRel);
    let Some(header) = read_header(mapped) else {
        revoke("header-invalid");
        return 0;
    };
    let generation = GENERATION.load(Ordering::Acquire);
    let mut consumer = CONSUMER.load(Ordering::Acquire);
    if header.generation != generation
        || header.consumer != consumer
        || header.producer < consumer
        || header.producer.saturating_sub(consumer) > u64::from(DVM_INPUT_RING_SLOT_COUNT)
    {
        revoke("cursor-or-generation-invalid");
        return 0;
    }
    // Pairs with L0's release fence before it advances `producer`. No record
    // bytes may be observed before the validated cursor becomes visible.
    fence(Ordering::Acquire);
    let available = header.producer - consumer;
    let count = available.min(MAX_RECORDS_PER_BROKER_TURN);
    let mut accepted = 0;
    for _ in 0..count {
        let Some(offset) = DvmInputRingHeader::record_offset(consumer)
            .try_into()
            .ok()
            .filter(|offset: &usize| {
                offset
                    .checked_add(DVM_INPUT_RING_RECORD_BYTES)
                    .is_some_and(|end| end <= DVM_INPUT_RING_APERTURE_BYTES as usize)
            })
        else {
            revoke("record-offset-invalid");
            return accepted;
        };
        let mut record = [0_u8; DVM_INPUT_RING_RECORD_BYTES];
        for (index, byte) in record.iter_mut().enumerate() {
            *byte = unsafe { mapped.add(offset + index).read_volatile() };
        }
        accepted += super::dvm_frames::consume_record(&record);
        consumer = consumer.saturating_add(1);
    }
    fence(Ordering::Release);
    unsafe {
        mapped
            .add(DVM_INPUT_RING_CONSUMER_OFFSET)
            .cast::<u64>()
            .write_volatile(consumer.to_le());
    }
    CONSUMER.store(consumer, Ordering::Release);
    accepted
}

/// Read-only poll recheck for the arm/recheck/commit protocol. An MSI-X edge
/// can arrive after inputd's STATS request drained the previous batch but
/// before the caller registers as an input waiter. Readiness must therefore
/// include the raw producer/consumer state, not only the decoded ingress
/// queue; otherwise that edge is lost and a finite poll can sleep forever.
pub(crate) fn has_pending_records() -> bool {
    if IRQ_PENDING.load(Ordering::Acquire) {
        return true;
    }
    if !INSTALLED.load(Ordering::Acquire) {
        return false;
    }
    let mapped = SHARED_ADDR.load(Ordering::Acquire) as *mut u8;
    if mapped.is_null() {
        return false;
    }
    let Some(header) = read_header(mapped) else {
        // Force a broker turn to perform the authoritative revoke rather than
        // letting a malformed live transport strand a sleeping reader.
        return true;
    };
    let generation = GENERATION.load(Ordering::Acquire);
    let consumer = CONSUMER.load(Ordering::Acquire);
    header.generation != generation
        || header.consumer != consumer
        || header.producer < consumer
        || header.producer.saturating_sub(consumer) > u64::from(DVM_INPUT_RING_SLOT_COUNT)
        || header.producer != consumer
}

fn input_ring_interrupt(_vector: u8) {
    IRQ_PENDING.store(true, Ordering::Release);
    // This is a wake-only leaf. It does not inspect shared bytes, consume a
    // record, allocate, or execute input policy in interrupt context.
    super::event_queue::wake_input_waiters();
}

fn revoke(reason: &str) {
    INSTALLED.store(false, Ordering::Release);
    let mapped = SHARED_ADDR.swap(0, Ordering::AcqRel) as *mut u8;
    if !mapped.is_null() {
        let flags = read_u32(mapped, DVM_INPUT_RING_FLAGS_OFFSET);
        write_u32(
            mapped,
            DVM_INPUT_RING_FLAGS_OFFSET,
            flags & !(DVM_INPUT_RING_FLAG_RUSTOS_READY | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY),
        );
        fence(Ordering::SeqCst);
    }
    release_mapping(mapped);
    IRQ_PENDING.store(false, Ordering::Release);
    // L0 can no longer deliver authenticated releases after revocation, so
    // revoke decoder/network authority and force a policy-visible reset
    // locally. Otherwise a malformed cursor/header could leave a key or
    // pointer button logically pressed.
    super::dvm_frames::revoke_active_session();
    crate::debug::warn!(input, "dvm-input-ring: transport revoked reason={}", reason);
}

fn reject_install(reason: u8) {
    INSTALL_REJECTION.store(reason, Ordering::Release);
    INSTALL_REJECTED.store(true, Ordering::Release);
    crate::debug::warn!(
        input,
        "dvm-input-ring: transport rejected reason={}",
        install_rejection_name(reason)
    );
}

fn report_install_rejection_once() {
    if INSTALL_REJECTION_REPORTED.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::debug::println_emergency(format_args!(
        "DVM input ring rejected: reason={} discovery={} ivshmem_candidates={} exact_apertures={} aperture_start={:#x}",
        install_rejection_name(INSTALL_REJECTION.load(Ordering::Acquire)),
        discovery_rejection_name(DISCOVERY_REJECTION.load(Ordering::Acquire)),
        DISCOVERY_IVSHMEM_CANDIDATES.load(Ordering::Acquire),
        DISCOVERY_EXACT_APERTURES.load(Ordering::Acquire),
        DISCOVERY_EXACT_APERTURE_START.load(Ordering::Acquire),
    ));
    crate::debug::warn!(
        input,
        "dvm-input-ring: persistent rejection reported to runtime"
    );
}

fn discovery_rejection_name(reason: u8) -> &'static str {
    match reason {
        DISCOVERY_REJECTION_NO_IVSHMEM => "no-ivshmem-device",
        DISCOVERY_REJECTION_SHARED_BAR => "missing-shared-memory-bar",
        DISCOVERY_REJECTION_REGISTER_BAR => "missing-register-bar",
        DISCOVERY_REJECTION_APERTURE_GEOMETRY => "aperture-geometry-mismatch",
        DISCOVERY_REJECTION_REGISTER_GEOMETRY => "register-bar-geometry-mismatch",
        DISCOVERY_REJECTION_LENGTH => "aperture-length-overflow",
        DISCOVERY_REJECTION_MAPPING => "aperture-map-failed",
        DISCOVERY_REJECTION_HEADER => "header-invalid",
        DISCOVERY_REJECTION_REGION => "header-region-mismatch",
        _ => "unknown-discovery-rejection",
    }
}

fn install_rejection_name(reason: u8) -> &'static str {
    match reason {
        INSTALL_REJECTION_ATTACH_BUDGET => "attach-retry-budget-exhausted",
        INSTALL_REJECTION_APERTURE_CHANGED => "recovery-aperture-changed",
        INSTALL_REJECTION_MSIX_CAPABILITY => "msix-capability-missing",
        INSTALL_REJECTION_MSIX_VECTOR_COUNT => "msix-vector-count-mismatch",
        INSTALL_REJECTION_MSIX_TABLE_RESOURCE => "msix-table-resource-invalid",
        INSTALL_REJECTION_MSIX_TABLE_LENGTH => "msix-table-length-overflow",
        INSTALL_REJECTION_MSIX_TABLE_OFFSET => "msix-table-offset-overflow",
        INSTALL_REJECTION_MSIX_TABLE_BOUNDS => "msix-table-out-of-bounds",
        INSTALL_REJECTION_VECTOR_ALLOCATION => "msi-vector-allocation-failed",
        INSTALL_REJECTION_HANDLER_REGISTRATION => "msi-handler-registration-failed",
        INSTALL_REJECTION_MESSAGE => "msi-message-unavailable",
        INSTALL_REJECTION_TABLE_MAPPING => "msix-table-map-failed",
        _ => "unknown-install-rejection",
    }
}

fn consume_attach_attempt() -> bool {
    ATTACH_ATTEMPTS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |attempts| {
            (attempts < MAX_ATTACH_ATTEMPTS_PER_BOOT).then_some(attempts + 1)
        })
        .is_ok()
}

fn release_mapping(mapped: *mut u8) {
    if !mapped.is_null() {
        crate::driver::mmio::unmap(mapped.cast());
    }
}

fn find_input_ring() -> Option<MappedInputRing> {
    let mut found = None;
    let mut ivshmem_candidates = 0_u32;
    let mut exact_aperture_candidates = 0_u32;
    let mut last_rejection = DISCOVERY_REJECTION_NO_IVSHMEM;
    let mut exact_aperture_rejection = DISCOVERY_REJECTION_NONE;
    crate::arch::pci::visit_devices(|device| {
        if device.vendor_id() != IVSHMEM_VENDOR_ID || device.device_id() != IVSHMEM_DEVICE_ID {
            return false;
        }
        ivshmem_candidates = ivshmem_candidates.saturating_add(1);
        let Some(resource) = device.resource(IVSHMEM_SHARED_MEMORY_BAR) else {
            last_rejection = DISCOVERY_REJECTION_SHARED_BAR;
            return false;
        };
        let Some(registers) = device.resource(IVSHMEM_REGISTERS_BAR) else {
            last_rejection = DISCOVERY_REJECTION_REGISTER_BAR;
            return false;
        };
        if resource.is_io || resource.size != DVM_INPUT_RING_APERTURE_BYTES {
            last_rejection = DISCOVERY_REJECTION_APERTURE_GEOMETRY;
            return false;
        }
        exact_aperture_candidates = exact_aperture_candidates.saturating_add(1);
        DISCOVERY_EXACT_APERTURE_START.store(resource.start, Ordering::Release);
        if registers.is_io || registers.size < size_of::<u32>() as u64 {
            last_rejection = DISCOVERY_REJECTION_REGISTER_GEOMETRY;
            exact_aperture_rejection = last_rejection;
            return false;
        }
        let Ok(resource_len) = usize::try_from(resource.size) else {
            last_rejection = DISCOVERY_REJECTION_LENGTH;
            exact_aperture_rejection = last_rejection;
            return false;
        };
        let mapped = crate::driver::mmio::map(resource.start, resource_len, true).cast::<u8>();
        if mapped.is_null() {
            last_rejection = DISCOVERY_REJECTION_MAPPING;
            exact_aperture_rejection = last_rejection;
            return false;
        }
        let Some(header) = read_header(mapped) else {
            release_mapping(mapped);
            last_rejection = DISCOVERY_REJECTION_HEADER;
            exact_aperture_rejection = last_rejection;
            return false;
        };
        if header.region_bytes != resource.size {
            release_mapping(mapped);
            last_rejection = DISCOVERY_REJECTION_REGION;
            exact_aperture_rejection = last_rejection;
            return false;
        }
        found = Some(MappedInputRing {
            device,
            resource_start: resource.start,
            mapped,
            header,
        });
        true
    });
    if found.is_none() {
        let rejection = if exact_aperture_candidates == 0 {
            last_rejection
        } else {
            exact_aperture_rejection
        };
        DISCOVERY_REJECTION.store(rejection, Ordering::Release);
        DISCOVERY_IVSHMEM_CANDIDATES.store(ivshmem_candidates as usize, Ordering::Release);
        DISCOVERY_EXACT_APERTURES.store(exact_aperture_candidates as usize, Ordering::Release);
        crate::debug::warn!(
            input,
            "DVM input ring unavailable: ivshmem_candidates={} exact_apertures={} reason={}",
            ivshmem_candidates,
            exact_aperture_candidates,
            discovery_rejection_name(rejection),
        );
    }
    found
}

fn read_header(mapped: *const u8) -> Option<DvmInputRingHeader> {
    let mut bytes = [0_u8; DvmInputRingHeader::encoded_len()];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { mapped.add(index).read_volatile() };
    }
    // Geometry is immutable for the boot, but the two cursors are independent
    // concurrent writers. Do not validate a byte-by-byte snapshot of a cursor:
    // a valid aligned u64 update could otherwise look torn and permanently
    // revoke the input transport under sustained pointer traffic.
    bytes[DVM_INPUT_RING_PRODUCER_OFFSET..DVM_INPUT_RING_PRODUCER_OFFSET + size_of::<u64>()]
        .fill(0);
    bytes[DVM_INPUT_RING_CONSUMER_OFFSET..DVM_INPUT_RING_CONSUMER_OFFSET + size_of::<u64>()]
        .fill(0);
    let mut header = DvmInputRingHeader::decode(&bytes)?;
    header.producer = read_u64(mapped, DVM_INPUT_RING_PRODUCER_OFFSET);
    header.consumer = read_u64(mapped, DVM_INPUT_RING_CONSUMER_OFFSET);
    header.is_valid().then_some(header)
}

fn arm_input_ring_interrupt(device: crate::arch::pci::PciDevice) -> Result<(), u8> {
    let capability = device
        .msix_capability()
        .ok_or(INSTALL_REJECTION_MSIX_CAPABILITY)?;
    if capability.table_entries() != INPUT_RING_MSIX_VECTOR_COUNT {
        return Err(INSTALL_REJECTION_MSIX_VECTOR_COUNT);
    }
    let table_resource = capability
        .table_resource(device)
        .ok_or(INSTALL_REJECTION_MSIX_TABLE_RESOURCE)?;
    let table_len =
        usize::try_from(table_resource.size).map_err(|_| INSTALL_REJECTION_MSIX_TABLE_LENGTH)?;
    let table_offset = usize::try_from(capability.table_offset())
        .map_err(|_| INSTALL_REJECTION_MSIX_TABLE_OFFSET)?;
    if table_offset
        .checked_add(MSIX_ENTRY_BYTES)
        .is_none_or(|end| end > table_len)
    {
        return Err(INSTALL_REJECTION_MSIX_TABLE_BOUNDS);
    }
    capability.set_function_masked(device, true);
    capability.set_enabled(device, false);
    let vector = crate::arch::msi::allocate_vector().ok_or(INSTALL_REJECTION_VECTOR_ALLOCATION)?;
    if !crate::arch::msi::register_handler(vector, input_ring_interrupt) {
        return Err(INSTALL_REJECTION_HANDLER_REGISTRATION);
    }
    let message = crate::arch::msi::message_for(vector).ok_or(INSTALL_REJECTION_MESSAGE)?;
    let table = crate::driver::mmio::map(table_resource.start, table_len, false).cast::<u8>();
    if table.is_null() {
        return Err(INSTALL_REJECTION_TABLE_MAPPING);
    }
    unsafe {
        program_msix_entry(table.add(table_offset), message);
        fence(Ordering::SeqCst);
        table
            .add(table_offset + MSIX_ENTRY_VECTOR_CONTROL_OFFSET)
            .cast::<u32>()
            .write_volatile(0);
    }
    fence(Ordering::SeqCst);
    capability.set_enabled(device, true);
    capability.set_function_masked(device, false);
    Ok(())
}

unsafe fn program_msix_entry(entry: *mut u8, message: crate::arch::msi::MsiMessage) {
    unsafe {
        entry
            .add(MSIX_ENTRY_VECTOR_CONTROL_OFFSET)
            .cast::<u32>()
            .write_volatile(MSIX_ENTRY_VECTOR_MASKED.to_le());
        entry
            .add(MSIX_ENTRY_ADDRESS_LOW_OFFSET)
            .cast::<u32>()
            .write_volatile((message.address as u32).to_le());
        entry
            .add(MSIX_ENTRY_ADDRESS_HIGH_OFFSET)
            .cast::<u32>()
            .write_volatile(((message.address >> 32) as u32).to_le());
        entry
            .add(MSIX_ENTRY_DATA_OFFSET)
            .cast::<u32>()
            .write_volatile(message.data.to_le());
    }
}

fn read_u32(base: *const u8, offset: usize) -> u32 {
    unsafe { u32::from_le(base.add(offset).cast::<u32>().read_volatile()) }
}

fn read_u64(base: *const u8, offset: usize) -> u64 {
    unsafe { u64::from_le(base.add(offset).cast::<u64>().read_volatile()) }
}

fn write_u32(base: *mut u8, offset: usize, value: u32) {
    unsafe { base.add(offset).cast::<u32>().write_volatile(value.to_le()) }
}
