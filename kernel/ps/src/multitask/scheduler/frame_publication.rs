//! Generation-bound publication of saved execution frames.
//!
//! The scheduler catalog owns frame construction.  This module owns only the
//! lock-free read view used by the local fair and exact handoff dispatchers. A reader obtains
//! a token only while the owner word says `Local` or the explicitly requested
//! `DirectHandoff` state, the CPU has no current or
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

/// Pin backing storage and recheck publication before any byte read. A stale
/// token cannot borrow an image replaced between observation and pinning.
#[cfg(test)]
pub(super) fn pin_allocation(token: StableFrameToken) -> Option<super::stack_resources::StackPin> {
    let task_id = super::current_identity::read_dispatch(token.slot)?.task_id?;
    pin_for_identity(token, task_id)
}

pub(super) fn pin_for_identity(
    token: StableFrameToken,
    task_id: u64,
) -> Option<super::stack_resources::StackPin> {
    let pin = super::stack_resources::pin_view(token.slot, task_id, token.view)?;
    recheck_pin_epoch(token)?;
    Some(pin)
}

/// Same own-CPU IRQ/preemption contract as frame_readers::acquire. The owned
/// reader retains backing and byte exclusion after this helper returns.
pub(super) unsafe fn read_for_identity(
    token: StableFrameToken, task_id: u64, cpu: usize,
) -> Option<super::stack_resources::frame_readers::PinnedFrameReader> {
    let reader = unsafe {
        super::stack_resources::frame_readers::pin_and_acquire(token.slot, task_id, token.view, cpu)
    }?;
    recheck_pin_epoch(token)?;
    Some(reader)
}

pub(super) unsafe fn read_allocation(
    token: StableFrameToken, cpu: usize,
) -> Option<super::stack_resources::frame_readers::PinnedFrameReader> {
    let task_id = super::current_identity::read_dispatch(token.slot)?.task_id?;
    unsafe { read_for_identity(token, task_id, cpu) }
}

fn recheck_pin_epoch(token: StableFrameToken) -> Option<()> {
    if read(token.slot)?.frame_epoch != token.frame_epoch { return None; }
    Some(())
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

/// Revoke observations of the old image before its first byte mutation. The
/// caller already holds exclusive resource writer custody and its existing
/// cross-owner publication transaction; no payload can authorize new readers
/// until that resource writer has completed the replacement publication.
pub(super) fn invalidate_before_image_write(slot: usize) {
    let version = begin_write(slot);
    finish_write(slot, version);
}

/// Publishes all catalog-derived facts required by the pure frame validator.
///
/// This is deliberately separate from runqueue custody: it may be refreshed
/// while a task is Running, but readers reject every owner outside their
/// requested Local or exact DirectHandoff custody state.
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

/// Binds an already complete frame publication to its committed queue owner
/// generation, including a blocked outgoing return. Current/Transition banks
/// still exclude observation until RSP ACK. Later owner transitions must bind
/// their own generation; this header alone never grants execution.
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

#[cfg(test)]
pub(super) fn test_read(slot: usize) -> Option<StableFrameToken> {
    read(slot)
}

/// Scalar payload only; CurrentReturn must authenticate its original physical
/// bank and resource binding before using this view or reading any frame bytes.
pub(super) fn return_view(slot: usize) -> Option<PublishedFrameView> {
    Some(read(slot)?.view)
}

fn frame_custody_matches(
    published_generation: u64,
    owner: runqueue::RunOwnerSnapshot,
    state: RunOwnerState,
    cpu: usize,
    slot: usize,
) -> bool {
    let cpu_matches = owner.cpu == Some(cpu);
    let executing = cpu_local::task_execution_owner(slot).is_some();
    match state {
        RunOwnerState::Local => rustos_scheduler_frame_gate::local_frame_custody(
            published_generation,
            owner.generation,
            owner.state == RunOwnerState::Local,
            cpu_matches,
            owner.runnable,
            executing,
        ),
        RunOwnerState::DirectHandoff => rustos_scheduler_frame_gate::direct_handoff_frame_custody(
            published_generation,
            owner.generation,
            owner.state == RunOwnerState::DirectHandoff,
            cpu_matches,
            owner.runnable,
            executing,
        ),
        _ => false,
    }
}

fn frame_token_for_state(
    slot: usize,
    cpu: usize,
    state: RunOwnerState,
) -> Option<StableFrameToken> {
    let owner = runqueue::owner(slot);
    if !frame_custody_matches(owner.generation, owner, state, cpu, slot) {
        return None;
    }
    let token = read(slot)?;
    if !frame_custody_matches(token.owner_generation, owner, state, cpu, slot) {
        return None;
    }
    // Close the race with a claim, migration, or CPU switch after the payload
    // snapshot. A new owner generation always requires a new token.
    if runqueue::owner(slot) != owner || cpu_local::task_execution_owner(slot).is_some() {
        return None;
    }
    Some(token)
}

/// Returns a validator input only for a non-executing, non-transitioning local
/// owner. Any publication race or generation mismatch is fail-closed.
pub(super) fn local_frame_token(slot: usize, cpu: usize) -> Option<StableFrameToken> {
    frame_token_for_state(slot, cpu, RunOwnerState::Local)
}

/// The exact FIFO owner has no fair bitmap entry. Its frame must still name
/// the direct owner generation and be absent from every execution stack.
pub(super) fn direct_handoff_frame_token(slot: usize, cpu: usize) -> Option<StableFrameToken> {
    frame_token_for_state(slot, cpu, RunOwnerState::DirectHandoff)
}

/// The current CPU owns this *executing* frame, so its saved RSP is not the
/// new IRQ-saved RSP. Validate the fresh RSP against the published geometry
/// rather than comparing it to the old suspended-frame token. The token may
/// name the Local/Direct generation immediately before the Running claim or
/// the same Running generation after the current CPU published its frame.
#[cfg(test)]
pub(super) fn running_frame_view(slot: usize, cpu: usize) -> Option<PublishedFrameView> {
    let owner = runqueue::owner(slot);
    if owner.state != RunOwnerState::Running || owner.cpu != Some(cpu) {
        return None;
    }
    let token = read(slot)?;
    if !rustos_scheduler_frame_gate::running_frame_geometry_custody(
        token.owner_generation,
        owner.generation,
        cpu_local::task_execution_owner(slot) == Some(cpu_local::TaskExecutionOwner::Current(cpu)),
    ) {
        return None;
    }
    if runqueue::owner(slot) != owner
        || VERSION[slot].load(Ordering::Acquire) != token.frame_epoch << 1
        || OWNER_GENERATION[slot].load(Ordering::Acquire) != token.owner_generation
    {
        return None;
    }
    Some(token.view)
}

/// Header observation only, never dispatch permission. Tokens alone do not
/// exclude claims: observations retain the resource-leaf reader guard. Executing
/// reads additionally require this CPU's IRQ-stabilized Current/Prepared bank.
pub(super) fn observed_frame_token(
    slot: usize,
    cpu: usize,
    executing: bool,
) -> Option<StableFrameToken> {
    let owner = runqueue::owner(slot);
    let token = read(slot)?;
    if executing {
        let bank = super::stack_resources::cpu_leases::owns_current(cpu, slot)
            || super::stack_resources::cpu_leases::owns_prepared(cpu, slot);
        if owner.state != RunOwnerState::Running
            || owner.cpu != Some(cpu)
            || !rustos_scheduler_frame_gate::running_frame_geometry_custody(
                token.owner_generation,
                owner.generation,
                bank,
            )
            || cpu_local::task_execution_owner(slot)
                .is_some_and(|owner| owner != cpu_local::TaskExecutionOwner::Current(cpu))
        {
            return None;
        }
    } else {
        if !matches!(
            owner.state,
            RunOwnerState::Local
                | RunOwnerState::DirectHandoff
                | RunOwnerState::Blocked
                | RunOwnerState::RemoteQueued
        ) || token.owner_generation != owner.generation
            || cpu_local::task_execution_owner(slot).is_some()
        {
            return None;
        }
    }
    if runqueue::owner(slot) != owner
        || VERSION[slot].load(Ordering::Acquire) != token.frame_epoch << 1
        || OWNER_GENERATION[slot].load(Ordering::Acquire) != token.owner_generation
    {
        return None;
    }
    Some(token)
}

/// The outgoing task is Local again, but this CPU still holds its Current
/// execution custody until the switch commits. Other CPUs must not treat it
/// as an ordinary nonexecuting frame; only this exact CPU can retain it.
pub(super) fn current_local_frame_token(slot: usize, cpu: usize) -> Option<StableFrameToken> {
    let token = read(slot)?;
    current_local_token_is_current(token, cpu).then_some(token)
}

pub(super) fn current_local_token_is_current(token: StableFrameToken, cpu: usize) -> bool {
    let owner = runqueue::owner(token.slot);
    rustos_scheduler_frame_gate::retained_current_frame_custody(
        token.owner_generation,
        owner.generation,
        owner.state == RunOwnerState::Local && owner.cpu == Some(cpu),
        owner.runnable,
        cpu_local::task_execution_owner(token.slot)
            == Some(cpu_local::TaskExecutionOwner::Current(cpu)),
    ) && VERSION[token.slot].load(Ordering::Acquire) == token.frame_epoch << 1
        && OWNER_GENERATION[token.slot].load(Ordering::Acquire) == token.owner_generation
        && runqueue::owner(token.slot) == owner
        && cpu_local::task_execution_owner(token.slot)
            == Some(cpu_local::TaskExecutionOwner::Current(cpu))
}

/// Confirms that no writer changed the exact frame epoch after validation.
/// This is the final fail-closed publication check for the authoritative local
/// picker. The later dispatch claim retains exact queue and stack lifetime.
fn token_is_current_for_state(token: StableFrameToken, cpu: usize, state: RunOwnerState) -> bool {
    let owner = runqueue::owner(token.slot);
    let expected_version = token.frame_epoch << 1;
    VERSION[token.slot].load(Ordering::Acquire) == expected_version
        && OWNER_GENERATION[token.slot].load(Ordering::Acquire) == token.owner_generation
        && frame_custody_matches(token.owner_generation, owner, state, cpu, token.slot)
}

pub(super) fn token_is_current(token: StableFrameToken, cpu: usize) -> bool {
    token_is_current_for_state(token, cpu, RunOwnerState::Local)
}

pub(super) fn direct_handoff_token_is_current(token: StableFrameToken, cpu: usize) -> bool {
    token_is_current_for_state(token, cpu, RunOwnerState::DirectHandoff)
}

/// Original successful claim retains the physical bank; this is an epoch
/// check, not new Local/Direct admission or permission to read frame bytes.
pub(super) fn claimed_frame_is_current(token: StableFrameToken) -> bool {
    VERSION[token.slot].load(Ordering::Acquire) == token.frame_epoch << 1
        && OWNER_GENERATION[token.slot].load(Ordering::Acquire) == token.owner_generation
        && SAVED_RSP[token.slot].load(Ordering::Acquire) == token.saved_rsp
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
    fn direct_handoff_frame_is_bound_only_after_the_exact_owner_publication() {
        let _guard = runqueue::test_serial_guard();
        reset_before_publication();
        runqueue::reset_before_publication();
        let slot = 46;
        publish(
            slot, 0x40_000, 0x30_000, 0x50_000, 0, 0, false, false, true, 1, 2,
        );
        runqueue::admit_blocked(slot);
        assert!(direct_handoff_frame_token(slot, 2).is_none());
        assert_eq!(
            runqueue::publish_direct_handoff(slot, 2),
            runqueue::RemoteWakeOutcome::Published {
                cpu: 2,
                notify: false,
            }
        );
        let token = direct_handoff_frame_token(slot, 2).expect("exact direct frame");
        assert!(direct_handoff_token_is_current(token, 2));
        assert!(local_frame_token(slot, 2).is_none());
        assert!(runqueue::materialize_direct_handoff(slot, 2, 100));
        assert!(!direct_handoff_token_is_current(token, 2));
        assert!(local_frame_token(slot, 2).is_some());
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

    #[test]
    fn running_frame_uses_its_published_geometry_without_reusing_old_saved_rsp() {
        let _cpu = cpu_local::test_publication_lock();
        let _runqueue = runqueue::test_serial_guard();
        reset_before_publication();
        runqueue::reset_before_publication();
        let slot = 47;
        let cpu = 3;
        publish(
            slot, 0x40_000, 0x30_000, 0x50_000, 0, 0, false, false, true, 1, 2,
        );
        runqueue::admit_blocked(slot);
        runqueue::publish_local(slot, cpu, 100);
        assert!(runqueue::claim_dispatch(slot, cpu, 100));
        let current = cpu_local::install_test_current_owner(cpu, slot);
        let view = running_frame_view(slot, cpu).expect("current Running owner has a prior frame");
        assert_eq!(view.kernel_stack_base, 0x30_000);
        assert_eq!(view.kernel_stack_top, 0x50_000);
        assert!(running_frame_view(slot, cpu + 1).is_none());
        drop(current);
        assert!(running_frame_view(slot, cpu).is_none());
        let current = cpu_local::install_test_current_owner(cpu, slot);
        OWNER_GENERATION[slot].store(runqueue::owner(slot).generation - 2, Ordering::Release);
        assert!(running_frame_view(slot, cpu).is_none());
        drop(current);
    }

    #[test]
    fn retained_current_frame_requires_exact_local_and_execution_generation() {
        let _cpu = cpu_local::test_publication_lock();
        let _queue = runqueue::test_serial_guard();
        reset_before_publication();
        runqueue::reset_before_publication();
        let slot = 47;
        let cpu = 3;
        publish(
            slot, 0x40_000, 0x30_000, 0x50_000, 0, 0, false, false, true, 0, 0,
        );
        runqueue::admit_blocked(slot);
        runqueue::publish_local(slot, cpu, 100);
        assert!(current_local_frame_token(slot, cpu).is_none());
        let current = cpu_local::install_test_current_owner(cpu, slot);
        let token = current_local_frame_token(slot, cpu).unwrap();
        assert!(current_local_token_is_current(token, cpu));
        assert!(local_frame_token(slot, cpu).is_none());
        assert!(current_local_frame_token(slot, cpu + 1).is_none());
        runqueue::publish_blocked(slot, cpu, 100);
        runqueue::publish_local(slot, cpu, 100);
        assert!(!current_local_token_is_current(token, cpu));
        let fresh = current_local_frame_token(slot, cpu).unwrap();
        drop(current);
        assert!(!current_local_token_is_current(fresh, cpu));
        assert!(current_local_frame_token(slot, cpu).is_none());
    }
}
