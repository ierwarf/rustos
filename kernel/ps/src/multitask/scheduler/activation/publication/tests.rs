//! Owner: actual physical activation publication and original frame/callback witnesses.
//! Boundary: real root producers, Cohort/frame loans, policy queues and modeled Current/MM.
//! Lifecycle: suspended root -> callback -> complete owned activation -> terminal fixture ACK.
//! Concurrency: canonical native namespace locks; no raw owner leaf crosses callback.
//! Failure: invalid bytes, callback unwind, retired metadata and replay never gain dispatch.
//! Forbidden: catalog activation or injected external ACK as CPU/hardware completion proof.
//! Evidence: common production publication and real queue/mailbox/funding cells, not HAL return.
use super::super::super::root_creation::tests::{bootstrap as root_bootstrap, suspended_roots};
use super::*;
use core::cell::Cell;
const ID: u64 = 124001;
fn bootstrap(abi: UserAbi) -> UserTaskBootstrap {
    let mut boot = root_bootstrap(abi);
    // Native byte-only root witnesses use small numeric addresses. Actual
    // activation must exercise the unchanged real user-range validator.
    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
    boot
}
fn roots(
    ids: &[u64],
    mask: u64,
    f: impl FnOnce(&[(usize, u64, process_table::ProcessHandle)], &DispatchPreemption),
) {
    suspended_roots(ids, mask, |_| bootstrap(UserAbi::Linux), f);
}
fn capture(roots: &[(usize, u64, process_table::ProcessHandle)]) -> Activation {
    let members: Vec<_> = roots.iter().map(|&(s, id, _)| (s, id)).collect();
    Activation::capture(&members).unwrap()
}
#[test]
pub(in crate::multitask::scheduler) fn actual_activation_holds_original_frame_and_binding_through_callback_before_queue_and_ack() {
    roots(&[ID, ID + 1], 1, |roots, turn| {
        let owner = capture(roots);
        let consumed = Cell::new(false);
        assert!(owner.commit_owned(
            turn,
            |_| 1,
            || {
                assert_eq!(nucleus_core::util::lockdep::current_lock_class(), None);
                assert!(super::super::super::current_terminal::OriginalCurrent::borrow(turn).is_none(),
                    "activation retains the original exclusive interval loan across the callback");
                for &(slot, id, _) in roots {
                    assert!(identity::cohort::busy(slot));
                    assert_eq!(identity::execution::count(slot), 1);
                    assert_eq!(stack_resources::frame_readers::test_count(slot), 1);
                    assert!(identity::lifecycle(slot).suspended);
                    assert_eq!(
                        runqueue::owner(slot).state,
                        runqueue::RunOwnerState::Blocked
                    );
                    assert!(stack_resources::reuse::begin_retired(slot, id).is_none());
                    assert!(
                        std::panic::catch_unwind(|| stack_resources::context::clear(slot)).is_err()
                    );
                    assert!(
                        identity::execution::with_claim(slot, |_| panic!("callback dispatched"))
                            .is_none()
                    );
                }
                consumed.set(true);
            }
        ));
        assert!(consumed.get());
        for &(slot, _, _) in roots {
            assert!(!identity::cohort::busy(slot));
            assert_eq!(identity::execution::count(slot), 0);
            assert_eq!(stack_resources::frame_readers::test_count(slot), 0);
            assert!(!identity::lifecycle(slot).suspended);
            assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
            let token = frame_publication::local_frame_token(slot, turn.cpu()).unwrap();
            assert_eq!(token.owner_generation, runqueue::owner(slot).generation);
        }
        let policy = dispatch_policy::lock(0);
        assert_eq!(policy.atomic_activation_handoff_remaining, 2);
        assert_eq!(policy.atomic_activation_pick_hints.len(), 2);
        assert!(dispatch_policy::test_published_atomic_activation_pending(0));
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_terminal_during_callback_keeps_hidden_original_retired_member_and_wakes_only_peer() {
    roots(&[ID, ID + 1], 1, |roots, turn| {
        let (slot, id, _) = roots[0];
        let metadata = identity::read(slot).unwrap();
        assert!(capture(roots).commit_owned(
            turn,
            |_| 1,
            || {
                identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true);
                runqueue::retire(slot, runqueue::weight::value(slot));
                identity::publish_metadata(slot, None);
                assert!(stack_resources::reuse::begin_retired(slot, id).is_none());
            }
        ));
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Retired
        );
        assert!(identity::lifecycle(slot).suspended);
        assert_eq!(
            runqueue::owner(roots[1].0).state,
            runqueue::RunOwnerState::Local
        );
        assert_eq!(
            dispatch_policy::lock(0).atomic_activation_handoff_remaining,
            1
        );
        identity::publish_metadata(slot, Some(metadata));
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_callback_unwind_leaves_all_suspended_and_releases_frame_loans_for_exact_retry() {
    roots(&[ID, ID + 1], 1, |roots, turn| {
        let owner = capture(roots);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.commit_owned(
                turn,
                |_| 1,
                || panic!("native activation authority fault")
            )))
            .is_err()
        );
        for &(slot, _, _) in roots {
            assert!(identity::lifecycle(slot).suspended);
            assert!(!identity::cohort::busy(slot));
            assert_eq!(identity::execution::count(slot), 0);
            assert_eq!(
                runqueue::owner(slot).state,
                runqueue::RunOwnerState::Blocked
            );
        }
        assert_eq!(
            dispatch_policy::lock(0).atomic_activation_handoff_remaining,
            0
        );
        assert!(capture(roots).commit_owned(turn, |_| 1, || {}));
    });
}
#[test]
fn actual_invalid_suspended_frame_panics_before_consuming_authority_without_releasing_suspension() {
    suspended_roots(
        &[ID],
        1,
        |_| {
            let mut boot = bootstrap(UserAbi::Linux);
            boot.entry = x86_64::VirtAddr::new(0xffff_8000_0000_0000);
            boot
        },
        |roots, turn| {
            let consumed = Cell::new(false);
            let owner = capture(roots);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.commit_owned(
                    turn,
                    |_| 1,
                    || consumed.set(true)
                )))
                .is_err()
            );
            assert!(!consumed.get());
            assert!(identity::lifecycle(roots[0].0).suspended);
            assert_eq!(
                runqueue::owner(roots[0].0).state,
                runqueue::RunOwnerState::Blocked
            );
            assert!(!identity::cohort::busy(roots[0].0));
        },
    );
}
#[test]
pub(in crate::multitask::scheduler) fn actual_activation_exact_capacity_commits_all_first_turn_records_without_overflow_or_replay() {
    let ids: Vec<_> = (0..CAP).map(|i| ID + i as u64).collect();
    roots(&ids, 1, |roots, turn| {
        assert!(capture(roots).commit_owned(turn, |_| 1, || {}));
        let policy = dispatch_policy::lock(0);
        assert_eq!(policy.atomic_activation_handoff_remaining, CAP);
        assert_eq!(policy.atomic_activation_pick_hints.len(), CAP);
        drop(policy);
        for &(slot, id, _) in roots {
            assert!(Activation::capture(&[(slot, id)]).is_none());
        }
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_single_activation_uses_spawn_fifo_and_rejects_foreign_duplicate_or_retired_members() {
    roots(&[ID, ID + 1], 1, |roots, turn| {
        let (slot, id, _) = roots[0];
        assert!(Activation::capture(&[(slot, id + 100)]).is_none());
        // Exercise canonical input preflight independently of Cohort's later
        // unique-slot/busy checks, which must not mask its duplicate witness.
        assert!(!super::super::distinct_nonzero_ids([id, id].into_iter()));
        assert!(super::super::distinct_nonzero_ids([id, roots[1].1].into_iter()));
        assert!(!super::super::distinct_nonzero_ids([0].into_iter()));
        assert!(Activation::capture(&[(slot, id), (slot, id)]).is_none());
        let owner = Activation::capture(&[(slot, id)]).unwrap();
        assert!(owner.commit_owned(turn, |_| 1, || {}));
        assert_eq!(dispatch_policy::lock(0).spawn_pick_hints.front(), Some(handoff_queue::FirstTurnHandoff { slot, task_id: id }));
        assert!(dispatch_policy::lock(0).atomic_activation_pick_hints.is_empty());
        assert!(Activation::capture(&[(slot, id)]).is_none());
        let (retired, retired_id, _) = roots[1];
        identity::update_live_lifecycle(retired, retired_id, identity::LifecycleGate::Retired, true);
        assert!(Activation::capture(&[(retired, retired_id)]).is_none());
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_over_capacity_activation_refuses_before_callback_with_all_real_members_suspended() {
    let ids: Vec<_> = (0..=CAP).map(|i| ID + i as u64).collect();
    roots(&ids, 1, |roots, _| {
        let members: Vec<_> = roots.iter().map(|&(slot, id, _)| (slot, id)).collect();
        assert!(Activation::capture(&members).is_none());
        for &(slot, _, _) in roots {
            assert!(identity::lifecycle(slot).suspended);
            assert_eq!(identity::execution::count(slot), 0);
        }
        assert!(dispatch_policy::lock(0).spawn_pick_hints.is_empty());
        assert!(dispatch_policy::lock(0).atomic_activation_pick_hints.is_empty());
    });
}
#[test]
fn actual_priority_saturation_degrades_to_original_spawn_fifo_not_atomic_count_overflow() {
    roots(&[ID, ID + 1], 1, |roots, turn| {
        {
            let mut policy = dispatch_policy::lock(0);
            for i in 0..CAP {
                assert!(super::super::enqueue_priority(
                    &mut policy,
                    handoff_queue::FirstTurnHandoff {
                        slot: 100 + i,
                        task_id: ID + 100 + i as u64
                    }
                ));
            }
        }
        assert!(capture(roots).commit_owned(turn, |_| 1, || {}));
        let mut policy = dispatch_policy::lock(0);
        assert_eq!(policy.atomic_activation_handoff_remaining, CAP);
        assert_eq!(policy.atomic_activation_pick_hints.len(), CAP);
        for &(slot, id, _) in roots {
            assert_eq!(
                policy.spawn_pick_hints.pop(),
                Some(handoff_queue::FirstTurnHandoff { slot, task_id: id })
            );
        }
        assert!(policy.spawn_pick_hints.is_empty());
    });
}
#[test]
fn actual_remote_activation_publishes_original_mailbox_before_complete_ack_and_bound_frame() {
    roots(&[ID, ID + 1], 3, |roots, turn| {
        assert!(capture(roots).commit_owned(turn, |_| 2, || {}));
        for &(slot, _, _) in roots {
            assert_eq!(
                runqueue::owner(slot).state,
                runqueue::RunOwnerState::RemoteQueued
            );
            assert_eq!(runqueue::owner(slot).cpu, Some(1));
            assert!(!identity::cohort::busy(slot));
        }
        assert_eq!(
            dispatch_policy::lock(1).atomic_activation_handoff_remaining,
            2
        );
        runqueue::drain_remote_wakes(1);
        for &(slot, _, _) in roots {
            assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
            assert!(frame_publication::local_frame_token(slot, 1).is_some());
        }
    });
}
#[test]
fn actual_activation_preserves_strict_class_sleeper_floor_without_priority_donation_shortcut() {
    roots(&[ID, ID + 1, ID + 2], 1, |roots, turn| {
        assert!(capture(&roots[..1]).commit_owned(turn, |_| 1, || {}));
        runqueue::raise_vruntime_floor(roots[0].0, 20_000_000);
        assert_eq!(kernel_creation::ready_floor(0, Some(false)), 20_000_000);
        assert_eq!(kernel_creation::ready_floor(0, Some(true)), 0);
        assert!(capture(&roots[1..]).commit_owned(turn, |_| 1, || {}));
        assert_eq!(
            runqueue::vruntime(roots[1].0),
            20_000_000 - SLEEPER_LATENCY_BONUS_NS
        );
        assert_eq!(
            dispatch_policy::lock(0).atomic_activation_handoff_remaining,
            2
        );
    });
}

#[test]
fn actual_activation_refuses_copied_current_slot_without_original_physical_cpu_bank_before_callback()
 {
    roots(&[ID], 1, |roots, turn| {
        let owner = capture(roots);
        let consumed = Cell::new(false);
        // Explicit negative native publication injection: the real bank still
        // holds the original kernel actor. A scalar replacement is not Current.
        let copied = cpu_local::install_test_current_owner(0, 120);
        // The complete entry also refuses in OriginalCurrent::borrow, before
        // reaching its repeated caller checks. Exercise that check itself:
        // matching CPU/scalar input must STILL not stand in for the real bank.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::caller(turn, 120);
        })).is_err());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.commit_owned(
                turn,
                |_| 1,
                || consumed.set(true)
            )))
            .is_err()
        );
        drop(copied);
        assert!(!consumed.get());
        assert!(identity::lifecycle(roots[0].0).suspended);
        assert!(!identity::cohort::busy(roots[0].0));
        assert_eq!(
            runqueue::owner(roots[0].0).state,
            runqueue::RunOwnerState::Blocked
        );
    });
}

#[test]
fn actual_single_activation_recomputes_same_band_floor_after_wake_for_original_spawn_donation() {
    roots(&[ID, ID + 1], 1, |roots, turn| {
        assert!(capture(&roots[..1]).commit_owned(turn, |_| 1, || {}));
        runqueue::raise_vruntime_floor(roots[0].0, 30_000_000);
        runqueue::raise_vruntime_floor(cpu_local::current_cpu_task_slot().unwrap(), 40_000_000);
        assert!(capture(&roots[1..]).commit_owned(turn, |_| 1, || {}));
        assert_eq!(
            runqueue::vruntime(roots[1].0),
            30_000_000 - SLEEPER_LATENCY_BONUS_NS - IPC_DONATION_BONUS_NS
        );
        let mut policy = dispatch_policy::lock(0);
        assert_eq!(policy.atomic_activation_handoff_remaining, 0);
        for &(slot, id, _) in roots {
            assert_eq!(
                policy.spawn_pick_hints.pop(),
                Some(handoff_queue::FirstTurnHandoff { slot, task_id: id })
            );
        }
    });
}

#[test]
fn actual_stop_gates_remain_ineligible_after_owned_activation_without_queue_or_priority_records() {
    roots(&[ID, ID + 1], 1, |roots, turn| {
        identity::update_live_lifecycle(roots[0].0, ID, identity::LifecycleGate::JobStopped, true);
        identity::update_live_lifecycle(
            roots[1].0,
            ID + 1,
            identity::LifecycleGate::ExecQuiesced,
            true,
        );
        assert!(capture(roots).commit_owned(turn, |_| 1, || {}));
        for &(slot, _, _) in roots {
            assert!(!identity::lifecycle(slot).suspended);
            assert_eq!(
                runqueue::owner(slot).state,
                runqueue::RunOwnerState::Blocked
            );
            assert!(
                identity::execution::with_claim(slot, |_| panic!("stopped root claimed")).is_none()
            );
        }
        let policy = dispatch_policy::lock(0);
        assert!(policy.atomic_activation_pick_hints.is_empty());
        assert!(policy.spawn_pick_hints.is_empty());
        assert_eq!(policy.atomic_activation_handoff_remaining, 0);
    });
}
