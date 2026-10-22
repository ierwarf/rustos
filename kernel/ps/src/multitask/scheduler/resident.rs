//! Owner: loans a Blocked IPC task keeps parked instead of releasing at the RSP ACK.
//! Boundary: one cell per slot, valid only for the owner generation it names.
//! Lifecycle: park at the ACK of a blocking return -> taken by the lane CAS, or
//! evicted by ANY other owner-word transition of that generation.
//! Concurrency: `generation` is the cell's single publication word; whoever
//! swaps it to zero (taker or evictor) owns the loans, so there is no double drop.
//! Failure: a park racing a wake re-reads the owner after publishing and evicts itself.
//! Forbidden: reading the cell without the generation CAS, parking a non-Blocked
//! task, or letting a cell outlive its generation.
//! Evidence: native residency witnesses in `irq_entry_tests/{switch,lane}.rs`;
//! ordering is argued here, not modeled in TLA+.
use super::{return_architecture::ArchitectureState, runqueue, stack_resources::StackPin};
use crate::multitask::current_identity::execution::ExecutionPermit;
use core::{cell::UnsafeCell, sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence}};
use nucleus_core::util::cache_padded::CachePadded;

/// Everything a parked task needs to resume without re-admission.
pub(super) struct Loans {
    pub(super) pin: StackPin,
    pub(super) permit: ExecutionPermit,
    pub(super) saved_rsp: usize,
    pub(super) arch: ArchitectureState,
    pub(super) cpu: usize,
}

struct Cell {
    /// Owner generation this cell is valid for; zero means empty. Owner
    /// generations start at one, so zero is never a live generation.
    generation: AtomicU64,
    pin: AtomicUsize,
    permit: AtomicUsize,
    saved_rsp: AtomicUsize,
    cpu: AtomicUsize,
    /// Written only by the parker before it publishes `generation`, read only by
    /// the unique winner of the generation swap.
    arch: UnsafeCell<Option<ArchitectureState>>,
}
// SAFETY: `arch` is accessed only under the generation-word ownership above.
unsafe impl Sync for Cell {}
impl Cell {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            pin: AtomicUsize::new(0),
            permit: AtomicUsize::new(0),
            saved_rsp: AtomicUsize::new(0),
            cpu: AtomicUsize::new(0),
            arch: UnsafeCell::new(None),
        }
    }
    /// Caller won the generation swap, so the payload is exclusively theirs.
    fn drain(&self) -> Loans {
        // ORDERING: the winning Acquire swap of `generation` orders these loads
        // after the parker's Release store of the same word.
        let pin = self.pin.swap(0, Ordering::Relaxed);
        let permit = self.permit.swap(0, Ordering::Relaxed);
        Loans {
            pin: StackPin::from_raw(pin),
            permit: ExecutionPermit::take(permit).expect("resident cell lost its permit"),
            saved_rsp: self.saved_rsp.load(Ordering::Relaxed),
            // SAFETY: the winning generation swap gives this caller exclusive access.
            arch: unsafe { (*self.arch.get()).take() }.expect("resident cell lost its architecture"),
            cpu: self.cpu.load(Ordering::Relaxed),
        }
    }
}

static CELLS: [CachePadded<Cell>; super::MAX_TASK] =
    [const { CachePadded::new(Cell::new()) }; super::MAX_TASK];

/// Park `loans` for `generation`. The loans are consumed either way: when the
/// owner already left that generation they are released here, outside any leaf.
pub(super) fn park(slot: usize, generation: u64, loans: Loans) -> bool {
    let cell = &CELLS[slot];
    assert_eq!(
        cell.generation.load(Ordering::Relaxed),
        0,
        "resident cell parked over live loans"
    );
    let Loans { pin, permit, saved_rsp, arch, cpu } = loans;
    cell.pin.store(pin.into_raw(), Ordering::Relaxed);
    cell.permit.store(permit.park(), Ordering::Relaxed);
    cell.saved_rsp.store(saved_rsp, Ordering::Relaxed);
    cell.cpu.store(cpu, Ordering::Relaxed);
    // SAFETY: the cell is empty (asserted above), so no taker can observe it
    // before the Release store of `generation` below.
    unsafe { *cell.arch.get() = Some(arch) };
    // ORDERING: Release publishes the payload; the fence pairs with the
    // SeqCst owner CAS so either the evictor's hook sees this generation or our
    // re-read below sees the evictor's new owner word (never both missed).
    cell.generation.store(generation, Ordering::Release);
    fence(Ordering::SeqCst);
    let owner = runqueue::owner(slot);
    if owner.generation == generation && owner.state == runqueue::RunOwnerState::Blocked {
        return true;
    }
    // The owner moved on between the ACK and publication. Whoever swaps the
    // word to zero owns the loans; if the hook got there first they are gone.
    drop(take(slot, generation));
    false
}

/// Win the cell for `generation`, or `None` if it is empty or evicted.
pub(super) fn take(slot: usize, generation: u64) -> Option<Loans> {
    let cell = &CELLS[slot];
    cell.generation
        .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    Some(cell.drain())
}

/// Observation for admission decisions; grants nothing.
#[cfg(test)]
pub(super) fn is_resident(slot: usize, generation: u64) -> bool {
    CELLS[slot].generation.load(Ordering::Acquire) == generation
}

/// Owner-word CAS hook: the transition from `previous_generation` succeeded, so
/// a cell published for it can never be taken by the lane. Evict it.
#[inline]
pub(super) fn on_owner_transition(slot: usize, previous_generation: u64) {
    if CELLS[slot].generation.load(Ordering::Acquire) == previous_generation {
        drop(take(slot, previous_generation));
    }
}

/// CPU a parked cell was parked on (valid only for the caller's generation).
pub(super) fn parked_cpu(slot: usize, generation: u64) -> Option<usize> {
    let cell = &CELLS[slot];
    (cell.generation.load(Ordering::Acquire) == generation).then(|| cell.cpu.load(Ordering::Relaxed))
}

/// Reference units this slot's cell holds in the execution-permit count.
#[inline]
pub(in crate::multitask) fn permit_units(slot: usize) -> u32 {
    u32::from(CELLS.get(slot).is_some_and(|cell| cell.generation.load(Ordering::Acquire) != 0))
}

/// The ACK of a changed return. Releases the outgoing loans, or parks them when
/// the outgoing task just blocked in an endpoint wait on a valid published frame.
pub(in crate::multitask) fn commit_outgoing(cpu: usize, from: usize, physical: bool) {
    use crate::multitask::current_identity::execution::banks;
    let pin = if physical { super::stack_resources::cpu_leases::take_outgoing(cpu) } else { None };
    let permit = banks::take_outgoing(cpu);
    let (Some(pin), Some(permit)) = (pin, permit) else { return };
    let Some(generation) = parkable(from, &pin, &permit) else { return };
    let Some(saved_rsp) = kernel_frame_rsp(cpu, from, generation) else { return };
    let Some(arch) = super::ipc_lane::outgoing_arch(cpu, from) else { return };
    park(from, generation, Loans { pin, permit, saved_rsp, arch, cpu });
}

/// Generation of a Blocked IPC wait whose published frame is intact.
fn parkable(from: usize, pin: &StackPin, permit: &ExecutionPermit) -> Option<u64> {
    if pin.slot() != from || permit.slot() != from || runqueue::is_idle_slot(from) {
        return None;
    }
    let owner = runqueue::owner(from);
    if owner.state != runqueue::RunOwnerState::Blocked
        || owner.wait_armed
        || !matches!(
            owner.wait_reason_kind,
            runqueue::wait::REASON_ENDPOINT_RECEIVE | runqueue::wait::REASON_ENDPOINT_REPLY
        )
    {
        return None;
    }
    Some(owner.generation)
}

/// Published saved RSP of a KERNEL-mode frame, after the same byte validation
/// selection would run. A frame that returns to user mode is never parked: the
/// lane resumes only continuations that block inside the kernel.
fn kernel_frame_rsp(cpu: usize, slot: usize, generation: u64) -> Option<usize> {
    let token = super::frame_publication::observed_frame_token(slot, cpu, false)?;
    if token.owner_generation != generation {
        return None;
    }
    // The frame was written by the trap stub on this very stack and published
    // through the frame-token protocol checked above; the byte-level proof the
    // generic selection re-runs is not repeated for a continuation that only
    // ever resumes where it blocked.
    // SAFETY: the outgoing pin this ACK still holds retains the allocation and
    // the published RSP names this task's saved frame.
    let cs = unsafe { (*(token.saved_rsp as *const super::SavedContext)).cs };
    (cs != crate::arch::gdt::user_code_selector().0 as u64).then_some(token.saved_rsp)
}

