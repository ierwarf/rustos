//! Single-consumer DVM input-ring transport and generation lease.
//!
//! - **Owner:** `kernel-io-manager` owns bounded ingress mechanics; `inputd`
//!   owns decode, focus/session policy, and delivery.
//! - **Boundary:** Shared cursors, sequence, checksum-bearing records, and
//!   consumer identity are untrusted.
//! - **Lifecycle:** Install transport, grant one exact consumer generation,
//!   drain bounded records, revoke/withdraw, and reject old-owner access.
//! - **Concurrency:** MSI-X only advances pending/wake state; normal context
//!   performs bounded copies under explicit producer/consumer ordering.
//! - **Failure:** Malformed record, overrun, owner exit, capacity, and stale
//!   generation terminate or withdraw the exact lease.
//! - **Forbidden:** No decode, focus policy, native-device fallback, multiple
//!   consumers, or polling loop in ring0.
//! - **Evidence:** `dvm-input-ingress` and `input-delivery-lifecycle`.
// RING3-MIGRATION-REFERENCE START: DVM input transport substrate.
// L0 is the sole producer of this fixed ivshmem ring. Ring0 maps the exact
// launch-created aperture, arms one MSI-X wake vector, and drains bounded
// records only for inputd's capability-gated broker. inputd retains all input
// policy, translation, modifier state, and client-read ownership.
use core::mem::{align_of, size_of};
use core::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

use driver_domain_protocol::{
    DVM_INPUT_RING_APERTURE_BYTES, DVM_INPUT_RING_CONSUMER_OFFSET,
    DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET, DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY,
    DVM_INPUT_RING_FLAG_RUSTOS_READY, DVM_INPUT_RING_FLAGS_OFFSET, DVM_INPUT_RING_PRODUCER_OFFSET,
    DVM_INPUT_RING_RECORD_BYTES, DVM_INPUT_RING_SLOT_COUNT, DvmInputRingHeader,
};
use rustos_user_abi::syscall::{
    INPUTD_DVM_RECORD_BYTES, INPUTD_DVM_RECORD_FLAG_RESET, InputDvmRecordWire,
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

/// Every shared control word is addressed through `Atomic*::from_ptr`; the
/// encoded ring layout remains bytes-on-the-wire rather than a Rust struct.
const fn input_ring_atomic_control_layout_is_valid() -> bool {
    DVM_INPUT_RING_FLAGS_OFFSET % align_of::<AtomicU32>() == 0
        && DVM_INPUT_RING_PRODUCER_OFFSET % align_of::<AtomicU64>() == 0
        && DVM_INPUT_RING_CONSUMER_OFFSET % align_of::<AtomicU64>() == 0
        && DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET % align_of::<AtomicU64>() == 0
        && DVM_INPUT_RING_FLAGS_OFFSET + size_of::<AtomicU32>() <= DVM_INPUT_RING_PRODUCER_OFFSET
        && DVM_INPUT_RING_PRODUCER_OFFSET + size_of::<AtomicU64>() <= DVM_INPUT_RING_CONSUMER_OFFSET
        && DVM_INPUT_RING_CONSUMER_OFFSET + size_of::<AtomicU64>()
            <= DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET
        && DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET + size_of::<AtomicU64>()
            <= DvmInputRingHeader::encoded_len()
        && size_of::<AtomicU32>() == size_of::<u32>()
        && size_of::<AtomicU64>() == size_of::<u64>()
}

const _: () = assert!(input_ring_atomic_control_layout_is_valid());

const fn shared_control_load_order() -> Ordering {
    Ordering::Acquire
}

const fn shared_control_publish_order() -> Ordering {
    Ordering::Release
}

const fn shared_control_update_order() -> Ordering {
    Ordering::AcqRel
}

const fn shared_control_update_failure_order() -> Ordering {
    Ordering::Acquire
}

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
static CONSUMER_WAKE_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Non-zero when inputd must observe a revocation barrier before any record
/// from a later transport generation. The broker consumes this atomically.
static RESET_PENDING_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Process capabilities are intentionally process-wide, so two authorized
/// threads can enter the broker concurrently. The shared consumer cursor has
/// exactly one linearization owner per drain turn.
static DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static INPUT_LIFECYCLE: crate::transport_lifecycle::TransportLifecycle =
    crate::transport_lifecycle::TransportLifecycle::detached();
static WITHDRAW_PENDING: AtomicBool = AtomicBool::new(false);
static REVOKE_PENDING: AtomicBool = AtomicBool::new(false);
static BROKER_CALLS: AtomicU64 = AtomicU64::new(0);
static RECORDS_COPIED: AtomicU64 = AtomicU64::new(0);
// Bounded drain observability. The host relay fails closed when the ring stays
// full past its credit window, and total record counts cannot distinguish a
// stalled consumer from one that is merely behind. These record how deep the
// backlog actually got and how often a turn lost the single-flight claim, so
// the failure is attributable without re-running with ad-hoc probes.
static OUTSTANDING_HIGH_WATER: AtomicU64 = AtomicU64::new(0);
static OUTSTANDING_HIGH_WATER_REPORTED: AtomicU64 = AtomicU64::new(0);
static DRAIN_CLAIM_LOST: AtomicU64 = AtomicU64::new(0);
static MAPPING_CLAIM_FAILURES: AtomicU64 = AtomicU64::new(0);
static READINESS_POLLS: AtomicU64 = AtomicU64::new(0);
/// Report only when the backlog high-water grows by a full broker turn, which
/// bounds output to at most one line per turn-sized step of the ring.
const OUTSTANDING_REPORT_STEP: u64 = MAX_RECORDS_PER_BROKER_TURN;
static REVOKE_COUNT: AtomicU64 = AtomicU64::new(0);
/// Turns skipped because the aperture prefix was not published yet. Counted so
/// a persistent unpublished region is visible instead of looking like an idle
/// ring.
static UNPUBLISHED_HEADER_TURNS: AtomicU64 = AtomicU64::new(0);

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
const INSTALL_REJECTION_DEVICE_CLAIM: u8 = 13;

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

struct DrainGuard;

impl Drop for DrainGuard {
    fn drop(&mut self) {
        // ORDERING: Release publishes every cursor/reset mutation before a
        // later broker caller may become the sole drain owner.
        DRAIN_IN_PROGRESS.store(false, Ordering::Release);
        finish_pending_lifecycle();
    }
}

fn try_claim_drain(owner: &AtomicBool) -> bool {
    // ORDERING: AcqRel is the single-consumer admission edge; Acquire on
    // failure observes that another caller retains cursor/reset custody.
    owner
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

struct MappedInputRing {
    device: crate::arch::pci::PciDevice,
    resource_start: u64,
    mapped: *mut u8,
    header: DvmInputRingHeader,
}

/// The shared aperture address is usable only while this exact lifecycle
/// generation holds an admitted claim. The drain owner may inspect and unmap
/// the pointer after `finish_drain`, but ordinary broker/readiness paths must
/// never load or dereference `SHARED_ADDR` without this guard.
struct TransportMappingClaim<'a> {
    lifecycle: Option<crate::transport_lifecycle::TransportClaim<'a>>,
    generation: u64,
    mapped: *mut u8,
}

impl TransportMappingClaim<'_> {
    fn mapped(&self) -> *mut u8 {
        self.mapped
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn validate_current(&self) -> bool {
        self.lifecycle
            .as_ref()
            .is_some_and(crate::transport_lifecycle::TransportClaim::validate_current)
    }

    fn epoch(&self) -> u64 {
        self.lifecycle
            .as_ref()
            .map_or(0, crate::transport_lifecycle::TransportClaim::epoch)
    }
}

impl Drop for TransportMappingClaim<'_> {
    fn drop(&mut self) {
        // Release the aperture claim before asking the transport-specific
        // drain owner to observe zero claims and unmap the retired generation.
        drop(self.lifecycle.take());
        finish_pending_lifecycle();
    }
}

fn claim_installed_mapping() -> Option<TransportMappingClaim<'static>> {
    if !INSTALLED.load(Ordering::Acquire) {
        return None;
    }
    let generation = GENERATION.load(Ordering::Acquire);
    let lifecycle = INPUT_LIFECYCLE.try_claim(generation)?;
    let mapped = SHARED_ADDR.load(Ordering::Acquire) as *mut u8;
    if mapped.is_null()
        || !INSTALLED.load(Ordering::Acquire)
        || GENERATION.load(Ordering::Acquire) != generation
        || !lifecycle.validate_current()
    {
        return None;
    }
    Some(TransportMappingClaim {
        lifecycle: Some(lifecycle),
        generation,
        mapped,
    })
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
    let Some(activation) = INPUT_LIFECYCLE.activate_begin(ring.header.generation) else {
        release_mapping(ring.mapped);
        return false;
    };
    let (previous_flags, _) = update_shared_flags(ring.mapped, |flags| {
        Some(
            (flags & !DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY) | DVM_INPUT_RING_FLAG_RUSTOS_READY,
        )
    })
    .expect("fixed input-ring flag update is unconditional");
    // ORDERING: AcqRel reads the L0-published READY state and releases this
    // RustOS-ready/policy-clear transition to the producer before activation.
    // Install requires the next policy owner to re-publish, so it clears the
    // policy bit. On a re-install that bit was set and an L0 producer is live,
    // and the relay reads the cleared window as a terminal revocation.
    if previous_flags & DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY != 0 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Input,
            "dvm-input-policy-cleared-by-install",
            ring.header.generation,
            u64::from(previous_flags),
        );
    }
    SHARED_ADDR.store(ring.mapped as usize, Ordering::Release);
    GENERATION.store(ring.header.generation, Ordering::Release);
    CONSUMER.store(ring.header.consumer, Ordering::Release);
    CONSUMER_WAKE_GENERATION.store(ring.header.consumer_wake_generation, Ordering::Release);
    if !activation.commit() {
        SHARED_ADDR.store(0, Ordering::Release);
        GENERATION.store(0, Ordering::Release);
        CONSUMER.store(0, Ordering::Release);
        CONSUMER_WAKE_GENERATION.store(0, Ordering::Release);
        release_mapping(ring.mapped);
        return false;
    }
    INSTALLED.store(true, Ordering::Release);
    crate::debug::info!(
        input,
        "DVM input ring ready: fixed {}-slot MSI-X ingress",
        DVM_INPUT_RING_SLOT_COUNT
    );
    true
}

/// Admit DVM production only after inputd's capability-gated ingestion worker
/// has armed its kernel waiter. The transport-ready flag above proves only
/// that MSI-X is armed; it does not prove that the sole ring consumer can
/// advance the cursor.
pub(crate) fn mark_policy_consumer_ready() -> bool {
    finish_pending_lifecycle();
    if !INSTALLED.load(Ordering::Acquire) && !try_install() {
        return false;
    }
    let Some(mapping) = claim_installed_mapping() else {
        return false;
    };
    let mapped = mapping.mapped();
    let header = match read_header(mapped) {
        Ok(header) => header,
        Err(HeaderRejection::Unpublished) => {
            UNPUBLISHED_HEADER_TURNS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        Err(HeaderRejection::Invalid) => {
            revoke("policy-ready-header-invalid");
            return false;
        }
    };
    if header.generation != mapping.generation()
        || header.consumer != CONSUMER.load(Ordering::Acquire)
    {
        revoke("policy-ready-lifecycle-invalid");
        return false;
    }
    let flags = load_shared_flags(mapped);
    let Some(admitted_flags) = admitted_policy_ready_flags(flags) else {
        revoke("policy-ready-transport-not-ready");
        return false;
    };
    if admitted_flags == flags {
        return true;
    }
    // A replacement inputd must never inherit records committed for a dead
    // policy owner. The producer is required to stop while the policy-ready
    // bit is clear, but one already-admitted commit may race that withdrawal.
    // Retire all pre-admission records before publishing the new owner.
    if header.producer != header.consumer {
        // ORDERING: Release retires the prior owner's records before the
        // policy-ready AcqRel update admits L0 production for this owner.
        store_shared_u64(
            mapped,
            DVM_INPUT_RING_CONSUMER_OFFSET,
            header.producer,
            shared_control_publish_order(),
        );
        CONSUMER.store(header.producer, Ordering::Release);
        RESET_PENDING_GENERATION.store(header.generation, Ordering::Release);
    }
    if update_shared_flags(mapped, |current| {
        (current & DVM_INPUT_RING_FLAG_RUSTOS_READY != 0)
            .then_some(current | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY)
    })
    .is_none()
    {
        revoke("policy-ready-transport-not-ready");
        return false;
    }
    // ORDERING: AcqRel publishes the exact policy-consumer admission after
    // the retired cursor is visible to the L0 producer.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Input,
        "dvm-input-policy-ready",
        header.generation,
        header.producer,
    );
    true
}

/// Withdraw inputd's policy-consumer lease without reallocating the pinned
/// transport or MSI-X vector.
///
/// Process-exit cleanup invokes this after revoking inputd's service endpoint.
/// Clearing the shared ready bit first stops new L0 admission; advancing the
/// consumer cursor then retires records that no longer have a live semantic
/// owner. A replacement inputd receives a generation-stamped reset barrier
/// before it may publish policy readiness again.
pub(crate) fn withdraw_policy_consumer() {
    if !INSTALLED.load(Ordering::Acquire) {
        super::wait_queue::wake_input_waiters();
        return;
    }
    let Some(mapping) = claim_installed_mapping() else {
        super::wait_queue::wake_input_waiters();
        return;
    };
    let mapped = mapping.mapped();
    let (flags, _) = update_shared_flags(mapped, |current| {
        Some(current & !DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY)
    })
    .expect("fixed input-ring policy withdrawal is unconditional");
    // ORDERING: AcqRel withdraws policy readiness before the lifecycle drain
    // publishes its reset barrier and wakes the former policy consumer.
    let generation = mapping.generation();
    crate::debug::record_milestone(
        crate::debug::LogCategory::Input,
        "dvm-input-policy-withdrawn",
        generation,
        u64::from(flags),
    );
    INPUT_LIFECYCLE.request_drain();
    // ORDERING: the Release pending bit publishes the retired generation and
    // shared-ready withdrawal before any quiescent finisher resets cursors.
    WITHDRAW_PENDING.store(true, Ordering::Release);
    RESET_PENDING_GENERATION.store(generation.max(1), Ordering::Release);
    super::wait_queue::wake_input_waiters();
    drop(mapping);
    wait_for_lifecycle_quiescence("policy-consumer-withdraw");
    crate::debug::warn!(
        input,
        "dvm-input-ring: policy consumer withdrawn generation={}",
        generation
    );
}

fn admitted_policy_ready_flags(flags: u32) -> Option<u32> {
    (flags & DVM_INPUT_RING_FLAG_RUSTOS_READY != 0)
        .then_some(flags | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY)
}

/// Drain a bounded batch only on inputd's broker call. The interrupt callback
/// merely wakes a sleeping poll waiter; it cannot parse a DVM frame or acquire
/// the decoder lock.
pub(crate) fn service_pending(dest: &mut [InputDvmRecordWire]) -> usize {
    BROKER_CALLS.fetch_add(1, Ordering::Relaxed);
    if dest.is_empty() {
        return 0;
    }
    // ORDERING: AcqRel is the one-drain linearization point and observes the
    // prior owner's cursor publication. A losing caller leaves all records and
    // reset authority untouched for the admitted owner or a later turn.
    if !try_claim_drain(&DRAIN_IN_PROGRESS) {
        DRAIN_CLAIM_LOST.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    let _drain_guard = DrainGuard;
    let reset_generation = RESET_PENDING_GENERATION.swap(0, Ordering::AcqRel);
    let mut written = 0;
    if reset_generation != 0 {
        dest[0] = InputDvmRecordWire {
            transport_generation: reset_generation,
            flags: INPUTD_DVM_RECORD_FLAG_RESET,
            len: 0,
            reserved0: 0,
            bytes: [0; INPUTD_DVM_RECORD_BYTES],
        };
        written = 1;
        if written == dest.len() {
            RECORDS_COPIED.fetch_add(1, Ordering::Relaxed);
            return written;
        }
    }
    if !INSTALLED.load(Ordering::Acquire) && !try_install() {
        RECORDS_COPIED.fetch_add(written as u64, Ordering::Relaxed);
        return written;
    }
    let Some(mapping) = claim_installed_mapping() else {
        RECORDS_COPIED.fetch_add(written as u64, Ordering::Relaxed);
        return written;
    };
    let mapped = mapping.mapped();
    let generation = mapping.generation();
    let reset_written = written;
    let _ = IRQ_PENDING.swap(false, Ordering::AcqRel);
    let header = match read_header(mapped) {
        Ok(header) => header,
        Err(HeaderRejection::Unpublished) => {
            UNPUBLISHED_HEADER_TURNS.fetch_add(1, Ordering::Relaxed);
            RECORDS_COPIED.fetch_add(reset_written as u64, Ordering::Relaxed);
            return reset_written;
        }
        Err(HeaderRejection::Invalid) => {
            revoke("header-invalid");
            RECORDS_COPIED.fetch_add(reset_written as u64, Ordering::Relaxed);
            return reset_written;
        }
    };
    let mut consumer = CONSUMER.load(Ordering::Acquire);
    if header.generation != generation
        || header.consumer != consumer
        || header.producer < consumer
        || header.producer.saturating_sub(consumer) > u64::from(DVM_INPUT_RING_SLOT_COUNT)
    {
        revoke("cursor-or-generation-invalid");
        RECORDS_COPIED.fetch_add(reset_written as u64, Ordering::Relaxed);
        return reset_written;
    }
    // ORDERING: the Acquire producer-cursor load in `read_header` pairs with
    // L0's Release cursor publication. No record bytes may be observed before
    // the validated cursor becomes visible.
    let available = header.producer - consumer;
    record_outstanding_high_water(available);
    let capacity = (dest.len() - written) as u64;
    let count = available.min(MAX_RECORDS_PER_BROKER_TURN).min(capacity);
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
            RECORDS_COPIED.fetch_add(reset_written as u64, Ordering::Relaxed);
            return reset_written;
        };
        let mut record = [0_u8; DVM_INPUT_RING_RECORD_BYTES];
        for (index, byte) in record.iter_mut().enumerate() {
            *byte = unsafe { mapped.add(offset + index).read_volatile() };
        }
        dest[written] = InputDvmRecordWire {
            transport_generation: generation,
            flags: 0,
            len: DVM_INPUT_RING_RECORD_BYTES as u16,
            reserved0: 0,
            bytes: record,
        };
        written += 1;
        consumer = consumer.saturating_add(1);
    }
    if !mapping.validate_current() || mapping.epoch() != generation {
        // ORDERING: publish the rejected generation for inputd's reset barrier
        // before this claim is dropped and another epoch may activate.
        RESET_PENDING_GENERATION.store(generation.max(1), Ordering::Release);
        RECORDS_COPIED.fetch_add(reset_written as u64, Ordering::Relaxed);
        return reset_written;
    }
    // ORDERING: Release publishes copied-record ownership retirement before
    // L0 acquires the consumer cursor and reuses the slot for a later frame.
    store_shared_u64(
        mapped,
        DVM_INPUT_RING_CONSUMER_OFFSET,
        consumer,
        shared_control_publish_order(),
    );
    CONSUMER.store(consumer, Ordering::Release);
    RECORDS_COPIED.fetch_add(written as u64, Ordering::Relaxed);
    if header.producer > consumer {
        // The turn is bounded to `MAX_RECORDS_PER_BROKER_TURN` and left records
        // behind. Re-arm on the backlog rather than on the producer's next
        // interrupt: this function clears `IRQ_PENDING` on entry, so without
        // this a consumer that fills one turn goes back to sleep holding a full
        // ring, and drain throughput becomes one turn per interrupt regardless
        // of how far behind it is.
        //
        // That is what the L0 relay was reporting: "fixed input-ring credit
        // timeout outstanding=1279 limit=1279" after 1692 forwarded events -
        // the outstanding count pinned exactly at the ring size, which is a
        // consumer that stopped rather than one that is merely slow.
        //
        // ORDERING: Release publishes the advanced consumer cursor above before
        // a woken reader observes the pending flag.
        IRQ_PENDING.store(true, Ordering::Release);
        super::wait_queue::wake_input_waiters();
    }
    written
}

/// Publish the consumer side of the inputd check-arm-recheck contract.
///
/// The waiter must already be registered before this generation is written.
/// L0 samples it only after committing a producer cursor and rings at most
/// once per generation. This gives batching under load without relying on a
/// stale empty/nonempty snapshot or an interrupt for every record.
pub(crate) fn arm_consumer_wake() -> bool {
    if !INSTALLED.load(Ordering::Acquire) {
        return false;
    }
    let Some(mapping) = claim_installed_mapping() else {
        return false;
    };
    let mapped = mapping.mapped();
    let generation = mapping.generation();
    let current = CONSUMER_WAKE_GENERATION.load(Ordering::Acquire);
    let Some(next) = current.checked_add(1) else {
        revoke("consumer-wake-generation-wrapped");
        return false;
    };
    if !mapping.validate_current() || mapping.epoch() != generation {
        return false;
    }
    // ORDERING: Release publishes the registered waiter before L0's Acquire
    // wake-generation read decides whether to send its MSI-X edge.
    store_shared_u64(
        mapped,
        DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET,
        next,
        shared_control_publish_order(),
    );
    CONSUMER_WAKE_GENERATION.store(next, Ordering::Release);
    true
}

/// Read-only poll recheck for the arm/recheck/commit protocol. An MSI-X edge
/// can arrive after inputd's STATS request drained the previous batch but
/// before the caller registers as an input waiter. Readiness must therefore
/// include the raw producer/consumer state, not only the decoded ingress
/// queue; otherwise that edge is lost and a finite poll can sleep forever.
pub(crate) fn has_pending_records() -> bool {
    READINESS_POLLS.fetch_add(1, Ordering::Relaxed);
    if RESET_PENDING_GENERATION.load(Ordering::Acquire) != 0 {
        return true;
    }
    if IRQ_PENDING.load(Ordering::Acquire) {
        return true;
    }
    if !INSTALLED.load(Ordering::Acquire) {
        return false;
    }
    let Some(mapping) = claim_installed_mapping() else {
        // An installed transport whose lifecycle claim cannot be taken is
        // mid activation or drain. Reporting "nothing pending" here lets the
        // sole consumer sleep through that transition even though records may
        // already be committed, and no later edge is owed to it. Force a
        // broker turn instead, exactly as the malformed-header case below
        // does, so the authoritative path observes the exact lifecycle state.
        report_mapping_claim_failure();
        return true;
    };
    let mapped = mapping.mapped();
    let Ok(header) = read_header(mapped) else {
        // Force a broker turn to perform the authoritative revoke rather than
        // letting a malformed live transport strand a sleeping reader.
        return true;
    };
    let generation = mapping.generation();
    let consumer = CONSUMER.load(Ordering::Acquire);
    header.generation != generation
        || header.consumer != consumer
        || header.producer < consumer
        || header.producer.saturating_sub(consumer) > u64::from(DVM_INPUT_RING_SLOT_COUNT)
        || header.producer != consumer
}

/// Publishes a new backlog high-water mark at a bounded cadence.
fn record_outstanding_high_water(available: u64) {
    let previous = OUTSTANDING_HIGH_WATER.fetch_max(available, Ordering::Relaxed);
    if available <= previous {
        return;
    }
    let reported = OUTSTANDING_HIGH_WATER_REPORTED.load(Ordering::Relaxed);
    if available < reported.saturating_add(OUTSTANDING_REPORT_STEP) {
        return;
    }
    OUTSTANDING_HIGH_WATER_REPORTED.store(available, Ordering::Relaxed);
    crate::debug::warn!(
        input,
        "dvm-input-ring: backlog high-water outstanding={} turns={} polls={} claim_lost={} mapping_claim_failed={} copied={}",
        available,
        BROKER_CALLS.load(Ordering::Relaxed),
        READINESS_POLLS.load(Ordering::Relaxed),
        DRAIN_CLAIM_LOST.load(Ordering::Relaxed),
        MAPPING_CLAIM_FAILURES.load(Ordering::Relaxed),
        RECORDS_COPIED.load(Ordering::Relaxed)
    );
}

/// Publishes the exact lifecycle state that refused a readiness claim.
///
/// A claim failure on an installed transport is the one state in which the
/// sole consumer can neither drain nor be owed a later edge, so it must be
/// attributable without re-running with ad-hoc probes. Output is bounded to
/// the first failure and then to exponentially spaced counts.
fn report_mapping_claim_failure() {
    let failures = MAPPING_CLAIM_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if failures != 1 && !failures.is_power_of_two() {
        return;
    }
    crate::debug::warn!(
        input,
        "dvm-input-ring: readiness claim refused failures={} generation={} lifecycle_epoch={} in_flight={} installed={} polls={} turns={} copied={}",
        failures,
        GENERATION.load(Ordering::Acquire),
        INPUT_LIFECYCLE.epoch(),
        INPUT_LIFECYCLE.in_flight(),
        INSTALLED.load(Ordering::Acquire),
        READINESS_POLLS.load(Ordering::Relaxed),
        BROKER_CALLS.load(Ordering::Relaxed),
        RECORDS_COPIED.load(Ordering::Relaxed)
    );
}

fn input_ring_interrupt(_vector: u8) {
    IRQ_PENDING.store(true, Ordering::Release);
    // This is a wake-only leaf. It does not inspect shared bytes, consume a
    // record, allocate, or execute input policy in interrupt context.
    super::wait_queue::wake_input_waiters();
}

/// Numeric identity of each revoke reason, in the order they appear above.
///
/// The reason reached only `debug::warn!`, which the product configuration does
/// not route to the debug transport, so a transport revocation mid-relay left
/// no record at all. The L0 relay bailed with "RustOS revoked input-ring
/// transport or policy-consumer readiness" and nothing on this side said which
/// check fired.
fn revoke_reason_code(reason: &str) -> u64 {
    match reason {
        "policy-ready-header-invalid" => 1,
        "policy-ready-lifecycle-invalid" => 2,
        "policy-ready-transport-not-ready" => 3,
        "header-invalid" => 4,
        "cursor-or-generation-invalid" => 5,
        "record-offset-invalid" => 6,
        "consumer-wake-generation-wrapped" => 7,
        _ => 0,
    }
}

fn revoke(reason: &str) {
    crate::debug::record_milestone(
        crate::debug::LogCategory::Input,
        "dvm-input-transport-revoked",
        GENERATION.load(Ordering::Acquire),
        revoke_reason_code(reason),
    );
    INPUT_LIFECYCLE.request_drain();
    // ORDERING: drain closes admission before Release publishes revoke work;
    // the generation and waiter wake follow that publication.
    REVOKE_PENDING.store(true, Ordering::Release);
    let generation = GENERATION.load(Ordering::Acquire).max(1);
    RESET_PENDING_GENERATION.store(generation, Ordering::Release);
    super::wait_queue::wake_input_waiters();
    crate::debug::warn!(
        input,
        "dvm-input-ring: transport drain requested reason={}",
        reason
    );
    finish_pending_lifecycle();
}

fn finish_pending_lifecycle() {
    // ORDERING: Acquire observes the drain request and its generation before
    // attempting the zero-claim transition to Revoked.
    if !WITHDRAW_PENDING.load(Ordering::Acquire) && !REVOKE_PENDING.load(Ordering::Acquire) {
        return;
    }
    let Some(retired_generation) = INPUT_LIFECYCLE.finish_drain() else {
        return;
    };
    if WITHDRAW_PENDING.swap(false, Ordering::AcqRel) {
        // ORDERING: AcqRel owns the one withdrawal reset; Acquire then observes
        // the mapping and cursor state published by the final claim.
        let mapped = SHARED_ADDR.load(Ordering::Acquire) as *mut u8;
        let valid = if mapped.is_null() {
            false
        } else if let Ok(header) = read_header(mapped) {
            let consumer = CONSUMER.load(Ordering::Acquire);
            if header.generation == retired_generation
                && header.consumer == consumer
                && header.producer >= consumer
                && header.producer.saturating_sub(consumer) <= u64::from(DVM_INPUT_RING_SLOT_COUNT)
            {
                // ORDERING: Release publishes withdrawal's retired cursor
                // before the pending-bit store exposes completion to L0.
                store_shared_u64(
                    mapped,
                    DVM_INPUT_RING_CONSUMER_OFFSET,
                    header.producer,
                    shared_control_publish_order(),
                );
                CONSUMER.store(header.producer, Ordering::Release);
                IRQ_PENDING.store(false, Ordering::Release);
                true
            } else {
                false
            }
        } else {
            false
        };
        if !valid {
            // ORDERING: Release hands malformed withdrawal to the stronger
            // revoke branch before this finisher relinquishes ownership.
            REVOKE_PENDING.store(true, Ordering::Release);
        }
    }
    // ORDERING: AcqRel selects the sole revoke/reset owner after every claim
    // and optional withdrawal cursor publication is complete.
    if !REVOKE_PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    INSTALLED.store(false, Ordering::Release);
    let mapped = SHARED_ADDR.swap(0, Ordering::AcqRel) as *mut u8;
    if !mapped.is_null() {
        let _ = update_shared_flags(mapped, |flags| {
            Some(
                flags
                    & !(DVM_INPUT_RING_FLAG_RUSTOS_READY
                        | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY),
            )
        });
        // ORDERING: AcqRel withdraws both RustOS-owned ready bits before this
        // mapping can be released and L0 observes a terminal revocation.
    }
    release_mapping(mapped);
    IRQ_PENDING.store(false, Ordering::Release);
    CONSUMER_WAKE_GENERATION.store(0, Ordering::Release);
    // The kernel does not decode or own input/network policy. Publish a
    // generation-stamped barrier so inputd can revoke its state and notify
    // netd before accepting records from any replacement generation.
    // ORDERING: Release publishes the exact retired epoch to policy consumers
    // before their waiter wake can observe transport absence.
    RESET_PENDING_GENERATION.store(retired_generation.max(1), Ordering::Release);
    REVOKE_COUNT.fetch_add(1, Ordering::Relaxed);
    super::wait_queue::wake_input_waiters();
    crate::debug::warn!(input, "dvm-input-ring: transport revoked after quiescence");
}

fn wait_for_lifecycle_quiescence(reason: &str) {
    const QUIESCE_TIMEOUT_NS: u64 = 2_000_000_000;
    let started = crate::arch::clock::monotonic_nanos();
    loop {
        finish_pending_lifecycle();
        // ORDERING: Acquire observes both pending-bit clear operations after
        // the lifecycle's zero-claim finish transition.
        if INPUT_LIFECYCLE.in_flight() == 0
            && !WITHDRAW_PENDING.load(Ordering::Acquire)
            && !REVOKE_PENDING.load(Ordering::Acquire)
        {
            return;
        }
        if crate::arch::clock::monotonic_nanos().saturating_sub(started) >= QUIESCE_TIMEOUT_NS {
            panic!(
                "DVM input lifecycle failed to quiesce reason={} generation={} in_flight={}",
                reason,
                INPUT_LIFECYCLE.epoch(),
                INPUT_LIFECYCLE.in_flight()
            );
        }
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InputTransportDebugSnapshot {
    pub broker_calls: u64,
    pub records_copied: u64,
    pub queued: usize,
    pub revoke_count: u64,
}

pub(crate) fn debug_snapshot() -> InputTransportDebugSnapshot {
    InputTransportDebugSnapshot {
        broker_calls: BROKER_CALLS.load(Ordering::Relaxed),
        records_copied: RECORDS_COPIED.load(Ordering::Relaxed),
        queued: usize::from(has_pending_records()),
        revoke_count: REVOKE_COUNT.load(Ordering::Relaxed),
    }
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
    crate::debug::println_serialized(format_args!(
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
        INSTALL_REJECTION_DEVICE_CLAIM => "device-already-claimed",
        _ => "unknown-install-rejection",
    }
}

fn consume_attach_attempt() -> bool {
    ATTACH_ATTEMPTS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |attempts| {
            if attempts >= MAX_ATTACH_ATTEMPTS_PER_BOOT {
                return None;
            }
            Some(attempts + 1)
        })
        .is_ok()
}

fn release_mapping(mapped: *mut u8) {
    if !mapped.is_null() {
        crate::driver::mmio::unmap(mapped.cast());
    }
}

const fn fixed_input_shared_bar_shape(is_io: bool, prefetchable: bool, size: u64) -> bool {
    !is_io && prefetchable && size == DVM_INPUT_RING_APERTURE_BYTES
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
        if !fixed_input_shared_bar_shape(resource.is_io, resource.prefetchable, resource.size) {
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
        // The L0 producer maps the launch-owned file as ordinary shared RAM.
        // Match it with WB here: this ring uses acquire/release atomics, not a
        // write-mostly framebuffer contract.
        let mapped =
            crate::driver::mmio::map_shared_write_back(resource.start, resource_len).cast::<u8>();
        if mapped.is_null() {
            last_rejection = DISCOVERY_REJECTION_MAPPING;
            exact_aperture_rejection = last_rejection;
            return false;
        }
        if !shared_control_words_are_aligned(mapped) {
            release_mapping(mapped);
            last_rejection = DISCOVERY_REJECTION_MAPPING;
            exact_aperture_rejection = last_rejection;
            return false;
        }
        let Ok(header) = read_header(mapped) else {
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

/// Why a header read did not yield a usable header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderRejection {
    /// The aperture has not published its immutable prefix yet. Skip the turn.
    Unpublished,
    /// The header is present and wrong. Revoke.
    Invalid,
}

fn read_header(mapped: *const u8) -> Result<DvmInputRingHeader, HeaderRejection> {
    let mut bytes = [0_u8; DvmInputRingHeader::encoded_len()];
    copy_immutable_header_bytes(mapped, &mut bytes);
    if header_is_unpublished(&bytes) {
        return Err(HeaderRejection::Unpublished);
    }
    // The mutable control words have independent atomic owners. Copying them
    // bytewise would be a non-atomic race with a valid cursor/ready update.
    write_control_words_to_header_bytes(mapped, &mut bytes);
    // Geometry is immutable for the boot, but the two cursors are independent
    // concurrent writers. Atomic consumer-before-producer loads preserve the
    // monotonic cursor invariant without a torn byte snapshot.
    let Some(header) = DvmInputRingHeader::decode(&bytes) else {
        report_invalid_header(mapped, &bytes);
        return Err(HeaderRejection::Invalid);
    };
    // Read the consumer first. `is_valid` requires `producer >= consumer`, and
    // the pair cannot be sampled atomically: L0 advances the producer while a
    // lifecycle retire on another CPU advances the consumer. Sampling the
    // producer first admits the window where the consumer moves past the
    // already-read producer, which `is_valid` cannot distinguish from a corrupt
    // header - and the caller answers a corrupt header by revoking the whole
    // transport. At 8 vCPU that fired mid-relay after about 140 events, clearing
    // both ready bits, and the L0 relay failed the FPS proof with
    // "RustOS revoked input-ring transport or policy-consumer readiness".
    //
    // In this order the invariant holds by construction rather than by luck.
    // Both cursors advance monotonically, so a producer read strictly after a
    // consumer read is at least the producer value that bounded that consumer,
    // which is at least the consumer itself. The outstanding bound survives the
    // same way: L0 admits at most a full ring beyond whatever consumer it last
    // observed, and that observation cannot be newer than this one.
    if header.is_valid() {
        Ok(header)
    } else {
        report_invalid_header(mapped, &bytes);
        Err(HeaderRejection::Invalid)
    }
}

/// Publishes which admission check rejected a header before the caller answers
/// by revoking the transport.
///
/// The revoke reason said only `header-invalid`, which covers a magic, version,
/// geometry, padding, flag, and generation check across a snapshot read
/// byte-by-byte while other CPUs write some of those fields. Two candidate
/// explanations were each worth a full 90-second acceptance run and neither was
/// it. The failing field is cheap to publish and ends the guessing.
/// Whether the header is unpublished rather than corrupt.
///
/// The magic is written once by L0 when it creates the aperture and never
/// changes, so an all-zero magic is not a damaged header - it is a region whose
/// publication this read got in front of. The field-level rejection record
/// distinguished the two: `arg1=0x5f` failed magic, version, header size, slot
/// count, record size, and region size together while the padding-must-be-zero
/// check passed and a re-read of flags and generation returned 0x7 and 1. Only
/// an all-zero prefix produces that combination.
///
/// Revoking on it is the transient-as-terminal defect again, and an expensive
/// one: revocation clears both ready bits and retires every undelivered record,
/// which is what the L0 relay reports as
/// "RustOS revoked input-ring transport or policy-consumer readiness".
fn header_is_unpublished(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[0..8].iter().all(|byte| *byte == 0)
}

fn report_invalid_header(mapped: *const u8, bytes: &[u8]) {
    let flags = load_shared_flags(mapped);
    let generation = read_immutable_u64(mapped, 56);
    let region_bytes = u64::from_le_bytes(bytes[16..24].try_into().unwrap_or_default());
    let mut checks = 0_u64;
    let mut set = |ok: bool, bit: u32| {
        if !ok {
            checks |= 1 << bit;
        }
    };
    set(
        bytes[0..8] == driver_domain_protocol::DVM_INPUT_RING_MAGIC,
        0,
    );
    set(
        u32::from_le_bytes(bytes[8..12].try_into().unwrap_or_default())
            == driver_domain_protocol::DVM_INPUT_RING_VERSION,
        1,
    );
    set(
        u32::from_le_bytes(bytes[12..16].try_into().unwrap_or_default())
            == driver_domain_protocol::DVM_INPUT_RING_HEADER_BYTES,
        2,
    );
    set(
        u32::from_le_bytes(bytes[24..28].try_into().unwrap_or_default())
            == DVM_INPUT_RING_SLOT_COUNT,
        3,
    );
    set(
        u32::from_le_bytes(bytes[28..32].try_into().unwrap_or_default())
            == DVM_INPUT_RING_RECORD_BYTES as u32,
        4,
    );
    set(bytes[36..56].iter().all(|byte| *byte == 0), 5);
    set(
        region_bytes >= driver_domain_protocol::DVM_INPUT_RING_MIN_REGION_BYTES
            && region_bytes <= DVM_INPUT_RING_APERTURE_BYTES,
        6,
    );
    set(
        flags & !driver_domain_protocol::DVM_INPUT_RING_KNOWN_FLAGS == 0,
        7,
    );
    set(
        flags & driver_domain_protocol::DVM_INPUT_RING_FLAG_READY != 0,
        8,
    );
    set(generation != 0, 9);
    crate::debug::record_milestone(
        crate::debug::LogCategory::Input,
        "dvm-input-header-rejected",
        (u64::from(flags) << 32) | (generation & 0xffff_ffff),
        checks,
    );
}

fn arm_input_ring_interrupt(device: crate::arch::pci::PciDevice) -> Result<(), u8> {
    // Claim the function before the first configuration write; this transport
    // owns its ivshmem function outright for the rest of the boot.
    let attach = crate::arch::pci::attach(device, crate::arch::pci::PciAttachMode::Exclusive)
        .ok_or(INSTALL_REJECTION_DEVICE_CLAIM)?;
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
    capability.set_function_masked(&attach, true);
    capability.set_enabled(&attach, false);
    let mut vector_lease =
        crate::arch::msi::MsiVectorLease::allocate().ok_or(INSTALL_REJECTION_VECTOR_ALLOCATION)?;
    if !vector_lease.register_handler(input_ring_interrupt) {
        return Err(INSTALL_REJECTION_HANDLER_REGISTRATION);
    }
    let message = vector_lease.message().ok_or(INSTALL_REJECTION_MESSAGE)?;
    let table = crate::driver::mmio::map_uncached(table_resource.start, table_len).cast::<u8>();
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
    capability.set_enabled(&attach, true);
    capability.set_function_masked(&attach, false);
    crate::driver::mmio::unmap(table.cast());
    vector_lease.commit().retain_permanent();
    attach.retain_permanent();
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

fn shared_control_words_are_aligned(base: *const u8) -> bool {
    let base = base as usize;
    base != 0
        && base
            .checked_add(DVM_INPUT_RING_FLAGS_OFFSET)
            .is_some_and(|address| address % align_of::<AtomicU32>() == 0)
        && base
            .checked_add(DVM_INPUT_RING_PRODUCER_OFFSET)
            .is_some_and(|address| address % align_of::<AtomicU64>() == 0)
        && base
            .checked_add(DVM_INPUT_RING_CONSUMER_OFFSET)
            .is_some_and(|address| address % align_of::<AtomicU64>() == 0)
        && base
            .checked_add(DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET)
            .is_some_and(|address| address % align_of::<AtomicU64>() == 0)
}

fn load_shared_flags(base: *const u8) -> u32 {
    load_shared_u32(base, DVM_INPUT_RING_FLAGS_OFFSET)
}

fn load_shared_u32(base: *const u8, offset: usize) -> u32 {
    debug_assert!(shared_control_words_are_aligned(base));
    // SAFETY: install admits a page-aligned WB mapping and the checked fixed
    // offset is aligned for `AtomicU32`. This temporary reference names only
    // the atomic control word; all concurrent accesses use these atomic helpers.
    let word = unsafe { AtomicU32::from_ptr(base.cast_mut().add(offset).cast::<u32>()) };
    u32::from_le(word.load(shared_control_load_order()))
}

fn load_shared_u64(base: *const u8, offset: usize) -> u64 {
    debug_assert!(shared_control_words_are_aligned(base));
    // SAFETY: install admits a page-aligned WB mapping and the checked fixed
    // offset is aligned for `AtomicU64`. This temporary reference names only
    // the atomic control word; all concurrent accesses use these atomic helpers.
    let word = unsafe { AtomicU64::from_ptr(base.cast_mut().add(offset).cast::<u64>()) };
    u64::from_le(word.load(shared_control_load_order()))
}

fn store_shared_u64(base: *mut u8, offset: usize, value: u64, ordering: Ordering) {
    debug_assert!(shared_control_words_are_aligned(base));
    // SAFETY: install admits a page-aligned WB mapping and the checked fixed
    // offset is aligned for `AtomicU64`. This temporary reference names only
    // the atomic control word; all concurrent accesses use these atomic helpers.
    let word = unsafe { AtomicU64::from_ptr(base.add(offset).cast::<u64>()) };
    word.store(value.to_le(), ordering);
}

fn update_shared_flags(
    base: *mut u8,
    mut update: impl FnMut(u32) -> Option<u32>,
) -> Option<(u32, u32)> {
    debug_assert!(shared_control_words_are_aligned(base));
    // SAFETY: install admits a page-aligned WB mapping and the checked fixed
    // offset is aligned for `AtomicU32`. This temporary reference names only
    // the atomic control word; all concurrent accesses use these atomic helpers.
    let word = unsafe { AtomicU32::from_ptr(base.add(DVM_INPUT_RING_FLAGS_OFFSET).cast::<u32>()) };
    let mut observed = word.load(shared_control_load_order());
    loop {
        let previous = u32::from_le(observed);
        let next = update(previous)?;
        match word.compare_exchange_weak(
            observed,
            next.to_le(),
            shared_control_update_order(),
            shared_control_update_failure_order(),
        ) {
            Ok(_) => return Some((previous, next)),
            Err(current) => observed = current,
        }
    }
}

fn copy_immutable_header_bytes(mapped: *const u8, bytes: &mut [u8]) {
    for range in [
        0..DVM_INPUT_RING_FLAGS_OFFSET,
        DVM_INPUT_RING_FLAGS_OFFSET + size_of::<u32>()..DVM_INPUT_RING_PRODUCER_OFFSET,
        DVM_INPUT_RING_PRODUCER_OFFSET + size_of::<u64>()..DVM_INPUT_RING_CONSUMER_OFFSET,
        DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET + size_of::<u64>()..bytes.len(),
    ] {
        for index in range {
            // SAFETY: `read_header` is called only for an admitted fixed-size
            // mapping, and these ranges exclude every concurrently mutable word.
            bytes[index] = unsafe { mapped.add(index).read_volatile() };
        }
    }
}

fn write_control_words_to_header_bytes(mapped: *const u8, bytes: &mut [u8]) {
    let flags = load_shared_flags(mapped);
    // ORDERING: consumer first, then producer, makes the monotonic ring bound
    // valid for this non-atomic composite snapshot.
    let consumer = load_shared_u64(mapped, DVM_INPUT_RING_CONSUMER_OFFSET);
    let producer = load_shared_u64(mapped, DVM_INPUT_RING_PRODUCER_OFFSET);
    let wake_generation = load_shared_u64(mapped, DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET);
    bytes[DVM_INPUT_RING_FLAGS_OFFSET..DVM_INPUT_RING_FLAGS_OFFSET + size_of::<u32>()]
        .copy_from_slice(&flags.to_le_bytes());
    bytes[DVM_INPUT_RING_PRODUCER_OFFSET..DVM_INPUT_RING_PRODUCER_OFFSET + size_of::<u64>()]
        .copy_from_slice(&producer.to_le_bytes());
    bytes[DVM_INPUT_RING_CONSUMER_OFFSET..DVM_INPUT_RING_CONSUMER_OFFSET + size_of::<u64>()]
        .copy_from_slice(&consumer.to_le_bytes());
    bytes[DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET
        ..DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET + size_of::<u64>()]
        .copy_from_slice(&wake_generation.to_le_bytes());
}

fn read_immutable_u64(base: *const u8, offset: usize) -> u64 {
    let mut bytes = [0_u8; size_of::<u64>()];
    for (index, byte) in bytes.iter_mut().enumerate() {
        // SAFETY: generation is immutable after L0 initializes the admitted
        // aperture; callers use this only for header diagnostics.
        *byte = unsafe { base.add(offset + index).read_volatile() };
    }
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_broker_callers_have_exactly_one_drain_owner() {
        let owner = AtomicBool::new(false);
        assert!(try_claim_drain(&owner));
        assert!(!try_claim_drain(&owner));
        // ORDERING: Release models the DrainGuard publication between turns.
        owner.store(false, Ordering::Release);
        assert!(try_claim_drain(&owner));
    }

    #[test]
    fn input_ring_control_words_are_atomic_and_ordered() {
        assert!(input_ring_atomic_control_layout_is_valid());
        assert_eq!(shared_control_load_order(), Ordering::Acquire);
        assert_eq!(shared_control_publish_order(), Ordering::Release);
        assert_eq!(shared_control_update_order(), Ordering::AcqRel);
        assert_eq!(shared_control_update_failure_order(), Ordering::Acquire);

        let mut backing = [0_u64; 32];
        let base = backing.as_mut_ptr().cast::<u8>();
        assert!(shared_control_words_are_aligned(base));

        let header = DvmInputRingHeader::new(DVM_INPUT_RING_APERTURE_BYTES, 9).encode();
        for (index, byte) in header.iter().enumerate() {
            // SAFETY: this test initializes private backing before any atomic
            // view exists, so there is no concurrent shared-memory access.
            unsafe { base.add(index).write_volatile(*byte) };
        }
        assert_eq!(
            load_shared_flags(base),
            driver_domain_protocol::DVM_INPUT_RING_FLAG_READY
        );

        store_shared_u64(
            base,
            DVM_INPUT_RING_PRODUCER_OFFSET,
            7,
            shared_control_publish_order(),
        );
        store_shared_u64(
            base,
            DVM_INPUT_RING_CONSUMER_OFFSET,
            3,
            shared_control_publish_order(),
        );
        store_shared_u64(
            base,
            DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET,
            11,
            shared_control_publish_order(),
        );
        assert_eq!(load_shared_u64(base, DVM_INPUT_RING_PRODUCER_OFFSET), 7);
        assert_eq!(load_shared_u64(base, DVM_INPUT_RING_CONSUMER_OFFSET), 3);
        assert_eq!(
            load_shared_u64(base, DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET),
            11
        );
        let snapshot = read_header(base).unwrap();
        assert_eq!(
            (
                snapshot.producer,
                snapshot.consumer,
                snapshot.consumer_wake_generation
            ),
            (7, 3, 11)
        );

        let production = include_str!("dvm_ring.rs")
            .split_once("#[cfg(test)]")
            .expect("input-ring tests remain below production")
            .0;
        assert!(production.contains("AtomicU32::from_ptr"));
        assert!(production.contains("AtomicU64::from_ptr"));
        assert!(production.contains("fn write_control_words_to_header_bytes"));
        assert!(!production.contains(".write_volatile(consumer.to_le())"));
        assert!(!production.contains(
            "DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET)\n            .cast::<u64>()"
        ));
    }

    #[test]
    fn policy_consumer_readiness_requires_transport_and_is_idempotent() {
        let provider_ready = driver_domain_protocol::DVM_INPUT_RING_FLAG_READY;
        assert_eq!(admitted_policy_ready_flags(provider_ready), None);

        let transport_ready = provider_ready | DVM_INPUT_RING_FLAG_RUSTOS_READY;
        let admitted = transport_ready | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY;
        assert_eq!(admitted_policy_ready_flags(transport_ready), Some(admitted));
        assert_eq!(admitted_policy_ready_flags(admitted), Some(admitted));
    }

    #[test]
    fn input_shared_ring_requires_prefetchable_write_back_atomic_memory() {
        assert!(fixed_input_shared_bar_shape(
            false,
            true,
            DVM_INPUT_RING_APERTURE_BYTES
        ));
        assert!(!fixed_input_shared_bar_shape(
            false,
            false,
            DVM_INPUT_RING_APERTURE_BYTES
        ));
        assert!(!fixed_input_shared_bar_shape(
            true,
            true,
            DVM_INPUT_RING_APERTURE_BYTES
        ));

        let production = include_str!("dvm_ring.rs")
            .split_once("#[cfg(test)]")
            .expect("input-ring tests must remain below production")
            .0;
        assert_eq!(
            production.matches("mmio::map_shared_write_back(").count(),
            1
        );
        assert!(!production.contains("mmio::map_write_combining(resource.start, resource_len)"));
    }

    #[test]
    fn policy_consumer_withdrawal_preserves_transport_but_stops_production() {
        let provider_ready = driver_domain_protocol::DVM_INPUT_RING_FLAG_READY;
        let transport_ready = provider_ready | DVM_INPUT_RING_FLAG_RUSTOS_READY;
        let admitted = transport_ready | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY;
        assert_eq!(
            admitted & !DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY,
            transport_ready
        );
        assert_eq!(
            admitted_policy_ready_flags(admitted & !DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY),
            Some(admitted)
        );
    }
}
