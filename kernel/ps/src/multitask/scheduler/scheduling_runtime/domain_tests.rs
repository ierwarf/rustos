//! Owner: real domain budget descriptor and independent refusal witnesses.
//! Boundary: injected incomplete owner is not normal-state or hardware refinement.
//! Lifecycle: generation, members and interval units must each retain custody.
//! Concurrency: shared runtime fixture; scalar owner leaves only.
//! Failure: incomplete authority is never reusable absence.
//! Forbidden: native recovery is not production settlement or epoch permission.
//! Evidence: native and focused implementation mutation witnesses.
use super::*;
const SLOT: usize = 63;
fn policy(epoch: u64) -> SchedulingContextPolicy {
    SchedulingContextPolicy {
        budget_ns: 4_000,
        period_ns: 10_000,
        refill_capacity: 4,
        cpu_mask: 1,
        criticality: 1,
        domain: 7,
        policy_epoch: epoch,
        timeout_endpoint_cap: 0,
    }
}
#[test]
fn domain_descriptor_tracks_the_budget_owner_not_a_stale_catalog_copy() {
    let _serial = test_serial_guard();
    reset_for_test();
    assert!(domain_descriptor(SLOT).is_none());
    let old = policy(3);
    let first = initialize_domain(SLOT, old).unwrap();
    let snapshot = domain_descriptor(SLOT).unwrap();
    assert_eq!((snapshot.policy(), snapshot.generation()), (old, first));
    let next = policy(4);
    let second = replace_domain(SLOT, old, first, next).unwrap();
    assert_ne!(first, second);
    let replacement = domain_descriptor(SLOT).unwrap();
    assert_eq!(
        (replacement.policy(), replacement.generation()),
        (next, second)
    );
    assert!(!domain_matches(
        SLOT,
        snapshot.generation(),
        snapshot.policy()
    ));
    assert!(replace_domain(SLOT, old, first, old).is_none());
    assert_eq!(domain_descriptor(SLOT).unwrap().generation(), second);
    reset_for_test();
}
#[test]
fn incomplete_domain_budget_cannot_be_projected_as_reusable_absence() {
    let _serial = test_serial_guard();
    reset_for_test();
    initialize_domain(SLOT, policy(3)).unwrap();
    let budget = domain_cell(SLOT).unwrap().lock().budget.take();
    assert!(std::panic::catch_unwind(|| domain_descriptor(SLOT)).is_err());
    domain_cell(SLOT).unwrap().lock().budget = budget;
    assert!(domain_descriptor(SLOT).is_some());
    reset_for_test();
}

#[test]
fn captured_interval_without_member_or_catalog_still_refuses_domain_replacement() {
    let _serial = test_serial_guard();
    reset_for_test();
    let old = policy(3);
    let generation = initialize_domain(63, old).unwrap();
    // Fault witness: normally context membership dominates this check. An
    // orphaned/incomplete owner is not absence and must fail closed separately.
    {
        let mut domain = domain_cell(63).unwrap().lock();
        assert_eq!(domain.members, 0);
        domain.active_intervals = 1;
    }
    let next = policy(4);
    assert!(super::super::domain_admission::admit(next).is_none());
    assert_eq!(domain_descriptor(63).unwrap().generation(), generation);
    assert_eq!(domain_descriptor(63).unwrap().policy(), old);
    // Native fault recovery, not a production settlement or hardware ACK.
    domain_cell(63).unwrap().lock().active_intervals = 0;
    let replacement = super::super::domain_admission::admit(next).unwrap();
    assert_ne!(replacement.generation(), generation);
    drop(replacement);
    reset_for_test();
}
