//! Owner: actual terminal membership turn/producer-publication witnesses.
//! Boundary: stopped-member election, exact process pin and creation exclusion, not physical MM/RSP.
//! Lifecycle: metadata stop precedes vote; detached rows retain turn until exact producer receipt.
//! Concurrency: table -> CPU -> runtime -> queue fixture isolation; no parallel selection weakening.
//! Failure: independent UID/nonce/join/lifecycle predicates and post-effect poison remain fail-closed.
//! Forbidden: native poison recovery and predicate injection never authorize production reopen.
//! Evidence: actual Turn, Join, identity commands, ProcessObjectPin and Class50 publication methods.
use super::*;
use crate::multitask::scheduler::retirement_cleanup::producer::{
    publish_process_turn_fixture, reset_process_turn_fixture,
};
use crate::multitask::{current_identity as identity, current_identity::TaskIdentity, scheduler};
const SLOT: usize = 120;
const PEER: usize = 121;
const ID: u64 = 120001;
fn metadata(h: ProcessHandle, id: u64) -> TaskIdentity {
    TaskIdentity {
        task_id: Some(id),
        user_mode: true,
        abi: Some(UserAbi::Linux),
        process_handle: Some(h),
        process_id: Some(ID),
        ..TaskIdentity::default()
    }
}
fn retired(slot: usize) -> bool {
    identity::publish_lifecycle_change(
        slot,
        identity::read(slot),
        identity::LifecycleGate::Retired,
        true,
    );
    true
}
fn fixture(f: impl FnOnce(ProcessHandle)) {
    scheduler::with_exec_census_fixture(ID, |h| {
        reset_for_tests();
        reset_process_turn_fixture(SLOT);
        reset_process_turn_fixture(PEER);
        identity::publish_metadata(SLOT, Some(metadata(h, ID)));
        register_root(h, SLOT, ID, 3).unwrap();
        f(h);
        // Native-only recovery of asserted effect poison. Pins have drained here.
        cell(h).unwrap().with(|s| s.terminal = None);
        unregister(h, SLOT, ID);
        unregister(h, PEER, ID + 1);
        identity::clear(SLOT);
        identity::clear(PEER);
        reset_for_tests();
        reset_process_turn_fixture(SLOT);
        reset_process_turn_fixture(PEER);
    });
}
fn peer(h: ProcessHandle) {
    let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
    join.register(PEER, ID + 1).unwrap();
    identity::publish_metadata(PEER, Some(metadata(h, ID + 1)));
    drop(join);
}
fn finish(mut turn: Turn) {
    let member = turn.member;
    retired(member.0);
    turn.mark_stopped();
    let receipt = publish_process_turn_fixture(member.0, member.1, turn.credential());
    turn.finish(receipt);
}
#[test]
fn terminal_turn_freezes_root_join_mask_and_competing_vote_before_effects_and_untouched_drop_releases_only_owner()
 {
    fixture(|h| {
        let turn = Turn::begin(h, (SLOT, ID)).unwrap();
        assert!(turn.last());
        assert!(turn.last_desired());
        assert!(Turn::begin(h, (SLOT, ID)).is_none());
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
        assert!(Change::begin(h, (SLOT, ID)).is_none());
        assert!(register_root(h, PEER, ID + 1, 3).is_none());
        drop(turn);
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_some());
        assert!(Turn::begin(h, (SLOT, ID)).is_some());
    });
}
#[test]
fn pending_creation_excludes_vote_without_spending_epoch_and_registered_child_is_live_before_creator_ack()
 {
    fixture(|h| {
        let before = cell(h).unwrap().with(|s| s.terminal_nonce);
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        assert!(Turn::begin(h, (SLOT, ID)).is_none());
        assert_eq!(cell(h).unwrap().with(|s| s.terminal_nonce), before);
        join.register(PEER, ID + 1).unwrap();
        assert!(Turn::begin(h, (SLOT, ID)).is_none());
        drop(join);
        let turn = Turn::begin(h, (SLOT, ID)).unwrap();
        assert!(!turn.last());
        assert!(!turn.last_desired());
    });
}
#[test]
fn committed_stop_election_has_one_last_member_while_desired_retired_peer_is_not_final_election_or_absence()
 {
    fixture(|h| {
        peer(h);
        retired(PEER);
        let turn = Turn::begin(h, (SLOT, ID)).unwrap();
        assert!(!turn.last());
        assert!(turn.last_desired());
        finish(turn);
        let next = Turn::begin(h, (PEER, ID + 1)).unwrap();
        assert!(next.last());
        assert!(next.last_desired());
        finish(next);
        assert!(Turn::begin(h, (SLOT, ID)).is_none());
        assert!(Turn::begin(h, (PEER, ID + 1)).is_none());
    });
}
#[test]
fn hidden_exact_retired_binding_counts_for_exit_authorization_but_missing_or_recycled_uid_never_counts_negative()
 {
    fixture(|h| {
        peer(h);
        retired(PEER);
        identity::publish_metadata(PEER, None);
        let turn = Turn::begin(h, (SLOT, ID)).unwrap();
        assert!(turn.last_desired());
        identity::clear(PEER);
        assert!(!turn.last_desired());
        identity::publish_metadata(PEER, Some(metadata(h, ID + 2)));
        retired(PEER);
        assert!(!turn.last_desired());
        drop(turn);
    });
}
#[test]
fn missing_target_foreign_process_generation_and_nonce_exhaustion_refuse_before_partial_creation_gate()
 {
    fixture(|h| {
        assert!(Turn::begin(h, (PEER, ID + 1)).is_none());
        assert!(Turn::begin(h, (SLOT, ID + 1)).is_none());
        let foreign = ProcessHandle::new(h.index(), h.generation() + 1);
        assert!(Turn::begin(foreign, (SLOT, ID)).is_none());
        cell(h).unwrap().with(|s| s.handle = Some(foreign));
        assert!(Turn::begin(h, (SLOT, ID)).is_none());
        cell(h).unwrap().with(|s| s.handle = Some(h));
        let before = cell(h).unwrap().with(|s| s.terminal_nonce);
        cell(h).unwrap().with(|s| s.terminal_nonce = u64::MAX);
        assert!(Turn::begin(h, (SLOT, ID)).is_none());
        assert!(cell(h).unwrap().with(|s| s.terminal.is_none()));
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_some());
        // Native-only recovery after independent exhaustion assertions.
        cell(h).unwrap().with(|s| s.terminal_nonce = before);
    });
}
#[test]
fn exact_lifecycle_stop_is_required_before_committed_vote_and_before_real_producer_publication() {
    fixture(|h| {
        let mut turn = Turn::begin(h, (SLOT, ID)).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| turn.mark_stopped())).is_err()
        );
        assert!(!cell(h).unwrap().with(|s| s.stopped.iter().any(|&b| b)));
        let key = turn.credential();
        assert!(!key.admits_publication());
        assert!(std::panic::catch_unwind(|| publish_process_turn_fixture(SLOT, ID, key)).is_err());
        assert!(!cell(h).unwrap().with(|s| s.stopped.iter().any(|&b| b)));
        drop(turn);
    });
}
#[test]
fn actual_dual_publication_receipt_not_fixture_or_copied_uid_releases_stopped_member_and_creation_gate()
 {
    fixture(|h| {
        let mut turn = Turn::begin(h, (SLOT, ID)).unwrap();
        retired(SLOT);
        turn.mark_stopped();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || turn.finish(Publication::fixture())
            ))
            .is_err()
        );
        assert!(
            cell(h)
                .unwrap()
                .with(|s| s.terminal.is_some_and(|r| r.phase == Phase::Poisoned))
        );
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
    });
}
#[test]
fn detached_stopped_member_keeps_exact_handle_epoch_and_turn_until_producer_ack_then_releases_empty_row_without_nonce_reset()
 {
    fixture(|h| {
        let mut turn = Turn::begin(h, (SLOT, ID)).unwrap();
        let nonce = turn.nonce;
        retired(SLOT);
        turn.mark_stopped();
        unregister(h, SLOT, ID);
        assert_eq!(cell(h).unwrap().with(|s| s.handle), Some(h));
        assert!(register_root(h, PEER, ID + 1, 3).is_none());
        let receipt = publish_process_turn_fixture(SLOT, ID, turn.credential());
        turn.finish(receipt);
        assert_eq!(cell(h).unwrap().with(|s| s.handle), None);
        identity::publish_metadata(PEER, Some(metadata(h, ID + 1)));
        register_root(h, PEER, ID + 1, 3).unwrap();
        let next = Turn::begin(h, (PEER, ID + 1)).unwrap();
        assert!(next.nonce > nonce);
    });
}
#[test]
fn live_peer_unregister_cannot_discard_potential_last_member_during_another_turn_but_exact_stopped_removal_can()
 {
    fixture(|h| {
        peer(h);
        let turn = Turn::begin(h, (SLOT, ID)).unwrap();
        assert!(std::panic::catch_unwind(|| unregister(h, PEER, ID + 1)).is_err());
        assert!(!turn.last());
        drop(turn);
        let peerturn = Turn::begin(h, (PEER, ID + 1)).unwrap();
        finish(peerturn);
        let turn = Turn::begin(h, (SLOT, ID)).unwrap();
        assert!(turn.last());
        unregister(h, PEER, ID + 1);
        assert!(turn.last_desired());
    });
}
#[test]
fn abandoned_stopped_turn_quarantines_even_after_member_detach_instead_of_reopening_creator_or_next_process_generation()
 {
    fixture(|h| {
        let mut turn = Turn::begin(h, (SLOT, ID)).unwrap();
        retired(SLOT);
        turn.mark_stopped();
        unregister(h, SLOT, ID);
        drop(turn);
        assert_eq!(cell(h).unwrap().with(|s| s.handle), Some(h));
        assert!(
            cell(h)
                .unwrap()
                .with(|s| s.terminal.is_some_and(|r| r.phase == Phase::Poisoned))
        );
        assert!(register_root(h, PEER, ID + 1, 3).is_none());
        assert!(Turn::begin(h, (PEER, ID + 1)).is_none());
    });
}
#[test]
fn old_reserved_turn_credential_cannot_publish_next_same_uid_turn_despite_valid_class50_owner_and_stopped_lifecycle()
 {
    fixture(|h| {
        let old = Turn::begin(h, (SLOT, ID)).unwrap();
        let key = old.credential();
        drop(old);
        let mut next = Turn::begin(h, (SLOT, ID)).unwrap();
        retired(SLOT);
        next.mark_stopped();
        assert!(next.nonce > key.nonce);
        assert!(!key.admits_publication());
        // Isolate each epoch qualifier: the other is deliberately made matching.
        cell(h)
            .unwrap()
            .with(|s| s.terminal.as_mut().unwrap().nonce = key.nonce);
        assert!(!key.admits_publication());
        cell(h).unwrap().with(|s| {
            s.terminal.as_mut().unwrap().nonce = next.nonce;
            s.terminal_nonce = key.nonce;
        });
        assert!(!key.admits_publication());
        cell(h).unwrap().with(|s| s.terminal_nonce = next.nonce);
        assert!(std::panic::catch_unwind(|| publish_process_turn_fixture(SLOT, ID, key)).is_err());
        assert!(cell(h).unwrap().with(|s| next.exact(s)));
    });
}
#[test]
fn terminal_turn_holds_real_process_object_until_exact_leaf_release_and_disjoint_process_votes_are_independent()
 {
    fixture(|h| {
        let other = process_table::create_process(
            ID + 1,
            crate::user::process_state::UserProcessState::new(
                kernel_mm::api::ProcessAddressSpace::empty_for_tests(),
                None,
                None,
                None,
                None,
                false,
                "/terminal-peer.elf",
            ),
        )
        .unwrap();
        identity::publish_metadata(PEER, Some(metadata(other, ID + 1)));
        register_root(other, PEER, ID + 1, 3).unwrap();
        let turn = Turn::begin(h, (SLOT, ID)).unwrap();
        let otherturn = Turn::begin(other, (PEER, ID + 1)).unwrap();
        assert_eq!(turn._process.handle(), h);
        assert_eq!(otherturn._process.handle(), other);
        process_table::detach_task(h).unwrap();
        process_table::detach_task(other).unwrap();
        assert_eq!(process_table::reap_exited_processes(), 0);
        drop(turn);
        assert_eq!(process_table::reap_exited_processes(), 1);
        drop(otherturn);
        assert_eq!(process_table::reap_exited_processes(), 1);
        unregister(other, PEER, ID + 1);
    });
}

#[test]
fn committed_stop_cannot_admit_creation_or_mask_replacement_even_if_other_metadata_looks_live_again()
 {
    fixture(|h| {
        let turn = Turn::begin(h, (SLOT, ID)).unwrap();
        finish(turn);
        identity::publish_lifecycle_change(
            SLOT,
            identity::read(SLOT),
            identity::LifecycleGate::Retired,
            false,
        );
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
        assert!(Change::begin(h, (SLOT, ID)).is_none());
        assert!(
            cell(h)
                .unwrap()
                .with(|s| s.terminal.is_none() && s.stopped.iter().any(|&b| b))
        );
    });
}
