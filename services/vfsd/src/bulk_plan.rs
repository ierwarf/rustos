//! Sizing and chunk-claim rules for vfsd's parallel storaged read lanes
//! (`bulk_lanes`).
//!
//! Pure arithmetic and atomics kept in the library so they are unit-tested; the
//! lanes themselves (threads, futexes, storaged IPC) run in the service binary.
//!
//! Read-ahead is not decided here: a sequential stream is overlapped by the
//! snapshot pipeline (`snapshot_pipeline`), which keeps whole windows in flight
//! in its own bounded ring. A lane job is always exactly the caller's range.

use core::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};

/// Reads below this stay on the calling thread: a lane wake-up costs more than
/// the few calls it would overlap. 512 KiB is 8 storaged bulk calls (2 per
/// lane across 4 lanes), matching `bulk_read_worker`'s 512 KiB window.
pub const PARALLEL_MIN_BYTES: usize = 512 * 1024;

const EIO: i32 = 5;

/// Whether a read of `len` bytes is worth splitting across lanes.
pub fn is_parallel_read(block_size: usize, chunk_blocks: u64, len: usize) -> bool {
    block_size != 0 && chunk_blocks != 0 && len >= PARALLEL_MIN_BYTES
}

/// Bytes of one storaged call, or `None` on overflow / a zero geometry.
pub fn chunk_bytes(block_size: usize, chunk_blocks: u64) -> Option<usize> {
    let blocks = usize::try_from(chunk_blocks).ok()?;
    match block_size.checked_mul(blocks)? {
        0 => None,
        bytes => Some(bytes),
    }
}

/// Number of storaged calls that cover `len` bytes.
pub fn chunk_count(len: usize, chunk_bytes: usize) -> usize {
    if chunk_bytes == 0 {
        0
    } else {
        len.div_ceil(chunk_bytes)
    }
}

/// Byte offset and length of chunk `index`; the last chunk may be short.
pub fn chunk_span(index: usize, chunk_bytes: usize, len: usize) -> (usize, usize) {
    let offset = index * chunk_bytes;
    (offset, chunk_bytes.min(len - offset))
}

/// Shared progress of one lane job: which chunk to read next, how many chunks
/// have completed, and the first error.
///
/// Every chunk is claimed exactly once. A claimed chunk is always completed,
/// even after an error (the lane then skips the read), so `finished` means
/// "every claimed chunk is done" and the owner may release the buffer.
pub struct ChunkCursor {
    chunks: usize,
    next: AtomicUsize,
    done: AtomicU32,
    error: AtomicI32,
}

impl ChunkCursor {
    /// `None` if the chunk count does not fit the 32-bit futex word.
    pub fn new(chunks: usize) -> Option<Self> {
        u32::try_from(chunks).ok()?;
        Some(Self {
            chunks,
            next: AtomicUsize::new(0),
            done: AtomicU32::new(0),
            error: AtomicI32::new(0),
        })
    }

    pub fn chunks(&self) -> usize {
        self.chunks
    }

    /// Whether some chunk has not been claimed yet (a hint for idle helpers).
    pub fn has_unclaimed(&self) -> bool {
        self.next.load(Ordering::Acquire) < self.chunks
    }

    /// Claims the next unclaimed chunk index.
    pub fn claim(&self) -> Option<usize> {
        if self.next.load(Ordering::Relaxed) >= self.chunks {
            return None;
        }
        // `chunks` is bounded by `u32::MAX` (checked in `new`), so on 64-bit a
        // single atomic `fetch_add` cannot wrap even under high contention and
        // avoids CAS retry loops across parallel lanes.
        let index = self.next.fetch_add(1, Ordering::AcqRel);
        (index < self.chunks).then_some(index)
    }

    /// The first error recorded, or 0.
    pub fn error(&self) -> i32 {
        self.error.load(Ordering::Acquire)
    }

    /// Records one claimed chunk as finished; returns whether it was the last.
    ///
    /// ORDERING: the AcqRel increment publishes this chunk's bytes (written by
    /// the storaged reply copy) to whoever Acquires `done_word`.
    pub fn complete(&self, result: Result<(), i32>) -> bool {
        if let Err(errno) = result {
            let errno = if errno == 0 { EIO } else { errno };
            let _ = self
                .error
                .compare_exchange(0, errno, Ordering::AcqRel, Ordering::Relaxed);
        }
        self.done.fetch_add(1, Ordering::AcqRel) as usize + 1 == self.chunks
    }

    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire) as usize >= self.chunks
    }

    /// The completion word, for a futex wait by the job owner.
    pub fn done_word(&self) -> &AtomicU32 {
        &self.done
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const BS: usize = 512;

    #[test]
    fn only_large_well_formed_reads_use_lanes() {
        assert!(is_parallel_read(BS, 127, PARALLEL_MIN_BYTES));
        assert!(!is_parallel_read(BS, 127, PARALLEL_MIN_BYTES - BS));
        assert!(!is_parallel_read(0, 127, PARALLEL_MIN_BYTES));
        assert!(!is_parallel_read(BS, 0, PARALLEL_MIN_BYTES));
    }

    #[test]
    fn chunk_geometry_covers_the_range_exactly_once() {
        assert_eq!(chunk_bytes(BS, 127), Some(127 * BS));
        assert_eq!(chunk_bytes(0, 127), None);
        assert_eq!(chunk_bytes(BS, 0), None);
        assert_eq!(chunk_bytes(usize::MAX, 2), None);
        let chunk = 127 * BS;
        let len = 4 * 1024 * 1024;
        let count = chunk_count(len, chunk);
        assert_eq!(count, 65);
        let mut covered = 0;
        for index in 0..count {
            let (offset, span) = chunk_span(index, chunk, len);
            assert_eq!(offset, covered);
            assert!(span > 0 && span <= chunk);
            covered += span;
        }
        assert_eq!(covered, len);
        assert_eq!(chunk_count(len, 0), 0);
    }

    #[test]
    fn cursor_claims_each_chunk_once_and_finishes_after_errors() {
        let cursor = ChunkCursor::new(3).unwrap();
        assert_eq!(cursor.claim(), Some(0));
        assert!(!cursor.complete(Ok(())));
        assert_eq!(cursor.claim(), Some(1));
        assert!(!cursor.complete(Err(7)));
        assert_eq!(cursor.error(), 7);
        assert!(cursor.has_unclaimed());
        // A claimed chunk after an error is still completed (without a read).
        assert_eq!(cursor.claim(), Some(2));
        assert!(!cursor.finished());
        assert!(cursor.complete(Err(9)));
        assert!(cursor.finished());
        // First error wins.
        assert_eq!(cursor.error(), 7);
        assert_eq!(cursor.claim(), None);
        assert!(!cursor.has_unclaimed());
        assert!(ChunkCursor::new(u32::MAX as usize + 1).is_none());
    }

    #[test]
    fn cursor_hands_out_disjoint_chunks_across_threads() {
        let cursor = ChunkCursor::new(1000).unwrap();
        let seen: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let mut mine = Vec::new();
                        while let Some(index) = cursor.claim() {
                            mine.push(index);
                            cursor.complete(Ok(()));
                        }
                        mine
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect()
        });
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..1000).collect::<Vec<_>>());
        assert!(cursor.finished());
        assert_eq!(cursor.error(), 0);
    }

    #[test]
    fn half_mib_bulk_reads_and_four_mib_snapshot_windows_use_parallel_lanes() {
        assert!(is_parallel_read(BS, 127, 512 * 1024));
        assert!(is_parallel_read(BS, 127, 4 * 1024 * 1024));
        let chunk = chunk_bytes(BS, 127).unwrap();
        assert_eq!(chunk, 65_024);
        assert_eq!(chunk_count(512 * 1024, chunk), 9);
        assert_eq!(chunk_count(4 * 1024 * 1024, chunk), 65);
        // Repeated claim attempts past exhaustion keep returning None cleanly.
        let cursor = ChunkCursor::new(4).unwrap();
        for expected in 0..4 {
            assert_eq!(cursor.claim(), Some(expected));
        }
        for _ in 0..16 {
            assert_eq!(cursor.claim(), None);
        }
    }
}
