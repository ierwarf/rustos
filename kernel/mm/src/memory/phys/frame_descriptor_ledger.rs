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
//! An exclusive 2 MiB head may hold a root-owned split-table deposit in its
//! otherwise-unused backing word. Demotion detaches it before ordinary 4 KiB
//! sharing, rekeying or release can repurpose that descriptor.
//!
//! The shared-record pool contains one record per physical frame. Keeping the
//! first mapping inline means that budget admits a complete two-way fork of
//! every resident frame; later forks fail before publication if the bounded
//! pool is exhausted. No operation here allocates or takes the allocator lock.

use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use super::PAGE_SIZE;
use crate::memory::{charge, kmem_charge};

mod api;
mod anon_run;
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
const SHARED_LOCK_SPINS: usize = 100_000_000;

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

/// Maximum number of shared-alias leaves prepared in one batched ledger run.
pub const SHARED_ALIAS_RUN_MAX: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedAliasMode {
    /// Convert an exclusive parent leaf or attach to an existing anonymous-fork
    /// shared leaf (`CowFrameKind::AnonymousFork`, backing identity 0).
    AnonymousFork,
    /// Inherit the existing shared COW kind and backing identity from the
    /// parent's live shared descriptor without an extra lock round-trip.
    InheritShared,
    /// Attach with an explicit COW kind and backing identity.
    Explicit {
        kind: CowFrameKind,
        backing_identity: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedAliasRequest {
    pub owner_va: u64,
    pub new_va: u64,
    pub frame_phys: u64,
    pub mode: SharedAliasMode,
}

impl SharedAliasRequest {
    pub const EMPTY: Self = Self {
        owner_va: 0,
        new_va: 0,
        frame_phys: 0,
        mode: SharedAliasMode::AnonymousFork,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedAliasRunError {
    /// Root charge quota, COW mapping record pool, or shared mapping count was
    /// exhausted.
    OutOfFrames,
    /// Root, mapping key, parent ownership, or shared role validation failed.
    InvalidFrameOwnership,
}

#[derive(Clone, Copy, Debug)]
pub struct SharedAliasTicket {
    frame_phys: u64,
    root_phys: u64,
    virtual_address: u64,
    mapping_id: u32,
    kind: CowFrameKind,
    converted_exclusive: bool,
    owns_frame_lock: bool,
    /// Root whose leaf this alias turns from privately owned into shared at
    /// publication (0 when none). See `adjust_shared_leaves`.
    bump_owner_root: u64,
}

impl SharedAliasTicket {
    pub const EMPTY: Self = Self {
        frame_phys: 0,
        root_phys: 0,
        virtual_address: 0,
        mapping_id: 0,
        kind: CowFrameKind::AnonymousFork,
        converted_exclusive: false,
        owns_frame_lock: false,
        bump_owner_root: 0,
    };
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
    /// Root shared-leaf census, shared backing identity, or exclusive huge
    /// head's deposited split-table frame. Zero for ordinary exclusive leaves.
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
        // The backing word of a root is its shared-leaf count (see
        // `adjust_shared_leaves`): zero once no leaf is left. It is a census
        // statistic, so a miscount is a test failure, not a kernel panic.
        debug_assert_eq!(root.shared_backing_identity.load(Ordering::Relaxed), 0);
        root.shared_backing_identity.store(0, Ordering::Relaxed);
        // The binding word is free for roots; the count is zero here, so
        // nothing is left charged to the group being released. The root frame
        // itself is the last page-table byte this root still owns.
        if let Some(group) = Self::root_group(root) {
            kmem_charge::uncharge(kmem_charge::Class::PageTable, group, PAGE_SIZE);
        }
        root.next_or_virtual_address.store(0, Ordering::Relaxed);
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

    fn claim_table(&self, root_phys: u64, table_phys: u64) -> TableClaim {
        if root_phys == 0 || root_phys == table_phys {
            return TableClaim::Invalid;
        }
        let Some(root) = self.verify_root(root_phys) else {
            return TableClaim::Invalid;
        };
        let Some(table) = self.record(table_phys) else {
            return TableClaim::Invalid;
        };
        // Bill the table frame before taking it, so a refused charge leaves
        // the descriptor untouched. An unbound root charges nothing.
        let group = Self::root_group(root);
        if let Some(group) = group
            && !kmem_charge::try_charge(kmem_charge::Class::PageTable, group, PAGE_SIZE)
        {
            return TableClaim::Quota;
        }
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
            if let Some(group) = group {
                kmem_charge::uncharge(kmem_charge::Class::PageTable, group, PAGE_SIZE);
            }
            return TableClaim::Invalid;
        }
        table
            .owner_or_table_head
            .store(root_phys, Ordering::Relaxed);
        TableClaim::Claimed
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
        self.refund_tables(root_phys, 1);
    }

    /// Returns `tables` page-table frames of `root_phys` to its kmem group.
    fn refund_tables(&self, root_phys: u64, tables: u64) {
        if let Some(group) = self.verify_root(root_phys).and_then(Self::root_group) {
            kmem_charge::uncharge(kmem_charge::Class::PageTable, group, tables * PAGE_SIZE);
        }
    }

    /// Counts the published lazy tables of a retiring root without allocating.
    fn table_count(&self, root_phys: u64) -> usize {
        let root = self
            .verify_root(root_phys)
            .expect("retired root is outside frame descriptors");
        let maximum = self.frame_count.load(DESCRIPTOR_ACQUIRE);
        let mut current = root.owner_or_table_head.load(DESCRIPTOR_ACQUIRE);
        let mut count = 0;
        while current != 0 {
            assert!(count < maximum, "frame-descriptor table cycle");
            let table = self
                .record(current)
                .expect("table descriptor points outside metadata");
            current = table.next_or_virtual_address.load(Ordering::Relaxed);
            count += 1;
        }
        count
    }

    /// Whether `table_phys` is a published lazy table owned by `root_phys`.
    fn table_is_owned(&self, root_phys: u64, table_phys: u64) -> bool {
        self.record(table_phys).is_some_and(|table| {
            table.role.load(DESCRIPTOR_ACQUIRE) == ROLE_LAZY_TABLE
                && table.owner_or_table_head.load(Ordering::Relaxed) == root_phys
        })
    }

    /// Detaches the newest published lazy table of a retiring root.
    ///
    /// Retirement holds the only reference to the root, so no publisher races
    /// the head. Popping one record at a time lets the caller settle each table
    /// frame in a bounded batch instead of materializing the whole set.
    fn pop_table(&self, root_phys: u64) -> Option<u64> {
        let root = self
            .verify_root(root_phys)
            .expect("retired root is outside frame descriptors");
        let current = root.owner_or_table_head.load(DESCRIPTOR_ACQUIRE);
        if current == 0 {
            return None;
        }
        let table = self
            .record(current)
            .expect("table descriptor points outside metadata");
        assert_eq!(table.role.load(DESCRIPTOR_ACQUIRE), ROLE_LAZY_TABLE);
        assert_eq!(table.owner_or_table_head.load(Ordering::Relaxed), root_phys);
        let next = table.next_or_virtual_address.swap(0, DESCRIPTOR_ACQ_REL);
        assert_eq!(
            root.owner_or_table_head.compare_exchange(
                current,
                next,
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE
            ),
            Ok(current),
            "retiring root published a table during retirement"
        );
        table.owner_or_table_head.store(0, Ordering::Relaxed);
        table.role.store(ROLE_FREE, DESCRIPTOR_RELEASE);
        self.refund_tables(root_phys, 1);
        Some(current)
    }

    fn claim_exclusive_data(&self, root_phys: u64, va: u64, frame_phys: u64, reserve: bool) -> DataClaim {
        if !valid_mapping(root_phys, va, frame_phys) || self.verify_root(root_phys).is_none() {
            return DataClaim::Invalid;
        }
        let Some(frame) = self.record(frame_phys) else {
            return DataClaim::Invalid;
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
            return DataClaim::Invalid;
        }
        if reserve && !self.try_charge_root(root_phys, 1) {
            frame.role.store(ROLE_FREE, DESCRIPTOR_RELEASE);
            return DataClaim::Quota;
        }
        frame
            .owner_or_table_head
            .store(root_phys, Ordering::Relaxed);
        frame.next_or_virtual_address.store(va, Ordering::Relaxed);
        DataClaim::Claimed
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

    fn cancel_exclusive_data(&self, root_phys: u64, va: u64, frame_phys: u64, reserved: bool) {
        let frame = self
            .record(frame_phys)
            .expect("cancelled data leaf is outside frame descriptors");
        assert_eq!(frame.owner_or_table_head.load(Ordering::Relaxed), root_phys);
        assert_eq!(frame.next_or_virtual_address.load(Ordering::Relaxed), va);
        assert_eq!(
            frame.shared_backing_identity.load(Ordering::Relaxed),
            0,
            "cancelled anonymous huge leaf retained its split-table deposit"
        );
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
        if reserved {
            self.uncharge_root(self.verify_root(root_phys).expect("missing data-leaf root"), 1);
        }
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
        let cache_owner = owner_root == 0 && owner_va == 0 && kind == CowFrameKind::PrivateFileSection;
        if (!cache_owner && !valid_mapping(owner_root, owner_va, frame_phys))
            || !valid_mapping(new_root, new_va, frame_phys)
            || (!cache_owner && self.verify_root(owner_root).is_none())
            || self.verify_root(new_root).is_none()
            || kind == CowFrameKind::PrivateFileSection && backing_identity == 0
            || kind == CowFrameKind::AnonymousFork && backing_identity != 0
        {
            return None;
        }
        let frame = self.record(frame_phys)?;
        let mapping_id = self.claim_mapping(new_root, new_va)?;
        let live_role = shared_live_role(kind);
        let locked_role = shared_locked_role(kind);
        let role = frame.role.load(DESCRIPTOR_ACQUIRE);
        let converted_exclusive = if !cache_owner
            && matches!(role, ROLE_EXCLUSIVE_DATA_LIVE | ROLE_EXCLUSIVE_DATA_CLAIMED)
            && frame.owner_or_table_head.load(Ordering::Relaxed) == owner_root
            && frame.next_or_virtual_address.load(Ordering::Relaxed) == owner_va
        {
            let mut acquired = false;
            for _ in 0..SHARED_LOCK_SPINS {
                match frame.role.compare_exchange(
                    ROLE_EXCLUSIVE_DATA_LIVE,
                    locked_role,
                    DESCRIPTOR_ACQ_REL,
                    DESCRIPTOR_ACQUIRE,
                ) {
                    Ok(_) => {
                        acquired = true;
                        break;
                    }
                    Err(ROLE_EXCLUSIVE_DATA_CLAIMED) => spin_loop(),
                    Err(_) => break,
                }
            }
            if !acquired {
                self.release_mapping_record(mapping_id);
                return None;
            }
            // A huge leaf must be demoted before a 4 KiB COW alias can replace
            // its descriptor's split-table deposit with a backing identity.
            if frame.shared_backing_identity.load(Ordering::Relaxed) != 0 {
                frame.role.store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
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
        } else if matches!(role, r if r == live_role || r == locked_role) {
            if !self.lock_shared(frame, kind, false) {
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
        let ticket = SharedAliasTicket {
            frame_phys,
            root_phys: new_root,
            virtual_address: new_va,
            mapping_id,
            kind,
            converted_exclusive,
            owns_frame_lock: true,
            bump_owner_root: Self::bump_owner(owner_root, kind, converted_exclusive, count),
        };
        if !self.try_charge_root(new_root, 1) {
            self.cancel_shared_alias_inner(ticket);
            return None;
        }
        Some(ticket)
    }

    /// Prepares a run of shared COW aliases from `owner_root` into `new_root`
    /// under a single root charge and ascending physical-frame lock order.
    ///
    /// On any validation, quota, pool, or lock failure, every prior frame in
    /// the run is unwound in reverse lock order and the entire run charge is
    /// refunded, leaving no frame mutated or charged.
    fn prepare_shared_alias_run(
        &self,
        owner_root: u64,
        new_root: u64,
        requests: &[SharedAliasRequest],
        tickets_out: &mut [SharedAliasTicket],
    ) -> Result<(), SharedAliasRunError> {
        let len = requests.len();
        if len == 0 {
            return Ok(());
        }
        if len > SHARED_ALIAS_RUN_MAX || tickets_out.len() < len {
            return Err(SharedAliasRunError::InvalidFrameOwnership);
        }
        if self.verify_root(owner_root).is_none() {
            return Err(SharedAliasRunError::InvalidFrameOwnership);
        }
        let Some(new_root_record) = self.verify_root(new_root) else {
            return Err(SharedAliasRunError::InvalidFrameOwnership);
        };

        // Pre-validate every request key and mode before charging or locking
        // any frame descriptor, and check whether frames are already strictly
        // ascending in physical address.
        let mut strictly_ascending = true;
        let mut prev_phys = 0_u64;
        for (idx, req) in requests.iter().enumerate() {
            if !valid_mapping(owner_root, req.owner_va, req.frame_phys)
                || !valid_mapping(new_root, req.new_va, req.frame_phys)
                || self.record(req.frame_phys).is_none()
            {
                return Err(SharedAliasRunError::InvalidFrameOwnership);
            }
            if let SharedAliasMode::Explicit {
                kind,
                backing_identity,
            } = req.mode
                && ((kind == CowFrameKind::PrivateFileSection && backing_identity == 0)
                    || (kind == CowFrameKind::AnonymousFork && backing_identity != 0))
            {
                return Err(SharedAliasRunError::InvalidFrameOwnership);
            }
            if idx != 0 && req.frame_phys <= prev_phys {
                strictly_ascending = false;
            }
            prev_phys = req.frame_phys;
        }

        // Single hierarchical charge reservation for the entire run.
        let pages = len as u64;
        if !self.try_charge_root(new_root, pages) {
            return Err(SharedAliasRunError::OutOfFrames);
        }

        // Build lock-acquisition order in ascending `frame_phys` so concurrent
        // runs can never AB-BA deadlock and duplicate physical frames in one
        // run are adjacent.
        let mut order = [0_u16; SHARED_ALIAS_RUN_MAX];
        for (pos, slot) in order[..len].iter_mut().enumerate() {
            *slot = pos as u16;
        }
        if !strictly_ascending {
            for i in 1..len {
                let key_idx = order[i];
                let key_phys = requests[key_idx as usize].frame_phys;
                let mut j = i;
                while j > 0 && requests[order[j - 1] as usize].frame_phys > key_phys {
                    order[j] = order[j - 1];
                    j -= 1;
                }
                order[j] = key_idx;
            }
        }

        for pos in 0..len {
            let idx = order[pos] as usize;
            let req = requests[idx];
            let same_as_prev = pos > 0
                && requests[order[pos - 1] as usize].frame_phys == req.frame_phys;
            let prev_kind = if same_as_prev {
                Some(tickets_out[order[pos - 1] as usize].kind)
            } else {
                None
            };
            match self.prepare_shared_alias_run_entry(
                owner_root,
                new_root,
                req,
                prev_kind,
            ) {
                Ok(ticket) => {
                    tickets_out[idx] = ticket;
                }
                Err(error) => {
                    for undo_pos in (0..pos).rev() {
                        let undo_ticket = tickets_out[order[undo_pos] as usize];
                        if undo_ticket.owns_frame_lock {
                            self.cancel_shared_alias_inner(undo_ticket);
                        } else {
                            self.cancel_shared_alias_duplicate_head(undo_ticket);
                        }
                    }
                    self.uncharge_root(new_root_record, pages);
                    return Err(error);
                }
            }
        }

        Ok(())
    }

    fn prepare_shared_alias_run_entry(
        &self,
        owner_root: u64,
        new_root: u64,
        req: SharedAliasRequest,
        already_locked_kind: Option<CowFrameKind>,
    ) -> Result<SharedAliasTicket, SharedAliasRunError> {
        let frame = self
            .record(req.frame_phys)
            .ok_or(SharedAliasRunError::InvalidFrameOwnership)?;
        let mapping_id = self
            .claim_mapping(new_root, req.new_va)
            .ok_or(SharedAliasRunError::OutOfFrames)?;

        let (kind, converted_exclusive, owns_frame_lock) =
            if let Some(locked_kind) = already_locked_kind {
                // This physical frame is already locked by an earlier entry in
                // the same run. Verify mode compatibility and exact parent
                // ownership without re-acquiring the role lock.
                let backing = frame.shared_backing_identity.load(Ordering::Relaxed);
                let mode_ok = match req.mode {
                    SharedAliasMode::InheritShared => true,
                    SharedAliasMode::AnonymousFork => {
                        locked_kind == CowFrameKind::AnonymousFork && backing == 0
                    }
                    SharedAliasMode::Explicit {
                        kind,
                        backing_identity,
                    } => kind == locked_kind && backing == backing_identity,
                };
                if !mode_ok
                    || !self.shared_mapping_is_exact_locked(frame, owner_root, req.owner_va)
                {
                    self.release_mapping_record(mapping_id);
                    return Err(SharedAliasRunError::InvalidFrameOwnership);
                }
                (locked_kind, false, false)
            } else {
                match req.mode {
                    SharedAliasMode::InheritShared => {
                        let role = frame.role.load(DESCRIPTOR_ACQUIRE);
                        let kind = match role {
                            ROLE_SHARED_ANONYMOUS_LIVE | ROLE_SHARED_ANONYMOUS_LOCKED => {
                                CowFrameKind::AnonymousFork
                            }
                            ROLE_SHARED_PRIVATE_FILE_LIVE | ROLE_SHARED_PRIVATE_FILE_LOCKED => {
                                CowFrameKind::PrivateFileSection
                            }
                            _ => {
                                self.release_mapping_record(mapping_id);
                                return Err(SharedAliasRunError::InvalidFrameOwnership);
                            }
                        };
                        if !self.lock_shared(frame, kind, false) {
                            self.release_mapping_record(mapping_id);
                            return Err(SharedAliasRunError::InvalidFrameOwnership);
                        }
                        if !self.shared_mapping_is_exact_locked(frame, owner_root, req.owner_va) {
                            frame
                                .role
                                .store(shared_live_role(kind), DESCRIPTOR_RELEASE);
                            self.release_mapping_record(mapping_id);
                            return Err(SharedAliasRunError::InvalidFrameOwnership);
                        }
                        (kind, false, true)
                    }
                    SharedAliasMode::AnonymousFork | SharedAliasMode::Explicit { .. } => {
                        let (kind, backing_identity) = match req.mode {
                            SharedAliasMode::AnonymousFork => (CowFrameKind::AnonymousFork, 0),
                            SharedAliasMode::Explicit {
                                kind,
                                backing_identity,
                            } => (kind, backing_identity),
                            SharedAliasMode::InheritShared => unreachable!(),
                        };
                        let live_role = shared_live_role(kind);
                        let locked_role = shared_locked_role(kind);
                        let role = frame.role.load(DESCRIPTOR_ACQUIRE);
                        let converted = if matches!(
                            role,
                            ROLE_EXCLUSIVE_DATA_LIVE | ROLE_EXCLUSIVE_DATA_CLAIMED
                        ) && frame.owner_or_table_head.load(Ordering::Relaxed)
                            == owner_root
                            && frame.next_or_virtual_address.load(Ordering::Relaxed)
                                == req.owner_va
                        {
                            let mut acquired = false;
                            for _ in 0..SHARED_LOCK_SPINS {
                                match frame.role.compare_exchange(
                                    ROLE_EXCLUSIVE_DATA_LIVE,
                                    locked_role,
                                    DESCRIPTOR_ACQ_REL,
                                    DESCRIPTOR_ACQUIRE,
                                ) {
                                    Ok(_) => {
                                        acquired = true;
                                        break;
                                    }
                                    Err(ROLE_EXCLUSIVE_DATA_CLAIMED) => spin_loop(),
                                    Err(_) => break,
                                }
                            }
                            if !acquired {
                                self.release_mapping_record(mapping_id);
                                return Err(SharedAliasRunError::InvalidFrameOwnership);
                            }
                            if frame.shared_backing_identity.load(Ordering::Relaxed) != 0 {
                                frame.role.store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
                                self.release_mapping_record(mapping_id);
                                return Err(SharedAliasRunError::InvalidFrameOwnership);
                            }
                            frame
                                .data_leaf_count_or_map_head
                                .store(pack_shared(1, 0), Ordering::Relaxed);
                            frame
                                .shared_backing_identity
                                .store(backing_identity, Ordering::Relaxed);
                            true
                        } else if role == live_role || role == locked_role {
                            if !self.lock_shared(frame, kind, false) {
                                self.release_mapping_record(mapping_id);
                                return Err(SharedAliasRunError::InvalidFrameOwnership);
                            }
                            if frame.shared_backing_identity.load(Ordering::Relaxed)
                                != backing_identity
                                || !self.shared_mapping_is_exact_locked(
                                    frame,
                                    owner_root,
                                    req.owner_va,
                                )
                            {
                                frame.role.store(live_role, DESCRIPTOR_RELEASE);
                                self.release_mapping_record(mapping_id);
                                return Err(SharedAliasRunError::InvalidFrameOwnership);
                            }
                            false
                        } else {
                            self.release_mapping_record(mapping_id);
                            return Err(SharedAliasRunError::InvalidFrameOwnership);
                        };
                        (kind, converted, true)
                    }
                }
            };

        let packed = frame.data_leaf_count_or_map_head.load(Ordering::Relaxed);
        let count = shared_count(packed);
        let head = shared_head(packed);
        let Some(next_count) = count.checked_add(1) else {
            if owns_frame_lock {
                if converted_exclusive {
                    frame
                        .data_leaf_count_or_map_head
                        .store(0, Ordering::Relaxed);
                    frame.shared_backing_identity.store(0, Ordering::Relaxed);
                    frame
                        .role
                        .store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
                } else {
                    frame
                        .role
                        .store(shared_live_role(kind), DESCRIPTOR_RELEASE);
                }
            }
            self.release_mapping_record(mapping_id);
            return Err(SharedAliasRunError::OutOfFrames);
        };
        let mapping = self
            .mapping(mapping_id)
            .expect("claimed shared mapping escaped the boot-sized pool");
        mapping.next.store(u64::from(head), Ordering::Relaxed);
        frame
            .data_leaf_count_or_map_head
            .store(pack_shared(next_count, mapping_id), Ordering::Relaxed);
        Ok(SharedAliasTicket {
            frame_phys: req.frame_phys,
            root_phys: new_root,
            virtual_address: req.new_va,
            mapping_id,
            kind,
            converted_exclusive,
            owns_frame_lock,
            bump_owner_root: if owns_frame_lock {
                Self::bump_owner(owner_root, kind, converted_exclusive, count)
            } else {
                0
            },
        })
    }

    /// Publishes every prepared ticket in a run targeting one root, updating
    /// the root's data-leaf count once and unlocking each distinct frame only
    /// after all of its claimed mapping records are marked live.
    fn publish_shared_alias_run(&self, tickets: &[SharedAliasTicket]) {
        let Some(first) = tickets.first() else {
            return;
        };
        let root_phys = first.root_phys;
        let root = self
            .verify_root(root_phys)
            .expect("shared alias root disappeared");
        for ticket in tickets {
            assert_eq!(ticket.root_phys, root_phys, "mixed roots in shared alias run");
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
        }
        root.data_leaf_count_or_map_head
            .fetch_add(tickets.len() as u64, DESCRIPTOR_ACQ_REL);
        root.shared_backing_identity
            .fetch_add(tickets.len() as u64, DESCRIPTOR_ACQ_REL);
        for ticket in tickets {
            self.adjust_shared_leaves(ticket.bump_owner_root, 1);
            if ticket.owns_frame_lock {
                let frame = self
                    .record(ticket.frame_phys)
                    .expect("shared alias frame disappeared");
                assert_eq!(
                    frame
                        .role
                        .swap(shared_live_role(ticket.kind), DESCRIPTOR_RELEASE),
                    shared_locked_role(ticket.kind)
                );
            }
        }
    }

    /// Cancels every prepared ticket in a run targeting one root, refunding
    /// the entire run charge in one uncharge and restoring every descriptor.
    fn cancel_shared_alias_run(&self, tickets: &[SharedAliasTicket]) {
        let Some(first) = tickets.first() else {
            return;
        };
        let root_phys = first.root_phys;
        let root = self
            .verify_root(root_phys)
            .expect("missing alias root");
        self.uncharge_root(root, tickets.len() as u64);
        // Pass 1: unlink all duplicate (non-lock-owning) tickets while their
        // frame descriptors remain locked.
        for ticket in tickets {
            assert_eq!(ticket.root_phys, root_phys, "mixed roots in shared alias run");
            if !ticket.owns_frame_lock {
                self.cancel_shared_alias_duplicate_any(*ticket);
            }
        }
        // Pass 2: unlink each lock-owning ticket (now guaranteed to be head of
        // its frame's list) and restore/unlock the frame descriptor.
        for ticket in tickets {
            if ticket.owns_frame_lock {
                self.cancel_shared_alias_inner(*ticket);
            }
        }
    }

    /// Unlinks a duplicate ticket known to be at the head of its locked frame's
    /// shared list (used during reverse-order rollback inside prepare).
    fn cancel_shared_alias_duplicate_head(&self, ticket: SharedAliasTicket) {
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
        frame
            .data_leaf_count_or_map_head
            .store(pack_shared(count - 1, next), Ordering::Relaxed);
        self.release_mapping_record(ticket.mapping_id);
    }

    /// Unlinks a duplicate ticket at any position in its locked frame's shared
    /// list (used by `cancel_shared_alias_run`, whose slice is in caller order).
    fn cancel_shared_alias_duplicate_any(&self, ticket: SharedAliasTicket) {
        let frame = self
            .record(ticket.frame_phys)
            .expect("cancelled shared alias frame disappeared");
        let packed = frame.data_leaf_count_or_map_head.load(Ordering::Relaxed);
        let count = shared_count(packed);
        let head = shared_head(packed);
        assert!(count >= 2);
        let mut previous = 0_u32;
        let mut current = head;
        let mut traversed = 0_usize;
        let maximum = self.mapping_count.load(DESCRIPTOR_ACQUIRE);
        while current != ticket.mapping_id {
            traversed += 1;
            assert!(traversed <= maximum, "shared mapping list cycle");
            let record = self
                .mapping(current)
                .expect("shared mapping id escaped pool");
            previous = current;
            current = u32::try_from(record.next.load(Ordering::Relaxed))
                .expect("shared mapping next id overflow");
            assert_ne!(current, 0, "cancelled duplicate mapping was not linked");
        }
        let record = self
            .mapping(ticket.mapping_id)
            .expect("cancelled shared alias mapping disappeared");
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
        self.release_mapping_record(ticket.mapping_id);
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
        root.shared_backing_identity
            .fetch_add(1, DESCRIPTOR_ACQ_REL);
        self.adjust_shared_leaves(ticket.bump_owner_root, 1);
        assert_eq!(
            frame
                .role
                .swap(shared_live_role(ticket.kind), DESCRIPTOR_RELEASE),
            shared_locked_role(ticket.kind)
        );
    }

    fn cancel_shared_alias(&self, ticket: SharedAliasTicket) {
        self.uncharge_root(self.verify_root(ticket.root_phys).expect("missing alias root"), 1);
        self.cancel_shared_alias_inner(ticket);
    }

    fn cancel_shared_alias_inner(&self, ticket: SharedAliasTicket) {
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
            !ticket.converted_exclusive,
        );
        assert_eq!(result, DataLeafRelease::FrameRetained);
        self.decrement_root_data_count_with(root);
        if ticket.converted_exclusive {
            assert_eq!(
                shared_count(frame.data_leaf_count_or_map_head.load(Ordering::Relaxed)),
                1
            );
            if ticket.kind == CowFrameKind::PrivateFileSection {
                // File-backed leaves stay shared while mapped, so removing the
                // alias does not itself return the survivor to private.
                let survivor = frame.owner_or_table_head.load(Ordering::Relaxed);
                self.adjust_shared_leaves(survivor, -1);
            }
            frame
                .data_leaf_count_or_map_head
                .store(0, Ordering::Relaxed);
            frame.shared_backing_identity.store(0, Ordering::Relaxed);
            assert_eq!(
                frame
                    .role
                    .swap(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE),
                shared_locked_role(ticket.kind)
            );
        }
        true
    }

    /// Cache custody is the inline shared owner `(0, 0)`, a key no real root
    /// can have. It contributes to the shared count but not to a mapping quota.
    /// The supplying capability separately pays for cache residency.
    fn claim_private_file_cache(&self, frame_phys: u64, backing_identity: u64) -> bool {
        if frame_phys == 0 || !frame_phys.is_multiple_of(PAGE_SIZE) || backing_identity == 0 {
            return false;
        }
        let Some(frame) = self.record(frame_phys) else { return false; };
        if frame.role.compare_exchange(ROLE_FREE, ROLE_SHARED_PRIVATE_FILE_LOCKED, DESCRIPTOR_ACQ_REL, DESCRIPTOR_ACQUIRE).is_err() {
            return false;
        }
        frame.owner_or_table_head.store(0, Ordering::Relaxed);
        frame.next_or_virtual_address.store(0, Ordering::Relaxed);
        frame.data_leaf_count_or_map_head.store(pack_shared(1, 0), Ordering::Relaxed);
        frame.shared_backing_identity.store(backing_identity, Ordering::Relaxed);
        frame.role.store(ROLE_SHARED_PRIVATE_FILE_LIVE, DESCRIPTOR_RELEASE);
        true
    }

    /// Remove source custody after withdrawing its capability and draining
    /// in-flight installers. Mapped leaves retain the frame through TLB reclaim.
    fn release_private_file_cache(&self, frame_phys: u64, backing_identity: u64) -> Option<DataLeafRelease> {
        if backing_identity == 0 { return None; }
        let frame = self.record(frame_phys)?;
        if !self.lock_shared(frame, CowFrameKind::PrivateFileSection, false) { return None; }
        if frame.shared_backing_identity.load(Ordering::Relaxed) != backing_identity
            || frame.owner_or_table_head.load(Ordering::Relaxed) != 0
            || frame.next_or_virtual_address.load(Ordering::Relaxed) != 0 {
            frame.role.store(ROLE_SHARED_PRIVATE_FILE_LIVE, DESCRIPTOR_RELEASE);
            return None;
        }
        Some(self.remove_shared_mapping_locked(frame, 0, 0, 0, CowFrameKind::PrivateFileSection, true))
    }

    #[cfg(test)]
    fn claim_private_file_data(
        &self,
        root_phys: u64,
        va: u64,
        frame_phys: u64,
        backing_identity: u64,
    ) -> DataClaim {
        if backing_identity == 0
            || !valid_mapping(root_phys, va, frame_phys)
            || self.verify_root(root_phys).is_none()
        {
            return DataClaim::Invalid;
        }
        let Some(frame) = self.record(frame_phys) else {
            return DataClaim::Invalid;
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
            return DataClaim::Invalid;
        }
        if !self.try_charge_root(root_phys, 1) {
            frame.role.store(ROLE_FREE, DESCRIPTOR_RELEASE);
            return DataClaim::Quota;
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
        DataClaim::Claimed
    }

    #[cfg(test)]
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
        root.shared_backing_identity
            .fetch_add(1, DESCRIPTOR_ACQ_REL);
        assert_eq!(
            frame
                .role
                .swap(ROLE_SHARED_PRIVATE_FILE_LIVE, DESCRIPTOR_RELEASE),
            ROLE_SHARED_PRIVATE_FILE_LOCKED
        );
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
                ROLE_EXCLUSIVE_DATA_CLAIMED
                | ROLE_SHARED_ANONYMOUS_LOCKED
                | ROLE_SHARED_PRIVATE_FILE_LOCKED => spin_loop(),
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
            true,
        );
        let root = self.verify_root(claim.root_phys).expect("COW mapping root disappeared");
        let previous = root.data_leaf_count_or_map_head.fetch_sub(1, DESCRIPTOR_ACQ_REL);
        assert!(previous != 0, "data-leaf root count underflow");
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
                    if frame.shared_backing_identity.load(Ordering::Relaxed) != 0 {
                        frame.role.store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
                        return None;
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
                ROLE_EXCLUSIVE_DATA_CLAIMED
                | ROLE_SHARED_ANONYMOUS_LOCKED
                | ROLE_SHARED_PRIVATE_FILE_LOCKED => spin_loop(),
                _ => return None,
            }
        }
        None
    }

    /// Re-keys one live data mapping from `(root, old_va)` to `(root, new_va)`
    /// without touching its charge, frame, role or sharing set.
    ///
    /// This is the descriptor half of an atomic `mremap` relocation: the page
    /// table leaf moves and the claim that proves the leaf is exclusively owned
    /// (or one member of a COW sharing set) must name the new address in the
    /// same critical section as the PTE change, or a later unmap would find a
    /// leaf whose ledger key does not exist. Returns `false`, changing nothing,
    /// when no live mapping `(root, old_va)` of `frame_phys` exists, so a caller
    /// that preflighted ownership treats `false` as a ledger-corruption signal.
    fn rekey_data(&self, root_phys: u64, old_va: u64, new_va: u64, frame_phys: u64) -> bool {
        if !valid_mapping(root_phys, new_va, frame_phys) || self.verify_root(root_phys).is_none() {
            return false;
        }
        let Some(frame) = self.record(frame_phys) else {
            return false;
        };
        for _ in 0..SHARED_LOCK_SPINS {
            match frame.role.load(DESCRIPTOR_ACQUIRE) {
                ROLE_EXCLUSIVE_DATA_LIVE => {
                    if frame.owner_or_table_head.load(Ordering::Relaxed) != root_phys
                        || frame.next_or_virtual_address.load(Ordering::Relaxed) != old_va
                    {
                        return false;
                    }
                    // LIVE -> CLAIMED excludes `data_is_owned` readers (they
                    // answer `false` for a claimed frame) for the two stores.
                    if frame
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
                    if frame.shared_backing_identity.load(Ordering::Relaxed) != 0 {
                        frame.role.store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
                        return false;
                    }
                    frame
                        .next_or_virtual_address
                        .store(new_va, Ordering::Relaxed);
                    frame
                        .role
                        .store(ROLE_EXCLUSIVE_DATA_LIVE, DESCRIPTOR_RELEASE);
                    return true;
                }
                role @ (ROLE_SHARED_ANONYMOUS_LIVE | ROLE_SHARED_PRIVATE_FILE_LIVE) => {
                    let kind = if role == ROLE_SHARED_ANONYMOUS_LIVE {
                        CowFrameKind::AnonymousFork
                    } else {
                        CowFrameKind::PrivateFileSection
                    };
                    if !self.lock_shared(frame, kind, false) {
                        continue;
                    }
                    let updated = match self.find_shared_mapping_locked(frame, root_phys, old_va) {
                        Some(0) => {
                            frame
                                .next_or_virtual_address
                                .store(new_va, Ordering::Relaxed);
                            true
                        }
                        Some(id) => match self.mapping(id) {
                            Some(mapping) => {
                                mapping.virtual_address.store(new_va, Ordering::Relaxed);
                                true
                            }
                            None => false,
                        },
                        None => false,
                    };
                    frame.role.store(role, DESCRIPTOR_RELEASE);
                    return updated;
                }
                ROLE_EXCLUSIVE_DATA_CLAIMED
                | ROLE_SHARED_ANONYMOUS_LOCKED
                | ROLE_SHARED_PRIVATE_FILE_LOCKED => spin_loop(),
                _ => return false,
            }
        }
        false
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
        let result = self.remove_shared_mapping_locked(frame, root_phys, va, mapping_id, kind, true);
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
        unlock: bool,
    ) -> DataLeafRelease {
        let packed = frame.data_leaf_count_or_map_head.load(Ordering::Relaxed);
        let count = shared_count(packed);
        let head = shared_head(packed);
        assert!(count != 0, "shared mapping count underflow");
        // Per-root shared-leaf accounting: the removed leaf stops counting, and
        // an anonymous frame dropping to one mapping makes its survivor private.
        if kind == CowFrameKind::PrivateFileSection || count >= 2 {
            self.adjust_shared_leaves(root_phys, -1);
        }
        if kind == CowFrameKind::AnonymousFork && count == 2 {
            let survivor = if mapping_id == 0 {
                self.mapping(head)
                    .map(|m| m.root_and_state.load(DESCRIPTOR_ACQUIRE) & !MAPPING_STATE_MASK)
                    .unwrap_or(0)
            } else {
                frame.owner_or_table_head.load(Ordering::Relaxed)
            };
            self.adjust_shared_leaves(survivor, -1);
        }
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
        if unlock {
            frame.role.store(shared_live_role(kind), DESCRIPTOR_RELEASE);
        }
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
            // The tag only has to differ across one CAS window, so it wraps; a
            // panic here would let sustained fork churn kill ring0 after 2^32
            // free-list operations.
            let generation = free_head_generation(head).wrapping_add(1);
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
            // The tag only has to differ across one CAS window, so it wraps; a
            // panic here would let sustained fork churn kill ring0 after 2^32
            // free-list operations.
            let generation = free_head_generation(head).wrapping_add(1);
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

    /// The charge group a root is bound to. A root's `next_or_virtual_address`
    /// is otherwise unused (leaves use it for their virtual address), so it holds
    /// `group + 1`; zero means unbound.
    fn root_group(root: &FrameDescriptorRecord) -> Option<usize> {
        let word = root.next_or_virtual_address.load(Ordering::Relaxed);
        word.checked_sub(1).map(|group| group as usize)
    }

    fn try_charge_root(&self, root_phys: u64, pages: u64) -> bool {
        self.verify_root(root_phys).and_then(Self::root_group)
            .is_none_or(|group| charge::try_charge(group, pages))
    }

    fn uncharge_root(&self, root: &FrameDescriptorRecord, pages: u64) {
        if let Some(group) = Self::root_group(root) {
            charge::uncharge(group, pages);
        }
    }

    /// Binds a quiescent root to a charge group and charges the data leaves it
    /// already holds. The caller guarantees nothing claims, publishes or
    /// releases on this root concurrently (a new root, before its process is
    /// published or its exec committed). Refused if the root is unknown or
    /// already bound.
    fn bind_charge(&self, root_phys: u64, group: usize) -> bool {
        let Some(root) = self.verify_root(root_phys) else {
            return false;
        };
        let held = root.data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE);
        if !charge::try_charge(group, held) {
            return false;
        }
        if root
            .next_or_virtual_address
            .compare_exchange(0, group as u64 + 1, DESCRIPTOR_ACQ_REL, DESCRIPTOR_ACQUIRE)
            .is_err()
        {
            charge::uncharge(group, held);
            return false;
        }
        // The root frame and every table already published are existing state:
        // catch the group up without a limit check, as for data leaves.
        let tables = 1 + self.table_count(root_phys) as u64;
        kmem_charge::charge_forced(kmem_charge::Class::PageTable, group, tables * PAGE_SIZE);
        true
    }

    /// Whether `pages` more data leaves fit under every limit of the root's
    /// group. An unknown or unbound root is always admitted.
    fn root_admits(&self, root_phys: u64, pages: u64) -> bool {
        self.verify_root(root_phys)
            .and_then(Self::root_group)
            .is_none_or(|group| charge::would_admit(group, pages))
    }

    #[cfg(test)]
    fn decrement_root_data_count(&self, root_phys: u64) {
        let root = self
            .verify_root(root_phys)
            .expect("COW mapping root disappeared");
        self.decrement_root_data_count_with(root);
    }

    fn leaf_census(&self, root_phys: u64) -> Option<DataLeafCensus> {
        let root = self.verify_root(root_phys)?;
        // The two words move in separate atomics, so a leaf being published or
        // removed during the read skews `private` by that in-flight leaf only.
        let mapped = root.data_leaf_count_or_map_head.load(DESCRIPTOR_ACQUIRE);
        let shared = root.shared_backing_identity.load(DESCRIPTOR_ACQUIRE);
        Some(DataLeafCensus { mapped, shared })
    }

    /// Root that must start counting a leaf as shared when an alias joins its
    /// frame: the owner of a just-converted exclusive frame, or the sole
    /// mapper of an anonymous frame that was private at one mapping.
    fn bump_owner(owner_root: u64, kind: CowFrameKind, converted: bool, count_before: u32) -> u64 {
        let owner_turns_shared =
            converted || (kind == CowFrameKind::AnonymousFork && count_before == 1);
        if owner_turns_shared { owner_root } else { 0 }
    }

    /// Moves a root's shared-leaf count (kept in the root record's otherwise
    /// unused backing word). A shared leaf is a mapped data leaf whose frame is
    /// file-backed private COW or has two or more mappers, so killing the root
    /// does not free it. Roots that do not exist (cache custody, 0) are ignored.
    fn adjust_shared_leaves(&self, root_phys: u64, delta: i64) {
        if root_phys == 0 {
            return;
        }
        let Some(root) = self.verify_root(root_phys) else {
            return;
        };
        if delta >= 0 {
            root.shared_backing_identity
                .fetch_add(delta as u64, DESCRIPTOR_ACQ_REL);
        } else {
            let sub = delta.unsigned_abs();
            let result = root.shared_backing_identity.fetch_update(
                DESCRIPTOR_ACQ_REL,
                DESCRIPTOR_ACQUIRE,
                |value| Some(value.saturating_sub(sub)),
            );
            debug_assert!(matches!(result, Ok(v) if v >= sub), "shared-leaf count underflow");
        }
    }

    fn decrement_root_data_count_with(&self, root: &FrameDescriptorRecord) {
        let previous = root
            .data_leaf_count_or_map_head
            .fetch_sub(1, DESCRIPTOR_ACQ_REL);
        assert!(previous != 0, "data-leaf root count underflow");
        self.uncharge_root(root, 1);
    }
}

/// Outcome of reserving a data-frame descriptor and its resident quota.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataClaim {
    Claimed,
    Invalid,
    Quota,
}

/// Outcome of claiming a frame as a lazily published page table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableClaim {
    Claimed,
    /// The root or frame is not claimable (unknown root, frame not free).
    Invalid,
    /// The root's Job refused the page-table frame under its kmem limit.
    Quota,
}

pub static FRAME_DESCRIPTORS: FrameDescriptorLedger = FrameDescriptorLedger::empty();

#[cfg(test)]
mod tests;
