//! Owner: actual Class55/cohort/resource/queue witness fixtures.
//! Boundary: modeled host MM roots and CPU turn; no HAL topology/IPI/CR3/RSP evidence.
//! Lifecycle: real Ticket-backed bindings exclude the empty-catalog shortcut through final ACK.
//! Concurrency: canonical scheduler fixture; source transfer CAS and terminal overlap are explicit.
//! Failure: exact UID/process/ABI and desired-stop versus terminal-ACK refusal before effects.
//! Forbidden: native cursor/receipt/CPU phase recovery as production or external ACK.
//! Evidence: actual Grant, physical binding pins, Open CPU turn, queue and terminal publication.
use super::*;
use crate::multitask::scheduler::{self, PublishedFrameView, root_slots};
use crate::multitask::{current_identity as identity, process_placement as placement};
const ROOT: usize = 124;
const PEER: usize = 125;
const ID: u64 = 1245501;

fn install(ticket: &stack_resources::admission::Ticket, handle: ProcessHandle, abi: UserAbi) {
    let slot = ticket.slot();
    ticket.begin_commit();
    let mut context = scheduler::tests::test_user_context(handle);
    context.user_abi = Some(abi);
    context.address_space_root = 0x4000;
    context.scheduling_context =
        scheduler::scheduling_context::SchedulingContext::bind(slot, ticket.task_id());
    let process_id = context.process_id;
    ticket.install_context(context);
    runqueue::weight::initialize(slot, scheduler::NICE_0_LOAD);
    runqueue::initialize_vruntime(slot, 0);
    runqueue::initialize_exec_start_ticks(slot, 0);
    ticket.publish_start(scheduler::TaskStart {
        entry: crate::multitask::noop_task_entry,
        id: ticket.task_id(),
    });
    let base = stack_resources::storage_base(slot);
    stack_resources::publish_binding(
        slot,
        ticket.task_id(),
        PublishedFrameView {
            user_mode_task: true,
            kernel_process_task: false,
            kernel_stack_base: (base + scheduler::TASK_STACK_GUARD_BYTES) as u64,
            kernel_stack_top: scheduler::align_kernel_stack_top(base + scheduler::TASK_STACK_SIZE)
                as u64,
            alternate_stack_base: 0,
            alternate_stack_top: 0,
            stack_canary_intact: true,
            scheduler_storage_base: 0,
            scheduler_storage_end: 0,
        },
    );
    // The production constructor also publishes the scalar wait-key/MM root
    // while the actual Ticket still excludes foreign consumers.
    runqueue::address_space::initialize(slot, 0x4000);
    runqueue::affinity_payload::initialize_affinity(slot, 7, 7);
    identity::publish_metadata(
        slot,
        Some(identity::TaskIdentity {
            task_id: Some(ticket.task_id()),
            user_mode: true,
            abi: Some(abi),
            process_handle: Some(handle),
            process_id,
            ..Default::default()
        }),
    );
    // The Ticket still excludes all consumers while the native constructor
    // initializes its exact four gates; metadata alone never makes it runnable.
    for gate in [
        identity::LifecycleGate::Retired,
        identity::LifecycleGate::Suspended,
        identity::LifecycleGate::JobStopped,
        identity::LifecycleGate::ExecQuiesced,
    ] {
        identity::update_live_lifecycle(slot, ticket.task_id(), gate, false);
    }
}
fn process(id: u64) -> ProcessHandle {
    crate::multitask::process_table::create_process(
        id,
        crate::multitask::UserProcessState::new(
            crate::memory::paging::ProcessAddressSpace::modeled_root_for_host(0x4000),
            // Host modeled roots have no physical page tables. The existing
            // native MM cursor fixture avoids walking invented physical pages.
            Some(crate::user::linux::LinuxProcessState::default()),
            None,
            None,
            None,
            false,
            "/affinity-test.exe",
        ),
    )
    .unwrap()
}
fn fixture(peer_abi: UserAbi, f: impl FnOnce(ProcessHandle)) {
    fixture_with_peer_handle(peer_abi, false, f);
}
fn fixture_with_peer_handle(peer_abi: UserAbi, foreign: bool, f: impl FnOnce(ProcessHandle)) {
    affinity_fixture(UserAbi::Windows, peer_abi, foreign, f);
}
/// Canonical real-Ticket fixture shared by the independent affinity owners.
/// ABI/MM/current hardware equivalence is not granted by this native factory.
pub(in crate::multitask::scheduler) fn affinity_fixture(
    root_abi: UserAbi,
    peer_abi: UserAbi,
    foreign: bool,
    f: impl FnOnce(ProcessHandle),
) {
    let _catalog = scheduler::tests::boxed_scheduler();
    let handle = process(ID);
    let root =
        root_slots::RootPublication::begin(handle, root_slots::reserve(ROOT, ID).unwrap(), 7)
            .unwrap_or_else(|_| panic!("physical root refused"));
    install(root.ticket(), handle, root_abi);
    root.finish();
    let join = placement::Join::begin(handle, (ROOT, ID), root_abi).unwrap();
    let ticket = root_slots::reserve(PEER, ID + 1).unwrap();
    let peer_handle = if foreign { process(ID + 999) } else { handle };
    install(&ticket, peer_handle, peer_abi);
    join.register(PEER, ID + 1).unwrap();
    runqueue::admit_blocked(PEER);
    let _receipt = ticket.finish_commit();
    drop(join);
    f(handle);
    for slot in [ROOT, PEER] {
        assert_eq!(identity::execution::count(slot), 0);
        assert!(!identity::cohort::busy(slot));
        runqueue::retire(slot, 100);
        runqueue::release_retired(slot);
        assert!(stack_resources::close_for_reclaim(slot));
        stack_resources::task_start::clear(slot);
        stack_resources::context::clear(slot);
        drop(stack_resources::take_storage(slot));
        identity::clear(slot);
    }
}
fn capture(handle: ProcessHandle, mask: u64) -> Grant {
    Grant::capture(handle, (ROOT, ID), mask, 7).unwrap()
}
/// Narrow native preparation through the actual group owner, not payload reset.
pub(in crate::multitask::scheduler) fn change_blocked_process_mask(
    handle: ProcessHandle,
    mask: u64,
) {
    capture(handle, mask).finish_with(|_, _, _, _| panic!("blocked group woken"));
}
#[test]
pub(in crate::multitask::scheduler) fn actual_group_capture_owns_every_live_binding_and_excludes_reuse_until_queue_and_membership_ack()
{
    fixture(UserAbi::Windows, |h| {
        let grant = capture(h, 4);
        for slot in [ROOT, PEER] {
            assert_eq!(identity::execution::count(slot), 1);
            assert!(identity::cohort::busy(slot));
            assert!(
                identity::with_reclaim_binding(slot, Some(ID + u64::from(slot == PEER)), || ())
                    .is_none()
            );
            assert!(
                identity::execution::with_claim(slot, |_| panic!("unpublished cohort admitted"))
                    .is_none()
            );
        }
        assert!(placement::Join::begin(h, (ROOT, ID), UserAbi::Windows).is_none());
        assert!(placement::Change::begin(h, (ROOT, ID)).is_none());
        assert_eq!(placement::mask(h), Some(7));
        let result = grant.finish_with(|_, _, _, _| panic!("blocked slot reanimated"));
        assert_eq!(result.previous_mask, 7);
        assert!(!result.reschedule_required);
        assert_eq!(placement::mask(h), Some(4));
        for slot in [ROOT, PEER] {
            assert_eq!(
                runqueue::affinity_payload::affinity_snapshot(slot),
                (4, 4, false)
            );
            assert_eq!(
                runqueue::owner(slot).state,
                runqueue::RunOwnerState::Blocked
            );
        }
        assert!(placement::Join::begin(h, (ROOT, ID), UserAbi::Windows).is_some());
    });
}
#[test]
pub(in crate::multitask::scheduler) fn queued_rehome_keeps_old_process_mask_and_closed_admission_until_actual_owner_effect_returns() {
    fixture(UserAbi::Windows, |h| {
        runqueue::set_runnable(PEER, true);
        assert!(matches!(
            runqueue::publish_local_wake(PEER, 0, 100),
            runqueue::RemoteWakeOutcome::Published { .. }
        ));
        let mut calls = 0;
        let result = capture(h, 4).finish_with(|slot, id, mask, cohort| {
            calls += 1;
            assert_eq!((slot, id, mask), (PEER, ID + 1, 4));
            assert_eq!(placement::mask(h), Some(7));
            assert!(identity::cohort::busy(ROOT) && identity::cohort::busy(PEER));
            assert_eq!(identity::execution::count(PEER), 1);
            assert!(placement::Join::begin(h, (ROOT, ID), UserAbi::Windows).is_none());
            let before = runqueue::owner(slot);
            assert_eq!(
                runqueue::rehome_queued(slot, 2, 100),
                runqueue::RemoteWakeOutcome::Rejected
            );
            assert_eq!(runqueue::owner(slot), before);
            assert_eq!(
                runqueue::rehome_queued_owned(slot, 2, 100, cohort, id + 99),
                runqueue::RemoteWakeOutcome::Rejected
            );
            assert_eq!(runqueue::owner(slot), before);
            assert_eq!(
                scheduler::runqueue_policy::rehome_owned_on_mask(slot, mask, 100, cohort, id),
                Some(2)
            );
            assert_eq!(runqueue::owner(slot).cpu, Some(2));
        });
        assert_eq!(calls, 1);
        assert!(result.reschedule_required);
        assert_eq!(placement::mask(h), Some(4));
        let before = runqueue::owner(PEER);
        assert_eq!(
            runqueue::rehome_queued(PEER, 0, 100),
            runqueue::RemoteWakeOutcome::Rejected
        );
        assert_eq!(runqueue::owner(PEER), before);
    });
}
#[test]
pub(in crate::multitask::scheduler) fn running_affinity_never_steals_current_custody_but_marks_exclusion_and_requests_reschedule() {
    fixture(UserAbi::Windows, |h| {
        // Real MM claim requires an independently owned Open CPU turn. This
        // existing native phase witness has no directory guard or hardware ACK.
        crate::multitask::cpu_local::with_test_activation_return(0, ROOT, PEER, |phase| {
            if phase != 0 {
                return;
            }
            let (_, current) = identity::execution::with_claim(ROOT, |_| ()).unwrap();
            identity::execution::banks::initialize(0, current.unwrap());
            runqueue::set_runnable(PEER, true);
            runqueue::publish_local_wake(PEER, 0, 100);
            assert!(runqueue::claim_dispatch(PEER, 0, 100));
            let before = runqueue::owner(PEER);
            let result = capture(h, 4).finish_with(|slot, id, mask, cohort| {
                assert_eq!(
                    scheduler::runqueue_policy::rehome_owned_on_mask(slot, mask, 100, cohort, id),
                    Some(0)
                );
                assert_eq!(runqueue::owner(slot), before);
            });
            assert!(result.reschedule_required);
            assert_eq!(
                runqueue::affinity_payload::affinity_snapshot(PEER),
                (4, 4, true)
            );
            assert_eq!(runqueue::owner(PEER), before);
        });
    });
}
#[test]
pub(in crate::multitask::scheduler) fn untouched_grant_rollback_releases_all_exact_pins_without_masks_queue_or_membership_effects() {
    fixture(UserAbi::Windows, |h| {
        let owner = runqueue::owner(PEER);
        drop(capture(h, 4));
        assert_eq!(placement::mask(h), Some(7));
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(PEER),
            (7, 7, false)
        );
        assert_eq!(runqueue::owner(PEER), owner);
        assert_eq!(identity::execution::count(ROOT), 0);
        assert_eq!(identity::execution::count(PEER), 0);
        assert!(placement::Join::begin(h, (ROOT, ID), UserAbi::Windows).is_some());
    });
}
#[test]
pub(in crate::multitask::scheduler) fn post_pin_physical_abi_refusal_rolls_back_complete_group_before_any_effect() {
    fixture(UserAbi::Linux, |h| {
        assert!(matches!(
            Grant::capture(h, (ROOT, ID), 4, 7),
            Err(AffinityError::WrongAbi)
        ));
        assert_eq!(placement::mask(h), Some(7));
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(ROOT),
            (7, 7, false)
        );
        assert_eq!(identity::execution::count(ROOT), 0);
        assert_eq!(identity::execution::count(PEER), 0);
        assert!(placement::Change::begin(h, (ROOT, ID)).is_some());
    });
}
#[test]
pub(in crate::multitask::scheduler) fn invalid_requested_mask_and_foreign_full_handle_or_caller_uid_cannot_spend_real_membership() {
    fixture(UserAbi::Windows, |h| {
        for (requested, online) in [(0, 7), (8, 7), (1, 0)] {
            assert!(matches!(
                Grant::capture(h, (ROOT, ID), requested, online),
                Err(AffinityError::InvalidMask)
            ));
        }
        assert!(Grant::capture(h, (ROOT, ID + 99), 4, 7).is_err());
        assert!(
            Grant::capture(
                ProcessHandle::new(h.index(), h.generation() + 1),
                (ROOT, ID),
                4,
                7
            )
            .is_err()
        );
        assert_eq!(placement::mask(h), Some(7));
        assert!(placement::Change::begin(h, (ROOT, ID)).is_some());
    });
}
#[test]
pub(in crate::multitask::scheduler) fn desired_stop_without_terminal_ack_refuses_instead_of_treating_missing_member_as_drained() {
    fixture(UserAbi::Windows, |h| {
        identity::update_live_lifecycle(PEER, ID + 1, identity::LifecycleGate::Retired, true);
        assert!(Grant::capture(h, (ROOT, ID), 4, 7).is_err());
        assert_eq!(placement::mask(h), Some(7));
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(ROOT),
            (7, 7, false)
        );
        assert_eq!(identity::execution::count(ROOT), 0);
    });
}
#[test]
pub(in crate::multitask::scheduler) fn exact_terminal_membership_ack_excludes_stopped_peer_without_rewriting_or_waking_its_slot() {
    fixture(UserAbi::Windows, |h| {
        use scheduler::retirement_cleanup::producer::{
            publish_process_turn_fixture, reset_process_turn_fixture,
        };
        let mut turn = placement::terminal::Turn::begin(h, (PEER, ID + 1)).unwrap();
        identity::update_live_lifecycle(PEER, ID + 1, identity::LifecycleGate::Retired, true);
        turn.mark_stopped();
        let receipt = publish_process_turn_fixture(PEER, ID + 1, turn.credential());
        turn.finish(receipt);
        let owner = runqueue::owner(PEER);
        let grant = capture(h, 4);
        assert_eq!(grant.len, 1);
        assert_eq!(identity::execution::count(PEER), 0);
        grant.finish_with(|_, _, _, _| panic!("stopped peer reanimated"));
        assert_eq!(placement::mask(h), Some(4));
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(PEER),
            (7, 7, false)
        );
        assert_eq!(runqueue::owner(PEER), owner);
        reset_process_turn_fixture(PEER); // isolated Class50 fixture, not external ACK
    });
}

#[test]
pub(in crate::multitask::scheduler) fn foreign_physical_process_binding_refuses_after_exact_cohort_capture_without_spending_either_process()
 {
    fixture_with_peer_handle(UserAbi::Windows, true, |h| {
        assert!(matches!(
            Grant::capture(h, (ROOT, ID), 4, 7),
            Err(AffinityError::MissingTask)
        ));
        assert_eq!(placement::mask(h), Some(7));
        assert_eq!(identity::execution::count(ROOT), 0);
        assert_eq!(identity::execution::count(PEER), 0);
        assert!(placement::Change::begin(h, (ROOT, ID)).is_some());
    });
}
#[test]
pub(in crate::multitask::scheduler) fn terminal_stop_during_queue_effect_cannot_reuse_or_reanimate_captured_peer_and_survives_final_affinity_ack()
 {
    fixture(UserAbi::Windows, |h| {
        use scheduler::retirement_cleanup::producer::{
            publish_process_turn_fixture, reset_process_turn_fixture,
        };
        runqueue::set_runnable(PEER, true);
        runqueue::publish_local_wake(PEER, 0, 100);
        let result = capture(h, 4).finish_with(|slot, id, mask, cohort| {
            let mut turn = placement::terminal::Turn::begin(h, (slot, id)).unwrap();
            identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true);
            runqueue::retire(slot, 100);
            turn.mark_stopped();
            let receipt = publish_process_turn_fixture(slot, id, turn.credential());
            turn.finish(receipt);
            assert!(identity::with_reclaim_binding(slot, Some(id), || ()).is_none());
            assert!(stack_resources::reuse::begin_retired(slot, id).is_none());
            assert_eq!(
                scheduler::runqueue_policy::rehome_owned_on_mask(slot, mask, 100, cohort, id),
                None
            );
        });
        assert!(result.reschedule_required);
        assert_eq!(placement::mask(h), Some(4));
        assert_eq!(
            runqueue::owner(PEER).state,
            runqueue::RunOwnerState::Retired
        );
        assert!(identity::terminal_retired_matches(PEER, ID + 1));
        assert!(
            identity::execution::with_claim(PEER, |_| panic!("retired peer admitted")).is_none()
        );
        reset_process_turn_fixture(PEER);
    });
}

#[test]
pub(in crate::multitask::scheduler) fn peer_block_between_affinity_masks_and_queue_effect_never_spends_affinity_as_a_wake() {
    fixture(UserAbi::Windows, |h| {
        runqueue::set_runnable(PEER, true);
        runqueue::publish_local_wake(PEER, 0, 100);
        let result = capture(h, 4).finish_with(|slot, id, mask, cohort| {
            runqueue::publish_blocked(slot, 0, 100);
            let blocked = runqueue::owner(slot);
            assert_eq!(
                scheduler::runqueue_policy::rehome_owned_on_mask(slot, mask, 100, cohort, id),
                None
            );
            assert_eq!(runqueue::owner(slot), blocked);
        });
        assert!(result.reschedule_required);
        assert_eq!(placement::mask(h), Some(4));
        assert_eq!(
            runqueue::owner(PEER).state,
            runqueue::RunOwnerState::Blocked
        );
    });
}
