use super::super::super::scheduling_runtime;
use super::super::{close_for_reclaim, pin_view, take_storage, task_start, tests::admit_context};
use super::*;
const SLOT: usize = 114;
const ID: u64 = 114_001;
const CPU: usize = 2;
const WEIGHT: u32 = 120;
fn fixture(f: impl FnOnce(&StackPin)) {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let view = admit_context(SLOT, ID, None);
    runqueue::admit_blocked(SLOT);
    let pin = pin_view(SLOT, ID, view).unwrap();
    f(&pin);
    drop(pin);
    // ORDERING: native fixture models reader custody on the real counter.
    assert_eq!(RESOURCES[SLOT].readers.load(Ordering::Acquire), 0);
    assert!(close_for_reclaim(SLOT));
    task_start::clear(SLOT);
    drop(take_storage(SLOT));
}
fn claim(generation: u64) -> bool {
    runqueue::claim_dispatch_with_owner_generation(SLOT, CPU, WEIGHT, Some(generation))
}
#[test]
fn local_reader_refusal_preserves_owner_queue_and_membership_until_last_read_release() {
    fixture(|pin| {
        runqueue::publish_local(SLOT, CPU, WEIGHT);
        let owner = runqueue::owner(SLOT);
        let sequence = runqueue::local_runnable_sequence(CPU);
        let first = unsafe { acquire(pin, CPU) }.unwrap();
        let second = unsafe { acquire(pin, CPU) }.unwrap();
        assert!(!claim(owner.generation));
        assert_eq!(runqueue::owner(SLOT), owner);
        assert_eq!(runqueue::local_runnable_sequence(CPU), sequence);
        assert_eq!(runqueue::published_runnable_count(CPU), 1);
        drop(first);
        assert!(!claim(owner.generation));
        drop(second);
        assert!(claim(owner.generation));
        assert_eq!(runqueue::published_runnable_count(CPU), 0);
        assert!(
            unsafe { acquire(pin, CPU) }.is_none(),
            "Running without a bank cannot lend bytes"
        );
    });
}
#[test]
fn direct_reader_refusal_retains_exact_handoff_and_cannot_spend_a_republished_generation() {
    fixture(|pin| {
        runqueue::publish_direct_handoff(SLOT, CPU);
        let owner = runqueue::owner(SLOT);
        let reader = unsafe { acquire(pin, CPU) }.unwrap();
        assert!(!claim(owner.generation));
        assert_eq!(runqueue::owner(SLOT), owner);
        assert_eq!(runqueue::published_runnable_count(CPU), 0);
        drop(reader);
        assert!(runqueue::rollback_direct_handoff(SLOT, CPU));
        runqueue::publish_direct_handoff(SLOT, CPU);
        assert!(!claim(owner.generation));
        assert!(claim(runqueue::owner(SLOT).generation));
    });
}
#[test]
fn concurrent_reader_and_owner_cas_cannot_both_acquire_saved_header_authority() {
    fixture(|pin| {
        for _ in 0..64 {
            runqueue::reset_before_publication();
            runqueue::admit_blocked(SLOT);
            runqueue::publish_local(SLOT, CPU, WEIGHT);
            let generation = runqueue::owner(SLOT).generation;
            let start = std::sync::Barrier::new(2);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let start = &start;
                let read = scope.spawn(move || {
                    start.wait();
                    let reader = unsafe { acquire(pin, CPU) };
                    let claimed = rx.recv().unwrap();
                    let held = reader.is_some();
                    drop(reader);
                    (held, claimed)
                });
                let dispatch = scope.spawn(|| {
                    start.wait();
                    let claimed = claim(generation);
                    tx.send(claimed).unwrap(); // release peer even on counterfactual success
                    claimed
                });
                let (held, claimed) = read.join().unwrap();
                assert_eq!(dispatch.join().unwrap(), claimed);
                assert!(
                    !(held && claimed),
                    "a Running CAS crossed an admitted header reader"
                );
                if held {
                    assert!(claim(generation));
                }
            });
        }
    });
}
#[test]
fn running_reader_requires_own_current_or_prepared_bank_not_merely_a_running_owner() {
    fixture(|pin| {
        runqueue::publish_local(SLOT, CPU, WEIGHT);
        assert!(claim(runqueue::owner(SLOT).generation));
        assert!(unsafe { acquire(pin, CPU) }.is_none());
        let view = RESOURCES[SLOT].with(|r| r.binding.unwrap().1);
        let bank_pin = pin_view(
            SLOT,
            ID,
            super::super::super::context_validation::PublishedFrameView {
                user_mode_task: true,
                kernel_process_task: false,
                kernel_stack_base: view.primary_base,
                kernel_stack_top: view.primary_top,
                alternate_stack_base: 0,
                alternate_stack_top: 0,
                stack_canary_intact: true,
                scheduler_storage_base: 0,
                scheduler_storage_end: 0,
            },
        )
        .unwrap();
        super::super::cpu_leases::with_test_bank(CPU, bank_pin, true, || {
            assert!(unsafe { acquire(pin, CPU + 1) }.is_none());
            drop(unsafe { acquire(pin, CPU) }.unwrap());
        });
    });
}
#[test]
fn reader_saturation_and_writer_conflict_refuse_without_spending_any_custody() {
    fixture(|pin| {
        let owner = runqueue::owner(SLOT);
        // ORDERING: native fixture models reader custody on the real counter.
        RESOURCES[SLOT].readers.store(u32::MAX, Ordering::Release);
        assert!(unsafe { acquire(pin, CPU) }.is_none());
        assert!(
            with_claim_exclusion(SLOT, || panic!("saturated readers admitted claim")).is_none()
        );
        // ORDERING: native fixture models reader custody on the real counter.
        RESOURCES[SLOT].readers.store(0, Ordering::Release);
        RESOURCES[SLOT].with(|r| r.frame_writing = true);
        assert!(unsafe { acquire(pin, CPU) }.is_none());
        assert!(with_claim_exclusion(SLOT, || panic!("writer admitted CPU claim")).is_none());
        RESOURCES[SLOT].with(|r| r.frame_writing = false);
        assert_eq!(runqueue::owner(SLOT), owner);
    });
}
#[test]
fn reader_unwind_releases_only_its_borrow_and_reopens_exact_claim_admission() {
    fixture(|pin| {
        runqueue::publish_local(SLOT, CPU, WEIGHT);
        let generation = runqueue::owner(SLOT).generation;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _reader = unsafe { acquire(pin, CPU) }.unwrap();
                assert!(!claim(generation));
                panic!("reader callback failed");
            }))
            .is_err()
        );
        // ORDERING: native fixture models reader custody on the real counter.
        assert_eq!(RESOURCES[SLOT].readers.load(Ordering::Acquire), 0);
        assert!(claim(generation));
    });
}

#[test]
fn reserved_descriptor_never_becomes_frame_observation_authority_before_publication_ack() {
    use super::super::admission::{self, Phase};
    fixture(|pin| {
        assert!(unsafe { acquire(pin, CPU) }.is_some());
        for phase in [
            Phase::Reserved,
            Phase::Committing,
            Phase::Canceling,
            Phase::Poisoned,
        ] {
            admission::set_phase_for_test(SLOT, ID, Some(phase));
            assert!(unsafe { acquire(pin, CPU) }.is_none());
            // ORDERING: native fixture models reader custody on the real counter.
            assert_eq!(RESOURCES[SLOT].readers.load(Ordering::Acquire), 0);
        }
        admission::set_phase_for_test(SLOT, ID, None); // isolated release, no production ACK
        assert!(unsafe { acquire(pin, CPU) }.is_some());
    });
}

fn owned_fixture(f: impl FnOnce(PublishedFrameView)) {
    fixture(|_| {
        let geometry = RESOURCES[SLOT].with(|r| r.binding.unwrap().1);
        f(PublishedFrameView {
            user_mode_task: true, kernel_process_task: false,
            kernel_stack_base: geometry.primary_base, kernel_stack_top: geometry.primary_top,
            alternate_stack_base: geometry.alternate_base, alternate_stack_top: geometry.alternate_top,
            stack_canary_intact: true, scheduler_storage_base: 0, scheduler_storage_end: 0,
        });
    });
}

#[test]
fn owned_reader_uses_one_leaf_and_release_or_pin_transfer_use_none() {
    use nucleus_core::util::lockdep::{LockClass, work_budget};
    use rustos_user_abi::performance::FRAME_READ_MAX_RESOURCE_ACQUISITIONS;
    owned_fixture(|view| {
        {
            let control = work_budget::declare(LockClass::TaskStackResources, 2);
            let pin = pin_view(SLOT, ID, view).unwrap();
            let reader = unsafe { acquire(&pin, CPU) }.unwrap();
            drop(reader);
            drop(pin);
            assert_eq!(control.used(), Some(2));
        }
        let budget = work_budget::declare(LockClass::TaskStackResources, FRAME_READ_MAX_RESOURCE_ACQUISITIONS);
        let reader = unsafe { pin_and_acquire(SLOT, ID, view, CPU) }.unwrap();
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 2);
        assert_eq!(test_count(SLOT), 1);
        let pin = reader.into_pin();
        assert_eq!(test_count(SLOT), 0);
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 2);
        drop(pin);
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        assert_eq!(budget.used(), Some(1), "pin + reader + release is one resource leaf");
        drop(budget);
        let budget = work_budget::declare(LockClass::TaskStackResources, FRAME_READ_MAX_RESOURCE_ACQUISITIONS);
        drop(unsafe { pin_and_acquire(SLOT, ID, view, CPU) }.unwrap());
        assert_eq!(test_count(SLOT), 0);
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        assert_eq!(budget.used(), Some(1));
    });
}

#[test]
fn owned_reader_rechecks_frame_epoch_without_leaking_combined_custody() {
    use super::super::super::frame_publication;
    owned_fixture(|view| {
        let publish = || frame_publication::publish(
            SLOT, view.kernel_stack_top as usize - super::super::super::SAVED_CONTEXT_BYTES,
            view.kernel_stack_base, view.kernel_stack_top, 0, 0, true, false, true, 0, 0,
        );
        publish();
        let old = frame_publication::test_read(SLOT).unwrap();
        drop(unsafe { frame_publication::read_for_identity(old, ID, CPU) }.unwrap());
        publish();
        assert!(unsafe { frame_publication::read_for_identity(old, ID, CPU) }.is_none());
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        assert_eq!(test_count(SLOT), 0);
        let new = frame_publication::test_read(SLOT).unwrap();
        drop(unsafe { frame_publication::read_for_identity(new, ID, CPU) }.unwrap());
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        assert_eq!(test_count(SLOT), 0);
        frame_publication::clear(SLOT);
    });
}

#[test]
fn owned_reader_refusals_do_not_leak_pin_reader_or_owner_custody() {
    owned_fixture(|view| {
        let owner = runqueue::owner(SLOT);
        let refused = |id, view| {
            assert!(unsafe { pin_and_acquire(SLOT, id, view, CPU) }.is_none());
            assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
            assert_eq!(test_count(SLOT), 0);
            assert_eq!(runqueue::owner(SLOT), owner);
        };
        refused(ID + 1, view);
        refused(ID, PublishedFrameView { kernel_stack_base: view.kernel_stack_base + 8, ..view });
        RESOURCES[SLOT].with(|r| r.closing = true);
        refused(ID, view);
        RESOURCES[SLOT].with(|r| { r.closing = false; r.frame_writing = true; });
        refused(ID, view);
        RESOURCES[SLOT].with(|r| r.frame_writing = false);
        use super::super::admission::{self, Phase};
        for phase in [Phase::Reserved, Phase::Committing, Phase::Canceling, Phase::Poisoned] {
            admission::set_phase_for_test(SLOT, ID, Some(phase));
            refused(ID, view);
        }
        admission::set_phase_for_test(SLOT, ID, None);
        // ORDERING: native fixture models saturated reader custody on the real counter.
        RESOURCES[SLOT].readers.store(u32::MAX, Ordering::Release);
        assert!(unsafe { pin_and_acquire(SLOT, ID, view, CPU) }.is_none());
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        assert_eq!(test_count(SLOT), u32::MAX);
        // ORDERING: fixture removes only its synthetic reader saturation.
        RESOURCES[SLOT].readers.store(0, Ordering::Release);
        runqueue::publish_local(SLOT, CPU, WEIGHT);
        assert!(claim(runqueue::owner(SLOT).generation));
        assert!(unsafe { pin_and_acquire(SLOT, ID, view, CPU) }.is_none());
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        assert_eq!(test_count(SLOT), 0);
    });
}

#[test]
fn owned_reader_claim_race_and_unwind_keep_backing_and_exclusion_together() {
    owned_fixture(|view| {
        for _ in 0..64 {
            runqueue::reset_before_publication();
            runqueue::admit_blocked(SLOT);
            runqueue::publish_local(SLOT, CPU, WEIGHT);
            let generation = runqueue::owner(SLOT).generation;
            let start = std::sync::Barrier::new(2);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let start = &start;
                let read = scope.spawn(move || {
                    start.wait();
                    let reader = unsafe { pin_and_acquire(SLOT, ID, view, CPU) };
                    let claimed = rx.recv().unwrap();
                    assert!(!(reader.is_some() && claimed), "owner CAS crossed combined reader admission");
                    drop(reader);
                });
                let dispatch = scope.spawn(|| {
                    start.wait();
                    tx.send(claim(generation)).unwrap();
                });
                read.join().unwrap();
                dispatch.join().unwrap();
            });
            assert_eq!(test_count(SLOT), 0);
            assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        }
        runqueue::reset_before_publication();
        runqueue::admit_blocked(SLOT);
        runqueue::publish_local(SLOT, CPU, WEIGHT);
        let generation = runqueue::owner(SLOT).generation;
        assert!(std::panic::catch_unwind(|| {
            let _reader = unsafe { pin_and_acquire(SLOT, ID, view, CPU) }.unwrap();
            assert!(!claim(generation));
            panic!("combined reader callback failed");
        }).is_err());
        assert_eq!(test_count(SLOT), 0);
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        assert!(claim(generation));
    });
}
