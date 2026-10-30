use super::*;
use alloc::vec::Vec;

fn ledger() -> (
    FrameDescriptorLedger,
    Vec<FrameDescriptorRecord>,
    Vec<CowMappingRecord>,
) {
    let mut records: Vec<FrameDescriptorRecord> =
        (0..16).map(|_| FrameDescriptorRecord::empty()).collect();
    let mut mappings: Vec<CowMappingRecord> = (0..16).map(|_| CowMappingRecord::empty()).collect();
    let ledger = FrameDescriptorLedger::empty();
    ledger.install(
        records.as_mut_ptr(),
        records.len(),
        mappings.as_mut_ptr(),
        mappings.len(),
    );
    (ledger, records, mappings)
}

#[test]
fn one_catalog_tracks_tables_and_exact_data_leaves() {
    assert_eq!(core::mem::size_of::<FrameDescriptorRecord>(), 40);
    assert_eq!(core::mem::size_of::<CowMappingRecord>(), 24);
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let table = PAGE_SIZE * 2;
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_4000;
    ledger.register_root(root);
    assert!(ledger.claim_table(root, table));
    ledger.publish_table(root, table);
    assert!(ledger.claim_exclusive_data(root, va, frame));
    ledger.publish_exclusive_data(root, va, frame);
    assert!(ledger.data_is_owned(root, va, frame));
    assert_eq!(
        ledger.release_data(root, va, frame),
        Some(DataLeafRelease::FrameReusable)
    );
    assert!(ledger.table_is_owned(root, table));
    assert!(
        !ledger.data_is_owned(root, va, table),
        "a table frame is never also a data leaf"
    );
    assert_eq!(ledger.table_count(root), 1);
    assert_eq!(ledger.pop_table(root), Some(table));
    assert_eq!(ledger.pop_table(root), None);
    assert_eq!(ledger.table_count(root), 0);
    assert!(!ledger.table_is_owned(root, table));
    ledger.unregister_root(root);
}

#[test]
fn anonymous_aliases_are_exact_and_free_only_after_the_last_mapping() {
    let (ledger, _records, _mappings) = ledger();
    let parent = PAGE_SIZE;
    let child = PAGE_SIZE * 2;
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_4000;
    ledger.register_root(parent);
    ledger.register_root(child);
    assert!(ledger.claim_exclusive_data(parent, va, frame));
    ledger.publish_exclusive_data(parent, va, frame);
    let ticket = ledger
        .prepare_shared_alias(parent, va, child, va, frame, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(ticket);
    assert_eq!(
        ledger.cow_identity(parent, va, frame),
        Some((CowFrameKind::AnonymousFork, 0))
    );
    assert_eq!(
        ledger.cow_identity(child, va, frame),
        Some((CowFrameKind::AnonymousFork, 0))
    );
    assert!(ledger.data_is_owned(parent, va, frame));
    assert!(ledger.data_is_owned(child, va, frame));
    assert!(!ledger.data_is_owned(child, va + PAGE_SIZE, frame));
    assert_eq!(
        ledger.release_data(parent, va, frame),
        Some(DataLeafRelease::FrameRetained)
    );
    assert!(!ledger.data_is_owned(parent, va, frame));
    assert!(ledger.data_is_owned(child, va, frame));
    assert_eq!(
        ledger.release_data(child, va, frame),
        Some(DataLeafRelease::FrameReusable)
    );
    ledger.unregister_root(parent);
    ledger.unregister_root(child);
}

#[test]
fn failed_fork_alias_rolls_back_to_the_exact_exclusive_owner() {
    let (ledger, _records, _mappings) = ledger();
    let parent = PAGE_SIZE;
    let child = PAGE_SIZE * 2;
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_4000;
    ledger.register_root(parent);
    ledger.register_root(child);
    assert!(ledger.claim_exclusive_data(parent, va, frame));
    ledger.publish_exclusive_data(parent, va, frame);
    let ticket = ledger
        .prepare_shared_alias(parent, va, child, va, frame, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(ticket);
    assert!(ledger.rollback_shared_alias(ticket));
    assert!(ledger.data_is_owned(parent, va, frame));
    assert!(!ledger.data_is_owned(child, va, frame));
    assert_eq!(
        ledger.release_data(parent, va, frame),
        Some(DataLeafRelease::FrameReusable)
    );
    ledger.unregister_root(parent);
    ledger.unregister_root(child);
}

#[test]
fn anonymous_cow_removes_one_alias_then_promotes_the_survivor_in_place() {
    let (ledger, _records, _mappings) = ledger();
    let parent = PAGE_SIZE;
    let child = PAGE_SIZE * 2;
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_4000;
    ledger.register_root(parent);
    ledger.register_root(child);
    assert!(ledger.claim_exclusive_data(parent, va, frame));
    ledger.publish_exclusive_data(parent, va, frame);
    let ticket = ledger
        .prepare_shared_alias(parent, va, child, va, frame, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(ticket);

    let child_claim = ledger.try_claim_cow_mapping(child, va, frame).unwrap();
    assert!(!child_claim.can_promote_in_place());
    assert_eq!(
        ledger.commit_cow_mapping(child_claim),
        DataLeafRelease::FrameRetained
    );
    assert!(!ledger.data_is_owned(child, va, frame));

    let parent_claim = ledger.try_claim_cow_mapping(parent, va, frame).unwrap();
    assert!(parent_claim.can_promote_in_place());
    ledger.promote_cow_mapping_in_place(parent_claim);
    assert!(ledger.data_is_owned(parent, va, frame));
    assert_eq!(
        ledger.release_data(parent, va, frame),
        Some(DataLeafRelease::FrameReusable)
    );
    ledger.unregister_root(parent);
    ledger.unregister_root(child);
}

#[test]
fn private_file_role_requires_and_preserves_a_backing_identity() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let frame = PAGE_SIZE * 2;
    let va = 0x8000_8000;
    ledger.register_root(root);
    assert!(!ledger.claim_private_file_data(root, va, frame, 0));
    assert!(ledger.claim_private_file_data(root, va, frame, 41));
    ledger.publish_private_file_data(root, va, frame);
    assert_eq!(
        ledger.cow_identity(root, va, frame),
        Some((CowFrameKind::PrivateFileSection, 41))
    );
    let claim = ledger.try_claim_cow_mapping(root, va, frame).unwrap();
    assert_eq!(claim.kind(), CowFrameKind::PrivateFileSection);
    assert!(!claim.can_promote_in_place());
    ledger.cancel_cow_mapping(claim);
    assert_eq!(
        ledger.release_data(root, va, frame),
        Some(DataLeafRelease::FrameReusable)
    );
    ledger.unregister_root(root);
}

// Charge groups are process-wide statics; each test below owns distinct indices.

#[test]
fn publishing_and_releasing_data_leaves_charge_and_return_the_bound_group() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let (f1, f2) = (PAGE_SIZE * 3, PAGE_SIZE * 4);
    crate::memory::charge::init_group(10, None);
    ledger.register_root(root);
    assert!(ledger.bind_charge(root, 10));
    assert!(!ledger.bind_charge(root, 11), "a root is bound once");
    for (va, frame) in [(0x8000_4000, f1), (0x8000_5000, f2)] {
        assert!(ledger.claim_exclusive_data(root, va, frame));
        assert_eq!(crate::memory::charge::usage(10), if frame == f1 { 0 } else { 1 },
            "a claim alone is not charged; publication is");
        ledger.publish_exclusive_data(root, va, frame);
    }
    assert_eq!(crate::memory::charge::usage(10), 2);
    ledger.release_data(root, 0x8000_4000, f1).unwrap();
    assert_eq!(crate::memory::charge::usage(10), 1);
    ledger.release_data(root, 0x8000_5000, f2).unwrap();
    assert_eq!(crate::memory::charge::usage(10), 0);
    ledger.unregister_root(root);
}

#[test]
fn binding_a_root_that_already_holds_leaves_charges_what_it_holds() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let frame = PAGE_SIZE * 3;
    crate::memory::charge::init_group(12, None);
    ledger.register_root(root);
    assert!(ledger.claim_exclusive_data(root, 0x8000_4000, frame));
    ledger.publish_exclusive_data(root, 0x8000_4000, frame);
    assert_eq!(crate::memory::charge::usage(12), 0, "unbound roots charge nothing");
    assert!(ledger.bind_charge(root, 12));
    assert_eq!(crate::memory::charge::usage(12), 1, "binding catches up the held leaf");
    ledger.release_data(root, 0x8000_4000, frame).unwrap();
    assert_eq!(crate::memory::charge::usage(12), 0);
    ledger.unregister_root(root);
}

#[test]
fn shared_aliases_charge_each_mapping_root_and_a_cow_break_nets_zero() {
    let (ledger, _records, _mappings) = ledger();
    let (parent, child) = (PAGE_SIZE, PAGE_SIZE * 2);
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_4000;
    crate::memory::charge::init_group(13, None);
    crate::memory::charge::init_group(14, None);
    ledger.register_root(parent);
    ledger.register_root(child);
    assert!(ledger.bind_charge(parent, 13));
    assert!(ledger.bind_charge(child, 14));
    assert!(ledger.claim_exclusive_data(parent, va, frame));
    ledger.publish_exclusive_data(parent, va, frame);
    let ticket = ledger
        .prepare_shared_alias(parent, va, child, va, frame, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(ticket);
    assert_eq!(
        (crate::memory::charge::usage(13), crate::memory::charge::usage(14)),
        (1, 1),
        "each root that maps the frame is charged for its own mapping"
    );
    // Releasing the child's mapping returns only the child's group.
    let claim = ledger.try_claim_cow_mapping(child, va, frame).unwrap();
    ledger.cancel_cow_mapping(claim);
    ledger.release_data(child, va, frame).unwrap();
    assert_eq!(
        (crate::memory::charge::usage(13), crate::memory::charge::usage(14)),
        (1, 0)
    );
    ledger.release_data(parent, va, frame).unwrap();
    assert_eq!(crate::memory::charge::usage(13), 0);
    ledger.unregister_root(parent);
    ledger.unregister_root(child);
}

#[test]
fn root_admission_follows_the_bound_groups_limit_and_ignores_unbound_roots() {
    let (ledger, _records, _mappings) = ledger();
    let (bound, unbound) = (PAGE_SIZE, PAGE_SIZE * 2);
    crate::memory::charge::init_group(15, None);
    ledger.register_root(bound);
    ledger.register_root(unbound);
    assert!(ledger.root_admits(bound, 100), "no group, no limit");
    assert!(ledger.bind_charge(bound, 15));
    crate::memory::charge::set_limit(15, 2);
    assert!(ledger.root_admits(bound, 2));
    assert!(!ledger.root_admits(bound, 3));
    assert!(ledger.root_admits(unbound, u64::MAX), "an unbound root is never limited");
    assert!(ledger.root_admits(PAGE_SIZE * 9, u64::MAX), "an unknown root is never limited");
    ledger.unregister_root(bound);
    ledger.unregister_root(unbound);
}

#[test]
fn unregistering_a_root_clears_its_binding_so_the_record_can_be_reused() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    crate::memory::charge::init_group(16, None);
    ledger.register_root(root);
    assert!(ledger.bind_charge(root, 16));
    ledger.unregister_root(root);
    ledger.register_root(root); // asserts a clean record
    assert!(ledger.bind_charge(root, 16), "a reused root can bind again");
    ledger.unregister_root(root);
}

#[test]
fn private_file_leaves_are_charged_like_any_other_data_leaf() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let frame = PAGE_SIZE * 2;
    crate::memory::charge::init_group(17, None);
    ledger.register_root(root);
    assert!(ledger.bind_charge(root, 17));
    assert!(ledger.claim_private_file_data(root, 0x8000_8000, frame, 41));
    assert_eq!(crate::memory::charge::usage(17), 0);
    ledger.publish_private_file_data(root, 0x8000_8000, frame);
    assert_eq!(crate::memory::charge::usage(17), 1);
    ledger.release_data(root, 0x8000_8000, frame).unwrap();
    assert_eq!(crate::memory::charge::usage(17), 0);
    ledger.unregister_root(root);
}

#[test]
fn a_claim_that_is_cancelled_never_reaches_the_group() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let frame = PAGE_SIZE * 2;
    crate::memory::charge::init_group(18, None);
    ledger.register_root(root);
    assert!(ledger.bind_charge(root, 18));
    assert!(ledger.claim_exclusive_data(root, 0x8000_4000, frame));
    ledger.cancel_exclusive_data(root, 0x8000_4000, frame);
    assert_eq!(crate::memory::charge::usage(18), 0, "charging happens at publication");
    ledger.unregister_root(root);
}
