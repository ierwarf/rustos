//! Owner: actual physical resource/census mutual admission witnesses.
//! Boundary: actual root/thread/Resource53/Class55, terminal consumer ACK modeled.
//! Lifecycle: frozen census excludes physical detach; exact reclaim excludes new census.
//! Concurrency: both admission orders and logical-removal/abandonment prefixes.
//! Failure: no nonce/closing on refusal, no post-stop execution/MM/stack pin.
//! Forbidden: modeled terminal ACK/fixture cleanup as hardware or complete guard refinement.
//! Evidence: real born resources, original Intent/ExecMembership and non-Copy Reuse.
use super::*;
use crate::multitask::current_identity as identity;
use crate::multitask::process_placement::{self as placement, exec_membership::ExecMembership};
use crate::multitask::process_table::{self, ProcessHandle};
use crate::multitask::scheduler::*;
use crate::user::abi::UserAbi;
const ID: u64 = 83800;
fn boot() -> UserTaskBootstrap {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.entry = VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x440000);
    boot.stack_pointer = VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x780000);
    boot.linux_thread_state = Some(LinuxThreadState::default());
    boot
}
fn fixture(f: impl FnOnce(usize, usize, ProcessHandle)) {
    root_creation::tests::exec_root(ID, true, boot(), |target, handle| {
        let sibling = thread_creation::tests::active_sibling(target, ID + 1, boot());
        f(target, sibling, handle);
    });
}
fn intent(target: usize, handle: ProcessHandle) -> identity::exec_intent::Intent {
    let reservation = process_table::begin_exec(handle).unwrap();
    identity::exec_intent::begin(target, ID, reservation, true).unwrap()
}
fn cancel(intent: identity::exec_intent::Intent, census: Option<ExecMembership>) {
    if let Some(census) = census {
        census.finish_for_test();
    }
    intent.cancel_with(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
        process_table::cancel_exec_owned,
    );
}
fn stop(slot: usize, handle: ProcessHandle) {
    let id = ID + 1;
    let mut turn = placement::terminal::Turn::begin(handle, (slot, id)).unwrap();
    identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true);
    runqueue::retire(slot, NICE_0_LOAD);
    turn.mark_stopped();
    let mut producer = retirement_cleanup::producer::Producer::begin(
        slot,
        Some(id),
        TaskRetireReason::Exited,
        true,
    )
    .unwrap();
    producer.bind_process_turn(turn.credential());
    producer.begin_effects();
    let receipt = producer.publish(None, RetirementSideEffect::new(Some(id), None));
    retirement_cleanup::ack_kernel_fixture(slot);
    turn.finish(receipt);
    assert_eq!(identity::execution::count(slot), 0);
    // ORDERING: acquire observes the real last allocation loan release before reclaim.
    assert_eq!(RESOURCES[slot].pins.load(Ordering::Acquire), 0);
}
fn complete(mut reuse: Reuse, handle: ProcessHandle) {
    let slot = reuse.slot;
    let context = context::read(slot);
    let mut reset = retirement_cleanup::reuse::begin(slot, Some(ID + 1)).unwrap();
    if let Some(context) = context {
        assert!(scheduling_runtime::close_for_reclaim(
            slot,
            context.scheduling_context.identity()
        ));
        scheduling_runtime::release(slot, context.scheduling_context.identity());
    }
    dispatch_policy::remove_reclaimed_slot(slot);
    reset.mark_effects();
    reset.reset();
    runqueue::release_retired(slot);
    context::clear(slot);
    linux_thread_state::clear_reclaimed(slot);
    runqueue::simd_tls::reset_simd_state(slot);
    task_start::clear(slot);
    frame_publication::clear(slot);
    let storage = reuse.take_storage();
    process_table::detach_task(handle).unwrap();
    drop(storage);
    reset.finish();
    reuse.finish();
}
#[test]
fn actual_frozen_census_refuses_zero_borrower_reclaim_without_partial_close_or_nonce_and_denies_physical_clear()
 {
    fixture(|target, sibling, handle| {
        let intent = intent(target, handle);
        let census = ExecMembership::begin(&intent).unwrap();
        stop(sibling, handle);
        let before = RESOURCES[sibling].with(|r| (r.reservation_generation, r.closing));
        assert!(begin_retired(sibling, ID + 1).is_none());
        assert!(!close_for_reclaim(sibling));
        assert_eq!(
            RESOURCES[sibling].with(|r| (r.reservation_generation, r.closing)),
            before
        );
        // Explicit negative input permits reaching the physical-clear gate; not an admission.
        RESOURCES[sibling].with(|r| r.closing = true);
        assert!(std::panic::catch_unwind(|| context::clear(sibling)).is_err());
        assert!(std::panic::catch_unwind(|| task_start::clear(sibling)).is_err());
        assert!(task_start::read(sibling).is_some());
        assert!(context::read(sibling).is_some());
        RESOURCES[sibling].with(|r| r.closing = before.1);
        census.assert_matches(&intent);
        cancel(intent, Some(census));
        complete(begin_retired(sibling, ID + 1).unwrap(), handle);
    });
}
#[test]
fn actual_reclaim_first_refuses_census_until_physical_unregistration_while_original_reuse_stays_closed()
 {
    fixture(|target, sibling, handle| {
        stop(sibling, handle);
        let reuse = begin_retired(sibling, ID + 1).unwrap();
        let intent = intent(target, handle);
        assert!(ExecMembership::begin(&intent).is_none());
        let context = context::read(sibling).unwrap();
        assert!(scheduling_runtime::close_for_reclaim(
            sibling,
            context.scheduling_context.identity()
        ));
        scheduling_runtime::release(sibling, context.scheduling_context.identity());
        context::clear(sibling);
        let census = ExecMembership::begin(&intent).unwrap();
        assert!(!census.members().contains(&(sibling, ID + 1)));
        assert!(admission::reserve(sibling, ID + 2).is_none());
        assert!(RESOURCES[sibling].with(|r| r.reclaim == Some(reuse.key)));
        cancel(intent, Some(census));
        complete(reuse, handle);
    });
}
#[test]
fn actual_logical_unregister_cannot_erase_frozen_member_reclaim_exclusion() {
    fixture(|target, sibling, handle| {
        let intent = intent(target, handle);
        let census = ExecMembership::begin(&intent).unwrap();
        stop(sibling, handle);
        placement::unregister(handle, sibling, ID + 1);
        assert!(census.members().contains(&(sibling, ID + 1)));
        census.assert_matches(&intent);
        assert!(begin_retired(sibling, ID + 1).is_none());
        assert!(!close_for_reclaim(sibling));
        cancel(intent, Some(census));
        complete(begin_retired(sibling, ID + 1).unwrap(), handle);
    });
}
#[test]
fn actual_abandoned_reclaim_keeps_census_closed_without_new_execution_refs() {
    fixture(|target, sibling, handle| {
        stop(sibling, handle);
        let reuse = begin_retired(sibling, ID + 1).unwrap();
        let key = reuse.key;
        drop(reuse);
        let intent = intent(target, handle);
        assert!(ExecMembership::begin(&intent).is_none());
        assert_eq!(identity::execution::count(sibling), 0);
        // ORDERING: no additional allocation borrower was created by the census fence.
        assert_eq!(RESOURCES[sibling].pins.load(Ordering::Acquire), 0);
        assert!(begin_retired(sibling, ID + 1).is_none());
        cancel(intent, None);
        // Native owner recovery ONLY for fixture cleanup, never production reopen.
        RESOURCES[sibling].with(|r| {
            assert_eq!(r.reclaim.unwrap().phase, Phase::Poisoned);
            r.reclaim = Some(key);
        });
        complete(
            Reuse {
                slot: sibling,
                key,
                live: true,
            },
            handle,
        );
    });
}
#[test]
fn actual_terminal_census_refuses_stopped_member_reclaim_and_logical_detach_until_original_turn_release()
 {
    fixture(|target, sibling, handle| {
        stop(sibling, handle);
        let turn = placement::terminal::Turn::begin(handle, (target, ID)).unwrap();
        assert!(turn.last());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || turn.final_donation_members()
            ))
            .is_err()
        );
        assert!(begin_retired(sibling, ID + 1).is_none());
        assert!(!close_for_reclaim(sibling));
        placement::unregister(handle, sibling, ID + 1);
        assert!(begin_retired(sibling, ID + 1).is_none());
        RESOURCES[sibling].with(|r| r.closing = true);
        assert!(std::panic::catch_unwind(|| context::clear(sibling)).is_err());
        assert!(context::read(sibling).is_some());
        RESOURCES[sibling].with(|r| r.closing = false);
        drop(turn); // Original untouched election rollback, not an external stop ACK.
        complete(begin_retired(sibling, ID + 1).unwrap(), handle);
    });
}
#[test]
fn actual_reclaim_first_refuses_terminal_census_until_original_physical_unregistration() {
    fixture(|target, sibling, handle| {
        stop(sibling, handle);
        let reuse = begin_retired(sibling, ID + 1).unwrap();
        assert!(placement::terminal::Turn::begin(handle, (target, ID)).is_none());
        let context = context::read(sibling).unwrap();
        assert!(scheduling_runtime::close_for_reclaim(
            sibling,
            context.scheduling_context.identity()
        ));
        scheduling_runtime::release(sibling, context.scheduling_context.identity());
        context::clear(sibling);
        let turn = placement::terminal::Turn::begin(handle, (target, ID)).unwrap();
        assert!(turn.last());
        assert!(admission::reserve(sibling, ID + 2).is_none());
        drop(turn);
        complete(reuse, handle);
    });
}
#[test]
fn actual_overlapping_exec_and_terminal_censuses_keep_logical_tombstone_until_both_original_releases()
 {
    for exec_first in [true, false] {
        fixture(|target, sibling, handle| {
            let intent = intent(target, handle);
            let census = ExecMembership::begin(&intent).unwrap();
            stop(sibling, handle);
            let turn = placement::terminal::Turn::begin(handle, (target, ID)).unwrap();
            placement::unregister(handle, sibling, ID + 1);
            if exec_first {
                cancel(intent, Some(census));
                assert!(begin_retired(sibling, ID + 1).is_none());
                drop(turn);
            } else {
                drop(turn);
                census.assert_matches(&intent);
                assert!(begin_retired(sibling, ID + 1).is_none());
                cancel(intent, Some(census));
            }
            complete(begin_retired(sibling, ID + 1).unwrap(), handle);
        });
    }
}
#[test]
fn actual_concurrent_terminal_census_and_reclaim_admission_have_exactly_one_winner() {
    fixture(|target, sibling, handle| {
        stop(sibling, handle);
        let barrier = std::sync::Barrier::new(2);
        let (turn, reuse) = std::thread::scope(|scope| {
            let reclaimer = scope.spawn(|| {
                barrier.wait();
                begin_retired(sibling, ID + 1)
            });
            barrier.wait();
            let turn = placement::terminal::Turn::begin(handle, (target, ID));
            (turn, reclaimer.join().unwrap())
        });
        assert_ne!(turn.is_some(), reuse.is_some());
        drop(turn);
        complete(
            reuse.unwrap_or_else(|| begin_retired(sibling, ID + 1).unwrap()),
            handle,
        );
    });
}
#[test]
fn actual_final_terminal_withdraws_original_stopped_member_donation_without_directory_scan() {
    let mut bootstrap = boot();
    bootstrap.linux_thread_state = Some(LinuxThreadState::default());
    root_creation::tests::exec_root_with_release(
        ID,
        true,
        bootstrap,
        |target, handle| {
            let sibling = thread_creation::tests::active_sibling(target, ID + 1, boot());
            assert!(crate::multitask::scheduler::donation_publication::reserve_call(ID + 1).donation_reserved);
            // The original terminal consumer receipt is modeled by stop(), not the
            // complete sibling donation pipeline. Its REAL pending ledger edge is
            // intentionally retained for the actual FINAL process census engine.
            stop(sibling, handle);
            assert_eq!(donation_ledger::live_len(), 1);
            let owner =
                terminal_quarantine::begin(target, Some(ID), TaskRetireReason::Exited, false)
                    .unwrap();
            // SAFETY: native original entry/Current modeled; actual root bank and
            // original producer/process census retained through all real effects.
            unsafe {
                terminal_publication::guarded(owner, &|_| 1);
            }
            assert_eq!(donation_ledger::live_len(), 0);
            assert!(
                retirement_cleanup::test_cleanup_payload(target)
                    .unwrap()
                    .process_terminal
            );
            complete(begin_retired(sibling, ID + 1).unwrap(), handle);
        },
        terminal_publication::tests::release_published,
    );
}
#[test]
fn actual_concurrent_census_and_reclaim_admission_have_exactly_one_winner() {
    fixture(|target, sibling, handle| {
        stop(sibling, handle);
        let intent = intent(target, handle);
        let barrier = std::sync::Barrier::new(2);
        let (census, reuse) = std::thread::scope(|scope| {
            let reclaimer = scope.spawn(|| {
                barrier.wait();
                begin_retired(sibling, ID + 1)
            });
            barrier.wait();
            let census = ExecMembership::begin(&intent);
            (census, reclaimer.join().unwrap())
        });
        assert_ne!(census.is_some(), reuse.is_some());
        cancel(intent, census);
        complete(
            reuse.unwrap_or_else(|| begin_retired(sibling, ID + 1).unwrap()),
            handle,
        );
    });
}
#[test]
fn actual_exhausted_resource_nonce_does_not_install_a_process_reclaim_reservation() {
    fixture(|target, sibling, handle| {
        stop(sibling, handle);
        let original = RESOURCES[sibling].with(|r| {
            let original = r.reservation_generation;
            r.reservation_generation = u64::MAX;
            original
        });
        assert!(begin_retired(sibling, ID + 1).is_none());
        let intent = intent(target, handle);
        let census = ExecMembership::begin(&intent).unwrap();
        assert!(census.members().contains(&(sibling, ID + 1)));
        cancel(intent, Some(census));
        RESOURCES[sibling].with(|r| r.reservation_generation = original);
        complete(begin_retired(sibling, ID + 1).unwrap(), handle);
    });
}
#[test]
fn actual_physical_clear_requires_original_reclaim_nonce_before_releasing_process_or_mm() {
    fixture(|target, sibling, handle| {
        stop(sibling, handle);
        let reuse = begin_retired(sibling, ID + 1).unwrap();
        RESOURCES[sibling].with(|r| r.reclaim.as_mut().unwrap().nonce += 1);
        assert!(std::panic::catch_unwind(|| context::clear(sibling)).is_err());
        assert!(context::read(sibling).is_some());
        RESOURCES[sibling].with(|r| r.reclaim = Some(reuse.key));
        let intent = intent(target, handle);
        assert!(ExecMembership::begin(&intent).is_none());
        cancel(intent, None);
        complete(reuse, handle);
    });
}
