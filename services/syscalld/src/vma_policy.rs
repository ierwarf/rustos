use alloc::collections::BTreeMap;
use alloc::vec::Vec;

pub trait AddressRange {
    fn start(&self) -> u64;
    fn end(&self) -> u64;
}

/// A mapping record that can be cut at either end without reallocating.
pub trait VmaEntry: AddressRange + Clone {
    fn set_start(&mut self, start: u64);
    fn set_end(&mut self, end: u64);
}

/// Answers "which live range intersects `[start, end)` first" for next-fit.
pub trait ConflictIndex {
    /// End of the lowest-starting live range intersecting `[start, end)`.
    fn first_conflict_end(&self, start: u64, end: u64) -> Option<u64>;
}

impl<T: AddressRange> ConflictIndex for [T] {
    fn first_conflict_end(&self, start: u64, end: u64) -> Option<u64> {
        self.iter()
            .find(|range| start < range.end() && end > range.start())
            .map(AddressRange::end)
    }
}

/// Non-overlapping mapping records ordered by start address.
///
/// Every mutation touches only the records that intersect the request, so
/// mmap/munmap/mprotect cost is O(log n + overlaps) and never copies the table.
#[derive(Clone, Debug)]
pub struct VmaMap<V> {
    map: BTreeMap<u64, V>,
}

impl<V> Default for VmaMap<V> {
    fn default() -> Self {
        Self { map: BTreeMap::new() }
    }
}

impl<V: VmaEntry> VmaMap<V> {
    pub const fn new() -> Self {
        Self { map: BTreeMap::new() }
    }

    // TEST-HARNESS: used by tests and the lib build; the bin only queries ranges.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    // TEST-HARNESS: used by tests and the lib build; the bin only queries ranges.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    // TEST-HARNESS: used by tests and the lib build; the bin only queries ranges.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = &V> {
        self.map.values()
    }

    /// Records intersecting `[start, end)`, ascending by start.
    pub fn overlapping(&self, start: u64, end: u64) -> impl Iterator<Item = &V> {
        let live = start < end;
        let lead = live
            .then(|| self.map.range(..start).next_back())
            .flatten()
            .map(|(_, vma)| vma)
            .filter(|vma| vma.end() > start);
        let body = live
            .then(|| self.map.range(start..end).map(|(_, vma)| vma))
            .into_iter()
            .flatten();
        lead.into_iter().chain(body)
    }

    pub fn is_covered(&self, start: u64, end: u64) -> bool {
        let mut cursor = start;
        for vma in self.overlapping(start, end) {
            if vma.start() > cursor {
                return false;
            }
            cursor = cursor.max(vma.end());
            if cursor >= end {
                return true;
            }
        }
        false
    }

    /// Drop every record matching `drop_if` (whole-record, no splitting).
    pub fn remove_where(&mut self, mut drop_if: impl FnMut(&V) -> bool) {
        self.map.retain(|_, vma| !drop_if(vma));
    }

    fn overlapping_keys(&self, start: u64, end: u64) -> Vec<u64> {
        self.overlapping(start, end).map(AddressRange::start).collect()
    }

    /// Remove `[start, end)`, shrinking or splitting partially covered records.
    pub fn remove_range(&mut self, start: u64, end: u64) {
        self.update_range(start, end, None::<fn(&mut V)>);
    }

    /// Apply `update` to the part of every record inside `[start, end)`,
    /// splitting records that straddle a boundary.
    pub fn modify_range(&mut self, start: u64, end: u64, update: impl FnMut(&mut V)) {
        self.update_range(start, end, Some(update));
    }

    fn update_range(&mut self, start: u64, end: u64, mut update: Option<impl FnMut(&mut V)>) {
        for key in self.overlapping_keys(start, end) {
            let Some(vma) = self.map.remove(&key) else { continue };
            if vma.start() < start {
                let mut left = vma.clone();
                left.set_end(start);
                self.map.insert(left.start(), left);
            }
            if vma.end() > end {
                let mut right = vma.clone();
                right.set_start(end);
                self.map.insert(right.start(), right);
            }
            if let Some(update) = update.as_mut() {
                let mut mid = vma;
                if mid.start() < start {
                    mid.set_start(start);
                }
                if mid.end() > end {
                    mid.set_end(end);
                }
                update(&mut mid);
                self.map.insert(mid.start(), mid);
            }
        }
    }

    /// Insert `vma`, replacing whatever it overlaps.
    pub fn insert_replacing(&mut self, vma: V) {
        self.remove_range(vma.start(), vma.end());
        self.map.insert(vma.start(), vma);
    }

    /// Insert `vma` only into the parts of its range no record occupies.
    pub fn insert_in_gaps(&mut self, vma: V) {
        let (start, end) = (vma.start(), vma.end());
        let mut gaps = Vec::new();
        let mut cursor = start;
        for live in self.overlapping(start, end) {
            if live.start() > cursor {
                gaps.push((cursor, live.start()));
            }
            cursor = cursor.max(live.end());
        }
        if cursor < end {
            gaps.push((cursor, end));
        }
        for (gap_start, gap_end) in gaps {
            let mut piece = vma.clone();
            piece.set_start(gap_start);
            piece.set_end(gap_end);
            self.map.insert(gap_start, piece);
        }
    }
}

impl<V: VmaEntry> ConflictIndex for VmaMap<V> {
    fn first_conflict_end(&self, start: u64, end: u64) -> Option<u64> {
        self.overlapping(start, end).next().map(AddressRange::end)
    }
}

/// munmap as one transaction: the kernel revocation of the whole interval runs
/// first, and policy bookkeeping is removed only after it succeeded. A hole in
/// the policy table is still a valid unmap (legacy leaves may sit there), so
/// the revocation always runs.
pub fn unmap_range_with<S, E>(
    state: &mut S,
    start: u64,
    end: u64,
    revoke: impl FnOnce(u64, u64) -> Result<(), E>,
    remove_policy: impl FnOnce(&mut S, u64, u64),
) -> Result<(), E> {
    revoke(start, end)?;
    remove_policy(state, start, end);
    Ok(())
}

pub fn next_fit_with_wrap<I: ConflictIndex + ?Sized>(
    ranges: &I,
    lower_bound: u64,
    upper_bound: u64,
    hint: u64,
    len: u64,
    alignment: u64,
) -> Option<u64> {
    let lower_bound = align_up(lower_bound, alignment)?;
    let start = align_up(hint, alignment)?.max(lower_bound);
    if start >= upper_bound {
        return find_between(ranges, lower_bound, upper_bound, len, alignment);
    }
    find_between(ranges, start, upper_bound, len, alignment).or_else(|| {
        (start > lower_bound)
            .then(|| find_between(ranges, lower_bound, start, len, alignment))
            .flatten()
    })
}

fn find_between<I: ConflictIndex + ?Sized>(
    ranges: &I,
    mut cursor: u64,
    limit: u64,
    len: u64,
    alignment: u64,
) -> Option<u64> {
    loop {
        let end = cursor.checked_add(len)?;
        if end > limit {
            return None;
        }
        if let Some(conflict_end) = ranges.first_conflict_end(cursor, end) {
            cursor = align_up(conflict_end, alignment)?;
            continue;
        }
        return Some(cursor);
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    debug_assert!(alignment.is_power_of_two());
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn sparse_munmap_is_one_transaction_and_commits_policy_only_on_success() {
        use super::unmap_range_with;
        // (start, end) policy intervals; an empty one is (0, 0).
        let mut policy = [(0x4000_u64, 0x5000_u64), (0x6000, 0x7000)];
        let remove = |p: &mut [(u64, u64); 2], start: u64, end: u64| {
            for range in p.iter_mut() {
                if start < range.1 && range.0 < end {
                    *range = (0, 0);
                }
            }
        };
        let mut calls = 0;
        let refused = unmap_range_with(
            &mut policy,
            0x4000,
            0x7000,
            |start, end| {
                calls += 1;
                assert_eq!((start, end), (0x4000, 0x7000));
                Err(12)
            },
            remove,
        );
        assert_eq!(refused, Err(12));
        assert_eq!(calls, 1, "the whole interval is one revocation");
        assert_eq!(policy, [(0x4000, 0x5000), (0x6000, 0x7000)], "a refusal keeps policy");
        assert_eq!(
            unmap_range_with(&mut policy, 0x4000, 0x7000, |_, _| Ok::<(), i32>(()), remove),
            Ok(())
        );
        assert_eq!(policy, [(0, 0), (0, 0)]);
        // A repeated unmap of an existing hole still reaches the revocation.
        let mut reached = false;
        assert_eq!(
            unmap_range_with(
                &mut policy,
                0x4000,
                0x7000,
                |_, _| {
                    reached = true;
                    Ok::<(), i32>(())
                },
                remove
            ),
            Ok(())
        );
        assert!(reached);
    }

    use super::*;

    #[derive(Clone, Copy)]
    struct Range(u64, u64);

    impl AddressRange for Range {
        fn start(&self) -> u64 {
            self.0
        }

        fn end(&self) -> u64 {
            self.1
        }
    }

    #[test]
    fn next_fit_wraps_cursor_and_reuses_a_freed_gap() {
        let ranges = [Range(0x2000, 0x4000), Range(0x8000, 0x10_000)];
        assert_eq!(
            next_fit_with_wrap(&ranges[..], 0x2000, 0x10_000, 0xf000, 0x3000, 0x1000),
            Some(0x4000)
        );
    }

    #[test]
    fn next_fit_never_crosses_live_ranges_or_wrap_limit() {
        let ranges = [Range(0x4000, 0x8000)];
        assert_eq!(
            next_fit_with_wrap(&ranges[..], 0x2000, 0xa000, 0x3000, 0x3000, 0x1000),
            None
        );
    }

    #[test]
    fn out_of_range_hint_wraps_only_inside_the_user_window() {
        let ranges = [Range(0x2000, 0x4000)];
        assert_eq!(
            next_fit_with_wrap(&ranges[..], 0x2000, 0x10_000, 0x20_000, 0x2000, 0x1000),
            Some(0x4000)
        );
    }

    // ---- ordered map vs the retired vector implementation ----

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Vma {
        start: u64,
        end: u64,
        tag: u8,
    }

    impl AddressRange for Vma {
        fn start(&self) -> u64 {
            self.start
        }
        fn end(&self) -> u64 {
            self.end
        }
    }

    impl VmaEntry for Vma {
        fn set_start(&mut self, start: u64) {
            self.start = start;
        }
        fn set_end(&mut self, end: u64) {
            self.end = end;
        }
    }

    // The retired Vec-based semantics, kept as the oracle.
    fn ref_remove(v: &mut Vec<Vma>, start: u64, end: u64) {
        let mut updated = Vec::new();
        for vma in v.drain(..) {
            if end <= vma.start || start >= vma.end {
                updated.push(vma);
                continue;
            }
            if start > vma.start {
                updated.push(Vma { end: start, ..vma });
            }
            if end < vma.end {
                updated.push(Vma { start: end, ..vma });
            }
        }
        updated.sort_by_key(|vma| vma.start);
        *v = updated;
    }

    fn ref_insert(v: &mut Vec<Vma>, vma: Vma) {
        ref_remove(v, vma.start, vma.end);
        v.push(vma);
        v.sort_by_key(|vma| vma.start);
    }

    fn ref_set_tag(v: &mut Vec<Vma>, start: u64, end: u64, tag: u8) {
        let mut updated = Vec::new();
        for vma in v.drain(..) {
            if end <= vma.start || start >= vma.end {
                updated.push(vma);
                continue;
            }
            if start > vma.start {
                updated.push(Vma { end: start, ..vma });
            }
            updated.push(Vma { start: vma.start.max(start), end: vma.end.min(end), tag, ..vma });
            if end < vma.end {
                updated.push(Vma { start: end, ..vma });
            }
        }
        updated.sort_by_key(|vma| vma.start);
        *v = updated;
    }

    fn ref_covered(v: &[Vma], start: u64, end: u64) -> bool {
        let mut cursor = start;
        let mut hits: Vec<_> = v.iter().filter(|x| x.end > start && x.start < end).collect();
        hits.sort_by_key(|x| x.start);
        for vma in hits {
            if vma.start > cursor {
                return false;
            }
            cursor = cursor.max(vma.end);
            if cursor >= end {
                return true;
            }
        }
        false
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn ordered_map_matches_vector_semantics_under_random_operations() {
        const PAGE: u64 = 0x1000;
        for seed in 1..=40_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut reference: Vec<Vma> = Vec::new();
            let mut map: VmaMap<Vma> = VmaMap::new();
            for step in 0..400_u64 {
                let start = (1 + rng.below(60)) * PAGE;
                let end = start + (1 + rng.below(12)) * PAGE;
                match rng.below(5) {
                    0 | 1 => {
                        let vma = Vma { start, end, tag: (step % 250) as u8 };
                        ref_insert(&mut reference, vma);
                        map.insert_replacing(vma);
                    }
                    2 => {
                        ref_remove(&mut reference, start, end);
                        map.remove_range(start, end);
                    }
                    3 => {
                        let tag = rng.below(250) as u8;
                        ref_set_tag(&mut reference, start, end, tag);
                        map.modify_range(start, end, |vma| vma.tag = tag);
                    }
                    _ => {
                        let tag = rng.below(250) as u8;
                        reference.retain(|vma| vma.tag != tag);
                        map.remove_where(|vma| vma.tag == tag);
                    }
                }
                let live: Vec<Vma> = map.iter().copied().collect();
                assert_eq!(live, reference, "seed {seed} step {step}");
                assert_eq!(live.len(), map.len());
                assert!(live.windows(2).all(|w| w[0].end <= w[1].start), "overlap");
                let (qs, qe) = (start, end);
                let got: Vec<Vma> = map.overlapping(qs, qe).copied().collect();
                let want: Vec<Vma> =
                    reference.iter().copied().filter(|x| x.end > qs && x.start < qe).collect();
                assert_eq!(got, want, "overlapping seed {seed} step {step}");
                assert_eq!(map.is_covered(qs, qe), ref_covered(&reference, qs, qe));
                let len = (1 + rng.below(8)) * PAGE;
                let hint = rng.below(80) * PAGE;
                assert_eq!(
                    next_fit_with_wrap(&map, PAGE, 70 * PAGE, hint, len, PAGE),
                    next_fit_with_wrap(&reference[..], PAGE, 70 * PAGE, hint, len, PAGE),
                    "next-fit seed {seed} step {step}"
                );
            }
        }
    }

    #[test]
    fn heap_insert_in_gaps_keeps_existing_records() {
        let mut map: VmaMap<Vma> = VmaMap::new();
        map.insert_replacing(Vma { start: 0x3000, end: 0x4000, tag: 1 });
        map.insert_in_gaps(Vma { start: 0x1000, end: 0x6000, tag: 9 });
        let live: Vec<_> = map.iter().map(|v| (v.start, v.end, v.tag)).collect();
        assert_eq!(live, [(0x1000, 0x3000, 9), (0x3000, 0x4000, 1), (0x4000, 0x6000, 9)]);
    }

    #[test]
    fn empty_and_inverted_queries_touch_nothing() {
        let mut map: VmaMap<Vma> = VmaMap::new();
        map.insert_replacing(Vma { start: 0x1000, end: 0x3000, tag: 1 });
        assert_eq!(map.overlapping(0x2000, 0x2000).count(), 0);
        assert_eq!(map.overlapping(0x3000, 0x1000).count(), 0);
        map.remove_range(0x2000, 0x2000);
        assert_eq!(map.len(), 1);
        assert!(!map.is_covered(0x2000, 0x2000) || map.len() == 1);
    }
}
