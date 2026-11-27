extern crate std;

use super::*;
use alloc::vec::Vec;

fn ledger() -> (
    FrameDescriptorLedger,
    Vec<FrameDescriptorRecord>,
    Vec<CowMappingRecord>,
) {
    ledger_with_frames(16)
}

fn ledger_with_frames(frames: usize) -> (
    FrameDescriptorLedger,
    Vec<FrameDescriptorRecord>,
    Vec<CowMappingRecord>,
) {
    let mut records: Vec<FrameDescriptorRecord> =
        (0..frames).map(|_| FrameDescriptorRecord::empty()).collect();
    let mut mappings: Vec<CowMappingRecord> = (0..frames).map(|_| CowMappingRecord::empty()).collect();
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
fn huge_data_run_preserves_split_table_and_independent_leaf_lifetimes() {
    let (ledger, _records, _mappings) = ledger_with_frames(1024);
    let root = PAGE_SIZE;
    let table = PAGE_SIZE * 2;
    let child = PAGE_SIZE * 3;
    let first = PAGE_SIZE * 512;
    let va = 0x8000_0000;
    ledger.register_root(root);
    ledger.register_root(child);
    assert_eq!(ledger.claim_table(root, table), TableClaim::Claimed);
    ledger.publish_table(root, table);
    assert_eq!(ledger.claim_exclusive_data_run(root, va, first, 512), DataClaim::Claimed);
    assert!(!ledger.deposit_anon_huge_table(root, va + PAGE_SIZE, first, table));
    assert!(ledger.deposit_anon_huge_table(root, va, first, table));
    assert!(!ledger.deposit_anon_huge_table(root, va, first, table));
    ledger.publish_exclusive_data_run(root, va, first, 512);
    assert_eq!(census(&ledger, root), (512, 0, 512));
    assert_eq!(ledger.anon_huge_table(root, va, first), Some(table));
    assert_eq!(ledger.anon_huge_table(child, va, first), None);
    assert_eq!(ledger.release_data(root, va, first), None);
    assert!(!ledger.rekey_data(root, va, va + PAGE_SIZE, first));
    assert!(ledger.prepare_shared_alias(root, va, child, va, first, CowFrameKind::AnonymousFork, 0).is_none());
    assert_eq!(ledger.anon_huge_table(root, va, first), Some(table));
    assert_eq!(ledger.take_anon_huge_table(root, va, first), Some(table));
    assert_eq!(ledger.take_anon_huge_table(root, va, first), None);
    assert!(ledger.table_is_owned(root, table), "taking a deposit does not release table custody");
    for index in 0..512 {
        let offset = index * PAGE_SIZE;
        assert!(ledger.data_is_owned(root, va + offset, first + offset));
        assert_eq!(ledger.release_data(root, va + offset, first + offset), Some(DataLeafRelease::FrameReusable));
    }
    assert_eq!(census(&ledger, root), (0, 0, 0));
    assert_eq!(ledger.pop_table(root), Some(table));
    ledger.unregister_root(child);
    ledger.unregister_root(root);
}

#[test]
fn exclusive_run_rolls_back_quota_conflicts_and_cancelled_claims_exactly() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let va = 0x8000_0000;
    let first = PAGE_SIZE * 3;
    charge::init_group(39, None);
    ledger.register_root(root);
    assert!(ledger.bind_charge(root, 39));
    charge::set_limit(39, 3);
    assert_eq!(ledger.claim_exclusive_data_run(root, va, first, 4), DataClaim::Quota);
    assert_eq!(charge::usage(39), 0);
    charge::set_limit(39, 4);
    assert_eq!(ledger.claim_exclusive_data_run(root, va, first, 4), DataClaim::Claimed);
    assert_eq!(charge::usage(39), 4);
    ledger.cancel_exclusive_data_run(root, va, first, 4);
    assert_eq!(charge::usage(39), 0);
    assert_eq!(census(&ledger, root), (0, 0, 0));

    let conflicting = first + 2 * PAGE_SIZE;
    assert_eq!(ledger.claim_exclusive_data(root, va + 8 * PAGE_SIZE, conflicting, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(root, va + 8 * PAGE_SIZE, conflicting);
    charge::set_limit(39, 5);
    assert_eq!(ledger.claim_exclusive_data_run(root, va, first, 4), DataClaim::Invalid);
    assert_eq!(charge::usage(39), 1);
    for index in [0, 1, 3] {
        assert_eq!(ledger.record(first + index * PAGE_SIZE).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_FREE);
    }
    assert!(ledger.data_is_owned(root, va + 8 * PAGE_SIZE, conflicting));
    assert_eq!(ledger.release_data(root, va + 8 * PAGE_SIZE, conflicting), Some(DataLeafRelease::FrameReusable));
    assert_eq!(charge::usage(39), 0);
    ledger.unregister_root(root);
}

#[test]
fn exclusive_run_rejects_invalid_ranges_before_claiming_any_frame() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE * 4;
    let va = 0x8000_0000;
    ledger.register_root(root);
    for (va, first, pages) in [
        (va, PAGE_SIZE * 5, 0),
        (va, PAGE_SIZE * 5 + 1, 2),
        (va + 1, PAGE_SIZE * 5, 2),
        (va, PAGE_SIZE * 3, 3),
        (va, PAGE_SIZE * 15, 2),
        (u64::MAX & !(PAGE_SIZE - 1), PAGE_SIZE * 5, 2),
    ] {
        assert_eq!(ledger.claim_exclusive_data_run(root, va, first, pages), DataClaim::Invalid);
    }
    assert_eq!(census(&ledger, root), (0, 0, 0));
    ledger.unregister_root(root);
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

#[test]
fn rekey_moves_exclusive_ownership_without_touching_charge() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    let frame = PAGE_SIZE * 3;
    let (old_va, new_va) = (0x8000_4000, 0x9000_8000);
    ledger.register_root(root);
    assert_eq!(ledger.claim_exclusive_data(root, old_va, frame, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(root, old_va, frame);
    let leaves = ledger.verify_root(root).unwrap().data_leaf_count_or_map_head.load(Ordering::Relaxed);
    assert!(ledger.rekey_data(root, old_va, new_va, frame));
    assert!(!ledger.data_is_owned(root, old_va, frame), "the old key must be gone");
    assert!(ledger.data_is_owned(root, new_va, frame));
    assert_eq!(
        ledger.verify_root(root).unwrap().data_leaf_count_or_map_head.load(Ordering::Relaxed),
        leaves,
        "a relocation neither adds nor removes a data leaf"
    );
    // A stale or foreign key never re-keys.
    assert!(!ledger.rekey_data(root, old_va, new_va + PAGE_SIZE, frame));
    assert!(!ledger.rekey_data(root + PAGE_SIZE, new_va, old_va, frame));
    assert!(!ledger.rekey_data(root, new_va, old_va + 1, frame));
    assert!(ledger.data_is_owned(root, new_va, frame));
    assert_eq!(ledger.release_data(root, old_va, frame), None);
    assert_eq!(ledger.release_data(root, new_va, frame), Some(DataLeafRelease::FrameReusable));
    ledger.unregister_root(root);
}

#[test]
fn rekey_moves_one_member_of_a_shared_cow_set() {
    let (ledger, _records, _mappings) = ledger();
    let (parent, child) = (PAGE_SIZE, PAGE_SIZE * 2);
    let frame = PAGE_SIZE * 3;
    let (old_va, new_va, child_va) = (0x8000_4000, 0x9000_8000, 0x8000_4000);
    ledger.register_root(parent);
    ledger.register_root(child);
    assert_eq!(ledger.claim_exclusive_data(parent, old_va, frame, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(parent, old_va, frame);
    let ticket = ledger
        .prepare_shared_alias(parent, old_va, child, child_va, frame, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(ticket);
    // The primary (inline) member moves.
    assert!(ledger.rekey_data(parent, old_va, new_va, frame));
    assert!(ledger.data_is_owned(parent, new_va, frame));
    assert!(!ledger.data_is_owned(parent, old_va, frame));
    assert!(ledger.data_is_owned(child, child_va, frame), "the sibling keeps its key");
    // A list member moves too.
    assert!(ledger.rekey_data(child, child_va, child_va + 0x10_0000, frame));
    assert!(ledger.data_is_owned(child, child_va + 0x10_0000, frame));
    assert!(!ledger.data_is_owned(child, child_va, frame));
    assert!(ledger.data_is_owned(parent, new_va, frame));
    // Both members are still individually releasable under their new keys.
    assert_eq!(ledger.release_data(child, child_va + 0x10_0000, frame), Some(DataLeafRelease::FrameRetained));
    assert_eq!(ledger.release_data(parent, new_va, frame), Some(DataLeafRelease::FrameReusable));
    ledger.unregister_root(child);
    ledger.unregister_root(parent);
}

#[test]
fn shared_alias_run_batches_mixed_exclusive_inherited_and_duplicate_frames() {
    use crate::memory::charge;
    let (ledger, _records, _mappings) = ledger();
    let (parent, child, sibling) = (PAGE_SIZE, PAGE_SIZE * 2, PAGE_SIZE * 3);
    let (f_excl_hi, f_excl_lo, f_file, f_anon_shared) =
        (PAGE_SIZE * 7, PAGE_SIZE * 4, PAGE_SIZE * 5, PAGE_SIZE * 6);
    let va_base = 0x8000_0000_u64;
    charge::init_group(25, None);
    ledger.register_root(parent);
    ledger.register_root(child);
    ledger.register_root(sibling);
    assert!(ledger.bind_charge(child, 25));

    // Parent owns two exclusive frames (placed out of phys order in the run).
    assert_eq!(ledger.claim_exclusive_data(parent, va_base, f_excl_hi, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(parent, va_base, f_excl_hi);
    assert_eq!(
        ledger.claim_exclusive_data(parent, va_base + PAGE_SIZE, f_excl_lo, true),
        DataClaim::Claimed
    );
    ledger.publish_exclusive_data(parent, va_base + PAGE_SIZE, f_excl_lo);

    // Parent owns two aliases of the same private-file frame (duplicate phys in one run).
    assert!(ledger.claim_private_file_cache(f_file, 77));
    let t1 = ledger
        .prepare_shared_alias(0, 0, parent, va_base + PAGE_SIZE * 2, f_file, CowFrameKind::PrivateFileSection, 77)
        .unwrap();
    ledger.publish_shared_alias(t1);
    let t2 = ledger
        .prepare_shared_alias(0, 0, parent, va_base + PAGE_SIZE * 3, f_file, CowFrameKind::PrivateFileSection, 77)
        .unwrap();
    ledger.publish_shared_alias(t2);

    // Parent and sibling already share an anonymous COW frame.
    assert_eq!(
        ledger.claim_exclusive_data(parent, va_base + PAGE_SIZE * 4, f_anon_shared, true),
        DataClaim::Claimed
    );
    ledger.publish_exclusive_data(parent, va_base + PAGE_SIZE * 4, f_anon_shared);
    let t3 = ledger
        .prepare_shared_alias(
            parent,
            va_base + PAGE_SIZE * 4,
            sibling,
            va_base + PAGE_SIZE * 4,
            f_anon_shared,
            CowFrameKind::AnonymousFork,
            0,
        )
        .unwrap();
    ledger.publish_shared_alias(t3);

    let requests = [
        SharedAliasRequest {
            owner_va: va_base,
            new_va: va_base,
            frame_phys: f_excl_hi,
            mode: SharedAliasMode::AnonymousFork,
        },
        SharedAliasRequest {
            owner_va: va_base + PAGE_SIZE * 2,
            new_va: va_base + PAGE_SIZE * 2,
            frame_phys: f_file,
            mode: SharedAliasMode::InheritShared,
        },
        SharedAliasRequest {
            owner_va: va_base + PAGE_SIZE,
            new_va: va_base + PAGE_SIZE,
            frame_phys: f_excl_lo,
            mode: SharedAliasMode::AnonymousFork,
        },
        SharedAliasRequest {
            owner_va: va_base + PAGE_SIZE * 3,
            new_va: va_base + PAGE_SIZE * 3,
            frame_phys: f_file,
            mode: SharedAliasMode::InheritShared,
        },
        SharedAliasRequest {
            owner_va: va_base + PAGE_SIZE * 4,
            new_va: va_base + PAGE_SIZE * 4,
            frame_phys: f_anon_shared,
            mode: SharedAliasMode::InheritShared,
        },
    ];
    let mut tickets = [SharedAliasTicket::EMPTY; 5];
    assert_eq!(
        ledger.prepare_shared_alias_run(parent, child, &requests, &mut tickets),
        Ok(())
    );
    // Charge is reserved once on prepare; leaf count updates once on publish.
    assert_eq!(charge::usage(25), 5);
    assert_eq!(
        ledger.verify_root(child).unwrap().data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE),
        0
    );
    ledger.publish_shared_alias_run(&tickets);
    assert_eq!(charge::usage(25), 5);
    assert_eq!(
        ledger.verify_root(child).unwrap().data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE),
        5
    );

    assert_eq!(
        ledger.cow_identity(child, va_base, f_excl_hi),
        Some((CowFrameKind::AnonymousFork, 0))
    );
    assert_eq!(
        ledger.cow_identity(child, va_base + PAGE_SIZE, f_excl_lo),
        Some((CowFrameKind::AnonymousFork, 0))
    );
    assert_eq!(
        ledger.cow_identity(child, va_base + PAGE_SIZE * 2, f_file),
        Some((CowFrameKind::PrivateFileSection, 77))
    );
    assert_eq!(
        ledger.cow_identity(child, va_base + PAGE_SIZE * 3, f_file),
        Some((CowFrameKind::PrivateFileSection, 77))
    );
    assert_eq!(
        ledger.cow_identity(child, va_base + PAGE_SIZE * 4, f_anon_shared),
        Some((CowFrameKind::AnonymousFork, 0))
    );

    // Release every child leaf and verify exact charge drainage.
    for req in &requests {
        assert_eq!(
            ledger.release_data(child, req.new_va, req.frame_phys),
            Some(DataLeafRelease::FrameRetained)
        );
    }
    assert_eq!(charge::usage(25), 0);
    assert_eq!(
        ledger.verify_root(child).unwrap().data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE),
        0
    );

    // Release parent/sibling/cache custody to verify full reusability.
    assert_eq!(ledger.release_data(parent, va_base, f_excl_hi), Some(DataLeafRelease::FrameReusable));
    assert_eq!(
        ledger.release_data(parent, va_base + PAGE_SIZE, f_excl_lo),
        Some(DataLeafRelease::FrameReusable)
    );
    assert_eq!(
        ledger.release_data(parent, va_base + PAGE_SIZE * 2, f_file),
        Some(DataLeafRelease::FrameRetained)
    );
    assert_eq!(
        ledger.release_data(parent, va_base + PAGE_SIZE * 3, f_file),
        Some(DataLeafRelease::FrameRetained)
    );
    assert_eq!(
        ledger.release_private_file_cache(f_file, 77),
        Some(DataLeafRelease::FrameReusable)
    );
    assert_eq!(
        ledger.release_data(sibling, va_base + PAGE_SIZE * 4, f_anon_shared),
        Some(DataLeafRelease::FrameRetained)
    );
    assert_eq!(
        ledger.release_data(parent, va_base + PAGE_SIZE * 4, f_anon_shared),
        Some(DataLeafRelease::FrameReusable)
    );
    ledger.unregister_root(sibling);
    ledger.unregister_root(child);
    ledger.unregister_root(parent);
}

#[test]
fn shared_alias_run_mid_batch_failure_rolls_back_all_prior_frames_and_charge() {
    use crate::memory::charge;
    let (ledger, _records, _mappings) = ledger();
    let (parent, child) = (PAGE_SIZE, PAGE_SIZE * 2);
    let (f0, f1, f2_unowned) = (PAGE_SIZE * 3, PAGE_SIZE * 4, PAGE_SIZE * 5);
    let va = 0x8000_0000_u64;
    charge::init_group(26, None);
    ledger.register_root(parent);
    ledger.register_root(child);
    assert!(ledger.bind_charge(child, 26));

    assert_eq!(ledger.claim_exclusive_data(parent, va, f0, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(parent, va, f0);
    assert_eq!(ledger.claim_exclusive_data(parent, va + PAGE_SIZE, f1, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(parent, va + PAGE_SIZE, f1);

    // 1. Mid-batch invalid ownership (f2_unowned is ROLE_FREE) must unwind f0 and f1.
    let bad_requests = [
        SharedAliasRequest {
            owner_va: va,
            new_va: va,
            frame_phys: f0,
            mode: SharedAliasMode::AnonymousFork,
        },
        SharedAliasRequest {
            owner_va: va + PAGE_SIZE,
            new_va: va + PAGE_SIZE,
            frame_phys: f1,
            mode: SharedAliasMode::AnonymousFork,
        },
        SharedAliasRequest {
            owner_va: va + PAGE_SIZE * 2,
            new_va: va + PAGE_SIZE * 2,
            frame_phys: f2_unowned,
            mode: SharedAliasMode::AnonymousFork,
        },
    ];
    let mut tickets = [SharedAliasTicket::EMPTY; 3];
    assert_eq!(
        ledger.prepare_shared_alias_run(parent, child, &bad_requests, &mut tickets),
        Err(SharedAliasRunError::InvalidFrameOwnership)
    );
    assert_eq!(charge::usage(26), 0);
    assert_eq!(ledger.record(f0).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_EXCLUSIVE_DATA_LIVE);
    assert_eq!(ledger.record(f1).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_EXCLUSIVE_DATA_LIVE);
    assert_eq!(
        ledger.record(f0).unwrap().data_leaf_count_or_map_head.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        ledger.record(f1).unwrap().data_leaf_count_or_map_head.load(Ordering::Relaxed),
        0
    );
    assert!(!ledger.data_is_owned(child, va, f0));
    assert!(!ledger.data_is_owned(child, va + PAGE_SIZE, f1));
    assert!(ledger.data_is_owned(parent, va, f0));
    assert!(ledger.data_is_owned(parent, va + PAGE_SIZE, f1));

    // 2. Quota refusal leaves all prior frames untouched and usage at zero.
    charge::set_limit(26, 1);
    assert_eq!(
        ledger.prepare_shared_alias_run(parent, child, &bad_requests[..2], &mut tickets[..2]),
        Err(SharedAliasRunError::OutOfFrames)
    );
    assert_eq!(charge::usage(26), 0);
    assert_eq!(ledger.record(f0).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_EXCLUSIVE_DATA_LIVE);
    assert_eq!(ledger.record(f1).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_EXCLUSIVE_DATA_LIVE);

    // 3. Pool exhaustion mid-run (only 2 mapping slots for a 3-frame run) rolls back cleanly.
    let mut small_records: Vec<FrameDescriptorRecord> =
        (0..8).map(|_| FrameDescriptorRecord::empty()).collect();
    let mut small_mappings: Vec<CowMappingRecord> =
        (0..2).map(|_| CowMappingRecord::empty()).collect();
    let small = FrameDescriptorLedger::empty();
    small.install(
        small_records.as_mut_ptr(),
        small_records.len(),
        small_mappings.as_mut_ptr(),
        small_mappings.len(),
    );
    charge::init_group(28, None);
    small.register_root(parent);
    small.register_root(child);
    assert!(small.bind_charge(child, 28));
    for (idx, frame) in [f0, f1, f2_unowned].iter().enumerate() {
        let leaf_va = va + (idx as u64) * PAGE_SIZE;
        assert_eq!(small.claim_exclusive_data(parent, leaf_va, *frame, true), DataClaim::Claimed);
        small.publish_exclusive_data(parent, leaf_va, *frame);
    }
    assert_eq!(
        small.prepare_shared_alias_run(parent, child, &bad_requests, &mut tickets),
        Err(SharedAliasRunError::OutOfFrames)
    );
    assert_eq!(charge::usage(28), 0);
    for frame in [f0, f1, f2_unowned] {
        assert_eq!(small.record(frame).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_EXCLUSIVE_DATA_LIVE);
    }
    // Both mapping records must have been returned to the free pool so a 2-page run succeeds.
    assert_eq!(
        small.prepare_shared_alias_run(parent, child, &bad_requests[..2], &mut tickets[..2]),
        Ok(())
    );
    small.cancel_shared_alias_run(&tickets[..2]);
    assert_eq!(charge::usage(28), 0);
    for (idx, frame) in [f0, f1, f2_unowned].iter().enumerate() {
        let leaf_va = va + (idx as u64) * PAGE_SIZE;
        assert_eq!(small.release_data(parent, leaf_va, *frame), Some(DataLeafRelease::FrameReusable));
    }
    small.unregister_root(child);
    small.unregister_root(parent);

    assert_eq!(ledger.release_data(parent, va, f0), Some(DataLeafRelease::FrameReusable));
    assert_eq!(ledger.release_data(parent, va + PAGE_SIZE, f1), Some(DataLeafRelease::FrameReusable));
    ledger.unregister_root(child);
    ledger.unregister_root(parent);
}

#[test]
fn shared_alias_run_charges_root_once_and_cancel_refunds_atomically() {
    use crate::memory::charge;
    let (ledger, _records, _mappings) = ledger();
    let (parent, child) = (PAGE_SIZE, PAGE_SIZE * 2);
    let (f0, f1, f_file) = (PAGE_SIZE * 3, PAGE_SIZE * 4, PAGE_SIZE * 5);
    let va = 0x8000_0000_u64;
    charge::init_group(27, None);
    charge::set_qos(27, 2, 0);
    charge::set_limit(27, 8);
    ledger.register_root(parent);
    ledger.register_root(child);
    assert!(ledger.bind_charge(child, 27));

    assert_eq!(ledger.claim_exclusive_data(parent, va, f0, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(parent, va, f0);
    assert_eq!(ledger.claim_exclusive_data(parent, va + PAGE_SIZE, f1, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(parent, va + PAGE_SIZE, f1);
    assert!(ledger.claim_private_file_cache(f_file, 91));
    let t1 = ledger
        .prepare_shared_alias(0, 0, parent, va + PAGE_SIZE * 2, f_file, CowFrameKind::PrivateFileSection, 91)
        .unwrap();
    ledger.publish_shared_alias(t1);
    let t2 = ledger
        .prepare_shared_alias(0, 0, parent, va + PAGE_SIZE * 3, f_file, CowFrameKind::PrivateFileSection, 91)
        .unwrap();
    ledger.publish_shared_alias(t2);

    // 4-page run with duplicate `f_file` crosses high=2 in one single charge call.
    let requests = [
        SharedAliasRequest {
            owner_va: va + PAGE_SIZE * 2,
            new_va: va + PAGE_SIZE * 2,
            frame_phys: f_file,
            mode: SharedAliasMode::InheritShared,
        },
        SharedAliasRequest {
            owner_va: va,
            new_va: va,
            frame_phys: f0,
            mode: SharedAliasMode::AnonymousFork,
        },
        SharedAliasRequest {
            owner_va: va + PAGE_SIZE * 3,
            new_va: va + PAGE_SIZE * 3,
            frame_phys: f_file,
            mode: SharedAliasMode::InheritShared,
        },
        SharedAliasRequest {
            owner_va: va + PAGE_SIZE,
            new_va: va + PAGE_SIZE,
            frame_phys: f1,
            mode: SharedAliasMode::AnonymousFork,
        },
    ];
    let mut tickets = [SharedAliasTicket::EMPTY; 4];
    assert_eq!(
        ledger.prepare_shared_alias_run(parent, child, &requests, &mut tickets),
        Ok(())
    );
    assert_eq!(charge::usage(27), 4);
    assert_eq!(charge::high_events(27), 1);

    // Cancel the entire run in caller order (including both duplicate tickets).
    ledger.cancel_shared_alias_run(&tickets);
    assert_eq!(charge::usage(27), 0);
    assert_eq!(
        ledger.verify_root(child).unwrap().data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE),
        0
    );
    assert_eq!(ledger.record(f0).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_EXCLUSIVE_DATA_LIVE);
    assert_eq!(ledger.record(f1).unwrap().role.load(DESCRIPTOR_ACQUIRE), ROLE_EXCLUSIVE_DATA_LIVE);
    assert_eq!(
        ledger.record(f_file).unwrap().role.load(DESCRIPTOR_ACQUIRE),
        ROLE_SHARED_PRIVATE_FILE_LIVE
    );
    assert_eq!(
        shared_count(ledger.record(f_file).unwrap().data_leaf_count_or_map_head.load(Ordering::Relaxed)),
        3
    );

    assert_eq!(ledger.release_data(parent, va, f0), Some(DataLeafRelease::FrameReusable));
    assert_eq!(ledger.release_data(parent, va + PAGE_SIZE, f1), Some(DataLeafRelease::FrameReusable));
    assert_eq!(
        ledger.release_data(parent, va + PAGE_SIZE * 2, f_file),
        Some(DataLeafRelease::FrameRetained)
    );
    assert_eq!(
        ledger.release_data(parent, va + PAGE_SIZE * 3, f_file),
        Some(DataLeafRelease::FrameRetained)
    );
    assert_eq!(
        ledger.release_private_file_cache(f_file, 91),
        Some(DataLeafRelease::FrameReusable)
    );
    ledger.unregister_root(child);
    ledger.unregister_root(parent);
}

fn census(ledger: &FrameDescriptorLedger, root: u64) -> (u64, u64, u64) {
    let c = ledger.leaf_census(root).unwrap();
    (c.mapped, c.shared, c.private())
}

fn publish_exclusive(ledger: &FrameDescriptorLedger, root: u64, va: u64, frame: u64) {
    assert_eq!(ledger.claim_exclusive_data(root, va, frame, true), DataClaim::Claimed);
    ledger.publish_exclusive_data(root, va, frame);
}

#[test]
fn census_counts_exclusive_leaves_as_private_once() {
    let (ledger, _records, _mappings) = ledger();
    let root = PAGE_SIZE;
    crate::memory::charge::init_group(1, None);
    ledger.register_root(root);
    assert!(ledger.bind_charge(root, 1));
    for index in 0..3_u64 {
        publish_exclusive(&ledger, root, 0x8000_0000 + index * PAGE_SIZE, PAGE_SIZE * (2 + index));
    }
    assert_eq!(census(&ledger, root), (3, 0, 3));
    for index in 0..3_u64 {
        assert_eq!(
            ledger.release_data(root, 0x8000_0000 + index * PAGE_SIZE, PAGE_SIZE * (2 + index)),
            Some(DataLeafRelease::FrameReusable)
        );
    }
    assert_eq!(census(&ledger, root), (0, 0, 0));
    ledger.unregister_root(root);
}

#[test]
fn census_charges_a_forked_page_to_neither_side_until_a_sibling_leaves() {
    let (ledger, _records, _mappings) = ledger();
    let (parent, child, other) = (PAGE_SIZE, PAGE_SIZE * 2, PAGE_SIZE * 3);
    let (shared, own) = (PAGE_SIZE * 4, PAGE_SIZE * 5);
    let va = 0x8000_4000;
    for (group, root) in [(2, parent), (3, child), (4, other)] {
        crate::memory::charge::init_group(group, None);
        ledger.register_root(root);
        assert!(ledger.bind_charge(root, group));
    }
    publish_exclusive(&ledger, parent, va, shared);
    publish_exclusive(&ledger, parent, va + PAGE_SIZE, own);
    assert_eq!(census(&ledger, parent), (2, 0, 2));
    // Fork: the shared page converts the parent's leaf and aliases the child's.
    let t = ledger
        .prepare_shared_alias(parent, va, child, va, shared, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(t);
    assert_eq!(census(&ledger, parent), (2, 1, 1), "only the unforked page is private");
    assert_eq!(census(&ledger, child), (1, 1, 0), "an alias frees nothing");
    // A second fork adds a mapper without touching the parent's shared count.
    let t = ledger
        .prepare_shared_alias(parent, va, other, va, shared, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(t);
    assert_eq!(census(&ledger, parent), (2, 1, 1));
    assert_eq!(census(&ledger, other), (1, 1, 0));
    // The child's exit leaves three-to-two: nobody becomes private yet.
    assert_eq!(ledger.release_data(child, va, shared), Some(DataLeafRelease::FrameRetained));
    assert_eq!(census(&ledger, child), (0, 0, 0));
    assert_eq!(census(&ledger, parent), (2, 1, 1));
    // The last sibling's exit makes the survivor the frame's sole owner.
    assert_eq!(ledger.release_data(other, va, shared), Some(DataLeafRelease::FrameRetained));
    assert_eq!(census(&ledger, parent), (2, 0, 2));
    // Everything the census calls private is exactly what teardown frees.
    let freed = [(va, shared), (va + PAGE_SIZE, own)]
        .into_iter()
        .filter(|&(v, f)| ledger.release_data(parent, v, f) == Some(DataLeafRelease::FrameReusable))
        .count() as u64;
    assert_eq!(freed, 2);
    for root in [parent, child, other] {
        ledger.unregister_root(root);
    }
}

#[test]
fn census_kill_frees_exactly_the_private_figure() {
    let (ledger, _records, _mappings) = ledger();
    let (parent, child) = (PAGE_SIZE, PAGE_SIZE * 2);
    let va = 0x8000_0000;
    for (group, root) in [(5, parent), (6, child)] {
        crate::memory::charge::init_group(group, None);
        ledger.register_root(root);
        assert!(ledger.bind_charge(root, group));
    }
    // Parent maps frames 3..=6; frames 3 and 4 are forked to the child, which
    // then COW-breaks frame 4 into its own frame 7 and adds private frame 8.
    for index in 0..4_u64 {
        publish_exclusive(&ledger, parent, va + index * PAGE_SIZE, PAGE_SIZE * (3 + index));
    }
    for index in 0..2_u64 {
        let t = ledger
            .prepare_shared_alias(
                parent,
                va + index * PAGE_SIZE,
                child,
                va + index * PAGE_SIZE,
                PAGE_SIZE * (3 + index),
                CowFrameKind::AnonymousFork,
                0,
            )
            .unwrap();
        ledger.publish_shared_alias(t);
    }
    let claim = ledger.try_claim_cow_mapping(child, va + PAGE_SIZE, PAGE_SIZE * 4).unwrap();
    assert_eq!(ledger.claim_exclusive_data(child, va + PAGE_SIZE, PAGE_SIZE * 7, false), DataClaim::Claimed);
    assert_eq!(ledger.commit_cow_mapping(claim), DataLeafRelease::FrameRetained);
    ledger.publish_exclusive_data(child, va + PAGE_SIZE, PAGE_SIZE * 7);
    publish_exclusive(&ledger, child, va + 2 * PAGE_SIZE, PAGE_SIZE * 8);

    let child_leaves = [(va, 3), (va + PAGE_SIZE, 7), (va + 2 * PAGE_SIZE, 8)];
    let parent_leaves = [(va, 3), (va + PAGE_SIZE, 4), (va + 2 * PAGE_SIZE, 5), (va + 3 * PAGE_SIZE, 6)];
    // Frame 3: parent+child; frame 4: parent only after the child's break.
    assert_eq!(census(&ledger, child), (3, 1, 2));
    assert_eq!(census(&ledger, parent), (4, 1, 3));
    let predicted_child = census(&ledger, child).2;
    let freed = child_leaves
        .iter()
        .filter(|&&(v, f)| ledger.release_data(child, v, PAGE_SIZE * f) == Some(DataLeafRelease::FrameReusable))
        .count() as u64;
    assert_eq!(freed, predicted_child, "the child's kill frees exactly its census figure");
    // With the child gone the parent is sole owner of frame 3 as well.
    assert_eq!(census(&ledger, parent), (4, 0, 4));
    let freed = parent_leaves
        .iter()
        .filter(|&&(v, f)| ledger.release_data(parent, v, PAGE_SIZE * f) == Some(DataLeafRelease::FrameReusable))
        .count() as u64;
    assert_eq!(freed, 4);
    ledger.unregister_root(child);
    ledger.unregister_root(parent);
}

#[test]
fn census_never_charges_file_backed_private_leaves() {
    let (ledger, _records, _mappings) = ledger();
    let (first, second) = (PAGE_SIZE, PAGE_SIZE * 2);
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_0000;
    for (group, root) in [(7, first), (8, second)] {
        crate::memory::charge::init_group(group, None);
        ledger.register_root(root);
        assert!(ledger.bind_charge(root, group));
    }
    assert!(ledger.claim_private_file_cache(frame, 99));
    for root in [first, second] {
        let t = ledger
            .prepare_shared_alias(0, 0, root, va, frame, CowFrameKind::PrivateFileSection, 99)
            .unwrap();
        ledger.publish_shared_alias(t);
        assert_eq!(census(&ledger, root), (1, 1, 0));
    }
    // One mapper left plus cache custody still frees nothing on exit.
    assert_eq!(ledger.release_data(first, va, frame), Some(DataLeafRelease::FrameRetained));
    assert_eq!(census(&ledger, first), (0, 0, 0));
    assert_eq!(census(&ledger, second), (1, 1, 0));
    assert_eq!(ledger.release_data(second, va, frame), Some(DataLeafRelease::FrameRetained));
    assert_eq!(
        ledger.release_private_file_cache(frame, 99),
        Some(DataLeafRelease::FrameReusable)
    );
    ledger.unregister_root(first);
    ledger.unregister_root(second);
}

#[test]
fn census_rolls_a_failed_fork_back_to_private() {
    let (ledger, _records, _mappings) = ledger();
    let (parent, child) = (PAGE_SIZE, PAGE_SIZE * 2);
    let frame = PAGE_SIZE * 3;
    let va = 0x8000_0000;
    for (group, root) in [(9, parent), (29, child)] {
        crate::memory::charge::init_group(group, None);
        ledger.register_root(root);
        assert!(ledger.bind_charge(root, group));
    }
    publish_exclusive(&ledger, parent, va, frame);
    let t = ledger
        .prepare_shared_alias(parent, va, child, va, frame, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.cancel_shared_alias(t);
    assert_eq!(census(&ledger, parent), (1, 0, 1), "a cancelled alias never counted");
    let t = ledger
        .prepare_shared_alias(parent, va, child, va, frame, CowFrameKind::AnonymousFork, 0)
        .unwrap();
    ledger.publish_shared_alias(t);
    assert_eq!(census(&ledger, parent), (1, 1, 0));
    assert!(ledger.rollback_shared_alias(t));
    assert_eq!(census(&ledger, parent), (1, 0, 1));
    assert_eq!(census(&ledger, child), (0, 0, 0));
    ledger.release_data(parent, va, frame).unwrap();
    ledger.unregister_root(parent);
    ledger.unregister_root(child);
}

#[test]
fn lazy_table_count_matches_list_walk_across_publish_cancel_pop_and_concurrent_push() {
    use std::sync::Barrier;
    let (ledger, _records, _mappings) = ledger_with_frames(128);
    let root = PAGE_SIZE;
    ledger.register_root(root);
    assert_eq!(ledger.table_count(root), 0);
    assert_eq!(ledger.walk_table_count(root), 0);

    // Cancelled table claims never increment table_count.
    let cancelled = PAGE_SIZE * 2;
    assert_eq!(ledger.claim_table(root, cancelled), TableClaim::Claimed);
    assert_eq!(ledger.table_count(root), 0);
    ledger.cancel_table(root, cancelled);
    assert_eq!(ledger.table_count(root), 0);
    assert_eq!(ledger.walk_table_count(root), 0);

    // Concurrent multi-thread publish_table keeps head and count atomic.
    let threads = 4_usize;
    let per_thread = 16_usize;
    let barrier = Barrier::new(threads);
    std::thread::scope(|scope| {
        for t in 0..threads {
            let (ledger, barrier) = (&ledger, &barrier);
            scope.spawn(move || {
                barrier.wait();
                for i in 0..per_thread {
                    let frame = PAGE_SIZE * (10 + (t * per_thread + i) as u64);
                    assert_eq!(ledger.claim_table(root, frame), TableClaim::Claimed);
                    ledger.publish_table(root, frame);
                }
            });
        }
    });
    let expected_total = threads * per_thread;
    assert_eq!(ledger.table_count(root), expected_total);
    assert_eq!(ledger.walk_table_count(root), expected_total);

    // Binding charge sees the exact O(1) count.
    crate::memory::kmem_charge::init_group(22, None);
    assert!(ledger.bind_charge(root, 22));
    assert_eq!(
        crate::memory::kmem_charge::usage(crate::memory::kmem_charge::Class::PageTable, 22),
        (1 + expected_total as u64) * PAGE_SIZE
    );

    // Popping decrements both the tracked count and walked length identically.
    for remaining in (0..expected_total).rev() {
        assert!(ledger.pop_table(root).is_some());
        assert_eq!(ledger.table_count(root), remaining);
        assert_eq!(ledger.walk_table_count(root), remaining);
    }
    assert_eq!(ledger.pop_table(root), None);
    assert_eq!(ledger.table_count(root), 0);
    ledger.unregister_root(root);
}

#[test]
fn high_fanout_shared_mapping_avl_supports_add_find_rekey_cancel_and_remove_in_log_depth() {
    // 256 sequential roots sharing one page at the same VA — the adversarial
    // sequential-key pattern from Chrome/zygote forks.
    const FANOUT: usize = 256;
    let (ledger, _records, _mappings) = ledger_with_frames(FANOUT + 16);
    let shared_frame = PAGE_SIZE * (FANOUT as u64 + 4);
    let shared_va = 0x7000_0000_u64;

    let roots: Vec<u64> = (1..=FANOUT as u64).map(|i| i * PAGE_SIZE).collect();
    for &root in &roots {
        ledger.register_root(root);
    }

    // Root 0 owns the frame exclusively, then forks to roots 1..FANOUT.
    publish_exclusive(&ledger, roots[0], shared_va, shared_frame);
    for &child in &roots[1..] {
        let ticket = ledger
            .prepare_shared_alias(
                roots[0],
                shared_va,
                child,
                shared_va,
                shared_frame,
                CowFrameKind::AnonymousFork,
                0,
            )
            .expect("shared alias should succeed");
        ledger.publish_shared_alias(ticket);
    }

    // Strictly balanced AVL tree: for 255 tree nodes, height must be <= 11
    // (1.44 * log2(257) ≈ 11.5), versus 255 for a singly-linked list.
    let height = ledger.assert_shared_tree_valid(shared_frame);
    assert!(height <= 11, "AVL tree unbalanced: height={height}");

    // Every root is owned and has exact COW identity.
    for &root in &roots {
        assert!(ledger.data_is_owned(root, shared_va, shared_frame));
        assert_eq!(
            ledger.cow_identity(root, shared_va, shared_frame),
            Some((CowFrameKind::AnonymousFork, 0))
        );
    }
    assert!(!ledger.data_is_owned(roots[0], shared_va + PAGE_SIZE, shared_frame));

    // Rekey a subset of tree members and the inline member; verify tree stays balanced.
    let rekeyed_va = 0x9000_0000_u64;
    for &idx in &[0, 1, 63, 127, 200, FANOUT - 1] {
        assert!(ledger.rekey_data(roots[idx], shared_va, rekeyed_va, shared_frame));
        assert!(!ledger.data_is_owned(roots[idx], shared_va, shared_frame));
        assert!(ledger.data_is_owned(roots[idx], rekeyed_va, shared_frame));
        ledger.assert_shared_tree_valid(shared_frame);
    }

    // Prepare and cancel a multi-duplicate run on the high-fanout frame.
    let extra_root = PAGE_SIZE * (FANOUT as u64 + 2);
    ledger.register_root(extra_root);
    let cancel_reqs = [
        SharedAliasRequest {
            owner_va: rekeyed_va,
            new_va: 0x5000_0000,
            frame_phys: shared_frame,
            mode: SharedAliasMode::InheritShared,
        },
        SharedAliasRequest {
            owner_va: rekeyed_va,
            new_va: 0x5000_1000,
            frame_phys: shared_frame,
            mode: SharedAliasMode::InheritShared,
        },
        SharedAliasRequest {
            owner_va: rekeyed_va,
            new_va: 0x5000_2000,
            frame_phys: shared_frame,
            mode: SharedAliasMode::InheritShared,
        },
    ];
    let mut cancel_tickets = [SharedAliasTicket::EMPTY; 3];
    assert_eq!(
        ledger.prepare_shared_alias_run(roots[0], extra_root, &cancel_reqs, &mut cancel_tickets),
        Ok(())
    );
    ledger.assert_shared_tree_valid(shared_frame);
    ledger.cancel_shared_alias_run(&cancel_tickets);
    ledger.assert_shared_tree_valid(shared_frame);
    ledger.unregister_root(extra_root);

    // Release the inline owner first (triggers AVL root promotion), then release
    // remaining roots in pseudo-random order, validating AVL invariants along the way.
    let va_for = |idx: usize| {
        if matches!(idx, 0 | 1 | 63 | 127 | 200) || idx == FANOUT - 1 {
            rekeyed_va
        } else {
            shared_va
        }
    };
    assert_eq!(
        ledger.release_data(roots[0], va_for(0), shared_frame),
        Some(DataLeafRelease::FrameRetained)
    );
    ledger.assert_shared_tree_valid(shared_frame);

    let mut remaining: Vec<usize> = (1..FANOUT).collect();
    // Stride-based permutation to exercise all deletion cases (0, 1, 2 children + root).
    let mut order = Vec::with_capacity(remaining.len());
    let mut cursor = 0_usize;
    while !remaining.is_empty() {
        cursor = (cursor + 37) % remaining.len();
        order.push(remaining.remove(cursor));
    }
    for (pos, idx) in order.iter().copied().enumerate() {
        let is_last = pos + 1 == order.len();
        let expected = if is_last {
            DataLeafRelease::FrameReusable
        } else {
            DataLeafRelease::FrameRetained
        };
        assert_eq!(
            ledger.release_data(roots[idx], va_for(idx), shared_frame),
            Some(expected)
        );
        if !is_last && pos % 16 == 0 {
            ledger.assert_shared_tree_valid(shared_frame);
        }
    }
    for &root in &roots {
        ledger.unregister_root(root);
    }
}
