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
