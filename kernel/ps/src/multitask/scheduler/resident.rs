//! Owner: loans a Blocked IPC task keeps parked instead of releasing at the RSP ACK.
//! Boundary: one cell per slot, valid only for the owner generation it names.
//! Lifecycle: park at the ACK of a blocking return -> taken by the lane CAS, or
//! evicted by ANY other owner-word transition of that generation.
//! Concurrency: `generation` is the cell's single ownership word: `0` empty, a
//! generation published, `BUSY` while one CPU moves the payload. Every payload
//! access follows a winning CAS of that word, so there is no double drop and a
//! payload is never written while another CPU reads it. Nobody ever waits for
//! `BUSY`: a parker that finds the cell busy, or already holding a newer
//! generation, just releases its loans (the task stays non-resident), so no lock
//! order, IRQ state or stalled vCPU can make this protocol block.
//! Failure: a park racing a wake re-reads the owner after publishing and evicts itself;
//! a late revoking hook finds its older generation gone and does nothing.
//! Forbidden: reading the cell without the generation CAS, parking a non-Blocked
//! task, waiting on `BUSY`, or letting a cell outlive its generation.
//! Evidence: native residency witnesses in `irq_entry_tests/{switch,lane}.rs`;
//! ordering is argued here, not modeled in TLA+.
use super::{return_architecture::ArchitectureState, runqueue, stack_resources::StackPin};
use crate::multitask::current_identity::execution::ExecutionPermit;
use core::{cell::UnsafeCell, sync::atomic::{AtomicU64, AtomicUsize, Ordering}};
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
    /// Written only by the parker that holds the cell `BUSY`, read only by the
    /// unique winner of the generation CAS.
    arch: UnsafeCell<Option<ArchitectureState>>,
}
// SAFETY: `arch` is accessed only while the accessor owns the generation word (`BUSY`).
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
    /// Caller holds the cell `BUSY` after a winning CAS, so the payload is exclusively theirs.
    fn drain(&self) -> Loans {
        // ORDERING: the winning Acquire CAS of `generation` orders these loads
        // after the parker's publication of the same word.
        // Winning the cell made the payload exclusively ours; `park` overwrites it
        // before the next publication, so no clearing exchange is needed.
        let pin = self.pin.load(Ordering::Relaxed);
        let permit = self.permit.load(Ordering::Relaxed);
        Loans {
            pin: StackPin::from_raw(pin),
            permit: ExecutionPermit::take(permit).expect("resident cell lost its permit"),
            saved_rsp: self.saved_rsp.load(Ordering::Relaxed),
            // SAFETY: the winning generation CAS gives this caller exclusive access.
            arch: unsafe { (*self.arch.get()).take() }.expect("resident cell lost its architecture"),
            cpu: self.cpu.load(Ordering::Relaxed),
        }
    }
}

/// Cell word while one CPU owns the payload: a park filling it, or a take or
/// eviction draining it. Larger than any owner generation (those are packed below
/// 2^64 >> shift), so it never equals a live generation.
const BUSY: u64 = u64::MAX;

static CELLS: [CachePadded<Cell>; super::MAX_TASK] =
    [const { CachePadded::new(Cell::new()) }; super::MAX_TASK];

/// Park `loans` for `generation`. The loans are consumed either way: when the
/// owner already left that generation, or the cell is not available, they are
/// released here, outside any leaf. Returns whether the cell is still parked.
pub(super) fn park(slot: usize, generation: u64, loans: Loans) -> bool {
    let cell = &CELLS[slot];
    if !claim_empty(cell, generation) {
        return false;
    }
    let Loans { pin, permit, saved_rsp, arch, cpu } = loans;
    cell.pin.store(pin.into_raw(), Ordering::Relaxed);
    cell.permit.store(permit.park(), Ordering::Relaxed);
    cell.saved_rsp.store(saved_rsp, Ordering::Relaxed);
    cell.cpu.store(cpu, Ordering::Relaxed);
    // SAFETY: `claim_empty` left the cell `BUSY` and ours, so no taker can read it
    // and no other parker can write it before the swap below publishes it.
    unsafe { *cell.arch.get() = Some(arch) };
    // ORDERING: the SeqCst swap publishes the payload and, as a full barrier,
    // pairs with the SeqCst owner CAS and the SeqCst hook load so either the
    // evictor's hook sees this generation or our re-read below sees the evictor's
    // new owner word (never both missed).
    let previous = cell.generation.swap(generation, Ordering::SeqCst);
    assert_eq!(previous, BUSY, "resident cell lost its fill claim");
    let owner = runqueue::owner_sequenced(slot);
    if owner.generation == generation && owner.state == runqueue::RunOwnerState::Blocked {
        return true;
    }
    // The owner moved on between the ACK and publication. Whoever wins the cell
    // owns the loans; if the hook got there first they are gone.
    drop(take(slot, generation));
    false
}

/// Win the cell for `generation`, or `None` if it is empty or evicted.
pub(super) fn take(slot: usize, generation: u64) -> Option<Loans> {
    assert!(generation != 0 && generation != BUSY, "resident take of a non-generation");
    let cell = &CELLS[slot];
    cell.generation
        .compare_exchange(generation, BUSY, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let loans = cell.drain();
    // ORDERING: Release orders the payload reads in `drain` before any parker can
    // claim the empty cell (its claim CAS is Acquire).
    cell.generation.store(0, Ordering::Release);
    Some(loans)
}

/// Makes `cell` `BUSY` and empty for a park of `generation`, or reports that it
/// cannot be had right now (then the parker gives its loans up).
///
/// Two parks of one slot overlap only when the first is delayed between its RSP
/// ACK and its publication (a descheduled vCPU) while the task, already woken,
/// runs elsewhere and blocks again; and a revoking hook runs after the owner CAS
/// it revokes for, so it too can arrive after the task's next park. All of that
/// resolves by generation: a published older cell is stale (its owner generation
/// is gone, so nothing can use it) and is retired here by winning it; a busy
/// cell, or one already holding a newer generation, makes this park the obsolete
/// one, which only costs this task its residency.
fn claim_empty(cell: &Cell, generation: u64) -> bool {
    loop {
        match cell.generation.load(Ordering::Relaxed) {
            BUSY => return false,
            newer if newer > generation => return false,
            held => {
                assert_ne!(held, generation, "resident cell parked twice for one generation");
                // ORDERING: AcqRel orders a stale cell's payload reads (`drain`) after
                // its publication and a drainer's reads before our payload writes.
                if cell.generation.compare_exchange(held, BUSY, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                    if held != 0 {
                        drop(cell.drain());
                    }
                    return true;
                }
            }
        }
    }
}

/// Observation for admission decisions; grants nothing.
#[cfg(test)]
pub(super) fn is_resident(slot: usize, generation: u64) -> bool {
    CELLS[slot].generation.load(Ordering::Acquire) == generation
}

/// Raw cell word, for witnesses of the claim protocol.
#[cfg(test)]
pub(super) fn test_word(slot: usize) -> u64 {
    CELLS[slot].generation.load(Ordering::Acquire)
}

/// Marks an empty cell `BUSY` as a delayed taker or parker would hold it.
#[cfg(test)]
pub(super) fn test_hold_busy(slot: usize) -> bool {
    CELLS[slot].generation.compare_exchange(0, BUSY, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// Ends [`test_hold_busy`].
#[cfg(test)]
pub(super) fn test_release_busy(slot: usize) {
    assert_eq!(CELLS[slot].generation.swap(0, Ordering::AcqRel), BUSY);
}

/// Whether `slot`'s cell holds, or is moving, any loans.
pub(super) fn occupied(slot: usize) -> bool {
    CELLS[slot].generation.load(Ordering::Acquire) != 0
}

/// Owner-word CAS hook: the transition from `previous_generation` succeeded, so
/// a cell published for it can never be taken by the lane. Evict it.
#[inline]
pub(super) fn on_owner_transition(slot: usize, previous_generation: u64) {
    // ORDERING: SeqCst pairs with the parker's SeqCst publication swap (see `park`).
    if CELLS[slot].generation.load(Ordering::SeqCst) == previous_generation {
        drop(take(slot, previous_generation));
    }
}

/// CPU a parked cell was parked on (valid only for the caller's generation).
pub(super) fn parked_cpu(slot: usize, generation: u64) -> Option<usize> {
    let cell = &CELLS[slot];
    if cell.generation.load(Ordering::Acquire) != generation {
        return None;
    }
    let cpu = cell.cpu.load(Ordering::Acquire);
    // The payload is rewritten only after the cell was taken and refilled, which
    // changes the word; seeing the generation again proves `cpu` is its payload.
    (cell.generation.load(Ordering::Relaxed) == generation).then_some(cpu)
}

/// Reference units this slot's cell holds in the execution-permit count. A `BUSY`
/// cell holds none: a parker still fills it (its permit is not parked yet) and a
/// taker has already claimed the unit, so quiescence observers count it as live.
#[inline]
pub(in crate::multitask) fn permit_units(slot: usize) -> u32 {
    u32::from(CELLS.get(slot).is_some_and(|cell| {
        let word = cell.generation.load(Ordering::Acquire);
        word != 0 && word != BUSY
    }))
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

/// [`commit_outgoing`] for the lane's own switch, which hands the ACK the saved RSP it
/// published and the generation of the Blocked word it committed: nothing is re-read
/// that the lane wrote one step earlier. A wake that moved the owner since is caught
/// by `park`, which re-reads the owner after publishing the cell.
pub(in crate::multitask) fn commit_outgoing_lane(cpu: usize, from: usize, saved_rsp: usize, generation: u64) {
    use crate::multitask::current_identity::execution::banks;
    let pin = super::stack_resources::cpu_leases::take_outgoing(cpu);
    let permit = banks::take_outgoing(cpu);
    let (Some(pin), Some(permit)) = (pin, permit) else { return };
    if pin.slot() != from || permit.slot() != from || runqueue::is_idle_slot(from) {
        return;
    }
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

