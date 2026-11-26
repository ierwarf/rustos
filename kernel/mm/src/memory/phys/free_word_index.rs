//! Exact index of central bitmap words containing at least one free frame.
//!
//! Every bit at level zero names a non-full `used` word. Each higher level
//! names nonempty words in the level below. The central allocator owns all
//! updates; this index is never read by lock-free cache release. Its backing
//! lives after the two reserved frame bitmaps, before the heap exists.
//!
//! A freed low-address island can rewind the allocation cursor. Once that
//! island is consumed, finding the next free word must not reread thousands
//! of full words while holding the IRQ-off central lock. Lookup ascends and
//! descends this hierarchy instead, without skipping any allocatable frame.

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
