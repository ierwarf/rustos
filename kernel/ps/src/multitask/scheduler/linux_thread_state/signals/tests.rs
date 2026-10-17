//! Owner: actual born Linux group and recipient witnesses, not legacy catalogs.
//! Boundary: original process/census/cohort and UID pins through last effect.
//! Lifecycle: blocked admission -> all gates/event -> one release -> original wake.
//! Concurrency: actual conflicting joins/terminal/cohort/reclaim are exercised.
//! Failure: wrong PID/UID/purpose and closing recipients refuse before effects.
//! Forbidden: modeled CPU admission or consumer ACKs as hardware acceptance.
//! Evidence: same production entry engines; native CPU admission/ACK remain inputs.
use super::*;
use super::super::super::{current_terminal::tests as fixture, runqueue,
    terminal_quarantine, terminal_publication, retirement_cleanup, TaskRetireReason};
use fixture::ID;
// Native topology eligibility is an explicit input, not HAL/hardware acceptance.
fn queue_in(scope: &DispatchPreemption<'_>, pid: u64, id: u64, signal: u64) -> bool {
    super::queue_in(scope, pid, id, signal, &|_| 1) == Ok(true)
}
fn process_sigchld_in(scope: &DispatchPreemption<'_>, pid: u64, events: u32) -> bool {
    super::process_sigchld_in(scope, pid, events, &|_| 1)
}
fn stop_in(scope: &DispatchPreemption<'_>, signal: u64) -> bool {
    super::stop_in(scope, signal, &|_| 1).expect("uncontended actual Current group")
}
fn state(slot: usize, id: u64) -> super::super::LinuxThreadState {
    super::super::test_exec_state(slot, id).unwrap()
}
fn retire(slot: usize, id: u64, scope: &DispatchPreemption<'_>) {
    check_scope(scope);
    let producer = terminal_quarantine::begin(slot, Some(id), TaskRetireReason::Exited, false).unwrap();
    // SAFETY: same native modeled masked CPU; real original Producer/pins.
    unsafe { terminal_publication::guarded(producer, &|_| 1); }
}
pub(in crate::multitask::scheduler) fn group_witness() {
    fixture::family(true, |leader, _| {
        let sibling = fixture::sibling(leader);
        fixture::with_current(leader, |leader, scope| {
        let target = Recipient::resolve(ID, ID).unwrap();
        let handle = target.context.process_handle.unwrap();
        let mask = process_placement::mask(handle);
        let group = Group::capture(&target).unwrap();
        assert!(Group::capture(&target).is_none());
        assert!(group.finish(scope, Some(19), &|_| 1, |owner| {
            for &(slot, id) in &owner.members[..owner.len] {
                assert!(identity::lifecycle(slot).job_stopped);
                assert!(identity::execution::initial(slot, id).is_none());
                assert!(owner.cohort.owns_job_control(slot, id));
                assert!(!owner.cohort.owns_affinity(slot, id));
                assert!(!owner.cohort.unsuspend(slot, id));
                assert!(identity::execution::pin_creation(slot, id, handle, UserAbi::Linux).is_none());
                assert!(identity::execution::count(slot) >= 1);
            }
            assert!(process_placement::Join::begin(handle, target.member(), UserAbi::Linux).is_none());
            assert!(process_placement::Change::begin(handle, target.member()).is_none());
        }));
        assert_eq!(process_placement::mask(handle), mask);
        assert!(identity::cohort::Cohort::begin_job_control(&[(leader, ID)]).is_some());
        assert!(identity::lifecycle(sibling).job_stopped);
        assert!(!stop_in(scope, 19));
        assert!(identity::execution::initial(leader, ID).is_none());
        assert!(process_placement::Join::begin(handle, target.member(), UserAbi::Linux).is_none());
        drop(target);
        assert!(queue_in(scope, ID, ID, rustos_user_abi::linux::SIGCONT));
        assert!(!identity::lifecycle(leader).job_stopped);
        assert!(!identity::lifecycle(sibling).job_stopped);
        assert_ne!(state(leader, ID).pending_signals & crate::user::sysops::linux::linux_signal_bit(
            rustos_user_abi::linux::SIGCONT).unwrap(), 0);
        retire(sibling, ID + 1, scope);
        });
        // Actual native consumers ACK only after the modeled CPU owner drops;
        // the shared fixture drains the actual leader-then-sibling FIFO.
        Some(sibling)
    });
}
#[test]
fn actual_group_job_control_seals_all_members_and_resumes_before_signal_delivery() { group_witness(); }

pub(in crate::multitask::scheduler) fn arm_witness() {
    fixture::current(|slot, scope| {
        runqueue::wait::publish_arm(slot, runqueue::wait::REASON_GENERIC, 0);
        assert!(runqueue::wait::armed(slot));
        let saved = runqueue::saved_rsp(slot);
        runqueue::set_saved_rsp(slot, 1); // consumed Current frame is never dereferenced by wake
        task_directory::forget(ID); // absent accelerator must still find exact original recipient
        assert!(queue_in(scope, ID, ID, 15));
        assert!(!runqueue::wait::armed(slot));
        assert!(!runqueue::wait::blocked(slot));
        assert_eq!(runqueue::wait::commit(slot), runqueue::WaitCommitOutcome::WakeWon);
        runqueue::set_saved_rsp(slot, saved);
    });
}
#[test]
fn actual_signal_original_recipient_revokes_block_arm_without_consumed_frame_read() { arm_witness(); }

pub(in crate::multitask::scheduler) fn sigchld_witness() {
    fixture::family(false, |leader, _| {
        let sibling = fixture::sibling(leader);
        let scope = DispatchPreemption::enter_model(0);
        let stop = rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP;
        let cont = rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_CONTINUE;
        assert!(process_sigchld_in(&scope, ID, stop));
        assert_eq!(state(leader, ID).pending_sigchld_events, stop);
        assert_eq!(state(sibling, ID + 1).pending_sigchld_events, 0);
        retire(leader, ID, &scope);
        assert_eq!(state(leader, ID).pending_sigchld_events, 0);
        assert_eq!(state(sibling, ID + 1).pending_sigchld_events, stop);
        assert!(process_sigchld_in(&scope, ID, cont)); // leaderless original recipient
        assert_eq!(state(sibling, ID + 1).pending_sigchld_events, stop | cont);
        retire(sibling, ID + 1, &scope);
        Some(sibling)
    });
}
#[test]
fn actual_process_sigchld_prefers_leader_then_transfers_and_qualifies_leaderless_original() { sigchld_witness(); }

#[test]
fn actual_group_contention_returns_retry_without_signal_or_partial_stop_effects() {
    fixture::current(|slot, scope| {
        let target = Recipient::resolve(ID, ID).unwrap();
        let group = Group::capture(&target).unwrap();
        assert_eq!(super::queue_in(scope, ID, ID, rustos_user_abi::linux::SIGCONT, &|_| 1), Err(()));
        assert_eq!(super::stop_in(scope, 19, &|_| 1), None);
        assert_eq!(state(slot, ID).pending_signals, 0);
        assert!(!identity::lifecycle(slot).job_stopped);
        drop(group); // untouched original census/cohort rollback, no effects
        drop(target);
        assert!(super::stop_in(scope, 19, &|_| 1).is_some());
        assert!(queue_in(scope, ID, ID, rustos_user_abi::linux::SIGCONT));
    });
}

#[test]
fn actual_signal_wrong_pid_generation_and_mask_refuse_or_preserve_exact_wait() {
    fixture::current(|slot, scope| {
        runqueue::wait::publish_arm(slot, runqueue::wait::REASON_GENERIC, 0);
        assert!(!queue_in(scope, ID + 1, ID, 15));
        assert!(!queue_in(scope, ID, ID + 99, 15));
        assert!(!queue_in(scope, ID, ID, 99));
        assert!(!process_sigchld_in(scope, ID, 0));
        assert!(runqueue::wait::armed(slot));
        let bit = crate::user::sysops::linux::linux_signal_bit(15).unwrap();
        super::super::test_update_exact(slot, ID, |s| s.signal_mask = bit);
        assert!(queue_in(scope, ID, ID, 15));
        assert!(runqueue::wait::armed(slot));
        assert_eq!(state(slot, ID).pending_signals, bit);
        assert!(identity::signal_pending(slot));
        assert!(queue_in(scope, ID, ID, 0));
        assert_eq!(state(slot, ID).pending_signals, bit);
    });
}

#[test]
fn actual_signal_retained_target_delays_terminal_effect_and_reuse_until_original_drop() {
    fixture::family(false, |slot, _| {
        let scope = DispatchPreemption::enter_model(0);
        let target = Recipient::resolve(ID, ID).unwrap();
        let producer = terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: modeled scope and actual original borrower force producer deferral.
        unsafe { terminal_publication::guarded(producer, &|_| 1); }
        assert_eq!(retirement_cleanup::reason(slot), Some(TaskRetireReason::Exited));
        assert!(identity::execution::count(slot) > 0);
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        assert!(Recipient::resolve(ID, ID).is_none());
        assert_eq!(target.queue(&scope, 15, 0, &|_| 1), Ok(false));
        // Same admitted original recipient, not a new loan after stop. Its
        // release makes the durable first producer eligible for finalization.
        assert_eq!(state(slot, ID).pending_signals & (1 << 14), 1 << 14);
        let producer = retirement_cleanup::producer::Producer::claim(slot, Some(ID)).unwrap();
        // SAFETY: SAME modeled masked scope; original deferred producer now drains.
        unsafe { terminal_publication::guarded(producer, &|_| 1); }
        None
    });
}
