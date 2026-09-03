//! Owner: compat owns normal-time adoption; pagerd owns mapping policy.
//! Boundary: one worker-bound reply consumes one fixed fault and frame grant.
//! Lifecycle: claim, validate, map or reject, release donation, then wake.
//! Concurrency: token generation, worker identity, and charge token are exact.
//! Failure: stale grants or failed maps revoke authority before owner wakeup.
//! Forbidden: housekeeping dispatch, generic reply caps, and retained grants.
//! Evidence: PagerFaultSlotLifecycle, PagerFrameGrantLifecycle, focused tests.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::memory as mm_api;
use crate::multitask as ps_api;
use rustos_user_abi::pager::{
    PagerFaultDispatchWire, PagerFaultReplyWire, PagerFaultRequestWire,
};
use x86_64::{VirtAddr, structures::paging::PageTableFlags};

static FIRST_ANONYMOUS_FAULT_COMPLETED: AtomicBool = AtomicBool::new(false);
static COMPLETED_ANONYMOUS_FAULTS: AtomicU64 = AtomicU64::new(0);
static REJECTED_ANONYMOUS_FAULT_GRANTS: AtomicU64 = AtomicU64::new(0);
static REJECTED_ANONYMOUS_FAULT_MAPS: AtomicU64 = AtomicU64::new(0);
/// Pages populated around a fault rather than by a fault of their own.
static POPULATED_RUN_PAGES: AtomicU64 = AtomicU64::new(0);
/// Anonymous faults ring0 could not serve because the wired reserve was empty.
static RING0_FAULTS_WITHOUT_FRAME: AtomicU64 = AtomicU64::new(0);
/// Anonymous faults restarted because a VMA writer held the publication.
static CONTENDED_ANONYMOUS_FAULTS: AtomicU64 = AtomicU64::new(0);
/// Anonymous first touches whose access was a *read*.
///
/// Counted because it decides whether a shared zero page is worth building. A
/// read first touch is the only case a zero page helps, and Linux's own answer
/// there (map the shared page read-only, copy on the later write) needs a COW
/// path this fault context cannot perform. Measure the case before paying for
/// the machinery.
static READ_FIRST_TOUCHES: AtomicU64 = AtomicU64::new(0);

/// Pages ring0 offers to populate for this fault, including the faulting page.
///
/// Bounded by the VMA's own remaining extent, so a run never crosses into a
/// range this fault carries no authority for, and by
/// `PAGER_FAULT_RUN_PAGES_MAX`. An unresolvable region offers exactly the
/// faulting page: the guaranteed one.
fn offered_run_pages(request: PagerFaultRequestWire) -> u32 {
    use rustos_user_abi::pager::{PAGER_FAULT_RUN_PAGES_MAX, PAGER_PAGE_BYTES};

    let Ok(snapshot) = ps_api::validate_fault_request(request) else {
        return 1;
    };
    let Some(remaining) = snapshot.region.end.checked_sub(request.virtual_address) else {
        return 1;
    };
    let pages = remaining / PAGER_PAGE_BYTES;
    u32::try_from(pages.min(u64::from(PAGER_FAULT_RUN_PAGES_MAX)))
        .unwrap_or(1)
        .max(1)
}

pub(crate) fn dispatch_wire(reservation: ps_api::PagerFaultReservation) -> PagerFaultDispatchWire {
    PagerFaultDispatchWire {
        request: reservation.request,
        zeroed_frame_capability: reservation.zeroed_frame_capability,
        granted_frame_rights: reservation.granted_frame_rights,
        map_run_pages_offered: offered_run_pages(reservation.request),
        reserved1: [0; 2],
    }
}

fn frame_binding(
    reservation: ps_api::PagerFaultReservation,
) -> mm_api::frame_capability::FrameGrantBinding {
    mm_api::frame_capability::FrameGrantBinding {
        fault_token: reservation.token,
        process_generation: reservation.request.process_generation,
        mm_generation: reservation.request.mm_generation,
        vma_generation: reservation.request.vma_generation,
        pager_epoch: reservation.request.object.pager_epoch,
    }
}

fn cancel_grant(reservation: ps_api::PagerFaultReservation) {
    let _ = mm_api::frame_capability::cancel_frame_grant(
        reservation.zeroed_frame_capability,
        frame_binding(reservation),
    );
}

/// Wakes only the task still blocked on this exact fault token and publishes a
/// real reply-handoff token. A delayed pager reply cannot wake a later wait.
fn wake_fault_owner(fault_token: u64, task_id: u64) -> bool {
    ps_api::complete_pager_fault_wake_handoff(fault_token, task_id)
}

fn finish_without_mapping(reservation: ps_api::PagerFaultReservation) {
    cancel_grant(reservation);
    let _ = ps_api::consume_pager_fault_reply(reservation.token);
    let _ = wake_fault_owner(reservation.token, reservation.request.task_id);
}

fn page_flags(rights: u32) -> PageTableFlags {
    use rustos_user_abi::pager::{VM_PROT_EXECUTE, VM_PROT_WRITE};

    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if rights & VM_PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if rights & VM_PROT_EXECUTE == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

/// Serves one anonymous first-touch fault entirely inside ring0.
///
/// This is the Zircon split. An anonymous page has no backing store and no
/// external owner, so supplying a zeroed frame for it is a *mechanism*, not a
/// policy: every input the decision needs - process, MM and VMA generations,
/// object identity, and the access against `region.prot` - is already held and
/// already validated by ring0's own `lookup`. Routing it through a user pager
/// only asked another process to recompute an answer ring0 had, and then
/// revalidated the whole thing again on the way back. Zircon resolves
/// anonymous VMOs in the kernel for the same reason and keeps its user pager
/// for VMOs created by `zx_pager_create_vmo`; RustOS keeps pagerd for the same
/// pager-backed cases, which is where `page_cache.rs` policy - load ownership,
/// COW, dirty writeback, eviction, provider restart - actually lives.
///
/// The faulting task never blocks: it returns straight to the instruction that
/// faulted. There is no fault slot, no frame grant, no donation, and no reply
/// custody, so none of those resources can be exhausted by anonymous paging,
/// and no other process has to be scheduled for this fault to make progress.
///
/// `prot` comes from the snapshot the caller already took. The exact VMA
/// publication permit below prevents an unmap/protect writer from changing the
/// prepared leaf until its one atomic install has completed, so a stale `prot`
/// can never reach a PTE.
pub fn serve_anonymous_first_touch(
    request: PagerFaultRequestWire,
    prot: u32,
) -> AnonymousFaultOutcome {
    serve_anonymous_first_touch_enabled(request, prot)
}

/// What ring0 did with one anonymous fault.
///
/// `Retry` is the case that must not be folded into `Refused`. A page fault is
/// restartable: when the only thing in the way is another thread editing this
/// process's VMA table, the correct answer is to resume the faulting
/// instruction and let it fault again, not to retire the thread. Returning to
/// user mode also restores the interrupt flag the fault gate cleared, so the
/// re-fault is a real backoff - the writer gets to finish, and this CPU can be
/// preempted in between.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnonymousFaultOutcome {
    /// The page is mapped; resume the faulting instruction.
    Mapped,
    /// Transient contention with a VMA writer. Resume and re-fault.
    Retry,
    /// This fault carries no authority ring0 can satisfy. Retire the thread.
    Refused,
}

/// The IRQ-off body of `serve_anonymous_first_touch`.
///
/// Its permitted work is limited to a lock-free VMA permit, wired-frame
/// reservation, and one prepared-leaf CAS.  In particular it must not take a
/// process-state lock, enter the global TLB protocol, allocate, populate a
/// fault-around run, or enable interrupts.
fn serve_anonymous_first_touch_enabled(
    request: PagerFaultRequestWire,
    prot: u32,
) -> AnonymousFaultOutcome {
    // Acquire before taking a reserve frame.  A writer that has withdrawn this
    // exact VMA wins cleanly; no frame is removed from the reserve in that
    // case, and no sleepable process-state validation runs with IF clear.
    let (permit, region) = match ps_api::current_pager_fault_install_permit(request) {
        Ok(held) => held,
        // The range is published but a writer holds it. Restarting the
        // instruction is the whole answer: it is cheaper than any wait we
        // could do here, it releases the interrupt flag on the way out, and
        // it re-validates from scratch. Counted separately from a refusal
        // because a rising retry rate is contention, not a defect.
        Err(ps_api::PagerVmaError::Unstable) => {
            let contended = CONTENDED_ANONYMOUS_FAULTS.fetch_add(1, Ordering::Relaxed) + 1;
            if contended == 1 || contended.is_multiple_of(4096) {
                nucleus_core::debug::record_milestone(
                    nucleus_core::debug::LogCategory::Compat,
                    "pager-anon-install-contended",
                    contended,
                    request.virtual_address,
                );
            }
            return AnonymousFaultOutcome::Retry;
        }
        Err(_) => {
            let rejected = REJECTED_ANONYMOUS_FAULT_MAPS.fetch_add(1, Ordering::Relaxed) + 1;
            if rejected == 1 || rejected.is_multiple_of(16) {
                nucleus_core::debug::record_milestone(
                    nucleus_core::debug::LogCategory::Compat,
                    "pager-anon-publication-rejected",
                    rejected,
                    request.virtual_address,
                );
            }
            return AnonymousFaultOutcome::Refused;
        }
    };

    // Frame supply, fast path first.
    //
    // The wired reserve is a *latency* device, not the supply: it is lock-free
    // and already zeroed, so the common fault never touches the physical
    // allocator. What it cannot be is the only source. It holds 128 frames and
    // its producer is a scheduled task, so making every anonymous page draw
    // from it caps sustained first-touch throughput at one producer turn per
    // 128 pages - against a boot that faults roughly twelve thousand pages in
    // its first two seconds. Measured that way it emptied 11,888 times before
    // `uiserver` died allocating a stack guard page.
    //
    // Falling back to the ordinary allocator here is sound, and the lock order
    // is why. `phys::alloc_frame` takes one IRQ-safe leaf spinlock and makes no
    // cross-CPU handshake. Every holder of that lock reaches it *after* the TLB
    // protocol - `unmap_prepared_pager_fault_pages_at` frees its frames only
    // once `flush_for_reclaim` has completed the shootdown - so no CPU can hold
    // it while waiting for an acknowledgement this interrupt-disabled CPU owes
    // it. That is exactly the property `ProcessStateLock` and the TLB protocol
    // lack, and it is why those two remain forbidden here.
    let frame = match mm_api::frame_capability::take_pager_fault_frame() {
        Some(frame) => frame,
        None => {
            let refused = RING0_FAULTS_WITHOUT_FRAME.fetch_add(1, Ordering::Relaxed) + 1;
            if refused.is_multiple_of(1024) || refused == 1 {
                nucleus_core::debug::record_milestone(
                    nucleus_core::debug::LogCategory::Compat,
                    "pager-ring0-anon-reserve-empty",
                    refused,
                    request.task_id,
                );
            }
            match mm_api::frame_capability::allocate_zeroed_frame() {
                Some(frame) => frame,
                // Both sources are empty. This is real memory exhaustion, not
                // a supply dip, so retrying would spin a thread on a fault
                // nothing is going to satisfy.
                None => return AnonymousFaultOutcome::Refused,
            }
        }
    };
    let installed = mm_api::paging::map_current_prepared_pager_fault_frame_at(
        VirtAddr::new(request.virtual_address),
        frame.as_u64(),
        page_flags(prot),
    );
    // Fault-around, while the permit is still held.
    //
    // Anonymous first touch is overwhelmingly sequential, and the expensive
    // part of serving one page is no longer the mapping - it is the exception
    // entry itself. Populating the run the VMA can support turns roughly
    // twelve thousand boot faults into a few hundred. Every page past the
    // faulting one is best effort and shares this fault's exact authority: the
    // same permit, the same protection, and a run `offered_run_pages` has
    // already clipped to this VMA's own extent, so no page outside the region
    // that raised the fault can be touched.
    let populated = match installed {
        Ok(_) => populate_run_under_permit(request, prot, region.start, region.end),
        Err(_) => 0,
    };
    // This is the writer/installer handoff point.  No subsequent bookkeeping
    // may keep the VMA writer from starting its ordinary locked mutation.
    drop(permit);
    if populated != 0 {
        let total = POPULATED_RUN_PAGES.fetch_add(populated, Ordering::Relaxed) + populated;
        if total.is_multiple_of(4096) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-fault-around-pages",
                total,
                populated,
            );
        }
    }
    let mapped = match installed {
        Ok(true) => true,
        Ok(false) => {
            // Another fault won the one leaf CAS.  That mapping resolves this
            // fault too; only our unused reserve frame must be returned.
            if !mm_api::frame_capability::return_pager_fault_frame(frame) {
                mm_api::phys::free_frame(frame);
            }
            true
        }
        Err(_) => false,
    };
    if !mapped {
        // The frame was never published into any address space, so it returns
        // to the reserve it came from. If the reserve will not take it back,
        // it must still be freed rather than leaked.
        if !mm_api::frame_capability::return_pager_fault_frame(frame) {
            mm_api::phys::free_frame(frame);
        }
        // First occurrence *and* every 16th. A rejected mapping stalls exactly
        // the thread that faulted, so the one that matters most is the first;
        // reporting only every 16th is how this class stayed invisible until a
        // service failed to register several layers away.
        let rejected = REJECTED_ANONYMOUS_FAULT_MAPS.fetch_add(1, Ordering::Relaxed) + 1;
        if rejected == 1 || rejected.is_multiple_of(16) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-anon-map-rejected",
                rejected,
                request.virtual_address,
            );
        }
        return AnonymousFaultOutcome::Refused;
    }

    if request.access == rustos_user_abi::pager::VM_ACCESS_READ {
        READ_FIRST_TOUCHES.fetch_add(1, Ordering::Relaxed);
    }
    let completed = COMPLETED_ANONYMOUS_FAULTS.fetch_add(1, Ordering::Relaxed) + 1;
    if completed.is_multiple_of(1024) || completed == 1 {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "pager-anon-fault-progress",
            request.process_handle,
            request.virtual_address,
        );
    }
    if FIRST_ANONYMOUS_FAULT_COMPLETED
        // ORDERING: AcqRel publishes exactly one first-touch milestone;
        // failed Acquire observes the CPU that already published it.
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "pager-anon-first-touch-complete",
            request.virtual_address,
            request.task_id,
        );
    }
    AnonymousFaultOutcome::Mapped
}

/// Installs the pages after the faulting one, up to the run this VMA can
/// support, and reports how many were published.
///
/// Called with the fault's own publication permit still held, so every page
/// here carries exactly the authority the faulting page did. It is strictly
/// best effort: the fault is already answered, so a page it cannot serve costs
/// a future fault and nothing else. It stops at the first page that is already
/// mapped, cannot be backed, or whose prepared leaf is gone - a run that keeps
/// going past a refusal would be spending frames on a range it has stopped
/// proving anything about.
fn populate_run_under_permit(
    request: PagerFaultRequestWire,
    prot: u32,
    region_start: u64,
    region_end: u64,
) -> u64 {
    use rustos_user_abi::pager::{PAGER_FAULT_RUN_PAGES_MAX, PAGER_PAGE_BYTES};

    // Align the run down to its own size, the way Linux picks an mTHP folio:
    // `ALIGN_DOWN(address, PAGE_SIZE << order)`. Running forward from wherever
    // the fault happened makes consecutive runs overlap and re-probe pages a
    // neighbouring run already installed; aligned blocks tile the region
    // instead, so a sequential walk touches each block exactly once and a
    // re-fault inside a populated block finds it whole.
    let block_bytes = u64::from(PAGER_FAULT_RUN_PAGES_MAX) * PAGER_PAGE_BYTES;
    let block_start = (request.virtual_address / block_bytes) * block_bytes;
    let Some(block_end) = block_start.checked_add(block_bytes) else {
        return 0;
    };
    // The run may never leave the region that raised this fault: outside it
    // this fault carries no authority, and the leaves are not prepared.
    let first = block_start.max(region_start);
    let last = block_end.min(region_end);
    if last <= first {
        return 0;
    }

    let flags = page_flags(prot);
    let mut populated = 0;
    let mut address = first;
    while address < last {
        if address == request.virtual_address {
            address = address.saturating_add(PAGER_PAGE_BYTES);
            continue;
        }
        // Same two-source supply as the faulting page: the reserve is the
        // lock-free fast path, the allocator is the supply.
        //
        // Restricting the run to the reserve alone was tried and reverted. It
        // reads as the conservative choice - best-effort pages should not take
        // an allocator lock with interrupts disabled - but it inverts under
        // load. The reserve is emptiest exactly when the fault rate is highest,
        // so the run went dead precisely when it was worth most: measured, a
        // boot fell from 11.3 pages per fault to 0.001, and the faults it
        // stopped amortizing tripled to 4 059, which drained the reserve
        // harder still. The run must degrade with supply, not switch off.
        let Some(frame) = mm_api::frame_capability::take_pager_fault_frame()
            .or_else(mm_api::frame_capability::allocate_zeroed_frame)
        else {
            break;
        };
        let installed = mm_api::paging::map_current_prepared_pager_fault_frame_at(
            VirtAddr::new(address),
            frame.as_u64(),
            flags,
        );
        match installed {
            Ok(true) => populated += 1,
            // Already present. Inside an aligned block that is an ordinary
            // partially-populated block, not a reason to abandon the rest of
            // it, so return the frame and keep going.
            Ok(false) => {
                if !mm_api::frame_capability::return_pager_fault_frame(frame) {
                    mm_api::phys::free_frame(frame);
                }
            }
            // The prepared leaf is gone. This fault has stopped proving
            // anything about the rest of the block; end the run.
            Err(_) => {
                if !mm_api::frame_capability::return_pager_fault_frame(frame) {
                    mm_api::phys::free_frame(frame);
                }
                break;
            }
        }
        address = address.saturating_add(PAGER_PAGE_BYTES);
    }
    populated
}

/// Publishes one census of the ring0 anonymous fault path.
///
/// Every counter here is otherwise only reported on its own first occurrence
/// and then at a stride, which is exactly wrong for diagnosing a boot: a run
/// where nothing completes and nothing is refused looks identical to a run
/// where the path was never entered. One periodic line that carries all of
/// them together is what distinguishes those two, and it is cheap because the
/// caller is a scheduled task, not the fault path.
pub fn record_anonymous_fault_census() {
    let completed = COMPLETED_ANONYMOUS_FAULTS.load(Ordering::Relaxed);
    let contended = CONTENDED_ANONYMOUS_FAULTS.load(Ordering::Relaxed);
    let refused = REJECTED_ANONYMOUS_FAULT_MAPS.load(Ordering::Relaxed);
    let reserve_misses = RING0_FAULTS_WITHOUT_FRAME.load(Ordering::Relaxed);
    let run_pages = POPULATED_RUN_PAGES.load(Ordering::Relaxed);
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "pager-anon-census-served",
        completed,
        run_pages,
    );
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "pager-anon-census-stalled",
        contended,
        (refused << 32) | (reserve_misses & u64::from(u32::MAX)),
    );
    // Free physical memory alongside the fault counts, in one line, because
    // the two are only meaningful together. A boot that fails allocating a GPU
    // atlas and a boot that fails serving faults look identical from either
    // number on its own, and telling them apart by inference cost a full
    // debugging pass.
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "pager-anon-census-supply",
        mm_api::phys::free_bytes() / rustos_user_abi::pager::PAGER_PAGE_BYTES,
        mm_api::frame_capability::pager_fault_reserve_depth().unwrap_or(usize::MAX) as u64,
    );
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "pager-anon-census-access",
        READ_FIRST_TOUCHES.load(Ordering::Relaxed),
        COMPLETED_ANONYMOUS_FAULTS.load(Ordering::Relaxed),
    );
}

/// Completes a reply received through the fixed pagerd rendezvous. The worker
/// identity check happens before this function claims any mapping authority;
/// generic IPC reply handles are intentionally not accepted here.
pub(crate) fn adopt_rendezvous_reply(
    reservation: ps_api::PagerFaultReservation,
    reply: PagerFaultReplyWire,
    pager_task_id: u64,
) -> Result<(), ps_api::PagerFaultSlotError> {
    let dispatch = dispatch_wire(reservation);
    if !reply.is_canonical_zeroed_for(dispatch) {
        let claimed = ps_api::claim_pager_fault_reply_from_pager(reservation.token, pager_task_id)?;
        finish_without_mapping(claimed);
        return Err(ps_api::PagerFaultSlotError::Malformed);
    }
    let claimed = ps_api::claim_pager_fault_reply_from_pager(reservation.token, pager_task_id)?;
    complete_claimed_reply(claimed, reply);
    Ok(())
}

/// Rejects a dispatched rendezvous request when pagerd's prevalidated output
/// buffer became unusable. Releasing the exact owner-bound slot wakes the
/// faulting task without mapping a frame, rather than leaving it stranded.
pub(crate) fn reject_rendezvous_dispatch(
    reservation: ps_api::PagerFaultReservation,
    pager_task_id: u64,
) -> Result<(), ps_api::PagerFaultSlotError> {
    let claimed = ps_api::claim_pager_fault_reply_from_pager(reservation.token, pager_task_id)?;
    finish_without_mapping(claimed);
    Ok(())
}

fn complete_claimed_reply(claimed: ps_api::PagerFaultReservation, reply: PagerFaultReplyWire) {
    let Ok(frame) = mm_api::frame_capability::take_frame_grant(
        reply.frame_capability,
        frame_binding(claimed),
        u64::from(reply.frame_rights),
    ) else {
        let rejected = REJECTED_ANONYMOUS_FAULT_GRANTS.fetch_add(1, Ordering::Relaxed) + 1;
        if rejected == 1 || rejected.is_multiple_of(16) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-anon-grant-rejected",
                rejected,
                claimed.request.task_id,
            );
        }
        // The reservation still owns its preallocated grant when pagerd's
        // reply cannot claim it. Return that exact authority to the reserve;
        // otherwise a malformed reply would leak both a grant slot and one of
        // the bounded exception-time frames.
        cancel_grant(claimed);
        let _ = ps_api::consume_pager_fault_reply(claimed.token);
        let _ = wake_fault_owner(claimed.token, claimed.request.task_id);
        return;
    };
    let mapped = ps_api::with_validated_fault_address_space(claimed.request, |_, address_space| {
        address_space.map_prepared_pager_fault_frame_at(
            VirtAddr::new(claimed.request.virtual_address),
            frame.as_u64(),
            page_flags(reply.frame_rights),
        )
    })
    .is_ok_and(|result| result.is_ok());
    if !mapped {
        let rejected = REJECTED_ANONYMOUS_FAULT_MAPS.fetch_add(1, Ordering::Relaxed) + 1;
        if rejected == 1 || rejected.is_multiple_of(16) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-anon-map-rejected",
                rejected,
                claimed.request.task_id,
            );
        }
        mm_api::phys::free_frame(frame);
    }
    if mapped {
        // Fault-around. The faulting page is served; populate the short run
        // pagerd asked for while we are still in ordinary syscall context.
        populate_run(
            claimed.request,
            reply.frame_rights,
            reply.map_run_pages,
        );
    }
    let _ = ps_api::consume_pager_fault_reply(claimed.token);
    // A successful grant claim permanently transfers one wired reserve frame
    // into the target address space (or frees it after a rejected mapping).
    // Replace exactly that consumed frame before handing execution back to the
    // fault owner. This is ordinary pager syscall context, not exception
    // entry, and it closes the reserve lifecycle without running unrelated
    // housekeeping on either handoff path. Deferring this solely to the
    // housekeeping task lets a sustained fault-owner <-> pagerd handoff chain
    // consume all 64 frames and turn a valid non-present fault into SIGSEGV.
    let _ = mm_api::frame_capability::replenish_pager_fault_frames(1);
    let woke = wake_fault_owner(claimed.token, claimed.request.task_id);
    if mapped && woke {
        let completed = COMPLETED_ANONYMOUS_FAULTS.fetch_add(1, Ordering::Relaxed) + 1;
        if completed.is_multiple_of(16) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-anon-fault-progress",
                claimed.request.process_handle,
                claimed.request.virtual_address,
            );
        }
    }
    if mapped
        && woke
        && FIRST_ANONYMOUS_FAULT_COMPLETED
            // ORDERING: AcqRel publishes exactly one first-touch milestone;
            // failed Acquire observes the worker that already published it.
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "pager-anon-first-touch-complete",
            claimed.token,
            claimed.request.task_id,
        );
    }
}

/// Populates the pages after the faulting one, up to the run this fault asked
/// for. Shared by ring0's own anonymous resolution and by a pager reply.
///
/// Strictly best effort, and deliberately so. The faulting page is already
/// mapped; every page here only saves a *future* fault. So this allocates from
/// the ordinary allocator rather than the wired reserve, stops at the first
/// page it cannot serve, and never reports failure - a short allocator or a
/// racing mapping costs throughput, not correctness.
///
/// Authority comes from the same request: same region, same rights, and
/// `with_validated_fault_address_space` revalidates process, MM, VMA and object
/// generations for every page. A page whose leaf is already present is skipped
/// by `map_prepared_pager_fault_frame_at` returning `AlreadyMapped`, which ends
/// the run.
fn populate_run(request: PagerFaultRequestWire, rights: u32, run_pages: u64) {
    use rustos_user_abi::pager::{PAGER_FAULT_RUN_PAGES_MAX, PAGER_PAGE_BYTES};

    let requested = run_pages.min(u64::from(PAGER_FAULT_RUN_PAGES_MAX));
    if requested <= 1 {
        return;
    }
    let flags = page_flags(rights);
    let mut populated = 0_u64;
    for index in 1..requested {
        let Some(offset) = index.checked_mul(PAGER_PAGE_BYTES) else {
            break;
        };
        let Some(address) = request.virtual_address.checked_add(offset) else {
            break;
        };
        let Some(frame) = mm_api::frame_capability::allocate_zeroed_frame() else {
            break;
        };
        // Each page revalidates independently. A concurrent unmap or exec
        // between two pages of the same run must stop the run, not map into an
        // address space that no longer owns the range.
        let mapped = ps_api::with_validated_fault_address_space(request, |_, space| {
            space.map_prepared_pager_fault_frame_at(VirtAddr::new(address), frame.as_u64(), flags)
        })
        .is_ok_and(|result| result.is_ok());
        if !mapped {
            mm_api::phys::free_frame(frame);
            break;
        }
        populated += 1;
    }
    if populated != 0 {
        let total = POPULATED_RUN_PAGES.fetch_add(populated, Ordering::Relaxed) + populated;
        if total.is_multiple_of(1024) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-fault-around-pages",
                total,
                requested,
            );
        }
    }
}

/// Anonymous paging needs no housekeeping at all.
///
/// Ring0 answers an anonymous fault in the faulting task's own context and
/// holds the only map of the range, so there is no dispatch to drive, no reply
/// to adopt on a later turn, and no second replica to reconcile. Pager-backed
/// objects drain their own fixed rendezvous from pagerd directly. The hook
/// stays because it is the registered normal-time entry point the page cache
/// will need; it reports the work it actually did, which is currently none.
pub fn service_deferred_work() -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::dispatch_wire;
    use crate::multitask::{PagerFaultReservation, PagerFaultState};
    use rustos_user_abi::pager::{
        PAGER_FAULT_ABI_VERSION, PagerEndpointCapabilityWire, PagerFaultRequestWire,
        PagerObjectIdentityWire, VM_ACCESS_READ, VM_OBJECT_ANONYMOUS, VM_PROT_READ,
    };

    fn reservation() -> PagerFaultReservation {
        PagerFaultReservation {
            token: 19,
            state: PagerFaultState::DispatchedToPager,
            request: PagerFaultRequestWire {
                version: PAGER_FAULT_ABI_VERSION,
                access: VM_ACCESS_READ,
                fault_token: 19,
                process_handle: 3,
                process_generation: 5,
                task_id: 7,
                task_generation: 8,
                mm_generation: 11,
                vma_generation: 13,
                virtual_address: 0x4000,
                deadline_ns: 17,
                scheduling_domain: 23,
                charge_token: 29,
                object: PagerObjectIdentityWire {
                    object_type: VM_OBJECT_ANONYMOUS,
                    rights: VM_PROT_READ,
                    slot: 31,
                    generation: 37,
                    pager_epoch: 41,
                    backing_generation: 43,
                    ..PagerObjectIdentityWire::default()
                },
                ..PagerFaultRequestWire::default()
            },
            endpoint: PagerEndpointCapabilityWire {
                slot: 43,
                generation: 47,
                rights: 1,
            },
            zeroed_frame_capability: 53,
            granted_frame_rights: VM_PROT_READ,
            dispatch_owner_task_id: 71,
        }
    }

    #[test]
    fn pager_rendezvous_wire_binds_exact_fault_subject_and_ticket() {
        let reservation = reservation();
        let wire = dispatch_wire(reservation);
        assert!(wire.is_canonical());
        assert_eq!(wire.request.fault_token, reservation.token);
        assert_eq!(wire.request.task_id, reservation.request.task_id);
        assert_eq!(
            wire.zeroed_frame_capability,
            reservation.zeroed_frame_capability
        );
        assert_eq!(wire.granted_frame_rights, reservation.granted_frame_rights);
    }
}
