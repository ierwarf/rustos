//! Anonymous demand-paging admission for the production MM broker.
//!
//! - **Owner:** ring0 owns anonymous paging end to end. An anonymous object has
//!   no backing store and no external owner, so supplying its zeroed page is a
//!   mechanism, not a policy. This module stamps an exact kernel-authored
//!   region and publishes it; `compat::pager::serve_anonymous_first_touch`
//!   then answers every fault against it in the faulting task's own context.
//!   `pagerd` keeps the pager-backed cases - load ownership, COW, dirty
//!   writeback, eviction, provider restart - which is where its policy lives.
//! - **Boundary:** The pagerd endpoint identity is untrusted and is carried in
//!   the region only as the fault capability a pager-backed object would use.
//!   The region template carries zeroed process and VMA generations so neither
//!   `syscalld` nor `pagerd` can forge MM authority; `kernel-ps` stamps them
//!   from live publications.
//! - **Lifecycle:** Resolve endpoint -> publish stamped VMA -> the range is
//!   faultable. There is one map, held by `kernel-ps`, so there is no second
//!   replica to keep in agreement and no admission, release, or protect
//!   round trip on the `mmap`, `munmap`, or `mprotect` path.
//! - **Concurrency:** Publication takes the target process-state lock and
//!   returns before anything else runs; no MM lock is held across IPC, because
//!   this path performs no IPC.
//! - **Failure:** An absent pagerd endpoint and per-process VMA pressure both
//!   fail closed onto the eager bootstrap mapping path, which is still
//!   correct, rather than leaving a partially published region.
//! - **Forbidden:** No pageable admission for the wired pager control graph, no
//!   PID-only authority, and no W+X region.
//! - **Evidence:** `pager-vma-publication`, `pager-fault-slot-lifecycle`, the
//!   focused tests below, and the `pager-admission-*` implementation
//!   mutations.

use super::*;

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_user_abi::pager::{
    PAGER_MAX_FAULT_SLOTS, PAGER_MAX_FRAME_GRANTS, PAGER_MAX_VMAS_PER_PROCESS,
    PAGER_WIRED_FAULT_FRAMES, PagerAnonymousPolicyWire, PagerEndpointCapabilityWire,
    PagerObjectIdentityWire, PagerVmRegionWire, VM_OBJECT_ANONYMOUS, VM_PROT_EXECUTE, VM_PROT_READ,
    VM_PROT_WRITE, VM_SHARING_PRIVATE,
};
use rustos_user_abi::syscall::IPC_SERVICE_PAGERD;

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

/// Services that resolve or carry faults.
///

/// Epoch stamped into every ring0-owned anonymous object.
///
/// The wire field is named for the pager because a pager-backed object's epoch
/// is that pager's restart generation. An anonymous object has no pager, so
/// ring0 owns the field and holds it fixed: what actually invalidates a stale
/// anonymous region is the VMA generation, which `kernel-ps` bumps on every
/// publication, revocation, and protection rewrite, and which every fault
/// revalidates.
const RING0_ANONYMOUS_EPOCH: u64 = 1;

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
    // A zero protection is still a reservation. Access is denied by the VMA
    // commit/protection state, but the interval must remain authoritative.
    pager_protection(prot)
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
fn in_pager_control_graph(target_pid: u64, policy: PagerAnonymousPolicyWire) -> bool {
    // Which services stay wired is a judgement about boot-time memory
    // behaviour, not a mechanism, so the list comes from the published policy
    // rather than a ring0 constant. Ring0 answers anonymous faults itself now,
    // so the recursive-fault cycle the list was originally built for cannot
    // form; keeping it is a conservative hold, which is exactly the kind of
    // decision a pager should be able to change without a kernel edit.
    policy
        .wired_services
        .iter()
        .copied()
        .take_while(|service_id| *service_id != 0)
        .any(|service_id| ipc_ops::process_owns_published_service_endpoint(target_pid, service_id))
}

/// Why an anonymous range is wired instead of demand-backed. Each variant is
/// a contract, not a downgrade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EagerByContract {
    /// pagerd has published no fault endpoint yet.
    ///
    /// A region carries a fault capability as part of its canonical form, so
    /// one cannot be stamped before that endpoint exists. Ring0 no longer needs
    /// pagerd to *serve* an anonymous fault; it still needs the identity to
    /// publish a well-formed region, and everything mapped before that point is
    /// wired exactly as it always was.
    PagerTransportAbsent,
    /// The target is a member of the wired pager control graph.
    PagerControlGraph,
    /// The published policy admits nothing to demand paging.
    PolicyDisabled,
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
    /// Published; first touch faults and ring0 answers it.
    Demand,
    /// Wired because that is this target's contract.
    Eager(EagerByContract),
    /// This range could not be published for a reason that is not a declared
    /// bound. The caller must surface this rather than fabricate success.
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
static WIRED_POLICY_DISABLED: AtomicU64 = AtomicU64::new(0);
static WIRED_PROCESS_VMA_CAPACITY: AtomicU64 = AtomicU64::new(0);
static WIRED_STALE_REGION_OVERLAP: AtomicU64 = AtomicU64::new(0);
static ADMISSION_PUBLISH_FAILED: AtomicU64 = AtomicU64::new(0);

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

/// Stable low-byte classification for a pager VMA publication failure.
///
/// Keep this mapping explicit: collapsing every failure to `ENOMEM` made a
/// stale fixed process slot look like capacity pressure and hid the lifecycle
/// defect during the first COW fork acceptance run.
fn pager_vma_error_class(error: multitask::PagerVmaError) -> u64 {
    match error {
        multitask::PagerVmaError::Malformed => 1,
        multitask::PagerVmaError::Overlap => 2,
        multitask::PagerVmaError::Pressure => 3,
        multitask::PagerVmaError::Stale => 4,
        multitask::PagerVmaError::Unstable => 5,
        multitask::PagerVmaError::Denied => 6,
    }
}

/// Publishes a bounded failure milestone without discarding the exact cause.
///
/// `arg0` packs `(occurrence << 8) | class`, while `arg1` carries the failed
/// start address. The first occurrence and every 64th remain the only records,
/// so preserving the cause does not turn a hot failure into an unbounded log.
fn record_publish_failure(start: u64, error: multitask::PagerVmaError) {
    let count = ADMISSION_PUBLISH_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 || count.is_multiple_of(64) {
        let occurrence_and_class = count.saturating_mul(1 << 8) | pager_vma_error_class(error);
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "pager-admission-publish-failed",
            occurrence_and_class,
            start,
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
        commit_state: rustos_user_abi::pager::VM_COMMIT_COMMITTED,
        vma_generation: 0,
        process_handle: 0,
        process_generation: 0,
        mm_generation: 0,
        fault_endpoint: endpoint,
        reserved1: [0; 2],
    })
}

/// Publishes one anonymous range so its pages fault on first touch.
///
/// On [`AnonymousAdmission::Demand`] the range is published and ring0 holds the
/// only map of it. Every other outcome leaves no published VMA, so the caller
/// may still map the range eagerly - but only [`AnonymousAdmission::Eager`]
/// means it *should*.
///
/// This performs no IPC. It used to send a synchronous admission call to a
/// single serial pagerd and wait for its reply, which is what put a 5.7 ms p99
/// on an `mmap` with a 60 us median: the pager could be parked inside a fault
/// rendezvous, and nothing on the control path inherited the caller's priority.
/// With anonymous faults answered in ring0, the pager holds no state this range
/// needs, so there is nothing to tell it and nothing to wait for.
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
    admit_anonymous_rights(target_pid, start, end, rights)
}

/// Produces the unbound child template for one canonical parent VMA.
///
/// Every reservation attribute is inherited verbatim, including `PROT_NONE`,
/// reserved/committed state, file identity, offset, sharing, and fault
/// endpoint. Only process/VMA authority stamps are cleared. Anonymous-private
/// objects receive the supplied fresh identity so untouched zero-fill pages do
/// not accidentally join the parent's anonymous object.
fn inherited_region_template(
    parent: PagerVmRegionWire,
    anonymous_identity: Option<(u64, PagerEndpointCapabilityWire)>,
) -> Result<PagerVmRegionWire, i64> {
    if !parent.is_canonical()
        || parent.object.object_type == VM_OBJECT_ANONYMOUS && parent.sharing != VM_SHARING_PRIVATE
    {
        return Err(LINUX_EINVAL);
    }

    let mut template = parent;
    template.vma_generation = 0;
    template.process_handle = 0;
    template.process_generation = 0;
    template.mm_generation = 0;
    if template.object.object_type == VM_OBJECT_ANONYMOUS {
        let (slot, endpoint) = anonymous_identity.ok_or(LINUX_ENOMEM)?;
        if slot == 0 {
            return Err(LINUX_ENOMEM);
        }
        template.object.backing_service = 0;
        template.object.slot = slot;
        template.object.generation = 1;
        template.object.pager_epoch = RING0_ANONYMOUS_EPOCH;
        template.object.backing_generation = 1;
        template.fault_endpoint = endpoint;
    }
    Ok(template)
}

pub(super) fn admit_inherited_region(
    target_pid: u64,
    parent: rustos_user_abi::pager::PagerVmRegionWire,
) -> Result<PagerVmRegionWire, i64> {
    let policy = crate::pager_policy::anonymous_policy();
    if policy.demand_enabled == 0 || in_pager_control_graph(target_pid, policy) {
        return Err(LINUX_ENOMEM);
    }

    let anonymous_identity = if parent.object.object_type == VM_OBJECT_ANONYMOUS {
        let slot = NEXT_ANON_OBJECT_SLOT.fetch_add(1, Ordering::Relaxed);
        let endpoint = fault_endpoint().ok_or(LINUX_ENOMEM)?;
        Some((slot, endpoint))
    } else {
        None
    };
    let template = inherited_region_template(parent, anonymous_identity)?;

    multitask::publish_inherited_pager_vma_for_process(
        target_pid,
        template,
        policy.process_vma_ceiling as usize,
    )
    .map_err(|error| {
        record_publish_failure(parent.start, error);
        LINUX_ENOMEM
    })
}

fn admit_anonymous_rights(
    target_pid: u64,
    start: u64,
    end: u64,
    rights: u32,
) -> AnonymousAdmission {
    let policy = crate::pager_policy::anonymous_policy();
    let Some(endpoint) = fault_endpoint() else {
        record_admission_class(
            &WIRED_TRANSPORT_ABSENT,
            "pager-wired-transport-absent",
            target_pid,
        );
        return AnonymousAdmission::Eager(EagerByContract::PagerTransportAbsent);
    };

    let eager_reason = if policy.demand_enabled == 0 {
        Some((
            EagerByContract::PolicyDisabled,
            &WIRED_POLICY_DISABLED,
            "pager-wired-policy-disabled",
        ))
    } else if in_pager_control_graph(target_pid, policy) {
        Some((
            EagerByContract::PagerControlGraph,
            &WIRED_CONTROL_GRAPH,
            "pager-wired-control-graph",
        ))
    } else {
        None
    };

    let Some(template) = region_template(start, end, rights, RING0_ANONYMOUS_EPOCH, endpoint)
    else {
        return AnonymousAdmission::Failed(LINUX_EINVAL);
    };
    match multitask::publish_pager_vma_for_process(
        target_pid,
        template,
        policy.process_vma_ceiling as usize,
    ) {
        Ok(_) => {
            if let Some((reason, counter, milestone)) = eager_reason {
                record_admission_class(counter, milestone, target_pid);
                AnonymousAdmission::Eager(reason)
            } else {
                AnonymousAdmission::Demand
            }
        }
        // Once this table is the reservation authority, exhausting it must
        // refuse the mapping. Wiring without a publication would recreate the
        // split authority this path is removing.
        Err(multitask::PagerVmaError::Pressure) => {
            record_admission_class(
                &WIRED_PROCESS_VMA_CAPACITY,
                "pager-vma-capacity-refused",
                target_pid,
            );
            AnonymousAdmission::Failed(LINUX_ENOMEM)
        }
        // MAP_FIXED handles one explicit withdraw-and-retry in the caller.
        Err(multitask::PagerVmaError::Overlap) => {
            record_admission_class(&WIRED_STALE_REGION_OVERLAP, "pager-vma-overlap", start);
            AnonymousAdmission::Eager(EagerByContract::StaleRegionOverlap)
        }
        Err(error) => {
            record_publish_failure(start, error);
            AnonymousAdmission::Failed(LINUX_ENOMEM)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_parent_region() -> PagerVmRegionWire {
        let endpoint = PagerEndpointCapabilityWire {
            slot: 5,
            generation: 7,
            rights: 1,
        };
        let mut region =
            region_template(0x4000, 0x8000, VM_PROT_READ, 3, endpoint).expect("template");
        region.vma_generation = 11;
        region.process_handle = 13;
        region.process_generation = 17;
        region.mm_generation = 19;
        assert!(region.is_canonical());
        region
    }

    #[test]
    fn pager_rights_accepts_prot_none_reservations_but_rejects_wx_and_unknown_bits() {
        assert_eq!(pager_rights(linux_abi::PROT_READ), Some(VM_PROT_READ));
        assert_eq!(
            pager_rights(linux_abi::PROT_READ | linux_abi::PROT_WRITE),
            Some(VM_PROT_READ | VM_PROT_WRITE)
        );
        assert_eq!(
            pager_rights(linux_abi::PROT_WRITE | linux_abi::PROT_EXEC),
            None
        );
        // PROT_NONE still publishes one authoritative reserved interval; its
        // zero rights deny access until a later committed protection update.
        assert_eq!(pager_rights(0), Some(0));
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
    fn inherited_file_private_reservation_preserves_dual_abi_policy() {
        let mut parent = canonical_parent_region();
        parent.object.object_type = rustos_user_abi::pager::VM_OBJECT_FILE_PRIVATE;
        parent.object.rights = VM_PROT_READ | VM_PROT_WRITE;
        parent.object.backing_service = 23;
        parent.object.slot = 29;
        parent.object.generation = 31;
        parent.object.pager_epoch = 37;
        parent.object.backing_generation = 41;
        parent.object_offset = 0x20_000;
        parent.prot = 0;
        parent.commit_state = rustos_user_abi::pager::VM_COMMIT_RESERVED;
        assert!(parent.is_canonical());

        let child = inherited_region_template(parent, None).expect("file-private child template");
        assert_eq!(child.start, parent.start);
        assert_eq!(child.end, parent.end);
        assert_eq!(child.object, parent.object);
        assert_eq!(child.object_offset, parent.object_offset);
        assert_eq!(child.prot, 0);
        assert_eq!(child.sharing, parent.sharing);
        assert_eq!(
            child.commit_state,
            rustos_user_abi::pager::VM_COMMIT_RESERVED
        );
        assert_eq!(child.fault_endpoint, parent.fault_endpoint);
        assert_eq!(child.vma_generation, 0);
        assert_eq!(child.process_handle, 0);
        assert_eq!(child.process_generation, 0);
        assert_eq!(child.mm_generation, 0);
    }

    #[test]
    fn inherited_anonymous_private_gets_fresh_identity_and_rejects_shared() {
        let parent = canonical_parent_region();
        let endpoint = PagerEndpointCapabilityWire {
            slot: 43,
            generation: 47,
            rights: u64::from(VM_PROT_READ),
        };
        let child = inherited_region_template(parent, Some((53, endpoint)))
            .expect("anonymous-private child template");
        assert_ne!(child.object.slot, parent.object.slot);
        assert_eq!(child.object.slot, 53);
        assert_eq!(child.object.backing_service, 0);
        assert_eq!(child.object.rights, parent.object.rights);
        assert_eq!(child.prot, parent.prot);
        assert_eq!(child.commit_state, parent.commit_state);
        assert_eq!(child.fault_endpoint, endpoint);

        let mut shared = parent;
        shared.sharing = rustos_user_abi::pager::VM_SHARING_SHARED;
        assert!(shared.is_canonical());
        assert_eq!(
            inherited_region_template(shared, Some((59, endpoint))),
            Err(LINUX_EINVAL)
        );
    }

    #[test]
    fn pager_vma_failure_telemetry_has_stable_distinct_classes() {
        assert_eq!(
            pager_vma_error_class(multitask::PagerVmaError::Malformed),
            1
        );
        assert_eq!(pager_vma_error_class(multitask::PagerVmaError::Overlap), 2);
        assert_eq!(pager_vma_error_class(multitask::PagerVmaError::Pressure), 3);
        assert_eq!(pager_vma_error_class(multitask::PagerVmaError::Stale), 4);
        assert_eq!(pager_vma_error_class(multitask::PagerVmaError::Unstable), 5);
        assert_eq!(pager_vma_error_class(multitask::PagerVmaError::Denied), 6);
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

    /// A ring0-owned anonymous object still has to satisfy object authority,
    /// which is what a zero epoch would break. The epoch is fixed precisely
    /// because the VMA generation, not this field, is what invalidates a stale
    /// anonymous region.
    #[test]
    fn the_ring0_anonymous_epoch_carries_object_authority() {
        let endpoint = PagerEndpointCapabilityWire {
            slot: 5,
            generation: 7,
            rights: 1,
        };
        assert_ne!(RING0_ANONYMOUS_EPOCH, 0);
        let template = region_template(
            0x4000,
            0x8000,
            VM_PROT_READ,
            RING0_ANONYMOUS_EPOCH,
            endpoint,
        )
        .expect("template");
        assert!(template.object.has_authority());
        assert_eq!(template.object.object_type, VM_OBJECT_ANONYMOUS);
        assert_eq!(template.object.backing_service, 0);
    }

    /// The capacity relation between the published bounds, asserted where every
    /// constant is visible. These were three independent `64`s with no declared
    /// relationship until a fault-slot table twice the size of its wired frame
    /// reserve drained the reserve and killed a user thread.
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
}
