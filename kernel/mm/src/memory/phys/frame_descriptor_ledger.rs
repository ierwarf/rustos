//! Boot-sized physical-frame descriptors and shared COW mapping records.
//!
//! - **Owner:** `kernel-mm::phys` owns every descriptor role and mapping node.
//! - **Boundary:** A PTE may publish a frame only after its exact descriptor
//!   claim; retirement must reconcile both authorities.
//! - **Lifecycle:** Claim, publish, lock/replace, acknowledge, release, then
//!   return a reusable frame exactly once.
//! - **Concurrency:** Role words serialize shared aliases; root and mapping
//!   intrusive lists are mutated only while the corresponding role is locked.
//! - **Failure:** Capacity, identity, and stale-state failures leave the prior
//!   live descriptor unchanged and return unpublished metadata.
//! - **Forbidden:** No allocator or sleepable lock is entered by descriptor
//!   operations, and no shared frame is reused before the final mapping ends.
//! - **Evidence:** `CowFrameLifecycle`, `PageTableMapTransaction`, and focused
//!   frame-descriptor implementation mutations.
//!
//! One permanent tagged-union descriptor exists for every physical frame.
//! Roots own their lazy table list and count every live data leaf; exclusive
//! data frames name one exact `(root, virtual_address)` mapping; shared frames
//! retain one inline mapping plus an intrusive list drawn from a second
//! boot-sized record pool. The shared role distinguishes anonymous-fork COW
//! from private-file/section COW and carries an opaque backing identity for the
//! latter, so Linux `MAP_PRIVATE` and Windows write-copy sections do not need a
//! second physical-ownership design.
//!
//! The shared-record pool contains one record per physical frame. Keeping the
//! first mapping inline means that budget admits a complete two-way fork of
//! every resident frame; later forks fail before publication if the bounded
//! pool is exhausted. No operation here allocates or takes the allocator lock.

use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use super::PAGE_SIZE;

mod api;
mod encoding;
pub use api::*;
use encoding::*;

const ROLE_FREE: u64 = 0;
const ROLE_ROOT: u64 = 1;
const ROLE_LAZY_TABLE: u64 = 2;
const ROLE_EXCLUSIVE_DATA_CLAIMED: u64 = 3;
const ROLE_EXCLUSIVE_DATA_LIVE: u64 = 4;
const ROLE_SHARED_ANONYMOUS_LIVE: u64 = 6;
const ROLE_SHARED_ANONYMOUS_LOCKED: u64 = 7;
const ROLE_SHARED_PRIVATE_FILE_LIVE: u64 = 8;
const ROLE_SHARED_PRIVATE_FILE_LOCKED: u64 = 9;

const MAPPING_STATE_MASK: u64 = 0x3;
const MAPPING_STATE_FREE: u64 = 0;
const MAPPING_STATE_CLAIMED: u64 = 1;
const MAPPING_STATE_LIVE: u64 = 2;
const MAPPING_STATE_REPLACING: u64 = 3;
const SHARED_LOCK_SPINS: usize = 1_000_000;

// ORDERING: Acquire observes payload initialized before a descriptor or free-list
// head was release-published.
const DESCRIPTOR_ACQUIRE: Ordering = Ordering::Acquire;
// ORDERING: Release publishes payload cleanup/initialization before role or head.
const DESCRIPTOR_RELEASE: Ordering = Ordering::Release;
// ORDERING: AcqRel is the linearization point for role locks, counts, and ABA heads.
const DESCRIPTOR_ACQ_REL: Ordering = Ordering::AcqRel;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CowFrameKind {
    AnonymousFork,
    PrivateFileSection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataLeafRelease {
    FrameRetained,
    FrameReusable,
}

#[derive(Clone, Copy, Debug)]
pub struct SharedAliasTicket {
    frame_phys: u64,
    root_phys: u64,
    virtual_address: u64,
    mapping_id: u32,
    kind: CowFrameKind,
    converted_exclusive: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CowMappingClaim {
    frame_phys: u64,
    root_phys: u64,
    virtual_address: u64,
    mapping_id: u32,
    kind: CowFrameKind,
    mapping_count: u32,
}

impl CowMappingClaim {
    pub const fn kind(self) -> CowFrameKind {
        self.kind
    }

    pub fn can_promote_in_place(self) -> bool {
        self.kind == CowFrameKind::AnonymousFork && self.mapping_count == 1
    }
}

#[repr(C)]
pub struct FrameDescriptorRecord {
    role: AtomicU64,
    owner_or_table_head: AtomicU64,
    next_or_virtual_address: AtomicU64,
    data_leaf_count_or_map_head: AtomicU64,
    shared_backing_identity: AtomicU64,
}

#[repr(C)]
pub struct CowMappingRecord {
    root_and_state: AtomicU64,
    virtual_address: AtomicU64,
    next: AtomicU64,
}

#[cfg(test)]
impl FrameDescriptorRecord {
    const fn empty() -> Self {
        Self {
            role: AtomicU64::new(ROLE_FREE),
            owner_or_table_head: AtomicU64::new(0),
            next_or_virtual_address: AtomicU64::new(0),
            data_leaf_count_or_map_head: AtomicU64::new(0),
            shared_backing_identity: AtomicU64::new(0),
        }
    }
}

#[cfg(test)]
impl CowMappingRecord {
    const fn empty() -> Self {
        Self {
            root_and_state: AtomicU64::new(MAPPING_STATE_FREE),
            virtual_address: AtomicU64::new(0),
            next: AtomicU64::new(0),
        }
    }
}

pub struct FrameDescriptorLedger {
    records: AtomicPtr<FrameDescriptorRecord>,
    frame_count: AtomicUsize,
    mappings: AtomicPtr<CowMappingRecord>,
    mapping_count: AtomicUsize,
    free_mapping_head: AtomicU64,
}

impl FrameDescriptorLedger {
    pub const fn empty() -> Self {
        Self {
            records: AtomicPtr::new(ptr::null_mut()),
            frame_count: AtomicUsize::new(0),
            mappings: AtomicPtr::new(ptr::null_mut()),
            mapping_count: AtomicUsize::new(0),
            free_mapping_head: AtomicU64::new(0),
        }
    }

    fn record(&self, frame_phys: u64) -> Option<&FrameDescriptorRecord> {
        if !frame_phys.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let index = usize::try_from(frame_phys / PAGE_SIZE).ok()?;
        let frame_count = self.frame_count.load(DESCRIPTOR_ACQUIRE);
        let records = self.records.load(DESCRIPTOR_ACQUIRE);
        if records.is_null() || index >= frame_count {
            return None;
        }
        // SAFETY: bootstrap permanently reserves this whole array.
        Some(unsafe { &*records.add(index) })
    }

    fn mapping(&self, id: u32) -> Option<&CowMappingRecord> {
        let index = usize::try_from(id.checked_sub(1)?).ok()?;
        let count = self.mapping_count.load(DESCRIPTOR_ACQUIRE);
        let mappings = self.mappings.load(DESCRIPTOR_ACQUIRE);
        if mappings.is_null() || index >= count {
            return None;
        }
        // SAFETY: bootstrap permanently reserves this whole array.
        Some(unsafe { &*mappings.add(index) })
    }

    pub fn install(
        &self,
        records: *mut FrameDescriptorRecord,
        frame_count: usize,
        mappings: *mut CowMappingRecord,
        mapping_count: usize,
    ) {
        assert!(!records.is_null() && frame_count != 0);
        assert!(!mappings.is_null() && mapping_count != 0);
        assert!(mapping_count <= u32::MAX as usize);
        for index in 0..mapping_count {
            // SAFETY: caller supplied this exact permanent zeroed array.
            let mapping = unsafe { &*mappings.add(index) };
            mapping
                .root_and_state
                .store(MAPPING_STATE_FREE, Ordering::Relaxed);
            mapping.virtual_address.store(0, Ordering::Relaxed);
            let next = if index + 1 < mapping_count {
                u64::try_from(index + 2).expect("COW mapping id overflow")
            } else {
                0
            };
            mapping.next.store(next, Ordering::Relaxed);
        }
        self.records.store(records, DESCRIPTOR_RELEASE);
        self.frame_count.store(frame_count, DESCRIPTOR_RELEASE);
        self.mappings.store(mappings, DESCRIPTOR_RELEASE);
        self.mapping_count.store(mapping_count, DESCRIPTOR_RELEASE);
        self.free_mapping_head
            .store(pack_free_head(0, 1), DESCRIPTOR_RELEASE);
    }

    fn register_root(&self, root_phys: u64) {
        let root = self
            .record(root_phys)
            .expect("new address-space root is outside frame descriptors");
        assert_eq!(root.owner_or_table_head.load(DESCRIPTOR_ACQUIRE), 0);
        assert_eq!(root.next_or_virtual_address.load(Ordering::Relaxed), 0);
        assert_eq!(root.data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE), 0);
        assert_eq!(root.shared_backing_identity.load(Ordering::Relaxed), 0);
        assert_eq!(
            root.role.compare_exchange(
                ROLE_FREE,
                ROLE_ROOT,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE
            ),
            Ok(ROLE_FREE),
            "reused root retained frame-descriptor state"
        );
    }

    fn verify_root(&self, root_phys: u64) -> Option<&FrameDescriptorRecord> {
        let root = self.record(root_phys)?;
        (root.role.load(DESCRIPTOR_ACQUIRE) == ROLE_ROOT).then_some(root)
    }

    fn unregister_root(&self, root_phys: u64) {
        let root = self
            .verify_root(root_phys)
            .expect("retired root lost its frame-descriptor identity");
        assert_eq!(root.owner_or_table_head.load(DESCRIPTOR_ACQUIRE), 0);
        assert_eq!(
            root.data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE),
            0,
            "retired root still owns data leaves"
        );
        assert_eq!(root.shared_backing_identity.load(Ordering::Relaxed), 0);
        assert_eq!(
            root.role.compare_exchange(
                ROLE_ROOT,
                ROLE_FREE,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE
            ),
            Ok(ROLE_ROOT)
        );
    }

    fn claim_table(&self, root_phys: u64, table_phys: u64) -> bool {
        if root_phys == 0 || root_phys == table_phys || self.verify_root(root_phys).is_none() {
            return false;
        }
        let Some(table) = self.record(table_phys) else {
            return false;
        };
        if table
            .role
            .compare_exchange(
                ROLE_FREE,
                ROLE_LAZY_TABLE,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
            )
            .is_err()
        {
            return false;
        }
        table
            .owner_or_table_head
            .store(root_phys, Ordering::Relaxed);
        true
    }

    fn publish_table(&self, root_phys: u64, table_phys: u64) {
        let root = self.verify_root(root_phys).expect("missing table root");
        let table = self
            .record(table_phys)
            .expect("lazy table is outside frame descriptors");
        assert_eq!(table.role.load(DESCRIPTOR_ACQUIRE), ROLE_LAZY_TABLE);
        assert_eq!(table.owner_or_table_head.load(Ordering::Relaxed), root_phys);
        let mut head = root.owner_or_table_head.load(DESCRIPTOR_ACQUIRE);
        loop {
            table.next_or_virtual_address.store(head, Ordering::Relaxed);
            match root.owner_or_table_head.compare_exchange_weak(
                head,
                table_phys,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
            ) {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }

    fn cancel_table(&self, root_phys: u64, table_phys: u64) {
        let table = self
            .record(table_phys)
            .expect("cancelled lazy table is outside frame descriptors");
        assert_eq!(table.owner_or_table_head.load(Ordering::Relaxed), root_phys);
        assert_eq!(table.next_or_virtual_address.load(Ordering::Relaxed), 0);
        table.owner_or_table_head.store(0, Ordering::Relaxed);
        assert_eq!(
            table.role.compare_exchange(
                ROLE_LAZY_TABLE,
                ROLE_FREE,
                DESCRIPTOR_RELEASE,
                DESCRIPTOR_ACQUIRE,
            ),
            Ok(ROLE_LAZY_TABLE),
            "only an unpublished table claim may be cancelled"
        );
    }

    fn drain_tables(&self, root_phys: u64) -> alloc::vec::Vec<u64> {
        let root = self
            .verify_root(root_phys)
            .expect("retired root is outside frame descriptors");
        let mut current = root.owner_or_table_head.swap(0, DESCRIPTOR_ACQ_REL);
        let maximum = self.frame_count.load(DESCRIPTOR_ACQUIRE);
        let mut tables = alloc::vec::Vec::new();
        while current != 0 {
            assert!(tables.len() < maximum, "frame-descriptor table cycle");
            let table = self
                .record(current)
                .expect("table descriptor points outside metadata");
            assert_eq!(table.role.load(DESCRIPTOR_ACQUIRE), ROLE_LAZY_TABLE);
            assert_eq!(table.owner_or_table_head.load(Ordering::Relaxed), root_phys);
            let next = table.next_or_virtual_address.swap(0, DESCRIPTOR_ACQ_REL);
            table.owner_or_table_head.store(0, Ordering::Relaxed);
            table.role.store(ROLE_FREE, DESCRIPTOR_RELEASE);
            tables.push(current);
            current = next;
        }
        tables
    }

    fn claim_exclusive_data(&self, root_phys: u64, va: u64, frame_phys: u64) -> bool {
        if !valid_mapping(root_phys, va, frame_phys) || self.verify_root(root_phys).is_none() {
            return false;
        }
        let Some(frame) = self.record(frame_phys) else {
            return false;
        };
        if frame
            .role
            .compare_exchange(
                ROLE_FREE,
                ROLE_EXCLUSIVE_DATA_CLAIMED,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
            )
            .is_err()
        {
            return false;
        }
        frame
            .owner_or_table_head
            .store(root_phys, Ordering::Relaxed);
        frame.next_or_virtual_address.store(va, Ordering::Relaxed);
        true
    }

    fn publish_exclusive_data(&self, root_phys: u64, va: u64, frame_phys: u64) {
        let root = self.verify_root(root_phys).expect("missing data-leaf root");
        let frame = self
            .record(frame_phys)
            .expect("data leaf is outside frame descriptors");
        assert_eq!(frame.owner_or_table_head.load(Ordering::Relaxed), root_phys);
        assert_eq!(frame.next_or_virtual_address.load(Ordering::Relaxed), va);
        root.data_leaf_count_or_map_head
            .fetch_add(1, DESCRIPTOR_ACQ_REL);
        assert_eq!(
            frame.role.compare_exchange(
                ROLE_EXCLUSIVE_DATA_CLAIMED,
                ROLE_EXCLUSIVE_DATA_LIVE,
                DESCRIPTOR_RELEASE,
                DESCRIPTOR_ACQUIRE,
            ),
            Ok(ROLE_EXCLUSIVE_DATA_CLAIMED),
            "data-leaf publication lost its descriptor claim"
        );
    }

    fn cancel_exclusive_data(&self, root_phys: u64, va: u64, frame_phys: u64) {
        let frame = self
            .record(frame_phys)
            .expect("cancelled data leaf is outside frame descriptors");
        assert_eq!(frame.owner_or_table_head.load(Ordering::Relaxed), root_phys);
        assert_eq!(frame.next_or_virtual_address.load(Ordering::Relaxed), va);
        frame.owner_or_table_head.store(0, Ordering::Relaxed);
        frame.next_or_virtual_address.store(0, Ordering::Relaxed);
        assert_eq!(
            frame.role.compare_exchange(
                ROLE_EXCLUSIVE_DATA_CLAIMED,
                ROLE_FREE,
                DESCRIPTOR_RELEASE,
                DESCRIPTOR_ACQUIRE,
            ),
            Ok(ROLE_EXCLUSIVE_DATA_CLAIMED)
        );
    }

    fn prepare_shared_alias(
        &self,
        owner_root: u64,
        owner_va: u64,
        new_root: u64,
        new_va: u64,
        frame_phys: u64,
        kind: CowFrameKind,
        backing_identity: u64,
    ) -> Option<SharedAliasTicket> {
        if !valid_mapping(owner_root, owner_va, frame_phys)
            || !valid_mapping(new_root, new_va, frame_phys)
            || self.verify_root(owner_root).is_none()
            || self.verify_root(new_root).is_none()
            || kind == CowFrameKind::PrivateFileSection && backing_identity == 0
            || kind == CowFrameKind::AnonymousFork && backing_identity != 0
        {
            return None;
        }
        let mapping_id = self.claim_mapping(new_root, new_va)?;
        let frame = self.record(frame_phys)?;
        let live_role = shared_live_role(kind);
        let locked_role = shared_locked_role(kind);
        let role = frame.role.load(DESCRIPTOR_ACQUIRE);
        let converted_exclusive = if role == ROLE_EXCLUSIVE_DATA_LIVE
            && frame.owner_or_table_head.load(Ordering::Relaxed) == owner_root
            && frame.next_or_virtual_address.load(Ordering::Relaxed) == owner_va
        {
            if frame
                .role
                .compare_exchange(role, locked_role, DESCRIPTOR_ACQ_REL, DESCRIPTOR_ACQUIRE)
                .is_err()
            {
                self.release_mapping_record(mapping_id);
                return None;
            }
            frame
                .data_leaf_count_or_map_head
                .store(pack_shared(1, 0), Ordering::Relaxed);
            frame
                .shared_backing_identity
                .store(backing_identity, Ordering::Relaxed);
            true
        } else if role == live_role {
            if frame
                .role
                .compare_exchange(role, locked_role, DESCRIPTOR_ACQ_REL, DESCRIPTOR_ACQUIRE)
                .is_err()
            {
                self.release_mapping_record(mapping_id);
                return None;
            }
            if frame.shared_backing_identity.load(Ordering::Relaxed) != backing_identity
                || !self.shared_mapping_is_exact_locked(frame, owner_root, owner_va)
            {
                frame.role.store(live_role, DESCRIPTOR_RELEASE);
                self.release_mapping_record(mapping_id);
                return None;
            }
            false
        } else {
            self.release_mapping_record(mapping_id);
            return None;
        };

        let packed = frame.data_leaf_count_or_map_head.load(Ordering::Relaxed);
        let count = shared_count(packed);
        let head = shared_head(packed);
        let Some(next_count) = count.checked_add(1) else {
            if converted_exclusive {
                frame
                    .data_leaf_count_or_map_head
                    .store(0, Ordering::Relaxed);
                frame.shared_backing_identity.store(0, Ordering::Relaxed);
                frame
                    .role
                    .store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
            } else {
                frame.role.store(live_role, DESCRIPTOR_RELEASE);
            }
            self.release_mapping_record(mapping_id);
            return None;
        };
        let mapping = self
            .mapping(mapping_id)
            .expect("claimed shared mapping escaped the boot-sized pool");
        mapping.next.store(u64::from(head), Ordering::Relaxed);
        frame
            .data_leaf_count_or_map_head
            .store(pack_shared(next_count, mapping_id), Ordering::Relaxed);
        Some(SharedAliasTicket {
            frame_phys,
            root_phys: new_root,
            virtual_address: new_va,
            mapping_id,
            kind,
            converted_exclusive,
        })
    }

    fn publish_shared_alias(&self, ticket: SharedAliasTicket) {
        let root = self
            .verify_root(ticket.root_phys)
            .expect("shared alias root disappeared");
        let frame = self
            .record(ticket.frame_phys)
            .expect("shared alias frame disappeared");
        let mapping = self
            .mapping(ticket.mapping_id)
            .expect("shared alias mapping disappeared");
        assert_eq!(
            mapping.root_and_state.compare_exchange(
                ticket.root_phys | MAPPING_STATE_CLAIMED,
                ticket.root_phys | MAPPING_STATE_LIVE,
                DESCRIPTOR_RELEASE,
                DESCRIPTOR_ACQUIRE,
            ),
            Ok(ticket.root_phys | MAPPING_STATE_CLAIMED)
        );
        root.data_leaf_count_or_map_head
            .fetch_add(1, DESCRIPTOR_ACQ_REL);
        assert_eq!(
            frame
                .role
                .swap(shared_live_role(ticket.kind), DESCRIPTOR_RELEASE),
            shared_locked_role(ticket.kind)
        );
    }

    fn cancel_shared_alias(&self, ticket: SharedAliasTicket) {
        let frame = self
            .record(ticket.frame_phys)
            .expect("cancelled shared alias frame disappeared");
        let packed = frame.data_leaf_count_or_map_head.load(Ordering::Relaxed);
        assert_eq!(shared_head(packed), ticket.mapping_id);
        let mapping = self
            .mapping(ticket.mapping_id)
            .expect("cancelled shared alias mapping disappeared");
        let next = u32::try_from(mapping.next.load(Ordering::Relaxed))
            .expect("shared mapping next id overflow");
        let count = shared_count(packed);
        assert!(count >= 2);
        if ticket.converted_exclusive {
            assert_eq!(count, 2);
            assert_eq!(next, 0);
            frame
                .data_leaf_count_or_map_head
                .store(0, Ordering::Relaxed);
            frame.shared_backing_identity.store(0, Ordering::Relaxed);
            frame
                .role
                .store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
        } else {
            frame
                .data_leaf_count_or_map_head
                .store(pack_shared(count - 1, next), Ordering::Relaxed);
            frame
                .role
                .store(shared_live_role(ticket.kind), DESCRIPTOR_RELEASE);
        }
        self.release_mapping_record(ticket.mapping_id);
    }

    fn rollback_shared_alias(&self, ticket: SharedAliasTicket) -> bool {
        let Some(root) = self.verify_root(ticket.root_phys) else {
            return false;
        };
        let Some(frame) = self.record(ticket.frame_phys) else {
            return false;
        };
        if !self.lock_shared(frame, ticket.kind, false) {
            return false;
        }
        if self.find_shared_mapping_locked(frame, ticket.root_phys, ticket.virtual_address)
            != Some(ticket.mapping_id)
        {
            frame
                .role
                .store(shared_live_role(ticket.kind), DESCRIPTOR_RELEASE);
            return false;
        }
        let mapping = self
            .mapping(ticket.mapping_id)
            .expect("rolled-back shared alias mapping disappeared");
        if mapping
            .root_and_state
            .compare_exchange(
                ticket.root_phys | MAPPING_STATE_LIVE,
                ticket.root_phys | MAPPING_STATE_REPLACING,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
            )
            .is_err()
        {
            frame
                .role
                .store(shared_live_role(ticket.kind), DESCRIPTOR_RELEASE);
            return false;
        }
        let result = self.remove_shared_mapping_locked(
            frame,
            ticket.root_phys,
            ticket.virtual_address,
            ticket.mapping_id,
            ticket.kind,
        );
        assert_eq!(result, DataLeafRelease::FrameRetained);
        self.decrement_root_data_count_with(root);
        if ticket.converted_exclusive {
            assert_eq!(
                shared_count(frame.data_leaf_count_or_map_head.load(Ordering::Relaxed)),
                1
            );
            frame
                .data_leaf_count_or_map_head
                .store(0, Ordering::Relaxed);
            frame.shared_backing_identity.store(0, Ordering::Relaxed);
            assert_eq!(
                frame
                    .role
                    .swap(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE),
                shared_live_role(ticket.kind)
            );
        }
        true
    }

    fn claim_private_file_data(
        &self,
        root_phys: u64,
        va: u64,
        frame_phys: u64,
        backing_identity: u64,
    ) -> bool {
        if backing_identity == 0
            || !valid_mapping(root_phys, va, frame_phys)
            || self.verify_root(root_phys).is_none()
        {
            return false;
        }
        let Some(frame) = self.record(frame_phys) else {
            return false;
        };
        if frame
            .role
            .compare_exchange(
                ROLE_FREE,
                ROLE_SHARED_PRIVATE_FILE_LOCKED,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
            )
            .is_err()
        {
            return false;
        }
        frame
            .owner_or_table_head
            .store(root_phys, Ordering::Relaxed);
        frame.next_or_virtual_address.store(va, Ordering::Relaxed);
        frame
            .data_leaf_count_or_map_head
            .store(pack_shared(1, 0), Ordering::Relaxed);
        frame
            .shared_backing_identity
            .store(backing_identity, Ordering::Relaxed);
        true
    }

    fn publish_private_file_data(&self, root_phys: u64, va: u64, frame_phys: u64) {
        let root = self
            .verify_root(root_phys)
            .expect("missing private-file root");
        let frame = self
            .record(frame_phys)
            .expect("private-file frame is outside descriptors");
        assert_eq!(frame.owner_or_table_head.load(Ordering::Relaxed), root_phys);
        assert_eq!(frame.next_or_virtual_address.load(Ordering::Relaxed), va);
        assert_ne!(frame.shared_backing_identity.load(Ordering::Relaxed), 0);
        root.data_leaf_count_or_map_head
            .fetch_add(1, DESCRIPTOR_ACQ_REL);
        assert_eq!(
            frame
                .role
                .swap(ROLE_SHARED_PRIVATE_FILE_LIVE, DESCRIPTOR_RELEASE),
            ROLE_SHARED_PRIVATE_FILE_LOCKED
        );
    }

    fn cancel_private_file_data(&self, root_phys: u64, va: u64, frame_phys: u64) {
        let frame = self
            .record(frame_phys)
            .expect("cancelled private-file frame is outside descriptors");
        assert_eq!(
            frame.role.load(DESCRIPTOR_ACQUIRE),
            ROLE_SHARED_PRIVATE_FILE_LOCKED
        );
        assert_eq!(frame.owner_or_table_head.load(Ordering::Relaxed), root_phys);
        assert_eq!(frame.next_or_virtual_address.load(Ordering::Relaxed), va);
        self.clear_shared_frame(frame);
    }

    fn data_is_owned(&self, root_phys: u64, va: u64, frame_phys: u64) -> bool {
        let Some(frame) = self.record(frame_phys) else {
            return false;
        };
        for _ in 0..SHARED_LOCK_SPINS {
            match frame.role.load(DESCRIPTOR_ACQUIRE) {
                ROLE_EXCLUSIVE_DATA_LIVE => {
                    return frame.owner_or_table_head.load(Ordering::Relaxed) == root_phys
                        && frame.next_or_virtual_address.load(Ordering::Relaxed) == va;
                }
                ROLE_SHARED_ANONYMOUS_LIVE => {
                    if self.lock_shared(frame, CowFrameKind::AnonymousFork, false) {
                        let exact = self.shared_mapping_is_exact_locked(frame, root_phys, va);
                        frame
                            .role
                            .store(ROLE_SHARED_ANONYMOUS_LIVE, DESCRIPTOR_RELEASE);
                        return exact;
                    }
                }
                ROLE_SHARED_PRIVATE_FILE_LIVE => {
                    if self.lock_shared(frame, CowFrameKind::PrivateFileSection, false) {
                        let exact = self.shared_mapping_is_exact_locked(frame, root_phys, va);
                        frame
                            .role
                            .store(ROLE_SHARED_PRIVATE_FILE_LIVE, DESCRIPTOR_RELEASE);
                        return exact;
                    }
                }
                ROLE_SHARED_ANONYMOUS_LOCKED | ROLE_SHARED_PRIVATE_FILE_LOCKED => spin_loop(),
                _ => return false,
            }
        }
        false
    }

    fn cow_identity(
        &self,
        root_phys: u64,
        va: u64,
        frame_phys: u64,
    ) -> Option<(CowFrameKind, u64)> {
        let frame = self.record(frame_phys)?;
        for _ in 0..SHARED_LOCK_SPINS {
            let (kind, live_role) = match frame.role.load(DESCRIPTOR_ACQUIRE) {
                ROLE_SHARED_ANONYMOUS_LIVE => {
                    (CowFrameKind::AnonymousFork, ROLE_SHARED_ANONYMOUS_LIVE)
                }
                ROLE_SHARED_PRIVATE_FILE_LIVE => (
                    CowFrameKind::PrivateFileSection,
                    ROLE_SHARED_PRIVATE_FILE_LIVE,
                ),
                ROLE_SHARED_ANONYMOUS_LOCKED | ROLE_SHARED_PRIVATE_FILE_LOCKED => {
                    spin_loop();
                    continue;
                }
                _ => return None,
            };
            if !self.lock_shared(frame, kind, false) {
                continue;
            }
            let identity = if self.shared_mapping_is_exact_locked(frame, root_phys, va) {
                Some((kind, frame.shared_backing_identity.load(Ordering::Relaxed)))
            } else {
                None
            };
            frame.role.store(live_role, DESCRIPTOR_RELEASE);
            return identity;
        }
        None
    }

    fn try_claim_cow_mapping(
        &self,
        root_phys: u64,
        va: u64,
        frame_phys: u64,
    ) -> Option<CowMappingClaim> {
        let frame = self.record(frame_phys)?;
        let kind = match frame.role.load(DESCRIPTOR_ACQUIRE) {
            ROLE_SHARED_ANONYMOUS_LIVE => CowFrameKind::AnonymousFork,
            ROLE_SHARED_PRIVATE_FILE_LIVE => CowFrameKind::PrivateFileSection,
            _ => return None,
        };
        if !self.lock_shared(frame, kind, true) {
            return None;
        }
        let Some(mapping_id) = self.find_shared_mapping_locked(frame, root_phys, va) else {
            frame.role.store(shared_live_role(kind), DESCRIPTOR_RELEASE);
            return None;
        };
        if mapping_id != 0 {
            let Some(mapping) = self.mapping(mapping_id) else {
                frame.role.store(shared_live_role(kind), DESCRIPTOR_RELEASE);
                return None;
            };
            if mapping
                .root_and_state
                .compare_exchange(
                    root_phys | MAPPING_STATE_LIVE,
                    root_phys | MAPPING_STATE_REPLACING,
                    DESCRIPTOR_ACQ_REL,
                    DESCRIPTOR_ACQUIRE,
                )
                .is_err()
            {
                frame.role.store(shared_live_role(kind), DESCRIPTOR_RELEASE);
                return None;
            }
        }
        Some(CowMappingClaim {
            frame_phys,
            root_phys,
            virtual_address: va,
            mapping_id,
            kind,
            mapping_count: shared_count(frame.data_leaf_count_or_map_head.load(Ordering::Relaxed)),
        })
    }

    fn cancel_cow_mapping(&self, claim: CowMappingClaim) {
        let frame = self
            .record(claim.frame_phys)
            .expect("cancelled COW frame disappeared");
        if claim.mapping_id != 0 {
            let mapping = self
                .mapping(claim.mapping_id)
                .expect("cancelled COW mapping disappeared");
            assert_eq!(
                mapping.root_and_state.compare_exchange(
                    claim.root_phys | MAPPING_STATE_REPLACING,
                    claim.root_phys | MAPPING_STATE_LIVE,
                    DESCRIPTOR_RELEASE,
                    DESCRIPTOR_ACQUIRE,
                ),
                Ok(claim.root_phys | MAPPING_STATE_REPLACING)
            );
        }
        frame
            .role
            .store(shared_live_role(claim.kind), DESCRIPTOR_RELEASE);
    }

    fn commit_cow_mapping(&self, claim: CowMappingClaim) -> DataLeafRelease {
        let frame = self
            .record(claim.frame_phys)
            .expect("committed COW frame disappeared");
        let result = self.remove_shared_mapping_locked(
            frame,
            claim.root_phys,
            claim.virtual_address,
            claim.mapping_id,
            claim.kind,
        );
        self.decrement_root_data_count(claim.root_phys);
        result
    }

    fn promote_cow_mapping_in_place(&self, claim: CowMappingClaim) {
        assert!(claim.can_promote_in_place());
        assert_eq!(claim.mapping_id, 0);
        let frame = self
            .record(claim.frame_phys)
            .expect("promoted COW frame disappeared");
        assert_eq!(
            frame.owner_or_table_head.load(Ordering::Relaxed),
            claim.root_phys
        );
        assert_eq!(
            frame.next_or_virtual_address.load(Ordering::Relaxed),
            claim.virtual_address
        );
        assert_eq!(
            shared_count(frame.data_leaf_count_or_map_head.load(Ordering::Relaxed)),
            1
        );
        frame
            .data_leaf_count_or_map_head
            .store(0, Ordering::Relaxed);
        frame.shared_backing_identity.store(0, Ordering::Relaxed);
        assert_eq!(
            frame
                .role
                .swap(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE),
            ROLE_SHARED_ANONYMOUS_LOCKED
        );
    }

    fn release_data(&self, root_phys: u64, va: u64, frame_phys: u64) -> Option<DataLeafRelease> {
        let root = self.verify_root(root_phys)?;
        let frame = self.record(frame_phys)?;
        for _ in 0..SHARED_LOCK_SPINS {
            match frame.role.load(DESCRIPTOR_ACQUIRE) {
                ROLE_EXCLUSIVE_DATA_LIVE => {
                    if frame.owner_or_table_head.load(Ordering::Relaxed) != root_phys
                        || frame.next_or_virtual_address.load(Ordering::Relaxed) != va
                        || frame
                            .role
                            .compare_exchange(
                                ROLE_EXCLUSIVE_DATA_LIVE,
                                ROLE_EXCLUSIVE_DATA_CLAIMED,
                                DESCRIPTOR_ACQ_REL,
                                DESCRIPTOR_ACQUIRE,
                            )
                            .is_err()
                    {
                        continue;
                    }
                    self.decrement_root_data_count_with(root);
                    frame.owner_or_table_head.store(0, Ordering::Relaxed);
                    frame.next_or_virtual_address.store(0, Ordering::Relaxed);
                    frame.role.store(ROLE_FREE, DESCRIPTOR_RELEASE);
                    return Some(DataLeafRelease::FrameReusable);
                }
                ROLE_SHARED_ANONYMOUS_LIVE => {
                    if self.lock_shared(frame, CowFrameKind::AnonymousFork, false) {
                        return self.release_locked_shared(
                            root,
                            frame,
                            root_phys,
                            va,
                            CowFrameKind::AnonymousFork,
                        );
                    }
                }
                ROLE_SHARED_PRIVATE_FILE_LIVE => {
                    if self.lock_shared(frame, CowFrameKind::PrivateFileSection, false) {
                        return self.release_locked_shared(
                            root,
                            frame,
                            root_phys,
                            va,
                            CowFrameKind::PrivateFileSection,
                        );
                    }
                }
                ROLE_SHARED_ANONYMOUS_LOCKED | ROLE_SHARED_PRIVATE_FILE_LOCKED => spin_loop(),
                _ => return None,
            }
        }
        None
    }

    fn release_locked_shared(
        &self,
        root: &FrameDescriptorRecord,
        frame: &FrameDescriptorRecord,
        root_phys: u64,
        va: u64,
        kind: CowFrameKind,
    ) -> Option<DataLeafRelease> {
        let Some(mapping_id) = self.find_shared_mapping_locked(frame, root_phys, va) else {
            frame.role.store(shared_live_role(kind), DESCRIPTOR_RELEASE);
            return None;
        };
        if mapping_id != 0 {
            let Some(mapping) = self.mapping(mapping_id) else {
                frame.role.store(shared_live_role(kind), DESCRIPTOR_RELEASE);
                return None;
            };
            if mapping
                .root_and_state
                .compare_exchange(
                    root_phys | MAPPING_STATE_LIVE,
                    root_phys | MAPPING_STATE_REPLACING,
                    DESCRIPTOR_ACQ_REL,
                    DESCRIPTOR_ACQUIRE,
                )
                .is_err()
            {
                frame.role.store(shared_live_role(kind), DESCRIPTOR_RELEASE);
                return None;
            }
        }
        let result = self.remove_shared_mapping_locked(frame, root_phys, va, mapping_id, kind);
        self.decrement_root_data_count_with(root);
        Some(result)
    }

    fn remove_shared_mapping_locked(
        &self,
        frame: &FrameDescriptorRecord,
        root_phys: u64,
        va: u64,
        mapping_id: u32,
        kind: CowFrameKind,
    ) -> DataLeafRelease {
        let packed = frame.data_leaf_count_or_map_head.load(Ordering::Relaxed);
        let count = shared_count(packed);
        let head = shared_head(packed);
        assert!(count != 0, "shared mapping count underflow");
        if mapping_id == 0 {
            assert_eq!(frame.owner_or_table_head.load(Ordering::Relaxed), root_phys);
            assert_eq!(frame.next_or_virtual_address.load(Ordering::Relaxed), va);
            if count == 1 {
                self.clear_shared_frame(frame);
                return DataLeafRelease::FrameReusable;
            }
            let promoted = self
                .mapping(head)
                .expect("shared primary promotion lost its head record");
            let promoted_root_state = promoted.root_and_state.load(DESCRIPTOR_ACQUIRE);
            assert_eq!(promoted_root_state & MAPPING_STATE_MASK, MAPPING_STATE_LIVE);
            let promoted_root = promoted_root_state & !MAPPING_STATE_MASK;
            let promoted_va = promoted.virtual_address.load(Ordering::Relaxed);
            let next = u32::try_from(promoted.next.load(Ordering::Relaxed))
                .expect("shared promotion next id overflow");
            frame
                .owner_or_table_head
                .store(promoted_root, Ordering::Relaxed);
            frame
                .next_or_virtual_address
                .store(promoted_va, Ordering::Relaxed);
            frame
                .data_leaf_count_or_map_head
                .store(pack_shared(count - 1, next), Ordering::Relaxed);
            self.release_mapping_record(head);
        } else {
            let mut previous = 0_u32;
            let mut current = head;
            let mut traversed = 0_usize;
            while current != mapping_id {
                traversed += 1;
                assert!(
                    traversed <= self.mapping_count.load(DESCRIPTOR_ACQUIRE),
                    "shared mapping list cycle"
                );
                let record = self
                    .mapping(current)
                    .expect("shared mapping id escaped pool");
                previous = current;
                current = u32::try_from(record.next.load(Ordering::Relaxed))
                    .expect("shared mapping next id overflow");
                assert_ne!(current, 0, "shared mapping claim was not linked");
            }
            let record = self
                .mapping(mapping_id)
                .expect("shared mapping claim escaped pool");
            let next = u32::try_from(record.next.load(Ordering::Relaxed))
                .expect("shared mapping next id overflow");
            if previous == 0 {
                frame
                    .data_leaf_count_or_map_head
                    .store(pack_shared(count - 1, next), Ordering::Relaxed);
            } else {
                self.mapping(previous)
                    .expect("shared mapping predecessor escaped pool")
                    .next
                    .store(u64::from(next), Ordering::Relaxed);
                frame
                    .data_leaf_count_or_map_head
                    .store(pack_shared(count - 1, head), Ordering::Relaxed);
            }
            self.release_mapping_record(mapping_id);
        }
        frame.role.store(shared_live_role(kind), DESCRIPTOR_RELEASE);
        DataLeafRelease::FrameRetained
    }

    fn shared_mapping_is_exact_locked(
        &self,
        frame: &FrameDescriptorRecord,
        root_phys: u64,
        va: u64,
    ) -> bool {
        self.find_shared_mapping_locked(frame, root_phys, va)
            .is_some()
    }

    fn find_shared_mapping_locked(
        &self,
        frame: &FrameDescriptorRecord,
        root_phys: u64,
        va: u64,
    ) -> Option<u32> {
        if frame.owner_or_table_head.load(Ordering::Relaxed) == root_phys
            && frame.next_or_virtual_address.load(Ordering::Relaxed) == va
        {
            return Some(0);
        }
        let mut current = shared_head(frame.data_leaf_count_or_map_head.load(Ordering::Relaxed));
        let maximum = self.mapping_count.load(DESCRIPTOR_ACQUIRE);
        let mut traversed = 0_usize;
        while current != 0 {
            traversed += 1;
            if traversed > maximum {
                return None;
            }
            let mapping = self.mapping(current)?;
            let root_state = mapping.root_and_state.load(DESCRIPTOR_ACQUIRE);
            let state = root_state & MAPPING_STATE_MASK;
            let root = root_state & !MAPPING_STATE_MASK;
            if (state == MAPPING_STATE_LIVE || state == MAPPING_STATE_REPLACING)
                && root == root_phys
                && mapping.virtual_address.load(Ordering::Relaxed) == va
            {
                return Some(current);
            }
            current = u32::try_from(mapping.next.load(Ordering::Relaxed)).ok()?;
        }
        None
    }

    fn lock_shared(
        &self,
        frame: &FrameDescriptorRecord,
        kind: CowFrameKind,
        try_only: bool,
    ) -> bool {
        let live = shared_live_role(kind);
        let locked = shared_locked_role(kind);
        let attempts = if try_only { 1 } else { SHARED_LOCK_SPINS };
        for _ in 0..attempts {
            match frame
                .role
                .compare_exchange(live, locked, DESCRIPTOR_ACQ_REL, DESCRIPTOR_ACQUIRE)
            {
                Ok(_) => return true,
                Err(observed) if observed == locked => spin_loop(),
                Err(_) => return false,
            }
        }
        false
    }

    fn clear_shared_frame(&self, frame: &FrameDescriptorRecord) {
        frame.owner_or_table_head.store(0, Ordering::Relaxed);
        frame.next_or_virtual_address.store(0, Ordering::Relaxed);
        frame
            .data_leaf_count_or_map_head
            .store(0, Ordering::Relaxed);
        frame.shared_backing_identity.store(0, Ordering::Relaxed);
        frame.role.store(ROLE_FREE, DESCRIPTOR_RELEASE);
    }

    fn claim_mapping(&self, root_phys: u64, va: u64) -> Option<u32> {
        let mut head = self.free_mapping_head.load(DESCRIPTOR_ACQUIRE);
        loop {
            let id = free_head_id(head);
            if id == 0 {
                return None;
            }
            let mapping = self.mapping(id)?;
            let next = u32::try_from(mapping.next.load(Ordering::Relaxed)).ok()?;
            let generation = free_head_generation(head)
                .checked_add(1)
                .expect("COW mapping free-list generation exhausted");
            let desired = pack_free_head(generation, next);
            match self.free_mapping_head.compare_exchange_weak(
                head,
                desired,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
            ) {
                Ok(_) => {
                    assert_eq!(
                        mapping.root_and_state.load(DESCRIPTOR_ACQUIRE),
                        MAPPING_STATE_FREE
                    );
                    mapping.next.store(0, Ordering::Relaxed);
                    mapping.virtual_address.store(va, Ordering::Relaxed);
                    mapping
                        .root_and_state
                        .store(root_phys | MAPPING_STATE_CLAIMED, DESCRIPTOR_RELEASE);
                    return Some(id);
                }
                Err(observed) => head = observed,
            }
        }
    }

    fn release_mapping_record(&self, id: u32) {
        let mapping = self
            .mapping(id)
            .expect("released COW mapping escaped its pool");
        mapping.virtual_address.store(0, Ordering::Relaxed);
        mapping
            .root_and_state
            .store(MAPPING_STATE_FREE, DESCRIPTOR_RELEASE);
        let mut head = self.free_mapping_head.load(DESCRIPTOR_ACQUIRE);
        loop {
            mapping
                .next
                .store(u64::from(free_head_id(head)), Ordering::Relaxed);
            let generation = free_head_generation(head)
                .checked_add(1)
                .expect("COW mapping free-list generation exhausted");
            let desired = pack_free_head(generation, id);
            match self.free_mapping_head.compare_exchange_weak(
                head,
                desired,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
            ) {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }

    fn decrement_root_data_count(&self, root_phys: u64) {
        let root = self
            .verify_root(root_phys)
            .expect("COW mapping root disappeared");
        self.decrement_root_data_count_with(root);
    }

    fn decrement_root_data_count_with(&self, root: &FrameDescriptorRecord) {
        let previous = root
            .data_leaf_count_or_map_head
            .fetch_sub(1, DESCRIPTOR_ACQ_REL);
        assert!(previous != 0, "data-leaf root count underflow");
    }
}

pub static FRAME_DESCRIPTORS: FrameDescriptorLedger = FrameDescriptorLedger::empty();

#[cfg(test)]
mod tests;
