//! Owner: old ledger witness IDs over original physically born donor/root/receiver loans.
//! Boundary: SAME owned publication and bounded ledger, not scalar insertion wrappers.
//! Lifecycle: exact receive/repeat -> namespace-specific or terminal withdrawal -> restore.
//! Concurrency: serialized native namespace; cancellation and repeated receive are explicit inputs.
//! Failure: duplicate reservation and foreign bind refuse without leaking receiver counters.
//! Forbidden: synthetic UID-only grants or these native fixtures as hardware acceptance.
//! Evidence: all historical ledger IDs retained and mutations still hit actual shared engines.
use super::*;

pub(in crate::multitask::scheduler) fn namespaces(pager_first: bool) {
    born(|slots| {
        let scope = masked();
        for i in [0, 2] { runqueue::weight::replace(slots[i], SYSTEM_CLASS_WEIGHT_FLAG | super::super::super::NICE_0_LOAD); }
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(bind_with(&scope, 771, ID, ID + 1, false));
        assert!(pager_with(&scope, 771, ID + 2, ID + 1));
        assert_eq!(donation_ledger::live_len(), 2);
        assert!(donation_ledger::inherited_system(slots[1]));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), Some((slots[2], 771)));
        let (first, second, remaining) = if pager_first {
            (PagerFault, IpcReply, slots[0])
        } else { (IpcReply, PagerFault, slots[2]) };
        assert!(donation_ledger::release_reply(771, first));
        assert!(!donation_ledger::release_reply(771, first));
        assert_eq!(donation_ledger::live_len(), 1);
        assert!(donation_ledger::inherited_system(slots[1]));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), Some((remaining, 771)));
        assert!(donation_ledger::release_reply(771, second));
        assert!(!donation_ledger::inherited_system(slots[1]));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), None);
        assert_eq!(donation_ledger::live_len(), 0);
    });
}

pub(in crate::multitask::scheduler) fn exact_bind() {
    born(|slots| {
        let scope = masked();
        runqueue::weight::replace(slots[0], SYSTEM_CLASS_WEIGHT_FLAG | super::super::super::NICE_0_LOAD);
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(!reserve_call_with(&scope, ID).donation_reserved);
        assert!(bind_with(&scope, 701, ID, ID + 1, false));
        assert!(bind_with(&scope, 701, ID, ID + 1, false));
        assert!(bind_with(&scope, 701, ID, ID + 2, false));
        assert!(!bind_with(&scope, 701, ID + 3, ID + 2, false));
        assert_eq!(donation_ledger::live_len(), 1);
        assert!(donation_ledger::inherited_system(slots[1]));
        assert!(!donation_ledger::inherited_system(slots[2]));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), Some((slots[0], 701)));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[2]), None);
        assert!(donation_ledger::release_reply(701, IpcReply));
        assert!(!donation_ledger::inherited_system(slots[1]));
    });
}

pub(in crate::multitask::scheduler) fn endpoint_retire() {
    born(|slots| {
        let scope = masked();
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(donation_ledger::attach(701, IpcReply, ID));
        donation_ledger::release_task(ID);
        assert_eq!(donation_ledger::live_len(), 0);
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(bind_with(&scope, 702, ID, ID + 1, false));
        donation_ledger::release_task(ID + 1);
        assert_eq!(donation_ledger::live_len(), 0);
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[1]), None);
    });
}

pub(in crate::multitask::scheduler) fn ordinary() {
    born(|slots| {
        let scope = masked();
        let first = reserve_call_with(&scope, ID);
        assert!(first.donation_reserved && !first.system_class);
        assert!(bind_with(&scope, 710, ID, ID + 1, false));
        let second = reserve_call_with(&scope, ID + 1);
        assert!(second.donation_reserved && !second.system_class);
        assert_eq!(second.scheduling_context, first.scheduling_context);
        assert_eq!(second.scheduling_context_owner_task_id, Some(ID));
        assert!(bind_with(&scope, 711, ID + 1, ID + 2, false));
        for slot in [slots[1], slots[2]] {
            assert!(!donation_ledger::inherited_system(slot));
            assert_eq!(donation_ledger::borrowed_context_owner_slot(slot), Some(slots[0]));
        }
        assert!(donation_ledger::release_reply(710, IpcReply));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(slots[2]), None);
        assert!(donation_ledger::release_reply(711, IpcReply));
    });
}

pub(in crate::multitask::scheduler) fn restore_previous() {
    born(|slots| {
        let scope = masked();
        assert!(reserve_call_with(&scope, ID).donation_reserved);
        assert!(bind_with(&scope, 810, ID, ID + 3, false));
        assert!(reserve_call_with(&scope, ID + 1).donation_reserved);
        assert!(bind_with(&scope, 811, ID + 1, ID + 3, false));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[3]), Some((slots[1], 811)));
        assert!(donation_ledger::release_reply(811, IpcReply));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[3]), Some((slots[0], 810)));
        assert!(donation_ledger::release_reply(810, IpcReply));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[3]), None);
        assert!(donation_ledger::test_charge_publication_is_empty(slots[3]));
    });
}

/// A self-funded ordinary edge published lazily is, to every ledger reader, exactly
/// the edge the eager protocol inserts; the lock-free release leaves no trace.
pub(in crate::multitask::scheduler) fn lazy_edges() {
    use donation_ledger::lazy_state_for_tests as state;
    born(|slots| {
        // Reserve + bind + lane release without the ledger leaf.
        assert!(donation_ledger::reserve_own_lazy(slots[0], ID, false));
        assert_eq!(state(slots[0]), 1);
        assert_eq!(donation_ledger::bind_own_lazy(slots[0], ID, 801, slots[1], ID + 1), Some(true));
        assert_eq!(state(slots[0]), 2);
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), Some((slots[0], 801)));
        assert!(donation_ledger::release_reply_lane(slots[0], ID, 801));
        assert_eq!(state(slots[0]), 0);
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), None);
        assert_eq!(donation_ledger::live_len(), 0);

        // A ledger reader settles a bound lazy edge into the eager entry.
        assert!(donation_ledger::reserve_own_lazy(slots[0], ID, false));
        assert_eq!(donation_ledger::bind_own_lazy(slots[0], ID, 802, slots[1], ID + 1), Some(true));
        assert_eq!(donation_ledger::live_len(), 1);
        assert_eq!(state(slots[0]), 0);
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), Some((slots[0], 802)));
        assert!(!donation_ledger::release_reply_lane(slots[0], ID, 801), "other reply");
        assert!(donation_ledger::release_reply(802, IpcReply));
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), None);
        assert_eq!(donation_ledger::live_len(), 0);

        // A reserved lazy edge is an ordinary reservation to cancel, attach and bind.
        assert!(donation_ledger::reserve_own_lazy(slots[0], ID, false));
        assert!(donation_ledger::cancel_reservation(ID));
        assert!(!donation_ledger::cancel_reservation(ID));
        assert!(donation_ledger::reserve_own_lazy(slots[0], ID, false));
        assert!(donation_ledger::attach(803, IpcReply, ID));
        assert_eq!(state(slots[0]), 0);
        assert!(donation_ledger::release_reply(803, IpcReply));
        assert!(donation_ledger::reserve_own_lazy(slots[0], ID, false));
        assert!(bind_with(&masked(), 804, ID, ID + 1, false));
        assert!(donation_ledger::release_reply_lane(slots[0], ID, 804), "settled edge releases through the ledger");
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), None);
        assert_eq!(donation_ledger::live_len(), 0);

        // Task retirement settles then withdraws.
        assert!(donation_ledger::reserve_own_lazy(slots[0], ID, false));
        assert_eq!(donation_ledger::bind_own_lazy(slots[0], ID, 805, slots[1], ID + 1), Some(true));
        donation_ledger::release_task(ID + 1);
        assert_eq!(donation_ledger::live_len(), 0);
        assert_eq!(donation_ledger::borrowed_context_charge_token(slots[1]), None);

        // Never lazy: a priority edge, a second edge for one donor, a receiver that
        // already carries a borrowed context.
        assert!(!donation_ledger::lazy_reserve_for_tests(slots[0], ID, true));
        assert!(donation_ledger::reserve_own_lazy(slots[0], ID, false));
        assert!(!donation_ledger::lazy_reserve_for_tests(slots[0], ID, false));
        assert!(donation_ledger::cancel_reservation(ID));
        assert!(donation_ledger::reserve_own_lazy(slots[0], ID, false));
        assert_eq!(donation_ledger::bind_own_lazy(slots[0], ID, 806, slots[1], ID + 1), Some(true));
        assert!(donation_ledger::reserve_own_lazy(slots[2], ID + 2, false));
        assert_eq!(donation_ledger::bind_own_lazy(slots[2], ID + 2, 807, slots[1], ID + 1), None,
            "receiver already serves a borrowed context");
        assert!(donation_ledger::cancel_reservation(ID + 2));
        assert!(donation_ledger::release_reply(806, IpcReply));
        assert_eq!(donation_ledger::live_len(), 0);
    });
}
