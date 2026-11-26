//! Lock-free, IRQ-safe slab block pool for dynamically growing scheduler queues.
//!
//! - **Owner:** `kernel-ps` owns node storage inside a grow-only `AtomicDir`;
//!   individual CPU-local queues (`deferred_wake`, `wake_retry`, `handoff_queue`,
//!   `RemoteWakeMailbox`, `fair_retry`) own only their active node chains.
//! - **Boundary:** Nodes are allocated only in preemptible normal context (at boot
//!   or when task slots grow) and pushed onto the lock-free freelist; IRQ and
//!   masked-lock paths pop and push nodes without calling the heap allocator.
//! - **Lifecycle:** `alloc_node` pops a pre-allocated block (or allocates a new
//!   slot in normal context), links it into a caller-owned chain, and `free_node`
//!   returns empty blocks to the freelist with a monotonically tagged CAS.
//! - **Concurrency:** The freelist uses a 64-bit `(tag_generation << 32) | id`
//!   `AcqRel` CAS so concurrent pop/push across CPUs and IRQs cannot suffer ABA.
//! - **Failure:** Allocation failure in normal context or reserve exhaustion
//!   under IRQ returns `None`/`false` cleanly without corrupting chain links.
//! - **Forbidden:** Calling the global allocator while interrupts are disabled,
//!   holding a spin lock across `dir.ensure`, or freeing slab chunks once
//!   published.
//! - **Evidence:** Unit tests below and scheduler queue stress tests.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::slot_dir::AtomicDir;

pub(in crate::multitask) const SLAB_BLOCK_CAP: usize = 32;
pub(in crate::multitask) const NO_NODE: u32 = u32::MAX;
const SLAB_DIR_CHUNKS: usize = 1024;
const SLAB_DIR_CHUNK_LEN: usize = 64;
const MAX_SLAB_NODES: usize = SLAB_DIR_CHUNKS * SLAB_DIR_CHUNK_LEN;

struct SlabNode<T: Copy> {
    next: u32,
    head: u8,
    len: u8,
    items: [Option<T>; SLAB_BLOCK_CAP],
}

impl<T: Copy> SlabNode<T> {
    const fn empty() -> Self {
        Self {
            next: NO_NODE,
            head: 0,
            len: 0,
            items: [None; SLAB_BLOCK_CAP],
        }
    }

    fn reset(&mut self) {
        self.next = NO_NODE;
        self.head = 0;
        self.len = 0;
        self.items.fill(None);
    }
}

struct SlabCell<T: Copy>(UnsafeCell<SlabNode<T>>);

// SAFETY: A node is exclusively owned by either the freelist (`free_head` CAS)
// or the single queue currently holding its `u32` ID.
unsafe impl<T: Copy> Sync for SlabCell<T> {}

pub(in crate::multitask) struct SlabPool<T: Copy> {
    dir: AtomicDir<SlabCell<T>, SLAB_DIR_CHUNK_LEN, SLAB_DIR_CHUNKS>,
    next_id: AtomicU32,
    /// `(generation << 32) | node_id` (`NO_NODE` = empty).
    free_head: AtomicU64,
    free_count: AtomicU32,
}

impl<T: Copy> SlabPool<T> {
    pub(in crate::multitask) const fn new() -> Self {
        Self {
            dir: AtomicDir::new(),
            next_id: AtomicU32::new(0),
            free_head: AtomicU64::new(NO_NODE as u64),
            free_count: AtomicU32::new(0),
        }
    }

    #[inline]
    fn can_preallocate() -> bool {
        #[cfg(test)]
        {
            true
        }
        #[cfg(not(test))]
        {
            nucleus_core::util::lockdep::held_spin_lock_depth() == 0
                && nucleus_core::util::lockdep::irq_context_depth() == 0
        }
    }

    #[inline]
    fn can_allocate_from_heap() -> bool {
        #[cfg(test)]
        {
            true
        }
        #[cfg(not(test))]
        {
            Self::can_preallocate() && x86_64::instructions::interrupts::are_enabled()
        }
    }

    fn alloc_fresh_id(&self) -> Option<u32> {
        let id = self
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                usize::try_from(cur)
                    .ok()
                    .filter(|&n| n < MAX_SLAB_NODES)
                    .map(|_| cur + 1)
            })
            .ok()?;
        let index = usize::try_from(id).ok()?;
        if !self
            .dir
            .ensure(index, || SlabCell(UnsafeCell::new(SlabNode::empty())))
        {
            return None;
        }
        Some(id)
    }

    pub(in crate::multitask) fn preallocate_reserve(&self, min_free: usize) {
        if !Self::can_preallocate() {
            return;
        }
        while (self.free_count.load(Ordering::Acquire) as usize) < min_free {
            let Some(id) = self.alloc_fresh_id() else {
                break;
            };
            self.push_free(id);
        }
    }

    fn pop_free(&self) -> Option<u32> {
        loop {
            // ORDERING: Acquire observes initialized node storage written before
            // the node was pushed onto the freelist.
            let packed = self.free_head.load(Ordering::Acquire);
            let id = packed as u32;
            if id == NO_NODE {
                return None;
            }
            let tag = (packed >> 32) as u32;
            // SAFETY: `id` is a published slot in `self.dir`, and only the CAS
            // winner mutates `node.next` below. Another popping thread that races
            // might read a stale `next`, in which case our CAS observes the bumped
            // `tag` and retries.
            let next = unsafe { (*self.dir[id as usize].0.get()).next };
            let next_packed = (u64::from(tag.wrapping_add(1)) << 32) | u64::from(next);
            // ORDERING: AcqRel pops the head node and prevents ABA via `tag`.
            if self
                .free_head
                .compare_exchange_weak(packed, next_packed, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.free_count.fetch_sub(1, Ordering::Relaxed);
                // SAFETY: exclusive ownership just won via `free_head` CAS.
                unsafe {
                    (*self.dir[id as usize].0.get()).reset();
                }
                return Some(id);
            }
        }
    }

    fn push_free(&self, id: u32) {
        debug_assert_ne!(id, NO_NODE);
        loop {
            // ORDERING: Acquire pairs with previous head update.
            let packed = self.free_head.load(Ordering::Acquire);
            let head = packed as u32;
            let tag = (packed >> 32) as u32;
            // SAFETY: caller owns `id` exclusively prior to publishing it back on `free_head`.
            unsafe {
                let node = &mut *self.dir[id as usize].0.get();
                node.reset();
                node.next = head;
            }
            let next_packed = (u64::from(tag.wrapping_add(1)) << 32) | u64::from(id);
            // ORDERING: Release publishes reset node state before making `id` visible.
            if self
                .free_head
                .compare_exchange_weak(packed, next_packed, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.free_count.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }

    fn alloc_node(&self) -> Option<u32> {
        if let Some(id) = self.pop_free() {
            return Some(id);
        }
        if Self::can_allocate_from_heap() {
            return self.alloc_fresh_id();
        }
        None
    }

    #[inline]
    unsafe fn node(&self, id: u32) -> &SlabNode<T> {
        // SAFETY: caller owns the queue containing `id`.
        unsafe { &*self.dir[id as usize].0.get() }
    }

    #[inline]
    unsafe fn node_mut(&self, id: u32) -> &mut SlabNode<T> {
        // SAFETY: caller holds exclusive mutable access to the queue containing `id`.
        unsafe { &mut *self.dir[id as usize].0.get() }
    }
}

/// Queue / list built out of pooled slab blocks (`SlabPool<T>`).
pub(in crate::multitask) struct SlabQueue<T: Copy> {
    head_node: u32,
    tail_node: u32,
    len: usize,
    _marker: core::marker::PhantomData<T>,
}

impl<T: Copy> SlabQueue<T> {
    pub(in crate::multitask) const fn new() -> Self {
        Self {
            head_node: NO_NODE,
            tail_node: NO_NODE,
            len: 0,
            _marker: core::marker::PhantomData,
        }
    }

    #[inline]
    pub(in crate::multitask) const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub(in crate::multitask) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(in crate::multitask) fn front(&self, pool: &SlabPool<T>) -> Option<T> {
        let mut cursor = self.head_node;
        while cursor != NO_NODE {
            // SAFETY: shared reference to `self` grants shared access to nodes.
            let node = unsafe { pool.node(cursor) };
            if node.len != 0 {
                return node.items[usize::from(node.head)];
            }
            cursor = node.next;
        }
        None
    }

    pub(in crate::multitask) fn preallocate_for_pool(pool: &SlabPool<T>, min_free: usize) {
        pool.preallocate_reserve(min_free);
    }

    pub(in crate::multitask) fn push_back(&mut self, pool: &SlabPool<T>, item: T) -> bool {
        if self.tail_node != NO_NODE {
            // SAFETY: `tail_node` belongs exclusively to `&mut self`.
            let tail = unsafe { pool.node_mut(self.tail_node) };
            let end = usize::from(tail.head) + usize::from(tail.len);
            if end < SLAB_BLOCK_CAP {
                tail.items[end] = Some(item);
                tail.len += 1;
                self.len += 1;
                return true;
            }
        }
        let Some(new_id) = pool.alloc_node() else {
            return false;
        };
        // SAFETY: `new_id` was freshly allocated exclusively for `self`.
        unsafe {
            let node = pool.node_mut(new_id);
            node.items[0] = Some(item);
            node.head = 0;
            node.len = 1;
            node.next = NO_NODE;
            if self.tail_node != NO_NODE {
                pool.node_mut(self.tail_node).next = new_id;
            } else {
                self.head_node = new_id;
            }
        }
        self.tail_node = new_id;
        self.len += 1;
        true
    }

    pub(in crate::multitask) fn pop_front(&mut self, pool: &SlabPool<T>) -> Option<T> {
        while self.head_node != NO_NODE {
            let node_id = self.head_node;
            // SAFETY: `head_node` belongs exclusively to `&mut self`.
            let (item, empty, next) = unsafe {
                let node = pool.node_mut(node_id);
                if node.len == 0 {
                    (None, true, node.next)
                } else {
                    let idx = usize::from(node.head);
                    let item = node.items[idx].take();
                    node.head = (idx + 1) as u8;
                    node.len -= 1;
                    (item, node.len == 0, node.next)
                }
            };
            if empty {
                self.head_node = next;
                if next == NO_NODE {
                    self.tail_node = NO_NODE;
                }
                pool.push_free(node_id);
            }
            if let Some(value) = item {
                self.len -= 1;
                return Some(value);
            }
        }
        None
    }

    pub(in crate::multitask) fn clear(&mut self, pool: &SlabPool<T>) {
        let mut cursor = self.head_node;
        self.head_node = NO_NODE;
        self.tail_node = NO_NODE;
        self.len = 0;
        while cursor != NO_NODE {
            // SAFETY: every node in this chain is exclusively owned by `self`.
            let next = unsafe { pool.node(cursor).next };
            pool.push_free(cursor);
            cursor = next;
        }
    }

    pub(in crate::multitask) fn any<'a>(
        &self,
        pool: &'a SlabPool<T>,
        mut pred: impl FnMut(&T) -> bool,
    ) -> bool {
        let mut cursor = self.head_node;
        while cursor != NO_NODE {
            // SAFETY: shared reference to `self` grants shared access to nodes.
            let node = unsafe { pool.node(cursor) };
            let start = usize::from(node.head);
            let end = start + usize::from(node.len);
            for slot in &node.items[start..end] {
                if let Some(val) = slot
                    && pred(val)
                {
                    return true;
                }
            }
            cursor = node.next;
        }
        false
    }

    pub(in crate::multitask) fn find_mut<'a>(
        &mut self,
        pool: &'a SlabPool<T>,
        mut pred: impl FnMut(&T) -> bool,
    ) -> Option<&'a mut T> {
        let mut cursor = self.head_node;
        while cursor != NO_NODE {
            // SAFETY: mutable borrow of `self` grants exclusive access to nodes.
            let node = unsafe { pool.node_mut(cursor) };
            let start = usize::from(node.head);
            let end = start + usize::from(node.len);
            let next = node.next;
            for idx in start..end {
                if node.items[idx].as_ref().is_some_and(&mut pred) {
                    return node.items[idx].as_mut();
                }
            }
            cursor = next;
        }
        None
    }

    pub(in crate::multitask) fn for_each(&self, pool: &SlabPool<T>, mut f: impl FnMut(&T)) {
        let mut cursor = self.head_node;
        while cursor != NO_NODE {
            // SAFETY: shared reference to `self` grants shared access to nodes.
            let node = unsafe { pool.node(cursor) };
            let start = usize::from(node.head);
            let end = start + usize::from(node.len);
            for slot in &node.items[start..end] {
                if let Some(val) = slot {
                    f(val);
                }
            }
            cursor = node.next;
        }
    }

    /// Walks queued entries in FIFO order, allowing removal (`pred(&item) -> bool`)
    /// or replacement while preserving the relative order of retained entries.
    pub(in crate::multitask) fn pop_first_matching(
        &mut self,
        pool: &SlabPool<T>,
        mut pred: impl FnMut(&mut T) -> bool,
    ) -> Option<T> {
        let mut prev = NO_NODE;
        let mut cursor = self.head_node;
        while cursor != NO_NODE {
            // SAFETY: mutable borrow of `self` grants exclusive access to nodes.
            let (found, next, became_empty) = unsafe {
                let node = pool.node_mut(cursor);
                let start = usize::from(node.head);
                let end = start + usize::from(node.len);
                let mut found = None;
                for idx in start..end {
                    if let Some(item) = node.items[idx].as_mut()
                        && pred(item)
                    {
                        let taken = node.items[idx].take();
                        for shift in idx + 1..end {
                            node.items[shift - 1] = node.items[shift].take();
                        }
                        node.len -= 1;
                        found = taken;
                        break;
                    }
                }
                (found, node.next, node.len == 0)
            };
            if became_empty {
                if prev == NO_NODE {
                    self.head_node = next;
                } else {
                    // SAFETY: `prev` is a live node in `self`.
                    unsafe {
                        pool.node_mut(prev).next = next;
                    }
                }
                if self.tail_node == cursor {
                    self.tail_node = prev;
                }
                pool.push_free(cursor);
                cursor = next;
            } else {
                prev = cursor;
                cursor = next;
            }
            if let Some(item) = found {
                self.len -= 1;
                return Some(item);
            }
        }
        None
    }

    pub(in crate::multitask) fn retain(&mut self, pool: &SlabPool<T>, mut keep: impl FnMut(&mut T) -> bool) -> usize {
        let mut removed = 0usize;
        let mut prev = NO_NODE;
        let mut cursor = self.head_node;
        while cursor != NO_NODE {
            // SAFETY: mutable borrow of `self` grants exclusive access to nodes.
            let (next, became_empty, node_removed) = unsafe {
                let node = pool.node_mut(cursor);
                let start = usize::from(node.head);
                let end = start + usize::from(node.len);
                let mut write = start;
                let mut dropped = 0usize;
                for read in start..end {
                    if let Some(mut item) = node.items[read].take() {
                        if keep(&mut item) {
                            node.items[write] = Some(item);
                            write += 1;
                        } else {
                            dropped += 1;
                        }
                    }
                }
                node.len = (write - start) as u8;
                (node.next, node.len == 0, dropped)
            };
            removed += node_removed;
            self.len -= node_removed;
            if became_empty {
                if prev == NO_NODE {
                    self.head_node = next;
                } else {
                    // SAFETY: `prev` is a live node in `self`.
                    unsafe {
                        pool.node_mut(prev).next = next;
                    }
                }
                if self.tail_node == cursor {
                    self.tail_node = prev;
                }
                pool.push_free(cursor);
            } else {
                prev = cursor;
            }
            cursor = next;
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slab_queue_grows_across_blocks_retains_fifo_and_recycles() {
        let pool: SlabPool<u64> = SlabPool::new();
        pool.preallocate_reserve(2);
        let mut q = SlabQueue::new();
        for i in 1..=(SLAB_BLOCK_CAP as u64 * 3 + 5) {
            assert!(q.push_back(&pool, i));
        }
        assert_eq!(q.len(), SLAB_BLOCK_CAP * 3 + 5);
        assert_eq!(
            q.pop_first_matching(&pool, |x| *x == SLAB_BLOCK_CAP as u64 + 7),
            Some(SLAB_BLOCK_CAP as u64 + 7)
        );
        q.retain(&pool, |x| *x % 2 == 0);
        let mut prev = 0;
        while let Some(v) = q.pop_front(&pool) {
            assert!(v > prev && v % 2 == 0);
            prev = v;
        }
        assert!(q.is_empty());
        assert!(pool.free_count.load(Ordering::Acquire) >= 3);
    }
}
