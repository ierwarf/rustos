//! Owner: actual born faulting Current and actual born pager worker.
//! Boundary: real typed service/FIFO/reader/floor and retained MM capture.
//! Lifecycle: both original physical loans survive publication; none crosses trap.
//! Concurrency: exact wait mismatch, precommit wake, full FIFO and pending phase.
//! Failure: no unrelated epoch clear, no rollback or newly granted Current.
//! Forbidden: catalog projections as witnesses or native hardware claims.
//! Evidence: earlier IRQ/bootstrap/topology inputs modeled; real backing effects.
use super::*;
const WORKER: u64 = 130901;
const TOKEN: u64 = 0x51;
fn handoff(scope: &DispatchPreemption<'_>, wait: &CurrentWait, token: u64, id: u64) -> PagerFaultHandoffOutcome {
    // Earlier admitted topology is an explicit native input, not a grant.
    super::handoff_with(scope, wait, token, id, &|_| 1)
}
fn peers(f: impl FnOnce(usize, usize, &DispatchPreemption<'_>, &CurrentWait)) {
    super::super::irq_entry_tests::current_with_peer(true, |sender, worker| {
        let worker = worker.unwrap();
        // Serialized native fixture cleanup: remove stale empty FIFO hints,
        // not a production owner/recovery permission.
        for cpu in 0..2 { assert!(sync_handoff::take_next_ready(cpu, false, |_| false).is_none()); }
        let scope = super::super::irq_entry_tests::scope();
        // SAFETY: SAME original model mask and actual born Current bank remain
        // retained until wait Drop, before any simulated scheduling boundary.
        let wait = unsafe { CurrentWait::capture() }.unwrap();
        assert_eq!(wait.slot(), sender);
        assert!(wait.arm(BlockReason::PagerFault(TOKEN)));
        assert!(wait.commit());
        runqueue::wait::set_reason(worker, runqueue::wait::REASON_PAGER_SERVICE, 0);
        runqueue::wait::set_blocked(worker, true);
        runqueue::publish_blocked(worker, 0, runqueue::weight::value(worker));
        frame_publication::bind_local_owner_generation(worker, runqueue::owner(worker).generation);
        runqueue::raise_vruntime_floor(sender, IPC_DONATION_BONUS_NS * 4);
        runqueue::raise_vruntime_floor(worker, IPC_DONATION_BONUS_NS * 8);
        f(sender, worker, &scope, &wait);
        sync_handoff::remove_slot_all_cpus(worker);
        for cpu in 0..2 { assert!(sync_handoff::take_next_ready(cpu, false, |_| false).is_none()); }
        drop(wait);
        drop(scope);
    });
}
pub(in crate::multitask::scheduler) fn original_pager_trace() {
    peers(|sender, worker, scope, wait| {
        let before = runqueue::owner(worker);
        let refs = current_identity::execution::count(sender);
        assert_eq!(handoff(scope, wait, TOKEN + 1, WORKER), PagerFaultHandoffOutcome::SenderMismatch);
        assert_eq!(runqueue::owner(worker), before);
        assert_eq!(handoff(scope, wait, TOKEN, WORKER), PagerFaultHandoffOutcome::DirectSameCpu);
        assert!(wait.committed_pager(TOKEN));
        assert!(runqueue::wait::blocked(sender));
        assert!(!runqueue::wait::armed(worker));
        assert!(!runqueue::wait::blocked(worker));
        assert_eq!(runqueue::wait::reason(worker), (runqueue::wait::REASON_NONE, 0));
        let owner = runqueue::owner(worker);
        assert_eq!(owner.state, runqueue::RunOwnerState::DirectHandoff);
        assert_eq!(owner.generation, before.generation + 1);
        assert!(runqueue::vruntime(worker) < IPC_DONATION_BONUS_NS * 8);
        assert!(frame_publication::direct_handoff_frame_token(worker, scope.cpu()).is_some());
        let record = sync_handoff::take_next_ready(scope.cpu(), false, |_| true).unwrap();
        assert_eq!(record.task_id(), WORKER);
        assert_eq!(record.bound_direct_generation(), Some(owner.generation));
        assert!(sync_handoff::take_next_ready(scope.cpu(), false, |_| true).is_none());
        assert_eq!(handoff(scope, wait, TOKEN, WORKER), PagerFaultHandoffOutcome::ReceiverMismatch);
        assert_eq!(current_identity::execution::count(sender), refs);
        assert!(stack_resources::cpu_leases::owns_current(scope.cpu(), sender));
    });
}
#[test]
fn actual_pager_full_fifo_preserves_original_worker_wait_and_faulting_current() {
    peers(|sender, worker, scope, wait| {
        let before = runqueue::owner(worker);
        let floor = runqueue::vruntime(worker);
        sync_handoff::with_full_fifo_model(scope.cpu(), || {
            assert_eq!(handoff(scope, wait, TOKEN, WORKER), PagerFaultHandoffOutcome::DirectCustodyUnavailable);
            assert_eq!(runqueue::owner(worker), before);
            assert_eq!(runqueue::wait::reason(worker), (runqueue::wait::REASON_PAGER_SERVICE, 0));
            assert!(runqueue::wait::blocked(worker));
            assert_eq!(runqueue::vruntime(worker), floor);
            assert!(wait.committed_pager(TOKEN));
            assert!(stack_resources::cpu_leases::owns_current(scope.cpu(), sender));
        });
        assert_eq!(handoff(scope, wait, TOKEN, WORKER), PagerFaultHandoffOutcome::DirectSameCpu);
    });
}
#[test]
fn actual_pager_remote_worker_publication_binds_exact_fifo_and_mailbox_generation() {
    ipc_publication::tests::peers_with_policy(true, 3, |sender, worker, scope| {
        // Earlier typed service wait/topology input; actual born worker/funding
        // policy admits both CPUs. This does not model hardware IPI delivery.
        runqueue::wait::set_reason(worker, runqueue::wait::REASON_PAGER_SERVICE, 0);
        runqueue::affinity_payload::set_affinity(worker, 2, 3, false);
        // SAFETY: SAME original masked scope and actual Current bank enclose
        // this loan until the callback exits, before any scheduling trap.
        let wait = unsafe { CurrentWait::capture() }.unwrap();
        assert_eq!(wait.slot(), sender);
        assert!(wait.arm(BlockReason::PagerFault(TOKEN)));
        assert!(wait.commit());
        assert_eq!(super::handoff_with(scope, &wait, TOKEN, 130801, &|_| 2),
            PagerFaultHandoffOutcome::DirectCrossCpu);
        let transfer = runqueue::owner(worker);
        assert_eq!(transfer.state, runqueue::RunOwnerState::RemoteQueued);
        assert_eq!(transfer.cpu, Some(1));
        assert_eq!(runqueue::drain_remote_wakes(1), 1);
        let record = sync_handoff::take_next_ready(1, false, |_| true).unwrap();
        assert_eq!(record.task_id(), 130801);
        let local = runqueue::owner(worker);
        assert_eq!(local.generation, transfer.generation + 1);
        assert_eq!(local.state, runqueue::RunOwnerState::Local);
        assert!(frame_publication::local_frame_token(worker, 1).is_some());
        assert_eq!(record.claim_generation(), None, "remote hint is NOT a Direct execution grant");
        assert_eq!(record.bind_selected_local_owner(local, 1).claim_generation(), Some(local.generation));
        assert!(wait.committed_pager(TOKEN));
        assert_eq!(runqueue::wait::reason(worker), (runqueue::wait::REASON_NONE, 0));
    });
}
#[test]
fn actual_pager_does_not_clear_an_unrelated_rearmed_worker_wait() {
    peers(|_, worker, scope, wait| {
        runqueue::wait::set_reason(worker, runqueue::wait::REASON_ENDPOINT_REPLY, 0x99);
        let before = runqueue::owner(worker);
        assert_eq!(handoff(scope, wait, TOKEN, WORKER), PagerFaultHandoffOutcome::ReceiverMismatch);
        assert_eq!(runqueue::owner(worker), before);
        assert_eq!(runqueue::wait::reason(worker), (runqueue::wait::REASON_ENDPOINT_REPLY, 0x99));
        assert!(!sync_handoff::pending(scope.cpu()));
    });
}
#[test]
fn actual_pager_last_floor_effect_keeps_original_reader_binding_and_typed_phase() {
    peers(|sender, worker, scope, wait| {
        let binding = wake_publication::Binding::capture(worker, WORKER).unwrap();
        let typed = runqueue::wait::TypedWake::capture(&binding, runqueue::wait::REASON_PAGER_SERVICE, 0).unwrap();
        let donor_refs = current_identity::execution::count(sender);
        ipc_publication::publish_with(scope, &binding, &typed, scope.cpu(), || {
            assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0);
            assert!(wait.committed_pager(TOKEN));
            assert_eq!(current_identity::execution::count(sender), donor_refs);
            assert!(!runqueue::wait::try_publish_arm(worker, runqueue::wait::REASON_PAGER_SERVICE, 0));
            let token = frame_publication::direct_handoff_frame_token(worker, scope.cpu()).unwrap();
            assert!(frame_publication::pin_for_identity(token, WORKER).is_some());
            // Production final-claim predicate still refuses this real reader;
            // native callback helper invokes the SAME predicate, not hardware.
            assert!(stack_resources::frame_readers::with_claim_exclusion(worker, || true).is_none());
            latency_floor(wait, TOKEN, &binding, scope.cpu());
        }).unwrap();
        assert!(runqueue::vruntime(worker) < IPC_DONATION_BONUS_NS * 8);
    });
}
#[test]
fn actual_pager_precommit_current_worker_wake_never_reads_consumed_frame_or_reblocks() {
    peers(|_, worker, scope, wait| {
        let token = frame_publication::observed_frame_token(worker, scope.cpu(), false).unwrap();
        let bank = frame_publication::pin_for_identity(token, WORKER).unwrap();
        runqueue::wake_wait(worker);
        assert!(!matches!(runqueue::publish_local_wake(worker, 1, runqueue::weight::value(worker)),
            runqueue::RemoteWakeOutcome::Rejected));
        runqueue::test_running_claim(worker, 1, runqueue::weight::value(worker));
        assert!(runqueue::wait::try_publish_arm(worker, runqueue::wait::REASON_PAGER_SERVICE, 0));
        let rsp = runqueue::saved_rsp(worker);
        // Earlier remote CPU Current/RSP ACK is a native input. The physical
        // allocation bank is REAL; saved frame has already been consumed.
        stack_resources::cpu_leases::with_test_bank(1, bank, false, || {
            let _publication = cpu_local::install_test_current_owner(1, worker);
            {
                runqueue::set_saved_rsp(worker, 1);
                assert_eq!(handoff(scope, wait, TOKEN, WORKER), PagerFaultHandoffOutcome::WakeOnly);
                assert_eq!(runqueue::wait::commit(worker), runqueue::WaitCommitOutcome::WakeWon);
                assert!(runqueue::owner(worker).runnable);
                assert_eq!(runqueue::owner(worker).state, runqueue::RunOwnerState::Running);
                assert!(!sync_handoff::pending(scope.cpu()));
                assert!(runqueue::wait::try_publish_arm(worker, runqueue::wait::REASON_ENDPOINT_REPLY, 0x99));
                assert_eq!(handoff(scope, wait, TOKEN, WORKER), PagerFaultHandoffOutcome::ReceiverMismatch);
                assert_eq!(runqueue::wait::reason(worker), (runqueue::wait::REASON_ENDPOINT_REPLY, 0x99));
                runqueue::set_saved_rsp(worker, rsp);
            }
        });
    });
}
#[test]
fn actual_pager_resume_borrows_original_current_mm_without_claim_or_frame_read() {
    peers(|sender, _, scope, wait| {
        assert!(wait.restore_pager(TOKEN));
        let owner = runqueue::owner(sender);
        let rsp = runqueue::saved_rsp(sender);
        let refs = current_identity::execution::count(sender);
        let architecture = return_architecture::CurrentArchitecture::capture(scope, wait).unwrap();
        // Actual terminal admission closes NEW loans, while this original
        // Current/MM loan must still support the exception's final effects.
        let stop = terminal_quarantine::begin(sender, Some(wait.id()), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: SAME original mask/bank is still live; actual terminal
        // admission must refuse a NEW loan, not replace the retained one.
        assert!(unsafe { CurrentWait::capture() }.is_none());
        assert!(return_architecture::CurrentArchitecture::capture(scope, wait).is_some());
        drop(stop); // Untouched reservation, not terminal teardown/hardware ACK.
        drop(architecture); // Native capture only; privileged installation is NOT run.
        assert_eq!(runqueue::owner(sender), owner);
        assert_eq!(runqueue::saved_rsp(sender), rsp);
        assert_eq!(current_identity::execution::count(sender), refs);
        assert!(stack_resources::cpu_leases::owns_current(scope.cpu(), sender));
        assert!(!stack_resources::cpu_leases::owns_prepared(scope.cpu(), sender));
    });
}
