//! Typed wait arms and synchronous-IPC custody adapters for the current task.
//!
//! - **Owner:** PS owns current-task wait, donation, and reply-wake scheduler
//!   mutations; IPC runtime owns the endpoint/reply capabilities passed here.
//! - **Boundary:** endpoint, reply, task, process, and scheduling-context
//!   identities cross from IPC runtime or the syscall layer and are untrusted
//!   until revalidated against the current slot and exact live generation.
//! - **State machine:** a running task arms one exact wait, commits to blocked
//!   custody or loses to a wake, then a terminal reply settles donation and
//!   scheduling-context custody before publishing one wake handoff.
//! - **Invariants:** reply donation is bounded and one-shot; typed waits retain
//!   their exact identity; no fallback may mint execution or context authority.
//! - **Concurrency:** interrupts are masked around every current-CPU snapshot
//!   and catalog fallback; owner-word CAS and the donation ledger provide the
//!   publication points that do not need the global scheduler catalog.
//! - **Failure/recovery:** stale identities fail closed, duplicate terminal
//!   settlement is idempotent where specified, and an unanswerable publication
//!   falls back to the locked lifecycle owner without an unbounded wait.
//! - **Forbidden:** no identity-only wake, detached donation, polling loop, or
//!   policy transfer from the named user service into this adapter may return.
//! - **Evidence:** `scheduler-dispatch`, `ipc-priority-donation`, and
//!   `ipc-scheduling-context-handoff`; focused witnesses live in
//!   `scheduler::task_wait` and `scheduler::synchronous_handoff_tests`.

use x86_64::instructions::interrupts;

use super::super::{deferred_wake, scheduler};

/// Arms a race-free block on the current task; must be paired with
/// `commit_block_current_task`. Returns false if the slot is invalid or this is
/// the root task.
pub fn arm_block_current_task() -> bool {
    arm_current_wait(scheduler::BlockReason::Generic)
}

/// Arms the current task for one exact endpoint receive epoch. Only this typed
/// wait may be consumed by the same-CPU direct IPC handoff path.
pub fn arm_block_current_task_on_endpoint(endpoint: u64) -> bool {
    endpoint != 0 && arm_current_wait(scheduler::BlockReason::EndpointReceive(endpoint))
}

/// Arms the caller for one exact synchronous reply epoch.  The typed reason is
/// consumed by the fast rendezvous commit; ordinary wake/block code preserves
/// its existing race semantics.
pub fn arm_block_current_task_on_reply(reply: u64) -> bool {
    reply != 0 && arm_current_wait(scheduler::BlockReason::EndpointReply(reply))
}

/// Arms the caller on its exact fixed pager-fault token. Endpoint IPC is
/// intentionally absent here: exception ingress must not acquire endpoint or
/// reply registries before this wait is committed.
pub fn arm_block_current_task_on_pager_fault(token: u64) -> bool {
    token != 0 && arm_current_wait(scheduler::BlockReason::PagerFault(token))
}

/// Arms pagerd on the fixed fault-rendezvous mailbox. Unlike endpoint receive,
/// this is backed by a bounded atomic waiter table that exception ingress can
/// consume without touching generic IPC state.
pub fn arm_block_current_task_on_pager_service() -> bool {
    arm_current_wait(scheduler::BlockReason::PagerService)
}

/// The original masked CPU scope retains its physical Current bank; the exact
/// backing loan is consumed before IRQ restoration. No catalog fallback can
/// manufacture Current custody from a scalar slot or Running word.
fn arm_current_wait(reason: scheduler::BlockReason) -> bool {
    interrupts::without_interrupts(|| {
        // SAFETY: original same-CPU IRQ/Current scope contains the full loan.
        unsafe { scheduler::arm_current_wait(reason) }.unwrap_or(false)
    })
}

/// Cancels a previously armed block without marking the current task blocked.
pub fn cancel_block_current_task() -> bool {
    interrupts::without_interrupts(|| {
        // SAFETY: original same-CPU IRQ/Current scope contains the full loan.
        unsafe { scheduler::cancel_current_wait() }.unwrap_or(false)
    })
}

/// Whether a receiver whose call wake was refused still waits on `endpoint`.
/// Observation only; see `wake_publication::receive_phase_on_endpoint`.
pub fn endpoint_receiver_still_waiting(task_id: u64, endpoint: u64) -> bool {
    scheduler::wake_publication::receive_phase_on_endpoint(task_id, endpoint)
}

pub fn wake_task(task_id: u64) -> bool {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return deferred_wake::defer_current_cpu(task_id);
    }
    // Original masked CPU owner, exact binding/frame and queue arbitration;
    // no Scheduler directory is acquired for this wake producer.
    scheduler::wake_publication::wake_task(task_id)
}

/// Irreversible base-class/fair-weight self-demotion under the original masked
/// CPU and actual physical Current loan. Reply-scoped donations are untouched.
pub fn demote_current_user_task_to_user_class() -> bool {
    scheduler::current_class::demote()
}

/// Reports whether `task_id` currently carries the kernel's effective System
/// class, including a live reply-scoped donation. This value is sampled before
/// endpoint enqueue; the reply capability remains the authoritative donation
/// lifetime after publication.
pub fn task_has_system_scheduling_class(task_id: u64) -> bool {
    scheduler::donation_publication::system_class(task_id)
}

/// Associates a live synchronous IPC reply with a caller-to-server priority
/// donation. The reply/cancellation paths revoke it before waking the caller.
pub fn inherit_ipc_priority(reply: u64, donor_task_id: u64, receiver_task_id: u64) -> bool {
    scheduler::donation_publication::bind(reply, donor_task_id, receiver_task_id, true)
}

/// Wakes an exact pager-service rendezvous from actual bound process resources.
/// The original typed wait phase, not a hint or scan, owns publication.
pub fn wake_pager_service_waiter_for_process(process_id: u64) -> Option<u64> {
    scheduler::wake_publication::pager_service(process_id)
}

/// Binds one dispatched fault token to the fault owner's effective scheduling
/// context. The fixed fault slot the dispatch came from is the prior admission
/// proof, so this needs no separate donor reservation.
pub fn inherit_pager_fault_priority(
    fault_token: u64,
    donor_task_id: u64,
    receiver_task_id: u64,
) -> bool {
    scheduler::donation_publication::pager(fault_token, donor_task_id, receiver_task_id)
}

/// Original donor/root loans and revalidated reply funding, including non-Current
/// donors. No scalar Current shortcut or Scheduler fallback grants custody.
pub fn reserve_ipc_call_donation(donor_task_id: u64) -> scheduler::IpcCallAdmission {
    scheduler::donation_publication::reserve_call(donor_task_id)
}

pub fn reserve_ipc_priority(donor_task_id: u64) -> bool {
    scheduler::donation_publication::reserve_priority(donor_task_id)
}

pub fn cancel_ipc_priority_reservation(donor_task_id: u64) -> bool {
    // The reservation lives in the donation ledger behind its own bounded
    // lock, so the cancellation needs interrupt exclusion but not the catalog.
    interrupts::without_interrupts(|| scheduler::cancel_reply_donation_reservation(donor_task_id))
}

pub fn attach_reserved_ipc_priority(reply: u64, donor_task_id: u64) -> bool {
    // The donation ledger owns the edge behind its own bounded lock, so this
    // transfer needs interrupt exclusion but not the task catalog.
    interrupts::without_interrupts(|| scheduler::attach_reply_donation(reply, donor_task_id))
}

pub fn bind_reserved_ipc_priority(reply: u64, donor_task_id: u64, receiver_task_id: u64) -> bool {
    scheduler::donation_publication::bind(reply, donor_task_id, receiver_task_id, false)
}

/// Releases a pager-fault donation from the pager-fault key space.
pub fn release_pager_fault_priority(fault_token: u64) -> bool {
    interrupts::without_interrupts(|| {
        scheduler::release_reply_donation(
            fault_token,
            scheduler::ipc_donation::DonationNamespace::PagerFault,
        )
    })
}

pub fn release_ipc_priority(reply: u64) -> bool {
    interrupts::without_interrupts(|| {
        scheduler::release_reply_donation(
            reply,
            scheduler::ipc_donation::DonationNamespace::IpcReply,
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplySchedulingContextSettlement {
    Returned,
    OwnerRetired,
    IdentityMismatch,
}

const fn classify_unpublished_reply_custody(
    identity_matches: bool,
    owner_is_live: bool,
) -> ReplySchedulingContextSettlement {
    if identity_matches {
        ReplySchedulingContextSettlement::Returned
    } else if owner_is_live {
        ReplySchedulingContextSettlement::IdentityMismatch
    } else {
        ReplySchedulingContextSettlement::OwnerRetired
    }
}

fn settle_reply_scheduling_context(
    reply: u64,
    custody: kernel_ipc_runtime::api::ReplySchedulingContextCustody,
) -> ReplySchedulingContextSettlement {
    let identity = custody.identity();
    let owner_task_id = custody.context_owner_task_id();
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
    scope.mask_irqs();
    // ONE original masked CPU scope encloses both the exact resource/identity
    // comparison and ledger release. Hints are candidates, never authority.
    let (matches, live) = scheduler::reply_context_custody(owner_task_id, identity);
    let settlement = classify_unpublished_reply_custody(matches, live);
    let _ = scheduler::release_reply_donation(
        reply,
        scheduler::ipc_donation::DonationNamespace::IpcReply,
    );
    settlement
}

/// Returns the exact caller scheduling-context custody carried by one normal
/// reply. A mismatched live object is corruption. A retained exact object can
/// return custody after stop, as before; this is NOT a live execution/funding
/// grant, and the subsequent wake still requires its own lifetime admission.
pub fn settle_ipc_reply_scheduling_context(
    reply: u64,
    custody: kernel_ipc_runtime::api::ReplySchedulingContextCustody,
) -> bool {
    settle_reply_scheduling_context(reply, custody) == ReplySchedulingContextSettlement::Returned
}

/// Settles custody after the reply object has atomically entered Cancelled.
/// The context owner may have retired before a downstream server observes its
/// timeout; that is a legal terminal race. A still-live owner with a different
/// scheduling-context identity remains a hard invariant violation.
pub fn settle_cancelled_ipc_reply_scheduling_context(
    reply: u64,
    custody: kernel_ipc_runtime::api::ReplySchedulingContextCustody,
) -> bool {
    settle_terminal_ipc_reply_scheduling_context(reply, custody)
}

// Both cancellation and completion own a terminal transport edge. A downstream
// reply can outlive its original budget owner just as a timeout can; neither
// may borrow a replacement context, or excuse a live owner's wrong generation.
fn settle_terminal_ipc_reply_scheduling_context(
    reply: u64,
    custody: kernel_ipc_runtime::api::ReplySchedulingContextCustody,
) -> bool {
    settle_reply_scheduling_context(reply, custody)
        != ReplySchedulingContextSettlement::IdentityMismatch
}

pub fn complete_ipc_reply_wake_handoff_with_custody(
    reply: u64,
    completion: kernel_ipc_runtime::api::ReplyCompletion,
) -> bool {
    let custody = completion
        .scheduling_context
        .expect("synchronous IPC reply completed without scheduling-context custody");
    assert_eq!(
        custody.caller_task_id(),
        completion.caller_task_id,
        "reply returned scheduling-context custody to a different caller"
    );
    assert!(
        settle_terminal_ipc_reply_scheduling_context(reply, custody),
        "reply returned stale scheduling-context custody"
    );
    complete_ipc_reply_wake_handoff(reply, completion.caller_task_id)
}

pub fn complete_fast_ipc_reply_wake_handoff_with_custody(
    reply: u64,
    completion: kernel_ipc_runtime::api::ReplyCompletion,
) -> bool {
    let custody = completion
        .scheduling_context
        .expect("fast synchronous IPC reply completed without scheduling-context custody");
    assert_eq!(
        custody.caller_task_id(),
        completion.caller_task_id,
        "fast reply returned scheduling-context custody to a different caller"
    );
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
    scope.mask_irqs();
    let outcome = scheduler::ipc_publication::reply(
        &scope,
        reply,
        completion.caller_task_id,
        custody.context_owner_task_id(),
        custody.identity(),
    ).expect("fast reply returned stale scheduling-context custody");
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    // No generic fresh-lookup fallback after ORIGINAL typed-wait refusal.
    let woken = outcome != scheduler::FastIpcReplyHandoffOutcome::Rejected;
    if !woken {
        // The common producer already recorded the retry in this scope.
        // Diagnostics must not acquire a second dispatch-preemption owner.
        note_refused_reply_wake("ipc-fast-reply-wake-refused", reply, completion.caller_task_id);
    }
    woken
}

/// Publish a completed fast reply. The caller's wake is held back to the replier's
/// next boundary when the caller is parked on this CPU: the replier's own receive
/// then carries the switch (`take_deferred_fast_ipc_reply`), exactly as the fused
/// `REPLY_RECV` does inside one syscall. Anywhere else, or when a boundary already
/// holds one, the caller is woken now and the return value says to reschedule.
pub fn publish_fast_ipc_reply(
    reply: u64,
    completion: kernel_ipc_runtime::api::ReplyCompletion,
) -> bool {
    if let Some(replier) = super::current_task_id()
        && scheduler::pending_reply::stash(replier, reply, completion)
    {
        return false;
    }
    complete_fast_ipc_reply_wake_handoff_with_custody(reply, completion)
}

/// [`publish_fast_ipc_reply`] for a reply object that was queued rather than
/// fast-delivered. Its scheduling-context custody is settled now, in the order the
/// immediate wake settles it; only the wake itself is deferred.
pub fn publish_ipc_reply(
    reply: u64,
    completion: kernel_ipc_runtime::api::ReplyCompletion,
) -> bool {
    let custody = completion
        .scheduling_context
        .expect("synchronous IPC reply completed without scheduling-context custody");
    assert_eq!(
        custody.caller_task_id(),
        completion.caller_task_id,
        "reply returned scheduling-context custody to a different caller"
    );
    assert!(
        settle_terminal_ipc_reply_scheduling_context(reply, custody),
        "reply returned stale scheduling-context custody"
    );
    if let Some(replier) = super::current_task_id()
        && scheduler::pending_reply::stash(replier, reply, completion)
    {
        return false;
    }
    complete_ipc_reply_wake_handoff(reply, completion.caller_task_id)
}

/// The reply this task deferred, for its blocking receive to carry.
pub fn take_deferred_fast_ipc_reply() -> Option<(u64, kernel_ipc_runtime::api::ReplyCompletion)> {
    if !scheduler::pending_reply::present() {
        return None;
    }
    scheduler::pending_reply::take_for(super::current_task_id()?)
}

/// Whether this CPU holds a deferred reply (lock-free; the syscall entry's gate).
pub fn fast_ipc_reply_deferred() -> bool {
    scheduler::pending_reply::present()
}

/// Wake the deferred caller now, as the reply syscall would have, and request the
/// hand-back reschedule. Idempotent.
pub fn settle_deferred_fast_ipc_reply() {
    if let Some((reply, completion)) = scheduler::pending_reply::take_any()
        && complete_fast_ipc_reply_wake_handoff_with_custody(reply, completion)
    {
        crate::multitask::request_deferred_reschedule();
    }
}

/// [`settle_deferred_fast_ipc_reply`] at a scheduler entry: the entry's own masked
/// scope publishes the wake and its dispatcher picks the caller next.
pub(in crate::multitask) fn settle_deferred_fast_ipc_reply_in(
    scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
) {
    let Some((reply, completion)) = scheduler::pending_reply::take_any() else { return };
    let Some(custody) = completion.scheduling_context else { return };
    let outcome = scheduler::ipc_publication::reply(
        scope,
        reply,
        completion.caller_task_id,
        custody.context_owner_task_id(),
        custody.identity(),
    )
    .expect("deferred fast reply returned stale scheduling-context custody");
    if outcome == scheduler::FastIpcReplyHandoffOutcome::Rejected {
        // `scope` is the scheduler entry's original owner, still live here.
        note_refused_reply_wake("ipc-fast-reply-wake-refused", reply, completion.caller_task_id);
    }
}

/// Withdraws exact reply donation and retains the original wake lifetime loan
/// through exact per-CPU FIFO publication. No legacy guard or lookup after wake.
pub fn complete_ipc_reply_wake_handoff(reply: u64, task_id: u64) -> bool {
    let woken = scheduler::wake_publication::reply(reply, task_id);
    if !woken {
        record_refused_reply_wake("ipc-reply-wake-refused", reply, task_id, REPLY_WAIT);
    }
    woken
}

/// Refused terminal wakes retain a bounded exact-wait retry and diagnostics.
/// Fast producers record within their original scope; unscoped terminal paths
/// use the recording wrapper below. Diagnostics are exponentially rate limited.
const REPLY_WAIT: fn(u64, u64) -> bool = scheduler::wake_retry::record_refused_reply;
const PAGER_FAULT_WAIT: fn(u64, u64) -> bool = scheduler::wake_retry::record_refused_pager_fault;

fn record_refused_reply_wake(
    name: &'static str,
    reply: u64,
    task_id: u64,
    retry: fn(u64, u64) -> bool,
) {
    // Retry this exact reply/fault wait at this CPU's next scheduler entry.
    // A pager-fault wait has no deadline waiter, so without this a refused
    // fault completion parked the faulting task indefinitely.
    let _ = retry(task_id, reply);
    note_refused_reply_wake(name, reply, task_id);
}

/// Log/expedite only: the producer may still hold its original dispatch scope.
/// Retry recording belongs to that producer, never a nested scope here.
fn note_refused_reply_wake(name: &'static str, reply: u64, task_id: u64) {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
    // A refused exact wake can leave the caller blocked with its response
    // already queued (the refusal paths give up before clearing the wait when
    // the caller's frame is between publications). Make its armed reply
    // deadline due now, so the next clockevent wakes it to re-poll instead of
    // leaving it parked for the full 100 ms-30 s deadline. Harmless when the
    // caller was not blocked yet: it re-polls after arming anyway.
    #[cfg(not(test))]
    let _ = crate::arch::rtc::expedite_sleep_waiter(task_id);
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    let total = REFUSED.fetch_add(1, Relaxed) + 1;
    if total > 16 && !total.is_power_of_two() {
        return;
    }
    crate::debug::record_milestone(crate::debug::LogCategory::Sched, name, task_id, reply);
}

/// Returns only the exact original pager wait epoch; a pending publication
/// excludes later arms through actual queue effects and binding/FIFO lifetime.
pub fn complete_pager_fault_wake_handoff(fault_token: u64, task_id: u64) -> bool {
    let woken = scheduler::wake_publication::pager_reply(fault_token, task_id);
    if !woken {
        record_refused_reply_wake("pager-fault-wake-refused", fault_token, task_id, PAGER_FAULT_WAIT);
    }
    woken
}

#[cfg(test)]
mod tests {
    use super::{ReplySchedulingContextSettlement, classify_unpublished_reply_custody};

    #[test]
    fn actual_slow_reply_to_reclaimed_original_context_is_terminal_not_kernel_panic() {
        use crate::multitask::scheduler::{self, root_creation};
        use crate::user::abi::UserAbi;
        use kernel_ipc_runtime::api::{ReplyCompletion, ReplySchedulingContextCustody};
        let id = 132301;
        let custody = core::cell::Cell::new(None);
        let foreign_refused = core::cell::Cell::new(false);
        root_creation::tests::exec_root(id, true,
            root_creation::tests::bootstrap(UserAbi::Linux), |_, _| {
                let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
                scope.mask_irqs();
                let admission = scheduler::donation_publication::reserve_call_with(&scope, id);
                assert!(admission.donation_reserved);
                let identity = admission.scheduling_context.unwrap();
                assert!(scheduler::reply_context_custody(id, identity).0);
                custody.set(ReplySchedulingContextCustody::new_with_owner(identity, id, id));
                assert!(scheduler::cancel_reply_donation_reservation(id));
                let foreign_identity = kernel_object::api::ObjectIdentity::new(identity.owner(),
                    identity.kind(), identity.slot(), identity.generation() + 1).unwrap();
                let foreign = ReplySchedulingContextCustody::new_with_owner(foreign_identity, id, id).unwrap();
                assert_eq!(scheduler::reply_context_custody(id, foreign_identity), (false, true));
                // Completion must enter its OWN original scope. A nested-scope
                // refusal (or a later wake panic) is not identity validation.
                drop(scope);
                let refusal = std::panic::catch_unwind(|| {
                    super::complete_ipc_reply_wake_handoff_with_custody(0x9797,
                        ReplyCompletion { caller_task_id: id, scheduling_context: Some(foreign) })
                });
                foreign_refused.set(refusal.err().is_some_and(|payload| {
                    payload.downcast_ref::<&str>().copied()
                        .or_else(|| payload.downcast_ref::<std::string::String>().map(|text| text.as_str()))
                        == Some("reply returned stale scheduling-context custody")
                }));
            });
        assert!(foreign_refused.get(), "a live owner's different context must still fail closed");
        let _namespace = scheduler::execution_test_serial_guard();
        let custody = custody.get().unwrap();
        // The SAME born context and its owner have actually crossed physical
        // retirement/reclaim. Only earlier IRQ/topology/external ACKs are modeled.
        assert_eq!(scheduler::reply_context_custody(id, custody.identity()), (false, false));
        let outcome = std::panic::catch_unwind(|| {
            super::complete_ipc_reply_wake_handoff_with_custody(0x9797,
                ReplyCompletion { caller_task_id: id, scheduling_context: Some(custody) })
        });
        assert_eq!(outcome.ok(), Some(false), "a late reply cannot panic or revive its retired caller");
    }

    #[test]
    fn refused_fast_reply_uses_original_scope_without_nested_retry_owner() {
        use crate::multitask::scheduler::{self, root_creation};
        use crate::user::abi::UserAbi;
        use kernel_ipc_runtime::api::{ReplyCompletion, ReplySchedulingContextCustody};
        let id = 132302;
        root_creation::tests::exec_root(id, true,
            root_creation::tests::bootstrap(UserAbi::Linux), |_, _| {
                let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
                scope.mask_irqs();
                let admission = scheduler::donation_publication::reserve_call_with(&scope, id);
                assert!(admission.donation_reserved);
                let custody = ReplySchedulingContextCustody::new_with_owner(
                    admission.scheduling_context.unwrap(), id, id).unwrap();
                assert!(scheduler::cancel_reply_donation_reservation(id));
                drop(scope);
                // The caller is not waiting on this reply. Actual publication
                // refuses under its own scope; logging must not enter a second
                // scope merely to record the same refusal again.
                let outcome = std::panic::catch_unwind(|| {
                    super::complete_fast_ipc_reply_wake_handoff_with_custody(0x9898,
                        ReplyCompletion { caller_task_id: id, scheduling_context: Some(custody) })
                });
                assert_eq!(outcome.ok(), Some(false));
            });
        // The scheduler-entry variant borrows a scope and cannot drop/reopen
        // it. Pin its logging-only refusal route as well as the real call above.
        let source = include_str!("ipc.rs").split("#[cfg(test)]").next().unwrap();
        let deferred = source.split("fn settle_deferred_fast_ipc_reply_in(").nth(1).unwrap()
            .split("pub fn complete_ipc_reply_wake_handoff(").next().unwrap();
        assert!(deferred.contains("note_refused_reply_wake("));
        assert!(!deferred.contains("record_refused_reply_wake("));
    }

    #[test]
    fn cancelled_custody_accepts_only_exact_return_or_retired_owner() {
        assert_eq!(
            classify_unpublished_reply_custody(true, true),
            ReplySchedulingContextSettlement::Returned
        );
        assert_eq!(
            classify_unpublished_reply_custody(false, false),
            ReplySchedulingContextSettlement::OwnerRetired
        );
        assert_eq!(
            classify_unpublished_reply_custody(false, true),
            ReplySchedulingContextSettlement::IdentityMismatch
        );
    }
}
