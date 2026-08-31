//! Bounded file-page policy owned by pagerd.
//!
//! - **Owner:** `pagerd` owns cache/COW/reclaim policy; vfsd owns file
//!   descriptions, storaged owns durable mutation, and ring0 owns frames/PTEs.
//! - **Boundary:** Backing identities, generations, load/writeback tokens,
//!   frame capabilities, and TLB acknowledgements cross trust boundaries.
//! - **Lifecycle:** One miss owner loads, publishes clean, maps/COWs or dirties,
//!   writes back, revokes mappings, waits for exact TLB ACK, then releases.
//! - **Concurrency:** The fixed cache has one linear owner today; no entry lock
//!   may be held across vfsd/storaged IPC or kernel mapping work.
//! - **Failure:** Capacity, stale token/generation, dirty/mapped reclaim, and
//!   provider restart fail closed without publishing or releasing a frame.
//! - **Forbidden:** No path/frame-number key, duplicate load owner, writable
//!   private alias of the shared source, dirty discard, or pre-ACK reclaim.
//! - **Evidence:** `page-cache-lifecycle/PageCacheLifecycle`, focused source
//!   tests, and the `page-cache-*` implementation mutations.

use rustos_user_abi::pager::{
    PagerFaultRequestWire, PAGER_PAGE_BYTES, VM_OBJECT_FILE_PRIVATE, VM_OBJECT_FILE_SHARED,
    VM_OBJECT_IMAGE_SECTION,
};

pub const PAGER_MAX_CACHE_ENTRIES: usize = 64;
pub const PAGER_MAX_REVOKED_BACKINGS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageCacheKey {
    pub backing_service: u64,
    pub object_slot: u64,
    pub object_generation: u64,
    pub backing_generation: u64,
    pub page_offset: u64,
}

impl PageCacheKey {
    pub fn from_fault(request: PagerFaultRequestWire) -> Result<Self, CacheError> {
        if !request.is_canonical()
            || !matches!(
                request.object.object_type,
                VM_OBJECT_FILE_PRIVATE | VM_OBJECT_FILE_SHARED | VM_OBJECT_IMAGE_SECTION
            )
            || request.object.backing_service == 0
        {
            return Err(CacheError::Malformed);
        }
        let page_offset = request
            .object_offset
            .checked_add(request.virtual_address & (PAGER_PAGE_BYTES - 1))
            .ok_or(CacheError::Malformed)?;
        if page_offset & (PAGER_PAGE_BYTES - 1) != 0 {
            return Err(CacheError::Malformed);
        }
        Ok(Self {
            backing_service: request.object.backing_service,
            object_slot: request.object.slot,
            object_generation: request.object.generation,
            backing_generation: request.object.backing_generation,
            page_offset,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CachePage {
    pub frame_capability: u64,
    pub cache_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheAdmission {
    LoadOwner { load_token: u64 },
    Coalesced { load_token: u64 },
    Hit(CachePage),
    RetryAfter { generation: u64 },
    Revoked { revoke_token: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheError {
    Malformed,
    Stale,
    Pressure,
    Busy,
    Dirty,
    Mapped,
    GenerationExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CacheState {
    Loading {
        load_token: u64,
    },
    Clean {
        page: CachePage,
        mappings: u32,
    },
    Dirty {
        page: CachePage,
        mappings: u32,
    },
    Writeback {
        page: CachePage,
        mappings: u32,
        writeback_token: u64,
    },
    Reclaiming {
        page: CachePage,
        tlb_generation: u64,
    },
    Revoked {
        page: CachePage,
        mappings: u32,
        dirty: bool,
        revoke_token: u64,
        tlb_generation: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RevokedBacking {
    backing_service: u64,
    backing_generation: u64,
    revoke_token: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CacheEntry {
    key: PageCacheKey,
    state: CacheState,
}

pub struct PageCache {
    entries: [Option<CacheEntry>; PAGER_MAX_CACHE_ENTRIES],
    revoked_backings: [Option<RevokedBacking>; PAGER_MAX_REVOKED_BACKINGS],
    next_generation: u64,
}

impl PageCache {
    pub const fn new() -> Self {
        Self {
            entries: [None; PAGER_MAX_CACHE_ENTRIES],
            revoked_backings: [None; PAGER_MAX_REVOKED_BACKINGS],
            next_generation: 1,
        }
    }

    pub fn admit_load(
        &mut self,
        key: PageCacheKey,
        load_token: u64,
    ) -> Result<CacheAdmission, CacheError> {
        if load_token == 0 {
            return Err(CacheError::Malformed);
        }
        if let Some(revoked) = self.revoked_backing(key) {
            return Ok(CacheAdmission::Revoked {
                revoke_token: revoked.revoke_token,
            });
        }
        if let Some(entry) = self.entry(key) {
            return Ok(match entry.state {
                CacheState::Loading { load_token } => CacheAdmission::Coalesced { load_token },
                CacheState::Clean { page, .. }
                | CacheState::Dirty { page, .. }
                | CacheState::Writeback { page, .. } => CacheAdmission::Hit(page),
                CacheState::Reclaiming { page, .. } => CacheAdmission::RetryAfter {
                    generation: page.cache_generation,
                },
                CacheState::Revoked { revoke_token, .. } => {
                    CacheAdmission::Revoked { revoke_token }
                }
            });
        }
        let slot = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_none())
            .ok_or(CacheError::Pressure)?;
        *slot = Some(CacheEntry {
            key,
            state: CacheState::Loading { load_token },
        });
        Ok(CacheAdmission::LoadOwner { load_token })
    }

    pub fn publish_clean(
        &mut self,
        key: PageCacheKey,
        load_token: u64,
        frame_capability: u64,
    ) -> Result<CachePage, CacheError> {
        if frame_capability == 0 {
            return Err(CacheError::Malformed);
        }
        let generation = self.allocate_generation()?;
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        if entry.state != (CacheState::Loading { load_token }) {
            return Err(CacheError::Stale);
        }
        let page = CachePage {
            frame_capability,
            cache_generation: generation,
        };
        entry.state = CacheState::Clean { page, mappings: 0 };
        Ok(page)
    }

    pub fn acquire_mapping(&mut self, key: PageCacheKey) -> Result<CachePage, CacheError> {
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        match &mut entry.state {
            CacheState::Clean { page, mappings }
            | CacheState::Dirty { page, mappings }
            | CacheState::Writeback { page, mappings, .. } => {
                *mappings = mappings.checked_add(1).ok_or(CacheError::Pressure)?;
                Ok(*page)
            }
            CacheState::Loading { .. }
            | CacheState::Reclaiming { .. }
            | CacheState::Revoked { .. } => Err(CacheError::Busy),
        }
    }

    pub fn release_mapping(
        &mut self,
        key: PageCacheKey,
        cache_generation: u64,
    ) -> Result<(), CacheError> {
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        match &mut entry.state {
            CacheState::Clean { page, mappings }
            | CacheState::Dirty { page, mappings }
            | CacheState::Writeback { page, mappings, .. }
            | CacheState::Revoked { page, mappings, .. }
                if page.cache_generation == cache_generation && *mappings != 0 =>
            {
                *mappings -= 1;
                Ok(())
            }
            _ => Err(CacheError::Stale),
        }
    }

    pub fn private_cow(
        &self,
        key: PageCacheKey,
        source_generation: u64,
        private_frame_capability: u64,
    ) -> Result<CachePage, CacheError> {
        if private_frame_capability == 0 {
            return Err(CacheError::Malformed);
        }
        let entry = self.entry(key).ok_or(CacheError::Stale)?;
        match entry.state {
            CacheState::Clean { page, .. } if page.cache_generation == source_generation => {
                Ok(CachePage {
                    frame_capability: private_frame_capability,
                    cache_generation: source_generation,
                })
            }
            _ => Err(CacheError::Stale),
        }
    }

    pub fn mark_dirty(
        &mut self,
        key: PageCacheKey,
        cache_generation: u64,
    ) -> Result<(), CacheError> {
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        match entry.state {
            CacheState::Clean { page, mappings } if page.cache_generation == cache_generation => {
                entry.state = CacheState::Dirty { page, mappings };
                Ok(())
            }
            CacheState::Dirty { page, .. } if page.cache_generation == cache_generation => Ok(()),
            _ => Err(CacheError::Stale),
        }
    }

    pub fn begin_writeback(
        &mut self,
        key: PageCacheKey,
        cache_generation: u64,
        writeback_token: u64,
    ) -> Result<(), CacheError> {
        if writeback_token == 0 {
            return Err(CacheError::Malformed);
        }
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        match entry.state {
            CacheState::Dirty { page, mappings } if page.cache_generation == cache_generation => {
                entry.state = CacheState::Writeback {
                    page,
                    mappings,
                    writeback_token,
                };
                Ok(())
            }
            _ => Err(CacheError::Stale),
        }
    }

    pub fn complete_writeback(
        &mut self,
        key: PageCacheKey,
        writeback_token: u64,
    ) -> Result<(), CacheError> {
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        match entry.state {
            CacheState::Writeback {
                page,
                mappings,
                writeback_token: expected,
            } if expected == writeback_token => {
                entry.state = CacheState::Clean { page, mappings };
                Ok(())
            }
            _ => Err(CacheError::Stale),
        }
    }

    pub fn begin_reclaim(
        &mut self,
        key: PageCacheKey,
        cache_generation: u64,
        tlb_generation: u64,
    ) -> Result<CachePage, CacheError> {
        if tlb_generation == 0 {
            return Err(CacheError::Malformed);
        }
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        match entry.state {
            CacheState::Clean { page, mappings: 0 }
                if page.cache_generation == cache_generation =>
            {
                entry.state = CacheState::Reclaiming {
                    page,
                    tlb_generation,
                };
                Ok(page)
            }
            CacheState::Clean { .. } => Err(CacheError::Mapped),
            CacheState::Dirty { .. } | CacheState::Writeback { .. } => Err(CacheError::Dirty),
            CacheState::Loading { .. }
            | CacheState::Reclaiming { .. }
            | CacheState::Revoked { .. } => Err(CacheError::Busy),
        }
    }

    pub fn complete_reclaim(
        &mut self,
        key: PageCacheKey,
        tlb_generation: u64,
    ) -> Result<CachePage, CacheError> {
        let slot = self
            .entries
            .iter_mut()
            .find(|slot| slot.is_some_and(|entry| entry.key == key))
            .ok_or(CacheError::Stale)?;
        match slot.expect("matched cache slot").state {
            CacheState::Reclaiming {
                page,
                tlb_generation: expected,
            } if expected == tlb_generation => {
                *slot = None;
                Ok(page)
            }
            _ => Err(CacheError::Stale),
        }
    }

    pub fn revoke_backing(
        &mut self,
        backing_service: u64,
        backing_generation: u64,
        revoke_token: u64,
    ) -> Result<usize, CacheError> {
        if backing_service == 0 || backing_generation == 0 || revoke_token == 0 {
            return Err(CacheError::Malformed);
        }
        self.record_revoked_backing(backing_service, backing_generation, revoke_token)?;
        let mut revoked = 0;
        for slot in &mut self.entries {
            let Some(entry) = *slot else {
                continue;
            };
            if entry.key.backing_service == backing_service
                && entry.key.backing_generation == backing_generation
            {
                let state = match entry.state {
                    CacheState::Loading { .. } => {
                        *slot = None;
                        revoked += 1;
                        continue;
                    }
                    CacheState::Clean { page, mappings } => CacheState::Revoked {
                        page,
                        mappings,
                        dirty: false,
                        revoke_token,
                        tlb_generation: 0,
                    },
                    CacheState::Dirty { page, mappings }
                    | CacheState::Writeback { page, mappings, .. } => CacheState::Revoked {
                        page,
                        mappings,
                        dirty: true,
                        revoke_token,
                        tlb_generation: 0,
                    },
                    CacheState::Reclaiming {
                        page,
                        tlb_generation,
                    } => CacheState::Revoked {
                        page,
                        mappings: 0,
                        dirty: false,
                        revoke_token,
                        tlb_generation,
                    },
                    CacheState::Revoked { .. } => continue,
                };
                *slot = Some(CacheEntry {
                    key: entry.key,
                    state,
                });
                revoked += 1;
            }
        }
        Ok(revoked)
    }

    pub fn begin_revoked_reclaim(
        &mut self,
        key: PageCacheKey,
        revoke_token: u64,
        tlb_generation: u64,
    ) -> Result<CachePage, CacheError> {
        if revoke_token == 0 || tlb_generation == 0 {
            return Err(CacheError::Malformed);
        }
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        match &mut entry.state {
            CacheState::Revoked {
                page,
                mappings: 0,
                dirty: false,
                revoke_token: expected,
                tlb_generation: expected_tlb,
            } if *expected == revoke_token
                && (*expected_tlb == 0 || *expected_tlb == tlb_generation) =>
            {
                *expected_tlb = tlb_generation;
                Ok(*page)
            }
            CacheState::Revoked { dirty: true, .. } => Err(CacheError::Dirty),
            CacheState::Revoked { mappings, .. } if *mappings != 0 => Err(CacheError::Mapped),
            _ => Err(CacheError::Stale),
        }
    }

    pub fn complete_revoked_reclaim(
        &mut self,
        key: PageCacheKey,
        revoke_token: u64,
        tlb_generation: u64,
    ) -> Result<CachePage, CacheError> {
        let slot = self
            .entries
            .iter_mut()
            .find(|slot| slot.is_some_and(|entry| entry.key == key))
            .ok_or(CacheError::Stale)?;
        match slot.expect("matched cache slot").state {
            CacheState::Revoked {
                page,
                mappings: 0,
                dirty: false,
                revoke_token: expected_revoke,
                tlb_generation: expected_tlb,
            } if expected_revoke == revoke_token
                && expected_tlb != 0
                && expected_tlb == tlb_generation =>
            {
                *slot = None;
                Ok(page)
            }
            _ => Err(CacheError::Stale),
        }
    }

    pub fn reauthorize_revoked_dirty(
        &mut self,
        key: PageCacheKey,
        revoke_token: u64,
        replacement: PageCacheKey,
    ) -> Result<CachePage, CacheError> {
        if replacement.backing_service != key.backing_service
            || replacement.object_slot != key.object_slot
            || replacement.object_generation != key.object_generation
            || replacement.page_offset != key.page_offset
            || replacement.backing_generation <= key.backing_generation
            || self.revoked_backing(replacement).is_some()
            || self.entry(replacement).is_some()
        {
            return Err(CacheError::Malformed);
        }
        let entry = self.entry_mut(key).ok_or(CacheError::Stale)?;
        match entry.state {
            CacheState::Revoked {
                page,
                mappings,
                dirty: true,
                revoke_token: expected,
                tlb_generation: 0,
            } if expected == revoke_token => {
                entry.key = replacement;
                entry.state = CacheState::Dirty { page, mappings };
                Ok(page)
            }
            _ => Err(CacheError::Stale),
        }
    }

    fn revoked_backing(&self, key: PageCacheKey) -> Option<RevokedBacking> {
        self.revoked_backings
            .iter()
            .flatten()
            .copied()
            .find(|entry| {
                entry.backing_service == key.backing_service
                    && entry.backing_generation == key.backing_generation
            })
    }

    fn record_revoked_backing(
        &mut self,
        backing_service: u64,
        backing_generation: u64,
        revoke_token: u64,
    ) -> Result<(), CacheError> {
        if let Some(existing) = self.revoked_backings.iter().flatten().find(|entry| {
            entry.backing_service == backing_service
                && entry.backing_generation == backing_generation
        }) {
            return (existing.revoke_token == revoke_token)
                .then_some(())
                .ok_or(CacheError::Stale);
        }
        let slot = self
            .revoked_backings
            .iter_mut()
            .find(|entry| entry.is_none())
            .ok_or(CacheError::Pressure)?;
        *slot = Some(RevokedBacking {
            backing_service,
            backing_generation,
            revoke_token,
        });
        Ok(())
    }

    fn entry(&self, key: PageCacheKey) -> Option<&CacheEntry> {
        self.entries.iter().flatten().find(|entry| entry.key == key)
    }

    fn entry_mut(&mut self, key: PageCacheKey) -> Option<&mut CacheEntry> {
        self.entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.key == key)
    }

    fn allocate_generation(&mut self) -> Result<u64, CacheError> {
        let generation = self.next_generation;
        self.next_generation = generation
            .checked_add(1)
            .filter(|generation| *generation != 0)
            .ok_or(CacheError::GenerationExhausted)?;
        Ok(generation)
    }
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(backing_generation: u64) -> PageCacheKey {
        PageCacheKey {
            backing_service: 12,
            object_slot: 3,
            object_generation: 5,
            backing_generation,
            page_offset: 0x4000,
        }
    }

    fn clean(cache: &mut PageCache, key: PageCacheKey) -> CachePage {
        assert_eq!(
            cache.admit_load(key, 7),
            Ok(CacheAdmission::LoadOwner { load_token: 7 })
        );
        cache.publish_clean(key, 7, 11).unwrap()
    }

    #[test]
    fn concurrent_miss_has_one_load_owner_and_exact_backing_generation() {
        let mut cache = PageCache::new();
        let first = key(13);
        assert_eq!(
            cache.admit_load(first, 17),
            Ok(CacheAdmission::LoadOwner { load_token: 17 })
        );
        assert_eq!(
            cache.admit_load(first, 19),
            Ok(CacheAdmission::Coalesced { load_token: 17 })
        );
        assert_eq!(
            cache.admit_load(key(14), 23),
            Ok(CacheAdmission::LoadOwner { load_token: 23 })
        );
    }

    #[test]
    fn private_cow_never_changes_the_shared_clean_source() {
        let mut cache = PageCache::new();
        let key = key(29);
        let source = clean(&mut cache, key);
        let private = cache.private_cow(key, source.cache_generation, 31).unwrap();
        assert_eq!(private.frame_capability, 31);
        assert_eq!(cache.acquire_mapping(key), Ok(source));
        assert_eq!(cache.release_mapping(key, source.cache_generation), Ok(()));
        assert_eq!(
            cache.begin_writeback(key, source.cache_generation, 37),
            Err(CacheError::Stale),
            "private COW must not dirty the shared source"
        );
    }

    #[test]
    fn dirty_page_requires_exact_writeback_before_reclaim() {
        let mut cache = PageCache::new();
        let key = key(41);
        let page = clean(&mut cache, key);
        cache.mark_dirty(key, page.cache_generation).unwrap();
        assert_eq!(
            cache.begin_reclaim(key, page.cache_generation, 43),
            Err(CacheError::Dirty)
        );
        cache
            .begin_writeback(key, page.cache_generation, 47)
            .unwrap();
        assert_eq!(cache.complete_writeback(key, 48), Err(CacheError::Stale));
        cache.complete_writeback(key, 47).unwrap();
        assert_eq!(
            cache.begin_reclaim(key, page.cache_generation, 53),
            Ok(page)
        );
    }

    #[test]
    fn reclaim_releases_frame_only_after_exact_tlb_ack_generation() {
        let mut cache = PageCache::new();
        let key = key(59);
        let page = clean(&mut cache, key);
        cache.begin_reclaim(key, page.cache_generation, 61).unwrap();
        assert_eq!(cache.complete_reclaim(key, 62), Err(CacheError::Stale));
        assert_eq!(cache.complete_reclaim(key, 61), Ok(page));
        assert_eq!(
            cache.admit_load(key, 67),
            Ok(CacheAdmission::LoadOwner { load_token: 67 })
        );
    }

    #[test]
    fn provider_restart_revokes_only_the_exact_old_backing_generation() {
        let mut cache = PageCache::new();
        let old = clean(&mut cache, key(71));
        clean(&mut cache, key(72));
        assert_eq!(cache.revoke_backing(12, 71, 73), Ok(1));
        assert_eq!(
            cache.admit_load(key(71), 73),
            Ok(CacheAdmission::Revoked { revoke_token: 73 })
        );
        assert!(matches!(
            cache.admit_load(key(72), 79),
            Ok(CacheAdmission::Hit(_))
        ));
        assert_eq!(cache.begin_revoked_reclaim(key(71), 73, 83), Ok(old));
        assert_eq!(
            cache.complete_revoked_reclaim(key(71), 73, 84),
            Err(CacheError::Stale)
        );
        assert_eq!(cache.complete_revoked_reclaim(key(71), 73, 83), Ok(old));
        assert_eq!(
            cache.admit_load(key(71), 89),
            Ok(CacheAdmission::Revoked { revoke_token: 73 })
        );
    }

    #[test]
    fn provider_revoke_quarantines_mapped_frame_until_release_and_exact_tlb_ack() {
        let mut cache = PageCache::new();
        let key = key(91);
        let page = clean(&mut cache, key);
        assert_eq!(cache.acquire_mapping(key), Ok(page));
        assert_eq!(cache.revoke_backing(12, 91, 97), Ok(1));
        assert_eq!(
            cache.begin_revoked_reclaim(key, 97, 101),
            Err(CacheError::Mapped)
        );
        cache.release_mapping(key, page.cache_generation).unwrap();
        assert_eq!(cache.begin_revoked_reclaim(key, 97, 101), Ok(page));
        assert_eq!(cache.complete_revoked_reclaim(key, 97, 101), Ok(page));
    }

    #[test]
    fn dirty_revoked_page_requires_exact_new_backing_reauthorization() {
        let mut cache = PageCache::new();
        let old_key = key(103);
        let page = clean(&mut cache, old_key);
        cache.mark_dirty(old_key, page.cache_generation).unwrap();
        assert_eq!(cache.revoke_backing(12, 103, 107), Ok(1));
        assert_eq!(
            cache.begin_revoked_reclaim(old_key, 107, 109),
            Err(CacheError::Dirty)
        );
        let replacement = key(104);
        assert_eq!(
            cache.reauthorize_revoked_dirty(old_key, 108, replacement),
            Err(CacheError::Stale)
        );
        assert_eq!(
            cache.reauthorize_revoked_dirty(old_key, 107, replacement),
            Ok(page)
        );
        cache
            .begin_writeback(replacement, page.cache_generation, 113)
            .unwrap();
    }

    #[test]
    fn revoked_loading_owner_cannot_republish_or_reload_stale_generation() {
        let mut cache = PageCache::new();
        let key = key(127);
        assert_eq!(
            cache.admit_load(key, 131),
            Ok(CacheAdmission::LoadOwner { load_token: 131 })
        );
        assert_eq!(cache.revoke_backing(12, 127, 137), Ok(1));
        assert_eq!(cache.publish_clean(key, 131, 139), Err(CacheError::Stale));
        assert_eq!(
            cache.admit_load(key, 149),
            Ok(CacheAdmission::Revoked { revoke_token: 137 })
        );
    }
}
