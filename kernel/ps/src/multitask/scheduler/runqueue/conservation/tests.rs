//! Witnesses of the custody conservation scan.
use super::*;
use std::vec::Vec;
use crate::multitask::scheduler::runqueue::{
    admit_blocked, ownership::test_force_word, owner, publish_local, publish_remote_wake,
    release_retired, reset_before_publication, retire, test_serial_guard, RemoteWakeOutcome,
    RunOwnerSnapshot,
};

fn reports(now: u64) -> Vec<CustodyReport> {
    let mut found = Vec::new();
    scan_slots(now, |report| found.push(report));
    found
}

#[test]
fn a_runnable_word_whose_custody_record_is_gone_is_reported_once_after_the_threshold() {
    const SLOT: usize = 90;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    // A RemoteQueued word with no mailbox record: the shape of a lost enqueue.
    let blocked = owner(SLOT);
    let stranded = RunOwnerSnapshot::new(RunOwnerState::RemoteQueued, Some(5), blocked.generation + 1);
    test_force_word(SLOT, blocked, stranded).unwrap();
    assert!(reports(1000).iter().all(|report| report.slot != SLOT), "reported before any age");
    assert!(reports(1000 + THRESHOLD_TICKS - 1).iter().all(|report| report.slot != SLOT));
    let found: Vec<_> = reports(1000 + THRESHOLD_TICKS)
        .into_iter()
        .filter(|report| report.slot == SLOT)
        .collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].gap, CustodyGap::RemoteWithoutMailbox);
    assert_eq!(found[0].age_ticks, THRESHOLD_TICKS);
    assert!(
        reports(1000 + 4 * THRESHOLD_TICKS).iter().all(|report| report.slot != SLOT),
        "the same word was reported twice"
    );
    retire(SLOT, 100);
    release_retired(SLOT);
    reset_before_publication();
}

#[test]
fn queued_mailboxed_and_blocked_tasks_are_conserved() {
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(91);
    admit_blocked(92);
    publish_local(92, 1, 100);
    admit_blocked(93);
    assert!(matches!(publish_remote_wake(93, 4, 100), RemoteWakeOutcome::Published { .. }));
    for now in [10, 10 + THRESHOLD_TICKS, 10 + 2 * THRESHOLD_TICKS] {
        let found = reports(now);
        assert!(
            found.iter().all(|report| !(91..=93).contains(&report.slot)),
            "a conserved task was reported: {found:?}"
        );
    }
    reset_before_publication();
}

#[test]
fn unrelated_mailbox_work_cannot_hide_a_lost_remote_owner_record() {
    const GOOD: usize = 96;
    const LOST: usize = 97;
    const CPU: usize = 4;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(GOOD);
    admit_blocked(LOST);
    assert!(matches!(
        publish_remote_wake(GOOD, CPU, 100),
        RemoteWakeOutcome::Published { .. }
    ));
    // A different RemoteQueued task has no mailbox receipt, but GOOD still
    // holds the shared pending edge for CPU. The old scan missed LOST forever.
    let blocked = owner(LOST);
    let stranded =
        RunOwnerSnapshot::new(RunOwnerState::RemoteQueued, Some(CPU), blocked.generation + 1);
    test_force_word(LOST, blocked, stranded).unwrap();
    assert!(reports(200).iter().all(|r| r.slot != LOST));
    let found = reports(200 + THRESHOLD_TICKS);
    assert!(found.iter().any(|r| r.slot == LOST && r.gap == CustodyGap::RemoteWithoutMailbox));
    assert!(found.iter().all(|r| r.slot != GOOD));
    retire(GOOD, 100);
    retire(LOST, 100);
    release_retired(GOOD);
    release_retired(LOST);
    reset_before_publication();
}

#[test]
fn suppressed_diagnostics_never_spend_the_global_report_budget() {
    let _serial = test_serial_guard();
    reset_before_publication();
    assert!(!reserve_report_slot(MAX_REPORTS_PER_SCAN));
    assert_eq!(REPORTS.load(Ordering::Relaxed), 0);
    for emitted in 0..MAX_REPORTS_PER_SCAN {
        assert!(reserve_report_slot(emitted));
    }
    assert!(!reserve_report_slot(MAX_REPORTS_PER_SCAN));
    assert_eq!(REPORTS.load(Ordering::Relaxed), MAX_REPORTS_PER_SCAN);
    for _ in MAX_REPORTS_PER_SCAN..MAX_REPORTS_PER_BOOT {
        assert!(reserve_report_slot(0));
    }
    assert!(!reserve_report_slot(0));
    assert_eq!(REPORTS.load(Ordering::Relaxed), MAX_REPORTS_PER_BOOT);
    reset_before_publication();
}

#[test]
fn a_custody_change_restarts_the_age() {
    const SLOT: usize = 94;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    let blocked = owner(SLOT);
    let migrating = RunOwnerSnapshot::new(RunOwnerState::Migrating, Some(2), blocked.generation + 1);
    test_force_word(SLOT, blocked, migrating).unwrap();
    assert!(reports(0).iter().all(|report| report.slot != SLOT));
    let moved = RunOwnerSnapshot::new(RunOwnerState::Migrating, Some(3), migrating.generation + 1);
    test_force_word(SLOT, migrating, moved).unwrap();
    assert!(reports(THRESHOLD_TICKS).iter().all(|report| report.slot != SLOT));
    assert!(reports(2 * THRESHOLD_TICKS).iter().any(|report| report.slot == SLOT
        && report.gap == CustodyGap::MigratingStuck));
    reset_before_publication();
}

#[test]
fn a_direct_handoff_without_its_fifo_record_is_reported_after_the_threshold() {
    const SLOT: usize = 95;
    let _serial = test_serial_guard();
    reset_before_publication();
    admit_blocked(SLOT);
    // The fixture commits DirectHandoff and discharges the duty without
    // publishing a FIFO record: the shape of a lost handoff enqueue.
    assert!(matches!(
        crate::multitask::scheduler::runqueue::publish_direct_handoff(SLOT, 6),
        RemoteWakeOutcome::Published { .. }
    ));
    assert!(reports(0).iter().all(|report| report.slot != SLOT));
    let found: Vec<_> = reports(THRESHOLD_TICKS).into_iter().filter(|r| r.slot == SLOT).collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].gap, CustodyGap::DirectWithoutFifo);
    retire(SLOT, 100);
    release_retired(SLOT);
    reset_before_publication();
}
