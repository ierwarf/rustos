//! Owner: real process-object/census/intent admission witnesses.
//! Boundary: membership and existing join/mask custody, not physical image/RSP or exit proof.
//! Lifecycle: exact intent and target captured; removal never makes late creation admissible.
//! Concurrency: real Class55 arbitration; native fixture isolates table -> CPU -> runtime -> queue.
//! Failure: checked epoch exhaustion/foreign target/refusal precede occupancy; post-stage Drop poisons.
//! Forbidden: native injected phase/epoch/metadata recovery is not production reopening.
//! Evidence: real process pin and reservation, actual Intent and creation/member ledger APIs.
use super::*;
use crate::multitask::current_identity as identity;
use crate::multitask::scheduler;
const SLOT: usize = 118;
const CHILD: usize = 119;
const PID: u64 = 118001;
fn metadata(handle: ProcessHandle, id: u64) -> identity::TaskIdentity {
    identity::TaskIdentity {
        task_id: Some(id),
        user_mode: true,
        abi: Some(UserAbi::Linux),
        process_handle: Some(handle),
        process_id: Some(PID),
        console_session: crate::io::session::ConsoleSessionHandle::SYSTEM,
        pager_charge: None,
    }
}
fn fixture(f: impl FnOnce(ProcessHandle)) {
    scheduler::with_exec_census_fixture(PID, |handle| {
        reset_for_tests();
        identity::publish_metadata(SLOT, Some(metadata(handle, PID)));
        register_root(handle, SLOT, PID, 3).unwrap();
        f(handle);
        unregister(handle, SLOT, PID);
        unregister(handle, CHILD, PID + 1);
        identity::clear(SLOT);
        identity::clear(CHILD);
        reset_for_tests();
    });
}
fn intent(handle: ProcessHandle) -> Intent {
    let reservation = process_table::begin_exec(handle).unwrap();
    identity::exec_intent::begin(SLOT, PID, reservation, false).unwrap()
}
fn cancel(intent: Intent) {
    let result = intent.cancel_with(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
        process_table::cancel_exec_owned,
    );
    assert!(!result.2);
}
#[test]
fn exec_census_freezes_new_join_root_and_mask_change_before_member_publication_until_exact_finish()
{
    fixture(|h| {
        let join = Join::begin(h, (SLOT, PID), UserAbi::Linux).unwrap();
        join.register(CHILD, PID + 1).unwrap();
        drop(join);
        let intent = intent(h);
        let census = ExecMembership::begin(&intent).unwrap();
        assert_eq!(census.members(), &[(SLOT, PID), (CHILD, PID + 1)]);
        assert!(Join::begin(h, (SLOT, PID), UserAbi::Linux).is_none());
        assert!(register_root(h, 120, PID + 2, 3).is_none());
        assert!(Change::begin(h, (SLOT, PID)).is_none());
        assert!(ExecMembership::begin(&intent).is_none());
        assert_eq!(mask(h), Some(3));
        census.assert_matches(&intent);
        census.finish_for_test();
        cancel(intent);
        let join = Join::begin(h, (SLOT, PID), UserAbi::Linux).unwrap();
        drop(join);
    });
}
#[test]
fn pending_creation_reader_refuses_exec_census_without_spending_epoch_and_drained_child_is_in_snapshot()
 {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, PID), UserAbi::Linux).unwrap();
        let before = cell(h).unwrap().with(|s| s.nonce);
        let intent = intent(h);
        assert!(ExecMembership::begin(&intent).is_none());
        assert_eq!(cell(h).unwrap().with(|s| s.nonce), before);
        join.register(CHILD, PID + 1).unwrap();
        drop(join);
        let census = ExecMembership::begin(&intent).unwrap();
        assert!(census.members().contains(&(CHILD, PID + 1)));
        census.finish_for_test();
        cancel(intent);
    });
}
#[test]
fn retired_member_removal_preserves_captured_uid_and_seal_while_recycled_slot_is_not_new_member() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, PID), UserAbi::Linux).unwrap();
        join.register(CHILD, PID + 1).unwrap();
        drop(join);
        let intent = intent(h);
        let census = ExecMembership::begin(&intent).unwrap();
        unregister(h, CHILD, PID + 1);
        assert!(census.members().contains(&(CHILD, PID + 1)));
        assert!(register_root(h, CHILD, PID + 2, 3).is_none());
        census.assert_matches(&intent);
        census.finish_for_test();
        cancel(intent);
    });
}
#[test]
fn missing_target_foreign_handle_and_exhausted_epoch_refuse_before_partial_exec_gate() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, PID), UserAbi::Linux).unwrap();
        join.register(CHILD, PID + 1).unwrap();
        drop(join);
        let intent = intent(h);
        unregister(h, SLOT, PID);
        assert_eq!(cell(h).unwrap().with(|s| s.handle), Some(h));
        assert!(ExecMembership::begin(&intent).is_none());
        register_root(h, SLOT, PID, 3).unwrap();
        cell(h)
            .unwrap()
            .with(|s| s.handle = Some(ProcessHandle::new(h.index(), h.generation() + 1)));
        assert!(ExecMembership::begin(&intent).is_none());
        CELLS[h.index()].with(|s| s.handle = Some(h));
        let before = cell(h).unwrap().with(|s| {
            let n = s.nonce;
            s.nonce = u64::MAX;
            n
        });
        assert!(ExecMembership::begin(&intent).is_none());
        cell(h).unwrap().with(|s| {
            assert_eq!(s.phase, 0);
            s.nonce = before;
        });
        let census = ExecMembership::begin(&intent).unwrap();
        census.finish_for_test();
        cancel(intent);
    });
}
#[test]
fn before_image_drop_releases_only_exact_census_and_old_epoch_cannot_spend_next_owner() {
    fixture(|h| {
        let intent = intent(h);
        let first = ExecMembership::begin(&intent).unwrap();
        let nonce = first.nonce;
        drop(first);
        let next = ExecMembership::begin(&intent).unwrap();
        assert!(next.nonce > nonce);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let stale = ExecMembership {
                    handle: h,
                    reservation: intent.reservation(),
                    target: (SLOT, PID),
                    key: intent.credential(),
                    nonce,
                    members: [(0, 0); MAX],
                    len: 0,
                    staged: core::cell::Cell::new(false),
                    live: false,
                    _process: process_table::pin_owner::pin(h).unwrap(),
                };
                stale.release(false);
            }))
            .is_err()
        );
        next.assert_matches(&intent);
        next.finish_for_test();
        cancel(intent);
    });
}
#[test]
fn staged_abandonment_retains_process_census_quarantine_instead_of_reopening_creators() {
    fixture(|h| {
        let intent = intent(h);
        let census = ExecMembership::begin(&intent).unwrap();
        census.mark_staged();
        drop(census);
        assert_eq!(cell(h).unwrap().with(|s| s.phase), 3);
        assert!(register_root(h, CHILD, PID + 1, 3).is_none());
        cancel(intent);
        assert!(Join::begin(h, (SLOT, PID), UserAbi::Linux).is_none());
        // Native recovery only; production abandonment retains occupancy.
        cell(h).unwrap().with(|s| s.phase = 0);
    });
}
#[test]
fn census_retains_actual_process_object_pin_until_release_outside_leaf_and_cancel_keeps_masks() {
    fixture(|h| {
        let intent = intent(h);
        let census = ExecMembership::begin(&intent).unwrap();
        assert_eq!(census._process.handle(), h);
        let before = mask(h);
        drop(census);
        cancel(intent);
        assert_eq!(mask(h), before);
    });
}

#[test]
fn terminal_turn_excludes_new_exec_census_but_frozen_exec_allows_exact_sibling_stop_without_reopening_creators()
 {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, PID), UserAbi::Linux).unwrap();
        join.register(CHILD, PID + 1).unwrap();
        drop(join);
        identity::publish_metadata(CHILD, Some(metadata(h, PID + 1)));
        let intent = intent(h);
        let turn = super::super::terminal::Turn::begin(h, (SLOT, PID)).unwrap();
        assert!(ExecMembership::begin(&intent).is_none());
        drop(turn);
        let census = ExecMembership::begin(&intent).unwrap();
        let mut sibling = super::super::terminal::Turn::begin(h, (CHILD, PID + 1)).unwrap();
        assert!(!sibling.last());
        identity::publish_lifecycle_change(
            CHILD,
            identity::read(CHILD),
            identity::LifecycleGate::Retired,
            true,
        );
        sibling.mark_stopped();
        let publication =
            crate::multitask::scheduler::retirement_cleanup::producer::publish_process_turn_fixture(
                CHILD,
                PID + 1,
                sibling.credential(),
            );
        sibling.finish(publication);
        unregister(h, CHILD, PID + 1);
        census.assert_matches(&intent);
        assert!(census.members().contains(&(CHILD, PID + 1)));
        assert!(Join::begin(h, (SLOT, PID), UserAbi::Linux).is_none());
        drop(census);
        cancel(intent);
    });
}
#[test]
fn committed_terminal_target_refuses_exec_census_independently_of_live_metadata_intent_and_open_placement_phase()
 {
    fixture(|h| {
        let intent = intent(h);
        cell(h).unwrap().with(|s| {
            let index = s
                .members
                .iter()
                .position(|&m| m == Some((SLOT, PID)))
                .unwrap();
            s.stopped[index] = true;
        });
        assert!(ExecMembership::begin(&intent).is_none());
        cell(h).unwrap().with(|s| s.stopped.fill(false));
        cancel(intent);
    });
}

#[test]
fn group_membership_freeze_blocks_new_exec_but_pending_exit_does_not_erase_existing_census_epoch_or_reopen_join()
 {
    fixture(|h| {
        let intent = intent(h);
        let exit = super::super::exit_membership::ExitMembership::begin(h, (SLOT, PID)).unwrap();
        assert!(ExecMembership::begin(&intent).is_none());
        drop(exit);
        let census = ExecMembership::begin(&intent).unwrap();
        let exit = super::super::exit_membership::ExitMembership::begin(h, (SLOT, PID)).unwrap();
        assert!(process_table::mark_process_exiting(PID).is_none());
        drop(exit);
        census.assert_matches(&intent);
        assert!(Join::begin(h, (SLOT, PID), UserAbi::Linux).is_none());
        drop(census);
        let result = intent.cancel_with(
            nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
            process_table::cancel_exec_owned,
        );
        assert!(result.2);
    });
}
