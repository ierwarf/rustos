//! Owner: historical IPC obligations over SAME original publication/ledger/funding engines.
//! Boundary: real births and participant loans; earlier queue/running selection is modeled.
//! Lifecycle: reserve -> exact receive -> original root interval -> revoke -> owned cleanup.
//! Concurrency: namespace isolation, source cancellation and terminal refusal preserve original loans.
//! Failure: self/closed/unknown participants and full capacity publish no orphan edge.
//! Forbidden: recursive catalog mirrors, scalar donor keys as interval lifetime, fake hardware ACK.
//! Evidence: old witness IDs call these actual-owner helpers, including depth beyond four.
use super::*;
use alloc::vec;
use super::super::super::{execution_interval, scheduling_runtime};

pub(in crate::multitask::scheduler) fn transitive() {
    born(|slots| {
        let scope = masked();
        runqueue::weight::replace(slots[0], SYSTEM_CLASS_WEIGHT_FLAG | super::super::super::NICE_0_LOAD);
        assert!(!donation_ledger::inherited_system(slots[1]));
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(donation_ledger::attach(10, IpcReply, ID));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[1]), None);
        assert!(!donation_ledger::inherited_system(slots[1]));
        assert!(bind_with(&scope, 10, ID, ID + 1, false));
        // Earlier exact receive wins; a later sender waiter cannot replace it.
        assert!(bind_with(&scope, 10, ID, ID + 3, false));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[3]), None);
        assert!(donation_ledger::inherited_system(slots[1]));
        let nested = reserve_call_with(&scope, ID + 1);
        assert!(nested.system_class && nested.donation_reserved);
        assert_eq!(nested.scheduling_context_owner_task_id, Some(ID));
        assert!(bind_with(&scope, 11, ID + 1, ID + 2, false));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[2]), Some((slots[0], 11)));
        assert!(donation_ledger::inherited_system(slots[2]));
        let queued = runqueue::owner(slots[2]);
        assert!(donation_ledger::release_reply(10, IpcReply));
        for slot in [slots[1], slots[2]] {
            assert!(!donation_ledger::inherited_system(slot));
            assert_eq!(donation_ledger::borrowed_context_owner_slot(slot), None);
        }
        assert_eq!(runqueue::owner(slots[2]), queued);
        assert!(!donation_ledger::release_reply(10, IpcReply));
        assert!(donation_ledger::release_reply(11, IpcReply));
        assert_eq!(donation_ledger::live_len(), 0);
    });
    super::actual_reply_attachment_never_selects_a_runnable_process_worker_before_exact_receive();
}

pub(in crate::multitask::scheduler) fn bounded_capacity() {
    born(|slots| {
        let scope = masked();
        for reply in 1..=MAX_TASK as u64 {
            assert!(reserve_call_with(&scope, ID).donation_reserved);
            assert!(bind_with(&scope, reply, ID, ID + 1, false));
            assert_eq!(donation_ledger::live_len(), reply as usize);
        }
        let queue = runqueue::owner(slots[1]);
        assert!(!reserve_call_with(&scope, ID).donation_reserved);
        assert!(!pager_with(&scope, 0x10000, ID, ID + 2));
        assert_eq!(donation_ledger::live_len(), MAX_TASK);
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[2]), None);
        assert_eq!(runqueue::owner(slots[1]), queue);
        // A released exact record restores one capacity, not a fabricated len.
        assert!(donation_ledger::release_reply(1, IpcReply));
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(!reserve_call_with(&scope, ID).donation_reserved);
        assert!(donation_ledger::cancel_reservation(ID));
        assert!(!donation_ledger::cancel_reservation(ID));
    });
}

pub(in crate::multitask::scheduler) fn self_and_terminal_refusal() {
    born(|slots| {
        let scope = masked();
        assert!(!bind_with(&scope, 20, ID, ID, true));
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(!bind_with(&scope, 20, ID, ID, false));
        assert_eq!(donation_ledger::live_len(), 1);
        let donor = Loans::capture(ID).unwrap();
        let receiver = participant(ID).unwrap();
        let self_loans = ReceiveLoans { donor: &donor, receiver: &receiver };
        // Exercise the actual ledger's OWN independent self-loop refusals as
        // well as the outer publisher: neither layer relies on the other's guard.
        assert!(!donation_ledger::bind_owned(&self_loans, 20, IpcReply));
        assert!(donation_ledger::cancel_reservation(ID));
        assert!(!donation_ledger::upsert_owned(&self_loans, 21, IpcReply));
        drop(receiver);
        drop(donor);
        let seal = terminal_quarantine::begin(slots[1], Some(ID + 1), TaskRetireReason::Exited, false).unwrap();
        assert!(!bind_with(&scope, 22, ID, ID + 1, true));
        assert!(!pager_with(&scope, 22, ID, ID + 1));
        assert_eq!(donation_ledger::live_len(), 0);
        drop(seal);
    });
}

pub(in crate::multitask::scheduler) fn bounded_chain() {
    // The removed recursive algorithm truncated at four. The actual independent
    // owner carries a root-compressed source; deeper topology must NOT truncate
    // or grow dispatch's stack. Every new source still requires live original loans.
    root_creation::tests::exec_root(ID, true, boot(), |leader, handle| {
        let mut slots = vec![leader];
        for i in 1..=8 {
            slots.push(thread_creation::tests::active_sibling(leader, ID + i, boot()));
        }
        let scope = masked();
        runqueue::weight::replace(leader, SYSTEM_CLASS_WEIGHT_FLAG | super::super::super::NICE_0_LOAD);
        for i in 0..8 {
            let admission = reserve_call_with(&scope, ID + i);
            assert!(admission.donation_reserved && admission.system_class);
            assert_eq!(admission.scheduling_context_owner_task_id, Some(ID));
            assert!(bind_with(&scope, 100 + i, ID + i, ID + i + 1, false));
            assert_eq!(donation_ledger::borrowed_context_charge_token(slots[i as usize + 1]), Some((leader, 100 + i)));
            assert!(donation_ledger::inherited_system(slots[i as usize + 1]));
        }
        // Capture ORIGINAL final source before cancellation. Even though the
        // participant/root pins survive, source revocation must deny insertion.
        let loans = Loans::capture(ID + 8).unwrap();
        assert!(reserve_call_with(&scope, ID + 8).donation_reserved);
        assert!(donation_ledger::release_reply(100, IpcReply));
        assert!(!donation_ledger::reserve_owned(&loans, true));
        // The reserved ORIGINAL basis survives as metadata, not fresh custody.
        // The bind guard must independently refuse it after source revocation.
        assert!(!bind_with(&scope, 109, ID + 8, ID + 1, false));
        assert!(donation_ledger::cancel_reservation(ID + 8));
        let fresh = reserve_call_with(&scope, ID + 8);
        assert!(fresh.donation_reserved && !fresh.system_class);
        assert_eq!(fresh.scheduling_context_owner_task_id, Some(ID + 8));
        assert!(donation_ledger::cancel_reservation(ID + 8));
        for &slot in &slots[1..] {
            assert!(!donation_ledger::inherited_system(slot));
            assert_eq!(donation_ledger::borrowed_context_charge_token(slot), None);
        }
        drop(loans);
        drop(scope);
        for i in 0..=8 { donation_ledger::release_task(ID + i); }
        for i in (1..=8).rev() {
            thread_creation::tests::release_born(slots[i], ID + i as u64, handle);
        }
    });
}

pub(in crate::multitask::scheduler) fn nested_interval() {
    root_creation::tests::donation_roots(ID, |slots, scope| {
        let root = participant(ID).unwrap();
        let worker = participant(ID + 2).unwrap();
        let outer = reserve_call_with(scope, ID);
        assert!(outer.donation_reserved);
        assert!(bind_with(scope, 10, ID, ID + 1, false));
        assert!(bind_with(scope, 10, ID, ID + 2, false));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[2]), None);
        let nested = reserve_call_with(scope, ID + 1);
        assert!(nested.donation_reserved);
        assert_eq!(nested.scheduling_context, outer.scheduling_context);
        assert_eq!(nested.scheduling_context_owner_task_id, Some(ID));
        assert!(bind_with(scope, 11, ID + 1, ID + 2, false));
        // Explicit earlier Running/selection input, NOT architecture admission.
        // The actual capture owns its non-Copy paired FundingPin until charge.
        runqueue::test_running_claim(slots[2], 0, runqueue::weight::value(slots[2]));
        execution_interval::test_open_captured(slots[2], 1);
        assert!(donation_ledger::release_reply(11, IpcReply));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[2]), None);
        let execution_interval::ConsumedInterval::Active(original) = execution_interval::consume(0, slots[2], ID + 2) else { panic!("lost original nested interval") };
        let (owner, outcome) = original.charge(100, 4500).unwrap();
        assert_eq!((owner, outcome.charged_ns, outcome.overrun_ns), (ID, 4000, 500));
        assert!(matches!(execution_interval::consume(0, slots[2], ID + 2), execution_interval::ConsumedInterval::AlreadySettled));
        // Original physical participant loans still exclude teardown of both
        // contexts. These are observations, never new execution authorization.
        let caller = stack_resources::context::read(root.slot()).unwrap().scheduling_context;
        let snapshot = caller.runtime_snapshot().unwrap();
        assert_eq!((snapshot.available_ns, snapshot.pending_refill_ns), (0, 4000));
        assert_eq!((snapshot.timeout_fault_count, snapshot.timeout_fault_consumed_ns, snapshot.timeout_fault_reply, snapshot.timeout_fault_action), (1, 4000, 11, 1));
        let domain = scheduling_runtime::domain_snapshot(usize::from(caller.domain_slot().unwrap()), caller.domain_generation().unwrap(), caller.policy().unwrap()).unwrap();
        // CPU 0's domain bank funded the interval; the other per-CPU banks of
        // the whole-domain ledger keep their full share.
        let untouched_banks = (nucleus_core::util::lockdep::MAX_TRACKED_CPUS as u64 - 1) * 4000;
        assert_eq!((domain.available_ns, domain.pending_refill_ns, domain.consumed_ns), (untouched_banks, 4000, 4000));
        // A later modeled interval must use the worker's OWN independent 7us
        // context/domain. It cannot re-spend the cancelled old 4us root.
        execution_interval::test_open_captured(slots[2], 2);
        let execution_interval::ConsumedInterval::Active(native) = execution_interval::consume(0, slots[2], ID + 2) else { panic!("missing own interval") };
        let (owner, outcome) = native.charge(200, 4500).unwrap();
        assert_eq!((owner, outcome.charged_ns, outcome.overrun_ns), (ID + 2, 4500, 0));
        drop(worker);
        drop(root);
    });
}
