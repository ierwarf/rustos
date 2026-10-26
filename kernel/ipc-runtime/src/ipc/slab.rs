use core::sync::atomic::{AtomicUsize, Ordering};

use nucleus_core::util::cache_padded::CachePadded;
use nucleus_core::util::lockdep::{MAX_TRACKED_CPUS, TrackedSpinLock, current_cpu_index};

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
///
/// The rotating cursor is CPU-private: each CPU starts in its own region of
/// the pool. One shared cursor took a contended RMW on a single cache line for
/// every insertion from every CPU, and steered concurrent inserters onto the
/// same slot locks. Every scan still covers the whole pool.
pub(super) struct GenerationalSlab<T, const N: usize, const CLASS: u8> {
    slots: [CacheAligned<TrackedSpinLock<Slot<T>, CLASS>>; N],
    next_hint: [CachePadded<AtomicUsize>; MAX_TRACKED_CPUS],
}

impl<T, const N: usize, const CLASS: u8> GenerationalSlab<T, N, CLASS> {
    pub const fn new() -> Self {
        Self {
            slots: [const { CacheAligned(TrackedSpinLock::new(Slot::new())) }; N],
            next_hint: Self::initial_hints(),
        }
    }

    const fn initial_hints() -> [CachePadded<AtomicUsize>; MAX_TRACKED_CPUS] {
        let mut hints = [const { CachePadded::new(AtomicUsize::new(0)) }; MAX_TRACKED_CPUS];
        let mut cpu = 0;
        while cpu < MAX_TRACKED_CPUS {
            hints[cpu] = CachePadded::new(AtomicUsize::new(cpu * N / MAX_TRACKED_CPUS));
            cpu += 1;
        }
        hints
    }

    fn cpu_hint(&self) -> &AtomicUsize {
        &self.next_hint[current_cpu_index() % MAX_TRACKED_CPUS]
    }

    #[track_caller]
    pub fn insert(&self, value: T) -> Result<u64, T> {
        assert!(N != 0 && N < INDEX_MASK as usize);
        let hint = self.cpu_hint();
        // ORDERING: Relaxed; the cursor is a CPU-private placement hint only.
        let start = hint.load(Ordering::Relaxed) % N;
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
            hint.store((index + 1) % N, Ordering::Relaxed);
            return Ok(handle);
        }
        Err(value.expect("slab insertion value disappeared without a free slot"))
    }

    /// Allocates from an advisory locality hint without updating the shared
    /// round-robin cursor. Every candidate still takes its slot lock and checks
    /// vacancy and generation; the hint grants no object authority. Collisions
    /// scan the full pool, so locality never partitions capacity.
    #[track_caller]
    pub fn insert_with_hint(&self, value: T, hint: u64) -> Result<u64, T> {
        assert!(N != 0 && N < INDEX_MASK as usize);
        let start = (hint % N as u64) as usize;
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
            if let Some(handle) = inserted {
                return Ok(handle);
            }
        }
        Err(value.expect("hinted slab insertion lost its unpublished value"))
    }

    /// [`insert_with_hint`](Self::insert_with_hint) that builds the value inside
    /// the claimed slot, so a large value is written once instead of being moved
    /// through every caller frame. `build` runs under the slot lock and must be
    /// bounded; a full pool never calls it.
    #[track_caller]
    pub fn insert_with_hint_built(&self, hint: u64, build: impl FnOnce() -> T) -> Option<u64> {
        assert!(N != 0 && N < INDEX_MASK as usize);
        let start = (hint % N as u64) as usize;
        let mut build = Some(build);
        for offset in 0..N {
            let index = (start + offset) % N;
            let inserted = self.with_slot(index, |slot| {
                if slot.value.is_some() || slot.generation == 0 {
                    return None;
                }
                let handle = slot.handle(index);
                slot.value = Some((build.take()?)());
                Some(handle)
            });
            if inserted.is_some() {
                return inserted;
            }
        }
        None
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
    /// A value with no destructor has nothing to run after the guard, so it is
    /// retired in place and `None` comes back instead of a moved-out copy.
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
            // A value with no destructor has nothing to run after the guard, so
            // it is retired in place instead of being moved out to be dropped.
            let retired = if core::mem::needs_drop::<T>() {
                slot.value.take()
            } else {
                slot.value = None;
                None
            };
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
        for (cpu, hint) in self.next_hint.iter().enumerate() {
            hint.store(cpu * N / MAX_TRACKED_CPUS, Ordering::Relaxed);
        }
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

/// Returns the slot selected by a generational handle without validating the
/// generation. Callers may use this only to locate advisory side metadata;
/// the slab remains the authority for handle validity.
pub(super) fn slot_index<const N: usize>(handle: u64) -> Option<usize> {
    decode_handle::<N>(handle).map(|(index, _generation)| index)
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

    #[test]
    fn retiring_in_place_returns_a_destructor_bearing_value_but_not_a_plain_one() {
        let plain = GenerationalSlab::<u64, 2, TEST_CLASS>::new();
        let handle = plain.insert_with_hint(7, 0).unwrap();
        let (seen, retired) = plain.with_mut_take(handle, |value| (*value, true)).unwrap();
        assert_eq!((seen, retired), (7, None));
        assert_eq!(plain.with(handle, |value| *value), None, "the slot is retired either way");

        let counted = GenerationalSlab::<std::rc::Rc<()>, 2, TEST_CLASS>::new();
        let marker = std::rc::Rc::new(());
        let handle = counted.insert_with_hint(std::rc::Rc::clone(&marker), 0).unwrap();
        let ((), retired) = counted.with_mut_take(handle, |_| ((), true)).unwrap();
        assert!(retired.is_some(), "a value with a destructor is handed back to run after the guard");
        assert_eq!(std::rc::Rc::strong_count(&marker), 2);
        drop(retired);
        assert_eq!(std::rc::Rc::strong_count(&marker), 1);

        let kept = GenerationalSlab::<u64, 2, TEST_CLASS>::new();
        let handle = kept.insert_with_hint(9, 0).unwrap();
        assert_eq!(kept.with_mut_take(handle, |value| (*value, false)).unwrap(), (9, None));
        assert_eq!(kept.with(handle, |value| *value), Some(9));
    }

    #[test]
    fn hinted_reuse_changes_generation_without_touching_global_cursor() {
        use core::sync::atomic::Ordering;
        let slab = GenerationalSlab::<u64, 4, TEST_CLASS>::new();
        let mut previous = None;
        for value in 0..128 {
            let handle = slab.insert_with_hint(value, 2).unwrap();
            assert_eq!(super::slot_index::<4>(handle), Some(2));
            if let Some(stale) = previous {
                assert_ne!(handle, stale);
                assert_eq!(slab.with(stale, |v| *v), None);
                assert_eq!(slab.remove(stale), None);
            }
            assert_eq!(slab.remove(handle), Some(value));
            previous = Some(handle);
        }
        assert_eq!(slab.next_hint[0].load(Ordering::Relaxed), 0);
    }

    #[test]
    fn hinted_collisions_use_full_capacity_and_preserve_unpublished_value() {
        let slab = GenerationalSlab::<u64, 4, TEST_CLASS>::new();
        let handles: [_; 4] =
            core::array::from_fn(|i| slab.insert_with_hint(i as u64, u64::MAX).unwrap());
        assert_eq!(slab.insert_with_hint(99, u64::MAX), Err(99));
        for (i, handle) in handles.into_iter().enumerate() {
            assert_eq!(slab.remove(handle), Some(i as u64));
        }
        assert!(slab.insert_with_hint(99, u64::MAX).is_ok());
    }

    #[test]
    fn hinted_allocation_skips_permanently_exhausted_generation() {
        let slab = GenerationalSlab::<u64, 4, TEST_CLASS>::new();
        slab.with_slot(2, |slot| slot.generation = super::MAX_GENERATION);
        let last = slab.insert_with_hint(11, 2).unwrap();
        assert_eq!(super::slot_index::<4>(last), Some(2));
        assert_eq!(slab.remove(last), Some(11));
        let next = slab.insert_with_hint(22, 2).unwrap();
        assert_eq!(super::slot_index::<4>(next), Some(3));
        assert_eq!(slab.with(last, |v| *v), None);
        assert_eq!(slab.with(next, |v| *v), Some(22));
    }

    #[test]
    fn concurrent_hinted_allocations_never_share_live_authority() {
        let slab = GenerationalSlab::<u64, 4, TEST_CLASS>::new();
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            let threads: [_; 4] = core::array::from_fn(|i| {
                let slab = &slab;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    slab.insert_with_hint(i as u64, 0).unwrap()
                })
            });
            let handles = threads.map(|thread| thread.join().unwrap());
            for i in 0..4 {
                assert_eq!(slab.with(handles[i], |v| *v), Some(i as u64));
                for j in 0..i {
                    assert_ne!(handles[i], handles[j]);
                }
            }
        });
    }
}
