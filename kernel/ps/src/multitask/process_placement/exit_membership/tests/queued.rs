//! Owner: durable exit request custody and exact completion witnesses.
//! Boundary: real Class55 queue/claim plus process mark, not physical switch ACK.
//! Lifecycle: creator prefix -> drain -> stop ACK; abandoned loan keeps poisoned backing.
//! Concurrency: parent's actual table/runtime fixture; no caller-owned pin survives wait.
//! Failure: exec/terminal/foreign generation independently refuse effect admission.
//! Forbidden: row absence/reuse/poison cannot manufacture completion.
//! Evidence: actual PendingExit::publish, claim_next, receipt and terminal methods.
use super::*;

fn reason() -> Reason {
    Reason::Terminated {
        requested_by_pid: Some(ID + 90),
    }
}
fn submit(h: ProcessHandle) -> Receipt {
    PendingExit::begin(h, (SLOT, ID))
        .unwrap()
        .publish(ID, reason())
}
fn finish(mut census: ExitMembership) {
    census.begin_effects();
    for &(slot, _) in census.members() {
        stop(slot);
    }
    census.finish();
}
#[test]
fn explicit_suspension_retains_original_census_nonce_effects_and_pin_without_reopening_join() {
    fixture(|h| {
        let receipt = submit(h);
        let (census, _) = claim_next().unwrap();
        let nonce = census.nonce;
        let original_members = census.members().to_vec();
        assert!(!census.effects_started());
        census.suspend();
        assert!(!receipt.complete());
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
        assert!(PendingExit::begin(h, (SLOT, ID)).is_none());
        let (mut resumed, _) = claim_next().unwrap();
        assert_eq!(resumed.nonce, nonce);
        assert_eq!(resumed.members(), original_members);
        assert!(!resumed.effects_started());
        resumed.begin_effects();
        resumed.suspend();
        let retained_pin = cell(h).unwrap().with(|s| {
            let work = s.exit_work.as_ref().unwrap();
            assert_eq!(work.process.handle(), h);
            assert!(work.captured.as_ref().unwrap().effects);
            s.exit.unwrap().phase == Phase::Suspended
        });
        assert!(retained_pin);
        assert_eq!(process_table::reap_exited_processes(), 0);
        let (resumed, _) = claim_next().unwrap();
        assert_eq!(resumed.nonce, nonce);
        assert_eq!(resumed.members(), original_members);
        assert!(resumed.effects_started());
        stop(SLOT);
        resumed.finish();
        assert!(receipt.complete());
    });
}

#[test]
fn suspended_effects_retain_captured_detached_uid_instead_of_recollecting_recycled_live_member() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        join.register(PEER, ID + 1).unwrap();
        identity::publish_metadata(PEER, Some(meta(h, ID + 1)));
        drop(join);
        let receipt = submit(h);
        let (mut census, _) = claim_next().unwrap();
        census.begin_effects();
        detached_peer(h);
        identity::clear(PEER);
        identity::publish_metadata(PEER, Some(meta(h, ID + 2)));
        census.suspend();
        let (resumed, _) = claim_next().unwrap();
        assert!(resumed.effects_started());
        assert!(resumed.members().contains(&(PEER, ID + 1)));
        assert!(!resumed.members().contains(&(PEER, ID + 2)));
        assert!(resumed.member_stopped((PEER, ID + 1)));
        assert!(!identity::lifecycle(PEER).retired);
        stop(SLOT);
        resumed.finish();
        assert!(receipt.complete());
        assert!(!identity::lifecycle(PEER).retired);
    });
}

#[test]
fn queued_exit_waits_for_real_join_ack_not_visible_membership_prefix() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        let receipt = submit(h);
        assert!(!receipt.complete());
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
        assert!(claim_next().is_none());
        join.register(PEER, ID + 1).unwrap();
        identity::publish_metadata(PEER, Some(meta(h, ID + 1)));
        assert!(claim_next().is_none());
        drop(join);
        let (census, captured_reason) = claim_next().unwrap();
        assert_eq!(captured_reason, reason());
        assert!(census.members().contains(&(PEER, ID + 1)));
        assert!(claim_next().is_none());
        assert!(!receipt.complete());
        finish(census);
        assert!(receipt.complete());
    });
}
#[test]
fn durable_request_outlives_requester_binding_detach_and_reuse_without_spending_replacement() {
    fixture(|h| {
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        join.register(PEER, ID + 1).unwrap();
        identity::publish_metadata(PEER, Some(meta(h, ID + 1)));
        drop(join);
        let receipt = PendingExit::begin(h, (PEER, ID + 1)).unwrap().publish(
            ID,
            Reason::Exited {
                slot: PEER,
                task_id: ID + 1,
                process_id: ID,
                logical_cpu: 2,
            },
        );
        detached_peer(h);
        identity::clear(PEER);
        identity::publish_metadata(PEER, Some(meta(h, ID + 2)));
        assert!(!receipt.complete());
        let (mut census, captured_reason) = claim_next().unwrap();
        assert_eq!(
            captured_reason,
            Reason::Exited {
                slot: PEER,
                task_id: ID + 1,
                process_id: ID,
                logical_cpu: 2
            }
        );
        assert!(census.member_stopped((PEER, ID + 1)));
        assert!(!census.member_stopped((PEER, ID + 2)));
        census.begin_effects();
        stop(SLOT);
        census.finish();
        assert!(receipt.complete());
        assert_eq!(identity::read(PEER).unwrap().task_id, Some(ID + 2));
    });
}
#[test]
fn duplicate_receipt_keeps_original_reason_and_only_exact_effect_ack_completes() {
    fixture(|h| {
        assert_eq!(target_by_pid(ID), Some((h, (SLOT, ID))));
        assert!(target_by_pid(ID + 5).is_none());
        let receipt = submit(h);
        let duplicate = queued_for(h).unwrap();
        assert_eq!(duplicate.nonce, receipt.nonce);
        assert!(PendingExit::begin(h, (SLOT, ID)).is_none());
        let (census, r) = claim_next().unwrap();
        assert_eq!(r, reason());
        assert!(!duplicate.complete());
        finish(census);
        assert!(receipt.complete() && duplicate.complete());
        assert!(queued_for(h).is_none());
    });
}
#[test]
fn terminal_turn_drains_before_claim_and_seal_publication_does_not_steal_its_ack() {
    fixture(|h| {
        let terminal = super::super::super::terminal::Turn::begin(h, (SLOT, ID)).unwrap();
        let receipt = submit(h);
        assert!(claim_next().is_none());
        assert!(!receipt.complete());
        drop(terminal); // untouched exact terminal rollback
        let (census, _) = claim_next().unwrap();
        finish(census);
        assert!(receipt.complete());
    });
}
#[test]
fn exec_owner_and_process_exec_refusal_preserve_durable_work_until_actual_cancel() {
    fixture(|h| {
        let reservation = process_table::begin_exec(h).unwrap();
        let receipt = submit(h);
        assert!(claim_next().is_none());
        assert!(!receipt.complete());
        assert!(cell(h).unwrap().with(|s| s.exit_work.is_some()));
        process_table::cancel_exec(reservation);
        let (census, _) = claim_next().unwrap();
        finish(census);
        assert!(receipt.complete());
    });
}

#[test]
fn actual_exec_census_release_precedes_cancel_and_durable_exit_claim_after_pending_exit() {
    fixture(|h| {
        let reservation = process_table::begin_exec(h).unwrap();
        let intent = identity::exec_intent::begin(SLOT, ID, reservation, false).unwrap();
        let exec = super::super::super::exec_membership::ExecMembership::begin(&intent).unwrap();
        // Self exit retains exit_pending before publishing while exec owns the row.
        assert!(process_table::mark_process_exiting_for_handle(h, ID).is_none());
        let receipt = submit(h);
        assert!(claim_next().is_none());
        assert!(!receipt.complete());
        // This is the production failure-path ordering: release Class55 before
        // cancellation can invoke its synchronous pending-exit continuation.
        drop(exec);
        let (_, pid, terminal) = intent.cancel_with(
            nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
            process_table::cancel_exec_owned,
        );
        assert_eq!(pid, ID);
        assert!(terminal);
        let (census, _) = claim_next().unwrap();
        finish(census);
        assert!(receipt.complete());
    });
}
#[test]
fn placement_busy_or_poison_never_grants_exit_claim_from_process_marker_alone() {
    fixture(|h| {
        let receipt = submit(h);
        for phase in [1, 2, 3, 4] {
            cell(h).unwrap().with(|s| s.phase = phase);
            assert!(claim_next().is_none());
            assert!(!receipt.complete());
            assert!(cell(h).unwrap().with(|s| s.exit_work.is_some()));
        }
        cell(h).unwrap().with(|s| s.phase = 0);
        let (census, _) = claim_next().unwrap();
        finish(census);
        assert!(receipt.complete());
    });
}
#[test]
fn abandoned_queued_claim_even_before_effects_returns_process_pin_to_poison_custody() {
    fixture(|h| {
        let receipt = submit(h);
        let (census, _) = claim_next().unwrap();
        assert!(cell(h).unwrap().with(|s| s.exit_work.is_none()));
        drop(census);
        assert!(cell(h).unwrap().with(|s| {
            s.exit_work
                .as_ref()
                .is_some_and(|w| w.process.handle() == h && w.nonce == receipt.nonce)
                && s.exit.unwrap().phase == Phase::Poisoned
        }));
        assert!(!receipt.complete());
        assert!(queued_for(h).is_none());
        assert!(claim_next().is_none());
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
    });
}
#[test]
fn abandoned_queued_effects_preserve_work_pin_and_never_ack_stopped_metadata() {
    fixture(|h| {
        let receipt = submit(h);
        let (mut census, _) = claim_next().unwrap();
        census.begin_effects();
        stop(SLOT);
        drop(census);
        assert!(
            cell(h)
                .unwrap()
                .with(|s| s.exit_work.is_some() && s.exit.unwrap().phase == Phase::Poisoned)
        );
        assert!(!receipt.complete());
        assert!(claim_next().is_none());
    });
}
#[test]
fn completion_watermark_survives_row_release_but_absence_and_next_unfinished_nonce_do_not_ack() {
    fixture(|h| {
        let receipt = submit(h);
        assert!(!receipt.complete());
        let future = Receipt {
            handle: h,
            nonce: receipt.nonce + 1,
        };
        let (census, _) = claim_next().unwrap();
        finish(census);
        assert!(receipt.complete());
        assert!(!future.complete());
        unregister(h, SLOT, ID);
        assert!(cell(h).unwrap().with(|s| s.handle.is_none()));
        assert!(receipt.complete());
        assert!(!future.complete());
        reset_for_tests();
        assert!(receipt.complete());
        assert!(!future.complete());
    });
}
#[test]
fn exact_process_exit_marker_refuses_foreign_pid_without_setting_exit_on_original_object() {
    fixture(|h| {
        assert_eq!(process_table::is_process_exiting(ID), Some(false));
        let foreign = ProcessHandle::new(h.index(), h.generation() + 1);
        assert!(process_table::mark_process_exiting_for_handle(foreign, ID).is_none());
        assert_eq!(process_table::is_process_exiting(ID), Some(false));
        assert!(process_table::mark_process_exiting_for_handle(h, ID + 1).is_none());
        assert_eq!(process_table::is_process_exiting(ID), Some(false));
        assert_eq!(
            process_table::mark_process_exiting_for_handle(h, ID),
            Some(())
        );
        assert_eq!(process_table::is_process_exiting(ID), Some(true));
    });
}
