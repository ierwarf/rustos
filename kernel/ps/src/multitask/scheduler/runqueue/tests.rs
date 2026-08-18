//! Runnable-ownership and remote-wake custody tests.
//!
//! Split out of `runqueue.rs` so the module stays under the source line
//! budget; the contents are unchanged.

use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

struct RunQueueTestScope {
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl RunQueueTestScope {
    fn new() -> Self {
        let serial = TEST_GUARD.lock().unwrap();
        reset_before_publication();
        reset_test_local_migrating_owner();
        Self { _serial: serial }
    }
}

impl Drop for RunQueueTestScope {
    fn drop(&mut self) {
        reset_before_publication();
        reset_test_local_migrating_owner();
    }
}

/// The lock-free membership mirror is what every candidate scan reads, so
/// a mutation that forgets to publish would hide a runnable task from
/// dispatch, or keep naming one that left. Walk the full local lifecycle
/// and require the mirror to equal the locked bitmap after each step.
#[test]
fn published_membership_mirrors_the_locked_bitmap_through_the_local_lifecycle() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();

    fn assert_mirrored(cpu: usize, step: &str) {
        let locked = RUN_QUEUES[cpu].inner.lock().runnable;
        let published: [u64; BITMAP_WORDS] = core::array::from_fn(|word| {
            RUN_QUEUES[cpu].published_runnable[word].load(Ordering::Acquire)
        });
        assert_eq!(published, locked, "membership mirror diverged after {step}");
    }

    const CPU: usize = 3;
    const SLOT: usize = 65;
    // Deliberately past the first bitmap word, so a mirror that publishes
    // only word zero fails here.
    assert!(SLOT >= 64 && SLOT < MAX_TASK);

    assert_mirrored(CPU, "reset");
    admit_blocked(SLOT);
    assert!(matches!(
        publish_remote_wake(SLOT, CPU, 1024),
        RemoteWakeOutcome::Published { cpu: CPU, .. }
    ));
    assert_mirrored(CPU, "remote wake published");
    assert_eq!(drain_remote_wakes(CPU), 1);
    assert_mirrored(CPU, "remote wake drained");
    let (word, bit) = bitmap_location(SLOT);
    assert_ne!(
        RUN_QUEUES[CPU].published_runnable[word].load(Ordering::Acquire) & bit,
        0,
        "a drained wake must be visible to a lock-free scan"
    );

    assert!(claim_dispatch(SLOT, CPU, 1024));
    assert_mirrored(CPU, "dispatch claimed");
    publish_blocked(SLOT, CPU, 1024);
    assert_mirrored(CPU, "blocked");
    assert_eq!(
        RUN_QUEUES[CPU].published_runnable[word].load(Ordering::Acquire) & bit,
        0,
        "a blocked slot must leave the lock-free membership"
    );
    retire(SLOT, 1024);
    assert_mirrored(CPU, "retired");
}

#[test]
fn remote_wake_has_one_mailbox_and_one_local_owner() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_blocked(7);
    assert_eq!(
        publish_remote_wake(7, 2, 1024),
        RemoteWakeOutcome::Published {
            cpu: 2,
            notify: true
        }
    );
    assert_eq!(
        owner(7),
        RunOwnerSnapshot::new(RunOwnerState::RemoteQueued, Some(2), 3)
    );
    assert_eq!(drain_remote_wakes(2), 1);
    assert!(is_local_dispatchable(7, 2));
    assert!(claim_dispatch(7, 2, 1024));
    assert_eq!(owner(7).state, RunOwnerState::Running);
    publish_blocked(7, 2, 1024);
    assert_eq!(owner(7).state, RunOwnerState::Blocked);
}

#[test]
fn duplicate_wake_is_idempotent_and_terminal_wake_fails_closed() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_blocked(9);
    assert_eq!(
        publish_remote_wake(9, 1, 100),
        RemoteWakeOutcome::Published {
            cpu: 1,
            notify: true
        }
    );
    assert_eq!(
        publish_remote_wake(9, 1, 100),
        RemoteWakeOutcome::AlreadyOwned { cpu: Some(1) }
    );
    assert_eq!(drain_remote_wakes(1), 1);
    retire(9, 100);
    assert_eq!(owner(9).state, RunOwnerState::Retired);
    assert_eq!(publish_remote_wake(9, 1, 100), RemoteWakeOutcome::Rejected);
    release_retired(9);
    assert_eq!(owner(9).state, RunOwnerState::Dormant);
}

#[test]
fn duplicate_local_wake_is_idempotent_and_terminal_wake_fails_closed() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_blocked(10);
    assert_eq!(
        publish_local_wake(10, 1, 100),
        RemoteWakeOutcome::Published {
            cpu: 1,
            notify: false
        }
    );
    assert_eq!(owner(10).state, RunOwnerState::Local);
    assert_eq!(
        publish_local_wake(10, 1, 100),
        RemoteWakeOutcome::AlreadyOwned { cpu: Some(1) }
    );
    retire(10, 100);
    assert_eq!(owner(10).state, RunOwnerState::Retired);
    assert_eq!(publish_local_wake(10, 1, 100), RemoteWakeOutcome::Rejected);
    release_retired(10);
    assert_eq!(owner(10).state, RunOwnerState::Dormant);
}

#[test]
fn local_wake_deduplicates_a_still_remote_queued_owner() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_blocked(12);
    // A cross-CPU wake lands the owner in `RemoteQueued`, not yet drained
    // into the local runqueue.
    assert_eq!(
        publish_remote_wake(12, 2, 100),
        RemoteWakeOutcome::Published {
            cpu: 2,
            notify: true
        }
    );
    assert_eq!(owner(12).state, RunOwnerState::RemoteQueued);
    // A same-CPU wake racing that drain must dedup against it, not treat the
    // still-in-flight owner as fresh `Blocked` custody it can re-publish.
    assert_eq!(
        publish_local_wake(12, 2, 100),
        RemoteWakeOutcome::AlreadyOwned { cpu: Some(2) }
    );
    assert_eq!(owner(12).state, RunOwnerState::RemoteQueued);
}

#[test]
fn dispatch_rejects_wrong_cpu_without_changing_owner() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_blocked(11);
    publish_local(11, 3, 200);
    assert!(!claim_dispatch(11, 4, 200));
    assert!(is_local_dispatchable(11, 3));
}

#[test]
fn nonrunnable_local_slot_cannot_be_claimed_until_its_exact_wake() {
    let _scope = RunQueueTestScope::new();
    let slot = 12;
    let cpu = 3;
    let weight = 220;
    admit_blocked(slot);
    publish_local(slot, cpu, weight);
    set_runnable(slot, false);
    let blocked = owner(slot);

    assert_eq!(blocked.state, RunOwnerState::Local);
    assert_eq!(blocked.cpu, Some(cpu));
    assert!(!blocked.runnable);
    assert!(!is_local_dispatchable(slot, cpu));
    assert!(
        !claim_dispatch(slot, cpu, weight),
        "a Local owner with runnable=false must not consume queue custody"
    );
    assert_eq!(
        owner(slot),
        blocked,
        "rejected claim must not advance generation"
    );
    assert_eq!(
        published_runnable_count(cpu),
        1,
        "rejected claim must preserve queue membership"
    );

    set_runnable(slot, true);
    let restored = owner(slot);
    assert!(is_local_dispatchable(slot, cpu));
    assert!(claim_dispatch(slot, cpu, weight));
    let claimed = owner(slot);
    assert_eq!(claimed.state, RunOwnerState::Running);
    assert_eq!(claimed.cpu, Some(cpu));
    assert!(claimed.runnable);
    assert_eq!(claimed.generation, restored.generation + 1);
    assert_eq!(published_runnable_count(cpu), 0);
}

#[test]
fn affinity_rehome_invalidates_and_coalesces_old_mailbox_generations() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_blocked(13);
    assert!(matches!(
        publish_remote_wake(13, 1, 300),
        RemoteWakeOutcome::Published { cpu: 1, .. }
    ));
    assert!(matches!(
        rehome_queued(13, 4, 300),
        RemoteWakeOutcome::Published { cpu: 4, .. }
    ));
    assert!(matches!(
        rehome_queued(13, 1, 300),
        RemoteWakeOutcome::Published { cpu: 1, .. }
    ));
    assert_eq!(drain_remote_wakes(4), 1);
    assert!(!is_local_dispatchable(13, 4));
    // CPU 1 receives one current record rather than the old and new
    // generations occupying two finite mailbox entries.
    assert_eq!(drain_remote_wakes(1), 1);
    assert!(is_local_dispatchable(13, 1));
}

#[test]
fn running_admission_and_load_placement_are_cpu_exact() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_running(17, 5);
    assert_eq!(
        owner(17),
        RunOwnerSnapshot::new(RunOwnerState::Running, Some(5), 2)
    );
    admit_blocked(18);
    publish_local(18, 3, 50);
    assert_eq!(least_loaded_cpu((1 << 3) | (1 << 4), 4), 4);
}

#[test]
fn idle_steal_uses_single_owner_mailbox_transfer() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_blocked(19);
    publish_local(19, 1, 75);
    assert_eq!(published_runnable_count(1), 1);
    assert!(matches!(
        rehome_queued(19, 2, 75),
        RemoteWakeOutcome::Published { cpu: 2, .. }
    ));
    assert_eq!(published_runnable_count(1), 0);
    assert_eq!(drain_remote_wakes(2), 1);
    assert!(claim_dispatch(19, 2, 75));
    assert_eq!(owner(19).state, RunOwnerState::Running);
    assert_eq!(owner(19).cpu, Some(2));
}

#[test]
fn local_dispatch_gate_observes_queue_and_remote_mailbox_authority() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    admit_running(21, 6);
    assert!(!local_dispatch_work_pending(6));

    admit_blocked(22);
    assert!(matches!(
        publish_remote_wake(22, 6, 250),
        RemoteWakeOutcome::Published { cpu: 6, .. }
    ));
    assert!(local_dispatch_work_pending(6));
    assert_eq!(drain_remote_wakes(6), 1);
    assert!(local_dispatch_work_pending(6));
    assert!(claim_dispatch(22, 6, 250));
    assert!(!local_dispatch_work_pending(6));
}

#[test]
fn migrating_owner_remains_runnable_until_mailbox_admission() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    let slot = 31;
    let target_cpu = 4;
    admit_blocked(slot);
    let blocked = owner(slot);
    let migrating = blocked.next(RunOwnerState::Migrating, Some(target_cpu));
    OWNER_WORDS[slot]
        .compare_exchange(blocked, migrating)
        .expect("test migration owner publication");

    assert_eq!(owner(slot), migrating);
    assert!(
        owner(slot).runnable,
        "the migration handoff must remain schedulable before target mailbox admission"
    );
    assert!(matches!(
        publish_migrating_record(slot, migrating, target_cpu, 125),
        RemoteWakeOutcome::Published { cpu: 4, .. }
    ));
    assert!(owner(slot).runnable);
    assert_eq!(drain_remote_wakes(target_cpu), 1);
    assert!(is_local_dispatchable(slot, target_cpu));
    retire(slot, 125);
    release_retired(slot);
    reset_before_publication();
}

#[test]
fn running_owner_runnable_bit_tracks_block_and_wake() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    let slot = 32;
    let cpu = 2;
    admit_running(slot, cpu);
    assert!(owner(slot).runnable);

    set_runnable(slot, false);
    assert_eq!(owner(slot).state, RunOwnerState::Running);
    assert_eq!(owner(slot).cpu, Some(cpu));
    assert!(
        !owner(slot).runnable,
        "a running task that blocks must withdraw only its runnable bit"
    );

    set_runnable(slot, true);
    assert_eq!(owner(slot).state, RunOwnerState::Running);
    assert_eq!(owner(slot).cpu, Some(cpu));
    assert!(
        owner(slot).runnable,
        "a wake raced with the running owner must restore its runnable bit"
    );
    retire(slot, 100);
    release_retired(slot);
    reset_before_publication();
}

#[test]
fn running_task_may_requeue_only_on_its_owner_cpu() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    let slot = 33;
    let owner_cpu = 2;
    admit_running(slot, owner_cpu);
    let before = owner(slot);

    assert!(
        catch_unwind(AssertUnwindSafe(|| publish_local(slot, owner_cpu + 1, 220))).is_err(),
        "a foreign CPU cannot publish a running task into its local queue"
    );
    assert_eq!(owner(slot), before);
    assert_eq!(published_runnable_count(owner_cpu), 0);
    assert_eq!(published_runnable_count(owner_cpu + 1), 0);

    publish_local(slot, owner_cpu, 220);
    assert!(is_local_dispatchable(slot, owner_cpu));
    assert_eq!(published_runnable_count(owner_cpu), 1);
    retire(slot, 220);
    release_retired(slot);
    reset_before_publication();
}

#[test]
fn local_block_removes_exact_queue_membership() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    let cpu = 3;
    let blocked = 34;
    let unaffected = 35;
    admit_blocked(blocked);
    admit_blocked(unaffected);
    publish_local(blocked, cpu, 140);
    publish_local(unaffected, cpu, 160);
    assert_eq!(published_runnable_count(cpu), 2);

    publish_blocked(blocked, cpu, 140);
    assert_eq!(owner(blocked).state, RunOwnerState::Blocked);
    assert_eq!(owner(blocked).cpu, None);
    assert!(!is_local_dispatchable(blocked, cpu));
    assert_eq!(published_runnable_count(cpu), 1);
    assert!(
        is_local_dispatchable(unaffected, cpu),
        "blocking one slot must preserve every other local membership"
    );

    retire(blocked, 140);
    release_retired(blocked);
    retire(unaffected, 160);
    release_retired(unaffected);
    reset_before_publication();
}

#[test]
fn migration_owner_names_target_until_mailbox_publication() {
    let _scope = RunQueueTestScope::new();
    let slot = 36;
    let source_cpu = 1;
    let target_cpu = 5;
    let weight = 180;
    admit_blocked(slot);
    publish_local(slot, source_cpu, weight);

    assert!(matches!(
        rehome_queued(slot, target_cpu, weight),
        RemoteWakeOutcome::Published { cpu: 5, .. }
    ));
    let migrating = take_test_local_migrating_owner()
        .expect("local rehome must publish a transient migrating owner before its mailbox");
    assert_eq!(migrating.state, RunOwnerState::Migrating);
    assert_eq!(
        migrating.cpu,
        Some(target_cpu),
        "the transient migration owner must identify the destination, never the source"
    );
    assert!(migrating.runnable);
    assert_eq!(published_runnable_count(source_cpu), 0);
    assert_eq!(drain_remote_wakes(target_cpu), 1);
    assert!(is_local_dispatchable(slot, target_cpu));
}

#[test]
fn retirement_removes_local_queue_membership() {
    let _guard = TEST_GUARD.lock().unwrap();
    reset_before_publication();
    let slot = 37;
    let cpu = 6;
    let weight = 200;
    admit_blocked(slot);
    publish_local(slot, cpu, weight);
    assert!(is_local_dispatchable(slot, cpu));
    assert_eq!(published_runnable_count(cpu), 1);

    retire(slot, weight);
    let retired = owner(slot);
    assert_eq!(retired.state, RunOwnerState::Retired);
    assert_eq!(retired.cpu, None);
    assert!(!retired.runnable);
    assert!(!is_local_dispatchable(slot, cpu));
    assert_eq!(published_runnable_count(cpu), 0);
    release_retired(slot);
    reset_before_publication();
}
