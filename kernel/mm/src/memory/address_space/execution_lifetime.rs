//! Physical root ownership, distinct from process-state box ownership.
//! Last release only enqueues preallocated storage. Reclamation (including HAL
//! inactive-root/shootdown) runs in an explicit cold drain, never a pin Drop.
use alloc::{boxed::Box, vec::Vec};
use core::{
    cell::UnsafeCell,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
};
const CLOSED: usize = 1usize << (usize::BITS - 1);
const COUNT: usize = CLOSED - 1;
struct Root {
    root: u64,
    state: AtomicUsize,
    activation: AtomicUsize,
    next: AtomicPtr<Root>,
    queue: &'static Queue,
    reclaim: bool,
    payload: UnsafeCell<Option<Box<dyn Send>>>,
    // Installed by the unique mutable MM owner before external PTE publication.
    // Pins never access this list; the cold consumer drops it after reclaim.
    backings: UnsafeCell<Backings>,
    // Lock-free census mirror; observers never access the mutable lease list.
    retained_pages: AtomicU64,
    mapping_meter: UnsafeCell<Option<crate::memory::kmem_charge::SharedMeter>>,
}
struct Backings {
    leases: Vec<super::ImmutableBacking>,
    group: Option<usize>,
    charged: u64,
}
impl Drop for Root {
    fn drop(&mut self) {
        // Release any retired process payload before returning backing charges.
        drop(self.payload.get_mut().take());
        let backings = self.backings.get_mut();
        // Backing pages remain billed until all root custody is dropped.
        backings.leases.clear();
        if let Some(group) = backings.group {
            crate::memory::charge::uncharge(group, backings.charged);
        }
    }
}
struct Queue {
    head: AtomicPtr<Root>,
    draining: AtomicBool,
}
impl Queue {
    const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            draining: AtomicBool::new(false),
        }
    }
    fn push(&self, root: NonNull<Root>) {
        let node = unsafe { root.as_ref() };
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            node.next.store(head, Ordering::Relaxed);
            match self.head.compare_exchange_weak(
                head,
                root.as_ptr(),
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => head = current,
            }
        }
    }
    fn drain(&self, limit: usize, mut reclaim: impl FnMut(u64, bool)) -> usize {
        if self
            .draining
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return 0;
        }
        struct Drain<'a>(&'a AtomicBool);
        impl Drop for Drain<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _drain = Drain(&self.draining);
        let mut count = 0;
        while count < limit {
            let head = self.head.load(Ordering::Acquire);
            if head.is_null() {
                break;
            }
            // Only this consumer can dereference/remove/free queued nodes.
            let next = unsafe { (*head).next.load(Ordering::Relaxed) };
            if self
                .head
                .compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            // SAFETY: the single consumer detached the node; check zero-ref
            // custody before Box ownership/destruction, including failure unwind.
            assert_eq!(unsafe { (*head).state.load(Ordering::Acquire) }, CLOSED);
            assert_eq!(
                unsafe { (*head).activation.load(Ordering::Acquire) },
                CLOSED
            );
            let node = unsafe { Box::from_raw(head) };
            reclaim(node.root, node.reclaim);
            drop(node); // outside all owner leaves; no root pointer escapes
            count += 1;
        }
        count
    }
}
static RETIRED: Queue = Queue::new();
#[derive(Debug)]
pub(super) struct Owner {
    root: NonNull<Root>,
}
/// Cold owner transfer retains old mapping/backing metadata with the root.
pub struct RetirementOwner {
    pub(super) owner: Owner,
}
impl RetirementOwner {
    pub fn retire<T: Send + 'static>(self, payload: T) {
        let payload: Box<dyn Send> = Box::new(payload);
        // SAFETY: the unique original Owner still holds its initial reference,
        // so the drain cannot see this node until after payload installation.
        unsafe {
            *self.owner.root.as_ref().payload.get() = Some(payload);
        }
        drop(self.owner);
    }
}

#[derive(Debug)]
pub struct ExecutionPin {
    root: NonNull<Root>,
}

/// A transient activation grant, separate from physical execution references.
/// Admission and close share the activation CAS. Drop releases admission before
/// its retained physical ref; it never allocates, locks, or performs HAL work.
#[derive(Debug)]
pub struct ActivationPin {
    pin: ExecutionPin,
}
// SAFETY: the grant owns one checked activation unit and one physical ref;
// cross-thread transfer does not confer CPU/register authority.
unsafe impl Send for ActivationPin {}
// SAFETY: shared access only reads immutable root identity/atomic close state.
unsafe impl Sync for ActivationPin {}
impl ActivationPin {
    pub fn root_phys(&self) -> u64 {
        self.pin.root_phys()
    }
    pub fn is_open(&self) -> bool {
        self.pin.is_open()
    }
}
impl Drop for ActivationPin {
    fn drop(&mut self) {
        release_activation(self.pin.root);
        // The physical ExecutionPin field drops only after admission release.
    }
}
fn release_activation(root: NonNull<Root>) {
    let node = unsafe { root.as_ref() };
    assert!(
        node.activation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| (state & COUNT
                != 0)
                .then(|| state - 1))
            .is_ok(),
        "MM activation grant released twice"
    );
}
// SAFETY: Owner holds one reference to an atomically managed allocation;
// sealing/release are atomic and expose no mutable PTE borrow.
unsafe impl Send for Owner {}
// SAFETY: shared Owner access only seals or atomically pins that allocation.
unsafe impl Sync for Owner {}
// SAFETY: each non-Copy pin owns one atomic reference, released once on Drop.
unsafe impl Send for ExecutionPin {}
// SAFETY: shared pin access reads immutable root identity or atomic state only.
unsafe impl Sync for ExecutionPin {}
impl Owner {
    pub(super) fn new(root: u64, reclaim: bool) -> Self {
        Self::with_queue(root, reclaim, &RETIRED)
    }
    fn with_queue(root: u64, reclaim: bool, queue: &'static Queue) -> Self {
        Self {
            root: NonNull::from(Box::leak(Box::new(Root {
                root,
                state: AtomicUsize::new(1),
                activation: AtomicUsize::new(0),
                next: AtomicPtr::new(ptr::null_mut()),
                queue,
                reclaim,
                payload: UnsafeCell::new(None),
                backings: UnsafeCell::new(Backings { leases: Vec::new(), group: None, charged: 0 }),
                retained_pages: AtomicU64::new(0),
                mapping_meter: UnsafeCell::new(None),
            }))),
        }
    }
    pub(super) fn set_mapping_meter(&mut self, meter: crate::memory::kmem_charge::SharedMeter) {
        unsafe { *self.root.as_ref().mapping_meter.get() = Some(meter); }
    }
    pub(super) fn retain_backing(
        &mut self,
        backing: super::ImmutableBacking,
    ) -> Result<bool, super::AddressSpaceError> {
        // Only the unique mutable owner touches this list; pins do not access it.
        let backings = unsafe { &mut *self.root.as_ref().backings.get() };
        if let Some(existing) = backings.leases.iter().find(|old| old.same_identity(&backing)) {
            if existing.pages != backing.pages {
                return Err(super::AddressSpaceError::InvalidFrameOwnership);
            }
            return Ok(false);
        }
        backings.leases.try_reserve(1).map_err(|_| super::AddressSpaceError::OutOfFrames)?;
        let Some(total) = backings.charged.checked_add(backing.pages) else {
            return Err(super::AddressSpaceError::OutOfFrames);
        };
        if backings.group.is_some_and(|group| !crate::memory::charge::try_charge(group, backing.pages)) {
            return Err(super::AddressSpaceError::OutOfFrames);
        }
        backings.charged = total;
        backings.leases.push(backing);
        // SAFETY: the unique owner retains this root allocation across the store.
        // ORDERING: Relaxed; this is standalone census data, not publication.
        unsafe { self.root.as_ref() }.retained_pages.store(total, Ordering::Relaxed);
        Ok(true)
    }
    pub(super) fn cancel_backing(&mut self, backing: super::ImmutableBacking) {
        let backings = unsafe { &mut *self.root.as_ref().backings.get() };
        let index = backings.leases.iter().position(|old| old.same_identity(&backing)).expect("missing staged backing");
        let lease = backings.leases.swap_remove(index);
        let pages = lease.pages;
        drop(lease);
        drop(backing);
        backings.charged -= pages;
        // SAFETY: the unique owner retains this root allocation across the store.
        // ORDERING: Relaxed; census uses the scalar only, never lease contents.
        unsafe { self.root.as_ref() }.retained_pages.store(backings.charged, Ordering::Relaxed);
        if let Some(group) = backings.group {
            crate::memory::charge::uncharge(group, pages);
        }
    }
    pub(super) fn backings_bound(&self) -> bool {
        unsafe { (&*self.root.as_ref().backings.get()).group.is_some() }
    }
    pub(super) fn bind_backings(&mut self, group: usize) -> bool {
        let backings = unsafe { &mut *self.root.as_ref().backings.get() };
        if backings.group.is_some() || !crate::memory::charge::try_charge(group, backings.charged) {
            return false;
        }
        backings.group = Some(group);
        true
    }
    pub(super) fn unbind_backings(&mut self) {
        let backings = unsafe { &mut *self.root.as_ref().backings.get() };
        if let Some(group) = backings.group.take() {
            crate::memory::charge::uncharge(group, backings.charged);
        }
    }
    pub(super) fn backing_metadata_bytes(&self) -> usize {
        (unsafe { (&*self.root.as_ref().backings.get()).leases.capacity() })
            * core::mem::size_of::<super::ImmutableBacking>()
    }
    pub(super) fn pin(&self) -> Option<ExecutionPin> {
        acquire(self.root)
    }
    pub(super) fn seal(&self) {
        unsafe {
            self.root
                .as_ref()
                .activation
                .fetch_or(CLOSED, Ordering::AcqRel);
            self.root.as_ref().state.fetch_or(CLOSED, Ordering::AcqRel);
        }
    }

    /// No partial close on refusal: an in-flight claim/architecture/RSP grant
    /// retains open admission until its CPU completes or explicitly cancels it.
    pub(super) fn try_seal_activation(&self) -> bool {
        let node = unsafe { self.root.as_ref() };
        if node
            .activation
            .compare_exchange(0, CLOSED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            if node.activation.load(Ordering::Acquire) != CLOSED {
                return false;
            }
            // An idempotent closer also completes physical close; it cannot
            // return between the winning closer's two atomic publications.
            node.state.fetch_or(CLOSED, Ordering::AcqRel);
            return true;
        }
        node.state.fetch_or(CLOSED, Ordering::AcqRel);
        true
    }
}
impl ExecutionPin {
    /// Whole immutable backing pages retained by this root, counted once per
    /// backing identity, including pages retained after partial/full unmap.
    pub fn retained_backing_pages(&self) -> u64 {
        // SAFETY: this pin retains the root until the method returns.
        // ORDERING: Relaxed; no pointer or mapping authority is derived here.
        unsafe { self.root.as_ref() }.retained_pages.load(Ordering::Relaxed)
    }
    pub fn root_phys(&self) -> u64 {
        unsafe { self.root.as_ref().root }
    }
    pub fn is_open(&self) -> bool {
        unsafe { self.root.as_ref().state.load(Ordering::Acquire) & CLOSED == 0 }
    }
    pub fn try_clone(&self) -> Option<Self> {
        acquire(self.root)
    }

    /// Capture an activation grant while this physical ref retains the node.
    /// Losing close/saturation refuses without consuming any other grant.
    pub fn try_activate(&self) -> Option<ActivationPin> {
        let node = unsafe { self.root.as_ref() };
        let mut state = node.activation.load(Ordering::Acquire);
        loop {
            if state & CLOSED != 0 || state & COUNT == COUNT {
                return None;
            }
            match node.activation.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => state = current,
            }
        }
        match acquire(self.root) {
            Some(pin) => Some(ActivationPin { pin }),
            None => {
                release_activation(self.root);
                None
            }
        }
    }
}
fn acquire(root: NonNull<Root>) -> Option<ExecutionPin> {
    let node = unsafe { root.as_ref() };
    let mut state = node.state.load(Ordering::Acquire);
    loop {
        if state & CLOSED != 0 || state & COUNT == 0 || state & COUNT == COUNT {
            return None;
        }
        match node.state.compare_exchange_weak(
            state,
            state + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(ExecutionPin { root }),
            Err(current) => state = current,
        }
    }
}
fn release(root: NonNull<Root>) {
    let node = unsafe { root.as_ref() };
    let mut state = node.state.load(Ordering::Acquire);
    loop {
        assert_ne!(state & COUNT, 0, "MM execution reference released twice");
        match node.state.compare_exchange_weak(
            state,
            state - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                if state & COUNT == 1 {
                    assert!(state & CLOSED != 0, "open MM lost its owner reference");
                    node.queue.push(root);
                }
                return;
            }
            Err(current) => state = current,
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.seal();
        release(self.root);
    }
}
impl Drop for ExecutionPin {
    fn drop(&mut self) {
        release(self.root);
    }
}
/// Caller must run outside scheduler/process-state/leaf owners and IRQ-return
/// publication. Existing HAL inactive-root/global-shootdown retirement is kept.
pub fn drain_retired_address_spaces(limit: usize) -> usize {
    #[cfg(rustos_boot_image)]
    if !x86_64::instructions::interrupts::are_enabled() {
        return 0;
    }
    RETIRED.drain(limit, |root, reclaim| {
        if reclaim {
            super::retirement::reclaim_root(root);
        }
    })
}
#[cfg(test)]
mod activation_tests;
#[cfg(test)]
mod tests;
