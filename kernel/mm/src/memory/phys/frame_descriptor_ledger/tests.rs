extern crate std;

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
    assert_eq!(ledger.claim_table(root, table), TableClaim::Claimed);
    ledger.publish_table(root, table);
    assert_eq!(ledger.claim_exclusive_data(root, va, frame, true), DataClaim::Claimed);
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
fn cache_custody_survives_last_unmap_and_can_be_mapped_again() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_4000;
    ledger.register_root(root);
    assert!(ledger.claim_private_file_cache(frame, 41));
    assert!(!ledger.claim_private_file_cache(frame, 41));
    let ticket = ledger.prepare_shared_alias(0, 0, root, va, frame, CowFrameKind::PrivateFileSection, 41).unwrap();
    ledger.publish_shared_alias(ticket);
    assert_eq!(ledger.release_data(root, va, frame), Some(DataLeafRelease::FrameRetained));
    let ticket = ledger.prepare_shared_alias(0, 0, root, va, frame, CowFrameKind::PrivateFileSection, 41).unwrap();
    ledger.publish_shared_alias(ticket);
    assert_eq!(ledger.release_private_file_cache(frame, 42), None);
    assert_eq!(ledger.release_private_file_cache(frame, 41), Some(DataLeafRelease::FrameRetained));
    assert!(ledger.prepare_shared_alias(0, 0, root, va + PAGE_SIZE, frame, CowFrameKind::PrivateFileSection, 41).is_none());
    assert_eq!(ledger.release_data(root, va, frame), Some(DataLeafRelease::FrameReusable));
    ledger.unregister_root(root);
}

#[test]
fn cache_custody_survives_private_cow_and_cancelled_alias() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_4000;
    ledger.register_root(root);
    assert!(ledger.claim_private_file_cache(frame, 41));
    let ticket = ledger.prepare_shared_alias(0, 0, root, va, frame, CowFrameKind::PrivateFileSection, 41).unwrap();
    ledger.cancel_shared_alias(ticket);
    assert!(!ledger.data_is_owned(root, va, frame));
    let ticket = ledger.prepare_shared_alias(0, 0, root, va, frame, CowFrameKind::PrivateFileSection, 41).unwrap();
    ledger.publish_shared_alias(ticket);
    let claim = ledger.try_claim_cow_mapping(root, va, frame).unwrap();
    assert!(!claim.can_promote_in_place());
    let replacement = PAGE_SIZE * 4;
    assert_eq!(ledger.claim_exclusive_data(root, va, replacement, false), DataClaim::Claimed);
    ledger.publish_exclusive_data(root, va, replacement);
    assert_eq!(ledger.commit_cow_mapping(claim), DataLeafRelease::FrameRetained);
    assert_eq!(ledger.release_private_file_cache(frame, 41), Some(DataLeafRelease::FrameReusable));
    assert_eq!(ledger.release_data(root, va, replacement), Some(DataLeafRelease::FrameReusable));
    ledger.unregister_root(root);
}

#[test]
fn cache_custody_without_mappings_releases_frame_once() {
    let (ledger, _records, _mappings) = ledger();
    let frame = PAGE_SIZE * 3;
    assert!(ledger.claim_private_file_cache(frame, 41));
    assert_eq!(ledger.release_private_file_cache(frame, 41), Some(DataLeafRelease::FrameReusable));
    assert_eq!(ledger.release_private_file_cache(frame, 41), None);
}

#[test]
fn invalid_alias_frame_does_not_consume_mapping_records() {
    let (ledger, _records, _mappings) = ledger();
    let parent = PAGE_SIZE;
    let child = PAGE_SIZE * 2;
    let va = 0x8000_4000;
    ledger.register_root(parent);
    ledger.register_root(child);
    let head = ledger.free_mapping_head.load(DESCRIPTOR_ACQUIRE);
    for frame in [PAGE_SIZE * 16, PAGE_SIZE * 3 + 1] {
        assert!(ledger
            .prepare_shared_alias(parent, va, child, va, frame, CowFrameKind::AnonymousFork, 0)
            .is_none());
        assert_eq!(ledger.free_mapping_head.load(DESCRIPTOR_ACQUIRE), head);
    }
    ledger.unregister_root(parent);
    ledger.unregister_root(child);
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
    assert_eq!(ledger.claim_exclusive_data(parent, va, frame, true), DataClaim::Claimed);
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
    assert_eq!(ledger.claim_exclusive_data(parent, va, frame, true), DataClaim::Claimed);
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
fn alias_removal_can_retain_role_lock_until_exclusive_conversion() {
    let (ledger, _records, _mappings) = ledger();
    let parent = PAGE_SIZE;
    let child = PAGE_SIZE * 2;
    let frame_phys = PAGE_SIZE * 3;
    let va = 0x8000_4000;
    ledger.register_root(parent);
    ledger.register_root(child);
    assert_eq!(ledger.claim_exclusive_data(parent, va, frame_phys, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(parent, va, frame_phys);
    let ticket = ledger
        .prepare_shared_alias(parent, va, child, va, frame_phys, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(ticket);
    let frame = ledger.record(frame_phys).unwrap();
    assert!(ledger.lock_shared(frame, CowFrameKind::AnonymousFork, false));
    assert_eq!(
        ledger.remove_shared_mapping_locked(
            frame, child, va, ticket.mapping_id, CowFrameKind::AnonymousFork, false,
        ),
        DataLeafRelease::FrameRetained
    );
    assert_eq!(frame.role.load(DESCRIPTOR_ACQUIRE), ROLE_SHARED_ANONYMOUS_LOCKED);
    // Finish the rollback's conversion and accounting before releasing the fixture.
    frame.data_leaf_count_or_map_head.store(0, Ordering::Relaxed);
    frame.role.store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
    ledger.decrement_root_data_count(child);
    assert_eq!(ledger.release_data(parent, va, frame_phys), Some(DataLeafRelease::FrameReusable));
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
    assert_eq!(ledger.claim_exclusive_data(parent, va, frame, true), DataClaim::Claimed);
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
    assert_eq!(ledger.claim_private_file_data(root, va, frame, 0), DataClaim::Invalid);
    assert_eq!(ledger.claim_private_file_data(root, va, frame, 41), DataClaim::Claimed);
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
        assert_eq!(ledger.claim_exclusive_data(root, va, frame, true), DataClaim::Claimed);
        assert_eq!(crate::memory::charge::usage(10), if frame == f1 { 1 } else { 2 },
            "claims reserve quota before publication");
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
    assert_eq!(ledger.claim_exclusive_data(root, 0x8000_4000, frame, true), DataClaim::Claimed);
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
    assert_eq!(ledger.claim_exclusive_data(parent, va, frame, true), DataClaim::Claimed);
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
    assert_eq!(ledger.claim_private_file_data(root, 0x8000_8000, frame, 41), DataClaim::Claimed);
    assert_eq!(crate::memory::charge::usage(17), 1);
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
    assert_eq!(ledger.claim_exclusive_data(root, 0x8000_4000, frame, true), DataClaim::Claimed);
    ledger.cancel_exclusive_data(root, 0x8000_4000, frame, true);
    assert_eq!(crate::memory::charge::usage(18), 0, "charging happens at publication");
    ledger.unregister_root(root);
}

#[test]
fn page_table_frames_are_billed_at_claim_and_refunded_exactly_once() {
    use crate::memory::kmem_charge::{self, Class};
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let (t1, t2, t3) = (PAGE_SIZE * 2, PAGE_SIZE * 3, PAGE_SIZE * 4);
    kmem_charge::init_group(20, None);
    ledger.register_root(root);
    assert_eq!(ledger.claim_table(root, t1), TableClaim::Claimed);
    ledger.publish_table(root, t1);
    assert_eq!(kmem_charge::usage(Class::PageTable, 20), 0, "unbound roots charge nothing");
    assert!(ledger.bind_charge(root, 20));
    assert_eq!(
        kmem_charge::usage(Class::PageTable, 20),
        2 * PAGE_SIZE,
        "binding catches up the root frame and the published table"
    );
    kmem_charge::set_limit(Class::PageTable, 20, 3 * PAGE_SIZE);
    assert_eq!(ledger.claim_table(root, t2), TableClaim::Claimed);
    assert_eq!(
        ledger.claim_table(root, t3),
        TableClaim::Quota,
        "the Job's page-table budget refuses the fourth frame"
    );
    assert_eq!(kmem_charge::usage(Class::PageTable, 20), 3 * PAGE_SIZE);
    ledger.cancel_table(root, t2);
    assert_eq!(kmem_charge::usage(Class::PageTable, 20), 2 * PAGE_SIZE, "cancel refunds");
    assert_eq!(ledger.claim_table(root, t3), TableClaim::Claimed);
    ledger.publish_table(root, t3);
    while ledger.pop_table(root).is_some() {}
    assert_eq!(kmem_charge::usage(Class::PageTable, 20), PAGE_SIZE, "only the root remains");
    ledger.unregister_root(root);
    assert_eq!(kmem_charge::usage(Class::PageTable, 20), 0);
}

#[test]
fn a_failed_table_claim_does_not_leak_its_charge() {
    use crate::memory::kmem_charge::{self, Class};
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let table = PAGE_SIZE * 2;
    kmem_charge::init_group(21, None);
    ledger.register_root(root);
    assert!(ledger.bind_charge(root, 21));
    assert_eq!(ledger.claim_table(root, table), TableClaim::Claimed);
    assert_eq!(
        ledger.claim_table(root, table),
        TableClaim::Invalid,
        "a frame already claimed is not claimable twice"
    );
    assert_eq!(kmem_charge::usage(Class::PageTable, 21), 2 * PAGE_SIZE);
    ledger.cancel_table(root, table);
    ledger.unregister_root(root);
    assert_eq!(kmem_charge::usage(Class::PageTable, 21), 0);
}

#[test]
fn quota_claims_cancel_and_cow_replacement_transfer_one_existing_charge() {
    use crate::memory::charge;
    let (ledger, _records, _mappings) = ledger();
    let (parent, child, source, replacement) = (PAGE_SIZE, PAGE_SIZE * 2, PAGE_SIZE * 3, PAGE_SIZE * 4);
    let va = 0x8000_4000;
    charge::init_group(30, None);
    charge::init_group(31, Some(30));
    ledger.register_root(parent);
    ledger.register_root(child);
    assert!(ledger.bind_charge(parent, 30));
    assert!(ledger.bind_charge(child, 31));
    charge::set_limit(30, 2);
    assert_eq!(ledger.claim_exclusive_data(parent, va, source, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(parent, va, source);
    let alias = ledger.prepare_shared_alias(parent, va, child, va, source, CowFrameKind::AnonymousFork, 0).unwrap();
    ledger.publish_shared_alias(alias);
    assert_eq!(charge::usage(30), 2);
    assert_eq!(ledger.claim_exclusive_data(child, va + PAGE_SIZE, replacement, true), DataClaim::Quota);
    let cow = ledger.try_claim_cow_mapping(child, va, source).unwrap();
    assert_eq!(ledger.claim_exclusive_data(child, va, replacement, false), DataClaim::Claimed);
    ledger.cancel_exclusive_data(child, va, replacement, false);
    assert_eq!(charge::usage(30), 2);
    assert_eq!(ledger.claim_exclusive_data(child, va, replacement, false), DataClaim::Claimed);
    ledger.publish_exclusive_data(child, va, replacement);
    assert_eq!(ledger.commit_cow_mapping(cow), DataLeafRelease::FrameRetained);
    assert_eq!(charge::usage(30), 2);
    ledger.release_data(child, va, replacement).unwrap();
    ledger.release_data(parent, va, source).unwrap();
    assert_eq!(charge::usage(30), 0);
    ledger.unregister_root(child);
    ledger.unregister_root(parent);
}

#[test]
fn root_binding_refuses_existing_pages_without_leaking_a_reservation() {
    use crate::memory::charge;
    let (ledger, _records, _mappings) = ledger();
    let (root, frame, va) = (PAGE_SIZE, PAGE_SIZE * 2, 0x8000_4000);
    charge::init_group(32, None);
    charge::set_limit(32, 0);
    ledger.register_root(root);
    assert_eq!(ledger.claim_exclusive_data(root, va, frame, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(root, va, frame);
    assert!(!ledger.bind_charge(root, 32));
    assert_eq!(charge::usage(32), 0);
    charge::set_limit(32, 1);
    assert!(ledger.bind_charge(root, 32));
    ledger.release_data(root, va, frame).unwrap();
    ledger.unregister_root(root);
    assert_eq!(charge::usage(32), 0);
}

#[test]
fn concurrent_claims_report_quota_as_out_of_frames_and_refund_exactly() {
    use crate::memory::{address_space::{data_claim_result, AddressSpaceError}, charge};
    use std::sync::Barrier;
    let (ledger, _records, _mappings) = ledger();
    let (root_a, root_b) = (PAGE_SIZE, PAGE_SIZE * 2);
    let (frame_a, frame_b) = (PAGE_SIZE * 3, PAGE_SIZE * 4);
    let va = 0x8000_4000;
    charge::init_group(36, None);
    charge::init_group(37, Some(36));
    charge::init_group(38, Some(36));
    ledger.register_root(root_a);
    ledger.register_root(root_b);
    assert!(ledger.bind_charge(root_a, 37));
    assert!(ledger.bind_charge(root_b, 38));
    charge::set_limit(36, 1);
    // Force both callers past the old advisory check before either claims.
    assert!(ledger.root_admits(root_a, 1));
    assert!(ledger.root_admits(root_b, 1));
    let ready = Barrier::new(3);
    let release = Barrier::new(3);
    std::thread::scope(|scope| {
        let mut workers = std::vec::Vec::new();
        for (root, frame) in [(root_a, frame_a), (root_b, frame_b)] {
            let (ledger, ready, release) = (&ledger, &ready, &release);
            workers.push(scope.spawn(move || {
                let claim = ledger.claim_exclusive_data(root, va, frame, true);
                ready.wait();
                release.wait();
                if claim == DataClaim::Claimed {
                    ledger.cancel_exclusive_data(root, va, frame, true);
                }
                claim
            }));
        }
        ready.wait();
        assert_eq!(charge::usage(36), 1);
        release.wait();
        let claims: std::vec::Vec<_> = workers.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(claims.iter().filter(|&&c| c == DataClaim::Claimed).count(), 1);
        assert_eq!(claims.iter().filter(|&&c| c == DataClaim::Quota).count(), 1);
        let loser = *claims.iter().find(|&&c| c == DataClaim::Quota).unwrap();
        assert_eq!(data_claim_result(loser), Err(AddressSpaceError::OutOfFrames));
        assert_eq!(data_claim_result(DataClaim::Invalid), Err(AddressSpaceError::InvalidFrameOwnership));
    });
    assert_eq!((charge::usage(36), charge::usage(37), charge::usage(38)), (0, 0, 0));
    assert_eq!(ledger.record(frame_a).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_FREE);
    assert_eq!(ledger.record(frame_b).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_FREE);
    charge::set_limit(36, 0);
    assert_eq!(ledger.claim_private_file_data(root_a, va, frame_a, 7), DataClaim::Quota);
    assert_eq!(ledger.claim_private_file_data(root_a, va, frame_a, 0), DataClaim::Invalid);
    assert_eq!(charge::usage(36), 0);
    ledger.unregister_root(root_a);
    ledger.unregister_root(root_b);
}
