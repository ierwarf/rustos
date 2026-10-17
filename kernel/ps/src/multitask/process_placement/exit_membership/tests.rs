//! Owner: actual group membership freeze/pin/identity witnesses.
//! Boundary: desired withdrawal ACK, not terminal consumer or MM/RSP completion.
//! Lifecycle: untouched rollback versus effects poison, exact captured UIDs survive removal.
//! Concurrency: existing table/CPU/runtime/queue fixture order; default parallel tests unchanged.
//! Failure: independent join/UID/handle/nonce admission and missing binding ACK refuse.
//! Forbidden: predicate injection/poison recovery here never permits production reopen.
//! Evidence: real ExitMembership/Join/identity and process-object pin methods.
use super::*;
use crate::multitask::{current_identity as identity, current_identity::TaskIdentity, scheduler};
const SLOT: usize = 116;
const PEER: usize = 117;
const ID: u64 = 116001;
mod queued;
fn meta(h: ProcessHandle, id: u64) -> TaskIdentity {
    TaskIdentity {
        task_id: Some(id),
        user_mode: true,
        abi: Some(UserAbi::Linux),
        process_handle: Some(h),
        process_id: Some(ID),
        ..TaskIdentity::default()
    }
}
fn fixture(f: impl FnOnce(ProcessHandle)) {
    scheduler::with_exec_census_fixture(ID, |h| {
        reset_for_tests();
        identity::publish_metadata(SLOT, Some(meta(h, ID)));
        register_root(h, SLOT, ID, 3).unwrap();
        f(h);
        // Native-only cleanup of asserted effect poison, after owner Drop.
        let abandoned_work = cell(h).unwrap().with(|s| {
            s.exit = None;
            s.phase = 0;
            s.exit_work.take()
        });
        // Never drop a physical process reference inside Class55.
        drop(abandoned_work);
        unregister(h, SLOT, ID);
        unregister(h, PEER, ID + 1);
        identity::clear(SLOT);
        identity::clear(PEER);
        reset_for_tests();
    });
}
fn stop(slot: usize) {
    identity::publish_lifecycle_change(
        slot,
        identity::read(slot),
        identity::LifecycleGate::Retired,
        true,
    );
}
#[test]
fn group_freeze_denies_duplicate_root_join_and_mask_before_effects_and_reopens_only_exact_untouched_owner()
 {
    fixture(|h| {
        let owner = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        assert_eq!(owner.members(), &[(SLOT, ID)]);
        assert!(ExitMembership::begin(h, (SLOT, ID)).is_none());
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
        assert!(Change::begin(h, (SLOT, ID)).is_none());
        assert!(register_root(h, PEER, ID + 1, 3).is_none());
        drop(owner);
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_some());
    });
}
#[test]
fn pending_join_refuses_census_before_nonce_spend_then_registered_child_is_in_frozen_membership() {
    fixture(|h| {
        let nonce = cell(h).unwrap().with(|s| s.exit_nonce);
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        assert!(ExitMembership::begin(h, (SLOT, ID)).is_none());
        assert_eq!(cell(h).unwrap().with(|s| s.exit_nonce), nonce);
        join.register(PEER, ID + 1).unwrap();
        assert!(ExitMembership::begin(h, (SLOT, ID)).is_none());
        drop(join);
        let owner = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        assert!(owner.members().contains(&(PEER, ID + 1)));
    });
}
#[test]
fn wrong_target_foreign_ledger_generation_and_exhausted_nonce_refuse_without_partial_membership_gate()
 {
    fixture(|h| {
        assert!(ExitMembership::begin(h, (SLOT, ID + 1)).is_none());
        assert!(ExitMembership::begin(h, (PEER, ID + 1)).is_none());
        let foreign = ProcessHandle::new(h.index(), h.generation() + 1);
        assert!(ExitMembership::begin(foreign, (SLOT, ID)).is_none());
        cell(h).unwrap().with(|s| s.handle = Some(foreign));
        assert!(ExitMembership::begin(h, (SLOT, ID)).is_none());
        cell(h).unwrap().with(|s| s.handle = Some(h));
        let before = cell(h).unwrap().with(|s| s.exit_nonce);
        cell(h).unwrap().with(|s| s.exit_nonce = u64::MAX);
        assert!(ExitMembership::begin(h, (SLOT, ID)).is_none());
        assert!(cell(h).unwrap().with(|s| s.exit.is_none()));
        cell(h).unwrap().with(|s| s.exit_nonce = before);
    });
}
#[test]
fn exact_retired_uid_not_hidden_metadata_absence_or_recycled_slot_is_required_for_withdrawal_ack() {
    fixture(|h| {
        let mut owner = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        owner.begin_effects();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.finish())).is_err());
        assert!(
            cell(h)
                .unwrap()
                .with(|s| s.exit.is_some_and(|r| r.phase == Phase::Poisoned))
        );
    });
    fixture(|h| {
        let mut owner = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        owner.begin_effects();
        stop(SLOT);
        identity::publish_metadata(SLOT, None);
        owner.finish();
        assert!(cell(h).unwrap().with(|s| s.exit.is_none()));
    });
    fixture(|h| {
        let mut owner = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        owner.begin_effects();
        identity::clear(SLOT);
        identity::publish_metadata(SLOT, Some(meta(h, ID + 1)));
        stop(SLOT);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.finish())).is_err());
    });
}
#[test]
fn captured_members_survive_stop_and_removal_while_effect_abandonment_never_reopens_creation() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        join.register(PEER, ID + 1).unwrap();
        drop(join);
        identity::publish_metadata(PEER, Some(meta(h, ID + 1)));
        let mut owner = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        owner.begin_effects();
        stop(PEER);
        let mut terminal = super::super::terminal::Turn::begin(h, (PEER, ID + 1)).unwrap();
        terminal.mark_stopped();
        let publication =
            crate::multitask::scheduler::retirement_cleanup::producer::publish_process_turn_fixture(
                PEER,
                ID + 1,
                terminal.credential(),
            );
        terminal.finish(publication);
        unregister(h, PEER, ID + 1);
        assert!(owner.members().contains(&(PEER, ID + 1)));
        drop(owner);
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
        assert!(ExitMembership::begin(h, (SLOT, ID)).is_none());
        assert!(
            cell(h)
                .unwrap()
                .with(|s| s.exit.unwrap().phase == Phase::Poisoned)
        );
    });
}
#[test]
fn exit_scope_does_not_spend_or_reopen_existing_exec_epoch_on_pending_exit_rollback() {
    fixture(|h| {
        let epoch = cell(h).unwrap().with(|s| {
            s.phase = 4;
            s.nonce += 1;
            s.nonce
        });
        let owner = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        drop(owner);
        assert_eq!(cell(h).unwrap().with(|s| (s.phase, s.nonce)), (4, epoch));
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
    });
}
#[test]
fn original_process_object_is_retained_until_owner_drops_outside_leaf_even_after_last_attach_detaches()
 {
    fixture(|h| {
        let owner = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        assert_eq!(owner._process.as_ref().unwrap().handle(), h);
        process_table::detach_task(h).unwrap();
        assert_eq!(process_table::reap_exited_processes(), 0);
        drop(owner);
        assert_eq!(process_table::reap_exited_processes(), 1);
    });
}
#[test]
fn stale_credential_cannot_begin_effects_for_next_same_uid_owner_and_row_record_epochs_are_independent()
 {
    fixture(|h| {
        let old = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        let nonce = old.nonce;
        drop(old);
        let mut next = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        assert!(next.nonce > nonce);
        cell(h).unwrap().with(|s| s.exit_nonce = nonce);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| next.begin_effects()))
                .is_err()
        );
        cell(h).unwrap().with(|s| {
            s.exit_nonce = next.nonce;
            s.exit.as_mut().unwrap().nonce = nonce;
        });
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| next.begin_effects()))
                .is_err()
        );
        cell(h)
            .unwrap()
            .with(|s| s.exit.as_mut().unwrap().nonce = next.nonce);
        assert!(!next.effects);
    });
}

#[test]
fn waiting_seal_closes_new_creation_but_original_join_finishes_and_late_child_is_captured_only_after_final_ack()
 {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        let pending = PendingExit::begin(h, (SLOT, ID)).unwrap();
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
        assert!(Change::begin(h, (SLOT, ID)).is_none());
        assert!(register_root(h, PEER, ID + 1, 3).is_none());
        assert!(PendingExit::begin(h, (SLOT, ID)).is_none());
        let pending = match pending.try_capture() {
            Err(p) => p,
            Ok(_) => panic!("exit captured pending constructor"),
        };
        join.register(PEER, ID + 1).unwrap();
        identity::publish_metadata(PEER, Some(meta(h, ID + 1)));
        let pending = match pending.try_capture() {
            Err(p) => p,
            Ok(_) => panic!("membership prefix is not final creator ACK"),
        };
        drop(join);
        let census = match pending.try_capture() {
            Ok(c) => c,
            Err(_) => panic!("drained creator was not captured"),
        };
        assert!(census.members().contains(&(PEER, ID + 1)));
        drop(census);
    });
}
#[test]
fn cancelled_captured_join_leaves_no_phantom_child_and_waiting_rollback_preserves_live_members() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        let pending = PendingExit::begin(h, (SLOT, ID)).unwrap();
        drop(join);
        let census = match pending.try_capture() {
            Ok(c) => c,
            Err(_) => panic!("cancelled constructor retained join"),
        };
        assert_eq!(census.members(), &[(SLOT, ID)]);
        drop(census);
        let pending = PendingExit::begin(h, (SLOT, ID)).unwrap();
        drop(pending);
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_some());
        assert!(
            cell(h)
                .unwrap()
                .with(|s| s.members.contains(&Some((SLOT, ID))))
        );
    });
}
fn detached_peer(h: ProcessHandle) {
    stop(PEER);
    let mut terminal = super::super::terminal::Turn::begin(h, (PEER, ID + 1)).unwrap();
    terminal.mark_stopped();
    let publication =
        crate::multitask::scheduler::retirement_cleanup::producer::publish_process_turn_fixture(
            PEER,
            ID + 1,
            terminal.credential(),
        );
    terminal.finish(publication);
    unregister(h, PEER, ID + 1);
}

#[test]
fn reserved_exit_committed_detach_retains_exact_negative_through_reuse_until_owned_finish() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        join.register(PEER, ID + 1).unwrap();
        identity::publish_metadata(PEER, Some(meta(h, ID + 1)));
        drop(join);
        let mut census = ExitMembership::begin(h, (SLOT, ID)).unwrap();
        detached_peer(h);
        identity::clear(PEER);
        identity::publish_metadata(PEER, Some(meta(h, ID + 2)));
        assert!(census.member_stopped((PEER, ID + 1)));
        assert!(!census.member_stopped((PEER, ID + 2)));
        census.begin_effects();
        stop(SLOT);
        census.finish();
        assert!(
            !cell(h)
                .unwrap()
                .with(|s| s.members.contains(&Some((PEER, ID + 1))))
        );
        assert_eq!(identity::read(PEER).unwrap().task_id, Some(ID + 2));
    });
}
#[test]
fn waiting_exit_retains_committed_detached_uid_through_binding_clear_and_recycled_slot_without_spending_new_task()
 {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        join.register(PEER, ID + 1).unwrap();
        identity::publish_metadata(PEER, Some(meta(h, ID + 1)));
        drop(join);
        let pending = PendingExit::begin(h, (SLOT, ID)).unwrap();
        detached_peer(h);
        identity::clear(PEER);
        identity::publish_metadata(PEER, Some(meta(h, ID + 2)));
        let mut census = match pending.try_capture() {
            Ok(c) => c,
            Err(_) => panic!("drained stopped member was not captured"),
        };
        assert!(census.member_stopped((PEER, ID + 1)));
        assert!(!census.member_stopped((PEER, ID + 2)));
        assert!(census.members().contains(&(PEER, ID + 1)));
        census.begin_effects();
        stop(SLOT);
        census.finish();
        assert!(
            !cell(h)
                .unwrap()
                .with(|s| s.members.contains(&Some((PEER, ID + 1))))
        );
        assert_eq!(identity::read(PEER).unwrap().task_id, Some(ID + 2));
    });
}
#[test]
fn waiting_rollback_prunes_only_committed_detached_tombstones_not_live_or_stopped_attached_members()
{
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        join.register(PEER, ID + 1).unwrap();
        identity::publish_metadata(PEER, Some(meta(h, ID + 1)));
        drop(join);
        let pending = PendingExit::begin(h, (SLOT, ID)).unwrap();
        detached_peer(h);
        stop(SLOT);
        let mut attached = super::super::terminal::Turn::begin(h, (SLOT, ID)).unwrap();
        attached.mark_stopped();
        let publication =
            crate::multitask::scheduler::retirement_cleanup::producer::publish_process_turn_fixture(
                SLOT,
                ID,
                attached.credential(),
            );
        attached.finish(publication);
        drop(pending);
        assert!(
            cell(h)
                .unwrap()
                .with(|s| s.members.contains(&Some((SLOT, ID)))
                    && !s.members.contains(&Some((PEER, ID + 1))))
        );
    });
}
#[test]
fn waiting_seal_rejects_foreign_target_and_nonce_exhaustion_and_stale_row_record_phase_independently()
 {
    fixture(|h| {
        assert!(PendingExit::begin(h, (SLOT, ID + 1)).is_none());
        let before = cell(h).unwrap().with(|s| s.exit_nonce);
        cell(h).unwrap().with(|s| s.exit_nonce = u64::MAX);
        assert!(PendingExit::begin(h, (SLOT, ID)).is_none());
        assert!(cell(h).unwrap().with(|s| s.exit.is_none()));
        cell(h).unwrap().with(|s| s.exit_nonce = before);
        let pending = PendingExit::begin(h, (SLOT, ID)).unwrap();
        cell(h).unwrap().with(|s| s.exit_nonce = pending.nonce - 1);
        assert!(!cell(h).unwrap().with(|s| pending.exact(s)));
        cell(h).unwrap().with(|s| {
            s.exit_nonce = pending.nonce;
            s.exit.as_mut().unwrap().nonce = pending.nonce - 1;
        });
        assert!(!cell(h).unwrap().with(|s| pending.exact(s)));
        cell(h).unwrap().with(|s| {
            s.exit.as_mut().unwrap().nonce = pending.nonce;
            s.exit.as_mut().unwrap().phase = Phase::Reserved;
        });
        assert!(!cell(h).unwrap().with(|s| pending.exact(s)));
        cell(h)
            .unwrap()
            .with(|s| s.exit.as_mut().unwrap().phase = Phase::Waiting);
        drop(pending);
    });
}
#[test]
fn captured_join_cannot_register_into_reserved_census_even_if_counter_is_forged_to_look_active() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        let pending = PendingExit::begin(h, (SLOT, ID)).unwrap();
        cell(h)
            .unwrap()
            .with(|s| s.exit.as_mut().unwrap().phase = Phase::Reserved);
        assert!(join.register(PEER, ID + 1).is_none());
        cell(h)
            .unwrap()
            .with(|s| s.exit.as_mut().unwrap().phase = Phase::Waiting);
        drop(join);
        drop(pending);
    });
}
