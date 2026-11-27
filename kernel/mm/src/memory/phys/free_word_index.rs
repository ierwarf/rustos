//! Exact index of central bitmap words containing at least one free frame.
//!
//! - **Owner:** `kernel-mm` central physical frame allocator owns the multi-level
//!   summary bitmaps placed right after the reserved frame bitmaps.
//! - **Boundary:** Tracks only central `used` bitmap word emptiness/non-emptiness
//!   for `DIRECT_MAP_PHYS_LIMIT`.
//! - **Lifecycle:** Initialized before heap startup from central bitmap state
//!   and updated monotonically as bitmap words transition empty <-> non-empty.
//! - **Concurrency:** Accessed solely while holding the IRQ-off central frame
//!   allocator lock; lock-free caches never touch this index.
//! - **Failure:** Out-of-range word indices are clamped/rejected and empty levels
//!   return `None` without skipping allocatable low/high words.
//! - **Forbidden:** Unsynchronized concurrent mutation outside central lock or
//!   stale set bits pointing at full bitmap words.
//! - **Evidence:** `kernel-mm` `free_word_index` unit tests and allocator invariants.

use super::{BITS_PER_WORD, BitmapView, DIRECT_MAP_PHYS_LIMIT, PAGE_SIZE};

const MAX_LEVELS: usize = 4;
const _: () = assert!(
    DIRECT_MAP_PHYS_LIMIT / PAGE_SIZE / BITS_PER_WORD as u64
        <= (BITS_PER_WORD as u64).pow(MAX_LEVELS as u32)
);

pub(super) struct FreeWordIndex {
    levels: [BitmapView; MAX_LEVELS],
    level_count: usize,
}

impl FreeWordIndex {
    pub(super) const EMPTY: Self = Self {
        levels: [BitmapView::EMPTY; MAX_LEVELS],
        level_count: 0,
    };

    pub(super) fn backing_words(mut bitmap_words: usize) -> Option<usize> {
        let mut total = 0_usize;
        for _ in 0..MAX_LEVELS {
            if bitmap_words == 0 {
                return Some(total);
            }
            bitmap_words = bitmap_words.div_ceil(BITS_PER_WORD);
            total = total.checked_add(bitmap_words)?;
            if bitmap_words == 1 {
                return Some(total);
            }
        }
        None
    }

    /// # Safety
    /// `backing` names at least `backing_words(bitmap_words)` aligned, zeroed
    /// words that outlive the allocator and have no other mutable owner.
    pub(super) unsafe fn from_raw(backing: *mut u64, mut bitmap_words: usize) -> Self {
        let required = Self::backing_words(bitmap_words).expect("physical free-word index too large");
        let mut index = Self::EMPTY;
        let mut offset = 0;
        while bitmap_words != 0 {
            bitmap_words = bitmap_words.div_ceil(BITS_PER_WORD);
            // SAFETY: the caller supplied the complete zeroed backing span;
            // each level is a disjoint prefix of the remaining words.
            index.levels[index.level_count] =
                BitmapView::from_raw(unsafe { backing.add(offset) }, bitmap_words);
            index.level_count += 1;
            offset += bitmap_words;
            if bitmap_words == 1 {
                break;
            }
        }
        assert_eq!(offset, required);
        index
    }

    /// Update only when a central word changes between full and non-full.
    pub(super) fn set_available(&mut self, mut bitmap_word: usize, mut available: bool) {
        for level in &self.levels[..self.level_count] {
            let word_index = bitmap_word / BITS_PER_WORD;
            let mask = 1_u64 << (bitmap_word % BITS_PER_WORD);
            let before = level.load_word(word_index);
            let after = if available { before | mask } else { before & !mask };
            if before == after {
                break;
            }
            level.store_word(word_index, after);
            // The parent records only whether this whole summary word is
            // nonempty. Changing any other bit does not touch higher levels.
            if (before == 0) == (after == 0) {
                break;
            }
            bitmap_word = word_index;
            available = after != 0;
        }
    }

    /// Returns the first non-full central word at or after `first`.
    /// At most `2 * MAX_LEVELS - 1` summary words are read, independent of the
    /// number of full central words between the cursor and the answer.
    pub(super) fn next_available(&self, mut first: usize, words_read: &mut usize) -> Option<usize> {
        let mut depth = 0;
        while depth < self.level_count {
            let level = self.levels[depth];
            let word_index = first / BITS_PER_WORD;
            if word_index >= level.words {
                return None;
            }
            *words_read += 1;
            let candidates = level.load_word(word_index) & (u64::MAX << (first % BITS_PER_WORD));
            if candidates != 0 {
                let mut found = word_index * BITS_PER_WORD + candidates.trailing_zeros() as usize;
                while depth != 0 {
                    depth -= 1;
                    *words_read += 1;
                    let children = self.levels[depth].load_word(found);
                    assert_ne!(children, 0, "free-word index points to an empty child");
                    found = found * BITS_PER_WORD + children.trailing_zeros() as usize;
                }
                return Some(found);
            }
            // The suffix of this word is empty; its parent must find the next
            // nonempty word, not return this word's earlier set bits again.
            first = word_index + 1;
            depth += 1;
        }
        None
    }
}

/// Number of sub-word/cross-word run orders tracked over central bitmap words:
/// orders 0..=6 correspond to runs of length `>= 1, 2, 4, 8, 16, 32, 64`.
pub(super) const RUN_ORDERS: usize = 7;
pub(super) const MAX_RUN_ORDER: usize = RUN_ORDERS - 1;

/// Thresholds of consecutive completely-free central words (`used[w] == 0`)
/// indexed by `all_free_orders`. Order 3 (`7` words) is the exact minimum
/// full-word core required by any 512-page (2 MiB) contiguous run.
const ALL_FREE_THRESHOLDS: [usize; 5] = [1, 2, 4, 7, 16];

/// Returns `(order_index, threshold_words)` for the tightest tracked all-free
/// threshold `<= min_full_words`. Requires `min_full_words >= 1`.
#[inline]
pub(super) const fn all_free_order_for(min_full_words: usize) -> (usize, usize) {
    if min_full_words >= 16 {
        (4, 16)
    } else if min_full_words >= 7 {
        (3, 7)
    } else if min_full_words >= 4 {
        (2, 4)
    } else if min_full_words >= 2 {
        (1, 2)
    } else {
        (0, 1)
    }
}

/// Bitmask of orders `0..RUN_ORDERS` for which at least one free run of length
/// `>= (1 << order)` starts at some bit `0..64` of the current word.
#[inline]
fn run_order_mask(free_w: u64, free_next: u64) -> u8 {
    if free_w == 0 {
        return 0;
    }
    let r1 = (free_w as u128) | ((free_next as u128) << BITS_PER_WORD);
    let r2 = r1 & (r1 >> 1);
    if (r2 as u64) == 0 {
        return 0b0000_0001;
    }
    let r4 = r2 & (r2 >> 2);
    if (r4 as u64) == 0 {
        return 0b0000_0011;
    }
    let r8 = r4 & (r4 >> 4);
    if (r8 as u64) == 0 {
        return 0b0000_0111;
    }
    let r16 = r8 & (r8 >> 8);
    if (r16 as u64) == 0 {
        return 0b0000_1111;
    }
    let r32 = r16 & (r16 >> 16);
    if (r32 as u64) == 0 {
        return 0b0001_1111;
    }
    let r64 = r32 & (r32 >> 32);
    if (r64 as u64) == 0 {
        return 0b0011_1111;
    }
    0b0111_1111
}

/// Bit `i` (for `0 <= i < 64`) of the returned `u64` is 1 iff bits `[i, i + k)`
/// of `(free_w | (free_next << 64))` are all 1. Requires `1 <= k <= 64`.
#[inline]
pub(super) fn exact_run_starts_in_word(free_w: u64, free_next: u64, k: usize) -> u64 {
    debug_assert!((1..=BITS_PER_WORD).contains(&k));
    let mut acc = u128::MAX;
    let mut pow = (free_w as u128) | ((free_next as u128) << BITS_PER_WORD);
    let mut rem = k;
    let mut shift = 0_u32;
    let mut step = 1_u32;
    while rem != 0 {
        if rem & 1 != 0 {
            acc &= pow >> shift;
            shift += step;
        }
        rem >>= 1;
        if rem != 0 {
            pow &= pow >> step;
            step <<= 1;
        }
    }
    acc as u64
}

/// Bitmask of `ALL_FREE_THRESHOLDS` entries for which at least one run of that
/// many consecutive all-free central words starts inside `af_w`.
#[inline]
fn all_free_order_mask(af_w: u64, af_next: u64) -> u8 {
    if af_w == 0 {
        return 0;
    }
    let r1 = (af_w as u128) | ((af_next as u128) << BITS_PER_WORD);
    let r2 = r1 & (r1 >> 1);
    if (r2 as u64) == 0 {
        return 0b00001;
    }
    let r4 = r2 & (r2 >> 2);
    if (r4 as u64) == 0 {
        return 0b00011;
    }
    let r7 = r4 & (r2 >> 4) & (r1 >> 6);
    if (r7 as u64) == 0 {
        return 0b00111;
    }
    let r8 = r4 & (r4 >> 4);
    let r16 = r8 & (r8 >> 8);
    if (r16 as u64) == 0 {
        return 0b01111;
    }
    0b11111
}

/// Bit `i` (for `0 <= i < 64`) is 1 iff at least `ALL_FREE_THRESHOLDS[order]`
/// consecutive all-free words start at bit `i` of `af_w`.
#[inline]
fn all_free_starts_in_word(af_w: u64, af_next: u64, order: usize) -> u64 {
    let r1 = (af_w as u128) | ((af_next as u128) << BITS_PER_WORD);
    match order {
        0 => r1 as u64,
        1 => (r1 & (r1 >> 1)) as u64,
        2 => {
            let r2 = r1 & (r1 >> 1);
            (r2 & (r2 >> 2)) as u64
        }
        3 => {
            let r2 = r1 & (r1 >> 1);
            let r4 = r2 & (r2 >> 2);
            (r4 & (r2 >> 4) & (r1 >> 6)) as u64
        }
        4 => {
            let r2 = r1 & (r1 >> 1);
            let r4 = r2 & (r2 >> 2);
            let r8 = r4 & (r4 >> 4);
            (r8 & (r8 >> 8)) as u64
        }
        _ => unreachable!("all-free order out of range"),
    }
}

/// Multi-order hierarchical index over the central `used` bitmap.
///
/// - `orders[0..=6]` track central bitmap words where a contiguous free run of
///   length `>= 1, 2, 4, 8, 16, 32, 64` starts (including runs crossing into
///   the immediately following word). `orders[0]` is the exact non-full-word
///   index used by single-frame allocation and cache refill.
/// - `all_free` is a 1-bit-per-word bitmap of completely free central words
///   (`used[w] == 0`), indexed by `all_free_orders[0..=4]` for consecutive runs
///   of `1, 2, 4, 7, 16` all-free words so large contiguous searches (including
///   512-page / 2 MiB runs) skip fragmented islands in `O(log N)`.
pub(super) struct FreeRunIndex {
    orders: [FreeWordIndex; RUN_ORDERS],
    all_free: BitmapView,
    all_free_orders: [FreeWordIndex; ALL_FREE_THRESHOLDS.len()],
}

impl FreeRunIndex {
    pub(super) const EMPTY: Self = Self {
        orders: [FreeWordIndex::EMPTY; RUN_ORDERS],
        all_free: BitmapView::EMPTY,
        all_free_orders: [FreeWordIndex::EMPTY; ALL_FREE_THRESHOLDS.len()],
    };

    pub(super) fn backing_words(bitmap_words: usize) -> Option<usize> {
        let per_order = FreeWordIndex::backing_words(bitmap_words)?;
        let orders_total = per_order.checked_mul(RUN_ORDERS)?;
        let all_free_words = bitmap_words.div_ceil(BITS_PER_WORD);
        let per_af_order = FreeWordIndex::backing_words(all_free_words)?;
        let af_orders_total = per_af_order.checked_mul(ALL_FREE_THRESHOLDS.len())?;
        orders_total
            .checked_add(all_free_words)?
            .checked_add(af_orders_total)
    }

    /// # Safety
    /// `backing` names at least `backing_words(bitmap_words)` aligned, zeroed
    /// words that outlive the allocator and have no other mutable owner.
    pub(super) unsafe fn from_raw(backing: *mut u64, bitmap_words: usize) -> Self {
        let required = Self::backing_words(bitmap_words).expect("physical free-run index too large");
        let per_order = FreeWordIndex::backing_words(bitmap_words)
            .expect("physical free-word index too large");
        let all_free_words = bitmap_words.div_ceil(BITS_PER_WORD);
        let per_af_order = FreeWordIndex::backing_words(all_free_words)
            .expect("physical all-free index too large");
        let mut index = Self::EMPTY;
        let mut offset = 0;
        for order in 0..RUN_ORDERS {
            // SAFETY: disjoint prefix of the caller's zeroed backing span.
            index.orders[order] =
                unsafe { FreeWordIndex::from_raw(backing.add(offset), bitmap_words) };
            offset += per_order;
        }
        // SAFETY: disjoint prefix of the caller's zeroed backing span.
        index.all_free = BitmapView::from_raw(unsafe { backing.add(offset) }, all_free_words);
        offset += all_free_words;
        for af_order in 0..ALL_FREE_THRESHOLDS.len() {
            // SAFETY: disjoint prefix of the caller's zeroed backing span.
            index.all_free_orders[af_order] =
                unsafe { FreeWordIndex::from_raw(backing.add(offset), all_free_words) };
            offset += per_af_order;
        }
        assert_eq!(offset, required);
        index
    }

    /// Updates all summary levels after `used[word_index]` changed from
    /// `before` to `after`. Only touches higher levels on predicate edges.
    #[inline]
    pub(super) fn on_used_word_changed(
        &mut self,
        used: BitmapView,
        word_index: usize,
        before: u64,
        after: u64,
    ) {
        if before == after {
            return;
        }
        let free_next = if word_index + 1 < used.words {
            !used.load_word(word_index + 1)
        } else {
            0
        };
        let old_mask = run_order_mask(!before, free_next);
        let new_mask = run_order_mask(!after, free_next);
        self.apply_order_diff(word_index, old_mask, new_mask);

        if word_index > 0 {
            let prev_used = used.load_word(word_index - 1);
            if prev_used & (1_u64 << (BITS_PER_WORD - 1)) == 0 {
                let free_prev = !prev_used;
                let old_prev = run_order_mask(free_prev, !before);
                let new_prev = run_order_mask(free_prev, !after);
                self.apply_order_diff(word_index - 1, old_prev, new_prev);
            }
        }

        if (before == 0) != (after == 0) {
            let af_word = word_index / BITS_PER_WORD;
            let af_bit = 1_u64 << (word_index % BITS_PER_WORD);
            let af_before = self.all_free.load_word(af_word);
            let af_after = if after == 0 {
                af_before | af_bit
            } else {
                af_before & !af_bit
            };
            if af_before != af_after {
                self.all_free.store_word(af_word, af_after);
                let af_next = if af_word + 1 < self.all_free.words {
                    self.all_free.load_word(af_word + 1)
                } else {
                    0
                };
                self.apply_af_diff(
                    af_word,
                    all_free_order_mask(af_before, af_next),
                    all_free_order_mask(af_after, af_next),
                );
                if af_word > 0 {
                    let af_prev = self.all_free.load_word(af_word - 1);
                    if af_prev & (1_u64 << (BITS_PER_WORD - 1)) != 0 {
                        self.apply_af_diff(
                            af_word - 1,
                            all_free_order_mask(af_prev, af_before),
                            all_free_order_mask(af_prev, af_after),
                        );
                    }
                }
            }
        }
    }

    #[inline]
    fn apply_order_diff(&mut self, word_index: usize, old_mask: u8, new_mask: u8) {
        let mut diff = old_mask ^ new_mask;
        while diff != 0 {
            let order = diff.trailing_zeros() as usize;
            diff &= diff - 1;
            self.orders[order].set_available(word_index, (new_mask & (1_u8 << order)) != 0);
        }
    }

    #[inline]
    fn apply_af_diff(&mut self, af_word: usize, old_mask: u8, new_mask: u8) {
        let mut diff = old_mask ^ new_mask;
        while diff != 0 {
            let order = diff.trailing_zeros() as usize;
            diff &= diff - 1;
            self.all_free_orders[order]
                .set_available(af_word, (new_mask & (1_u8 << order)) != 0);
        }
    }

    /// Returns the first non-full central word at or after `first` (order 0).
    #[inline]
    pub(super) fn next_available(&self, first: usize, words_read: &mut usize) -> Option<usize> {
        self.orders[0].next_available(first, words_read)
    }

    /// Returns the first central word `>= first` where a free run of length
    /// `>= (1 << order)` starts.
    #[inline]
    pub(super) fn next_word_with_run_order(
        &self,
        order: usize,
        first: usize,
        words_read: &mut usize,
    ) -> Option<usize> {
        self.orders[order].next_available(first, words_read)
    }

    /// Returns the first central word index `>= first_word` that starts a run
    /// of at least `ALL_FREE_THRESHOLDS[af_order]` consecutive all-free words.
    pub(super) fn next_all_free_run(
        &self,
        af_order: usize,
        first_word: usize,
        words_read: &mut usize,
    ) -> Option<usize> {
        let af_word = first_word / BITS_PER_WORD;
        if af_word >= self.all_free.words {
            return None;
        }
        *words_read += 1;
        let af_cur = self.all_free.load_word(af_word);
        let af_next = if af_word + 1 < self.all_free.words {
            *words_read += 1;
            self.all_free.load_word(af_word + 1)
        } else {
            0
        };
        let starts = all_free_starts_in_word(af_cur, af_next, af_order)
            & (u64::MAX << (first_word % BITS_PER_WORD));
        if starts != 0 {
            return Some(af_word * BITS_PER_WORD + starts.trailing_zeros() as usize);
        }
        let next_af_word =
            self.all_free_orders[af_order].next_available(af_word + 1, words_read)?;
        *words_read += 1;
        let cur = self.all_free.load_word(next_af_word);
        let next = if next_af_word + 1 < self.all_free.words {
            *words_read += 1;
            self.all_free.load_word(next_af_word + 1)
        } else {
            0
        };
        let starts = all_free_starts_in_word(cur, next, af_order);
        assert_ne!(starts, 0, "all-free run index names an empty summary word");
        Some(next_af_word * BITS_PER_WORD + starts.trailing_zeros() as usize)
    }
}

#[cfg(test)]
impl FreeRunIndex {
    pub(super) fn assert_matches_bitmap(&self, used: BitmapView) {
        let words = used.words;
        for order in 0..RUN_ORDERS {
            let need = 1_usize << order;
            for first in 0..=words {
                let expected = (first..words).find(|&w| {
                    let free_w = !used.load_word(w);
                    let free_next = if w + 1 < words { !used.load_word(w + 1) } else { 0 };
                    exact_run_starts_in_word(free_w, free_next, need) != 0
                });
                let mut reads = 0;
                assert_eq!(
                    self.orders[order].next_available(first, &mut reads),
                    expected,
                    "order {order} mismatch at first={first}"
                );
                assert!(reads <= 2 * MAX_LEVELS - 1);
            }
        }
        for (af_order, &threshold) in ALL_FREE_THRESHOLDS.iter().enumerate() {
            for first in 0..=words {
                let expected = (first..words).find(|&w| {
                    w + threshold <= words && (w..w + threshold).all(|k| used.load_word(k) == 0)
                });
                let mut reads = 0;
                assert_eq!(
                    self.next_all_free_run(af_order, first, &mut reads),
                    expected,
                    "all-free order {af_order} (threshold={threshold}) mismatch at first={first}"
                );
                assert!(reads <= 2 * MAX_LEVELS + 3);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_for(words: usize) -> FreeWordIndex {
        let storage = alloc::boxed::Box::leak(
            alloc::vec![0_u64; FreeWordIndex::backing_words(words).unwrap()].into_boxed_slice(),
        );
        // SAFETY: this test's index exclusively owns the leaked zeroed words.
        unsafe { FreeWordIndex::from_raw(storage.as_mut_ptr(), words) }
    }

    fn lookup(index: &FreeWordIndex, first: usize) -> Option<usize> {
        let mut words_read = 0;
        let found = index.next_available(first, &mut words_read);
        assert!(words_read <= 2 * MAX_LEVELS - 1, "lookup traversed full-word distance");
        found
    }

    #[test]
    fn sparse_index_crosses_every_level_and_never_wraps_a_query() {
        let words = 1_000_003;
        let mut index = index_for(words);
        let available = [0, 63, 64, 4095, 4096, 262143, 262144, 999999, 1000002];
        for word in available {
            index.set_available(word, true);
        }
        for first in [0, 1, 63, 64, 65, 4095, 4096, 4097, 65536, 262143, 262144, 262145, 999999, 1000000, 1000002, words] {
            assert_eq!(lookup(&index, first), available.into_iter().find(|word| *word >= first));
        }
        for word in available {
            index.set_available(word, false);
            assert_eq!(lookup(&index, word), available.into_iter().find(|next| *next > word));
        }
        assert_eq!(lookup(&index, 0), None);
        index.set_available(words - 1, true);
        assert_eq!(lookup(&index, 0), Some(words - 1));
        assert_eq!(lookup(&index, words), None);
    }

    #[test]
    fn availability_updates_match_an_independent_ordered_set() {
        let words = 10007;
        let mut index = index_for(words);
        let mut available = alloc::collections::BTreeSet::new();
        for step in 0..4096 {
            let word = (step * 7919 + 37) % words;
            if step % 3 == 0 {
                index.set_available(word, false);
                available.remove(&word);
            } else {
                index.set_available(word, true);
                available.insert(word);
            }
            for first in [0, word, (word + 1).min(words), words / 2, words] {
                assert_eq!(lookup(&index, first), available.range(first..).next().copied());
            }
        }
        for word in available {
            index.set_available(word, false);
        }
        assert_eq!(lookup(&index, 0), None);
    }
}
