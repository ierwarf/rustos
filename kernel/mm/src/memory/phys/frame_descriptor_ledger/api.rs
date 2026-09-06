//! Narrow physical-ledger API used by page-table and fault machinery.
//!
//! - **Owner:** The parent frame descriptor ledger owns every transition.
//! - **Boundary:** Callers provide exact physical root/frame and page VA keys.
//! - **Lifecycle:** Claims must publish or cancel; releases settle one key.
//! - **Concurrency:** Shared operations serialize on the frame role word.
//! - **Failure:** Stale, duplicate, invalid, or exhausted claims fail closed.
//! - **Forbidden:** Callers cannot mutate descriptor payload directly.
//! - **Evidence:** Parent ledger tests and registered COW source witnesses.

use super::*;

pub fn register_lazy_table_root(root_phys: u64) {
    FRAME_DESCRIPTORS.register_root(root_phys);
}

pub fn register_data_leaf_root(root_phys: u64) {
    assert!(FRAME_DESCRIPTORS.verify_root(root_phys).is_some());
}

pub fn claim_lazy_table_record(root_phys: u64, table_phys: u64) -> bool {
    FRAME_DESCRIPTORS.claim_table(root_phys, table_phys)
}

pub fn publish_lazy_table_record(root_phys: u64, table_phys: u64) {
    FRAME_DESCRIPTORS.publish_table(root_phys, table_phys);
}

pub fn cancel_lazy_table_record(root_phys: u64, table_phys: u64) {
    FRAME_DESCRIPTORS.cancel_table(root_phys, table_phys);
}

pub fn drain_lazy_table_records(root_phys: u64) -> alloc::vec::Vec<u64> {
    FRAME_DESCRIPTORS.drain_tables(root_phys)
}

pub fn claim_data_leaf(root_phys: u64, virtual_address: u64, frame_phys: u64) -> bool {
    FRAME_DESCRIPTORS.claim_exclusive_data(root_phys, virtual_address, frame_phys)
}

pub fn publish_data_leaf(root_phys: u64, virtual_address: u64, frame_phys: u64) {
    FRAME_DESCRIPTORS.publish_exclusive_data(root_phys, virtual_address, frame_phys);
}

pub fn cancel_data_leaf(root_phys: u64, virtual_address: u64, frame_phys: u64) {
    FRAME_DESCRIPTORS.cancel_exclusive_data(root_phys, virtual_address, frame_phys);
}

pub fn prepare_shared_alias(
    owner_root: u64,
    owner_va: u64,
    new_root: u64,
    new_va: u64,
    frame_phys: u64,
    kind: CowFrameKind,
    backing_identity: u64,
) -> Option<SharedAliasTicket> {
    FRAME_DESCRIPTORS.prepare_shared_alias(
        owner_root,
        owner_va,
        new_root,
        new_va,
        frame_phys,
        kind,
        backing_identity,
    )
}

pub fn publish_shared_alias(ticket: SharedAliasTicket) {
    FRAME_DESCRIPTORS.publish_shared_alias(ticket);
}

pub fn cancel_shared_alias(ticket: SharedAliasTicket) {
    FRAME_DESCRIPTORS.cancel_shared_alias(ticket);
}

pub fn rollback_shared_alias(ticket: SharedAliasTicket) -> bool {
    FRAME_DESCRIPTORS.rollback_shared_alias(ticket)
}

pub fn claim_private_file_data_leaf(
    root_phys: u64,
    virtual_address: u64,
    frame_phys: u64,
    backing_identity: u64,
) -> bool {
    FRAME_DESCRIPTORS.claim_private_file_data(
        root_phys,
        virtual_address,
        frame_phys,
        backing_identity,
    )
}

pub fn publish_private_file_data_leaf(root_phys: u64, virtual_address: u64, frame_phys: u64) {
    FRAME_DESCRIPTORS.publish_private_file_data(root_phys, virtual_address, frame_phys);
}

pub fn cancel_private_file_data_leaf(root_phys: u64, virtual_address: u64, frame_phys: u64) {
    FRAME_DESCRIPTORS.cancel_private_file_data(root_phys, virtual_address, frame_phys);
}

pub fn data_leaf_is_owned(root_phys: u64, virtual_address: u64, frame_phys: u64) -> bool {
    FRAME_DESCRIPTORS.data_is_owned(root_phys, virtual_address, frame_phys)
}

pub fn data_leaf_cow_identity(
    root_phys: u64,
    virtual_address: u64,
    frame_phys: u64,
) -> Option<(CowFrameKind, u64)> {
    FRAME_DESCRIPTORS.cow_identity(root_phys, virtual_address, frame_phys)
}

pub fn try_claim_cow_mapping(
    root_phys: u64,
    virtual_address: u64,
    frame_phys: u64,
) -> Option<CowMappingClaim> {
    FRAME_DESCRIPTORS.try_claim_cow_mapping(root_phys, virtual_address, frame_phys)
}

pub fn cancel_cow_mapping(claim: CowMappingClaim) {
    FRAME_DESCRIPTORS.cancel_cow_mapping(claim);
}

pub fn commit_cow_mapping(claim: CowMappingClaim) -> DataLeafRelease {
    FRAME_DESCRIPTORS.commit_cow_mapping(claim)
}

pub fn promote_cow_mapping_in_place(claim: CowMappingClaim) {
    FRAME_DESCRIPTORS.promote_cow_mapping_in_place(claim);
}

pub fn release_data_leaf(
    root_phys: u64,
    virtual_address: u64,
    frame_phys: u64,
) -> Option<DataLeafRelease> {
    FRAME_DESCRIPTORS.release_data(root_phys, virtual_address, frame_phys)
}

pub fn data_leaf_count(root_phys: u64) -> Option<u64> {
    let root = FRAME_DESCRIPTORS.verify_root(root_phys)?;
    Some(root.data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE))
}

pub fn unregister_data_leaf_root(root_phys: u64) {
    FRAME_DESCRIPTORS.unregister_root(root_phys);
}
