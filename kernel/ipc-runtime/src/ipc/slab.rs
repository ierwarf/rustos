use core::sync::atomic::{AtomicUsize, Ordering};

use nucleus_core::util::lockdep::TrackedSpinLock;

const INDEX_BITS: u32 = 16;
const INDEX_MASK: u64 = (1_u64 << INDEX_BITS) - 1;
const MAX_GENERATION: u64 = u64::MAX >> INDEX_BITS;

struct Slot<T> {
    generation: u64,
    value: Option<T>,
}

#[repr(align(64))]
struct CacheAligned<T>(T);

impl<T> Slot<T> {
    const fn new() -> Self {
        Self {
            generation: 1,
            value: None,
        }
    }

    fn handle(&self, index: usize) -> u64 {
        (self.generation << INDEX_BITS) | (index as u64 + 1)
    }

    fn retire_generation(&mut self) {
        self.generation = if self.generation == MAX_GENERATION {
            // Generation zero is never a valid handle and permanently retires
            // the slot rather than permitting an ABA alias after wrap.
            0
        } else {
            self.generation + 1
        };
    }
}

/// Fixed-capacity object registry with generation-bound handles and one lock
/// per slot. Allocation scans from a rotating hint and never allocates memory.
pub(super) struct GenerationalSlab<T, const N: usize, const CLASS: u8> {
    slots: [CacheAligned<TrackedSpinLock<Slot<T>, CLASS>>; N],
    next_hint: AtomicUsize,
}

impl<T, const N: usize, const CLASS: u8> GenerationalSlab<T, N, CLASS> {
    pub const fn new() -> Self {
        Self {
            slots: [const { CacheAligned(TrackedSpinLock::new(Slot::new())) }; N],
            next_hint: AtomicUsize::new(0),
        }
    }

    #[track_caller]
    pub fn insert(&self, value: T) -> Result<u64, T> {
        assert!(N != 0 && N < INDEX_MASK as usize);
        let start = self.next_hint.fetch_add(1, Ordering::Relaxed) % N;
        let mut value = Some(value);
        for offset in 0..N {
            let index = (start + offset) % N;
            let inserted = self.with_slot(index, |slot| {
                if slot.value.is_some() || slot.generation == 0 {
                    return None;
                }
                let handle = slot.handle(index);
                slot.value = value.take();
                Some(handle)
            });
            let Some(handle) = inserted else {
                continue;
            };
            self.next_hint.store((index + 1) % N, Ordering::Relaxed);
            return Ok(handle);
        }
        Err(value.expect("slab insertion value disappeared without a free slot"))
    }

    #[track_caller]
    pub fn with<R>(&self, handle: u64, f: impl FnOnce(&T) -> R) -> Option<R> {
        let (index, generation) = decode_handle::<N>(handle)?;
        self.with_slot(index, |slot| {
            (slot.generation == generation)
                .then(|| slot.value.as_ref().map(f))
                .flatten()
        })
    }

    #[track_caller]
    pub fn with_mut<R>(&self, handle: u64, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let (index, generation) = decode_handle::<N>(handle)?;
        self.with_slot(index, |slot| {
            (slot.generation == generation)
                .then(|| slot.value.as_mut().map(f))
                .flatten()
        })
    }

    /// Runs `f` against a live slot's value and, when `f` asks for it, retires
    /// the slot under the same acquisition.
    ///
    /// `with_mut` followed by `remove` takes one slot's lock twice to settle a
    /// single decision. The ranked acquisition census measured that pair once
    /// per terminal fast-reply take, and the only lever that moves this
    /// kernel's IPC floor is acquiring fewer locks, not making one cheaper.
    ///
    /// The retired value is returned rather than dropped here, so its
    /// destructor still runs after the slot guard releases -- which is the
    /// property the two-step shape existed to get. Fusing also closes the
    /// window between the two acquisitions, so a decision and the retirement it
    /// authorizes can no longer be split by another CPU.
    ///
    /// `f` must not re-enter this slab: it runs while the slot lock is held.
    #[track_caller]
    pub fn with_mut_take<R>(
        &self,
        handle: u64,
        f: impl FnOnce(&mut T) -> (R, bool),
    ) -> Option<(R, Option<T>)> {
        let (index, generation) = decode_handle::<N>(handle)?;
        self.with_slot(index, |slot| {
            if slot.generation != generation {
                return None;
            }
            let value = slot.value.as_mut()?;
            let (result, retire) = f(value);
            if !retire {
                return Some((result, None));
            }
            let retired = slot.value.take();
            slot.retire_generation();
            Some((result, retired))
        })
    }

    #[track_caller]
    pub fn remove(&self, handle: u64) -> Option<T> {
        let (index, generation) = decode_handle::<N>(handle)?;
        self.with_slot(index, |slot| {
            if slot.generation != generation {
                return None;
            }
            let value = slot.value.take()?;
            slot.retire_generation();
            Some(value)
        })
    }

    #[track_caller]
    pub fn take_first_matching(&self, mut predicate: impl FnMut(&T) -> bool) -> Option<(u64, T)> {
        for index in 0..N {
            let matched = self.with_slot(index, |slot| {
                if slot.value.as_ref().is_none_or(|value| !predicate(value)) {
                    return None;
                }
                let handle = slot.handle(index);
                let value = slot
                    .value
                    .take()
                    .expect("matching slab slot lost its value");
                slot.retire_generation();
                Some((handle, value))
            });
            let Some(matched) = matched else {
                continue;
            };
            return Some(matched);
        }
        None
    }

    #[track_caller]
    pub fn visit_mut(&self, mut visitor: impl FnMut(u64, &mut T)) {
        for index in 0..N {
            self.with_slot(index, |slot| {
                let handle = slot.handle(index);
                if let Some(value) = slot.value.as_mut() {
                    visitor(handle, value);
                }
            });
        }
    }

    #[track_caller]
    pub fn find_handle(&self, mut predicate: impl FnMut(&T) -> bool) -> Option<u64> {
        for index in 0..N {
            let handle = self.with_slot(index, |slot| {
                slot.value
                    .as_ref()
                    .is_some_and(&mut predicate)
                    .then(|| slot.handle(index))
            });
            if handle.is_some() {
                return handle;
            }
        }
        None
    }

    #[cfg(test)]
    pub fn clear(&self) {
        for index in 0..N {
            let value = self.with_slot(index, |slot| {
                let value = slot.value.take();
                if value.is_some() {
                    slot.retire_generation();
                }
                value
            });
            drop(value);
        }
        self.next_hint.store(0, Ordering::Relaxed);
    }

    /// `TrackedSpinLock` disables task preemption while held, so callers must
    /// keep this closure bounded and must not block or allocate. IRQ-sharing
    /// call sites remain responsible for explicitly masking their interrupt
    /// source. The registry itself allocates no memory, and removed values are
    /// returned for destruction only after the guard has been released.
    #[track_caller]
    fn with_slot<R>(&self, index: usize, f: impl FnOnce(&mut Slot<T>) -> R) -> R {
        let mut slot = self.slots[index].0.lock();
        f(&mut slot)
    }
}

fn decode_handle<const N: usize>(handle: u64) -> Option<(usize, u64)> {
    let encoded_index = handle & INDEX_MASK;
    let generation = handle >> INDEX_BITS;
    if encoded_index == 0 || generation == 0 {
        return None;
    }
    let index = usize::try_from(encoded_index - 1).ok()?;
    (index < N).then_some((index, generation))
}

/// Decodes the stable, nonzero slot and generation carried by a valid-shaped
/// slab handle. This is an identity adapter only: callers must still use the
/// slab operation that validates a live object at that generation.
pub(super) fn identity_components<const N: usize>(handle: u64) -> Option<(u64, u64)> {
    let (index, generation) = decode_handle::<N>(handle)?;
    Some((index as u64 + 1, generation))
}

#[cfg(test)]
mod tests {
    use super::GenerationalSlab;

    const TEST_CLASS: u8 = 5;

    #[test]
    fn removed_handle_never_aliases_reused_slot() {
        let slab = GenerationalSlab::<u64, 1, TEST_CLASS>::new();
        let first = slab.insert(11).expect("first insert");
        assert_eq!(slab.remove(first), Some(11));
        let second = slab.insert(22).expect("second insert");
        assert_ne!(first, second);
        assert_eq!(slab.with(first, |value| *value), None);
        assert_eq!(slab.with(second, |value| *value), Some(22));
    }

    #[test]
    fn full_slab_returns_unpublished_value() {
        let slab = GenerationalSlab::<u64, 1, TEST_CLASS>::new();
        let _ = slab.insert(1).expect("first insert");
        assert_eq!(slab.insert(2), Err(2));
    }
}
