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
    assert_eq!(ledger.drain_tables(root), alloc::vec![table]);
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
