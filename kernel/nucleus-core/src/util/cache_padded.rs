//! Cache-line isolation for per-CPU and per-slot hot words.
//!
//! A dense `[AtomicUsize; MAX_TRACKED_CPUS]` puts all eight CPUs' words in one
//! 64-byte line, so every CPU-private update invalidates that line on every
//! other CPU. Wrapping each element gives it a line of its own without
//! changing how the word is read or written: `Deref` forwards to the inner
//! value, so `ARRAY[cpu].load(..)` keeps its exact meaning.

use core::ops::{Deref, DerefMut};

#[repr(align(64))]
#[derive(Debug, Default)]
pub struct CachePadded<T>(T);

impl<T> CachePadded<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for CachePadded<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::CachePadded;
    use core::sync::atomic::AtomicU64;

    #[test]
    fn adjacent_elements_never_share_a_cache_line() {
        let cells = [const { CachePadded::new(AtomicU64::new(0)) }; 2];
        let first = &cells[0] as *const _ as usize;
        let second = &cells[1] as *const _ as usize;
        assert_eq!(first % 64, 0);
        assert!(second - first >= 64);
    }
}
