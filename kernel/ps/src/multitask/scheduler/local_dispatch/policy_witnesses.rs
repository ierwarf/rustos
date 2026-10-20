//! Owner: SAME production fair/age/hint policy over actual born resources.
//! Boundary: earlier Current/CPU/class/age inputs are modeled, not grants.
//! Lifecycle: original physical/MM and UID banks survive every policy read.
//! Failure: no missing publication, stale UID, queue age or hint grants execution.
//! Evidence: historical policy regressions, not claim/return or hardware proof.
use super::*;
use crate::multitask::scheduler::{self, dispatch_policy, dispatcher, frame_publication,
    irq_entry_tests, root_creation, stack_resources, thread_creation};
const ID: u64 = 130900;

pub(in crate::multitask::scheduler) fn family(f: impl FnOnce(usize, usize, usize)) {
    irq_entry_tests::current_with_peer(true, |root, peer| {
        let first = peer.unwrap();
        let mut boot = root_creation::tests::bootstrap(crate::user::abi::UserAbi::Linux);
        boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x3000);
        boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x6000);
        let second = thread_creation::tests::active_sibling(root, ID + 2, boot);
        let handle = stack_resources::context::read(second).unwrap().process_handle.unwrap();
        for slot in [root, first, second] {
            dispatch_policy::remove_reclaimed_slot(slot);
            runqueue::wait::set_ready_since_ticks(slot, 0);
        }
        dispatch_policy::store_dispatch_streaks(0, 0, 0);
        f(root, first, second);
        assert!(stack_resources::cpu_leases::owns_current(0, root));
        assert!(current_identity::execution::banks::owns_current(0, root));
        assert!(frame_publication::local_frame_token(first, 0).is_some());
        thread_creation::tests::release_born(second, ID + 2, handle);
    });
}
fn fair(root: usize) -> usize {
    choose_local_next(0, root, false, super::super::SCHED_MAX_BURST_NS,
        crate::arch::clock::monotonic_nanos(), runqueue::local_runnable_sequence(0),
        donation_ledger::class_publication_sequence(), false, false).unwrap().0
}
fn overdue(root: usize, ticks: u64) -> Option<usize> {
    choose_overdue_system(0, root, ticks, crate::arch::clock::monotonic_nanos(),
        runqueue::local_runnable_sequence(0), donation_ledger::class_publication_sequence())
        .unwrap().map(|(slot, _)| slot)
}
// Explicit earlier complete wait/queue transition on the original born slot.
// A lone runnable-bit edit is an INCOMPLETE publication, not stable absence.
fn modeled_wait(slot: usize, blocked: bool) {
    runqueue::wait::set_blocked(slot, blocked);
    if blocked { runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot)); }
    else { let _ = runqueue::publish_local_wake(slot, 0, runqueue::weight::value(slot)); }
    frame_publication::bind_local_owner_generation(slot, runqueue::owner(slot).generation);
}
fn ticks() -> u64 { crate::arch::rtc::ticks_per_second().max(1).saturating_mul(2) }
fn system(slot: usize) {
    runqueue::weight::replace(slot, super::super::SYSTEM_CLASS_WEIGHT_FLAG | super::super::NICE_0_LOAD);
}

#[test]
fn pending_migration_selects_exact_destination_without_early_placement_ack() {
    let mut observed = None;
    family(|root, target, _| {
        let (root_mask, process_mask, _) = runqueue::affinity_payload::affinity_snapshot(root);
        runqueue::affinity_payload::set_affinity(root, root_mask, process_mask, true);
        let retained = current_can_keep_running(root, 0, crate::arch::clock::monotonic_nanos());
        runqueue::affinity_payload::set_affinity(root, root_mask, process_mask, false);

        let (_, process_mask, _) = runqueue::affinity_payload::affinity_snapshot(target);
        runqueue::affinity_payload::set_affinity(target, 1, process_mask, true);
        let original = runqueue::owner(target);
        let now = crate::arch::clock::monotonic_nanos();
        let admission = published_candidate_admission(target, 0, now);
        let excluded = published_candidate_admission(target, 1, now);
        let qualified = qualified_local_handoff_generation(target, 0, now);
        system(target);
        let selected = choose_local_next(
            0, root, false, super::super::SCHED_MAX_BURST_NS,
            crate::arch::clock::monotonic_nanos(), runqueue::local_runnable_sequence(0),
            donation_ledger::class_publication_sequence(), false, false,
        );
        observed = Some((
            retained, admission, excluded, qualified, original.generation,
            selected, target, runqueue::owner(target) == original,
            runqueue::affinity_payload::affinity_snapshot(target).2,
        ));
    });
    // Assert only after original MM/resource/UID teardown, including for a
    // deliberately refused mutant; a witness failure must not abort its RAII.
    let (retained, admission, excluded, qualified, generation, selected, target,
        unchanged, pending) = observed.unwrap();
    assert!(!retained, "pending intent must not become retained Current permission");
    assert_eq!(admission, Ok(true), "stable intent must reach the included destination claim");
    assert_eq!(excluded, Ok(false), "migration intent must never bypass the actual mask");
    assert_eq!(qualified, Ok(Some(generation)));
    assert_eq!(selected, Ok((target, generation)), "actual fair scan must select the born frame");
    assert!(unchanged);
    assert!(pending, "selection must not spend the later RSP placement ACK");
}

pub(in crate::multitask::scheduler) fn bounded_system_burst_reserves_a_ready_user_turn() {
    let _serial = scheduler::execution_test_serial_guard();
    dispatch_policy::store_dispatch_streaks(0, super::super::MAX_CONSECUTIVE_SYSTEM_DISPATCHES - 1, 0);
    assert!(!user_reservation_due(0));
    dispatch_policy::record_completed_choice(0, true, false);
    assert!(user_reservation_due(0));
    dispatch_policy::record_completed_choice(0, true, true);
    assert_eq!(dispatch_policy::system_dispatch_streak(0), super::super::MAX_CONSECUTIVE_SYSTEM_DISPATCHES);
    assert_eq!(dispatch_policy::latency_handoff_streak(0), 1);
    dispatch_policy::record_completed_choice(0, false, false);
    assert!(!user_reservation_due(0));
    assert_eq!(dispatch_policy::latency_handoff_streak(0), 0);
    dispatch_policy::record_completed_choice(0, true, false);
    assert_eq!(dispatch_policy::system_dispatch_streak(0), 1);
    dispatch_policy::store_dispatch_streaks(0, 0, 0);
}

pub(in crate::multitask::scheduler) fn user_reservation_obeys_vruntime_without_a_wall_clock_bypass() {
    family(|root, newer, older| {
        system(root);
        // The SAME original Current return publishes the outgoing frame and
        // fair membership. Physical/MM and UID banks stay borrowed throughout;
        // this partial prologue is policy evidence, not a completed return.
        scheduler::execution_interval::test_open_captured(root, 2);
        let entry = crate::multitask::cpu_local::irq_entry::DispatchEntry::from_scope(irq_entry_tests::scope());
        let mut outgoing = scheduler::current_return::CurrentReturn::capture(entry.turn()).unwrap();
        outgoing.account(crate::arch::rtc::ticks(), false);
        outgoing.publish_frame(runqueue::saved_rsp(root), true);
        assert_eq!(outgoing.publish_queue(true), runqueue::ReturnPublication::Local);
        runqueue::raise_vruntime_floor(newer, 10);
        runqueue::raise_vruntime_floor(older, 20);
        runqueue::wait::set_ready_since_ticks(newer, 2);
        runqueue::wait::set_ready_since_ticks(older, 1);
        // A distant wall-clock age does not bypass the System class arm.
        assert!(!user_reservation_due(0));
        assert_eq!(fair(root), root);
        dispatch_policy::store_dispatch_streaks(0, super::super::MAX_CONSECUTIVE_SYSTEM_DISPATCHES, 0);
        assert_eq!(fair(root), newer);
        // Reservation orders User peers by charged runtime, not oldest age.
        runqueue::raise_vruntime_floor(newer, 21);
        assert_eq!(fair(root), older);
    });
}

pub(in crate::multitask::scheduler) fn fair_locality_is_bounded_by_class_and_vruntime_lag() {
    family(|root, global, local| {
        runqueue::raise_vruntime_floor(root, 100_000_000);
        runqueue::raise_vruntime_floor(global, 1_000_000);
        runqueue::raise_vruntime_floor(local, 1_000_000 + SCHED_CPU_LOCALITY_LAG_NS);
        runqueue::affinity_payload::record_last_cpu(global, 1);
        runqueue::affinity_payload::record_last_cpu(local, 0);
        assert_eq!(fair(root), local);
        runqueue::raise_vruntime_floor(local, 1_000_001 + SCHED_CPU_LOCALITY_LAG_NS);
        assert_eq!(fair(root), global);
        // Both directions of each real global/local comparison are exercised.
        runqueue::affinity_payload::record_last_cpu(global, 0);
        assert_eq!(fair(root), global);
        runqueue::raise_vruntime_floor(global, 1_000_002 + SCHED_CPU_LOCALITY_LAG_NS);
        assert_eq!(fair(root), local);
        system(global);
        runqueue::raise_vruntime_floor(global, u64::MAX / 2);
        assert_eq!(fair(root), global, "locality cannot cross strict class priority");
    });
}

pub(in crate::multitask::scheduler) fn overdue_system_task_is_forced_after_latency_bound() {
    family(|root, peer, other| {
        system(peer);
        system(other);
        assert_eq!(overdue(root, ticks()), None, "zero is the exact not-queued age");
        runqueue::wait::set_ready_since_ticks(peer, ticks());
        assert_eq!(overdue(root, ticks()), None, "fresh work is not overdue");
        runqueue::wait::set_ready_since_ticks(peer, 1);
        assert_eq!(overdue(root, ticks()), Some(peer));
        runqueue::wait::set_ready_since_ticks(other, 2);
        assert_eq!(overdue(root, ticks()), Some(peer));
        modeled_wait(peer, true);
        assert_eq!(overdue(root, ticks()), Some(other));
        modeled_wait(peer, false);
    });
}

pub(in crate::multitask::scheduler) fn overdue_system_continuation_precedes_unrelated_ipc_hint_without_losing_it() {
    overdue_preserves_hint(false);
}
pub(in crate::multitask::scheduler) fn overdue_system_continuation_precedes_a_fresh_latency_handoff() {
    overdue_preserves_hint(true);
}
fn overdue_preserves_hint(latency: bool) {
    family(|root, old, hinted| {
        system(old);
        if !latency { system(hinted); }
        runqueue::wait::set_ready_since_ticks(old, 1);
        runqueue::wait::set_ready_since_ticks(hinted, ticks());
        let hint = dispatch_policy::LatencyPickHint { slot: hinted, task_id: ID + 2 };
        {
            let mut policy = dispatch_policy::lock(0);
            if latency {
                policy.latency_pick_hints[0] = Some(hint);
                policy.latency_pick_hint_head = 0;
                policy.latency_pick_hint_len = 1;
            } else { policy.next_pick_hint = Some(hint); }
        }
        let entry = crate::multitask::cpu_local::irq_entry::DispatchEntry::from_scope(irq_entry_tests::scope());
        entry.turn().begin_priority_choice();
        assert_eq!(dispatcher::test_policy_choice(entry.turn(), ticks()), Some(old));
        if latency { assert_eq!(dispatch_policy::lock(0).latency_pick_hint_len, 1); }
        else { assert_eq!(dispatch_policy::lock(0).next_pick_hint, Some(hint)); }
        // Stable earlier blocked input denies only old work. The SAME real
        // priority chain then consumes the exact hint without granting a claim.
        modeled_wait(old, true);
        assert_eq!(overdue(root, ticks()), None);
        assert_eq!(dispatcher::test_policy_choice(entry.turn(), ticks()), Some(hinted));
        if latency { assert_eq!(dispatch_policy::lock(0).latency_pick_hint_len, 0); }
        else { assert!(dispatch_policy::lock(0).next_pick_hint.is_none()); }
        assert!(entry.turn().claimed_return().is_none());
        modeled_wait(old, false);
    });
}

pub(in crate::multitask::scheduler) fn stale_pick_hint_falls_through_without_mutating_task_state() {
    family(|root, peer, _| {
        system(peer); // explicit earlier manifest admission, not a frame grant
        modeled_wait(peer, true);
        let owner = runqueue::owner(peer);
        let token = frame_publication::local_frame_token(peer, 0);
        let age = runqueue::wait::ready_since_ticks(peer);
        let publication = current_identity::read_dispatch(peer).unwrap();
        dispatch_policy::lock(0).next_pick_hint = Some(dispatch_policy::LatencyPickHint { slot: peer, task_id: ID + 1 });
        assert!(dispatcher::test_hint_refuses_selection(0, root));
        assert_eq!(runqueue::owner(peer), owner);
        let scalar_observation = |token: Option<frame_publication::StableFrameToken>| {
            token.map(|t| (t.slot, t.owner_generation, t.frame_epoch, t.saved_rsp))
        };
        assert_eq!(scalar_observation(frame_publication::local_frame_token(peer, 0)), scalar_observation(token));
        assert_eq!(runqueue::wait::ready_since_ticks(peer), age);
        assert_eq!(current_identity::read_dispatch(peer).unwrap().task_id, publication.task_id);
        assert!(runqueue::wait::blocked(peer));
        modeled_wait(peer, false);
        // A consumed ordering hint is not a wake or a context mutation.
        assert!(dispatch_policy::lock(0).next_pick_hint.is_none());
    });
}

pub(in crate::multitask::scheduler) fn dispatch_fairness_and_handoff_state_is_cpu_isolated() {
    let _serial = scheduler::execution_test_serial_guard();
    dispatch_policy::store_dispatch_streaks(0, 0, 0);
    dispatch_policy::store_dispatch_streaks(1, super::super::MAX_CONSECUTIVE_SYSTEM_DISPATCHES, 3);
    dispatch_policy::lock(1).next_pick_hint = Some(dispatch_policy::LatencyPickHint { slot: 7, task_id: 7 });
    dispatch_policy::record_completed_choice(0, false, false);
    dispatch_policy::lock(0).next_pick_hint = None;
    assert_eq!(dispatch_policy::system_dispatch_streak(0), 0);
    assert_eq!(dispatch_policy::system_dispatch_streak(1), super::super::MAX_CONSECUTIVE_SYSTEM_DISPATCHES);
    assert_eq!(dispatch_policy::latency_handoff_streak(1), 3);
    assert_eq!(dispatch_policy::lock(1).next_pick_hint.unwrap().slot, 7);
    dispatch_policy::store_dispatch_streaks(1, 0, 0);
    dispatch_policy::lock(1).next_pick_hint = None;
}
