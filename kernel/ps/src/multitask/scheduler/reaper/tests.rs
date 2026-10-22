//! Owner: actual retired grant and joint terminal/resource completion boundary.
//! Boundary: real Ticket/context/row/queue seals, not physical CPU switch or user MM.
//! Lifecycle: external completion precedes vacancy and terminal reopening.
//! Concurrency: canonical global native fixture locks; independent slot grants.
//! Failure: desired stop, exact UID, bank/backing and terminal ACK are separate qualifiers.
//! Forbidden: these host witnesses do not prove full guard deletion or external process effects.
//! Evidence: shared production Grant::capture/detach/finish_with, no catalog reclaim methods.
use super::super::{
    TaskStart, execution_image, reclaim::RetirementSideEffect, root_slots,
    scheduling_context,
};
use super::*;
const SLOT: usize = 126;
const OTHER: usize = 127;
const ID: u64 = 126011;
fn context(slot: usize, id: u64) -> TaskContext {
    TaskContext {
        scheduling_context: scheduling_context::SchedulingContext::bind(slot, id),
        saved_rsp: 0,        address_space_root: 0,        user_mode: false,
        user_abi: None,
        console_session: crate::io::session::ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: None,
        user_stack: None,
        windows_thread_state: None,
    }
}
fn prepare(slot: usize, id: u64) {
    prepare_with(slot, id, context(slot, id));
}
fn prepare_with(slot: usize, id: u64, context: TaskContext) {
    current_identity::clear(slot);
    retirement_cleanup::reset(slot);
    let ticket = root_slots::reserve(slot, id).unwrap();
    execution_image::reserved_kernel_entry(
        &ticket,
        8,
        16,
        2,
        crate::multitask::noop_task_entry as *const () as u64,
        0,
    );
    ticket.begin_commit();
    ticket.install_context(context);
    runqueue::simd_tls::initialize_simd_state(slot, id);
    ticket.publish_start(TaskStart {
        id,
        entry: crate::multitask::noop_task_entry,
    });
    let base = stack_resources::storage_base(slot);
    runqueue::initialize_kernel_stack_bounds(
        slot,
        (base + super::super::TASK_STACK_GUARD_BYTES) as u64,
        super::super::align_kernel_stack_top(base + super::super::TASK_STACK_SIZE) as u64,
    );
    stack_resources::publish_binding(
        slot,
        id,
        super::super::context_validation::PublishedFrameView {
            user_mode_task: false,
            kernel_process_task: false,
            kernel_stack_base: (base + super::super::TASK_STACK_GUARD_BYTES) as u64,
            kernel_stack_top: super::super::align_kernel_stack_top(
                base + super::super::TASK_STACK_SIZE,
            ) as u64,
            alternate_stack_base: 0,
            alternate_stack_top: 0,
            stack_canary_intact: true,
            scheduler_storage_base: 0,
            scheduler_storage_end: 0,
        },
    );
    current_identity::publish_metadata(
        slot,
        Some(current_identity::TaskIdentity {
            task_id: Some(id),
            ..Default::default()
        }),
    );
    assert!(root_slots::admit_blocked(ticket).is_ok());
}
fn stop(slot: usize, id: u64, ack: bool) {
    current_identity::publish_lifecycle_change(
        slot,
        current_identity::read(slot),
        current_identity::LifecycleGate::Retired,
        true,
    );
    runqueue::retire(slot, 100);
    retirement_cleanup::publish_terminal(
        slot,
        Some(id),
        TaskRetireReason::Exited,
        None,
        RetirementSideEffect::new(Some(id), None),
    );
    if ack {
        retirement_cleanup::ack_kernel_fixture(slot);
    }
}
fn fixture(f: impl FnOnce()) {
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _runtime = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    scheduling_runtime::reset_for_test();
    f();
    for slot in [SLOT, OTHER] {
        assert!(stack_resources::context::read(slot).is_none());
        assert!(stack_resources::task_start::read(slot).is_none());
        assert_eq!(current_identity::execution::binding(slot), None);
        retirement_cleanup::reset(slot);
    }
}
fn complete(grant: Grant) {
    grant.detach().finish_with(|reclaim| reclaim.complete());
}
#[test]
fn actual_detached_prefix_keeps_both_seals_and_hidden_uid_until_external_completion_returns() {
    fixture(|| {
        prepare(SLOT, ID);
        stop(SLOT, ID, true);
        let grant = Grant::capture(SLOT, None).unwrap();
        assert!(Grant::capture(SLOT, None).is_none());
        assert!(root_slots::reserve(SLOT, ID + 1).is_none());
        let detached = grant.detach();
        assert!(stack_resources::context::read(SLOT).is_none());
        assert!(stack_resources::task_start::read(SLOT).is_none());
        assert_eq!(current_identity::execution::binding(SLOT), Some(ID));
        let mut effects = 0;
        detached.finish_with(|reclaim| {
            assert!(root_slots::reserve(SLOT, ID + 1).is_none());
            assert!(retirement_cleanup::reuse::begin(SLOT, None).is_none());
            assert!(stack_resources::reuse::begin(SLOT, Some(ID)).is_none());
            assert_eq!(current_identity::execution::binding(SLOT), Some(ID));
            effects += 1;
            reclaim.complete();
        });
        assert_eq!(effects, 1);
        assert_eq!(current_identity::execution::binding(SLOT), None);
        let seal = retirement_cleanup::reuse::begin(SLOT, None).unwrap();
        drop(seal);
        let ticket = root_slots::reserve(SLOT, ID + 1).unwrap();
        root_slots::cancel_unpublished(ticket);
    });
}
#[test]
fn physical_retired_grant_independently_requires_desired_stop_and_exact_terminal_run_owner() {
    fixture(|| {
        prepare(SLOT, ID);
        assert!(stack_resources::reuse::begin_retired(SLOT, ID).is_none());
        retirement_cleanup::publish_terminal(
            SLOT,
            Some(ID),
            TaskRetireReason::Exited,
            None,
            RetirementSideEffect::new(Some(ID), None),
        );
        retirement_cleanup::ack_kernel_fixture(SLOT);
        assert!(Grant::capture(SLOT, None).is_none());
        current_identity::publish_lifecycle_change(
            SLOT,
            current_identity::read(SLOT),
            current_identity::LifecycleGate::Retired,
            true,
        );
        assert!(Grant::capture(SLOT, None).is_none());
        runqueue::retire(SLOT, 100);
        complete(Grant::capture(SLOT, None).unwrap());
    });
}
#[test]
pub(in crate::multitask::scheduler) fn pending_external_terminal_ack_refuses_reaper_before_metadata_or_resource_effects() {
    fixture(|| {
        prepare(SLOT, ID);
        stop(SLOT, ID, false);
        assert!(Grant::capture(SLOT, None).is_none());
        assert!(stack_resources::context::read(SLOT).is_some());
        assert_eq!(stack_resources::task_start::read(SLOT).unwrap().id, ID);
        assert_eq!(
            runqueue::owner(SLOT).state,
            runqueue::RunOwnerState::Retired
        );
        retirement_cleanup::ack_kernel_fixture(SLOT);
        complete(Grant::capture(SLOT, None).unwrap());
    });
}
#[test]
fn captured_backing_and_lifecycle_custody_refuse_then_release_into_same_exact_retired_grant() {
    fixture(|| {
        prepare(SLOT, ID);
        let pin = current_identity::execution::pin_waiter(SLOT, ID).unwrap();
        stop(SLOT, ID, true);
        assert!(Grant::capture(SLOT, None).is_none());
        // Untouched Class50 seal was released; the genuine pin still owns the lifetime.
        let seal = retirement_cleanup::reuse::begin(SLOT, Some(ID)).unwrap();
        drop(seal);
        assert_eq!(current_identity::execution::count(SLOT), 1);
        assert!(stack_resources::context::read(SLOT).is_some());
        drop(pin);
        complete(Grant::capture(SLOT, None).unwrap());
    });
}
#[test]
fn hidden_metadata_keeps_exact_retired_binding_without_using_absence_as_drain_evidence() {
    fixture(|| {
        prepare(SLOT, ID);
        stop(SLOT, ID, true);
        current_identity::publish_metadata(SLOT, None);
        assert!(current_identity::read(SLOT).is_none());
        assert!(!current_identity::terminal_retired_matches(SLOT, ID + 1));
        assert_eq!(current_identity::execution::binding(SLOT), Some(ID));
        complete(Grant::capture(SLOT, None).unwrap());
    });
}
#[test]
fn two_real_retired_slots_own_disjoint_seals_and_first_ack_cannot_reopen_second() {
    fixture(|| {
        prepare(SLOT, ID);
        prepare(OTHER, ID + 1);
        stop(SLOT, ID, true);
        stop(OTHER, ID + 1, true);
        let a = Grant::capture(SLOT, None).unwrap().detach();
        let b = Grant::capture(OTHER, None).unwrap().detach();
        a.finish_with(|r| r.complete());
        assert!(root_slots::reserve(OTHER, ID + 2).is_none());
        assert_eq!(current_identity::execution::binding(OTHER), Some(ID + 1));
        b.finish_with(|r| r.complete());
    });
}

#[test]
fn funded_interval_is_closed_not_discarded_and_last_charge_release_precedes_retired_detachment() {
    fixture(|| {
        let policy = scheduling_context::SchedulingContextPolicy {
            budget_ns: 4_000,
            period_ns: 10_000,
            refill_capacity: 2,
            cpu_mask: 1,
            criticality: 1,
            domain: 7,
            policy_epoch: 3,
            timeout_endpoint_cap: 0,
        };
        let generation = scheduling_runtime::initialize_domain(6, policy).unwrap();
        let mut context = context(SLOT, ID);
        assert!(context.scheduling_context.admit(policy, 6, generation));
        let identity = context.scheduling_context.identity();
        prepare_with(SLOT, ID, context);
        let stamp = current_identity::PagerChargeStamp {
            context_slot: identity.slot(),
            context_generation: identity.generation(),
            scheduling_domain: policy.domain,
            domain_generation: u64::from(generation),
            policy_epoch: policy.policy_epoch,
            period_ns: policy.period_ns,
        };
        let pin = scheduling_runtime::pin_published_funding(stamp, 0, ID, 77).unwrap();
        stop(SLOT, ID, true);
        assert!(Grant::capture(SLOT, None).is_none());
        assert!(stack_resources::context::read(SLOT).is_some());
        assert!(scheduling_runtime::pin_published_funding(stamp, 0, ID, 88).is_none());
        let (owner, charge) = pin.charge(100, 300);
        assert_eq!(owner, ID);
        assert_eq!(charge.charged_ns, 300);
        let snapshot = scheduling_runtime::runtime_snapshot(SLOT, identity, policy).unwrap();
        assert_eq!(snapshot.consumed_ns, 300);
        complete(Grant::capture(SLOT, None).unwrap());
        assert!(scheduling_runtime::runtime_snapshot(SLOT, identity, policy).is_none());
    });
}
#[test]
fn abandoned_external_completion_keeps_terminal_and_physical_reuse_quarantined_even_after_empty_metadata()
 {
    fixture(|| {
        prepare(SLOT, ID);
        stop(SLOT, ID, true);
        let detached = Grant::capture(SLOT, None).unwrap().detach();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            detached.finish_with(|reclaim| {
                drop(reclaim); // no external ACK; neither seal may implicitly open
                panic!("explicit abandoned external completion fixture");
            });
        }));
        assert!(result.is_err());
        assert!(stack_resources::context::read(SLOT).is_none());
        assert_eq!(current_identity::execution::binding(SLOT), Some(ID));
        assert!(root_slots::reserve(SLOT, ID + 1).is_none());
        assert!(retirement_cleanup::reuse::begin(SLOT, None).is_none());
        assert!(stack_resources::reuse::begin(SLOT, Some(ID)).is_none());
        // Native fixture recovery only, not an external ACK or a production retry route.
        retirement_cleanup::reuse::recover_detached_fixture(SLOT);
        stack_resources::reuse::recover_detached_fixture(SLOT, ID);
    });
}

#[test]
fn saved_frame_backing_without_lifecycle_count_independently_delays_the_real_reaper() {
    fixture(|| {
        prepare(SLOT, ID);
        let (base, top) = runqueue::kernel_stack_bounds(SLOT);
        let frame = stack_resources::pin_view(
            SLOT,
            ID,
            super::super::context_validation::PublishedFrameView {
                user_mode_task: false,
                kernel_process_task: false,
                kernel_stack_base: base,
                kernel_stack_top: top,
                alternate_stack_base: 0,
                alternate_stack_top: 0,
                stack_canary_intact: true,
                scheduler_storage_base: 0,
                scheduler_storage_end: 0,
            },
        )
        .unwrap();
        assert_eq!(current_identity::execution::count(SLOT), 0);
        stop(SLOT, ID, true);
        assert!(Grant::capture(SLOT, None).is_none());
        assert!(stack_resources::context::read(SLOT).is_some());
        drop(frame);
        complete(Grant::capture(SLOT, None).unwrap());
    });
}
#[test]
fn physical_all_cpu_policy_withdrawal_removes_only_the_owned_slot_before_external_ack() {
    fixture(|| {
        prepare(SLOT, ID);
        prepare(OTHER, ID + 1);
        stop(SLOT, ID, true);
        stop(OTHER, ID + 1, true);
        for cpu in 0..nucleus_core::util::lockdep::MAX_TRACKED_CPUS {
            let mut p = dispatch_policy::lock(cpu);
            p.next_pick_hint = Some(dispatch_policy::LatencyPickHint { slot: SLOT, task_id: ID });
            p.latency_pick_hint_head = 0;
            p.latency_pick_hint_len = 2;
            p.latency_pick_hints[0] = Some(dispatch_policy::LatencyPickHint {
                slot: SLOT,
                task_id: ID,
            });
            p.latency_pick_hints[1] = Some(dispatch_policy::LatencyPickHint {
                slot: OTHER,
                task_id: ID + 1,
            });
            p.spawn_pick_hints
                .enqueue(super::super::handoff_queue::FirstTurnHandoff {
                    slot: SLOT,
                    task_id: ID,
                })
                .unwrap();
            p.spawn_pick_hints
                .enqueue(super::super::handoff_queue::FirstTurnHandoff {
                    slot: OTHER,
                    task_id: ID + 1,
                })
                .unwrap();
            p.atomic_activation_pick_hints
                .enqueue(super::super::handoff_queue::FirstTurnHandoff {
                    slot: SLOT,
                    task_id: ID,
                })
                .unwrap();
            p.atomic_activation_pick_hints
                .enqueue(super::super::handoff_queue::FirstTurnHandoff {
                    slot: OTHER,
                    task_id: ID + 1,
                })
                .unwrap();
            // Model the earlier producer's COMPLETE queue/count/flag effect,
            // not a FIFO entry with no committed prefix accounting.
            p.atomic_activation_handoff_remaining = 2;
            dispatch_policy::publish_atomic_activation_pending(cpu);
        }
        let a = Grant::capture(SLOT, None).unwrap().detach();
        for cpu in 0..nucleus_core::util::lockdep::MAX_TRACKED_CPUS {
            let mut p = dispatch_policy::lock(cpu);
            assert_eq!(p.next_pick_hint, None);
            assert_eq!(p.latency_pick_hint_len, 1);
            assert_eq!(p.latency_pick_hints[0].unwrap().slot, OTHER);
            assert_eq!(p.spawn_pick_hints.pop().unwrap().slot, OTHER);
            assert!(p.spawn_pick_hints.is_empty());
            assert_eq!(p.atomic_activation_pick_hints.front().unwrap().slot, OTHER);
            assert_eq!(p.atomic_activation_handoff_remaining, 1);
            assert!(dispatch_policy::atomic_activation_pending(cpu));
        }
        a.finish_with(|r| r.complete());
        complete(Grant::capture(OTHER, None).unwrap());
        for cpu in 0..nucleus_core::util::lockdep::MAX_TRACKED_CPUS {
            let p = dispatch_policy::lock(cpu);
            assert!(p.atomic_activation_pick_hints.is_empty());
            assert_eq!(p.atomic_activation_handoff_remaining, 0);
            assert!(!dispatch_policy::atomic_activation_pending(cpu));
        }
    });
}
