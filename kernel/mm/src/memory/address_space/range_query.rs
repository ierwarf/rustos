//! One-walk residency queries over a user page range.
//!
//! - **Owner:** `kernel-mm` owns page-table residency queries; callers decide
//!   what an occupied or missing range means for their policy.
//! - **Boundary:** Only a page-aligned range fully inside user space is
//!   admitted. A huge leaf at an upper level counts as mapped for its whole
//!   span, so an "unmapped" answer never races past an occupied span.
//! - **Lifecycle:** Read-only. Each table on the path is loaded once and its
//!   entries are scanned in place; a missing PML4/PDPT/PD entry answers for
//!   its whole span without descending.
//! - **Concurrency:** The caller serialises page-table mutation (process
//!   state lock or exclusive ownership of a not-yet-published image).
//! - **Failure:** Range validation errors are returned; the walk itself
//!   cannot fail.
//! - **Forbidden:** No per-page root-to-leaf re-walk, no mutation, no
//!   allocation.
//! - **Evidence:** `process-address-space`.

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use super::{
    AddressSpaceError, PAGE_4KIB_U64, ProcessAddressSpace, validate_user_page_range,
};
use crate::memory::kernel_vm;

/// What one table slot says about the span it covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Slot<T> {
    Absent,
    Table(T),
    Leaf,
}

/// First page in `[start, end)` whose residency equals `want_mapped`.
///
/// `load(level, parent, index)` reads one slot; `level` is 4 (PML4, `parent`
/// is `None`) down to 1 (PT, where `Table` is treated as a leaf).
pub(super) fn first_page_with_residency<T: Copy>(
    start: u64,
    end: u64,
    want_mapped: bool,
    mut load: impl FnMut(u8, Option<T>, usize) -> Slot<T>,
) -> Option<u64> {
    const SHIFTS: [u32; 4] = [39, 30, 21, 12];
    let mut cursor = start;
    while cursor < end {
        let mut parent = None;
        let mut uniform = None;
        let mut level = 4_u8;
        while level >= 2 {
            let shift = SHIFTS[(4 - level) as usize];
            let index = ((cursor >> shift) & 0x1ff) as usize;
            match load(level, parent, index) {
                Slot::Absent => uniform = Some((shift, false)),
                Slot::Leaf => uniform = Some((shift, true)),
                Slot::Table(next) => parent = Some(next),
            }
            if uniform.is_some() {
                break;
            }
            level -= 1;
        }
        if let Some((shift, mapped)) = uniform {
            if mapped == want_mapped {
                return Some(cursor);
            }
            cursor = ((cursor >> shift) + 1) << shift;
            continue;
        }
        // `parent` is the page table covering `cursor`; scan its entries.
        let mut index = ((cursor >> 12) & 0x1ff) as usize;
        while index < 512 && cursor < end {
            let mapped = !matches!(load(1, parent, index), Slot::Absent);
            if mapped == want_mapped {
                return Some(cursor);
            }
            index += 1;
            cursor += PAGE_4KIB_U64;
        }
    }
    None
}

impl ProcessAddressSpace {
    /// First mapped page in the range, or `None` when the whole range is free.
    pub fn first_mapped_user_page(
        &self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<Option<VirtAddr>, AddressSpaceError> {
        self.first_user_page_with_residency(start, page_count, true)
    }

    /// First unmapped page in the range, or `None` when the whole range is mapped.
    pub fn first_unmapped_user_page(
        &self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<Option<VirtAddr>, AddressSpaceError> {
        self.first_user_page_with_residency(start, page_count, false)
    }

    fn first_user_page_with_residency(
        &self,
        start: VirtAddr,
        page_count: usize,
        want_mapped: bool,
    ) -> Result<Option<VirtAddr>, AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        let end = start.as_u64() + page_count as u64 * PAGE_4KIB_U64;
        let root = self.root_table_ref();
        let found = first_page_with_residency(
            start.as_u64(),
            end,
            want_mapped,
            |level, parent, index| {
                let table = match parent {
                    None => root,
                    // SAFETY: `parent` came from a present, non-huge entry of
                    // this address space's own table tree, which the caller
                    // keeps stable for the duration of the query.
                    Some(phys) => unsafe { kernel_vm::phys_to_table_ref(phys) },
                };
                let entry = &table[index];
                if entry.is_unused() {
                    Slot::Absent
                } else if level == 1 || entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                    Slot::Leaf
                } else {
                    Slot::Table(entry.addr())
                }
            },
        );
        Ok(found.map(VirtAddr::new))
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;

    use super::*;

    const PAGE: u64 = PAGE_4KIB_U64;

    /// Sparse fake tree: a set of mapped 4 KiB pages plus 2 MiB huge leaves.
    struct Fake {
        pages: BTreeSet<u64>,
        huge_2m: BTreeSet<u64>,
        loads: usize,
    }

    impl Fake {
        fn load(&mut self, level: u8, parent: Option<u64>, index: usize) -> Slot<u64> {
            self.loads += 1;
            // Table identity is the base address of the span it covers.
            let (base, span) = match level {
                4 => (0, 1_u64 << 48),
                3 => (parent.unwrap(), 1 << 39),
                2 => (parent.unwrap(), 1 << 30),
                _ => (parent.unwrap(), 1 << 21),
            };
            let child_span = span >> 9;
            let child = base + index as u64 * child_span;
            let any = |set: &BTreeSet<u64>| set.range(child..child + child_span).next().is_some();
            match level {
                1 => {
                    if self.pages.contains(&child) {
                        Slot::Leaf
                    } else {
                        Slot::Absent
                    }
                }
                2 if self.huge_2m.contains(&child) => Slot::Leaf,
                _ => {
                    if any(&self.pages) || any(&self.huge_2m) {
                        Slot::Table(child)
                    } else {
                        Slot::Absent
                    }
                }
            }
        }
    }

    fn naive(fake: &Fake, start: u64, end: u64, want: bool) -> Option<u64> {
        (start..end).step_by(PAGE as usize).find(|page| {
            let mapped = fake.pages.contains(page)
                || fake.huge_2m.iter().any(|h| (*h..*h + (1 << 21)).contains(page));
            mapped == want
        })
    }

    fn query(fake: &mut Fake, start: u64, end: u64, want: bool) -> Option<u64> {
        let mut loader = |l, p, i| fake.load(l, p, i);
        first_page_with_residency(start, end, want, &mut loader)
    }

    #[test]
    fn empty_stack_reserve_range_is_one_short_walk() {
        let mut fake = Fake { pages: BTreeSet::new(), huge_2m: BTreeSet::new(), loads: 0 };
        let start = 0x7fff_0000_0000_u64;
        assert_eq!(query(&mut fake, start, start + 256 * PAGE, true), None);
        assert!(fake.loads <= 3, "a missing PML4 slot answers for 512 GiB: {}", fake.loads);
    }

    #[test]
    fn dense_range_scans_each_table_once() {
        let mut fake = Fake { pages: BTreeSet::new(), huge_2m: BTreeSet::new(), loads: 0 };
        let start = 0x4000_0000_u64;
        // Keep the PT present but the scanned range empty.
        fake.pages.insert(start + 300 * PAGE);
        assert_eq!(query(&mut fake, start, start + 256 * PAGE, true), None);
        assert!(fake.loads <= 3 + 256, "walk loads: {}", fake.loads);
    }

    #[test]
    fn matches_naive_scan_on_random_layouts() {
        let mut seed = 0x1234_5678_9abc_def1_u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..300 {
            let mut fake =
                Fake { pages: BTreeSet::new(), huge_2m: BTreeSet::new(), loads: 0 };
            let base = 0x4000_0000_u64 + (rnd() % 4) * (1 << 30);
            for _ in 0..(rnd() % 40) {
                fake.pages.insert(base + (rnd() % 3000) * PAGE);
            }
            if rnd() % 3 == 0 {
                fake.huge_2m.insert(base + (rnd() % 4) * (1 << 21));
            }
            let start = base + (rnd() % 1200) * PAGE;
            let end = start + (1 + rnd() % 1500) * PAGE;
            for want in [true, false] {
                let got = query(&mut fake, start, end, want);
                assert_eq!(got, naive(&fake, start, end, want), "want={want}");
            }
        }
    }
}
