//! Unit and conservation tests for per-CPU paired execution descriptors (Track L2).
//!
//! - **Owner:** tests exercise `scheduler::pair` custody helpers under serialized
//!   scheduler/runqueue test guards.
//! - **Boundary:** tests validate conservation invariants, `Paired` park/resume,
//!   break requests, and withdrawal against `runqueue::ownership` without mocking
//!   out real owner transitions.
//! - **Lifecycle:** each test resets publication state cleanly before verifying
//!   park, resume, break, and retirement transitions.
//! - **Concurrency:** serialized via `test_publication_lock`, `test_serial_guard`,
//!   and `runqueue::test_serial_guard` to isolate per-CPU pair globals in unit tests.
//! - **Failure:** any conservation gap, stale generation, or orphaned `Paired`
//!   task causes an immediate assertion failure.
//! - **Forbidden:** no production code changes solely to bypass `Paired` checks,
//!   and no shared mutable test state leaking across cases.
//! - **Evidence:** `scheduler-owner-word/SchedulerOwnerWord` and `cargo test -p
//!   kernel-ps --lib -- pair`.

use alloc::vec::Vec;
use super::*;
use super::super::runqueue::{self, RetireOutcome, RemoteWakeOutcome};
use super::super::runqueue::conservation::{self, CustodyGap, CustodyReport, THRESHOLD_TICKS};
use super::super::runqueue::ownership;
use super::super::scheduling_runtime;

fn custody_reports(now_ticks: u64) -> Vec<CustodyReport> {
    let mut found = Vec::new();
    conservation::scan_slots(now_ticks, |report| found.push(report));
    found
}

#[test]
fn conservation_accepts_live_paired_partner_and_flags_orphaned_paired() {
    const PARTNER: usize = 92;
    const CPU: usize = 2;
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _runtime = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();

    runqueue::admit_blocked(PARTNER);
    let blocked = runqueue::owner(PARTNER);
    let paired = runqueue::pair_claim(PARTNER, blocked, CPU).unwrap();
    assert_eq!(paired.state, RunOwnerState::Paired);
    assert_eq!(paired.cpu, Some(CPU));
    assert!(!paired.runnable);

    let ctrl = &PAIR_CONTROLS[CPU];
    ctrl.partner_slot_plus1.store(PARTNER as u64 + 1, Ordering::Release);
    ctrl.partner_owner_gen.store(paired.generation, Ordering::Release);
    ctrl.state
        .store(PairControl::pack(PHASE_LIVE, 0, 1), Ordering::Release);

    assert!(holds_partner(CPU, PARTNER, paired.generation));
    assert!(!holds_partner(CPU, PARTNER, paired.generation + 1));
    assert!(custody_reports(0).iter().all(|r| r.slot != PARTNER));
    assert!(
        custody_reports(THRESHOLD_TICKS)
            .iter()
            .all(|r| r.slot != PARTNER)
    );

    // Clear descriptor while keeping owner word at Paired: custody gap must fire after threshold.
    ctrl.partner_slot_plus1.store(0, Ordering::Release);
    ctrl.partner_owner_gen.store(0, Ordering::Release);
    ctrl.state.store(PHASE_VACANT, Ordering::Release);
    assert!(!holds_partner(CPU, PARTNER, paired.generation));
    assert!(custody_reports(100).iter().all(|r| r.slot != PARTNER));
    let found = custody_reports(100 + THRESHOLD_TICKS);
    assert!(
        found
            .iter()
            .any(|r| r.slot == PARTNER && r.gap == CustodyGap::PairedWithoutDescriptor),
        "expected PairedWithoutDescriptor in {found:?}"
    );

    runqueue::reset_before_publication();
}

#[test]
fn paired_custody_guards_wake_wait_and_redirects_wake_affinity_and_retire_to_break_requests() {
    const PARTNER: usize = 93;
    const CPU: usize = 1;
    const WEIGHT: u32 = 100;
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _runtime = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();

    runqueue::admit_blocked(PARTNER);
    let blocked = runqueue::owner(PARTNER);
    let paired = runqueue::pair_claim(PARTNER, blocked, CPU).unwrap();

    let ctrl = &PAIR_CONTROLS[CPU];
    ctrl.partner_slot_plus1.store(PARTNER as u64 + 1, Ordering::Release);
    ctrl.partner_owner_gen.store(paired.generation, Ordering::Release);
    ctrl.state
        .store(PairControl::pack(PHASE_LIVE, 0, 5), Ordering::Release);

    // 1. Direct set_blocked(false) / wake_wait must refuse to corrupt Paired state.
    runqueue::wait::set_blocked(PARTNER, false);
    assert_eq!(runqueue::owner(PARTNER), paired);

    // 2. Stale-generation break requests cannot raise FLAG_BREAK_REQUESTED.
    request_break_with_wake(CPU, PARTNER, paired.generation + 9);
    assert!(!break_requested(CPU));
    request_break_for_affinity(CPU, PARTNER, paired.generation + 9);
    assert!(!break_requested(CPU));
    request_break_for_retire(CPU, PARTNER, paired.generation + 9);
    assert!(!break_requested(CPU));

    // 3. Exact-generation local and remote wakes return Rejected and set FLAG_BREAK_REQUESTED | FLAG_WAKE_PARTNER.
    for wake_fn in [
        runqueue::publish_local_wake as fn(usize, usize, u32) -> RemoteWakeOutcome,
        runqueue::publish_remote_wake,
    ] {
        ctrl.state
            .store(PairControl::pack(PHASE_LIVE, 0, 5), Ordering::Release);
        assert!(!break_requested(CPU));
        assert!(matches!(
            wake_fn(PARTNER, CPU, WEIGHT),
            RemoteWakeOutcome::Rejected
        ));
        let word = ctrl.state.load(Ordering::Acquire);
        assert!(break_requested(CPU));
        assert_ne!(word & FLAG_WAKE_PARTNER, 0);
        assert_eq!(runqueue::owner(PARTNER), paired);
    }

    // 4. Affinity migration break requests set FLAG_BREAK_REQUESTED without FLAG_WAKE_PARTNER.
    ctrl.state
        .store(PairControl::pack(PHASE_LIVE, 0, 5), Ordering::Release);
    request_break_for_affinity(CPU, PARTNER, paired.generation);
    let word = ctrl.state.load(Ordering::Acquire);
    assert!(break_requested(CPU));
    assert_eq!(word & FLAG_WAKE_PARTNER, 0);

    // 5. Waiting retire_unless_claim_in_flight defers withdrawal via ClaimInFlight and sets FLAG_BREAK_REQUESTED.
    ctrl.state
        .store(PairControl::pack(PHASE_LIVE, 0, 5), Ordering::Release);
    assert_eq!(
        runqueue::retire_unless_claim_in_flight(PARTNER, WEIGHT),
        RetireOutcome::ClaimInFlight
    );
    assert!(break_requested(CPU));
    assert_eq!(runqueue::owner(PARTNER).state, RunOwnerState::Paired);

    // 6. Dissolving to Blocked restores exact wait reason and lets retire proceed.
    let dissolved = runqueue::wait::dissolve_paired_to_blocked(PARTNER, RECEIVE, 0x4455, || {
        runqueue::pair_break_blocked(PARTNER, paired).unwrap()
    });
    assert_eq!(dissolved.state, RunOwnerState::Blocked);
    assert_eq!(runqueue::wait::reason_snapshot(PARTNER), (RECEIVE, 0x4455));
    ctrl.partner_slot_plus1.store(0, Ordering::Release);
    ctrl.partner_owner_gen.store(0, Ordering::Release);
    ctrl.state.store(PHASE_VACANT, Ordering::Release);

    assert_eq!(
        runqueue::retire_unless_claim_in_flight(PARTNER, WEIGHT),
        RetireOutcome::Retired
    );
    runqueue::release_retired(PARTNER);
    runqueue::reset_before_publication();
}

#[test]
fn paired_custody_supports_park_resume_local_break_and_withdraw() {
    const SLOT: usize = 91;
    const CPU: usize = 3;
    const WEIGHT: u32 = 100;
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _runtime = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();

    // Local -> Running -> Paired -> Running -> Paired -> Local
    runqueue::admit_blocked(SLOT);
    runqueue::publish_local(SLOT, CPU, WEIGHT);
    runqueue::test_running_claim(SLOT, CPU, WEIGHT);
    let mut rq = runqueue::owner(SLOT);
    assert_eq!(rq.state, RunOwnerState::Running);
    let paired = runqueue::pair_park(SLOT, rq, CPU).unwrap();
    assert_eq!(paired.state, RunOwnerState::Paired);
    assert_eq!(paired.cpu, Some(CPU));

    rq = runqueue::pair_resume(SLOT, paired, CPU).unwrap();
    assert_eq!(rq.state, RunOwnerState::Running);
    assert_eq!(rq.cpu, Some(CPU));
    assert!(rq.runnable);

    let paired2 = runqueue::pair_park(SLOT, rq, CPU).unwrap();
    assert_eq!(paired2.state, RunOwnerState::Paired);
    let local = runqueue::pair_break_local(SLOT, CPU, WEIGHT).unwrap();
    assert_eq!(local.state, RunOwnerState::Local);
    assert_eq!(local.cpu, Some(CPU));
    assert!(local.runnable);

    // Paired -> Withdraw -> Retiring (non-waiting withdraw path)
    runqueue::test_running_claim(SLOT, CPU, WEIGHT);
    let paired3 = runqueue::pair_park(SLOT, runqueue::owner(SLOT), CPU).unwrap();
    assert_eq!(paired3.state, RunOwnerState::Paired);
    let retiring = ownership::transition(SLOT, paired3, ownership::OwnerEvent::Withdraw, None)
        .unwrap()
        .settled();
    assert_eq!(retiring.state, RunOwnerState::Retiring);
    assert_eq!(retiring.cpu, Some(CPU));

    runqueue::reset_before_publication();
}

#[test]
fn reset_before_publication_clears_all_cpu_pair_descriptors() {
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _runtime = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();

    for cpu in 0..MAX_TRACKED_CPUS {
        PAIR_CONTROLS[cpu]
            .partner_slot_plus1
            .store(10 + cpu as u64, Ordering::Release);
        PAIR_CONTROLS[cpu]
            .partner_owner_gen
            .store(77, Ordering::Release);
        PAIR_CONTROLS[cpu]
            .state
            .store(PairControl::pack(PHASE_PREPARING, FLAG_BREAK_REQUESTED, 9), Ordering::Release);
    }

    reset_before_publication();

    for cpu in 0..MAX_TRACKED_CPUS {
        assert_eq!(current_phase(PAIR_CONTROLS[cpu].state.load(Ordering::Acquire)), PHASE_VACANT);
        assert_eq!(PAIR_CONTROLS[cpu].partner_slot_plus1.load(Ordering::Acquire), 0);
        assert_eq!(PAIR_CONTROLS[cpu].partner_owner_gen.load(Ordering::Acquire), 0);
        assert!(!break_requested(cpu));
    }
}

#[test]
fn breaking_phase_refuses_new_break_flags_so_concurrent_wakes_retry_until_dissolved() {
    const PARTNER: usize = 92;
    const CPU: usize = 2;
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _runtime = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();

    runqueue::admit_blocked(PARTNER);
    let blocked = runqueue::owner(PARTNER);
    let paired = runqueue::pair_claim(PARTNER, blocked, CPU).unwrap();

    let ctrl = &PAIR_CONTROLS[CPU];
    ctrl.partner_slot_plus1.store(PARTNER as u64 + 1, Ordering::Release);
    ctrl.partner_owner_gen.store(paired.generation, Ordering::Release);
    ctrl.state.store(
        PairControl::pack(super::PHASE_BREAKING, FLAG_BREAK_REQUESTED, 3),
        Ordering::Release,
    );

    assert!(!super::request_break_with_wake(CPU, PARTNER, paired.generation));
    assert_eq!(ctrl.state.load(Ordering::Acquire) & FLAG_WAKE_PARTNER, 0);

    let dissolved = runqueue::wait::dissolve_paired_to_blocked(PARTNER, 1, 0x1122, || {
        runqueue::pair_break_blocked(PARTNER, paired).unwrap()
    });
    assert_eq!(dissolved.state, RunOwnerState::Blocked);
    ctrl.partner_slot_plus1.store(0, Ordering::Release);
    ctrl.partner_owner_gen.store(0, Ordering::Release);
    ctrl.state.store(PHASE_VACANT, Ordering::Release);
    runqueue::reset_before_publication();
}
