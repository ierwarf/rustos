//! Anonymous demand-paging admission for the production MM broker.
//!
//! - **Owner:** `pagerd` owns anonymous fault policy. This module only stamps
//!   an exact kernel-authored region and admits it into that policy; it never
//!   chooses page contents, eviction order, or backing.
//! - **Boundary:** The pagerd endpoint identity, its epoch, and every protocol
//!   reply are untrusted. The region template carries zeroed process and VMA
//!   generations so neither `syscalld` nor `pagerd` can forge MM authority;
//!   `kernel-ps` stamps them from live publications.
//! - **Lifecycle:** Resolve endpoint -> publish stamped VMA -> admit into
//!   pagerd -> the range becomes faultable. Any failure revokes the exact VMA
//!   generation before returning, so no half-published region survives.
//! - **Concurrency:** Publication takes the target process-state lock and
//!   returns before the pagerd call runs; no MM lock is held across service
//!   IPC and no PTE is partially changed.
//! - **Failure:** Absent pagerd, a stale epoch, a malformed reply, and slot
//!   pressure all fail closed. The caller keeps the eager bootstrap mapping
//!   path, which is still correct, rather than a partially admitted region.
//! - **Forbidden:** No pageable admission for the wired pager control graph
//!   (that would be a recursive fault), no unbounded epoch retry, no PID-only
//!   authority, and no W+X region.
//! - **Evidence:** `pager-vma-publication`, `pager-fault-slot-lifecycle`, the
//!   focused tests below, and the `pager-admission-*` implementation
//!   mutations.

use super::*;

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_user_abi::pager::{
    PAGER_MAX_FAULT_SLOTS, PAGER_MAX_FRAME_GRANTS, PAGER_MAX_VMAS_PER_PROCESS,
    PAGER_PRESSURE_REGION_SPLIT_NO_SLOT, PAGER_WIRED_FAULT_FRAMES, PagerEndpointCapabilityWire,
    PagerObjectIdentityWire, PagerProtectRangeWire, PagerVmRegionWire, VM_OBJECT_ANONYMOUS,
    VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE, VM_SHARING_PRIVATE, pager_pressure_name,
};
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT, COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT,
    COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_PAGERD, CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    IPC_SERVICE_PAGERD, IPC_SERVICE_ROOTD, IPC_SERVICE_STORAGED, IPC_SERVICE_VFSD,
};

// The capacities ring0 compiles against and the ones the pager sizes its own
// tables from have to be one set of numbers. They were three independent
// `64`s with no declared relation until a fault-slot table twice the size of
// its wired frame reserve drained the reserve and killed a user thread. These
// asserts are where the shared ABI and the ring0 implementation are bound
// together; `kernel-mm` itself stays free of the user ABI.
const _: () = assert!(
    crate::memory::frame_capability::MAX_PREALLOCATED_PAGER_FAULT_FRAMES
        == PAGER_WIRED_FAULT_FRAMES,
    "the wired fault reserve must be exactly the size the shared ABI publishes"
);
const _: () = assert!(
    crate::memory::frame_capability::MAX_PAGER_FRAME_GRANTS == PAGER_MAX_FRAME_GRANTS,
    "the grant table must be exactly the size the shared ABI publishes"
);
const _: () = assert!(
    crate::memory::frame_capability::MAX_PREALLOCATED_PAGER_FAULT_FRAMES >= PAGER_MAX_FAULT_SLOTS,
    "every reservable fault slot must have a wired frame behind it"
);
const _: () = assert!(
    PAGER_MAX_VMAS_PER_PROCESS <= rustos_user_abi::pager::PAGER_MAX_TRACKED_REGIONS,
    "one process's VMA table must fit inside the pager's region table"
);

/// Services that resolve or carry faults. Their own anonymous memory stays
/// eagerly mapped, because a fault raised inside this graph would have to be
/// resolved by a member of the same graph.
const PAGER_CONTROL_GRAPH: [u64; 5] = [
    IPC_SERVICE_PAGERD,
    IPC_SERVICE_ROOTD,
    IPC_SERVICE_VFSD,
    IPC_SERVICE_STORAGED,
    linux_abi::IPC_SERVICE_LINUX_SYSCALLD,
];

/// Highest pagerd epoch proven by an authenticated reply. Zero means this boot
/// has not yet observed one, so the first admission must learn it.
static OBSERVED_PAGER_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Monotonic anonymous VM-object slot source. Anonymous objects have no
/// backing service, so the kernel owns their identity; the slot never repeats
/// within a boot, which keeps a stale region from matching a newer fault.
static NEXT_ANON_OBJECT_SLOT: AtomicU64 = AtomicU64::new(1);

/// Translates a Linux protection into pager VMA rights, including the empty
/// protection used by guard ranges. W+X and unknown bits always fail closed.
pub(super) fn pager_protection(prot: u64) -> Option<u32> {
    let supported = linux_abi::PROT_READ | linux_abi::PROT_WRITE | linux_abi::PROT_EXEC;
    if prot & !supported != 0
        || (prot & linux_abi::PROT_WRITE != 0 && prot & linux_abi::PROT_EXEC != 0)
    {
        return None;
    }
    let mut rights = 0;
    if prot & linux_abi::PROT_READ != 0 {
        rights |= VM_PROT_READ;
    }
    if prot & linux_abi::PROT_WRITE != 0 {
        rights |= VM_PROT_WRITE;
    }
    if prot & linux_abi::PROT_EXEC != 0 {
        rights |= VM_PROT_EXECUTE;
    }
    Some(rights)
}

fn pager_rights(prot: u64) -> Option<u32> {
    pager_protection(prot).filter(|rights| *rights != 0)
}

/// Resolves the live pagerd endpoint as a fault capability.
fn fault_endpoint() -> Option<PagerEndpointCapabilityWire> {
    let identity = ipc_ops::service_endpoint(IPC_SERVICE_PAGERD)?.identity()?;
    let capability = PagerEndpointCapabilityWire {
        slot: identity.slot(),
        generation: identity.generation(),
        rights: u64::from(VM_PROT_READ),
    };
    capability.has_authority().then_some(capability)
}

/// True when the target process is part of the wired pager control graph.
///
/// Lock-free: `process_owns_published_service_endpoint` reads one atomic per
/// service and is conservative in the safe direction (see its contract). The
/// previous form called `process_owns_live_service_endpoint`, which takes the
/// service-endpoint registry lock, so every eligible anonymous `mmap` could
/// acquire that lock up to five times and contend with service registration
/// and lookup on a hot path.
///
/// A member is classified correctly even before it registers, because a
/// mapping made while pagerd has no endpoint is already
/// [`EagerByContract::PagerTransportAbsent`]: no process can obtain a
/// demand-backed range until the pager transport exists, and pagerd publishes
/// its own owner word before its endpoint becomes visible. That closes the
/// registration-time window in which a control-graph member could otherwise
/// have acquired demand-backed memory and later become a fault resolver.
fn in_pager_control_graph(target_pid: u64) -> bool {
    PAGER_CONTROL_GRAPH
        .iter()
        .any(|service_id| ipc_ops::process_owns_published_service_endpoint(target_pid, *service_id))
}

/// Why an anonymous range is wired instead of demand-backed. Each variant is
/// a contract, not a downgrade: no pager exists that could serve the range, or
/// serving it would require the target to resolve its own fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EagerByContract {
    /// pagerd has published no fault endpoint yet. Every mapping made before
    /// the pager transport exists is wired by definition.
    PagerTransportAbsent,
    /// The target resolves or carries faults. A fault raised inside this graph
    /// would have to be resolved by a member of the same graph.
    PagerControlGraph,
    /// This process has used its bounded per-process demand-paging capacity
    /// (`PAGER_MAX_VMAS_PER_PROCESS`). Anonymous ranges beyond it are wired.
    ///
    /// This is a declared bound, not a failure: a dynamic loader publishes far
    /// more than 64 anonymous ranges, so refusing the mapping would make an
    /// ordinary process unable to start. What was wrong before was not the
    /// wiring, it was that the bound was indistinguishable from a broken
    /// transport, a refused epoch, and a stale identity - all of them mapped
    /// to the same invisible eager mapping.
    ProcessVmaCapacity,
    /// pagerd's bounded region table is full. Also a declared bound: failing
    /// the mapping would let one process's residue break every other process.
    /// Dead regions are what fill this table, and unconfirmed releases are now
    /// reconciled rather than dropped, so growth here is a real signal.
    PagerRegionCapacity,
    /// A published region already covers this range for this exact process
    /// generation, so the previous mapping's publication outlived its memory.
    ///
    /// Wiring the range keeps the mapping semantically correct - anonymous
    /// zero-filled memory either way - while the counter and its milestone
    /// name the leaked range. Failing the mapping instead is what killed the
    /// dynamic loader: `ld.so` reports it as "cannot allocate symbol search
    /// list" and the process never starts, which is a far worse outcome than
    /// one wired range plus a loud, addressed report.
    StaleRegionOverlap,
}

/// Outcome of admitting one anonymous range.
///
/// The three cases exist because they must be handled differently and used to
/// be indistinguishable. `admit_anonymous_region` previously returned
/// `Result<(), i64>` and *every* error fell through to an eager mapping that
/// reported success, so a full pagerd region table, a refused epoch, and a
/// broken transport all looked exactly like normal boot-time wiring. Demand
/// paging could stop being used for the rest of the boot with nothing in the
/// system saying so.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AnonymousAdmission {
    /// Published and admitted; first touch faults.
    Demand,
    /// Wired because that is this target's contract.
    Eager(EagerByContract),
    /// The pager transport is live and this range could not be admitted. The
    /// caller must surface this rather than fabricate success.
    Failed(i64),
}

/// Wired-by-contract mappings, by reason, and admission failures.
///
/// Counting rather than logging every mapping: an anonymous `mmap` is hot, and
/// a per-mapping record would be its own performance defect. The first
/// occurrence of each class and every 64th after it are published, which is
/// enough to see a class appear and to see one that is growing.
/// One counter per class, not one shared failure counter. The publish rule is
/// "first occurrence and every 64th", so a shared counter reports only whichever
/// class happened first and hides every later class behind it.
static WIRED_TRANSPORT_ABSENT: AtomicU64 = AtomicU64::new(0);
static WIRED_CONTROL_GRAPH: AtomicU64 = AtomicU64::new(0);
static WIRED_PROCESS_VMA_CAPACITY: AtomicU64 = AtomicU64::new(0);
static WIRED_PAGER_REGION_CAPACITY: AtomicU64 = AtomicU64::new(0);
static WIRED_STALE_REGION_OVERLAP: AtomicU64 = AtomicU64::new(0);
static ADMISSION_REFUSED: AtomicU64 = AtomicU64::new(0);
static ADMISSION_PUBLISH_FAILED: AtomicU64 = AtomicU64::new(0);
static ADMISSION_TRANSPORT_FAILED: AtomicU64 = AtomicU64::new(0);
static ADMISSION_EPOCH_EXHAUSTED: AtomicU64 = AtomicU64::new(0);

fn record_admission_class(counter: &AtomicU64, name: &'static str, detail: u64) {
    let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 || count.is_multiple_of(64) {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            name,
            count,
            detail,
        );
    }
}

/// Builds the unbound region template. Process and VMA generations stay zero:
/// `kernel-ps` is the only writer permitted to stamp them.
fn region_template(
    start: u64,
    end: u64,
    rights: u32,
    epoch: u64,
    endpoint: PagerEndpointCapabilityWire,
) -> Option<PagerVmRegionWire> {
    let slot = NEXT_ANON_OBJECT_SLOT.fetch_add(1, Ordering::Relaxed);
    if slot == 0 || epoch == 0 {
        return None;
    }
    Some(PagerVmRegionWire {
        start,
        end,
        object: PagerObjectIdentityWire {
            object_type: VM_OBJECT_ANONYMOUS,
            reserved0: 0,
            rights,
            backing_service: 0,
            slot,
            generation: 1,
            pager_epoch: epoch,
            backing_generation: 1,
        },
        object_offset: 0,
        prot: rights,
        sharing: VM_SHARING_PRIVATE,
        reserved0: 0,
        vma_generation: 0,
        process_handle: 0,
        process_generation: 0,
        mm_generation: 0,
        fault_endpoint: endpoint,
        reserved1: [0; 2],
    })
}

/// Sends one bounded admission call and reports the epoch pagerd proved.
///
/// The returned epoch is authoritative even when the status is an error, which
/// is what lets a single stale-epoch retry converge without a loop.
fn admit_call(region: PagerVmRegionWire) -> Result<(i32, u64), i64> {
    let Some(snapshot) = multitask::current_user_snapshot() else {
        return Err(LINUX_EINVAL);
    };
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_PAGERD;
    request.header.op = COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT;
    request.header.service_id = IPC_SERVICE_PAGERD;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    let payload = as_bytes(&region);
    let payload_len = u32::try_from(payload.len()).map_err(|_| LINUX_EINVAL)?;
    request.payload[..payload.len()].copy_from_slice(payload);
    request.payload_len = payload_len;

    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_PAGERD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    )?;
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    if response.header.protocol != COMMERCIAL_MAX_PROTOCOL_PAGERD
        || response.header.op != COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT
        || response.header.service_id != IPC_SERVICE_PAGERD
        || response.descriptor_count != 0
        || response.payload_len != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok((response.status, response.value0))
}

/// Records a proven epoch, keeping the highest value a reply has justified.
fn observe_epoch(epoch: u64) {
    if epoch == 0 {
        return;
    }
    let _ = OBSERVED_PAGER_EPOCH.fetch_max(epoch, Ordering::Relaxed);
}

/// Releases that ring0 has already performed but pagerd has not confirmed.
///
/// Ring0 frees its VMA slot synchronously; the pagerd notification can still
/// fail (transport error, a refused reply, a service epoch that rolled). The
/// result used to be discarded, which left pagerd holding a region for memory
/// that no longer exists. A dead region refuses re-admission of its own range
/// as an overlap and consumes a fixed table slot, so repeated failures fill the
/// table and every later admission is refused.
///
/// This is a bounded reconciliation queue, not a retry loop: a failed release
/// is parked here and re-sent from the next admission, which is exactly the
/// point where a dead region would otherwise start causing refusals.
const MAX_PENDING_RELEASES: usize = 32;

/// Slot publication states. Payload words are written before the slot is
/// published `READY` and are read only after it is claimed for drain, so no
/// reader can observe a half-written release.
const RELEASE_SLOT_EMPTY: u64 = 0;
const RELEASE_SLOT_WRITING: u64 = 1;
const RELEASE_SLOT_READY: u64 = 2;
const RELEASE_SLOT_DRAINING: u64 = 3;

/// Fixed reconciliation queue. Owning the slot arrays in one value rather than
/// five parallel statics is what makes the publication protocol testable: a
/// witness constructs its own queue instead of serializing against a global.
struct PendingReleaseQueue {
    state: [AtomicU64; MAX_PENDING_RELEASES],
    process_handle: [AtomicU64; MAX_PENDING_RELEASES],
    process_generation: [AtomicU64; MAX_PENDING_RELEASES],
    start: [AtomicU64; MAX_PENDING_RELEASES],
    end: [AtomicU64; MAX_PENDING_RELEASES],
    /// Replacement protection, or `0` for a release.
    ///
    /// A release and an `mprotect(PROT_NONE)` reach the pager as the same
    /// outcome - the covered span stops being tracked while the remainders
    /// survive - so both reconcile through this one queue rather than two.
    prot: [AtomicU64; MAX_PENDING_RELEASES],
}

impl PendingReleaseQueue {
    const fn new() -> Self {
        Self {
            state: [const { AtomicU64::new(RELEASE_SLOT_EMPTY) }; MAX_PENDING_RELEASES],
            process_handle: [const { AtomicU64::new(0) }; MAX_PENDING_RELEASES],
            process_generation: [const { AtomicU64::new(0) }; MAX_PENDING_RELEASES],
            start: [const { AtomicU64::new(0) }; MAX_PENDING_RELEASES],
            end: [const { AtomicU64::new(0) }; MAX_PENDING_RELEASES],
            prot: [const { AtomicU64::new(0) }; MAX_PENDING_RELEASES],
        }
    }

    /// Parks one unconfirmed release. `false` means the queue is full, which is
    /// a terminal leak rather than a deferral.
    fn park(
        &self,
        process_handle: u64,
        process_generation: u64,
        start: u64,
        end: u64,
        prot: u64,
    ) -> bool {
        for index in 0..MAX_PENDING_RELEASES {
            // ORDERING: AcqRel claims one empty slot exclusively; a losing
            // racer observes the winner's claim and moves on.
            if self.state[index]
                .compare_exchange(
                    RELEASE_SLOT_EMPTY,
                    RELEASE_SLOT_WRITING,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue;
            }
            self.process_handle[index].store(process_handle, Ordering::Relaxed);
            self.process_generation[index].store(process_generation, Ordering::Relaxed);
            self.start[index].store(start, Ordering::Relaxed);
            self.end[index].store(end, Ordering::Relaxed);
            self.prot[index].store(prot, Ordering::Relaxed);
            // ORDERING: Release publishes the complete payload to the drain
            // side, which reads the fields only after its own Acquire claim.
            self.state[index].store(RELEASE_SLOT_READY, Ordering::Release);
            return true;
        }
        false
    }

    /// Claims one published slot and frees it before the caller retries.
    ///
    /// The slot is returned to the pool *before* the notification is re-sent,
    /// so reconciliation never holds queue capacity across a service call.
    fn claim(&self, index: usize) -> Option<(u64, u64, u64, u64, u64)> {
        // ORDERING: AcqRel claims one published slot and observes its payload.
        self.state[index]
            .compare_exchange(
                RELEASE_SLOT_READY,
                RELEASE_SLOT_DRAINING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .ok()?;
        let claimed = (
            self.process_handle[index].load(Ordering::Relaxed),
            self.process_generation[index].load(Ordering::Relaxed),
            self.start[index].load(Ordering::Relaxed),
            self.end[index].load(Ordering::Relaxed),
            self.prot[index].load(Ordering::Relaxed),
        );
        // ORDERING: Release returns the slot to a later parker.
        self.state[index].store(RELEASE_SLOT_EMPTY, Ordering::Release);
        Some(claimed)
    }
}

static PENDING_RELEASES: PendingReleaseQueue = PendingReleaseQueue::new();

/// Releases parked for reconciliation, and releases lost because the queue was
/// full. A nonzero `pager-release-queue-overflow` means pagerd is holding
/// regions ring0 can no longer name: a real leak, reported as one.
static PARKED_RELEASES: AtomicU64 = AtomicU64::new(0);
static LOST_RELEASES: AtomicU64 = AtomicU64::new(0);
/// Edits pagerd refused because a split had no free slot. Counted separately
/// from every other refusal because it is the only retryable one.
static SPLIT_HEADROOM_REFUSALS: AtomicU64 = AtomicU64::new(0);

/// Parks one unconfirmed range edit and accounts for it.
fn park_pending_release(
    process_handle: u64,
    process_generation: u64,
    start: u64,
    end: u64,
    prot: u64,
) -> bool {
    if PENDING_RELEASES.park(process_handle, process_generation, start, end, prot) {
        record_admission_class(&PARKED_RELEASES, "pager-release-parked", start);
        return true;
    }
    record_admission_class(
        &LOST_RELEASES,
        pager_pressure_name(rustos_user_abi::pager::PAGER_PRESSURE_RELEASE_QUEUE_FULL),
        start,
    );
    false
}

/// Re-sends every parked range edit once. A slot returns to the queue when the
/// retry also fails, so reconciliation is bounded per call and never loops.
fn drain_pending_releases() {
    for index in 0..MAX_PENDING_RELEASES {
        let Some((process_handle, process_generation, start, end, prot)) =
            PENDING_RELEASES.claim(index)
        else {
            continue;
        };
        let resent = if prot == 0 {
            send_release(process_handle, process_generation, start, end)
        } else {
            send_protect(process_handle, process_generation, start, end, prot as u32)
        };
        if resent.is_err() {
            // pagerd is still refusing. Park it again rather than dropping it;
            // a re-sent edit that already applied matches no region and is a
            // no-op, so replay is idempotent.
            let _ = park_pending_release(process_handle, process_generation, start, end, prot);
        }
    }
}

/// Sends one range edit to pagerd and reports whether it applied.
///
/// A refusal carries its cause in `value1`, so the caller can tell a retryable
/// split-headroom refusal from a malformed range without guessing. That
/// distinction is the difference between reconciling and leaking: a
/// `REGION_SPLIT_NO_SLOT` refusal means pagerd deliberately kept the *whole*
/// region rather than losing a remainder, and a later retry will settle it.
fn send_range_edit(op: u16, payload: &[u8]) -> Result<(), i64> {
    if fault_endpoint().is_none() {
        // The transport is gone. pagerd cannot be holding a region for a
        // process whose pager no longer exists, so this is not a leak.
        return Ok(());
    }
    let Some(snapshot) = multitask::current_user_snapshot() else {
        return Err(LINUX_EINVAL);
    };
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_PAGERD;
    request.header.op = op;
    request.header.service_id = IPC_SERVICE_PAGERD;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    let payload_len = u32::try_from(payload.len()).map_err(|_| LINUX_EINVAL)?;
    request.payload[..payload.len()].copy_from_slice(payload);
    request.payload_len = payload_len;
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_PAGERD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    )?;
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    if response.header.protocol != COMMERCIAL_MAX_PROTOCOL_PAGERD
        || response.header.op != op
        || response.status != 0
    {
        if response.value1 == u64::from(PAGER_PRESSURE_REGION_SPLIT_NO_SLOT) {
            record_admission_class(
                &SPLIT_HEADROOM_REFUSALS,
                pager_pressure_name(PAGER_PRESSURE_REGION_SPLIT_NO_SLOT),
                response.value1,
            );
        }
        return Err(LINUX_EIO);
    }
    Ok(())
}

/// Sends one release notification. Errors are the caller's to reconcile.
fn send_release(
    process_handle: u64,
    process_generation: u64,
    start: u64,
    end: u64,
) -> Result<(), i64> {
    let release = rustos_user_abi::pager::PagerReleaseRangeWire {
        version: rustos_user_abi::pager::PAGER_FAULT_ABI_VERSION,
        reserved0: [0; 3],
        process_handle,
        process_generation,
        start,
        end,
        reserved1: [0; 2],
    };
    if !release.is_canonical() {
        return Err(LINUX_EINVAL);
    }
    send_range_edit(COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT, as_bytes(&release))
}

/// Sends one protection-narrowing notification.
///
/// pagerd answers a fault with the protection it holds, so a narrowing ring0
/// applied but never published here would let the pager grant rights the
/// process no longer has.
fn send_protect(
    process_handle: u64,
    process_generation: u64,
    start: u64,
    end: u64,
    prot: u32,
) -> Result<(), i64> {
    let protect = PagerProtectRangeWire {
        version: rustos_user_abi::pager::PAGER_FAULT_ABI_VERSION,
        reserved0: 0,
        prot,
        process_handle,
        process_generation,
        start,
        end,
        reserved1: [0; 2],
    };
    if !protect.is_canonical() {
        return Err(LINUX_EINVAL);
    }
    send_range_edit(COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT, as_bytes(&protect))
}

/// Tells pagerd that ring0 has released its VMA slot for an exact range.
///
/// The identity comes from the unmap that released the slot, so a release can
/// never name a different process than the publication did. A failure is
/// parked for reconciliation instead of being discarded; the return value says
/// whether pagerd has confirmed, so the caller does not have to guess.
pub(super) fn release_anonymous_region(
    process_handle: u64,
    process_generation: u64,
    start: u64,
    end: u64,
) -> Result<(), i64> {
    match send_release(process_handle, process_generation, start, end) {
        Ok(()) => Ok(()),
        Err(errno) => {
            let _ = park_pending_release(process_handle, process_generation, start, end, 0);
            Err(errno)
        }
    }
}

/// Tells pagerd that ring0 has narrowed protection over an exact range.
///
/// Ring0 has already applied the change, so a failed notification must not
/// fail `mprotect`. It is parked for reconciliation exactly like a release,
/// and re-sent from the next admission.
pub(super) fn protect_anonymous_region(
    process_handle: u64,
    process_generation: u64,
    start: u64,
    end: u64,
    prot: u32,
) -> Result<(), i64> {
    match send_protect(process_handle, process_generation, start, end, prot) {
        Ok(()) => Ok(()),
        Err(errno) => {
            let _ = park_pending_release(
                process_handle,
                process_generation,
                start,
                end,
                u64::from(prot),
            );
            Err(errno)
        }
    }
}

/// Admits one anonymous range into pagerd so its pages fault on first touch.
///
/// On [`AnonymousAdmission::Demand`] the range is published and pagerd holds
/// matching policy state. Every other outcome leaves no published VMA, so the
/// caller may still map the range eagerly - but only
/// [`AnonymousAdmission::Eager`] means it *should*.
pub(super) fn admit_anonymous_region(
    target_pid: u64,
    start: u64,
    end: u64,
    prot: u64,
) -> AnonymousAdmission {
    let Some(rights) = pager_rights(prot) else {
        // The broker already rejected W+X and empty protection before it got
        // here, so this is unreachable rather than ordinary. Fail closed.
        return AnonymousAdmission::Failed(LINUX_EINVAL);
    };
    let Some(endpoint) = fault_endpoint() else {
        record_admission_class(
            &WIRED_TRANSPORT_ABSENT,
            "pager-wired-transport-absent",
            target_pid,
        );
        return AnonymousAdmission::Eager(EagerByContract::PagerTransportAbsent);
    };
    if in_pager_control_graph(target_pid) {
        record_admission_class(
            &WIRED_CONTROL_GRAPH,
            "pager-wired-control-graph",
            target_pid,
        );
        return AnonymousAdmission::Eager(EagerByContract::PagerControlGraph);
    }

    // Reconcile before admitting. A release that failed earlier still holds a
    // pagerd region, and that dead region is exactly what refuses this range
    // as an overlap and eventually fills the table.
    drain_pending_releases();

    // A boot that has not yet proven an epoch starts at pagerd's initial one;
    // a wrong guess costs exactly one bounded retry, never a loop.
    let mut epoch = OBSERVED_PAGER_EPOCH.load(Ordering::Relaxed).max(1);
    for attempt in 0..2 {
        let Some(template) = region_template(start, end, rights, epoch, endpoint) else {
            return AnonymousAdmission::Failed(LINUX_EINVAL);
        };
        let published = match multitask::publish_pager_vma_for_process(target_pid, template) {
            Ok(published) => published,
            // The bounded per-process VMA table is full. Wire the range and
            // count it; see `EagerByContract::ProcessVmaCapacity`.
            Err(multitask::PagerVmaError::Pressure) => {
                record_admission_class(
                    &WIRED_PROCESS_VMA_CAPACITY,
                    "pager-wired-process-vma-capacity",
                    target_pid,
                );
                return AnonymousAdmission::Eager(EagerByContract::ProcessVmaCapacity);
            }
            // A stale publication still covers this range. Wire it and name
            // the exact address; see `EagerByContract::StaleRegionOverlap`.
            Err(multitask::PagerVmaError::Overlap) => {
                record_admission_class(
                    &WIRED_STALE_REGION_OVERLAP,
                    "pager-wired-stale-region-overlap",
                    start,
                );
                return AnonymousAdmission::Eager(EagerByContract::StaleRegionOverlap);
            }
            // A stale process identity or a malformed template. Neither is a
            // capacity bound, so neither may be hidden behind one.
            Err(_) => {
                record_admission_class(
                    &ADMISSION_PUBLISH_FAILED,
                    "pager-admission-publish-failed",
                    start,
                );
                return AnonymousAdmission::Failed(LINUX_ENOMEM);
            }
        };
        match admit_call(published) {
            Ok((0, proven)) => {
                observe_epoch(proven);
                return AnonymousAdmission::Demand;
            }
            Ok((status, proven)) => {
                // pagerd refused this template. Withdraw the exact generation
                // before deciding whether a proven newer epoch is worth one
                // more attempt.
                let _ = multitask::revoke_pager_vma_for_process(
                    target_pid,
                    published.start,
                    published.vma_generation,
                );
                observe_epoch(proven);
                if attempt == 0 && proven != 0 && proven != epoch {
                    epoch = proven;
                    continue;
                }
                // pagerd refused for a reason a retry cannot fix: its bounded
                // region table is full. Wire the range and count it, rather
                // than failing an ordinary mapping because of residue left by
                // some other process. The counter is the signal that used to
                // be missing entirely.
                record_admission_class(
                    &WIRED_PAGER_REGION_CAPACITY,
                    "pager-wired-region-capacity",
                    u64::from(status.unsigned_abs()),
                );
                let _ = ADMISSION_REFUSED.fetch_add(1, Ordering::Relaxed);
                return AnonymousAdmission::Eager(EagerByContract::PagerRegionCapacity);
            }
            Err(errno) => {
                let _ = multitask::revoke_pager_vma_for_process(
                    target_pid,
                    published.start,
                    published.vma_generation,
                );
                record_admission_class(
                    &ADMISSION_TRANSPORT_FAILED,
                    "pager-admission-transport-failed",
                    u64::from(errno.unsigned_abs()),
                );
                return AnonymousAdmission::Failed(errno);
            }
        }
    }
    record_admission_class(
        &ADMISSION_EPOCH_EXHAUSTED,
        "pager-admission-epoch-exhausted",
        start,
    );
    AnonymousAdmission::Failed(LINUX_ENOMEM)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The queue must be a bound, not a silent sink.
    ///
    /// A release that cannot be parked means pagerd is holding a region ring0
    /// can no longer name. That is a real leak, and the whole point of this
    /// change is that it is reported as one instead of discarded.
    #[test]
    fn pager_rights_rejects_write_execute_and_unknown_protection() {
        assert_eq!(pager_rights(linux_abi::PROT_READ), Some(VM_PROT_READ));
        assert_eq!(
            pager_rights(linux_abi::PROT_READ | linux_abi::PROT_WRITE),
            Some(VM_PROT_READ | VM_PROT_WRITE)
        );
        assert_eq!(
            pager_rights(linux_abi::PROT_WRITE | linux_abi::PROT_EXEC),
            None
        );
        assert_eq!(pager_rights(0), None);
        assert_eq!(pager_protection(0), Some(0));
        assert_eq!(pager_rights(1 << 20), None);
    }

    #[test]
    fn region_template_is_unbound_and_carries_object_authority() {
        let endpoint = PagerEndpointCapabilityWire {
            slot: 5,
            generation: 7,
            rights: 1,
        };
        let template =
            region_template(0x4000, 0x8000, VM_PROT_READ, 3, endpoint).expect("template");
        // kernel-ps is the only writer allowed to stamp these.
        assert_eq!(template.vma_generation, 0);
        assert_eq!(template.process_handle, 0);
        assert_eq!(template.process_generation, 0);
        assert_eq!(template.mm_generation, 0);
        assert!(template.object.has_authority());
        assert_eq!(template.object.pager_epoch, 3);
        assert_eq!(template.sharing, VM_SHARING_PRIVATE);
    }

    #[test]
    fn anonymous_object_slots_never_repeat_within_a_boot() {
        let endpoint = PagerEndpointCapabilityWire {
            slot: 5,
            generation: 7,
            rights: 1,
        };
        let first = region_template(0x4000, 0x8000, VM_PROT_READ, 1, endpoint).expect("first");
        let second = region_template(0x4000, 0x8000, VM_PROT_READ, 1, endpoint).expect("second");
        assert_ne!(first.object.slot, second.object.slot);
    }
    #[test]
    fn the_release_queue_is_bounded_and_publishes_every_parked_payload() {
        let queue = PendingReleaseQueue::new();
        for index in 0..MAX_PENDING_RELEASES {
            assert!(
                queue.park(7, 9, 0x4000 + index as u64 * 0x1000, 0x5000, 0),
                "slot {index} was refused below capacity"
            );
        }
        assert!(
            !queue.park(7, 9, 0xdead_0000, 0xdead_1000, 0),
            "an overfull queue silently accepted another release"
        );
        let ready = queue
            .state
            .iter()
            .filter(|state| state.load(Ordering::Acquire) == RELEASE_SLOT_READY)
            .count();
        assert_eq!(ready, MAX_PENDING_RELEASES);
    }

    /// Freeing the slot before the retry is what keeps reconciliation from
    /// holding queue capacity across a service call.
    #[test]
    fn a_claimed_release_slot_is_free_before_its_retry_runs() {
        let queue = PendingReleaseQueue::new();
        assert!(queue.park(11, 13, 0x8000, 0x9000, 0));
        assert_eq!(queue.claim(0), Some((11, 13, 0x8000, 0x9000, 0)));
        assert_eq!(
            queue.state[0].load(Ordering::Acquire),
            RELEASE_SLOT_EMPTY,
            "a claimed slot stayed reserved across its retry"
        );
        assert_eq!(queue.claim(0), None, "a drained slot was claimed twice");
        assert!(
            queue.park(17, 19, 0xa000, 0xb000, 0),
            "the drained slot was not available to a later parker"
        );
        assert_eq!(queue.claim(0), Some((17, 19, 0xa000, 0xb000, 0)));
    }

    /// Both range edits reconcile through one queue, and a parked entry says
    /// which kind it is. A protection narrowing that pagerd never received is
    /// the same class of leak as a release that never arrived: the two
    /// replicas keep different maps until it is re-sent.
    #[test]
    fn the_reconciliation_queue_distinguishes_a_release_from_a_protect() {
        let queue = PendingReleaseQueue::new();
        assert!(queue.park(11, 13, 0x8000, 0x9000, 0));
        assert!(queue.park(11, 13, 0x9000, 0xa000, u64::from(VM_PROT_READ)));
        assert_eq!(
            queue.claim(0),
            Some((11, 13, 0x8000, 0x9000, 0)),
            "a release parks with no replacement protection"
        );
        assert_eq!(
            queue.claim(1),
            Some((11, 13, 0x9000, 0xa000, u64::from(VM_PROT_READ))),
            "a protect parks with the exact protection it narrowed to"
        );
    }

    /// The capacity relation between the two replicas, asserted where both
    /// constants are visible. These were three independent `64`s with no
    /// declared relationship until a fault-slot table twice the size of its
    /// wired frame reserve drained the reserve and killed a user thread.
    #[test]
    fn the_published_capacities_state_their_relation_to_each_other() {
        assert_eq!(
            crate::memory::frame_capability::MAX_PREALLOCATED_PAGER_FAULT_FRAMES,
            PAGER_WIRED_FAULT_FRAMES
        );
        assert_eq!(
            crate::memory::frame_capability::MAX_PAGER_FRAME_GRANTS,
            PAGER_MAX_FRAME_GRANTS
        );
        assert!(PAGER_WIRED_FAULT_FRAMES >= PAGER_MAX_FAULT_SLOTS);
        assert!(
            rustos_user_abi::pager::PAGER_MIN_FULLY_TRACKED_PROCESSES >= 2,
            "one process filling its VMA table must not wedge every other process"
        );
        assert_eq!(
            rustos_user_abi::pager::PAGER_MAX_TRACKED_REGIONS,
            PAGER_MAX_VMAS_PER_PROCESS * rustos_user_abi::pager::PAGER_MIN_FULLY_TRACKED_PROCESSES
        );
    }

    #[test]
    fn observed_epoch_only_advances() {
        OBSERVED_PAGER_EPOCH.store(4, Ordering::Relaxed);
        observe_epoch(2);
        assert_eq!(OBSERVED_PAGER_EPOCH.load(Ordering::Relaxed), 4);
        observe_epoch(9);
        assert_eq!(OBSERVED_PAGER_EPOCH.load(Ordering::Relaxed), 9);
        observe_epoch(0);
        assert_eq!(OBSERVED_PAGER_EPOCH.load(Ordering::Relaxed), 9);
    }
}
