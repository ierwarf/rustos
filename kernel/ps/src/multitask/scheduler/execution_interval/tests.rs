use super::super::super::current_identity::PagerChargeStamp;
use super::super::scheduling_context::{SchedulingContext, SchedulingContextPolicy};
use super::super::scheduling_runtime;
use super::*;

fn runtime(
    slot: usize,
    task_id: u64,
) -> (
    PagerChargeStamp,
    SchedulingContextPolicy,
    kernel_object::api::identity::ObjectIdentity,
) {
    let policy = SchedulingContextPolicy {
        budget_ns: 4_000,
        period_ns: 10_000,
        refill_capacity: 2,
        cpu_mask: 1,
        criticality: 1,
        domain: 7,
        policy_epoch: 3,
        timeout_endpoint_cap: 0,
    };
    let generation = scheduling_runtime::initialize_domain(0, policy).unwrap();
    let identity = SchedulingContext::derived_identity(slot, task_id).unwrap();
    assert!(scheduling_runtime::initialize(
        slot, identity, policy, 0, generation
    ));
    (
        PagerChargeStamp {
            context_slot: identity.slot(),
            context_generation: identity.generation(),
            scheduling_domain: policy.domain,
            domain_generation: u64::from(generation),
            policy_epoch: policy.policy_epoch,
            period_ns: policy.period_ns,
        },
        policy,
        identity,
    )
}

#[test]
fn captured_funding_survives_close_and_blocks_context_and_domain_reuse() {
    let _serial = scheduling_runtime::test_serial_guard();
    scheduling_runtime::reset_for_test();
    let (stamp, policy, identity) = runtime(60, 91_001);
    let pin = scheduling_runtime::pin_published_funding(stamp, 0, 91_001, 77).unwrap();
    assert!(!scheduling_runtime::close_for_reclaim(60, identity));
    assert!(scheduling_runtime::pin_published_funding(stamp, 0, 91_001, 88).is_none());
    assert_eq!(
        scheduling_runtime::published_stamp_admission(stamp, 0, 0),
        None
    );
    assert!(
        scheduling_runtime::replace_domain(0, policy, stamp.domain_generation as u32, policy)
            .is_none()
    );
    let (owner, outcome) = pin.charge(100, 1_000);
    assert_eq!(owner, 91_001);
    assert_eq!(outcome.charged_ns, 1_000);
    assert!(scheduling_runtime::close_for_reclaim(60, identity));
    scheduling_runtime::release(60, identity);
    let replacement = SchedulingContext::derived_identity(60, 91_002).unwrap();
    assert!(scheduling_runtime::initialize(
        60,
        replacement,
        policy,
        0,
        stamp.domain_generation as u32
    ));
    assert!(scheduling_runtime::pin_published_funding(stamp, 0, 91_001, 77).is_none());
    // No interval is executing, but the replacement context still owns membership.
    assert!(
        scheduling_runtime::replace_domain(0, policy, stamp.domain_generation as u32, policy)
            .is_none()
    );
    scheduling_runtime::release(60, identity); // stale ACK cannot spend replacement.
    assert!(
        scheduling_runtime::replace_domain(0, policy, stamp.domain_generation as u32, policy)
            .is_none()
    );
    scheduling_runtime::release(60, replacement);
    assert!(
        scheduling_runtime::replace_domain(0, policy, stamp.domain_generation as u32, policy)
            .is_some()
    );
    scheduling_runtime::reset_for_test();
}

#[test]
fn pinned_exhaustion_keeps_the_captured_reply_not_a_replacement_donation() {
    let _serial = scheduling_runtime::test_serial_guard();
    scheduling_runtime::reset_for_test();
    let (stamp, policy, identity) = runtime(61, 91_003);
    let pin = scheduling_runtime::pin_published_funding(stamp, 0, 91_003, 0xabc).unwrap();
    assert!(!scheduling_runtime::close_for_reclaim(61, identity));
    let (_, outcome) = pin.charge(100, 5_000);
    assert!(outcome.exhausted);
    let snapshot = scheduling_runtime::runtime_snapshot(61, identity, policy).unwrap();
    assert_eq!(snapshot.timeout_fault_reply, 0xabc);
    assert!(scheduling_runtime::close_for_reclaim(61, identity));
    scheduling_runtime::reset_for_test();
}

#[test]
fn a_cpu_interval_is_consumed_once_and_uses_only_its_own_clock_domain() {
    let cpu = MAX_TRACKED_CPUS - 1;
    install(
        cpu,
        ExecutionInterval {
            slot: 62,
            task_id: 91_004,
            running_generation: 3,
            start_ticks: 0,
            start_local_ns: 100, charge_from_ticks: 0, charge_from_local_ns: 100,
            funding: None,
        },
    );
    let ConsumedInterval::Active(interval) = consume(cpu, 62, 91_004) else {
        panic!("missing interval");
    };
    assert_eq!(interval.elapsed_ns(1, Some(170), |_, _| 900), 70);
    assert_eq!(interval.elapsed_ns(1, Some(90), |_, _| 900), 900);
    assert_eq!(interval.elapsed_ns(1, None, |_, _| 900), 900);
    assert!(interval.charge(200, 70).is_none());
    assert!(matches!(
        consume(cpu, 62, 91_004),
        ConsumedInterval::AlreadySettled
    ));
    assert!(matches!(consume(cpu, 62, 91_005), ConsumedInterval::Absent));
    install(
        cpu,
        ExecutionInterval {
            slot: 62,
            task_id: 91_005,
            running_generation: 4,
            start_ticks: 2,
            start_local_ns: 0, charge_from_ticks: 2, charge_from_local_ns: 0,
            funding: None,
        },
    );
    let ConsumedInterval::Active(interval) = consume(cpu, 62, 91_005) else {
        panic!("missing new interval");
    };
    assert_eq!(interval.elapsed_ns(2, None, |_, _| 900), 0);
    assert!(interval.charge(200, 0).is_none());
}

#[test]
fn funding_pin_rejects_wrong_owner_cpu_and_identity_without_leaking_custody() {
    let _serial = scheduling_runtime::test_serial_guard();
    scheduling_runtime::reset_for_test();
    let (stamp, _, identity) = runtime(63, 91_006);
    assert!(scheduling_runtime::pin_published_funding(stamp, 0, 91_007, 0).is_none());
    assert!(scheduling_runtime::pin_published_funding(stamp, 1, 91_006, 0).is_none());
    assert!(
        scheduling_runtime::pin_published_funding(
            PagerChargeStamp {
                context_generation: stamp.context_generation + 1,
                ..stamp
            },
            0,
            91_006,
            0
        )
        .is_none()
    );
    assert!(scheduling_runtime::close_for_reclaim(63, identity));
    scheduling_runtime::reset_for_test();
}

#[test]
fn bootstrap_running_work_has_an_interval_before_it_becomes_idle() {
    // SAME original native obligation now starts from the original image and
    // born Ticket/physical descriptor, not a scalar-only synthetic admission.
    super::super::bootstrap::initial_interval_witness();
}

#[test]
fn prepared_grant_keeps_its_funder_after_donation_publication_changes() {
    let _runtime = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    scheduling_runtime::reset_for_test();
    runqueue::reset_before_publication();
    donation_ledger::reset();
    let receiver = 60;
    let (stamp, _, lender_identity) = runtime(61, 93001);
    let pin = scheduling_runtime::pin_published_funding(stamp, 0, 93001, 0xdef).unwrap();
    let own = super::super::super::current_identity::TaskIdentity {
        task_id: Some(93002),
        ..Default::default()
    };
    super::super::super::current_identity::clear(receiver);
    super::super::super::current_identity::publish_metadata(receiver, Some(own));
    let original = ExecutionInterval {
        slot: receiver, task_id: 93002, running_generation: 1,
        start_ticks: 1, start_local_ns: 0, charge_from_ticks: 1, charge_from_local_ns: 0, funding: Some(pin),
    };
    assert!(!scheduling_runtime::close_for_reclaim(61, lender_identity));
    // Current publication has no borrowed donor. Opening must consume the
    // prepared lender, not re-resolve this into the receiver's unbudgeted self.
    assert_eq!(
        donation_ledger::borrowed_context_charge_token(receiver),
        None
    );
    runqueue::admit_running(receiver, 0);
    install(0, original);
    let scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
    let current = CurrentInterval::borrow(&scope, receiver, 93002).unwrap();
    // Same ORIGINAL executing funding, despite absent current donation and
    // closed fresh admission. Original loan outlives both paired budget reads.
    let snapshot = current.runtime_snapshot().unwrap();
    assert_eq!((snapshot.executing_task_id, snapshot.context_owner_task_id), (93002, 93001));
    assert_eq!(snapshot.context_identity_slot, lender_identity.slot());
    assert_eq!(snapshot.context_identity_generation, lender_identity.generation());
    assert_eq!(snapshot.budget_ns, 4000);
    assert_eq!(snapshot.period_ns, 10000);
    assert_eq!(snapshot.domain, 7);
    assert_eq!(snapshot.context_available_ns, 4000);
    assert_eq!(snapshot.domain_available_ns, 4000);
    assert_eq!(snapshot.timeout_fault_count, 0);
    assert!(CurrentInterval::borrow(&scope, receiver, 93002).is_none());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
        consume(0, receiver, 93002))).is_err());
    drop(current);
    drop(scope);
    let ConsumedInterval::Active(interval) = consume(0, receiver, 93002) else {
        panic!("missing granted interval");
    };
    let (owner, outcome) = interval.charge(100, 1000).unwrap();
    assert_eq!(owner, 93001);
    assert_eq!(outcome.charged_ns, 1000);
    assert!(scheduling_runtime::close_for_reclaim(61, lender_identity));
    super::super::super::current_identity::clear(receiver);
    scheduling_runtime::reset_for_test();
}

#[test]
fn original_current_snapshot_retains_exact_timeout_reply_and_pair_after_shared_exhaustion() {
    let _runtime = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    scheduling_runtime::reset_for_test();
    runqueue::reset_before_publication();
    let (stamp, policy, identity) = runtime(61, 93011);
    let original = scheduling_runtime::pin_published_funding(stamp, 0, 93011, 0xaaa).unwrap();
    let peer = scheduling_runtime::pin_published_funding(stamp, 0, 93011, 0xdef).unwrap();
    runqueue::admit_running(60, 0);
    install(0, ExecutionInterval { slot: 60, task_id: 93012, running_generation: 1,
        start_ticks: 1, start_local_ns: 0, charge_from_ticks: 1, charge_from_local_ns: 0, funding: Some(original) });
    // Another admitted charge can exhaust shared credit; this existing original
    // interval still owns the pair. It must observe the exact fault, not re-admit.
    assert_eq!(peer.charge(100, 4500).1.overrun_ns, 500);
    assert!(!scheduling_runtime::close_for_reclaim(61, identity));
    let scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
    let current = CurrentInterval::borrow(&scope, 60, 93012).unwrap();
    let snapshot = current.runtime_snapshot().unwrap();
    assert_eq!((snapshot.executing_task_id, snapshot.context_owner_task_id), (93012, 93011));
    assert_eq!(snapshot.context_available_ns, 0);
    assert_eq!(snapshot.domain_available_ns, 0);
    assert_eq!(snapshot.context_pending_refill_ns, policy.budget_ns);
    assert_eq!(snapshot.domain_pending_refill_ns, policy.budget_ns);
    assert_eq!(snapshot.timeout_fault_count, 1);
    assert_eq!(snapshot.timeout_fault_consumed_ns, policy.budget_ns);
    assert_eq!(snapshot.timeout_fault_budget_ns, policy.budget_ns);
    assert_eq!(snapshot.timeout_fault_period_ns, policy.period_ns);
    assert_eq!(snapshot.timeout_fault_reply, 0xdef);
    assert_eq!(snapshot.timeout_endpoint_cap, 0);
    assert_eq!(snapshot.timeout_fault_action, 1);
    drop(current);
    drop(scope);
    let ConsumedInterval::Active(original) = consume(0, 60, 93012) else { panic!("lost interval"); };
    assert!(original.charge(100, 0).is_none());
    assert!(scheduling_runtime::close_for_reclaim(61, identity));
    scheduling_runtime::reset_for_test();
}

#[test]
fn grant_pin_cannot_authorize_credit_drained_after_preparation() {
    let _serial = scheduling_runtime::test_serial_guard();
    scheduling_runtime::reset_for_test();
    let (stamp, _, _) = runtime(63, 93003);
    let old = scheduling_runtime::pin_published_funding(stamp, 0, 93003, 1).unwrap();
    assert_eq!(old.charge(100, 4000).1.charged_ns, 4000);
    assert!(scheduling_runtime::pin_published_funding(stamp, 0, 93003, 2).is_none());
    scheduling_runtime::reset_for_test();
}

#[test]
fn return_cancels_unused_grant_and_notifies_final_closing_funder() {
    let _serial = scheduling_runtime::test_serial_guard();
    scheduling_runtime::reset_for_test();
    let (stamp, _, identity) = runtime(61, 94001);
    let pin = scheduling_runtime::pin_published_funding(stamp, 0, 94001, 0xaaa).unwrap();
    let mut unused = Some(pin);
    assert!(!scheduling_runtime::close_for_reclaim(61, identity));
    super::super::retirement_cleanup::take_reap_pending();
    drop(unused.take());
    assert!(super::super::retirement_cleanup::take_reap_pending());
    assert!(scheduling_runtime::close_for_reclaim(61, identity));
    assert!(unused.is_none());
    drop(unused.take()); // duplicate cancellation cannot spend another pin
    scheduling_runtime::reset_for_test();
}

#[test]
fn cancellation_cannot_settle_active_funding_or_clear_a_foreign_cpu_grant() {
    let _serial = scheduling_runtime::test_serial_guard();
    scheduling_runtime::reset_for_test();

    let (stamp, _, identity) = runtime(61, 94011);
    let active = scheduling_runtime::pin_published_funding(stamp, 0, 94011, 0xabc).unwrap();
    let unused = scheduling_runtime::pin_published_funding(stamp, 0, 94011, 0xdef).unwrap();
    install(
        0,
        ExecutionInterval {
            slot: 60,
            task_id: 94012,
            running_generation: 9,
            start_ticks: 1,
            start_local_ns: 0, charge_from_ticks: 1, charge_from_local_ns: 0,
            funding: Some(active),
        },
    );
    let foreign = ExecutionInterval {
        slot: 63, task_id: 94014, running_generation: 10,
        start_ticks: 1, start_local_ns: 0, charge_from_ticks: 1, charge_from_local_ns: 0, funding: None,
    };
    install(1, foreign);
    drop(unused);
    assert!(with_interval(1, |cell| cell.active.as_ref().is_some_and(|interval|
        interval.slot == 63 && interval.task_id == 94014)));
    assert!(!scheduling_runtime::close_for_reclaim(61, identity));
    let ConsumedInterval::Active(interval) = consume(0, 60, 94012) else {
        panic!("return cancellation spent the executing interval");
    };
    assert_eq!(interval.running_generation, 9);
    drop(interval);
    assert!(scheduling_runtime::close_for_reclaim(61, identity));
    assert!(matches!(consume(1, 63, 94014), ConsumedInterval::Active(_)));
    scheduling_runtime::reset_for_test();
}

#[test]
fn changed_return_requires_exact_running_identity_generation_and_active_interval() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    scheduling_runtime::reset_for_test();
    runqueue::reset_before_publication();
    let (stamp, _, lender) = runtime(61, 94021);
    let grant = scheduling_runtime::pin_published_funding(stamp, 0, 94021, 1).unwrap();
    let slot = 60;
    current_identity::clear(slot);
    current_identity::publish_metadata(
        slot,
        Some(current_identity::TaskIdentity {
            task_id: Some(94022),
            ..Default::default()
        }),
    );
    runqueue::admit_running(slot, 0);
    let generation = runqueue::owner(slot).generation;
    // Earlier original claim receipt is projected only for this bank-match
    // unit witness. Physical turn/claim qualification has separate real roots.
    let refused = || std::panic::catch_unwind(||
        require_exact_interval(0, slot, 94022, generation)).is_err();
    assert!(refused()); // a scratch change alone cannot open an interval
    for (saved_slot, id, saved_generation) in [
        (slot + 1, 94022, generation),
        (slot, 94023, generation),
        (slot, 94022, generation + 2),
    ] {
        install(
            0,
            ExecutionInterval {
                slot: saved_slot,
                task_id: id,
                running_generation: saved_generation,
                start_ticks: 0,
                start_local_ns: 0, charge_from_ticks: 0, charge_from_local_ns: 0,
                funding: None,
            },
        );
        assert!(refused());
        assert!(matches!(
            consume(0, saved_slot, id),
            ConsumedInterval::Active(_)
        ));
    }
    assert!(!scheduling_runtime::close_for_reclaim(61, lender)); // failed return did not discard the candidate
    install(
        0,
        ExecutionInterval {
            slot,
            task_id: 94022,
            running_generation: generation,
            start_ticks: 0,
            start_local_ns: 0, charge_from_ticks: 0, charge_from_local_ns: 0,
            funding: None,
        },
    );
    require_exact_interval(0, slot, 94022, generation);
    drop(grant);
    assert!(scheduling_runtime::close_for_reclaim(61, lender));
    assert!(matches!(
        consume(0, slot, 94022),
        ConsumedInterval::Active(_)
    ));
    current_identity::clear(slot);
    scheduling_runtime::reset_for_test();
}
