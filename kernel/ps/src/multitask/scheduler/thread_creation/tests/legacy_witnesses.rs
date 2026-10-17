//! Whole legacy thread-reservation obligations use actual physical Ticket/MM,
//! Join and parent Cohort owners. Earlier fixture entry/ABI is native input.
use super::*;

pub(in crate::multitask::scheduler) fn reserved_exclusion() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |_| {
        // Intercept the real producer BEFORE storage exists too. Even that
        // partial prefix belongs to its original Ticket, not a vacant context.
        let pending = stack_resources::admission::reserve(FIRST_DYNAMIC_TASK_SLOT, ID + 7).unwrap();
        assert!(root_slots::reserve(pending.slot(), ID + 9).is_none());
        assert!(pending.is_reserved_current());
        assert!(pending.begin_cancel());
        pending.finish_cancel();
        let r = reserve_actual();
        let slot = r.ticket().slot();
        let generation = r.ticket().generation();
        assert!(stack_resources::context::read(slot).is_none());
        // A process producer and another thread producer BOTH use the real
        // admission engine; an absent context does not release the Ticket.
        assert!(root_slots::reserve(slot, ID + 9).is_none());
        assert!(stack_resources::admission::reserve(slot, ID + 10).is_none());
        assert!(r.ticket().is_reserved_current());
        assert_eq!(r.ticket().generation(), generation);
        let next = reserve_with(ID + 11, &capture_creator(ROOT).unwrap()).unwrap();
        assert_ne!(next.ticket().slot(), slot);
        cancel(next);
        cancel(r);
        let replacement = root_slots::reserve(slot, ID + 12).unwrap();
        assert_ne!(replacement.generation(), generation);
        root_slots::cancel_unpublished(replacement);
    });
}

pub(in crate::multitask::scheduler) fn inheritance() {
    affinity_fixture(UserAbi::Windows, UserAbi::Windows, false, |h| {
        let r = reserve_actual();
        assert!(Publication::capture(&r, UserAbi::Linux).is_none());
        for mask in [4, 3] {
            super::super::super::process_affinity::tests::change_blocked_process_mask(h, mask);
            let p = Publication::capture(&r, UserAbi::Windows).unwrap();
            assert_eq!(p.task_mask, mask);
            assert_eq!(p.membership.mask(), mask);
            drop(p);
        }
        // No real UID reuse is permitted while the original reservation/MM
        // remain retained. Corrupt only the copied creator start, refusal-only.
        stack_resources::task_start::test_mismatched_start(ROOT, ID + 99, || {
            assert!(Publication::capture(&r, UserAbi::Windows).is_none());
        });
        assert_eq!(stack_resources::task_start::read(ROOT).unwrap().id, ID);
        identity::update_live_lifecycle(ROOT, ID, identity::LifecycleGate::Retired, true);
        assert!(Publication::capture(&r, UserAbi::Windows).is_none());
        cancel(r);
    });
}

pub(in crate::multitask::scheduler) fn rejected_attachment() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |_| {
        let creator = capture_creator(ROOT).unwrap();
        let before = process_table::thread_count_by_pid(ID).unwrap();
        let allocated: [bool; MAX_TASK] = core::array::from_fn(stack_resources::allocated);
        process_table::mark_process_exiting(ID).unwrap();
        assert!(reserve_with(ID + 8, &creator).is_none());
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            assert_eq!(stack_resources::allocated(slot), allocated[slot]);
            assert!(!stack_resources::admission::is_reserved(slot));
        }
        assert_eq!(process_table::thread_count_by_pid(ID), Some(before));
    });
}
