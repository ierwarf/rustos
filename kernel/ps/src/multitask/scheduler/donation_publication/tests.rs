//! Owner: actual born participant and reply-ledger admission witnesses.
//! Boundary: original masked CPU plus Terminal/Identity/Resource loans, not catalog arrays.
//! Lifecycle: complete physical births -> scoped admission -> revocation -> modeled cleanup ACK.
//! Concurrency: canonical native namespace; exact source cancellation occurs between capture/commit.
//! Failure: foreign/missing/sealed/IRQ-enabled loans and cancelled source deny new ledger effects.
//! Forbidden: scalar Current as donor authority or root cancellation as fresh nested funding.
//! Evidence: real shared resource/ref/ledger engines; topology and external ACK explicitly modeled.
use super::super::ipc_donation::DonationNamespace::{IpcReply, PagerFault};
use super::super::{root_creation, terminal_quarantine, thread_creation, TaskRetireReason};
use super::*;
use crate::user::abi::UserAbi;
pub(in crate::multitask::scheduler) mod legacy_witnesses;
pub(in crate::multitask::scheduler) mod ledger_witnesses;
const ID: u64 = 85400;
fn boot() -> super::super::UserTaskBootstrap {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
    boot
}
fn born(f: impl FnOnce([usize; 4])) {
    root_creation::tests::exec_root(ID, true, boot(), |leader, handle| {
        let b = thread_creation::tests::active_sibling(leader, ID + 1, boot());
        let c = thread_creation::tests::active_sibling(leader, ID + 2, boot());
        let d = thread_creation::tests::active_sibling(leader, ID + 3, boot());
        f([leader, b, c, d]);
        for id in ID..ID + 4 {
            donation_ledger::release_task(id);
        }
        thread_creation::tests::release_born(d, ID + 3, handle);
        thread_creation::tests::release_born(c, ID + 2, handle);
        thread_creation::tests::release_born(b, ID + 1, handle);
    });
}
fn masked() -> DispatchPreemption<'static> {
    let mut scope = DispatchPreemption::enter_model(0);
    scope.mask_irqs();
    scope
}
#[test]
fn actual_normal_reply_withdraws_only_original_reply_namespace_before_wake_or_refusal() {
    born(|slots| {
        let scope = masked();
        runqueue::weight::replace(slots[1], SYSTEM_CLASS_WEIGHT_FLAG | super::super::NICE_0_LOAD);
        assert!(bind_with(&scope, 593, ID + 1, ID, true));
        // A different original funding root: withdrawing the reply must neither
        // remove nor revoke this independently funded equal-number pager edge.
        runqueue::weight::replace(slots[3], SYSTEM_CLASS_WEIGHT_FLAG | super::super::NICE_0_LOAD);
        assert!(pager_with(&scope, 593, ID + 3, ID));
        // Earlier exact reply wait is a native input, not donation/wake authority.
        runqueue::wait::set_reason(slots[0], runqueue::wait::REASON_ENDPOINT_REPLY, 593);
        assert!(super::super::wake_publication::reply_with(&scope, 593, ID, &|_| 1));
        assert!(!donation_ledger::release_reply(593, IpcReply));
        assert!(system_class_with(&scope, ID)); // exact pager edge survives equal number
        assert!(donation_ledger::release_reply(593, PagerFault));
        assert!(!system_class_with(&scope, ID));
        assert!(bind_with(&scope, 594, ID + 1, ID + 2, true));
        let stop = terminal_quarantine::begin(slots[2], Some(ID + 2), TaskRetireReason::Exited, false).unwrap();
        assert!(!super::super::wake_publication::reply_with(&scope, 594, ID + 2, &|_| 1));
        assert!(!donation_ledger::release_reply(594, IpcReply));
        drop(stop);
        super::super::sync_handoff::remove_slot_all_cpus(slots[0]);
    });
}

#[test]
pub(in crate::multitask::scheduler) fn actual_noncurrent_donor_requires_original_bound_resource_and_full_context() {
    born(|slots| {
        let scope = masked();
        task_directory::record(ID + 1, slots[0]);
        let original = participant(ID + 1).unwrap();
        let expected = stack_resources::context::read(slots[1]).unwrap().scheduling_context.identity();
        {
            use nucleus_core::util::lockdep::{LockClass, work_budget};
            let budget = work_budget::declare(LockClass::TaskStackResources, 1);
            assert_eq!(stack_resources::context::retained_scheduling_context(&original), Some(expected));
            assert_eq!(budget.used(), Some(1), "one original participant qualification is one resource leaf");
        }
        stack_resources::task_start::test_mismatched_start(slots[1], ID + 99, || {
            assert!(stack_resources::context::retained_scheduling_context(&original).is_none());
        });
        drop(original);
        let admission = reserve_call_with(&scope, ID + 1);
        assert!(admission.donation_reserved);
        assert!(!admission.system_class);
        assert_eq!(admission.scheduling_context_owner_task_id, Some(ID + 1));
        assert_eq!(
            admission.scheduling_context,
            Some(
                stack_resources::context::read(slots[1])
                    .unwrap()
                    .scheduling_context
                    .identity()
            )
        );
        assert!(donation_ledger::cancel_reservation(ID + 1));
        task_directory::record(ID + 99, slots[1]);
        let absent = reserve_call_with(&scope, ID + 99);
        assert!(!absent.donation_reserved);
        assert_eq!(absent.scheduling_context, None);
        task_directory::forget(ID + 99);
        let original = identity::read(slots[1]).unwrap();
        identity::publish_metadata(slots[1], None);
        assert!(!reserve_call_with(&scope, ID + 1).donation_reserved);
        identity::publish_metadata(slots[1], Some(original));
        stack_resources::context::test_mismatched_context(slots[1], ID + 99, || {
            let original = participant(ID + 1).unwrap();
            assert!(stack_resources::context::retained_scheduling_context(&original).is_none());
            drop(original);
            let refused = reserve_call_with(&scope, ID + 1);
            assert!(!refused.donation_reserved);
            assert!(refused.scheduling_context.is_none());
            assert_eq!(donation_ledger::live_len(), 0);
        });
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_class_admission_uses_trusted_bit_and_preserves_ordinary_custody() {
    born(|slots| {
        let scope = masked();
        let original = runqueue::weight::value(slots[1]);
        assert!(!system_class_with(&scope, ID + 1));
        runqueue::weight::replace(slots[1], original | SYSTEM_CLASS_WEIGHT_FLAG);
        assert!(system_class_with(&scope, ID + 1));
        assert!(reserve_call_with(&scope, ID + 1).system_class);
        assert!(donation_ledger::cancel_reservation(ID + 1));
        runqueue::weight::replace(slots[1], original & !SYSTEM_CLASS_WEIGHT_FLAG);
        let ordinary = reserve_call_with(&scope, ID + 1);
        assert!(!ordinary.system_class);
        assert!(ordinary.donation_reserved);
        runqueue::weight::replace(slots[1], original);
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_participant_loans_survive_capture_and_terminal_seal_denies_new_admission() {
    born(|slots| {
        let before = identity::execution::count(slots[1]);
        let loans = Loans::capture(ID + 1).unwrap();
        assert_eq!(identity::execution::count(slots[1]), before + 1);
        let stop =
            terminal_quarantine::begin(slots[1], Some(ID + 1), TaskRetireReason::Exited, false)
                .unwrap();
        assert!(Loans::capture(ID + 1).is_none());
        // The original loan precedes the seal. Producer drain cannot complete
        // before this bounded edge insertion and original participant release.
        assert!(donation_ledger::reserve_owned(&loans, false));
        assert!(donation_ledger::cancel_reservation(ID + 1));
        drop(loans);
        assert_eq!(identity::execution::count(slots[1]), before);
        drop(stop);
    });
}
#[test]
fn actual_cancelled_original_source_refuses_after_capture_before_reservation_insert() {
    born(|slots| {
        let scope = masked();
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(bind_with(&scope, 31, ID, ID + 1, false));
        let before = [
            identity::execution::count(slots[0]),
            identity::execution::count(slots[1]),
        ];
        let loans = Loans::capture(ID + 1).unwrap();
        assert_eq!(loans.basis.root(), (slots[0], ID));
        assert_eq!(identity::execution::count(slots[0]), before[0] + 1);
        assert_eq!(identity::execution::count(slots[1]), before[1] + 1);
        assert!(donation_ledger::release_reply(31, IpcReply));
        let len = donation_ledger::live_len();
        assert!(!donation_ledger::reserve_owned(&loans, false));
        assert_eq!(donation_ledger::live_len(), len);
        drop(loans);
        assert_eq!(identity::execution::count(slots[0]), before[0]);
        assert_eq!(identity::execution::count(slots[1]), before[1]);
    });
}
#[test]
fn actual_changed_still_live_source_refuses_stale_new_reservation_basis() {
    born(|slots| {
        let scope = masked();
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(bind_with(&scope, 35, ID, ID + 1, false));
        let original = Loans::capture(ID + 1).unwrap();
        assert_eq!(original.basis.root(), (slots[0], ID));
        assert!(reserve_call_with(&scope, ID + 2).donation_reserved);
        assert!(bind_with(&scope, 36, ID + 2, ID + 1, false));
        assert!(!donation_ledger::reserve_owned(&original, false));
        let fresh = reserve_call_with(&scope, ID + 1);
        assert!(fresh.donation_reserved);
        assert_eq!(fresh.scheduling_context_owner_task_id, Some(ID + 2));
        assert!(donation_ledger::cancel_reservation(ID + 1));
    });
}
#[test]
fn actual_reservation_retains_original_source_when_a_newer_reply_changes_current_funding() {
    born(|slots| {
        let scope = masked();
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(bind_with(&scope, 41, ID, ID + 1, false));
        let nested = reserve_call_with(&scope, ID + 1);
        assert!(nested.donation_reserved);
        assert_eq!(nested.scheduling_context_owner_task_id, Some(ID));
        assert!(reserve_call_with(&scope, ID + 2).donation_reserved);
        assert!(bind_with(&scope, 42, ID + 2, ID + 1, false));
        assert_eq!(
            donation_ledger::borrowed_context_owner_slot(slots[1]),
            Some(slots[2])
        );
        assert!(bind_with(&scope, 43, ID + 1, ID + 3, false));
        assert_eq!(
            donation_ledger::borrowed_context_charge_token(slots[3]),
            Some((slots[0], 43))
        );
        assert!(reserve_call_with(&scope, ID + 3).donation_reserved);
        assert!(donation_ledger::release_reply(41, IpcReply));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[3]), None);
        // Intermediate source remains as inactive metadata, not fresh funding.
        assert!(!bind_with(&scope, 44, ID + 3, ID + 2, false));
    });
}
#[test]
fn actual_root_cancellation_cannot_revive_a_reserved_nested_edge_or_replace_it_with_self_funding() {
    born(|slots| {
        let scope = masked();
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(bind_with(&scope, 51, ID, ID + 1, false));
        assert!(reserve_call_with(&scope, ID + 1).donation_reserved);
        assert!(donation_ledger::release_reply(51, IpcReply));
        let len = donation_ledger::live_len();
        assert!(!bind_with(&scope, 52, ID + 1, ID + 2, false));
        assert!(!bind_with(&scope, 52, ID + 1, ID + 2, true));
        assert_eq!(donation_ledger::live_len(), len);
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[2]), None);
        assert!(donation_ledger::cancel_reservation(ID + 1));
        // New unrelated call after cancelling the original reservation may use
        // the worker's own actual context. The old call never obtains this grant.
        assert!(reserve_call_with(&scope, ID + 1).donation_reserved);
        assert!(bind_with(&scope, 53, ID + 1, ID + 2, false));
        assert_eq!(
            donation_ledger::borrowed_context_owner_slot(slots[2]),
            Some(slots[1])
        );
    });
}
#[test]
fn actual_pager_and_reply_equal_numbers_have_distinct_owned_edges_and_exact_receivers() {
    born(|slots| {
        let scope = masked();
        assert!(pager_with(&scope, 61, ID, ID + 1));
        assert!(reserve_call_with(&scope, ID + 2).donation_reserved);
        assert!(bind_with(&scope, 61, ID + 2, ID + 3, false));
        assert!(donation_ledger::release_reply(61, PagerFault));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[1]), None);
        assert_eq!(
            donation_ledger::borrowed_context_owner_slot(slots[3]),
            Some(slots[2])
        );
        assert!(!pager_with(&scope, 62, ID, ID + 99));
        let original = identity::read(slots[1]).unwrap();
        identity::publish_metadata(slots[1], None);
        assert!(!pager_with(&scope, 62, ID, ID + 1));
        identity::publish_metadata(slots[1], Some(original));
        assert!(!bind_with(&scope, 63, ID, ID, true));
        assert!(donation_ledger::release_reply(61, IpcReply));
    });
}
#[test]
fn actual_reply_attachment_never_selects_a_runnable_process_worker_before_exact_receive() {
    born(|slots| {
        let scope = masked();
        runqueue::weight::replace(slots[0], super::super::SYSTEM_CLASS_WEIGHT_FLAG
            | super::super::NICE_0_LOAD);
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(donation_ledger::attach(81, IpcReply, ID));
        for &slot in &slots[1..] {
            assert_eq!(donation_ledger::borrowed_context_owner_slot(slot), None);
            assert!(!donation_ledger::inherited_system(slot));
        }
        // Actual queue-policy changes are only hints, not IPC_RECV ownership.
        runqueue::lower_vruntime_ceiling(slots[1], 0);
        runqueue::lower_vruntime_ceiling(slots[3], 0);
        assert!(donation_ledger::attach(81, IpcReply, ID));
        for &slot in &slots[1..] {
            assert_eq!(donation_ledger::borrowed_context_owner_slot(slot), None);
        }
        assert!(bind_with(&scope, 81, ID, ID + 2, false));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[2]),
            Some((slots[0], 81)));
        assert!(donation_ledger::inherited_system(slots[2]));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[1]), None);
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[3]), None);
        assert!(donation_ledger::release_reply(81, IpcReply));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[2]), None);
        assert!(!donation_ledger::inherited_system(slots[2]));
    });
}

#[test]
fn actual_queued_receive_moves_original_reply_funding_from_wake_candidate_to_consumer() {
    use kernel_ipc_runtime::api::{self as ipc, endpoint, ReplySchedulingContextCustody};
    let observed = core::cell::Cell::new(None);
    born(|slots| {
        let scope = masked();
        runqueue::weight::replace(slots[0], SYSTEM_CLASS_WEIGHT_FLAG | super::super::NICE_0_LOAD);
        let admission = reserve_call_with(&scope, ID);
        assert!(admission.donation_reserved);
        assert!(admission.system_class);
        let custody = ReplySchedulingContextCustody::new_with_owner(
            admission.scheduling_context.unwrap(), ID, ID).unwrap();
        let endpoint = endpoint::create_for_process(ID).unwrap();
        assert!(!ipc::add_endpoint_receiver_waiter_with_capacity(endpoint, ID + 1, 16).unwrap());
        let (reply, candidate) = endpoint::enqueue_call_with_handles_priority_and_custody(
            endpoint, ID, &[0xa7; 16], &[], ipc::EndpointCallPriority::System, custody).unwrap();
        assert_eq!(candidate, Some(ID + 1));
        assert!(donation_ledger::attach(reply.raw(), IpcReply, ID));
        // The SAME real sender-side bind also occurs before a fallible wake.
        // A sibling may actually dequeue before that selected waiter resumes.
        assert!(bind_with(&scope, reply.raw(), ID, ID + 1, false));
        let received = endpoint::receive_or_wait(endpoint, ID + 2, 16, 0, false).unwrap();
        assert!(matches!(received, endpoint::EndpointReceive::Queued((actual, ref bytes, _, caller))
            if actual == reply && bytes == &[0xa7; 16] && caller == ID));
        assert!(bind_with(&scope, reply.raw(), ID, ID + 2, true));
        // A late sender hint must not move authority back off the consumer.
        assert!(bind_with(&scope, reply.raw(), ID, ID + 1, false));
        observed.set(Some((slots[0], reply.raw(),
            donation_ledger::borrowed_context_charge_token(slots[1]),
            donation_ledger::borrowed_context_charge_token(slots[2]),
            donation_ledger::inherited_system(slots[1]),
            donation_ledger::inherited_system(slots[2]))));
        let cancelled = ipc::cancel_endpoint_call_with_transfers(reply, ID).unwrap();
        assert!(cancelled.scheduling_context.is_some());
        assert!(donation_ledger::release_reply(reply.raw(), IpcReply));
        let _ = ipc::fail_endpoints_owned_by_process(ID, ipc::IpcError::PeerClosed);
    });
    let (root, reply, candidate, consumer, candidate_system, consumer_system) = observed.get().unwrap();
    assert_eq!((candidate, consumer, candidate_system, consumer_system),
        (None, Some((root, reply)), false, true),
        "only the actual dequeuing worker may spend the original caller's budget");
}

#[test]
fn actual_unmasked_or_foreign_cpu_scope_refuses_all_donation_effects() {
    born(|_| {
        let unmasked = DispatchPreemption::enter_model_with_enabled_irqs(0);
        let len = donation_ledger::live_len();
        assert!(!reserve_call_with(&unmasked, ID).donation_reserved);
        assert!(!reserve_priority_with(&unmasked, ID));
        assert!(!system_class_with(&unmasked, ID));
        assert!(!pager_with(&unmasked, 71, ID, ID + 1));
        assert!(!bind_with(&unmasked, 72, ID, ID + 1, true));
        assert_eq!(donation_ledger::live_len(), len);
        drop(unmasked);
        let mut foreign = DispatchPreemption::enter_model(1);
        foreign.mask_irqs();
        assert!(!reserve_call_with(&foreign, ID).donation_reserved);
        assert!(!pager_with(&foreign, 73, ID, ID + 1));
        assert_eq!(donation_ledger::live_len(), len);
    });
}
