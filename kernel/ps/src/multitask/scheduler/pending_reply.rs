//! A completed fast IPC reply whose caller wake is deferred to the replier's next boundary.
//!
//! - **Owner:** this CPU's cell, written and read only by that CPU with IRQs masked.
//! - **Boundary:** the reply object already holds the response and the caller is still
//!   Blocked (resident) on this CPU; only the wake is outstanding. The cell carries the
//!   exact `(reply, completion)` the fused `REPLY_RECV` already carries across its own
//!   block transition, so a later receive hands it to `DeferredFastReply` unchanged.
//! - **Lifecycle:** `stash` (reply syscall) -> `take` by the same task's blocking receive
//!   (the reply rides the receiver's block, one lane reply switch) or `settle` (wake exactly
//!   as the reply syscall would have, one boundary later).
//! - **Concurrency:** every kernel entry other than that receive settles first (syscall
//!   entry, scheduler entry), so nothing else on the CPU observes a cell it does not own.
//! - **Failure:** a caller that is not parked on this CPU, an occupied cell or missing
//!   custody refuses the stash and the reply wakes immediately, as before.
//! - **Forbidden:** keeping a cell across a task switch, taking it for another task,
//!   waking from the cell without the reply's scheduling-context custody.
//! - **Evidence:** `tests` below; the differential against the unfused path is the
//!   `ipc_rt_intra_process` probe.
use super::{resident, runqueue, task_directory};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use kernel_ipc_runtime::api::ReplyCompletion;
use nucleus_core::util::cache_padded::CachePadded;
use nucleus_core::util::lockdep::MAX_TRACKED_CPUS;

#[derive(Clone, Copy)]
struct Pending {
    replier: u64,
    reply: u64,
    completion: ReplyCompletion,
}

struct Cell {
    present: AtomicBool,
    value: UnsafeCell<Option<Pending>>,
}

// SAFETY: element `cpu` is read and written only by `cpu` with its IRQs masked.
unsafe impl Sync for Cell {}

static CELLS: [CachePadded<Cell>; MAX_TRACKED_CPUS] = [const {
    CachePadded::new(Cell { present: AtomicBool::new(false), value: UnsafeCell::new(None) })
}; MAX_TRACKED_CPUS];

fn masked<R>(f: impl FnOnce(usize) -> R) -> R {
    #[cfg(not(test))]
    {
        x86_64::instructions::interrupts::without_interrupts(|| {
            f(nucleus_core::util::lockdep::current_cpu_index())
        })
    }
    #[cfg(test)]
    {
        f(nucleus_core::util::lockdep::current_cpu_index())
    }
}

/// Whether this CPU holds a deferred reply. A hint for the entry points: a set bit
/// is followed by a locked-out `take`, a clear one needs nothing.
#[inline]
pub(in crate::multitask) fn present() -> bool {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    // ORDERING: Relaxed; CPU-private, written by this CPU only.
    CELLS.get(cpu).is_some_and(|cell| cell.present.load(Ordering::Relaxed))
}

/// The caller sits Blocked on a reply wait on `cpu`: the case where holding its wake
/// back lets the replier's own block carry the switch. Anywhere else an immediate
/// wake is the faster answer.
fn caller_parked_on(caller: u64, cpu: usize) -> bool {
    let Some(slot) = task_directory::hint(caller) else { return false };
    let owner = runqueue::owner(slot);
    // A Blocked owner word names no CPU: the resident cell does (an IPC wait
    // parks its loans where it blocked), and a wait that parked nothing wakes
    // on the CPU it last ran on.
    owner.state == runqueue::RunOwnerState::Blocked
        && !owner.wait_armed
        && resident::parked_cpu(slot, owner.generation)
            .unwrap_or_else(|| usize::from(runqueue::affinity_payload::last_cpu(slot)))
            == cpu
}

/// Defer the caller wake of a completed fast reply. `false`: nothing was stored and
/// the caller must wake it now.
pub(in crate::multitask) fn stash(
    replier: u64, reply: u64, completion: ReplyCompletion,
) -> bool {
    if completion.scheduling_context.is_none() {
        return false;
    }
    masked(|cpu| {
        let Some(cell) = CELLS.get(cpu) else { return false };
        // SAFETY: see `Cell`.
        let slot = unsafe { &mut *cell.value.get() };
        if slot.is_some() || !caller_parked_on(completion.caller_task_id, cpu) {
            return false;
        }
        *slot = Some(Pending { replier, reply, completion });
        // ORDERING: Relaxed; CPU-private.
        cell.present.store(true, Ordering::Relaxed);
        true
    })
}

fn take_with(cpu: usize, only_for: Option<u64>) -> Option<(u64, ReplyCompletion)> {
    let cell = CELLS.get(cpu)?;
    // SAFETY: see `Cell`.
    let slot = unsafe { &mut *cell.value.get() };
    let pending = (*slot)?;
    if only_for.is_some_and(|task| task != pending.replier) {
        return None;
    }
    *slot = None;
    // ORDERING: Relaxed; CPU-private.
    cell.present.store(false, Ordering::Relaxed);
    Some((pending.reply, pending.completion))
}

/// The deferred reply of the task `replier`, if it is the one that stashed it.
pub(in crate::multitask) fn take_for(replier: u64) -> Option<(u64, ReplyCompletion)> {
    if !present() {
        return None;
    }
    masked(|cpu| take_with(cpu, Some(replier)))
}

/// Whatever this CPU holds, whoever stashed it.
pub(in crate::multitask) fn take_any() -> Option<(u64, ReplyCompletion)> {
    if !present() {
        return None;
    }
    masked(|cpu| take_with(cpu, None))
}

/// Forget every cell (boot reset, before any CPU publishes).
pub(in crate::multitask) fn reset() {
    for cell in &CELLS {
        // SAFETY: boot reset precedes any other CPU's access.
        unsafe { *cell.value.get() = None };
        cell.present.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_is_taken_once_by_its_replier_and_custody_is_required() {
        reset();
        masked(|cpu| {
            let cell = &CELLS[cpu];
            // SAFETY: test owns this CPU index.
            unsafe {
                *cell.value.get() = Some(Pending {
                    replier: 7,
                    reply: 9,
                    completion: ReplyCompletion { caller_task_id: 3, scheduling_context: None },
                })
            };
            cell.present.store(true, Ordering::Relaxed);
            assert!(take_with(cpu, Some(8)).is_none());
            assert!(cell.present.load(Ordering::Relaxed));
            assert_eq!(take_with(cpu, Some(7)).map(|(reply, _)| reply), Some(9));
            assert!(take_with(cpu, None).is_none());
            assert!(!cell.present.load(Ordering::Relaxed));
        });
        // Same test: the cells are process-wide statics.
        let completion = ReplyCompletion { caller_task_id: 3, scheduling_context: None };
        assert!(!stash(7, 9, completion), "a stash without scheduling-context custody is refused");
        assert!(!present());
    }
}
