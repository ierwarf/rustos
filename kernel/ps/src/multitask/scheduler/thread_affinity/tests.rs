//! Owner: actual single-target affinity Grant and retained caller/target binding witnesses.
//! Boundary: native Ticket/MM/CPU phase fixtures; no HAL topology, IPI, CR3 or RSP proof.
//! Lifecycle: exact physical pins survive masks, queue effects and terminal overlap through ACK.
//! Concurrency: canonical scheduler fixture; pending cohort excludes competing source/group CAS.
//! Failure: invalid masks and physical UID/ABI/process mismatch release untouched captures.
//! Forbidden: catalog authority, copied process identity, blocked wake or retired reanimation.
//! Evidence: production Grant methods with real bindings, queue states and process membership.
use super::*;
use crate::multitask::process_placement as placement;
use crate::multitask::scheduler::{self, process_affinity::tests::affinity_fixture};
const ROOT: usize = 124;
const PEER: usize = 125;
const ID: u64 = 1245501;

fn fixture(abi: UserAbi, f: impl FnOnce(ProcessHandle)) {
    affinity_fixture(abi, abi, false, f);
}
fn capture(handle: ProcessHandle, target: u64, abi: UserAbi) -> Grant {
    Grant::capture((ROOT, ID, handle, abi), target, abi).unwrap()
}

#[test]
pub(in crate::multitask::scheduler) fn linux_query_and_peer_set_preserve_process_mask_and_other_thread() {
    fixture(UserAbi::Linux, |h| {
        let query = capture(h, ID + 1, UserAbi::Linux);
        assert_eq!(query.mask(3), Ok(3));
        drop(query);
        let result = capture(h, ID + 1, UserAbi::Linux)
            .finish_with(2, 7, |_, _, _| panic!("blocked query target woken"))
            .unwrap();
        assert_eq!(result.previous_mask, 7);
        assert!(!result.reschedule_required);
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(ROOT),
            (7, 7, false)
        );
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(PEER),
            (2, 7, false)
        );
        assert_eq!(placement::mask(h), Some(7));
        assert_eq!(capture(h, ID + 1, UserAbi::Linux).mask(7), Ok(2));
    });
}

#[test]
pub(in crate::multitask::scheduler) fn windows_self_alias_owns_two_physical_refs_and_changes_only_thread_mask() {
    fixture(UserAbi::Windows, |h| {
        let grant = capture(h, 0, UserAbi::Windows);
        assert_eq!((grant.slot, grant.id), (ROOT, ID));
        assert_eq!(identity::execution::count(ROOT), 2);
        assert_eq!(identity::execution::count(PEER), 0);
        let result = grant
            .finish_with(4, 7, |_, _, _| panic!("blocked self woken"))
            .unwrap();
        assert_eq!(result.previous_mask, 7);
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(ROOT),
            (4, 7, false)
        );
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(PEER),
            (7, 7, false)
        );
        assert_eq!(placement::mask(h), Some(7));
        assert_eq!(identity::execution::count(ROOT), 0);
    });
}

#[test]
pub(in crate::multitask::scheduler) fn caller_and_target_pins_exclude_reuse_and_target_admission_until_untouched_release() {
    fixture(UserAbi::Linux, |h| {
        let grant = capture(h, ID + 1, UserAbi::Linux);
        for (slot, id) in [(ROOT, ID), (PEER, ID + 1)] {
            assert_eq!(identity::execution::count(slot), 1);
            assert!(identity::with_reclaim_binding(slot, Some(id), || ()).is_none());
            assert!(!stack_resources::close_for_reclaim(slot));
        }
        assert!(!identity::cohort::busy(ROOT));
        assert!(identity::cohort::busy(PEER));
        assert!(identity::execution::with_claim(PEER, |_| panic!("target admitted")).is_none());
        assert!(identity::cohort::Cohort::begin(&[(ROOT, ID), (PEER, ID + 1)]).is_none());
        assert_eq!(placement::mask(h), Some(7));
        drop(grant);
        for slot in [ROOT, PEER] {
            assert_eq!(identity::execution::count(slot), 0);
            assert!(!identity::cohort::busy(slot));
            assert_eq!(
                runqueue::affinity_payload::affinity_snapshot(slot),
                (7, 7, false)
            );
        }
    });
}

#[test]
pub(in crate::multitask::scheduler) fn invalid_online_or_empty_mask_refuses_before_any_effect() {
    fixture(UserAbi::Linux, |h| {
        for (mask, online) in [(0, 7), (8, 7), (4, 3), (1, 0)] {
            assert!(matches!(
                capture(h, ID + 1, UserAbi::Linux)
                    .finish_with(mask, online, |_, _, _| panic!("invalid mask effect")),
                Err(AffinityError::InvalidMask)
            ));
            assert!(!identity::cohort::busy(PEER));
            assert_eq!(identity::execution::count(ROOT), 0);
            assert_eq!(identity::execution::count(PEER), 0);
        }
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(PEER),
            (7, 7, false)
        );
        assert_eq!(placement::mask(h), Some(7));
        assert_eq!(
            capture(h, ID + 1, UserAbi::Linux).mask(0),
            Err(AffinityError::InvalidMask)
        );
    });
}

#[test]
pub(in crate::multitask::scheduler) fn process_mask_escape_refuses_after_actual_complete_group_change_without_effects() {
    fixture(UserAbi::Windows, |h| {
        scheduler::process_affinity::tests::change_blocked_process_mask(h, 3);
        assert!(matches!(
            capture(h, 0, UserAbi::Windows)
                .finish_with(4, 7, |_, _, _| panic!("process escape effect")),
            Err(AffinityError::InvalidMask)
        ));
        for slot in [ROOT, PEER] {
            assert_eq!(
                runqueue::affinity_payload::affinity_snapshot(slot),
                (3, 3, false)
            );
            assert_eq!(identity::execution::count(slot), 0);
            assert!(!identity::cohort::busy(slot));
        }
        assert_eq!(placement::mask(h), Some(3));
        assert_eq!(
            capture(h, 0, UserAbi::Windows).mask(4),
            Err(AffinityError::InvalidMask)
        );
    });
}

#[test]
pub(in crate::multitask::scheduler) fn foreign_physical_process_refuses_after_pin_without_spending_either_binding() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, true, |h| {
        assert!(matches!(
            Grant::capture((ROOT, ID, h, UserAbi::Linux), ID + 1, UserAbi::Linux),
            Err(AffinityError::PermissionDenied)
        ));
        for slot in [ROOT, PEER] {
            assert_eq!(identity::execution::count(slot), 0);
            assert!(!identity::cohort::busy(slot));
            assert_eq!(
                runqueue::affinity_payload::affinity_snapshot(slot),
                (7, 7, false)
            );
        }
    });
}

#[test]
pub(in crate::multitask::scheduler) fn physical_peer_abi_and_caller_abi_mismatch_cannot_spend_target_masks() {
    affinity_fixture(UserAbi::Linux, UserAbi::Windows, false, |h| {
        assert!(matches!(
            Grant::capture((ROOT, ID, h, UserAbi::Linux), ID + 1, UserAbi::Linux),
            Err(AffinityError::WrongAbi)
        ));
        assert!(matches!(
            Grant::capture((ROOT, ID, h, UserAbi::Linux), 0, UserAbi::Windows),
            Err(AffinityError::WrongAbi)
        ));
        // A copied Windows tuple cannot override the physical Linux caller.
        assert!(matches!(
            Grant::capture((ROOT, ID, h, UserAbi::Windows), 0, UserAbi::Windows),
            Err(AffinityError::MissingTask)
        ));
        assert_eq!(identity::execution::count(ROOT), 0);
        assert_eq!(identity::execution::count(PEER), 0);
        assert!(!identity::cohort::busy(PEER));
    });
}

#[test]
pub(in crate::multitask::scheduler) fn wrong_uid_missing_target_and_retired_caller_or_target_refuse_exact_physical_capture() {
    fixture(UserAbi::Linux, |h| {
        assert!(matches!(
            Grant::capture((ROOT, ID + 9, h, UserAbi::Linux), 0, UserAbi::Linux),
            Err(AffinityError::MissingTask)
        ));
        assert!(matches!(
            Grant::capture((ROOT, ID, h, UserAbi::Linux), ID + 9, UserAbi::Linux),
            Err(AffinityError::MissingTask)
        ));
        identity::update_live_lifecycle(PEER, ID + 1, identity::LifecycleGate::Retired, true);
        assert!(matches!(
            Grant::capture((ROOT, ID, h, UserAbi::Linux), ID + 1, UserAbi::Linux),
            Err(AffinityError::MissingTask)
        ));
        identity::update_live_lifecycle(ROOT, ID, identity::LifecycleGate::Retired, true);
        assert!(matches!(
            Grant::capture((ROOT, ID, h, UserAbi::Linux), 0, UserAbi::Linux),
            Err(AffinityError::MissingTask)
        ));
        assert_eq!(identity::execution::count(ROOT), 0);
        assert_eq!(identity::execution::count(PEER), 0);
    });
}

#[test]
pub(in crate::multitask::scheduler) fn queued_transfer_borrows_exact_target_cohort_until_real_queue_ack_without_process_publication() {
    fixture(UserAbi::Linux, |h| {
        runqueue::set_runnable(PEER, true);
        runqueue::publish_local_wake(PEER, 0, 100);
        let result = capture(h, ID + 1, UserAbi::Linux)
            .finish_with(4, 7, |slot, id, cohort| {
                assert_eq!((slot, id), (PEER, ID + 1));
                assert!(
                    identity::execution::with_claim(slot, |_| panic!(
                        "queue ACK released admission"
                    ))
                    .is_none()
                );
                assert_eq!(identity::execution::count(ROOT), 1);
                assert_eq!(identity::execution::count(PEER), 1);
                assert_eq!(placement::mask(h), Some(7));
                assert_eq!(
                    runqueue::affinity_payload::affinity_snapshot(slot),
                    (4, 7, true)
                );
                assert!(identity::cohort::Cohort::begin(&[(ROOT, ID), (PEER, ID + 1)]).is_none());
                let before = runqueue::owner(slot);
                assert_eq!(
                    runqueue::rehome_queued(slot, 2, 100),
                    runqueue::RemoteWakeOutcome::Rejected
                );
                assert_eq!(runqueue::owner(slot), before);
                assert_eq!(
                    scheduler::runqueue_policy::rehome_owned_on_mask(slot, 4, 100, cohort, id),
                    Some(2)
                );
                assert_eq!(runqueue::owner(slot).cpu, Some(2));
                assert!(identity::cohort::busy(slot));
            })
            .unwrap();
        assert!(result.reschedule_required);
        assert_eq!(placement::mask(h), Some(7));
        assert!(!identity::cohort::busy(PEER));
        assert_eq!(identity::execution::count(ROOT), 0);
        let before = runqueue::owner(PEER);
        assert_eq!(
            runqueue::rehome_queued(PEER, 0, 100),
            runqueue::RemoteWakeOutcome::Rejected
        );
        assert_eq!(runqueue::owner(PEER), before);
    });
}

#[test]
pub(in crate::multitask::scheduler) fn actual_running_target_keeps_current_custody_and_notifies_owner_cpu() {
    fixture(UserAbi::Linux, |h| {
        crate::multitask::cpu_local::with_test_activation_return(0, ROOT, PEER, |phase| {
            if phase != 0 {
                return;
            }
            let (_, current) = identity::execution::with_claim(ROOT, |_| ()).unwrap();
            identity::execution::banks::initialize(0, current.unwrap());
            runqueue::set_runnable(PEER, true);
            runqueue::publish_local_wake(PEER, 0, 100);
            assert!(runqueue::claim_dispatch(PEER, 0, 100));
            let before = runqueue::owner(PEER);
            let result = capture(h, ID + 1, UserAbi::Linux)
                .finish_with(4, 7, |slot, id, cohort| {
                    assert_eq!(
                        scheduler::runqueue_policy::rehome_owned_on_mask(slot, 4, 100, cohort, id),
                        Some(0)
                    );
                    assert_eq!(runqueue::owner(slot), before);
                })
                .unwrap();
            assert!(result.reschedule_required);
            assert_eq!(runqueue::owner(PEER), before);
            assert_eq!(
                runqueue::affinity_payload::affinity_snapshot(PEER),
                (4, 7, true)
            );
            assert_eq!(placement::mask(h), Some(7));
        });
    });
}

#[test]
pub(in crate::multitask::scheduler) fn block_during_owned_thread_queue_effect_is_not_an_affinity_wake() {
    fixture(UserAbi::Linux, |h| {
        runqueue::set_runnable(PEER, true);
        runqueue::publish_local_wake(PEER, 0, 100);
        capture(h, ID + 1, UserAbi::Linux)
            .finish_with(4, 7, |slot, id, cohort| {
                runqueue::publish_blocked(slot, 0, 100);
                let blocked = runqueue::owner(slot);
                assert_eq!(
                    scheduler::runqueue_policy::rehome_owned_on_mask(slot, 4, 100, cohort, id),
                    None
                );
                assert_eq!(runqueue::owner(slot), blocked);
            })
            .unwrap();
        assert_eq!(
            runqueue::owner(PEER).state,
            runqueue::RunOwnerState::Blocked
        );
        assert_eq!(placement::mask(h), Some(7));
    });
}

#[test]
pub(in crate::multitask::scheduler) fn terminal_overlap_remains_retired_and_pinned_through_thread_affinity_ack() {
    fixture(UserAbi::Linux, |h| {
        use scheduler::retirement_cleanup::producer::{
            publish_process_turn_fixture, reset_process_turn_fixture,
        };
        runqueue::set_runnable(PEER, true);
        runqueue::publish_local_wake(PEER, 0, 100);
        capture(h, ID + 1, UserAbi::Linux)
            .finish_with(4, 7, |slot, id, cohort| {
                let mut turn = placement::terminal::Turn::begin(h, (slot, id)).unwrap();
                identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true);
                runqueue::retire(slot, 100);
                turn.mark_stopped();
                let receipt = publish_process_turn_fixture(slot, id, turn.credential());
                turn.finish(receipt);
                assert!(identity::with_reclaim_binding(slot, Some(id), || ()).is_none());
                assert!(stack_resources::reuse::begin_retired(slot, id).is_none());
                assert_eq!(
                    scheduler::runqueue_policy::rehome_owned_on_mask(slot, 4, 100, cohort, id),
                    None
                );
            })
            .unwrap();
        assert_eq!(
            runqueue::owner(PEER).state,
            runqueue::RunOwnerState::Retired
        );
        assert!(identity::terminal_retired_matches(PEER, ID + 1));
        assert!(identity::execution::with_claim(PEER, |_| panic!("retired admitted")).is_none());
        assert_eq!(placement::mask(h), Some(7));
        reset_process_turn_fixture(PEER);
    });
}
