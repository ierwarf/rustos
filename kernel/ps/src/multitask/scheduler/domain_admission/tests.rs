//! Owner: real cold domain admission regression witnesses.
//! Boundary: budget/member authority, never a legacy catalog fixture grant.
//! Lifecycle: grants bridge creation; exact context release ends membership.
//! Concurrency: native fixture isolation and competing admission threads.
//! Failure: quota/full/generation/member refusals preserve prior authority.
//! Forbidden: test recovery is not a production epoch reset or hardware ACK.
//! Evidence: bounded native witnesses, not full class/return refinement.
use super::super::scheduling_context::SchedulingContext;
use super::*;
fn policy(domain: u64, epoch: u64) -> SchedulingContextPolicy {
    SchedulingContextPolicy {
        budget_ns: 4000,
        period_ns: 10000,
        refill_capacity: 4,
        cpu_mask: 1,
        criticality: 1,
        domain,
        policy_epoch: epoch,
        timeout_endpoint_cap: 0,
    }
}
#[test]
fn captured_domain_selection_excludes_replacement_before_context_and_duplicate_grants_drain_once() {
    let _fixture = super::super::tests::boxed_scheduler();
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
    scope.mask_irqs();
    let cpu = scope.cpu();
    let p = policy(900, 1);
    let first = admit(p).unwrap();
    let slot = first.slot();
    let generation = first.generation();
    let second = admit(p).unwrap();
    assert_eq!(second.slot(), slot);
    assert_eq!(second.generation(), generation);
    assert!(admit(policy(900, 2)).is_none());
    drop(first);
    assert!(admit(policy(900, 2)).is_none());
    drop(second);
    let replacement = admit(policy(900, 2)).unwrap();
    assert_eq!(replacement.slot(), slot);
    assert_ne!(replacement.generation(), generation);
    assert_eq!(nucleus_core::util::lockdep::DispatchPreemption::model_depth_on(cpu), 1);
    drop(scope);
}
#[test]
fn live_context_member_blocks_policy_reuse_after_capture_drop_and_close_until_exact_release() {
    let _fixture = super::super::tests::boxed_scheduler();
    let p = policy(901, 1);
    let grant = admit(p).unwrap();
    let mut context = SchedulingContext::bind(100, 0);
    assert!(context.admit(p, grant.slot(), grant.generation()));
    drop(grant);
    assert!(admit(policy(901, 2)).is_none());
    assert!(scheduling_runtime::close_for_reclaim(
        100,
        context.identity()
    ));
    assert!(admit(policy(901, 2)).is_none());
    let foreign = SchedulingContext::bind(100, 1).identity();
    scheduling_runtime::release(100, foreign);
    assert!(admit(policy(901, 2)).is_none());
    scheduling_runtime::release(100, context.identity());
    assert!(admit(policy(901, 2)).is_some());
}
#[test]
fn context_admission_refuses_foreign_domain_generation_policy_and_identity_without_spending_members()
 {
    let _fixture = super::super::tests::boxed_scheduler();
    let p = policy(902, 1);
    let grant = admit(p).unwrap();
    let slot = grant.slot();
    let generation = grant.generation();
    let mut context = SchedulingContext::bind(101, 101001);
    assert!(!context.admit(p, slot, generation + 1));
    assert!(!context.admit(policy(902, 2), slot, generation));
    assert!(context.admit(p, slot, generation));
    let mut peer = SchedulingContext::bind(101, 101002);
    assert!(!peer.admit(p, slot, generation));
    drop(grant);
    scheduling_runtime::release(101, peer.identity());
    assert!(admit(policy(902, 2)).is_none());
    scheduling_runtime::release(101, context.identity());
    let next = admit(policy(902, 2)).unwrap();
    assert!(!peer.admit(p, slot, generation));
    assert!(peer.admit(policy(902, 2), slot, next.generation()));
    scheduling_runtime::release(101, context.identity());
    assert!(admit(policy(902, 3)).is_none());
    scheduling_runtime::release(101, peer.identity());
    drop(next);
    assert!(admit(policy(902, 3)).is_some());
}
#[test]
pub(in crate::multitask::scheduler) fn deadline_quota_sums_exact_overlapping_cpu_masks_and_rounds_up_without_partial_replacement() {
    let _fixture = super::super::tests::boxed_scheduler();
    let mut a = policy(903, 1);
    a.criticality = 2;
    a.budget_ns = 6000;
    let first = admit(a).unwrap();
    let mut b = a;
    b.domain = 904;
    b.cpu_mask = 2;
    let second = admit(b).unwrap();
    let mut both = a;
    both.domain = 905;
    both.cpu_mask = 3;
    both.budget_ns = 4000;
    assert!(admit(both).is_none());
    assert_eq!(
        scheduling_runtime::domain_descriptor(first.slot())
            .unwrap()
            .policy(),
        a
    );
    drop(first);
    drop(second);
    let mut invalid = a;
    invalid.period_ns = 0;
    assert!(admit(invalid).is_none());
    // 3001/10000 + 6000/10000 exceeds 90%, including ceiling rounding.
    both.cpu_mask = 1;
    both.budget_ns = 3001;
    assert!(admit(both).is_none());
    both.budget_ns = 3000;
    assert!(admit(both).is_some());
}
#[test]
fn competing_deadline_domains_cannot_both_spend_one_cpu_utilization_capacity() {
    let _fixture = super::super::tests::boxed_scheduler();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut workers = std::vec::Vec::new();
    for domain in [906, 907] {
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let mut p = policy(domain, 1);
            p.criticality = 2;
            p.budget_ns = 6000;
            barrier.wait();
            admit(p)
        }));
    }
    let admitted: std::vec::Vec<_> = workers
        .into_iter()
        .filter_map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(admitted.len(), 1);
}
#[test]
fn exhausted_domain_generation_refuses_before_descriptor_publication_and_never_wraps() {
    let _fixture = super::super::tests::boxed_scheduler();
    let old = scheduling_runtime::set_domain_generation_for_test(0, u32::MAX);
    assert!(admit(policy(908, 1)).is_none());
    assert!(scheduling_runtime::domain_descriptor(0).is_none());
    // ORDERING: isolated fixture observation/recovery never supplies production admission.
    assert_eq!(
        scheduling_runtime::set_domain_generation_for_test(0, old),
        u32::MAX
    );
}
#[test]
fn full_registry_refuses_new_domain_without_spending_existing_capture_or_policy_generation() {
    let _fixture = super::super::tests::boxed_scheduler();
    let mut grants = std::vec::Vec::new();
    for id in 0..scheduling_context::MAX_SCHEDULING_DOMAINS {
        grants.push(admit(policy(1000 + id as u64, 1)).unwrap());
    }
    assert!(admit(policy(2000, 1)).is_none());
    let duplicate = admit(policy(1000, 1)).unwrap();
    assert_eq!(duplicate.generation(), grants[0].generation());
    assert!(admit(policy(1000, 2)).is_none());
    drop(duplicate);
    assert!(admit(policy(1000, 2)).is_none());
}
