//! Born physical allocation and actual Local->Migrating->mailbox effects.
//! Owner: original turn and non-Copy reader borrowing the physical StackPin.
//! Boundary: exact intermediate CAS, source unlink and target mailbox/drain.
//! Lifecycle: reader lives through CAS/publication; original loan drops last.
//! Concurrency: raced Blocked publication must refuse, never retry a new owner.
//! Failure: no stale-generation transfer, lost source membership or hidden wake.
//! Forbidden: final mailbox repair as proof of correct intermediate CAS target.
//! Evidence: actual owner/bitmap/mailbox, not the native all-ones bitmap model;
//! earlier IRQ/topology remain models, never hardware switch acceptance.
use super::*;
pub(in crate::multitask::scheduler) mod legacy_witnesses;

#[test]
fn original_migration_reader_spans_exact_owner_cas_and_target_mailbox() {
    super::super::super::dispatch_claim::tests::fixture(false, |slot, turn| {
        runqueue::affinity_payload::set_affinity(slot, 3, 3, false);
        let token = frame_publication::local_frame_token(slot, 0).unwrap();
        let pin = frame_publication::pin_for_identity(token, 82800).unwrap();
        // SAFETY: same original masked CPU and actual isolated born backing.
        let reader = unsafe { stack_resources::frame_readers::acquire(&pin, 0) }.unwrap();
        let frame = MigrationFrame { turn, token, id: 82800, source: 0, _reader: reader };
        assert!(frame.is_current());
        let moved = runqueue::rehome_queued_frame(&frame, 0, 1, runqueue::weight::value(slot));
        assert!(matches!(moved, runqueue::RemoteWakeOutcome::Published { cpu: 1, .. }));
        let transferred = runqueue::take_test_local_migrating_owner().unwrap();
        assert_eq!(transferred.state, runqueue::RunOwnerState::Migrating);
        assert_eq!(transferred.cpu, Some(1));
        assert_eq!(transferred.generation, token.owner_generation + 1);
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::RemoteQueued);
        assert_eq!(runqueue::owner(slot).cpu, Some(1));
        assert!(!frame.is_current()); // original owner was spent, not renewed
        assert!(!runqueue::published_local_membership(slot, 0));
        // The ORIGINAL backing remains pinned through actual remote completion.
        assert!(runqueue::drain_remote_wakes(1) >= 1);
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert!(runqueue::published_local_membership(slot, 1));
        assert!(!frame.is_current());
        drop(frame);
        drop(pin);
        // Complete the physical transfer back to the fixture's original CPU;
        // its earlier one-CPU reset must not leave remote queue membership.
        assert!(transfer(turn, slot, 1, 0));
        assert_eq!(runqueue::owner(slot).cpu, Some(0));
        assert!(!runqueue::published_local_membership(slot, 1));
    });
}

#[test]
fn raced_block_refuses_migration_without_reacquiring_generation_or_waking() {
    super::super::super::dispatch_claim::tests::fixture(false, |slot, turn| {
        runqueue::affinity_payload::set_affinity(slot, 3, 3, false);
        let token = frame_publication::local_frame_token(slot, 0).unwrap();
        let pin = frame_publication::pin_for_identity(token, 82800).unwrap();
        // SAFETY: actual original CPU and pinned born frame before queue race.
        let reader = unsafe { stack_resources::frame_readers::acquire(&pin, 0) }.unwrap();
        let frame = MigrationFrame { turn, token, id: 82800, source: 0, _reader: reader };
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        let blocked = runqueue::owner(slot);
        assert_eq!(blocked.state, runqueue::RunOwnerState::Blocked);
        assert!(!frame.is_current());
        assert_eq!(runqueue::rehome_queued_frame(&frame, 0, 1, runqueue::weight::value(slot)), runqueue::RemoteWakeOutcome::Rejected);
        assert_eq!(runqueue::owner(slot), blocked);
        let _ = runqueue::drain_remote_wakes(1);
        assert_eq!(runqueue::owner(slot), blocked);
        assert!(!runqueue::published_local_membership(slot, 1));
        drop(frame);
        drop(pin);
    });
}

#[test]
fn busiest_source_cpu_picks_highest_runnable_count_and_breaks_ties_by_load() {
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    assert_eq!(busiest_source_cpu(runqueue::nonempty_rq_cpu_mask()), None);

    // CPU 1: 1 task (weight 100), CPU 3: 1 task (weight 500), CPU 6: 2 tasks (weight 100 each).
    for (slot, cpu, weight) in [(30, 1, 100), (31, 3, 500), (32, 6, 100), (33, 6, 100)] {
        runqueue::weight::initialize(slot, weight);
        runqueue::admit_blocked(slot);
        runqueue::publish_local(slot, cpu, weight);
    }
    let mask = runqueue::nonempty_rq_cpu_mask();
    assert_eq!(mask, (1 << 1) | (1 << 3) | (1 << 6));
    assert_eq!(busiest_source_cpu(mask), Some(6));
    // Excluding CPU 6, CPU 3 beats CPU 1 on load tie-breaker.
    assert_eq!(busiest_source_cpu(mask & !(1 << 6)), Some(3));
    assert_eq!(busiest_source_cpu(mask & !((1 << 6) | (1 << 3))), Some(1));

    for (slot, weight) in [(30, 100), (31, 500), (32, 100), (33, 100)] {
        runqueue::retire(slot, weight);
        runqueue::release_retired(slot);
    }
    runqueue::reset_before_publication();
}
