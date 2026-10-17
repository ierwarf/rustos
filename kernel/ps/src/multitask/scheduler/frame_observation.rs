//! Copied header observations. Telemetry never grants dispatch permission.
//! Non-executing observation owns resource-leaf reader/claim exclusion;
//! Current/Prepared observation retains this CPU's IRQ/architecture exclusion.
use super::*;

// ABI witness for the padding-free POD copy inspected by zero diagnostics.
const _: () = assert!(mem::size_of::<SavedContext>() == 20 * mem::size_of::<u64>() + 16 * 16);

#[derive(Clone, Copy)]
pub(super) struct FrameSnapshot {
    pub(super) saved: SavedContext,
    pub(super) all_zero: bool,
}
impl FrameSnapshot {
    pub(super) fn from_saved(saved: SavedContext) -> Self {
        // SavedContext's fixed ABI consists only of initialized u64/u8 fields
        // and has no padding. Read the copy, never a borrow into task storage.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&saved as *const SavedContext).cast::<u8>(),
                mem::size_of::<SavedContext>(),
            )
        };
        let all_zero = bytes.iter().all(|byte| *byte == 0);
        Self { saved, all_zero }
    }
}
fn same_observation(
    left: frame_publication::StableFrameToken,
    right: frame_publication::StableFrameToken,
) -> bool {
    left.slot == right.slot
        && left.owner_generation == right.owner_generation
        && left.frame_epoch == right.frame_epoch
        && left.saved_rsp == right.saved_rsp
}

/// # Safety
/// Non-executing claim exclusion is acquired with the allocation pin. Executing
/// Current/Prepared byte stability is IRQ-stabilized here; borrowed alternate
/// boot backing still relies on its external architecture lifetime owner.
pub(super) unsafe fn observe(
    slot: usize,
    cpu: usize,
    saved_rsp: usize,
    executing: bool,
) -> Option<FrameSnapshot> {
    #[cfg(not(test))]
    return x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        observe_inner(slot, cpu, saved_rsp, executing)
    });
    #[cfg(test)]
    unsafe {
        observe_inner(slot, cpu, saved_rsp, executing)
    }
}
unsafe fn observe_inner(
    slot: usize,
    cpu: usize,
    saved_rsp: usize,
    executing: bool,
) -> Option<FrameSnapshot> {
    let token = frame_publication::observed_frame_token(slot, cpu, executing)?;
    if token.saved_rsp != saved_rsp
        || saved_rsp == 0
        || saved_rsp % mem::align_of::<SavedContext>() != 0
    {
        return None;
    }
    let end = saved_rsp.checked_add(SAVED_CONTEXT_BYTES)?;
    if !token.view.context_stack_contains(saved_rsp, end) {
        return None;
    }
    // Non-executing diagnostics cannot borrow unknown alternate backing.
    if !executing
        && !context_validation::stack_range_contains(
            token.view.kernel_stack_base,
            token.view.kernel_stack_top,
            saved_rsp,
            end,
        )
    {
        return None;
    }
    let task_id = current_identity::read_dispatch(slot)?.task_id?;
    let allocation = frame_publication::pin_for_identity(token, task_id)?;
    // SAFETY: observe() retains own-CPU IRQ exclusion throughout this copy.
    let frame_reader = unsafe { stack_resources::frame_readers::acquire(&allocation, cpu) }?;
    if !same_observation(
        token,
        frame_publication::observed_frame_token(slot, cpu, executing)?,
    ) {
        return None;
    }
    // SAFETY: exact initialized bounds, allocation pin, stable epoch and the
    // caller's documented byte-exclusion scope all span this POD copy.
    let saved = unsafe { context_validation::copy_saved_context(saved_rsp) }?;
    let snapshot = FrameSnapshot::from_saved(saved);
    if !same_observation(
        token,
        frame_publication::observed_frame_token(slot, cpu, executing)?,
    ) {
        return None;
    }
    drop(frame_reader);
    drop(allocation);
    Some(snapshot)
}

#[cfg(test)]
mod tests;
