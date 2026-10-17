//! Owner: isolated native diagnostic-owner regression witnesses.
//! Boundary: real counter cells/history/epoch, not catalog fixture counters.
//! Lifecycle: shared fixture isolates model globals; never a producer grant.
//! Concurrency: native fixture order is table → CPU → runtime → runqueue.
//! Failure: tests assert refusal, consumption, saturation and exact tuples.
//! Forbidden: no native recovery is hardware ACK or guard-deletion proof.
//! Evidence: bounded native witnesses and exact implementation mutants.
use super::*;
static REPORTERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn cpu_cells_drain_independently_and_merge_saturates_without_spending_next_window() {
    let _fixture = crate::multitask::scheduler::tests::boxed_scheduler();
    let a = Cell::new();
    let b = Cell::new();
    a.with(|row| {
        row.account(7, u64::MAX);
        row.dispatch(7);
        row.wait(20);
    });
    b.with(|row| {
        row.account(7, 1);
        row.dispatch(7);
        row.wait(40);
    });
    let mut window = a.with(Counters::take);
    window.merge(b.with(Counters::take));
    assert_eq!(window.runtime_profile_ns[7], u64::MAX);
    assert_eq!(window.runtime_profile_dispatches[7], 2);
    assert_eq!(window.runtime_profile_lock_wait_ns, 60);
    assert_eq!(window.runtime_profile_lock_wait_max_ns, 40);
    assert_eq!(window.runtime_profile_lock_acquisitions, 2);
    a.with(|row| row.account(7, 13));
    assert_eq!(b.with(Counters::take).runtime_profile_ns[7], 0);
    assert_eq!(a.with(Counters::take).runtime_profile_ns[7], 13);
    assert_eq!(a.with(Counters::take).runtime_profile_ns[7], 0);
}
#[test]
fn remote_window_drain_preserves_open_turn_phase_baseline_and_new_window_segments() {
    let _fixture = crate::multitask::scheduler::tests::boxed_scheduler();
    let cell = Cell::new();
    cell.with(|row| row.phase(SchedulerPhase::Prologue, 100));
    let baseline = cell.with(|row| row.phase_lifetime_ns);
    let old = cell.with(Counters::take);
    assert_eq!(
        old.runtime_profile_phase_ns[SchedulerPhase::Prologue as usize],
        100
    );
    assert_eq!(cell.with(|row| row.phase_lifetime_ns), baseline);
    cell.with(|row| row.phase(SchedulerPhase::Prologue, 30));
    assert_eq!(
        cell.with(|row| row.phase_lifetime_ns.saturating_sub(baseline)),
        30
    );
    let new = cell.with(Counters::take);
    assert_eq!(
        new.runtime_profile_phase_ns[SchedulerPhase::Prologue as usize],
        30
    );
    assert_eq!(cell.with(|row| row.phase_lifetime_ns), 130);
}
#[test]
fn worst_turn_merge_keeps_caller_and_attribution_with_exact_maximum_not_other_cpu() {
    let _fixture = crate::multitask::scheduler::tests::boxed_scheduler();
    let a = Cell::new();
    let b = Cell::new();
    let site_a = Location::caller();
    a.with(|row| row.hold(300, 51, site_a));
    let site_b = Location::caller();
    b.with(|row| row.hold(200, 99, site_b));
    let mut window = b.with(Counters::take);
    window.merge(a.with(Counters::take));
    assert_eq!(window.runtime_profile_lock_hold_ns, 500);
    assert_eq!(window.runtime_profile_lock_hold_max_ns, 300);
    assert_eq!(window.runtime_profile_lock_hold_max_attributed_ns, 51);
    assert!(core::ptr::eq(
        window.runtime_profile_lock_hold_max_caller.unwrap(),
        site_a
    ));
    let empty = a.with(Counters::take);
    assert_eq!(empty.runtime_profile_lock_hold_max_ns, 0);
    assert!(empty.runtime_profile_lock_hold_max_caller.is_none());
}
#[test]
fn per_slot_migration_history_distinguishes_root_uid_zero_reuse_and_cpus() {
    let _fixture = crate::multitask::scheduler::tests::boxed_scheduler();
    // Dedicated synthetic slot; no Scheduler/fixture table or CPU grant.
    let slot = MAX_TASK - 1;
    clear_history(slot);
    assert!(!migration(slot, 0, 0));
    assert!(migration(slot, 0, 1));
    assert!(!migration(slot, 0, 1));
    assert!(!migration(slot, 1, 0));
    assert!(migration(slot, 1, 1));
    clear_history(slot);
    assert!(!migration(slot, 1, 2));
    clear_history(slot);
}
#[test]
fn busy_reporter_refuses_without_consuming_cpu_counters_or_reopening_custody() {
    let _fixture = crate::multitask::scheduler::tests::boxed_scheduler();
    let _serial = REPORTERS.lock().unwrap();
    // ORDERING: fixture claims the same exclusive collector flag as production.
    assert!(
        DRAINING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    );
    let drain = Drain;
    let cell = Cell::new();
    cell.with(|row| row.dispatch(8));
    // ORDERING: busy refusal must preserve the previously published epoch.
    let started = STARTED.load(Ordering::Acquire);
    assert!(take(u64::MAX, 1).is_none());
    assert_eq!(STARTED.load(Ordering::Acquire), started);
    assert!(DRAINING.load(Ordering::Acquire));
    assert_eq!(cell.with(Counters::take).runtime_profile_dispatches[8], 1);
    drop(drain);
    // ORDERING: collector Drop release reopens the exact flag.
    assert!(!DRAINING.load(Ordering::Acquire));
}

#[test]
fn report_epoch_admits_one_due_consumer_and_preserves_partial_period_on_every_cpu() {
    let _fixture = crate::multitask::scheduler::tests::boxed_scheduler();
    let _serial = REPORTERS.lock().unwrap();
    // ORDERING: fixture reset precedes the first admitted epoch.
    STARTED.store(0, Ordering::Release);
    for cell in &CELLS {
        cell.with(|row| *row = Counters::new());
    }
    assert!(take(1, 100).is_none());
    CELLS[0].with(|row| row.account(7, 10));
    CELLS[1].with(|row| row.account(7, 20));
    assert!(take(100, 100).is_none());
    // ORDERING: not-due refusal retains the published timestamp.
    assert_eq!(STARTED.load(Ordering::Acquire), 1);
    assert_eq!(CELLS[1].with(|row| row.runtime_profile_ns[7]), 20);
    let first = take(101, 100).unwrap();
    assert_eq!(first.runtime_profile_started_ticks, 1);
    assert_eq!(first.runtime_profile_ns[7], 30);
    CELLS[1].with(|row| row.account(7, 9));
    assert!(take(101, 100).is_none());
    assert_eq!(CELLS[1].with(|row| row.runtime_profile_ns[7]), 9);
    let second = take(201, 100).unwrap();
    assert_eq!(second.runtime_profile_started_ticks, 101);
    assert_eq!(second.runtime_profile_ns[7], 9);
    assert_eq!(CELLS[1].with(|row| row.runtime_profile_ns[7]), 0);
    assert!(take(200, 100).is_none());
    // ORDERING: isolated fixture resets only after the final collection completed.
    STARTED.store(0, Ordering::Release);
}
