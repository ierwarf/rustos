//! Format-neutral private aliases of immutable, externally owned frames.
//! Backing custody belongs to the execution root, not a bootstrap/process box.
//! Callers prove immutability and serialize all mapping mutations. Write
//! upgrades copy before publication; no filesystem or ELF/PE policy lives here.
use super::*;
use alloc::sync::Arc;

/// Trusted immutable backing identity and the full resident cost retained by
/// one root lease, including pages not currently mapped by that root.
#[derive(Clone)]
pub struct ImmutableBacking {
    identity_type: core::any::TypeId,
    identity: u64,
    pub(super) pages: u64,
    _hold: Arc<dyn Send + Sync>,
}
impl ImmutableBacking {
    /// `identity` must uniquely identify the retained object within `T`, and
    /// `pages` must cover every resident page pinned by `hold` for its life.
    pub fn new<T: Send + Sync + 'static>(identity: u64, pages: u64, hold: Arc<T>) -> Self {
        Self { identity_type: core::any::TypeId::of::<T>(), identity, pages, _hold: hold }
    }
    pub(super) fn same_identity(&self, other: &Self) -> bool {
        self.identity_type == other.identity_type && self.identity == other.identity
    }
}

#[derive(Clone)]
pub(super) struct ImmutableMapping {
    pub(super) start: u64,
    pub(super) end: u64,
    backing: ImmutableBacking,
}
impl core::fmt::Debug for ImmutableMapping {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImmutableMapping")
            .field("start", &self.start)
            .field("end", &self.end)
            .finish()
    }
}

fn without_range(
    mappings: &[ImmutableMapping],
    start: u64,
    end: u64,
) -> Result<Vec<ImmutableMapping>, AddressSpaceError> {
    let mut result = Vec::new();
    let intersects = mappings.iter().any(|m| start < m.end && end > m.start);
    result
        .try_reserve_exact(mappings.len().saturating_add(usize::from(intersects)))
        .map_err(|_| AddressSpaceError::OutOfFrames)?;
    for mapping in mappings {
        if end <= mapping.start || start >= mapping.end {
            result.push(mapping.clone());
        } else {
            if mapping.start < start {
                result.push(ImmutableMapping {
                    end: start,
                    ..mapping.clone()
                });
            }
            if mapping.end > end {
                result.push(ImmutableMapping {
                    start: end,
                    ..mapping.clone()
                });
            }
        }
    }
    Ok(result)
}

impl ProcessAddressSpace {
    /// Host-only metadata fixture; it must never be used for page-table access.
    #[cfg(all(debug_assertions, not(rustos_boot_image)))]
    pub fn add_immutable_metadata_for_tests(&mut self) {
        assert_eq!(self.pml4_frame_phys, 0);
        let backing = ImmutableBacking::new(1, 1, Arc::new(()));
        self.execution.as_mut().expect("test MM owner").retain_backing(backing.clone()).unwrap();
        self.immutable.push(ImmutableMapping {
            start: USER_SPACE_BASE,
            end: USER_SPACE_BASE + PAGE_4KIB_U64,
            backing,
        });
    }

    /// `backing` must retain every frame and guarantee immutable contents for
    /// its entire lifetime. Only trusted kernel backing objects call this API;
    /// user physical addresses are never authority. Private writes are copied.
    /// New immutable aliases are prepared only on an unpublished, unbound root;
    /// this ensures their resident and metadata costs are admitted at binding.
    pub fn map_immutable_user_pages_at(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        flags: PageTableFlags,
        backing: ImmutableBacking,
    ) -> Result<UserRegion, AddressSpaceError> {
        if flags.contains(PageTableFlags::WRITABLE) {
            return Err(AddressSpaceError::ProtectionViolation);
        }
        if backing.identity == 0 || backing.pages == 0 {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        if self.execution.as_ref().expect("live MM owner").backings_bound() {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        validate_user_page_range(start, frames.len())?;
        self.immutable
            .try_reserve(1)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        // Retain BEFORE any external PTE can exist, including failed-map paths.
        // Execution pins and ordinary Drop both retain this until cold reclaim.
        let added = self.execution
            .as_mut()
            .expect("live MM owner")
            .retain_backing(backing.clone())?;
        let region = match self.map_existing_user_pages_at(start, frames, flags) {
            Ok(region) => region,
            Err(error) => {
                if added {
                    self.execution.as_mut().expect("live MM owner").cancel_backing(backing);
                }
                return Err(error);
            }
        };
        self.immutable.push(ImmutableMapping {
            start: start.as_u64(),
            end: region.end().as_u64(),
            backing,
        });
        Ok(region)
    }

    pub fn immutable_mapping_metadata_bytes(&self) -> usize {
        // Root custody conservatively retains one Arc per admitted segment
        // until retirement, even after the live range is unmapped/privatized.
        self.immutable.capacity() * core::mem::size_of::<ImmutableMapping>()
            + self
                .execution
                .as_ref()
                .map_or(0, |owner| owner.backing_metadata_bytes())
    }

    /// Worst-case live-record growth for a single partial protection/unmap.
    /// Root custody does not grow on edits; a split adds at most one record.
    pub fn immutable_edit_headroom_bytes(&self, start: u64, end: u64) -> usize {
        if !self
            .immutable
            .iter()
            .any(|m| start < m.end && end > m.start)
        {
            return 0;
        }
        self.immutable
            .len()
            .saturating_add(1)
            .saturating_sub(self.immutable.capacity())
            * core::mem::size_of::<ImmutableMapping>()
    }

    #[cfg(test)]
    pub(super) fn immutable_contains(&self, addr: u64) -> bool {
        self.immutable
            .iter()
            .any(|m| m.start <= addr && addr < m.end)
    }

    pub(super) fn immutable_unmap_plan(
        &self,
        start: u64,
        end: u64,
        external: &[(u64, u64)],
    ) -> Result<(Vec<(u64, u64)>, Vec<ImmutableMapping>), AddressSpaceError> {
        let mut segments = Vec::new();
        segments
            .try_reserve_exact(external.len().saturating_add(self.immutable.len()))
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        segments.extend_from_slice(external);
        segments.extend(
            self.immutable
                .iter()
                .filter(|m| start < m.end && end > m.start)
                .map(|m| (m.start, m.end)),
        );
        Ok((segments, without_range(&self.immutable, start, end)?))
    }

    pub(super) fn clone_immutable_mappings_into(
        &self,
        child: &mut Self,
    ) -> Result<(), AddressSpaceError> {
        for mapping in &self.immutable {
            let mut addr = mapping.start;
            while addr < mapping.end {
                let (_, flags) = self
                    .translate_user_with_flags(VirtAddr::new(addr))
                    .ok_or(AddressSpaceError::NotMapped)?;
                let flags = flags.difference(PageTableFlags::ACCESSED | PageTableFlags::DIRTY);
                let run_start = addr;
                let mut frames = Vec::new();
                while addr < mapping.end {
                    let (phys, next_flags) = self
                        .translate_user_with_flags(VirtAddr::new(addr))
                        .ok_or(AddressSpaceError::NotMapped)?;
                    if next_flags.difference(PageTableFlags::ACCESSED | PageTableFlags::DIRTY)
                        != flags
                    {
                        break;
                    }
                    frames
                        .try_reserve(1)
                        .map_err(|_| AddressSpaceError::OutOfFrames)?;
                    frames.push(phys.as_u64());
                    addr += PAGE_4KIB_U64;
                }
                child.map_immutable_user_pages_at(
                    VirtAddr::new(run_start),
                    &frames,
                    flags,
                    mapping.backing.clone(),
                )?;
            }
        }
        Ok(())
    }

    /// Stage ALL private copies/descriptors before rewriting any PTE. Source
    /// leases remain root-owned even after the replacement shootdown completes.
    pub(super) fn privatize_immutable_pages(
        &mut self,
        start: VirtAddr,
        pages: usize,
    ) -> Result<(), AddressSpaceError> {
        let end = page_addr(start, pages)?.as_u64();
        if !self
            .immutable
            .iter()
            .any(|m| start.as_u64() < m.end && end > m.start)
        {
            return Ok(());
        }
        let remaining = without_range(&self.immutable, start.as_u64(), end)?;
        let count: usize = self
            .immutable
            .iter()
            .map(|m| {
                (m.end.min(end).saturating_sub(m.start.max(start.as_u64())) / PAGE_4KIB_U64)
                    as usize
            })
            .sum();
        // The whole-span check avoids unnecessary staging. Each claim below
        // also reserves quota atomically; failed staging refunds all claims.
        if !phys::root_admits(self.pml4_frame_phys, count as u64) {
            return Err(AddressSpaceError::OutOfFrames);
        }
        let mut staged = StagedCopies {
            root: self.pml4_frame_phys,
            frames: Vec::new(),
        };
        staged
            .frames
            .try_reserve_exact(count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        for mapping in &self.immutable {
            let mut addr = mapping.start.max(start.as_u64());
            let limit = mapping.end.min(end);
            while addr < limit {
                let virt = VirtAddr::new(addr);
                let (source, flags) = self
                    .translate_user_with_flags(virt)
                    .ok_or(AddressSpaceError::NotMapped)?;
                if flags.intersects(ROOT_OWNED_USER_LEAF | COW_USER_LEAF)
                    || phys::data_leaf_is_owned(self.pml4_frame_phys, addr, source.as_u64())
                {
                    return Err(AddressSpaceError::InvalidFrameOwnership);
                }
                let frame = phys::alloc_frame_user().ok_or(AddressSpaceError::OutOfFrames)?;
                if let Err(error) = data_claim_result(phys::claim_data_leaf(self.pml4_frame_phys, addr, frame.as_u64())) {
                    phys::free_frame(frame);
                    return Err(error);
                }
                // SAFETY: source is pinned immutable backing; frame is a fresh
                // exclusively owned page and both direct-map spans are 4 KiB.
                unsafe {
                    ptr::copy_nonoverlapping(
                        higher_half_ptr(source),
                        higher_half_ptr(frame),
                        PAGE_4KIB,
                    );
                }
                staged.frames.push((virt, frame, flags));
                addr += PAGE_4KIB_U64;
            }
        }
        let mutation = begin_address_space_mutation(self.root_phys());
        for &(virt, frame, flags) in &staged.frames {
            // Serialized full-span preflight proved these tables and leaves;
            // no allocation or fallible authority lookup remains at commit.
            pager_fault_mapping::with_prepared_user_leaf_mut(self.root_phys(), virt, |entry| {
                entry.set_addr(frame, flags | ROOT_OWNED_USER_LEAF);
                Ok(())
            })
            .expect("immutable replacement lost preflighted leaf");
            phys::publish_data_leaf(self.pml4_frame_phys, virt.as_u64(), frame.as_u64());
        }
        staged.frames.clear(); // custody now belongs to the exact root ledger
        self.immutable = remaining;
        drop(mutation.flush_for_reclaim());
        Ok(())
    }
}

struct StagedCopies {
    root: u64,
    frames: Vec<(VirtAddr, PhysAddr, PageTableFlags)>,
}
impl Drop for StagedCopies {
    fn drop(&mut self) {
        for &(virt, frame, _) in &self.frames {
            phys::cancel_data_leaf(self.root, virt.as_u64(), frame.as_u64());
            phys::free_frame(frame);
        }
    }
}

/// Exercise the actual PTE/descriptor/TLB paths in the existing boot smoke,
/// not just host-side range planners. The source is never mutated after share.
pub(super) fn smoke_test() {
    let start = VirtAddr::new(USER_SPACE_BASE);
    let mut source = ProcessAddressSpace::new().expect("immutable source root");
    source
        .map_zeroed_user_pages_at(start, 3, PageTableFlags::NO_EXECUTE)
        .expect("source pages");
    source
        .initialize_user_bytes(start, b"immutable")
        .expect("source bytes");
    let frames = (0..3)
        .map(|i| {
            source
                .translate_user(page_addr(start, i).unwrap())
                .unwrap()
                .as_u64()
        })
        .collect::<Vec<_>>();
    let backing = ImmutableBacking::new(1, 3, Arc::new(source));
    let mut parent = ProcessAddressSpace::new().expect("immutable parent root");
    parent
        .map_immutable_user_pages_at(start, &frames, PageTableFlags::NO_EXECUTE, backing.clone())
        .expect("immutable mapping");
    assert!(parent.initialize_user_bytes(start, b"bad").is_err());
    // A budget smaller than the complete copy set must refuse without
    // changing any old PTE, byte, descriptor or charge.
    const GROUP: usize = crate::memory::charge::CHARGE_GROUPS - 2;
    crate::memory::charge::init_group(GROUP, None);
    crate::memory::kmem_charge::init_group(GROUP, None);
    assert!(parent.bind_job_charge(GROUP));
    crate::memory::charge::set_limit(GROUP, 4);
    assert_eq!(
        parent.protect_user_pages_at(
            start,
            3,
            PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE
        ),
        Err(AddressSpaceError::OutOfFrames)
    );
    assert_eq!(crate::memory::charge::usage(GROUP), 3);
    for (index, frame) in frames.iter().enumerate() {
        let (actual, flags) = parent
            .translate_user_with_flags(page_addr(start, index).unwrap())
            .unwrap();
        assert_eq!(actual.as_u64(), *frame);
        assert!(!flags.contains(PageTableFlags::WRITABLE));
    }
    crate::memory::charge::set_limit(GROUP, crate::memory::charge::UNLIMITED);
    let middle = page_addr(start, 1).unwrap();
    parent.protect_user_pages_at(middle, 1, USER_INACCESSIBLE_LEAF | PageTableFlags::NO_EXECUTE)
        .expect("hide immutable page");
    let mut hidden_bytes = [0u8; 1];
    assert!(parent.copy_from_user(middle, &mut hidden_bytes).is_err());
    let mut child = parent
        .clone_user_space_cow(&[], &[])
        .expect("immutable fork");
    assert_eq!(child.translate_user(start).unwrap().as_u64(), frames[0]);
    assert!(child.copy_from_user(middle, &mut hidden_bytes).is_err());
    child.protect_user_pages_at(middle, 1, PageTableFlags::NO_EXECUTE)
        .expect("restore child access");
    child.copy_from_user(middle, &mut hidden_bytes).expect("restored child read");
    assert!(parent.copy_from_user(middle, &mut hidden_bytes).is_err());
    parent.protect_user_pages_at(middle, 1, PageTableFlags::NO_EXECUTE)
        .expect("restore parent access");
    parent.copy_from_user(middle, &mut hidden_bytes).expect("restored parent read");
    let last = page_addr(start, 2).unwrap();
    parent.protect_user_pages_at(last, 1, USER_INACCESSIBLE_LEAF | PageTableFlags::NO_EXECUTE)
        .expect("hide before unmap");
    parent
        .protect_user_pages_at(
            start,
            1,
            PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        )
        .expect("private write upgrade");
    assert_ne!(parent.translate_user(start).unwrap().as_u64(), frames[0]);
    assert_eq!(crate::memory::charge::usage(GROUP), 4, "copy plus retained whole backing");
    parent
        .copy_into_user(start, b"private!!")
        .expect("private write");
    let mut bytes = [0u8; 9];
    child
        .copy_from_user(start, &mut bytes)
        .expect("fork source read");
    assert_eq!(&bytes, b"immutable");
    parent
        .unmap_present_user_pages_with_external_at(middle, 1, &[])
        .expect("private partial unmap");
    parent
        .map_zeroed_user_pages_at(middle, 1, PageTableFlags::NO_EXECUTE)
        .expect("reused hole");
    parent
        .unmap_present_user_pages_with_external_at(start, 3, &[])
        .expect("mixed owned/external unmap");
    assert_eq!(crate::memory::charge::usage(GROUP), 3, "unmap cannot refund retained backing");
    drop(parent);
    drop(backing);
    child
        .copy_from_user(start, &mut bytes)
        .expect("child retains backing");
    assert_eq!(&bytes, b"immutable");
    child
        .protect_user_pages_at(
            start,
            3,
            PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        )
        .expect("child write upgrade");
    child
        .copy_into_user(start, b"childcopy")
        .expect("child private write");
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Memory,
        "immutable-private-mapping-smoke",
        3,
        4,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mapping(start: u64, end: u64) -> ImmutableMapping {
        ImmutableMapping {
            start,
            end,
            backing: ImmutableBacking::new(1, 1, Arc::new(())),
        }
    }
    #[test]
    fn private_range_removal_splits_without_releasing_the_backing() {
        let original = [mapping(0x1000, 0x9000), mapping(0xa000, 0xc000)];
        let backing = original[0].backing.clone();
        let result = without_range(&original, 0x3000, 0x6000).unwrap();
        assert_eq!(
            result.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(),
            [(0x1000, 0x3000), (0x6000, 0x9000), (0xa000, 0xc000)]
        );
        assert!(Arc::ptr_eq(&backing._hold, &result[0].backing._hold));
        assert!(Arc::ptr_eq(&backing._hold, &result[1].backing._hold));
        assert!(without_range(&original, 0, 0x10000).unwrap().is_empty());
    }
    #[test]
    fn immutable_ranges_are_half_open_and_do_not_claim_a_reused_hole() {
        let mut space = ProcessAddressSpace::empty_for_tests();
        space.immutable.push(mapping(0x1000, 0x5000));
        space.immutable = without_range(&space.immutable, 0x2000, 0x3000).unwrap();
        assert!(space.immutable_contains(0x1fff));
        assert!(!space.immutable_contains(0x2000));
        assert!(space.immutable_contains(0x3000));
        assert!(!space.immutable_contains(0x5000));
    }
}
