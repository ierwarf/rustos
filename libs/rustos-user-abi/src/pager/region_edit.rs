//! The one definition of how a range edit changes a pager-managed region.
//!
//! - **Owner:** this module. Ring0's VMA table and pagerd's region table are
//!   two replicas of the same map; neither may derive its own split rule.
//! - **Boundary:** input is one region plus one page-aligned edit interval;
//!   output is the exact surviving fragments. No allocation, no policy.
//! - **Lifecycle:** an edit either leaves a region untouched, trims one end,
//!   splits it in two, or removes it. There is no fifth outcome.
//! - **Concurrency:** pure `const` arithmetic; both replicas may evaluate it
//!   independently and must reach the same answer.
//! - **Failure:** a malformed region or edit yields
//!   [`PagerRegionEdit::Malformed`] rather than a silent partial rewrite.
//! - **Forbidden:** wholesale removal on partial overlap. Linux `munmap(2)`
//!   leaves the surrounding parts mapped, so a replica that drops the whole
//!   region forgets memory the process can still touch.
//! - **Evidence:** unit tests below, `PagerRegionAgreement` TLA+, and the
//!   ring0/pagerd agreement tests that drive both replicas from this rule.
//!
//! # Why this exists
//!
//! Ring0 preserved the left and right remainders of a partial unmap while
//! pagerd deleted every overlapping region. The two then held different maps
//! of the same address space: a later fault in a surviving remainder passed
//! ring0's VMA check, reached pagerd, matched no region, and killed the
//! thread. Publishing the rule here makes that divergence unrepresentable.

use super::{PAGER_MAX_FAULT_SLOTS, PAGER_MAX_TRACKED_REGIONS, PAGER_MAX_VMAS_PER_PROCESS};
use super::{
    PAGER_PAGE_BYTES, PagerVmRegionWire, VM_PROT_EXECUTE, VM_PROT_KNOWN, VM_PROT_READ,
    VM_PROT_WRITE,
};

/// One page-aligned half-open edit interval `[start, end)`.
///
/// This is the shape of both `munmap` and `mprotect` as the pager protocol
/// sees them: an interval that is applied to whatever regions it overlaps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PagerRangeEdit {
    pub start: u64,
    pub end: u64,
    /// `None` removes the covered span; `Some(prot)` re-protects it in place.
    pub replacement_prot: Option<u32>,
}

impl PagerRangeEdit {
    pub const fn unmap(start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            replacement_prot: None,
        }
    }

    pub const fn protect(start: u64, end: u64, prot: u32) -> Self {
        Self {
            start,
            end,
            replacement_prot: Some(prot),
        }
    }

    pub const fn is_canonical(self) -> bool {
        if self.start == 0
            || self.start >= self.end
            || self.start & (PAGER_PAGE_BYTES - 1) != 0
            || self.end & (PAGER_PAGE_BYTES - 1) != 0
        {
            return false;
        }
        match self.replacement_prot {
            None => true,
            // A replacement protection may never introduce an unknown right
            // or a simultaneously writable and executable mapping. Widening
            // against the region it edits is checked in `apply`, which is the
            // only place that can see the original rights.
            //
            // `Some(0)` is `PROT_NONE` and is deliberately allowed: see
            // [`PagerRegionEdit::pager_fragments`] for why the two replicas
            // install different things for it.
            Some(prot) => {
                prot & !VM_PROT_KNOWN == 0
                    && !(prot & VM_PROT_WRITE != 0 && prot & VM_PROT_EXECUTE != 0)
            }
        }
    }

    pub const fn overlaps(self, region: PagerVmRegionWire) -> bool {
        self.start < region.end && region.start < self.end
    }
}

/// Every outcome of applying one edit to one region.
///
/// The fragment order is address order, and every fragment is a complete
/// region wire: a replica installs them verbatim rather than re-deriving
/// offsets, which is where the two implementations previously drifted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerRegionEdit {
    /// The edit does not touch this region. It keeps its slot unchanged.
    ///
    /// It carries the region so that [`PagerRegionEdit::fragments`] is total:
    /// a caller that installs "whatever survives" gets the right answer in
    /// every case without a separate untouched branch.
    Untouched(PagerVmRegionWire),
    /// The edit covers the region entirely. Its slot becomes free.
    Removed,
    /// One surviving fragment replaces the region in its own slot.
    Replaced(PagerVmRegionWire),
    /// Two surviving fragments. `left` reuses the region's slot; `right`
    /// needs one additional free slot, and is the only growth this rule can
    /// produce.
    Split {
        left: PagerVmRegionWire,
        right: PagerVmRegionWire,
    },
    /// Three fragments: an unedited head, the re-protected span, and an
    /// unedited tail. Only a `protect` edit can produce this.
    ProtectedSplit {
        left: PagerVmRegionWire,
        middle: PagerVmRegionWire,
        right: PagerVmRegionWire,
    },
    /// The region or the edit is not a shape this protocol can represent.
    /// A replica must fail the whole transaction rather than install part of
    /// it.
    Malformed,
    /// The edit widens rights the region does not already hold. Rejected
    /// before any fragment is produced.
    Denied,
}

/// Fragments an outcome installs, in address order, with how many are set.
pub type PagerRegionFragments = ([PagerVmRegionWire; 3], usize);

impl PagerRegionEdit {
    /// Every surviving fragment, in address order.
    ///
    /// This is what **ring0** installs. It includes a `PROT_NONE` span, which
    /// ring0 keeps as a deny-all VMA so the address stays owned and every
    /// access is refused before a fault is ever dispatched.
    pub const fn fragments(self) -> PagerRegionFragments {
        let empty = PagerVmRegionWire {
            start: 0,
            end: 0,
            object: crate::pager::PagerObjectIdentityWire {
                object_type: 0,
                reserved0: 0,
                rights: 0,
                backing_service: 0,
                slot: 0,
                generation: 0,
                pager_epoch: 0,
                backing_generation: 0,
            },
            object_offset: 0,
            prot: 0,
            sharing: 0,
            reserved0: 0,
            vma_generation: 0,
            process_handle: 0,
            process_generation: 0,
            mm_generation: 0,
            fault_endpoint: crate::pager::PagerEndpointCapabilityWire {
                slot: 0,
                generation: 0,
                rights: 0,
            },
            reserved1: [0; 2],
        };
        match self {
            Self::Removed | Self::Malformed | Self::Denied => ([empty, empty, empty], 0),
            Self::Untouched(region) => ([region, empty, empty], 1),
            Self::Replaced(only) => ([only, empty, empty], 1),
            Self::Split { left, right } => ([left, right, empty], 2),
            Self::ProtectedSplit {
                left,
                middle,
                right,
            } => ([left, middle, right], 3),
        }
    }

    /// The fragments a **pager** installs: everything ring0 keeps, minus any
    /// span left with no rights at all.
    ///
    /// The two replicas differ here on purpose, and only here.
    /// `mprotect(PROT_NONE)` leaves ring0 a deny-all VMA whose `lookup` refuses
    /// every access, so no fault for that span can ever be dispatched. A pager
    /// therefore has nothing to decide about it - and could not hold it anyway,
    /// because a region with no rights is not a canonical wire region. Dropping
    /// it in the pager is inert; keeping a deny-all VMA in ring0 is what makes
    /// the address stay owned instead of becoming re-mappable.
    pub const fn pager_fragments(self) -> PagerRegionFragments {
        let (fragments, len) = self.fragments();
        let mut kept = fragments;
        let mut kept_len = 0;
        let mut index = 0;
        while index < len {
            if fragments[index].prot != 0 {
                kept[kept_len] = fragments[index];
                kept_len += 1;
            }
            index += 1;
        }
        (kept, kept_len)
    }

    /// Slots the outcome occupies in ring0's table after installation.
    pub const fn fragment_count(self) -> usize {
        self.fragments().1
    }

    /// Additional free slots the outcome consumes beyond the region's own.
    ///
    /// A replica must check this against its free-slot count *before* it
    /// withdraws the original region, so a table that cannot hold the result
    /// refuses the edit instead of losing the region.
    pub const fn additional_slots(self) -> usize {
        match self.fragment_count() {
            0 | 1 => 0,
            count => count - 1,
        }
    }

    /// The same headroom question for a pager's table.
    pub const fn additional_pager_slots(self) -> usize {
        match self.pager_fragments().1 {
            0 | 1 => 0,
            count => count - 1,
        }
    }

    pub const fn is_rejection(self) -> bool {
        matches!(self, Self::Malformed | Self::Denied)
    }
}

/// Applies one edit to one region and returns the exact surviving fragments.
///
/// This is the whole rule. Both replicas call it; neither reimplements it.
///
/// The half-open cases, in address order:
///
/// | region vs edit                    | outcome            |
/// |-----------------------------------|--------------------|
/// | disjoint                          | `Untouched`        |
/// | edit covers region                | `Removed`          |
/// | edit covers the region's head     | `Replaced` (tail)  |
/// | edit covers the region's tail     | `Replaced` (head)  |
/// | edit lies strictly inside         | `Split`            |
///
/// A `protect` edit keeps the covered span instead of dropping it, so its
/// interior case yields three fragments rather than two.
pub const fn apply_region_edit(region: PagerVmRegionWire, edit: PagerRangeEdit) -> PagerRegionEdit {
    if !editable(region) || !edit.is_canonical() {
        return PagerRegionEdit::Malformed;
    }
    if !edit.overlaps(region) {
        return PagerRegionEdit::Untouched(region);
    }
    // Attenuation only. A replica may narrow the rights of a span it already
    // owns; it may never hand back more than the region carried.
    if let Some(prot) = edit.replacement_prot {
        if prot & !region.prot != 0 {
            return PagerRegionEdit::Denied;
        }
    }

    let head_end = if edit.start > region.start {
        edit.start
    } else {
        region.start
    };
    let tail_start = if edit.end < region.end {
        edit.end
    } else {
        region.end
    };
    let has_head = region.start < head_end;
    let has_tail = tail_start < region.end;

    let head = trimmed(region, region.start, head_end);
    let tail = trimmed(region, tail_start, region.end);
    let covered = trimmed(region, head_end, tail_start);

    match edit.replacement_prot {
        None => match (has_head, has_tail) {
            (false, false) => PagerRegionEdit::Removed,
            (true, false) => match head {
                Some(head) => PagerRegionEdit::Replaced(head),
                None => PagerRegionEdit::Malformed,
            },
            (false, true) => match tail {
                Some(tail) => PagerRegionEdit::Replaced(tail),
                None => PagerRegionEdit::Malformed,
            },
            (true, true) => match (head, tail) {
                (Some(left), Some(right)) => PagerRegionEdit::Split { left, right },
                _ => PagerRegionEdit::Malformed,
            },
        },
        Some(prot) => {
            let Some(middle) = reprotected(covered, prot) else {
                return PagerRegionEdit::Malformed;
            };
            match (has_head, has_tail) {
                (false, false) => PagerRegionEdit::Replaced(middle),
                (true, false) => match head {
                    Some(left) => PagerRegionEdit::Split {
                        left,
                        right: middle,
                    },
                    None => PagerRegionEdit::Malformed,
                },
                (false, true) => match tail {
                    Some(right) => PagerRegionEdit::Split {
                        left: middle,
                        right,
                    },
                    None => PagerRegionEdit::Malformed,
                },
                (true, true) => match (head, tail) {
                    (Some(left), Some(right)) => PagerRegionEdit::ProtectedSplit {
                        left,
                        middle,
                        right,
                    },
                    _ => PagerRegionEdit::Malformed,
                },
            }
        }
    }
}

/// Narrows `region` to `[start, end)`, carrying the backing offset with it.
///
/// The offset shift is the half of this rule the two replicas most easily get
/// wrong: a fragment that keeps the original `object_offset` reads the wrong
/// page of its backing object, and ring0's fault revalidation then rejects
/// every fault in it as stale.
const fn trimmed(region: PagerVmRegionWire, start: u64, end: u64) -> Option<PagerVmRegionWire> {
    if start >= end || start < region.start || end > region.end {
        return None;
    }
    let Some(object_offset) = region.object_offset.checked_add(start - region.start) else {
        return None;
    };
    let mut fragment = region;
    fragment.start = start;
    fragment.end = end;
    fragment.object_offset = object_offset;
    if editable(fragment) {
        Some(fragment)
    } else {
        None
    }
}

/// A region this rule may edit.
///
/// Every canonical wire region qualifies, and so does a ring0 `PROT_NONE`
/// region. A `PROT_NONE` VMA is a real ring0 state - `mprotect(PROT_NONE)`
/// creates one, and `munmap` must still be able to remove it - but it carries
/// no rights, so it is not a canonical *wire* region and no pager holds it.
/// Rejecting it here would make a deny-all span permanently un-unmappable.
const fn editable(region: PagerVmRegionWire) -> bool {
    let mut probe = region;
    if probe.prot == 0 {
        probe.prot = VM_PROT_READ;
    }
    probe.is_canonical()
}

/// Applies the replacement protection to an already-geometry-checked fragment.
///
/// `trimmed` has proved the fragment canonical at the region's original
/// rights, so the only field changing here is `prot`. It is set without a
/// second canonicality check on purpose: `PROT_NONE` is a legitimate ring0
/// outcome and is not a canonical wire region, which is precisely why
/// [`PagerRegionEdit::pager_fragments`] filters it out instead.
const fn reprotected(fragment: Option<PagerVmRegionWire>, prot: u32) -> Option<PagerVmRegionWire> {
    let Some(mut fragment) = fragment else {
        return None;
    };
    fragment.prot = prot;
    Some(fragment)
}

/// Slots one `munmap` edit can add to a replica's table.
///
/// An edit is one contiguous interval and regions never overlap, so it can
/// split at most the single region that strictly contains it. Every other
/// overlapped region is trimmed or removed, which never grows the table.
pub const PAGER_MAX_REGION_GROWTH_PER_UNMAP: usize = 1;

/// Slots one `mprotect` edit can add. Its interior case keeps the covered
/// span as a third fragment.
pub const PAGER_MAX_REGION_GROWTH_PER_PROTECT: usize = 2;

/// Processes whose entire ring0 VMA table pagerd can hold simultaneously.
///
/// This is the static relation the two capacities have to each other, and it
/// is the honest bound: beyond this many demand-paged processes admission
/// *will* refuse, and that refusal must stay explicit and counted rather than
/// downgrading demand paging in silence.
pub const PAGER_MIN_FULLY_TRACKED_PROCESSES: usize =
    PAGER_MAX_TRACKED_REGIONS / PAGER_MAX_VMAS_PER_PROCESS;

const _: () = assert!(
    PAGER_MAX_TRACKED_REGIONS % PAGER_MAX_VMAS_PER_PROCESS == 0,
    "pagerd's region table must be a whole multiple of one process's VMA table"
);
const _: () = assert!(
    PAGER_MIN_FULLY_TRACKED_PROCESSES >= 2,
    "one process filling its VMA table must never wedge every other process"
);
const _: () = assert!(
    PAGER_MAX_TRACKED_REGIONS >= PAGER_MAX_VMAS_PER_PROCESS + PAGER_MAX_REGION_GROWTH_PER_PROTECT,
    "a replica must be able to hold one full process table plus one split"
);
const _: () = assert!(
    PAGER_MAX_REGION_GROWTH_PER_PROTECT >= PAGER_MAX_REGION_GROWTH_PER_UNMAP,
    "a protect edit is the wider of the two growth cases"
);

/// Why a bounded pager resource refused, as one code both sides agree on.
///
/// A single `Pressure` result made a full region table, an empty fault-frame
/// reserve, an exhausted grant table and a full release queue indistinguishable
/// in the log, so every occurrence cost a fresh investigation of all four.
pub const PAGER_PRESSURE_UNSPECIFIED: u16 = 0;
/// pagerd's cross-process region table has no free slot for an admission.
pub const PAGER_PRESSURE_REGION_TABLE_FULL: u16 = 1;
/// A release or protect edit must split a region and no slot is free for the
/// second fragment. The replica keeps the wider region and asks for a retry.
pub const PAGER_PRESSURE_REGION_SPLIT_NO_SLOT: u16 = 2;
/// Ring0's per-process pager VMA table is full.
pub const PAGER_PRESSURE_VMA_SLOTS_FULL: u16 = 3;
/// Ring0's fixed fault-slot table has no free slot.
pub const PAGER_PRESSURE_FAULT_SLOTS_FULL: u16 = 4;
/// The wired fault-frame reserve is empty at exception time.
pub const PAGER_PRESSURE_FAULT_FRAME_RESERVE_EMPTY: u16 = 5;
/// The opaque frame-grant table has no free slot.
pub const PAGER_PRESSURE_GRANT_TABLE_FULL: u16 = 6;
/// Ring0's unconfirmed-release reconciliation queue overflowed.
pub const PAGER_PRESSURE_RELEASE_QUEUE_FULL: u16 = 7;
/// A publication sequence reached its terminal value.
pub const PAGER_PRESSURE_SEQUENCE_EXHAUSTED: u16 = 8;
pub const PAGER_PRESSURE_KNOWN_MAX: u16 = PAGER_PRESSURE_SEQUENCE_EXHAUSTED;

/// Stable log name for a pressure code, so one counter name means one cause.
pub const fn pager_pressure_name(code: u16) -> &'static str {
    match code {
        PAGER_PRESSURE_REGION_TABLE_FULL => "pager-pressure-region-table-full",
        PAGER_PRESSURE_REGION_SPLIT_NO_SLOT => "pager-pressure-region-split-no-slot",
        PAGER_PRESSURE_VMA_SLOTS_FULL => "pager-pressure-vma-slots-full",
        PAGER_PRESSURE_FAULT_SLOTS_FULL => "pager-pressure-fault-slots-full",
        PAGER_PRESSURE_FAULT_FRAME_RESERVE_EMPTY => "pager-pressure-fault-frame-reserve-empty",
        PAGER_PRESSURE_GRANT_TABLE_FULL => "pager-pressure-grant-table-full",
        PAGER_PRESSURE_RELEASE_QUEUE_FULL => "pager-pressure-release-queue-full",
        PAGER_PRESSURE_SEQUENCE_EXHAUSTED => "pager-pressure-sequence-exhausted",
        _ => "pager-pressure-unspecified",
    }
}

/// Wired fault frames a boot must hold for the fault path to be allocation-free.
///
/// Sized to the fault-slot table on purpose. Every slot that can be reserved
/// holds at most one reserve frame until its reply consumes or cancels it, so
/// sizing the reserve below the slot table makes "reserve empty" reachable
/// while slots are still free - an exhaustion with no admission point to
/// refuse at, which surfaces as an unexplained SIGSEGV rather than a counted
/// refusal. With this relation the reserve can only run dry after fault-slot
/// admission has already refused, and that refusal is counted.
pub const PAGER_WIRED_FAULT_FRAMES: usize = PAGER_MAX_FAULT_SLOTS;
/// Opaque frame grants a boot must be able to publish at once.
pub const PAGER_MAX_FRAME_GRANTS: usize = PAGER_MAX_FAULT_SLOTS;

const _: () = assert!(
    PAGER_WIRED_FAULT_FRAMES >= PAGER_MAX_FAULT_SLOTS,
    "every reservable fault slot must have a wired frame behind it"
);
const _: () = assert!(
    PAGER_MAX_FRAME_GRANTS >= PAGER_MAX_FAULT_SLOTS,
    "every reservable fault slot must have a grant slot behind it"
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::{
        PagerEndpointCapabilityWire, PagerObjectIdentityWire, VM_OBJECT_ANONYMOUS, VM_PROT_READ,
        VM_SHARING_PRIVATE,
    };

    fn region(start: u64, end: u64) -> PagerVmRegionWire {
        PagerVmRegionWire {
            start,
            end,
            object: PagerObjectIdentityWire {
                object_type: VM_OBJECT_ANONYMOUS,
                rights: VM_PROT_READ | VM_PROT_WRITE,
                slot: 9,
                generation: 3,
                pager_epoch: 1,
                backing_generation: 5,
                ..PagerObjectIdentityWire::default()
            },
            object_offset: 0,
            prot: VM_PROT_READ | VM_PROT_WRITE,
            sharing: VM_SHARING_PRIVATE,
            reserved0: 0,
            vma_generation: 7,
            process_handle: 2,
            process_generation: 4,
            mm_generation: 6,
            fault_endpoint: PagerEndpointCapabilityWire {
                slot: 11,
                generation: 13,
                rights: 1,
            },
            reserved1: [0; 2],
        }
    }

    #[test]
    fn a_disjoint_edit_leaves_the_region_alone() {
        let region = region(0x10_000, 0x14_000);
        assert_eq!(
            apply_region_edit(region, PagerRangeEdit::unmap(0x14_000, 0x15_000)),
            PagerRegionEdit::Untouched(region)
        );
        assert_eq!(
            apply_region_edit(region, PagerRangeEdit::unmap(0x0f_000, 0x10_000)),
            PagerRegionEdit::Untouched(region)
        );
    }

    #[test]
    fn a_covering_edit_removes_the_region() {
        let region = region(0x10_000, 0x14_000);
        assert_eq!(
            apply_region_edit(region, PagerRangeEdit::unmap(0x10_000, 0x14_000)),
            PagerRegionEdit::Removed
        );
        assert_eq!(
            apply_region_edit(region, PagerRangeEdit::unmap(0x0f_000, 0x20_000)),
            PagerRegionEdit::Removed
        );
    }

    /// The exact case that made ring0 and pagerd disagree: an unmap of the
    /// head must leave the tail mapped, at the tail's own backing offset.
    #[test]
    fn trimming_the_head_keeps_the_tail_and_shifts_its_backing_offset() {
        let region = region(0x10_000, 0x14_000);
        let PagerRegionEdit::Replaced(tail) =
            apply_region_edit(region, PagerRangeEdit::unmap(0x10_000, 0x12_000))
        else {
            panic!("head trim must keep the tail");
        };
        assert_eq!((tail.start, tail.end), (0x12_000, 0x14_000));
        assert_eq!(tail.object_offset, 0x2000);
        assert_eq!(tail.vma_generation, region.vma_generation);
    }

    #[test]
    fn trimming_the_tail_keeps_the_head_at_its_original_offset() {
        let region = region(0x10_000, 0x14_000);
        let PagerRegionEdit::Replaced(head) =
            apply_region_edit(region, PagerRangeEdit::unmap(0x12_000, 0x14_000))
        else {
            panic!("tail trim must keep the head");
        };
        assert_eq!((head.start, head.end), (0x10_000, 0x12_000));
        assert_eq!(head.object_offset, region.object_offset);
    }

    /// `munmap(2)` in the middle of a mapping leaves two smaller mappings on
    /// either side. A replica that returns `Removed` here forgets memory the
    /// process can still touch.
    #[test]
    fn an_interior_unmap_splits_into_two_mappings_like_linux() {
        let region = region(0x10_000, 0x18_000);
        let PagerRegionEdit::Split { left, right } =
            apply_region_edit(region, PagerRangeEdit::unmap(0x12_000, 0x14_000))
        else {
            panic!("an interior unmap must split");
        };
        assert_eq!((left.start, left.end), (0x10_000, 0x12_000));
        assert_eq!(left.object_offset, 0);
        assert_eq!((right.start, right.end), (0x14_000, 0x18_000));
        assert_eq!(right.object_offset, 0x4000);
        assert_eq!(
            apply_region_edit(region, PagerRangeEdit::unmap(0x12_000, 0x14_000)).additional_slots(),
            PAGER_MAX_REGION_GROWTH_PER_UNMAP
        );
    }

    #[test]
    fn an_interior_protect_keeps_three_fragments_and_only_narrows_rights() {
        let region = region(0x10_000, 0x18_000);
        let outcome = apply_region_edit(
            region,
            PagerRangeEdit::protect(0x12_000, 0x14_000, VM_PROT_READ),
        );
        let PagerRegionEdit::ProtectedSplit {
            left,
            middle,
            right,
        } = outcome
        else {
            panic!("an interior protect must keep the covered span");
        };
        assert_eq!(left.prot, region.prot);
        assert_eq!(middle.prot, VM_PROT_READ);
        assert_eq!((middle.start, middle.end), (0x12_000, 0x14_000));
        assert_eq!(middle.object_offset, 0x2000);
        assert_eq!(right.prot, region.prot);
        assert_eq!(right.object_offset, 0x4000);
        assert_eq!(
            outcome.additional_slots(),
            PAGER_MAX_REGION_GROWTH_PER_PROTECT
        );
    }

    #[test]
    fn a_protect_that_widens_rights_is_denied_before_any_fragment_exists() {
        let mut region = region(0x10_000, 0x18_000);
        region.prot = VM_PROT_READ;
        region.object.rights = VM_PROT_READ;
        assert_eq!(
            apply_region_edit(
                region,
                PagerRangeEdit::protect(0x10_000, 0x18_000, VM_PROT_READ | VM_PROT_WRITE)
            ),
            PagerRegionEdit::Denied
        );
    }

    #[test]
    fn a_misaligned_or_inverted_edit_is_malformed_rather_than_partially_applied() {
        let region = region(0x10_000, 0x18_000);
        for edit in [
            PagerRangeEdit::unmap(0x10_001, 0x18_000),
            PagerRangeEdit::unmap(0x10_000, 0x18_001),
            PagerRangeEdit::unmap(0x18_000, 0x10_000),
            PagerRangeEdit::unmap(0, 0x1000),
            PagerRangeEdit::protect(0x10_000, 0x18_000, VM_PROT_WRITE | VM_PROT_EXECUTE),
        ] {
            assert_eq!(apply_region_edit(region, edit), PagerRegionEdit::Malformed);
        }
    }

    /// The capacity claim the replicas rely on: only the region an edit lies
    /// strictly inside can grow the table, so one edit costs at most one slot
    /// no matter how many regions it crosses.
    #[test]
    fn only_a_strictly_interior_edit_grows_the_table() {
        let edit = PagerRangeEdit::unmap(0x12_000, 0x1a_000);
        let crossed = [
            region(0x10_000, 0x14_000),
            region(0x14_000, 0x18_000),
            region(0x18_000, 0x1c_000),
        ];
        let growth: usize = crossed
            .iter()
            .map(|region| apply_region_edit(*region, edit).additional_slots())
            .sum();
        assert_eq!(growth, 0);
        assert!(growth <= PAGER_MAX_REGION_GROWTH_PER_UNMAP);
    }

    #[test]
    fn every_pressure_code_has_its_own_name() {
        let mut names = [""; PAGER_PRESSURE_KNOWN_MAX as usize + 1];
        for (index, name) in names.iter_mut().enumerate() {
            *name = pager_pressure_name(index as u16);
        }
        for (index, name) in names.iter().enumerate() {
            assert!(
                !names[..index].contains(name),
                "pressure causes must not share a name"
            );
        }
        assert_eq!(
            pager_pressure_name(PAGER_PRESSURE_KNOWN_MAX + 1),
            "pager-pressure-unspecified"
        );
    }

    /// `mprotect(PROT_NONE)` is the one case where the two replicas install
    /// different things, and it has to stay explicit. Ring0 keeps a deny-all
    /// VMA so the address stays owned; the pager keeps nothing, because a
    /// span with no rights can never produce a fault it would have to answer.
    #[test]
    fn prot_none_is_a_ring0_vma_and_a_pager_removal() {
        let region = region(0x10_000, 0x18_000);
        let outcome = apply_region_edit(region, PagerRangeEdit::protect(0x12_000, 0x14_000, 0));
        let (ring0, ring0_len) = outcome.fragments();
        assert_eq!(ring0_len, 3);
        assert_eq!(ring0[1].prot, 0);
        assert_eq!((ring0[1].start, ring0[1].end), (0x12_000, 0x14_000));

        let (pager, pager_len) = outcome.pager_fragments();
        assert_eq!(pager_len, 2);
        assert_eq!((pager[0].start, pager[0].end), (0x10_000, 0x12_000));
        assert_eq!((pager[1].start, pager[1].end), (0x14_000, 0x18_000));
        assert_eq!(outcome.additional_pager_slots(), 1);

        // Every fragment a pager installs is one ring0 also installs. The
        // replicas may differ in what they drop, never in what they invent.
        for index in 0..pager_len {
            assert!(ring0[..ring0_len].contains(&pager[index]));
        }
    }

    /// A deny-all region is still ring0 state, so `munmap` has to be able to
    /// remove it. Rejecting it as non-canonical made a `PROT_NONE` span
    /// permanently un-unmappable.
    #[test]
    fn a_prot_none_region_can_still_be_unmapped() {
        let mut region = region(0x10_000, 0x18_000);
        region.prot = 0;
        assert_eq!(
            apply_region_edit(region, PagerRangeEdit::unmap(0x10_000, 0x18_000)),
            PagerRegionEdit::Removed
        );
        let PagerRegionEdit::Split { left, right } =
            apply_region_edit(region, PagerRangeEdit::unmap(0x12_000, 0x14_000))
        else {
            panic!("a deny-all region splits like any other");
        };
        assert_eq!(left.prot, 0);
        assert_eq!(right.prot, 0);
        // ...and a pager installs neither, because neither can fault.
        assert_eq!(
            apply_region_edit(region, PagerRangeEdit::unmap(0x12_000, 0x14_000))
                .pager_fragments()
                .1,
            0
        );
    }

    /// The property both replicas are checked against: an edit never leaves an
    /// address mapped that it removed, and never removes one it did not touch.
    #[test]
    fn surviving_fragments_are_exactly_the_region_minus_the_edit() {
        let region = region(0x10_000, 0x18_000);
        let page = PAGER_PAGE_BYTES;
        for edit_start in (0x0e_000..0x1a_000).step_by(page as usize) {
            for edit_end in ((edit_start + page)..=0x1a_000).step_by(page as usize) {
                let edit = PagerRangeEdit::unmap(edit_start, edit_end);
                let (fragments, len) = apply_region_edit(region, edit).fragments();
                for address in (region.start..region.end).step_by(page as usize) {
                    let removed = address >= edit_start && address < edit_end;
                    let covered = fragments[..len]
                        .iter()
                        .any(|fragment| fragment.contains(address));
                    assert_eq!(
                        covered, !removed,
                        "{address:#x} under unmap {edit_start:#x}..{edit_end:#x}"
                    );
                }
                // Every surviving page still names its own backing offset.
                for fragment in &fragments[..len] {
                    assert_eq!(
                        fragment.object_offset,
                        region.object_offset + (fragment.start - region.start)
                    );
                }
            }
        }
    }

    #[test]
    fn the_reserve_is_sized_to_the_fault_slot_table() {
        assert!(PAGER_WIRED_FAULT_FRAMES >= PAGER_MAX_FAULT_SLOTS);
        assert!(PAGER_MAX_FRAME_GRANTS >= PAGER_MAX_FAULT_SLOTS);
        assert!(PAGER_MIN_FULLY_TRACKED_PROCESSES >= 2);
    }
}
