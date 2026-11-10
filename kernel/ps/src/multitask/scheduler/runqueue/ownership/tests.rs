//! Witnesses of the owner transition table, the enqueue duty (I1), settled
//! words (I2) and the generation rule (I3).
use super::*;
use std::vec::Vec;
use crate::multitask::scheduler::runqueue::{
    self, admit_blocked, drain_remote_wakes, is_local_dispatchable, publish_direct_handoff,
    publish_remote_wake, release_retired, reset_before_publication, retire, test_serial_guard,
    RemoteWakeOutcome,
};
use std::panic::{catch_unwind, AssertUnwindSafe};

const RUNNABLE_DUTIES: [DutyKind; 4] = [
    DutyKind::RunQueue,
    DutyKind::Mailbox,
    DutyKind::HandoffFifo,
    DutyKind::Migration,
];

/// A word in `state` whose CPU field is the one the state requires.
fn word_in(state: RunOwnerState, generation: u64) -> RunOwnerSnapshot {
    let cpu = match state {
        RunOwnerState::Dormant | RunOwnerState::Blocked | RunOwnerState::Retired => None,
        _ => Some(1),
    };
    RunOwnerSnapshot::new(state, cpu, generation)
}

fn force(slot: usize, state: RunOwnerState) -> RunOwnerSnapshot {
    let before = owner(slot);
    let forced = word_in(state, before.generation + 1);
    test_force_word(slot, before, forced).unwrap();
    forced
}

#[test]
fn every_state_event_pair_has_at_most_one_row_and_rows_follow_the_duty_rule() {
    for state in RunOwnerState::ALL {
        for event in OwnerEvent::ALL {
            let rows = TRANSITIONS
                .iter()
                .filter(|row| row.from == state && row.event == event)
                .count();
            assert!(rows <= 1, "{state:?} x {event:?} has {rows} rows");
        }
    }
    for row in TRANSITIONS {
        // I1: entering runnable custody that is not CPU execution owes exactly
        // one enqueue; entering anything else owes none (LaneClaim's cell take
        // is the one non-enqueue duty, and only it skips the residency hook).
        let into_queue = row.to.holds_run_custody() && row.to != RunOwnerState::Running;
        assert_eq!(
            into_queue,
            RUNNABLE_DUTIES.contains(&row.duty),
            "{:?} --{:?}--> {:?} has duty {:?}",
            row.from,
            row.event,
            row.to,
            row.duty
        );
        assert_eq!(row.duty == DutyKind::ResidentTake, row.event == OwnerEvent::LaneClaim);
        // I2: settled states have exactly one exit, the exact-value CAS.
        if matches!(row.from, RunOwnerState::Retiring | RunOwnerState::Retired) {
            assert!(matches!(row.event, OwnerEvent::CompleteRetire | OwnerEvent::Release));
        }
        if row.from == RunOwnerState::Dormant {
            assert!(matches!(row.event, OwnerEvent::Admit | OwnerEvent::AdmitRunning));
        }
    }
    // Every live state can be withdrawn; settled ones cannot.
    for state in RunOwnerState::ALL {
        assert_eq!(lookup(state, OwnerEvent::Withdraw).is_some(), state.is_editable(), "{state:?}");
    }
    // Every state is reachable from Dormant through the table.
    let mut reached = [false; 9];
    reached[RunOwnerState::Dormant as usize] = true;
    for _ in 0..TRANSITIONS.len() {
        for row in TRANSITIONS {
            if reached[row.from as usize] {
                reached[row.to as usize] = true;
            }
        }
    }
    assert!(reached.iter().all(|reached| *reached), "unreachable owner state {reached:?}");
}

#[test]
fn the_table_is_exhaustive_over_live_words_every_row_advances_one_generation_and_others_panic() {
    const SLOT: usize = 101;
    let _serial = test_serial_guard();
    reset_before_publication();
    for state in RunOwnerState::ALL {
        for event in OwnerEvent::ALL {
            let expected = force(SLOT, state).with_wait(false, 2);
            test_force_word(SLOT, owner(SLOT), expected).unwrap();
            match lookup(state, event) {
                Some(row) => {
                    let committed = transition(SLOT, expected, event, Some(3))
                        .unwrap_or_else(|observed| panic!("{state:?} x {event:?} lost to {observed:?}"));
                    let next = committed.next;
                    assert_eq!(next.generation, expected.generation + 1, "I3 {state:?} x {event:?}");
                    assert_eq!(next.state, row.to);
                    assert_eq!(next.runnable, row.to.holds_run_custody());
                    assert_eq!(
                        next.cpu,
                        match row.cpu {
                            CpuRule::None => None,
                            CpuRule::Keep => expected.cpu,
                            CpuRule::Target => Some(3),
                        }
                    );
                    let kept = if row.wait == WaitRule::Keep { 2 } else { 0 };
                    assert_eq!(next.wait_reason_kind, kept, "{state:?} x {event:?} wait rule");
                    assert_eq!(owner(SLOT), next);
                    match row.duty {
                        DutyKind::None => {
                            let _ = committed.settled();
                        }
                        kind => {
                            let duty = committed.into_duty();
                            assert_eq!((duty.slot(), duty.generation(), duty.kind()), (SLOT, next.generation, kind));
                            duty.discharge(kind);
                        }
                    }
                }
                None => {
                    let refused = catch_unwind(AssertUnwindSafe(|| {
                        let _ = transition(SLOT, expected, event, Some(3));
                    }));
                    assert!(refused.is_err(), "{state:?} x {event:?} is not a row but was accepted");
                    assert_eq!(owner(SLOT), expected, "{state:?} x {event:?} wrote the word");
                }
            }
        }
    }
    reset_before_publication();
}

#[test]
fn a_dropped_enqueue_duty_panics_instead_of_stranding_the_task() {
    const SLOT: usize = 102;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    let blocked = owner(SLOT);
    let lost = catch_unwind(AssertUnwindSafe(|| {
        let committed = transition(SLOT, blocked, OwnerEvent::WakeRemote, Some(2)).unwrap();
        drop(committed);
    }));
    assert!(lost.is_err(), "a mailbox duty was dropped silently (I1)");
    let ignored = catch_unwind(AssertUnwindSafe(|| {
        let committed = transition(SLOT, owner(SLOT), OwnerEvent::Rehome, Some(3)).unwrap();
        let _ = committed.settled();
    }));
    assert!(ignored.is_err(), "a migration duty was treated as duty-free (I1)");
    reset_before_publication();
}

#[test]
fn a_second_withdrawal_waits_for_the_first_ones_exact_completion() {
    // Regression (I2): `retire` on a Retiring word used to withdraw it again
    // (Retiring -> Retiring'), so the first withdrawal's exact completion CAS
    // lost and panicked "scheduler retire completion raced".
    const SLOT: usize = 103;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    let retiring = transition(SLOT, owner(SLOT), OwnerEvent::Withdraw, None)
        .unwrap()
        .settled();
    let second = std::thread::spawn(|| retire(SLOT, 100));
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(owner(SLOT), retiring, "a second withdrawal rewrote a Retiring word");
    let _ = transition(SLOT, retiring, OwnerEvent::CompleteRetire, None)
        .expect("the first withdrawal's exact completion lost")
        .settled();
    second.join().unwrap();
    assert_eq!(owner(SLOT).state, RunOwnerState::Retired);
    release_retired(SLOT);
    reset_before_publication();
}

#[test]
fn withdrawing_a_never_admitted_slot_is_not_a_table_row() {
    const SLOT: usize = 104;
    let _serial = test_serial_guard();
    reset_before_publication();
    let dormant = owner(SLOT);
    assert!(catch_unwind(AssertUnwindSafe(|| retire(SLOT, 100))).is_err());
    assert_eq!(owner(SLOT), dormant);
    reset_before_publication();
}

#[test]
fn run_intent_is_never_raised_on_a_blocked_word() {
    // Regression (I1): a Blocked word with run intent claims "already
    // runnable" to every wake classifier while no queue holds it.
    const SLOT: usize = 105;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    let blocked = owner(SLOT);
    assert!(catch_unwind(AssertUnwindSafe(|| set_runnable(SLOT, true))).is_err());
    // A wake of a Blocked word clears its wait and leaves run intent to the
    // wake's custody transition.
    runqueue::wait::set_blocked(SLOT, false);
    assert_eq!(owner(SLOT), blocked, "run intent was raised without custody");
    retire(SLOT, 100);
    release_retired(SLOT);
    reset_before_publication();
}

#[test]
fn a_direct_handoff_keeps_run_intent_so_its_consumed_record_always_materializes() {
    // Regression (I1): lowering run intent on a DirectHandoff word made
    // `materialize_direct_handoff_with_generation` refuse its consumed FIFO
    // record, leaving a DirectHandoff owner in no queue; a later wake raised
    // run intent again on a task no dispatcher could find.
    const SLOT: usize = 106;
    const CPU: usize = 2;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    assert!(matches!(publish_direct_handoff(SLOT, CPU), RemoteWakeOutcome::Published { .. }));
    let direct = owner(SLOT);
    assert!(catch_unwind(AssertUnwindSafe(|| runqueue::wait::set_blocked(SLOT, true))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| set_runnable(SLOT, false))).is_err());
    assert_eq!(owner(SLOT), direct, "run intent was withdrawn from a DirectHandoff word");
    assert!(runqueue::materialize_direct_handoff_with_generation(
        SLOT,
        CPU,
        100,
        Some(direct.generation)
    ));
    assert!(is_local_dispatchable(SLOT, CPU));
    retire(SLOT, 100);
    release_retired(SLOT);
    reset_before_publication();
}

#[test]
fn queued_run_intent_may_be_withdrawn_in_place_and_keeps_its_membership() {
    const SLOT: usize = 107;
    const CPU: usize = 1;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    runqueue::publish_local(SLOT, CPU, 100);
    set_runnable(SLOT, false);
    assert!(!owner(SLOT).runnable);
    assert!(runqueue::published_local_membership(SLOT, CPU));
    set_runnable(SLOT, true);
    assert!(is_local_dispatchable(SLOT, CPU));
    retire(SLOT, 100);
    release_retired(SLOT);
    reset_before_publication();
}

#[test]
fn the_flight_ring_holds_the_last_transitions_newest_first() {
    const SLOT: usize = 108;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    assert!(matches!(publish_remote_wake(SLOT, 3, 100), RemoteWakeOutcome::Published { cpu: 3, .. }));
    assert_eq!(drain_remote_wakes(3), 1);
    let (ring, len) = flight_ring(SLOT);
    let decoded: Vec<_> = ring[..len]
        .iter()
        .map(|entry| (entry.old_state(), entry.next_state(), entry.event()))
        .collect();
    assert_eq!(
        decoded,
        [
            (RunOwnerState::RemoteQueued, RunOwnerState::Local, OwnerEvent::Adopt as u8),
            (RunOwnerState::Blocked, RunOwnerState::RemoteQueued, OwnerEvent::WakeRemote as u8),
            (RunOwnerState::Dormant, RunOwnerState::Blocked, OwnerEvent::Admit as u8),
        ]
    );
    assert_eq!(ring[0].actor(), ACTOR_IS_TARGET);
    // The ring keeps only the newest FLIGHT_DEPTH generations.
    for round in 0..FLIGHT_DEPTH {
        let target = 2 + round % 2;
        assert!(matches!(runqueue::rehome_queued(SLOT, target, 100), RemoteWakeOutcome::Published { .. }));
        assert_eq!(drain_remote_wakes(target), 1);
    }
    let (_, len) = flight_ring(SLOT);
    assert_eq!(len, FLIGHT_DEPTH);
    retire(SLOT, 100);
    release_retired(SLOT);
    reset_before_publication();
}
