//! Fixed descriptor-role and packed-head encoding.
//!
//! - **Owner:** The parent frame descriptor ledger owns these representations.
//! - **Boundary:** Only aligned, nonzero, distinct root/frame identities encode.
//! - **Lifecycle:** Packed counts and heads are decoded only under role custody.
//! - **Concurrency:** Callers hold or acquire the descriptor role before use.
//! - **Failure:** Invalid mapping identities are rejected before state mutation.
//! - **Forbidden:** These helpers allocate nothing and publish no atomic state.
//! - **Evidence:** Parent ledger unit tests cover exact aliases and ABA-safe reuse.

use super::*;

pub(super) fn valid_mapping(root_phys: u64, va: u64, frame_phys: u64) -> bool {
    root_phys != 0
        && frame_phys != 0
        && root_phys != frame_phys
        && root_phys.is_multiple_of(PAGE_SIZE)
        && va.is_multiple_of(PAGE_SIZE)
}

pub(super) const fn shared_live_role(kind: CowFrameKind) -> u64 {
    match kind {
        CowFrameKind::AnonymousFork => ROLE_SHARED_ANONYMOUS_LIVE,
        CowFrameKind::PrivateFileSection => ROLE_SHARED_PRIVATE_FILE_LIVE,
    }
}

pub(super) const fn shared_locked_role(kind: CowFrameKind) -> u64 {
    match kind {
        CowFrameKind::AnonymousFork => ROLE_SHARED_ANONYMOUS_LOCKED,
        CowFrameKind::PrivateFileSection => ROLE_SHARED_PRIVATE_FILE_LOCKED,
    }
}

pub(super) const fn pack_shared(count: u32, head: u32) -> u64 {
    (count as u64) << 32 | head as u64
}

pub(super) const fn shared_count(packed: u64) -> u32 {
    (packed >> 32) as u32
}

pub(super) const fn shared_head(packed: u64) -> u32 {
    packed as u32
}

pub(super) const fn pack_free_head(generation: u32, id: u32) -> u64 {
    (generation as u64) << 32 | id as u64
}

pub(super) const fn free_head_generation(packed: u64) -> u32 {
    (packed >> 32) as u32
}

pub(super) const fn free_head_id(packed: u64) -> u32 {
    packed as u32
}

/// Packed root lazy-table list header stored in `ROLE_ROOT.owner_or_table_head`:
/// high 32 bits hold the exact published table count, low 32 bits hold the head
/// table's frame index (`table_phys / PAGE_SIZE`, 0 when empty).
pub(super) const fn pack_table_head(count: u32, head_pfn: u32) -> u64 {
    ((count as u64) << 32) | (head_pfn as u64)
}

pub(super) const fn table_head_count(packed: u64) -> u32 {
    (packed >> 32) as u32
}

pub(super) const fn table_head_pfn(packed: u64) -> u32 {
    packed as u32
}

/// Packed AVL child links stored in `CowMappingRecord.next`:
/// high 32 bits hold the left child id, low 32 bits hold the right child id
/// (and also the next-free id while the record sits on the lock-free pool stack).
pub(super) const fn pack_mapping_links(left: u32, right: u32) -> u64 {
    ((left as u64) << 32) | (right as u64)
}

pub(super) const fn mapping_left(links: u64) -> u32 {
    (links >> 32) as u32
}

pub(super) const fn mapping_right(links: u64) -> u32 {
    links as u32
}

/// Packed page-aligned virtual address plus 6-bit AVL subtree height (1..=63)
/// stored in `CowMappingRecord.virtual_address`.
const MAPPING_HEIGHT_MASK: u64 = 0x3f;

pub(super) const fn pack_mapping_va(va: u64, height: u8) -> u64 {
    (va & !(PAGE_SIZE - 1)) | ((height as u64) & MAPPING_HEIGHT_MASK)
}

pub(super) const fn mapping_va(packed: u64) -> u64 {
    packed & !(PAGE_SIZE - 1)
}

pub(super) const fn mapping_height(packed: u64) -> u8 {
    (packed & MAPPING_HEIGHT_MASK) as u8
}
