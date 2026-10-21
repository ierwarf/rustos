//! Allocation custody independent of Scheduler. Publication is not a lifetime
//! grant: exact identity/geometry pins must precede saved-frame byte reads.
//! CPU execution/transition custody is still required separately.
use super::{MAX_TASK, context_validation::PublishedFrameView};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    primary_base: u64,
    primary_top: u64,
    alternate_base: u64,
    alternate_top: u64,
}

impl Geometry {
    fn from_view(view: PublishedFrameView) -> Self {
        Self {
            primary_base: view.kernel_stack_base,
            primary_top: view.kernel_stack_top,
            alternate_base: view.alternate_stack_base,
            alternate_top: view.alternate_stack_top,
        }
    }
}

/// Terminal50 reservation precedes this Identity51 -> Resources53 join. A
/// physical reuse receipt or constructor prefix cannot become an old producer.
pub(in crate::multitask) fn terminal_binding_matches(slot: usize, id: Option<u64>) -> bool {
    let Some(id) = id else {
        return false;
    };
    crate::multitask::current_identity::with_terminal_binding(slot, id, || {
        RESOURCES
            .get(slot)?
            .with(|r| {
                !r.closing
                    && r.reclaim.is_none()
                    && r.reservation.is_none()
                    && !r.frame_writing
                    && r.context.is_some()
                    && !r.context.and_then(|c| c.process_handle).is_some_and(crate::multitask::process_placement::root_membership::pending)
                    && r.start.is_some_and(|s| s.id == id)
                    && r.binding.is_some_and(|(bound, _)| bound == id)
            })
            .into()
    })
    .unwrap_or(false)
}

/// Identity51 -> Resource53. Only scalar identity/ref publication enters f.
/// Reservation release, not descriptor visibility, admits foreign consumers.
pub(in crate::multitask) fn with_cold_binding<R>(
    slot: usize,
    id: u64,
    f: impl FnOnce() -> R,
) -> Option<R> {
    RESOURCES.get(slot)?.with(|resources| {
        #[cfg(test)]
        let fixture_empty =
            resources.start.is_none() && resources.context.is_none() && resources.stack.is_none();
        #[cfg(not(test))]
        let fixture_empty = false;
        if resources.closing
            || resources.reservation.is_some()
            || resources.frame_writing
            || resources.context.and_then(|c| c.process_handle).is_some_and(crate::multitask::process_placement::root_membership::pending)
            || (!fixture_empty
                && (!resources.start.is_some_and(|start| start.id == id)
                    || !resources.binding.is_some_and(|(bound, _)| bound == id)))
        {
            return None;
        }
        Some(f())
    })
}

/// Native queue projection cannot override actual unpublished producer custody.
#[cfg(test)]
pub(in crate::multitask) fn fixture_claim_allowed(slot: usize) -> bool {
    admission::wake_allowed(slot)
}

/// Identity51 -> Resources53: an executor is a complete kernel binding, never
/// a user task, absent native projection, or unpublished constructor prefix.
pub(in crate::multitask) fn with_kernel_executor_binding<R>(
    slot: usize,
    id: u64,
    f: impl FnOnce() -> R,
) -> Option<R> {
    RESOURCES.get(slot)?.with(|r| {
        if r.closing
            || r.reservation.is_some()
            || r.reclaim.is_some()
            || r.frame_writing
            || !r.start.is_some_and(|s| s.id == id)
            || !r.binding.is_some_and(|(bound, _)| bound == id)
            || !r.context.is_some_and(|c| {
                !c.user_mode && c.user_abi.is_none()
                    && c.process_handle.is_none()
                    && c.process_id.is_none()
            })
        {
            return None;
        }
        Some(f())
    })
}

struct Resources {
    context: Option<super::TaskContext>,
    mm_pin: Option<crate::multitask::process_table::MmExecutionPin>,
    process_pin: Option<crate::multitask::process_table::pin_owner::ProcessObjectPin>,
    start: Option<super::TaskStart>,
    stack: Option<Vec<u8>>,
    binding: Option<(u64, Geometry)>,
    closing: bool,
    frame_writing: bool,
    reservation: Option<admission::Reservation>,
    reservation_generation: u64,
    reclaim: Option<reuse::Key>,
}

/// A borrowed actual Resource53 turn, never a copied context or new execution pin.
/// Only this owner and its children can construct the cross-owner membership view.
pub(in crate::multitask) struct Membership<'a> {
    resources: &'a Resources,
    slot: usize,
}
impl Membership<'_> {
    pub(in crate::multitask) fn binding(&self) -> Option<(crate::multitask::process_table::ProcessHandle, usize, u64)> {
        Some((self.resources.process_pin.as_ref()?.handle(), self.slot, self.resources.binding?.0))
    }
    pub(in crate::multitask) fn reclaim_nonce(&self) -> Option<u64> {
        self.resources.reclaim.map(reuse::Key::nonce)
    }
}
/// Checked candidate nonce borrowed BEFORE the actual producer-last resource seal.
/// Publication in Class55 and installation in Resource53 stay in this same turn.
pub(in crate::multitask) struct ReclaimAdmission<'a> {
    member: Membership<'a>,
    nonce: u64,
}
impl ReclaimAdmission<'_> {
    pub(in crate::multitask) fn member(&self) -> &Membership<'_> { &self.member }
    pub(in crate::multitask) fn nonce(&self) -> u64 { self.nonce }
}

struct ResourceCell {
    inner: TrackedSpinLock<Resources, { LockClass::TaskStackResources as u8 }>,
    pins: AtomicU32,
    /// Registered saved-frame readers. Like `pins`: every increment happens
    /// under the cell leaf after its admission checks, and a reader releases
    /// with one atomic decrement instead of a second leaf acquisition. Claims
    /// and writers read it under the leaf, where no new reader can register.
    readers: AtomicU32,
    closing_notice: AtomicBool,
}

impl ResourceCell {
    const fn new() -> Self {
        Self {
            inner: TrackedSpinLock::new(Resources {
                context: None,
                process_pin: None,
                mm_pin: None,
                start: None,
                stack: None,
                binding: None,
                closing: false,
                frame_writing: false,
                reservation: None,
                reservation_generation: 0,
                reclaim: None,
            }),
            pins: AtomicU32::new(0),
            readers: AtomicU32::new(0),
            closing_notice: AtomicBool::new(false),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Resources) -> R) -> R {
        #[cfg(not(test))]
        {
            // SAFETY: the closure runs with IRQs masked and drops the guard
            // before `without_interrupts` restores IF.
            x86_64::instructions::interrupts::without_interrupts(|| {
                let mut guard = unsafe { self.inner.lock_masked() };
                f(&mut guard)
            })
        }
        #[cfg(test)]
        {
            f(&mut self.inner.lock())
        }
    }

    fn pin(&self, task_id: u64, view: PublishedFrameView) -> bool {
        self.with(|resources| self.pin_locked(resources, task_id, view))
    }

    /// The caller holds this cell's leaf, including combined frame admission.
    fn pin_locked(&self, resources: &Resources, task_id: u64, view: PublishedFrameView) -> bool {
        if resources.closing
            || resources.frame_writing
            || resources.mm_pin.as_ref().is_some_and(|pin| !pin.is_open())
            || resources.start.is_none_or(|start| start.id != task_id)
            || resources.binding != Some((task_id, Geometry::from_view(view)))
        {
            return false;
        }
        // Every increment is serialized by the cell. Drops can only lower
        // this count; checked CAS cannot wrap on a concurrent decrement.
        self.pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| count.checked_add(1))
            .expect("stack resource pin overflow");
        true
    }

    fn unpin(&self) {
        let before = self.pins.fetch_sub(1, Ordering::Release);
        assert_ne!(before, 0, "stack resource pin released without custody");
        if before == 1 && self.closing_notice.load(Ordering::Acquire) {
            super::retirement_cleanup::mark_reap_pending();
        }
    }
}

static RESOURCES: [ResourceCell; MAX_TASK] = [const { ResourceCell::new() }; MAX_TASK];

/// Non-Copy allocation pin. Drop is lock-free/allocation-free so a future
/// architectural switch-release edge can carry it without a leaf lock.
#[derive(Debug)]
pub(super) struct StackPin {
    slot: usize,
}
impl Drop for StackPin {
    fn drop(&mut self) {
        RESOURCES[self.slot].unpin();
    }
}

pub(super) fn pin_view(slot: usize, task_id: u64, view: PublishedFrameView) -> Option<StackPin> {
    RESOURCES
        .get(slot)?
        .pin(task_id, view)
        .then(|| StackPin { slot })
}

pub(super) fn allocated(slot: usize) -> bool {
    RESOURCES[slot].with(|resources| resources.stack.is_some())
}

/// Copied lifecycle/refcount evidence for an IRQ-excluded architecture command.
/// The enclosing exec/terminal transaction still owns admission revocation.
pub(in crate::multitask::scheduler) fn simd_resource_state(
    slot: usize,
) -> Option<(u32, bool, bool)> {
    RESOURCES.get(slot)?.with(|resources| {
        Some((
            RESOURCES[slot].pins.load(Ordering::Acquire),
            resources.closing,
            resources.start.is_some(),
        ))
    })
}

/// Allocation is performed by the cold constructor before taking this leaf.
pub(super) fn install_storage(slot: usize, stack: Vec<u8>) {
    install_storage_with_reservation(slot, stack, None)
}
fn install_storage_with_reservation(
    slot: usize,
    stack: Vec<u8>,
    reservation: Option<admission::Reservation>,
) {
    let mut incoming = Some(stack);
    RESOURCES[slot].with(|resources| {
        assert_eq!(
            resources.reservation, reservation,
            "foreign slot storage installation"
        );
        assert!(
            !resources.frame_writing
                && resources.stack.is_none()
                && resources.binding.is_none()
                && resources.start.is_none()
                && resources.context.is_none()
                && resources.mm_pin.is_none()
        );
        assert_eq!(RESOURCES[slot].pins.load(Ordering::Acquire), 0);
        resources.closing = false;
        RESOURCES[slot]
            .closing_notice
            .store(false, Ordering::Release);
        resources.stack = incoming.take();
    });
}

pub(super) fn initialize_storage(slot: usize, f: impl FnOnce(&mut [u8])) {
    frame_access::initialize_storage(slot, f);
}

pub(super) fn storage_base(slot: usize) -> usize {
    RESOURCES[slot].with(|resources| {
        resources
            .stack
            .as_ref()
            .expect("task stack is allocated")
            .as_ptr() as usize
    })
}

pub(super) fn owned_canary_intact(slot: usize) -> Option<bool> {
    RESOURCES[slot].with(|resources| {
        let storage = resources.stack.as_ref()?;
        let guard = storage.get(..super::TASK_STACK_GUARD_BYTES)?;
        Some(guard.chunks_exact(core::mem::size_of::<u64>()).all(|word| {
            u64::from_ne_bytes(word.try_into().expect("exact canary word"))
                == super::STACK_CANARY_WORD
        }))
    })
}

/// Borrow the binding already retained by the original physical Current bank.
/// This is deliberately not new admission: terminal close cannot erase the old
/// loan. Caller keeps its borrowed masked CpuDispatchTurn until return finishes.
/// No allocation/frame bytes are read under this resource leaf.
pub(super) fn current_return_binding(
    cpu: usize,
    slot: usize,
    view: PublishedFrameView,
) -> Option<u64> {
    if !cpu_leases::owns_current(cpu, slot) {
        return None;
    }
    let cell = RESOURCES.get(slot)?;
    cell.with(|resources| {
        let id = resources.start?.id;
        let context = resources.context?;
        if view.user_mode_task != context.user_mode
            || view.kernel_process_task != (!context.user_mode && context.process_handle.is_some())
            || cell.pins.load(Ordering::Acquire) == 0
            || resources.reservation.is_some()
            || resources.frame_writing
            || resources.reclaim.is_some()
            || resources.binding != Some((id, Geometry::from_view(view)))
        {
            return None;
        }
        Some(id)
    })
}

/// One joined post-producer qualification. No raw leaf spans guard/header reads.
pub(super) fn capture_outgoing_image(
    cpu: usize, slot: usize, view: PublishedFrameView,
) -> Option<(u64, super::return_architecture::ArchitectureState, Option<core::ptr::NonNull<u8>>)> {
    if !cpu_leases::owns_current(cpu, slot) { return None; }
    let cell = RESOURCES.get(slot)?;
    cell.with(|resources| {
        let id = resources.start?.id;
        let architecture = context::architecture_locked(cell, resources, slot, id, view)?;
        let guard = resources.stack.as_ref().and_then(|stack|
            stack.get(..super::TASK_STACK_GUARD_BYTES))
            .and_then(|guard| core::ptr::NonNull::new(guard.as_ptr() as *mut u8));
        Some((id, architecture, guard))
    })
}

pub(super) fn publish_binding(slot: usize, task_id: u64, view: PublishedFrameView) {
    RESOURCES[slot].with(|resources| {
        super::outgoing_image::assert_mutable(slot);
        assert!(
            resources.start.is_some_and(|start| start.id == task_id),
            "frame binding lacks exact task descriptor"
        );
        let binding = (task_id, Geometry::from_view(view));
        if let Some(stack) = resources.stack.as_ref() {
            let base = stack.as_ptr() as usize;
            assert_eq!(
                view.kernel_stack_base,
                (base + super::TASK_STACK_GUARD_BYTES) as u64,
                "published stack does not name its allocation"
            );
            assert_eq!(
                view.kernel_stack_top,
                super::align_kernel_stack_top(base + stack.len()) as u64,
                "published stack top does not name its allocation"
            );
        }
        if let Some((old_task_id, _)) = resources.binding {
            assert_eq!(
                old_task_id, task_id,
                "stack allocation rebound before release"
            );
        }
        // Alternate-stack metadata may be adopted by its owning CPU. Pins
        // retain the allocation; the frame epoch still decides byte authority.
        resources.binding = Some(binding);
    });
}

/// Irreversible close for this binding. New frame observations cannot revive
/// it; existing pins retain the allocation until their last release.
pub(super) fn close_for_reclaim(slot: usize) -> bool {
    RESOURCES[slot].with(|resources| {
        if resources.reservation.is_some() || resources.reclaim.is_some()
            || crate::multitask::process_placement::reclaim::busy(&Membership { resources, slot })
        {
            return false;
        }
        resources.closing = true;
        RESOURCES[slot]
            .closing_notice
            .store(true, Ordering::Release);
        RESOURCES[slot].pins.load(Ordering::Acquire) == 0
            && !resources.frame_writing
            && super::super::current_identity::execution::count(slot) == 0
            && !super::super::current_identity::exec_intent::busy(slot)
    })
}

/// One-shot boot may initialize owners only before ANY physical lifetime exists.
/// This is refusal, not a teardown/reset operation on earlier backing.
pub(in crate::multitask::scheduler) fn assert_boot_vacant() {
    for (slot, cell) in RESOURCES.iter().enumerate() {
        cell.with(|r| {
            assert!(r.context.is_none() && r.start.is_none() && r.stack.is_none()
                && r.binding.is_none() && r.process_pin.is_none() && r.mm_pin.is_none()
                && r.reservation.is_none() && r.reclaim.is_none()
                && !r.closing && !r.frame_writing && cell.readers.load(Ordering::Acquire) == 0
                && cell.pins.load(Ordering::Acquire) == 0
                && crate::multitask::current_identity::execution::count(slot) == 0,
                "boot owner initialization overlaps physical task lifetime");
        });
    }
}

/// Initial execution refs share the producer/writer/close leaf before increment.
/// Callback is only the checked atomic reference acquisition, never external work.
pub(in crate::multitask) fn with_initial_execution<R>(
    slot: usize,
    acquire: impl FnOnce() -> R,
) -> Option<R> {
    RESOURCES.get(slot)?.with(|resources| {
        if resources.reservation.is_some() || resources.frame_writing || resources.closing {
            return None;
        }
        Some(acquire())
    })
}

/// Writer51 may join this higher-ranked leaf before opening an exec gate.
pub(in crate::multitask) fn image_writing(slot: usize) -> bool {
    RESOURCES
        .get(slot)
        .is_none_or(|cell| cell.with(|resources| resources.frame_writing))
}

/// Metadata detachment only. The caller destroys the returned allocation after
/// dropping directory/leaf locks, along with process/TLB reclamation work.
pub(super) fn take_storage(slot: usize) -> Option<Vec<u8>> {
    take_storage_with_reservation(slot, None, None)
}
fn take_storage_with_reservation(
    slot: usize,
    reservation: Option<admission::Reservation>,
    reclaim: Option<reuse::Key>,
) -> Option<Vec<u8>> {
    RESOURCES[slot].with(|resources| {
        assert_eq!(
            resources.reclaim, reclaim,
            "unowned reclaim storage detachment"
        );
        assert_eq!(
            resources.reservation, reservation,
            "foreign slot storage detachment"
        );
        assert!(
            !super::super::current_identity::exec_intent::busy(slot),
            "allocation detached with cold exec intent custody"
        );
        assert_eq!(
            super::super::current_identity::execution::count(slot),
            0,
            "allocation detached with captured lifecycle execution custody"
        );
        assert!(
            !resources.frame_writing,
            "allocation detached with frame writer custody"
        );
        assert_eq!(
            RESOURCES[slot].pins.load(Ordering::Acquire),
            0,
            "pinned stack allocation released"
        );
        assert!(
            resources.context.is_none() && resources.mm_pin.is_none(),
            "allocation detached before task/MM context transfer"
        );
        assert!(
            resources.start.is_none() || resources.closing,
            "task descriptor detached without closing"
        );
        resources.start = None;
        task_start::mirror_store(slot, None);
        resources.binding = None;
        resources.closing = resources.reclaim.is_some();
        RESOURCES[slot]
            .closing_notice
            .store(resources.closing, Ordering::Release);
        resources.stack.take()
    })
}

pub(in crate::multitask) mod activation;
pub(super) mod admission;
pub(in crate::multitask) mod context;
pub(in crate::multitask) mod cpu_leases;
pub(super) mod frame_access;
pub(super) mod frame_readers;
pub(in crate::multitask::scheduler) mod reuse;
pub(in crate::multitask) mod task_start;

#[cfg(test)]
pub(in crate::multitask::scheduler) fn install_exec_context_fixture(
    slot: usize,
    handle: crate::multitask::process_table::ProcessHandle,
) {
    let mut context = context::tests::metadata(slot, task_start::read(slot).unwrap().id);
    context.process_handle = Some(handle);
    // Synthetic receipt fixture only: no physical process-table authority.
    RESOURCES[slot].with(|resources| {
        assert!(resources.context.is_none());
        resources.context = Some(context);
    });
}

#[cfg(test)]
pub(super) fn admit_observation_test(slot: usize, task_id: u64) -> PublishedFrameView {
    tests::admit(slot, task_id)
}
#[cfg(test)]
mod kernel_executor_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
