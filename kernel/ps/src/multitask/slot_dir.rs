//! Chunked slot storage with stable addresses that is never freed.
//!
//! - **Owner:** `kernel-ps`, shared by the process table and every per-process
//!   side array that is indexed by the same slot number.
//! - **Boundary:** a fixed top-level directory of chunk pointers. A chunk holds
//!   `SLOT_CHUNK` elements, is allocated in normal context and published once.
//! - **Lifecycle:** chunks are leaked on purpose. A reader holding `&T` can
//!   therefore never see its storage move or disappear, which is what lets the
//!   fault path read these arrays with interrupts off and no lock.
//! - **Concurrency:** `AtomicDir::ensure` publishes with a release CAS; a loser
//!   frees only its own unpublished chunk. `ChunkTable` is plain data guarded by
//!   whatever lock owns it, and growth allocates before that lock is taken.
//! - **Failure:** allocation failure leaves the structure unchanged and is
//!   reported; capacity is bounded by `SLOT_CEILING`, an implementation ceiling,
//!   not a policy value (policy is clamped to it).
//! - **Forbidden:** shrinking, moving a chunk, or growing while a lock that an
//!   interrupt handler needs is held.
//! - **Evidence:** unit tests below; `job-lifecycle` and the process-table
//!   models treat a slot index as stable for the life of the process.
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ops::{Index, IndexMut};
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

/// Elements per chunk.
pub(crate) const SLOT_CHUNK: usize = 32;
/// Chunk pointers in the directory.
pub(crate) const SLOT_DIR: usize = 128;
/// Most slots any directory can ever hold.
pub(crate) const SLOT_CEILING: usize = SLOT_CHUNK * SLOT_DIR;

fn build_chunk<T, const C: usize>(mut make: impl FnMut() -> T) -> Option<Box<[T]>> {
    let mut items = Vec::new();
    items.try_reserve_exact(C).ok()?;
    for _ in 0..C {
        items.push(make());
    }
    Some(items.into_boxed_slice())
}

/// Lock-free, grow-only array of `T`, read with `get` from any context.
pub(crate) struct AtomicDir<T> {
    chunks: [AtomicPtr<T>; SLOT_DIR],
}

impl<T> AtomicDir<T> {
    pub(crate) const fn new() -> Self {
        Self { chunks: [const { AtomicPtr::new(ptr::null_mut()) }; SLOT_DIR] }
    }

    /// The element at `index`, or `None` while its chunk is not yet published.
    #[inline]
    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        let chunk = self.chunks.get(index / SLOT_CHUNK)?;
        // ORDERING: Acquire pairs with the release CAS in `ensure`, so the
        // chunk's initialized elements are visible before the pointer is.
        let base = chunk.load(Ordering::Acquire);
        if base.is_null() {
            return None;
        }
        // SAFETY: `base` is the first element of a leaked `Box<[T]>` of exactly
        // `SLOT_CHUNK` elements that is never freed or moved, and
        // `index % SLOT_CHUNK < SLOT_CHUNK`.
        Some(unsafe { &*base.add(index % SLOT_CHUNK) })
    }

    /// Makes sure the chunk holding `index` exists, allocating it with `make`
    /// in normal context. False means the index is beyond the ceiling or the
    /// allocation failed; nothing changed.
    pub(crate) fn ensure(&self, index: usize, make: impl FnMut() -> T) -> bool {
        let Some(cell) = self.chunks.get(index / SLOT_CHUNK) else {
            return false;
        };
        // ORDERING: Acquire so an already published chunk is fully visible.
        if !cell.load(Ordering::Acquire).is_null() {
            return true;
        }
        let Some(chunk) = build_chunk::<T, SLOT_CHUNK>(make) else {
            return false;
        };
        let base = Box::into_raw(chunk).cast::<T>();
        // ORDERING: AcqRel publishes the initialized chunk to every `get`.
        match cell.compare_exchange(ptr::null_mut(), base, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => true,
            Err(_) => {
                // SAFETY: `base` came from `Box::into_raw` of a boxed slice of
                // `SLOT_CHUNK` elements a moment ago and lost the race, so it
                // was never published and nothing else can reference it.
                drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(base, SLOT_CHUNK)) });
                true
            }
        }
    }

    /// Every element of every published chunk, in index order of the chunks.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.chunks.iter().flat_map(|cell| {
            // ORDERING: Acquire, as in `get`.
            let base = cell.load(Ordering::Acquire);
            let count = if base.is_null() { 0 } else { SLOT_CHUNK };
            (0..count).map(move |offset| {
                // SAFETY: as in `get`; `offset < SLOT_CHUNK`.
                unsafe { &*base.add(offset) }
            })
        })
    }

    /// Slots covered by published chunks (a multiple of `SLOT_CHUNK`).
    pub(crate) fn capacity(&self) -> usize {
        self.chunks
            .iter()
            // ORDERING: Acquire, as in `get`.
            .filter(|cell| !cell.load(Ordering::Acquire).is_null())
            .count()
            * SLOT_CHUNK
    }
}

impl<T> Index<usize> for AtomicDir<T> {
    type Output = T;
    /// For writers that hold a slot index: its chunk was published before the
    /// slot became allocatable, so absence is a broken ordering, not a state.
    fn index(&self, index: usize) -> &T {
        self.get(index).expect("slot chunk is published before its slot is used")
    }
}

/// Grow-only array guarded by its owner's lock. Same stable layout as
/// `AtomicDir`, but plain data, so indexing and mutation need no atomics.
pub(crate) struct ChunkTable<T, const C: usize = SLOT_CHUNK> {
    chunks: [Option<Box<[T]>>; SLOT_DIR],
    len: usize,
}

impl<T, const C: usize> ChunkTable<T, C> {
    pub(crate) const fn new() -> Self {
        Self { chunks: [const { None }; SLOT_DIR], len: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Builds a chunk without touching the table, so the caller can allocate
    /// before it takes the lock that guards this table.
    pub(crate) fn build(make: impl FnMut() -> T) -> Option<Box<[T]>> {
        build_chunk::<T, C>(make)
    }

    /// Installs a chunk built by `build` as the next `C` slots.
    /// Allocation-free. False when the table is at the ceiling.
    pub(crate) fn push_chunk(&mut self, chunk: Box<[T]>) -> bool {
        debug_assert_eq!(chunk.len(), C);
        let Some(cell) = self.chunks.get_mut(self.len / C) else {
            return false;
        };
        *cell = Some(chunk);
        self.len += C;
        true
    }

    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len {
            return None;
        }
        self.chunks[index / C].as_ref()?.get(index % C)
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index >= self.len {
            return None;
        }
        self.chunks[index / C].as_mut()?.get_mut(index % C)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.chunks[..self.len / C]
            .iter()
            .flat_map(|chunk| chunk.iter().flat_map(|chunk| chunk.iter()))
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> + '_ {
        let populated = self.len / C;
        self.chunks[..populated]
            .iter_mut()
            .flat_map(|chunk| chunk.iter_mut().flat_map(|chunk| chunk.iter_mut()))
    }
}

impl<T, const C: usize> Index<usize> for ChunkTable<T, C> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        self.get(index).expect("slot index beyond the grown table")
    }
}

impl<T, const C: usize> IndexMut<usize> for ChunkTable<T, C> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        self.get_mut(index).expect("slot index beyond the grown table")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU64;

    #[test]
    fn an_atomic_dir_publishes_chunks_on_demand_and_never_moves_them() {
        let dir: AtomicDir<AtomicU64> = AtomicDir::new();
        assert!(dir.get(0).is_none());
        assert!(dir.ensure(0, || AtomicU64::new(7)));
        let first = dir.get(5).unwrap() as *const AtomicU64;
        assert_eq!(dir.get(5).unwrap().load(Ordering::Relaxed), 7);
        assert!(dir.get(SLOT_CHUNK).is_none(), "the next chunk is not published yet");
        assert!(dir.ensure(SLOT_CHUNK + 3, || AtomicU64::new(9)));
        assert_eq!(dir.get(SLOT_CHUNK).unwrap().load(Ordering::Relaxed), 9);
        assert_eq!(dir.get(5).unwrap() as *const AtomicU64, first, "growth keeps old addresses");
        assert_eq!(dir.capacity(), 2 * SLOT_CHUNK);
        assert_eq!(dir.iter().count(), 2 * SLOT_CHUNK);
    }

    #[test]
    fn ensure_is_idempotent_and_refuses_beyond_the_ceiling() {
        let dir: AtomicDir<AtomicU64> = AtomicDir::new();
        assert!(dir.ensure(3, || AtomicU64::new(1)));
        let before = dir.get(3).unwrap() as *const AtomicU64;
        assert!(dir.ensure(3, || AtomicU64::new(2)), "a second ensure is a no-op");
        assert_eq!(dir.get(3).unwrap() as *const AtomicU64, before);
        assert_eq!(dir.get(3).unwrap().load(Ordering::Relaxed), 1);
        assert!(!dir.ensure(SLOT_CEILING, || AtomicU64::new(0)));
        assert!(dir.get(SLOT_CEILING).is_none());
    }

    #[test]
    fn a_chunk_table_grows_by_whole_chunks_and_indexes_across_them() {
        let mut table: ChunkTable<u32> = ChunkTable::new();
        type Chunks = ChunkTable<u32>;
        assert_eq!((table.len(), table.get(0)), (0, None));
        let mut next = 0;
        let mut make = || {
            next += 1;
            next
        };
        assert!(table.push_chunk(Chunks::build(&mut make).unwrap()));
        assert!(table.push_chunk(Chunks::build(&mut make).unwrap()));
        assert_eq!(table.len(), 2 * SLOT_CHUNK);
        assert_eq!(table[0], 1);
        assert_eq!(table[SLOT_CHUNK - 1], SLOT_CHUNK as u32);
        assert_eq!(table[SLOT_CHUNK], SLOT_CHUNK as u32 + 1);
        table[SLOT_CHUNK + 1] = 99;
        assert_eq!(table.iter().copied().filter(|&v| v == 99).count(), 1);
        assert_eq!(table.iter().count(), 2 * SLOT_CHUNK);
        assert_eq!(table.iter_mut().count(), 2 * SLOT_CHUNK);
        assert!(table.get(2 * SLOT_CHUNK).is_none());
    }

    #[test]
    fn a_chunk_table_refuses_a_chunk_past_the_ceiling() {
        type Bytes = ChunkTable<u8>;
        let mut table: Bytes = ChunkTable::new();
        for _ in 0..SLOT_DIR {
            assert!(table.push_chunk(Bytes::build(|| 0).unwrap()));
        }
        assert_eq!(table.len(), SLOT_CEILING);
        assert!(!table.push_chunk(Bytes::build(|| 0).unwrap()));
        assert_eq!(table.len(), SLOT_CEILING);
    }
}
