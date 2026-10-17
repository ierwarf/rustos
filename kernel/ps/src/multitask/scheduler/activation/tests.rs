//! Owner: suspended activation/authority boundary and CPU-local FIFO arbitration.
//! Boundary: real native Cohort and catalog/frame components; production directory
//! removal is compiler/source qualified, not a native HAL/CR3/RSP execution claim.
//! Lifecycle: refusal/one-shot capture before callback; terminal stop wins publication.
//! Concurrency: competing disjoint cohorts share only bounded per-CPU policy leaves.
//! Failure: full/duplicate/exhausted hint custody degrades, never grants execution.
//! Forbidden: native injected hidden identity or fixture recovery is not production reopen.
//! Evidence: real shared commit, Class51 affinity turn, Class42 hint admission and pins.
use super::super::{UserAbi, root_creation, tests::boxed_scheduler};
use super::*;
use crate::multitask::current_identity as identity;
use core::cell::Cell;
fn suspended(id: u64, f: impl FnOnce(usize)) {
    root_creation::tests::suspended_roots(
        &[id], 1, |_| root_creation::tests::bootstrap(UserAbi::Linux),
        |roots, _| f(roots[0].0),
    );
}
#[test]
fn exact_activation_capture_denies_bounds_uid_alias_duplicate_replay_and_unsuspended_binding_before_callback()
 {
    suspended(99001, |slot| {
    assert!(Activation::capture(&[]).is_none());
    assert!(Activation::capture(&[(slot, 0)]).is_none());
    assert!(Activation::capture(&[(slot, 99001), (slot, 99001)]).is_none());
    assert!(Activation::capture(&[(slot, 99002)]).is_none());
    assert!(Activation::capture(&[(slot, 99001); CAP + 1]).is_none());
    let owner = Activation::capture(&[(slot, 99001)]).unwrap();
    assert!(Activation::capture(&[(slot, 99001)]).is_none());
    drop(owner);
    assert_eq!(identity::execution::count(slot), 0);
    identity::update_live_lifecycle(slot, 99001, identity::LifecycleGate::Suspended, false);
    assert!(Activation::capture(&[(slot, 99001)]).is_none());
    assert_eq!(identity::execution::count(slot), 0);
    identity::update_live_lifecycle(slot, 99001, identity::LifecycleGate::Suspended, true);
    });
}
#[test]
fn shared_commit_retains_every_binding_and_suspend_through_authority_callback_and_one_release() {
    // Same obligation now executes the actual born-resource production commit.
    super::publication::tests::actual_activation_holds_original_frame_and_binding_through_callback_before_queue_and_ack();
}
#[test]
fn terminal_stop_and_hidden_metadata_during_callback_never_reawaken_or_consume_next_binding() {
    // Same obligation now executes the actual born-resource production commit.
    super::publication::tests::actual_terminal_during_callback_keeps_hidden_original_retired_member_and_wakes_only_peer();
}
#[test]
fn callback_unwind_before_task_effects_keeps_targets_suspended_and_releases_only_captured_refs() {
    // Same obligation now executes the actual born-resource production commit.
    super::publication::tests::actual_callback_unwind_leaves_all_suspended_and_releases_frame_loans_for_exact_retry();
}
#[test]
fn concurrent_task_affinity_refuses_under_same_identity_seal_without_spending_mask_epoch_or_callback()
 {
    suspended(99401, |slot| {
    let foreign_touched = Cell::new(false);
    assert!(
        identity::with_affinity_replacement(slot, Some(99402), None, || foreign_touched.set(true))
            .is_none()
    );
    assert!(!foreign_touched.get());
    let owner = Activation::capture(&[(slot, 99401)]).unwrap();
    let before = super::super::runqueue::affinity_payload::affinity_snapshot(slot);
    assert!(!super::super::runqueue::affinity_payload::try_set_affinity(
        slot, 1, 1, false
    ));
    assert_eq!(
        super::super::runqueue::affinity_payload::affinity_snapshot(slot),
        before
    );
    let touched = Cell::new(false);
    assert!(
        identity::with_affinity_replacement(slot, Some(99401), None, || touched.set(true))
            .is_none()
    );
    assert!(!touched.get());
    drop(owner);
    assert!(super::super::runqueue::affinity_payload::try_set_affinity(
        slot, 1, 1, false
    ));
    });
}
#[test]
fn hint_capacity_duplicate_and_counter_exhaustion_refuse_before_spending_queue_or_remaining() {
    let mut policy = super::super::dispatch_policy::CpuDispatchPolicy::new();
    for slot in 0..CAP {
        assert!(enqueue_priority(
            &mut policy,
            super::super::handoff_queue::FirstTurnHandoff {
                slot,
                task_id: slot as u64 + 1
            }
        ));
    }
    assert_eq!(policy.atomic_activation_handoff_remaining, CAP);
    assert!(!enqueue_priority(
        &mut policy,
        super::super::handoff_queue::FirstTurnHandoff {
            slot: CAP,
            task_id: 100
        }
    ));
    assert!(!enqueue_priority(
        &mut policy,
        super::super::handoff_queue::FirstTurnHandoff {
            slot: 0,
            task_id: 200
        }
    ));
    assert_eq!(policy.atomic_activation_pick_hints.len(), CAP);
    assert_eq!(policy.atomic_activation_handoff_remaining, CAP);
    let mut policy = super::super::dispatch_policy::CpuDispatchPolicy::new();
    policy.atomic_activation_handoff_remaining = usize::MAX;
    assert!(!enqueue_priority(
        &mut policy,
        super::super::handoff_queue::FirstTurnHandoff {
            slot: 0,
            task_id: 1
        }
    ));
    assert!(policy.atomic_activation_pick_hints.is_empty());
}
#[test]
fn competing_disjoint_cohorts_cannot_overflow_one_cpu_priority_queue_after_same_empty_observation()
{
    let _fixture = boxed_scheduler();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let shared = std::sync::Arc::new(nucleus_core::util::lockdep::TrackedSpinLock::<
        _,
        { nucleus_core::util::lockdep::LockClass::SchedulerPolicy as u8 },
    >::new(
        super::super::dispatch_policy::CpuDispatchPolicy::new()
    ));
    let mut workers = std::vec::Vec::new();
    for group in 0..2 {
        let barrier = barrier.clone();
        let shared = shared.clone();
        workers.push(std::thread::spawn(move || {
            assert!(shared.lock().atomic_activation_pick_hints.is_empty());
            barrier.wait();
            let mut count = 0;
            for i in 0..CAP {
                let slot = group * CAP + i;
                let mut policy = shared.lock();
                if enqueue_priority(
                    &mut policy,
                    super::super::handoff_queue::FirstTurnHandoff {
                        slot,
                        task_id: slot as u64 + 1,
                    },
                ) {
                    count += 1;
                }
            }
            count
        }));
    }
    let inserted: usize = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .sum();
    assert_eq!(inserted, CAP);
    let policy = shared.lock();
    assert_eq!(policy.atomic_activation_pick_hints.len(), CAP);
    assert_eq!(policy.atomic_activation_handoff_remaining, CAP);
}
