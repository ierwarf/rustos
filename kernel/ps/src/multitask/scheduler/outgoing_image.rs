//! Owner: one post-producer outgoing image borrowed from the original CPU turn.
//! Boundary: freeze mutable image metadata before dispatch effects, never at IRQ entry.
//! Lifecycle: joined qualification -> byte validation/claim/architecture -> RSP seal -> drop.
//! Concurrency: physical Current retains backing; image writers refuse while frozen.
//! Failure: bad binding/MM refuses before effects; mutation after freeze fails closed.
//! Forbidden: whole TaskContext caching, fresh MM pins, or this snapshot as incoming authority.
//! Evidence: native actual mutation fences/counts; hardware return remains independent.
use super::{context_validation::PublishedFrameView, return_architecture::ArchitectureState,
    stack_resources, MAX_TASK, ROOT_TASK_SLOT, TASK_STACK_GUARD_BYTES, STACK_CANARY_WORD};
use core::{ptr::NonNull, sync::atomic::{AtomicBool, Ordering}};

static FROZEN: [AtomicBool; MAX_TASK] = [const { AtomicBool::new(false) }; MAX_TASK];

pub(super) fn frozen(slot: usize) -> bool {
    // ORDERING: observe the original turn's published image-mutation fence.
    FROZEN[slot].load(Ordering::Acquire)
}
pub(super) fn assert_mutable(slot: usize) {
    assert!(!frozen(slot), "outgoing image mutation after post-producer freeze");
}
struct Freeze(usize);
impl Drop for Freeze {
    fn drop(&mut self) {
        // ORDERING: all image borrowers end before mutations may resume.
        FROZEN[self.0].store(false, Ordering::Release);
    }
}

/// Comparison metadata only: its owning turn retains actual Current/MM/UID
/// custody. No ABI, process handle, run intent or funding is replayed from here.
pub(in crate::multitask) struct OriginalOutgoingImage {
    pub(super) cpu: usize,
    pub(super) slot: usize,
    pub(super) id: u64,
    pub(super) view: PublishedFrameView,
    pub(super) architecture: ArchitectureState,
    guard: Option<NonNull<u8>>,
    _freeze: Freeze,
}
impl OriginalOutgoingImage {
    pub(in crate::multitask) fn capture(cpu: usize, slot: usize) -> Option<Self> {
        if !stack_resources::cpu_leases::owns_current(cpu, slot) { return None; }
        // ORDERING: publish exclusion before the joined resource qualification;
        // refusal observes another original turn rather than replacing its fence.
        FROZEN[slot].compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok()?;
        let freeze = Freeze(slot);
        let view = super::frame_publication::return_view(slot)?;
        // Freeze precedes this leaf: a writer either won first and refuses this
        // capture, or observes the fence under its own resource exclusion.
        let (id, architecture, guard) = stack_resources::capture_outgoing_image(cpu, slot, view)?;
        Some(Self { cpu, slot, id, view, architecture, guard, _freeze: freeze })
    }
    pub(super) fn matches_view(&self, view: PublishedFrameView) -> bool {
        let v = self.view;
        (v.kernel_stack_base, v.kernel_stack_top, v.alternate_stack_base, v.alternate_stack_top,
            v.scheduler_storage_base, v.scheduler_storage_end, v.user_mode_task, v.kernel_process_task)
            == (view.kernel_stack_base, view.kernel_stack_top, view.alternate_stack_base, view.alternate_stack_top,
                view.scheduler_storage_base, view.scheduler_storage_end, view.user_mode_task, view.kernel_process_task)
    }
    pub(super) fn canary_intact(&self) -> bool {
        if self.slot == ROOT_TASK_SLOT { return true; }
        let base = match self.guard {
            Some(ptr) => ptr.as_ptr() as usize,
            None => match self.view.kernel_stack_base.checked_sub(TASK_STACK_GUARD_BYTES as u64) {
                Some(base) => base as usize,
                None => return false,
            },
        };
        (0..TASK_STACK_GUARD_BYTES / core::mem::size_of::<u64>()).all(|i| {
            // SAFETY: the joined leaf qualified the actual allocation prefix,
            // or admitted external stack geometry. The original masked turn
            // retains Current and the freeze excludes image writers/rebinding.
            unsafe { core::ptr::read_unaligned((base as *const u64).add(i)) == STACK_CANARY_WORD }
        })
    }
}
