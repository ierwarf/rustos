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

use super::super::cpu_local::{scheduler_mut, scheduler_ref};
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

/// The wait payload is owner-generation-bound per slot, and the arm plus its
/// exact reason are one store, so this CPU's own execution ownership is the
/// whole precondition. Only a task the owner word does not place `Running`
/// here needs the catalog guard's lifetime tables.
fn arm_current_wait(reason: scheduler::BlockReason) -> bool {
    interrupts::without_interrupts(|| {
        if let Some(armed) = scheduler::arm_current_wait(reason) {
            return armed;
        }
        // SAFETY: interrupts are masked and no scheduler-owned reference escapes.
        unsafe { scheduler_mut().arm_block_current_task_with_reason(reason) }
    })
}

/// Cancels a previously armed block without marking the current task blocked.
pub fn cancel_block_current_task() -> bool {
    interrupts::without_interrupts(|| {
        if let Some(cancelled) = scheduler::cancel_current_wait() {
            return cancelled;
        }
        // SAFETY: interrupts are masked and no scheduler-owned reference escapes.
        unsafe { scheduler_mut().cancel_block_current_task() }
    })
}

pub fn wake_task(task_id: u64) -> bool {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return deferred_wake::defer_current_cpu(task_id);
    }
    // SAFETY: interrupts are masked and the scheduler access guard retains the
    // task catalog for the duration of the wake; no reference escapes.
    interrupts::without_interrupts(|| unsafe { scheduler_mut().wake_task(task_id) })
}

/// Permanently removes the current user task's base System-class admission and
/// caps its permanent fair weight without ever increasing a lower weight. A
/// reply-scoped IPC priority donation, if any, remains owned by that reply
/// capability and therefore remains effective until the normal release path.
pub fn demote_current_user_task_to_user_class() -> bool {
    // SAFETY: interrupts are masked and the scheduler access guard exclusively
    // owns the current task's class mutation; no reference escapes.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().demote_current_user_task_to_user_class()
    })
}

/// Reports whether `task_id` currently carries the kernel's effective System
/// class, including a live reply-scoped donation. This value is sampled before
/// endpoint enqueue; the reply capability remains the authoritative donation
/// lifetime after publication.
pub fn task_has_system_scheduling_class(task_id: u64) -> bool {
    // SAFETY: interrupts are masked and the scheduler access guard keeps the
    // queried task binding live for this snapshot; no reference escapes.
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().task_has_system_scheduling_class(task_id)
    })
}

/// Associates a live synchronous IPC reply with a caller-to-server priority
/// donation. The reply/cancellation paths revoke it before waking the caller.
pub fn inherit_ipc_priority(reply: u64, donor_task_id: u64, receiver_task_id: u64) -> bool {
    interrupts::without_interrupts(|| {
        // The receive commit that binds a reserved edge is the common case and
        // its receiver is this CPU's current task.
        if let Some(bound) =
            scheduler::bind_current_receiver_call_donation(reply, donor_task_id, receiver_task_id)
        {
            return bound;
        }
        // SAFETY: interrupts are masked and no scheduler-owned reference escapes.
        unsafe { scheduler_mut().inherit_ipc_priority(reply, donor_task_id, receiver_task_id) }
    })
}

/// One scheduler acquisition for the class query and the reservation an IPC
/// call needs before it can enqueue. See [`scheduler::IpcCallAdmission`].
pub fn reserve_ipc_call_donation(donor_task_id: u64) -> scheduler::IpcCallAdmission {
    interrupts::without_interrupts(|| {
        // Every input is published per slot for the task this CPU is running,
        // which is the donor in every syscall admission.
        if let Some(admission) = scheduler::reserve_current_call_donation(donor_task_id) {
            return admission;
        }
        // SAFETY: interrupt exclusion and the scheduler access guard serialize
        // the exact class query and donor reservation; no borrow escapes.
        unsafe { scheduler_mut().reserve_ipc_call_donation(donor_task_id) }
    })
}

pub fn reserve_ipc_priority(donor_task_id: u64) -> bool {
    // SAFETY: interrupt exclusion and the scheduler access guard serialize the
    // exact donor reservation; no scheduler-owned reference escapes.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().reserve_ipc_priority(donor_task_id)
    })
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
    // SAFETY: interrupt exclusion and the scheduler access guard make binding
    // the reserved donor to one reply/receiver an atomic scheduler mutation.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().bind_reserved_ipc_priority(
            reply,
            scheduler::ipc_donation::DonationNamespace::IpcReply,
            donor_task_id,
            receiver_task_id,
        )
    })
}

/// Selects and binds one exact worker for a process-owned endpoint in the same
/// scheduler transaction that reserves its reply-scoped donation.
pub fn bind_ipc_priority_to_process_worker(
    reply: u64,
    donor_task_id: u64,
    receiver_process_id: u64,
) -> Option<u64> {
    // SAFETY: interrupts are masked and the scheduler access guard makes the
    // worker selection and reply donation one catalog transaction.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().bind_ipc_priority_to_process_worker(
            reply,
            donor_task_id,
            receiver_process_id,
        )
    })
}

/// Revokes the bounded priority donation owned by a completed or cancelled IPC
/// reply capability. It is safe to call more than once for terminal races.
/// Binds a pager-fault donation. The fault token lives in its own key space:
/// it is `(generation << 8) | slot` while a reply handle is
/// `(generation << 16) | (index + 1)`, and the two overlap numerically, so the
/// ledger must be told which space this key belongs to.
pub fn bind_reserved_pager_fault_priority(
    fault_token: u64,
    donor_task_id: u64,
    receiver_task_id: u64,
) -> bool {
    // SAFETY: interrupt exclusion and the scheduler access guard make binding
    // the reserved donor to one fault token and exact worker an atomic
    // scheduler mutation; no scheduler-owned reference escapes.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().bind_reserved_ipc_priority(
            fault_token,
            scheduler::ipc_donation::DonationNamespace::PagerFault,
            donor_task_id,
            receiver_task_id,
        )
    })
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

/// Returns the exact caller scheduling-context custody carried by one terminal
/// reply. The IPC runtime guarantees one-shot extraction; PS revalidates the
/// live slot/generation before releasing any reply-scoped donation state.
pub fn settle_ipc_reply_scheduling_context(
    reply: u64,
    custody: kernel_ipc_runtime::api::ReplySchedulingContextCustody,
) -> bool {
    let identity = custody.identity();
    let owner_task_id = custody.context_owner_task_id();
    let valid = interrupts::without_interrupts(|| {
        // Custody is decided by the live slot/task binding the identity
        // encodes, which the per-slot publication already carries. Only an
        // unresolved or mismatched binding needs the catalog guard.
        if let Some(published) =
            scheduler::published_scheduling_context_matches(owner_task_id, identity)
        {
            return published;
        }
        // SAFETY: interrupts are masked and no scheduler-owned reference escapes.
        unsafe { scheduler_ref().scheduling_context_matches(owner_task_id, identity) }
    });
    let _ = interrupts::without_interrupts(|| {
        scheduler::release_reply_donation(
            reply,
            scheduler::ipc_donation::DonationNamespace::IpcReply,
        )
    });
    valid
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
        settle_ipc_reply_scheduling_context(reply, custody),
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
    // SAFETY: interrupts are masked and the scheduler access guard owns the
    // custody settlement plus exact reply-wake publication transaction.
    let outcome = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().settle_and_complete_fast_ipc_reply_handoff(
            reply,
            completion.caller_task_id,
            custody.context_owner_task_id(),
            custody.identity(),
        )
    })
    .expect("fast reply returned stale scheduling-context custody");
    match outcome {
        scheduler::FastIpcReplyHandoffOutcome::Direct
        | scheduler::FastIpcReplyHandoffOutcome::LocalFallback => true,
        scheduler::FastIpcReplyHandoffOutcome::Rejected => {
            complete_ipc_reply_wake_handoff(reply, completion.caller_task_id)
        }
    }
}

/// Completes the scheduling side of a terminal reply with one Scheduler
/// acquisition, then publishes only its opaque exact wake token to the
/// target CPU's handoff owner.  A stale token deliberately loses urgency; it
/// never falls back to the catalog hint path and cannot create execution
/// authority.
pub fn complete_ipc_reply_wake_handoff(reply: u64, task_id: u64) -> bool {
    let _ = interrupts::without_interrupts(|| {
        scheduler::release_reply_donation(
            reply,
            scheduler::ipc_donation::DonationNamespace::IpcReply,
        )
    });
    // SAFETY: interrupts are masked and the scheduler access guard validates
    // the exact terminal reply and task before producing an opaque wake token.
    let token = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().complete_ipc_reply_wake_handoff(reply, task_id)
    });
    interrupts::without_interrupts(|| token.is_some_and(scheduler::enqueue_reply_wake_handoff))
}

pub fn release_ipc_priorities_for_process(process_id: u64) {
    // SAFETY: interrupts are masked and the scheduler access guard exclusively
    // owns the process-wide donation retirement; no reference escapes.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().release_ipc_priorities_for_process(process_id)
    });
}
