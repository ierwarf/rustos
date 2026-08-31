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
    PagerEndpointCapabilityWire, PagerObjectIdentityWire, PagerVmRegionWire, VM_OBJECT_ANONYMOUS,
    VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE, VM_SHARING_PRIVATE,
};
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT, COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_PAGERD,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, IPC_SERVICE_PAGERD,
    IPC_SERVICE_ROOTD, IPC_SERVICE_STORAGED, IPC_SERVICE_VFSD,
};

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
static PAGER_ADMISSION_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

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
fn in_pager_control_graph(target_pid: u64) -> bool {
    PAGER_CONTROL_GRAPH
        .iter()
        .any(|service_id| ipc_ops::process_owns_live_service_endpoint(target_pid, *service_id))
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

    let attempt = PAGER_ADMISSION_ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1;
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "pager-backing-admission-begin",
        region.process_handle,
        region.start,
    );
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_PAGERD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    )?;
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "pager-backing-admission-complete",
        region.process_handle,
        region.start,
    );
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

/// Tells pagerd that ring0 has released its VMA slot for an exact range.
///
/// Ring0 frees the slot itself on unmap. Without this notification pagerd would
/// keep a dead region forever: it refuses to re-admit the same range as an
/// overlap, and its fixed table eventually fills and starts refusing every
/// admission, silently downgrading demand paging to eager mapping.
/// The identity comes from the unmap that released the slot, so a release can
/// never name a different process than the publication did.
pub(super) fn release_anonymous_region(
    process_handle: u64,
    process_generation: u64,
    start: u64,
    end: u64,
) {
    let release = rustos_user_abi::pager::PagerReleaseRangeWire {
        version: rustos_user_abi::pager::PAGER_FAULT_ABI_VERSION,
        reserved0: [0; 3],
        process_handle,
        process_generation,
        start,
        end,
        reserved1: [0; 2],
    };
    if !release.is_canonical() || fault_endpoint().is_none() {
        return;
    }
    let Some(snapshot) = multitask::current_user_snapshot() else {
        return;
    };
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_PAGERD;
    request.header.op = COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT;
    request.header.service_id = IPC_SERVICE_PAGERD;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    let payload = as_bytes(&release);
    let Ok(payload_len) = u32::try_from(payload.len()) else {
        return;
    };
    request.payload[..payload.len()].copy_from_slice(payload);
    request.payload_len = payload_len;
    let _ = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_PAGERD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    );
}

/// Admits one anonymous range into pagerd so its pages fault on first touch.
///
/// On success the range is published and pagerd holds matching policy state.
/// Every failure path leaves no published VMA, so the caller may still map the
/// range eagerly without risking a duplicate region.
pub(super) fn admit_anonymous_region(
    target_pid: u64,
    start: u64,
    end: u64,
    prot: u64,
) -> Result<(), i64> {
    let rights = pager_rights(prot).ok_or(LINUX_EINVAL)?;
    let endpoint = fault_endpoint().ok_or(LINUX_ENOSYS)?;
    if in_pager_control_graph(target_pid) {
        return Err(LINUX_ENOSYS);
    }

    // A boot that has not yet proven an epoch starts at pagerd's initial one;
    // a wrong guess costs exactly one bounded retry, never a loop.
    let mut epoch = OBSERVED_PAGER_EPOCH.load(Ordering::Relaxed).max(1);
    for attempt in 0..2 {
        let template = region_template(start, end, rights, epoch, endpoint).ok_or(LINUX_EINVAL)?;
        let published = multitask::publish_pager_vma_for_process(target_pid, template)
            .map_err(|_| LINUX_ENOMEM)?;
        match admit_call(published) {
            Ok((0, proven)) => {
                observe_epoch(proven);
                return Ok(());
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
                // pagerd refused for a reason a retry cannot fix, most often a
                // full region table. The caller falls back to eager mapping,
                // which is correct but silently disables demand paging, so the
                // downgrade has to be observable rather than invisible.
                nucleus_core::debug::record_milestone(
                    nucleus_core::debug::LogCategory::Compat,
                    "pager-backing-admission-refused",
                    u64::from(status.unsigned_abs()),
                    start,
                );
                return Err(LINUX_ENOSYS);
            }
            Err(errno) => {
                let _ = multitask::revoke_pager_vma_for_process(
                    target_pid,
                    published.start,
                    published.vma_generation,
                );
                return Err(errno);
            }
        }
    }
    Err(LINUX_ENOSYS)
}

#[cfg(test)]
mod tests {
    use super::*;

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
