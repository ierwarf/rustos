//! Generation-bound publication of saved execution frames.
//!
//! The scheduler catalog owns frame construction.  This module owns only the
//! lock-free read view used by the authoritative local fair dispatcher. A reader obtains
//! a token only while the owner word says `Local`, the CPU has no current or
//! transition-stack ownership of the slot, and the publication names that
//! exact owner generation.  It must validate the returned view before it can
//! dereference `saved_rsp`.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};

use super::super::cpu_local;
use super::{
    MAX_TASK,
    context_validation::PublishedFrameView,
    runqueue::{self, RunOwnerState},
};

static VERSION: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static OWNER_GENERATION: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static SAVED_RSP: [AtomicUsize; MAX_TASK] = [const { AtomicUsize::new(0) }; MAX_TASK];
static KERNEL_STACK_BASE: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static KERNEL_STACK_TOP: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static ALTERNATE_STACK_BASE: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static ALTERNATE_STACK_TOP: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static FLAGS: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static SCHEDULER_STORAGE_BASE: [AtomicUsize; MAX_TASK] = [const { AtomicUsize::new(0) }; MAX_TASK];
static SCHEDULER_STORAGE_END: [AtomicUsize; MAX_TASK] = [const { AtomicUsize::new(0) }; MAX_TASK];

const FLAG_PUBLISHED: u64 = 1 << 0;
const FLAG_USER_MODE: u64 = 1 << 1;
const FLAG_KERNEL_PROCESS: u64 = 1 << 2;
const FLAG_STACK_CANARY_INTACT: u64 = 1 << 3;

/// A coherent, catalog-free saved-frame observation.
#[derive(Clone, Copy, Debug)]
pub(super) struct StableFrameToken {
    pub(super) slot: usize,
    pub(super) owner_generation: u64,
    pub(super) frame_epoch: u64,
    pub(super) saved_rsp: usize,
    pub(super) view: PublishedFrameView,
}

fn begin_write(slot: usize) -> u64 {
    let cell = VERSION
        .get(slot)
        .expect("scheduler frame publication slot exceeds capacity");
    let version = cell.load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "scheduler frame publication has concurrent writer slot={slot}"
    );
    // Readers must observe this odd epoch before any payload mutation.
    cell.store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    version
}

fn finish_write(slot: usize, version: u64) {
    // Release publishes every payload field to readers that accept this epoch.
    VERSION[slot].store(version.wrapping_add(2), Ordering::Release);
}

/// Publishes all catalog-derived facts required by the pure frame validator.
///
/// This is deliberately separate from runqueue custody: it may be refreshed
/// while a task is Running, but readers reject every non-`Local` owner before
/// accepting the frame.
#[allow(clippy::too_many_arguments)]
pub(super) fn publish(
    slot: usize,
    saved_rsp: usize,
    kernel_stack_base: u64,
    kernel_stack_top: u64,
    alternate_stack_base: u64,
    alternate_stack_top: u64,
    user_mode_task: bool,
    kernel_process_task: bool,
    stack_canary_intact: bool,
    scheduler_storage_base: usize,
    scheduler_storage_end: usize,
) {
    let version = begin_write(slot);
    SAVED_RSP[slot].store(saved_rsp, Ordering::Relaxed);
    KERNEL_STACK_BASE[slot].store(kernel_stack_base, Ordering::Relaxed);
    KERNEL_STACK_TOP[slot].store(kernel_stack_top, Ordering::Relaxed);
    ALTERNATE_STACK_BASE[slot].store(alternate_stack_base, Ordering::Relaxed);
    ALTERNATE_STACK_TOP[slot].store(alternate_stack_top, Ordering::Relaxed);
    SCHEDULER_STORAGE_BASE[slot].store(scheduler_storage_base, Ordering::Relaxed);
    SCHEDULER_STORAGE_END[slot].store(scheduler_storage_end, Ordering::Relaxed);
    let mut flags = FLAG_PUBLISHED;
    if user_mode_task {
        flags |= FLAG_USER_MODE;
    }
    if kernel_process_task {
        flags |= FLAG_KERNEL_PROCESS;
    }
    if stack_canary_intact {
        flags |= FLAG_STACK_CANARY_INTACT;
    }
    FLAGS[slot].store(flags, Ordering::Relaxed);
    finish_write(slot, version);
}

/// Binds an already complete frame publication to the owner generation that
/// just became locally runnable.  The binding is invalidated by every later
/// owner transition because the owner generation changes.
pub(super) fn bind_local_owner_generation(slot: usize, generation: u64) {
    assert_ne!(
        generation, 0,
        "scheduler frame publication has zero generation"
    );
    let version = begin_write(slot);
    OWNER_GENERATION[slot].store(generation, Ordering::Relaxed);
    finish_write(slot, version);
}

/// Clears frame authority before a retired slot becomes reusable.
pub(super) fn clear(slot: usize) {
    let version = begin_write(slot);
    OWNER_GENERATION[slot].store(0, Ordering::Relaxed);
    SAVED_RSP[slot].store(0, Ordering::Relaxed);
    KERNEL_STACK_BASE[slot].store(0, Ordering::Relaxed);
    KERNEL_STACK_TOP[slot].store(0, Ordering::Relaxed);
    ALTERNATE_STACK_BASE[slot].store(0, Ordering::Relaxed);
    ALTERNATE_STACK_TOP[slot].store(0, Ordering::Relaxed);
    FLAGS[slot].store(0, Ordering::Relaxed);
    SCHEDULER_STORAGE_BASE[slot].store(0, Ordering::Relaxed);
    SCHEDULER_STORAGE_END[slot].store(0, Ordering::Relaxed);
    finish_write(slot, version);
}

fn read(slot: usize) -> Option<StableFrameToken> {
    let version = VERSION.get(slot)?.load(Ordering::Acquire);
    if version & 1 != 0 {
        return None;
    }
    let owner_generation = OWNER_GENERATION[slot].load(Ordering::Relaxed);
    let saved_rsp = SAVED_RSP[slot].load(Ordering::Relaxed);
    let kernel_stack_base = KERNEL_STACK_BASE[slot].load(Ordering::Relaxed);
    let kernel_stack_top = KERNEL_STACK_TOP[slot].load(Ordering::Relaxed);
    let alternate_stack_base = ALTERNATE_STACK_BASE[slot].load(Ordering::Relaxed);
    let alternate_stack_top = ALTERNATE_STACK_TOP[slot].load(Ordering::Relaxed);
    let flags = FLAGS[slot].load(Ordering::Relaxed);
    let scheduler_storage_base = SCHEDULER_STORAGE_BASE[slot].load(Ordering::Relaxed);
    let scheduler_storage_end = SCHEDULER_STORAGE_END[slot].load(Ordering::Relaxed);
    // Neither payload load may move below this validation point.
    fence(Ordering::Acquire);
    if VERSION[slot].load(Ordering::Relaxed) != version || flags & FLAG_PUBLISHED == 0 {
        return None;
    }
    Some(StableFrameToken {
        slot,
        owner_generation,
        frame_epoch: version >> 1,
        saved_rsp,
        view: PublishedFrameView {
            user_mode_task: flags & FLAG_USER_MODE != 0,
            kernel_process_task: flags & FLAG_KERNEL_PROCESS != 0,
            kernel_stack_base,
            kernel_stack_top,
            alternate_stack_base,
            alternate_stack_top,
            stack_canary_intact: flags & FLAG_STACK_CANARY_INTACT != 0,
            scheduler_storage_base,
            scheduler_storage_end,
        },
    })
}

/// Returns a validator input only for a non-executing, non-transitioning local
/// owner.  Any publication race or generation mismatch is fail-closed.
pub(super) fn local_frame_token(slot: usize, cpu: usize) -> Option<StableFrameToken> {
    let owner = runqueue::owner(slot);
    if owner.state != RunOwnerState::Local || owner.cpu != Some(cpu) || !owner.runnable {
        return None;
    }
    if cpu_local::task_execution_owner(slot).is_some() {
        return None;
    }
    let token = read(slot)?;
    if token.owner_generation != owner.generation {
        return None;
    }
    // Close the race with a claim, migration, or CPU switch after the payload
    // snapshot. A new owner generation always requires a new token.
    if runqueue::owner(slot) != owner || cpu_local::task_execution_owner(slot).is_some() {
        return None;
    }
    Some(token)
}

/// Confirms that no writer changed the exact frame epoch after validation.
/// This is the final fail-closed publication check for the authoritative local
/// picker. The later dispatch claim retains exact queue and stack lifetime.
pub(super) fn token_is_current(token: StableFrameToken, cpu: usize) -> bool {
    let owner = runqueue::owner(token.slot);
    let expected_version = token.frame_epoch << 1;
    VERSION[token.slot].load(Ordering::Acquire) == expected_version
        && OWNER_GENERATION[token.slot].load(Ordering::Acquire) == token.owner_generation
        && owner.generation == token.owner_generation
        && owner.state == RunOwnerState::Local
        && owner.cpu == Some(cpu)
        && owner.runnable
        && cpu_local::task_execution_owner(token.slot).is_none()
}

pub(super) fn reset_before_publication() {
    for slot in 0..MAX_TASK {
        VERSION[slot].store(0, Ordering::Relaxed);
        OWNER_GENERATION[slot].store(0, Ordering::Relaxed);
        SAVED_RSP[slot].store(0, Ordering::Relaxed);
        KERNEL_STACK_BASE[slot].store(0, Ordering::Relaxed);
        KERNEL_STACK_TOP[slot].store(0, Ordering::Relaxed);
        ALTERNATE_STACK_BASE[slot].store(0, Ordering::Relaxed);
        ALTERNATE_STACK_TOP[slot].store(0, Ordering::Relaxed);
        FLAGS[slot].store(0, Ordering::Relaxed);
        SCHEDULER_STORAGE_BASE[slot].store(0, Ordering::Relaxed);
        SCHEDULER_STORAGE_END[slot].store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_local_generation_is_required_for_a_stable_frame_token() {
        let _guard = runqueue::test_serial_guard();
        reset_before_publication();
        runqueue::reset_before_publication();
        let slot = 43;
        publish(
            slot, 0x40_000, 0x30_000, 0x50_000, 0, 0, false, false, true, 1, 2,
        );
        runqueue::admit_blocked(slot);
        runqueue::publish_local(slot, 2, 100);
        assert!(local_frame_token(slot, 2).is_some());

        // A new owner generation cannot consume the previous stable frame.
        runqueue::claim_dispatch(slot, 2, 100);
        assert!(local_frame_token(slot, 2).is_none());
    }

    #[test]
    fn unpublished_or_stale_frame_never_becomes_local_authority() {
        let _guard = runqueue::test_serial_guard();
        reset_before_publication();
        runqueue::reset_before_publication();
        let slot = 44;
        runqueue::admit_blocked(slot);
        runqueue::publish_local(slot, 1, 100);
        assert!(local_frame_token(slot, 1).is_none());
    }

    #[test]
    fn executing_or_transition_stack_owners_reject_a_matching_local_frame() {
        let _cpu = cpu_local::test_publication_lock();
        let _runqueue = runqueue::test_serial_guard();
        reset_before_publication();
        runqueue::reset_before_publication();
        let slot = 45;
        publish(
            slot, 0x40_000, 0x30_000, 0x50_000, 0, 0, false, false, true, 1, 2,
        );
        runqueue::admit_blocked(slot);
        runqueue::publish_local(slot, 3, 100);

        let current = cpu_local::install_test_current_owner(3, slot);
        assert!(local_frame_token(slot, 3).is_none());
        drop(current);

        let transition = cpu_local::install_test_transition_owner(3, 1, slot);
        assert!(local_frame_token(slot, 3).is_none());
        drop(transition);
    }
}
