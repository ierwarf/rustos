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
    assert_eq!(RESOURCES[SLOT].with(|r| r.frame_readers), 0);
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
        RESOURCES[SLOT].with(|r| r.frame_readers = u32::MAX);
        assert!(unsafe { acquire(pin, CPU) }.is_none());
        assert!(
            with_claim_exclusion(SLOT, || panic!("saturated readers admitted claim")).is_none()
        );
        RESOURCES[SLOT].with(|r| {
            r.frame_readers = 0;
            r.frame_writing = true;
        });
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
        assert_eq!(RESOURCES[SLOT].with(|r| r.frame_readers), 0);
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
            assert_eq!(RESOURCES[SLOT].with(|r| r.frame_readers), 0);
        }
        admission::set_phase_for_test(SLOT, ID, None); // isolated release, no production ACK
        assert!(unsafe { acquire(pin, CPU) }.is_some());
    });
}
