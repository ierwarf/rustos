//! Actual original dispatch path with real born backing and funding.
//! Owner: original turn/Current bank, physical frame/reader and funding receipt.
//! Boundary: real priority selection, claim, executing interval and return loan.
//! Lifecycle: original grants span failed fast probe, fallback and final effect.
//! Concurrency: component reader exclusions and exact Direct generation races.
//! Failure: incomplete prefix retains FIFO/count/pending; no fake Kernel bank.
//! Forbidden: scalar identity as lifetime, manufactured Current allocation or ACK.
//! Evidence: born allocations and production core; earlier IRQ/topology and
//! register/RSP ACK remain native models, not full joint hardware qualification.
use super::*;
use crate::multitask::cpu_local::{self, dispatch_turn};
use crate::user::abi::UserAbi;
const ID: u64 = 129800;
fn current(f: impl FnOnce(usize, &dispatch_turn::CpuDispatchTurn<'static>)) {
    super::super::root_creation::tests::exec_root(ID, true,
        super::super::root_creation::tests::bootstrap(UserAbi::Linux), |slot, _| {
            // Earlier single-CPU placement is a native model, not HAL topology
            // or hardware validation. Preserve the surrounding kernel fixture.
            let kernel_masks = runqueue::affinity_payload::affinity_snapshot(121);
            runqueue::affinity_payload::set_affinity(121, 1, 1, false);
            // Explicit earlier native executor wait input. The real bitmap
            // must contain only this test's runnable user, not rely on a
            // synthetic all-slots scan to force incomplete policy/fallback.
            runqueue::test_blocked_return(121, 0, runqueue::weight::value(121));
            let rsp = runqueue::saved_rsp(slot);
            // SAFETY: isolated born physical allocation; model the earlier
            // IRQ's complete admitted user frame before any reader/queue effect.
            unsafe {
                let frame = &mut *(rsp as *mut super::super::SavedContext);
                frame.rip = crate::memory::paging::USER_SPACE_BASE + 0x1000;
                frame.rsp = crate::memory::paging::USER_SPACE_BASE + 0x4000;
            }
            let _banks = current_identity::execution::banks::isolate(0);
            // Earlier initial execution is modeled, but the ORIGINAL non-Copy
            // UID bank must exist before diagnostics or return capture; scalar
            // Current alone is not a retained execution unit.
            current_identity::execution::banks::initialize(0,
                current_identity::execution::initial(slot, ID).unwrap());
            runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
            super::super::execution_interval::test_open_captured(slot, 2);
            dispatch_turn::with_original_current_model(slot, |turn| f(slot, turn));
            runqueue::affinity_payload::set_affinity(121, kernel_masks.0, kernel_masks.1, kernel_masks.2);
        });
}
fn ack_model(slot: usize) {
    let active = super::super::execution_interval::consume(0, slot, ID);
    assert!(matches!(active, super::super::execution_interval::ConsumedInterval::Active(_)));
    super::super::stack_resources::cpu_leases::publish(0, slot, slot);
    super::super::stack_resources::cpu_leases::commit(0);
}
pub(in crate::multitask::scheduler) fn diagnostic_foreign_input() {
    current(|slot, turn| {
        // A running interval has no queued age; age is never execution custody.
        runqueue::wait::set_ready_since_ticks(slot, 0);
        let remote_slot = 2;
        assert_ne!(slot, remote_slot);
        // Earlier FOREIGN CPU table/queue input only, never physical execution
        // permission. The actual observer must sample only its original bank.
        runqueue::admit_running(remote_slot, 1);
        let remote = cpu_local::install_test_current_owner(1, remote_slot);
        let _ = crate::multitask::run_authority::take_window();
        runtime_profile::observe_original_current(turn);
        assert_eq!(crate::multitask::run_authority::take_window(), None);
        // A real disagreement in this entering CPU's retained Current still
        // reports. Restore the earlier native queue input before actual return.
        let weight = runqueue::weight::value(slot);
        assert!(matches!(runqueue::publish_return(slot, 0, weight, false),
            runqueue::ReturnPublication::Blocked { .. }));
        runtime_profile::observe_original_current(turn);
        let (first, count) = crate::multitask::run_authority::take_window().unwrap();
        assert_eq!(count, 1);
        assert_eq!(first & 0xffff_ffff,
            crate::multitask::run_authority::Mismatch::LegacyRunningQueueNot as u64);
        assert!(matches!(runqueue::publish_local_wake(slot, 0, weight),
            runqueue::RemoteWakeOutcome::Published { .. }));
        runqueue::test_running_claim(slot, 0, weight);
        drop(remote);
        // Only the earlier foreign queue input is reset; it owned no physical
        // allocation or UID unit and this is not an execution/reuse ACK.
        runqueue::retire(remote_slot, super::super::NICE_0_LOAD);
        runqueue::release_retired(remote_slot);
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        let result = dispatch(&mut outgoing, runqueue::saved_rsp(slot), false);
        assert_eq!(result.next_slot, slot);
        ack_model(slot);
    });
}

#[test]
fn actual_dispatch_owns_choice_frame_funding_interval_and_return_without_catalog() {
    current(|slot, turn| {
        dispatch_policy::store_dispatch_streaks(0, 3, 3);
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        let result = dispatch(&mut outgoing, runqueue::saved_rsp(slot), false);
        assert_eq!(dispatch_policy::system_dispatch_streak(0), 0);
        assert_eq!(dispatch_policy::latency_handoff_streak(0), 0);
        assert_eq!(result.next_slot, slot);
        assert!(turn.owns_priority_choice());
        assert!(super::super::execution_interval::original_claim_is_active(turn));
        assert_eq!(turn.claimed_return().unwrap().id, ID);
        assert!(super::super::stack_resources::cpu_leases::owns_prepared(0, slot));
        super::super::execution_interval::finish_return(turn, slot);
        assert!(super::super::return_architecture::ReturnArchitecture::capture(turn, result).is_some());
        assert_eq!(cpu_local::scheduler_current_task_scratch(), slot);
        ack_model(slot);
    });
}
#[test]
fn actual_earlier_claim_discharges_only_its_exact_spawn_first_turn() {
    current(|slot, turn| {
        let record = FirstTurnHandoff { slot, task_id: ID };
        let tail = FirstTurnHandoff { slot: MAX_TASK - 1, task_id: ID + 1 };
        {
            let mut policy = dispatch_policy::lock(0);
            policy.spawn_pick_hints.enqueue(record).unwrap();
            policy.spawn_pick_hints.enqueue(tail).unwrap();
        }
        assert!(sync_handoff::enqueue(0, sync_handoff::SyncHandoffRecord::new(slot, ID)));
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        // Real higher-priority synchronous selection and original physical claim,
        // not a scalar Running input or a direct policy-only completion.
        let result = dispatch(&mut outgoing, runqueue::saved_rsp(slot), false);
        assert_eq!(result.next_slot, slot);
        assert_eq!(turn.claimed_return().unwrap().id, ID);
        assert!(super::super::execution_interval::original_claim_is_active(turn));
        let remaining = {
            let policy = dispatch_policy::lock(0);
            (policy.spawn_pick_hints.front(), policy.spawn_pick_hints.len())
        };
        // Explicit foreign ordering input, NOT a new physical incarnation.
        // Even this original retained claim cannot withdraw another UID's hint.
        let foreign = FirstTurnHandoff { slot, task_id: ID + 2 };
        {
            let mut policy = dispatch_policy::lock(0);
            policy.spawn_pick_hints.enqueue(foreign).unwrap();
            policy.atomic_activation_pick_hints.enqueue(foreign).unwrap();
            policy.atomic_activation_handoff_remaining = 1;
            dispatch_policy::publish_atomic_activation_pending(0);
        }
        dispatch_policy::discharge_claimed_first_turn(turn);
        let preserved = {
            let mut policy = dispatch_policy::lock(0);
            (policy.spawn_pick_hints.pop(), policy.spawn_pick_hints.pop(),
                policy.atomic_activation_pick_hints.front(),
                policy.atomic_activation_handoff_remaining,
                dispatch_policy::atomic_activation_pending(0))
        };
        ack_model(slot);
        dispatch_policy::remove_reclaimed_slot(slot);
        dispatch_policy::remove_reclaimed_slot(tail.slot);
        assert_eq!(remaining, (Some(tail), 1),
            "successful earlier claim must discharge its own first-turn promise");
        assert_eq!(preserved, (Some(tail), Some(foreign), Some(foreign), 1, true),
            "completion must preserve foreign UID and following FIFO order");
    });
}

#[test]
fn actual_failed_fast_probe_reuses_one_original_choice_and_outgoing_interval() {
    current(|slot, turn| {
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        let rsp = runqueue::saved_rsp(slot);
        assert!(fast(&mut outgoing, rsp, None).is_none());
        assert!(turn.owns_priority_choice());
        assert!(turn.claimed_return().is_none());
        let result = dispatch(&mut outgoing, rsp, true);
        assert_eq!(result.next_slot, slot);
        assert!(turn.owns_priority_choice());
        assert_eq!(outgoing.account(999, true), 0); // actual original once-only receipt
        assert!(super::super::execution_interval::original_claim_is_active(turn));
        ack_model(slot);
    });
}
#[test]
fn actual_direct_fifo_selection_retains_consumed_generation_through_original_claim() {
    super::super::dispatch_claim::tests::fixture(false, |slot, turn| {
        turn.begin_priority_choice();
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        assert!(matches!(runqueue::publish_direct_handoff(slot, 0), runqueue::RemoteWakeOutcome::Published { cpu: 0, .. }));
        let direct = runqueue::owner(slot);
        assert_eq!(direct.state, runqueue::RunOwnerState::DirectHandoff);
        assert!(sync_handoff::enqueue(0, sync_handoff::SyncHandoffRecord::new(slot, 82800)));
        assert!(sync_handoff::pending(0));
        let Decision::Selected(candidate) = synchronous(0) else { panic!("actual Direct FIFO lost its selected record"); };
        assert_eq!(candidate.slot, slot);
        assert_eq!(candidate.id, 82800);
        assert_eq!(candidate.generation, direct.generation);
        assert!(matches!(candidate.custody, FrameCustody::Direct));
        // This earlier Kernel Current fixture has no real outgoing allocation,
        // so it cannot manufacture a CurrentReturn. Drive the SAME production
        // DispatchFrame engine from the actual consumed Candidate and turn;
        // whole outgoing/architecture/ACK remains covered separately/modeled.
        let token = frame_publication::direct_handoff_frame_token(slot, 0).unwrap();
        let FrameCapture::Ready(frame) = DispatchFrame::capture(turn, candidate.slot,
            candidate.id, candidate.generation, token.saved_rsp, candidate.custody)
            else { panic!("consumed Direct generation lost physical frame"); };
        assert!(frame.prepare().unwrap().claim());
        assert!(super::super::execution_interval::original_claim_is_active(turn));
        assert_eq!(turn.claimed_return().unwrap().running_generation, direct.generation + 1);
        super::super::dispatch_claim::tests::finish_modeled_preparation(slot, 121);
        assert!(!sync_handoff::pending(0));
    });
}

#[test]
pub(super) fn actual_incomplete_first_turn_retains_fifo_and_pending_while_original_reader_excludes_claim() {
    super::super::dispatch_claim::tests::fixture(false, |slot, turn| {
        turn.begin_priority_choice();
        let record = FirstTurnHandoff { slot, task_id: 82800 };
        {
            let mut policy = dispatch_policy::lock(0);
            assert!(policy.atomic_activation_pick_hints.is_empty());
            policy.atomic_activation_pick_hints.enqueue(record).unwrap();
            policy.atomic_activation_handoff_remaining = 1;
            dispatch_policy::publish_atomic_activation_pending(0);
        }
        let token = frame_publication::local_frame_token(slot, 0).unwrap();
        let pin = frame_publication::pin_for_identity(token, record.task_id).unwrap();
        // SAFETY: same actual original masked CPU and exact born allocation.
        let reader = unsafe { super::super::stack_resources::frame_readers::acquire(&pin, 0) }.unwrap();
        // A competing reader permits qualification but excludes the actual
        // claim. The prefix must remain pending until its original claim wins.
        assert!(matches!(first_turn(turn, 0, true, donation_ledger::class_publication_sequence()), Decision::Incomplete));
        {
            let policy = dispatch_policy::lock(0);
            assert_eq!(policy.atomic_activation_pick_hints.front(), Some(record));
            assert_eq!(policy.atomic_activation_handoff_remaining, 1);
        }
        assert!(dispatch_policy::atomic_activation_pending(0));
        assert!(turn.claimed_return().is_none());
        assert!(!super::super::stack_resources::cpu_leases::owns_prepared(0, slot));
        drop(reader);
        drop(pin);
        assert!(matches!(first_turn(turn, 0, true, donation_ledger::class_publication_sequence()), Decision::Claimed(Urgency::Atomic)));
        assert!(super::super::execution_interval::original_claim_is_active(turn));
        super::super::dispatch_claim::tests::finish_modeled_preparation(slot, 121);
        assert!(!dispatch_policy::atomic_activation_pending(0));
        assert_eq!(dispatch_policy::lock(0).atomic_activation_handoff_remaining, 0);
    });
}
