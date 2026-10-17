//! Owner: durable independent suspended-thread materialization producers.
//! Boundary: actual Ticket/original MM move into a row before caller safe points.
//! Lifecycle: checked nonce, exclusive kernel claim, complete cancellation/Slot ACK.
//! Concurrency: Class50 row leaves never cross process state, allocation or effects.
//! Failure: untouched work survives caller death; abandoned kernel effects poison.
//! Forbidden: killable user loans, receipt authority, MM root without generation.
//! Evidence: actual shared reservation, MM activation, frame and membership owners.
use super::super::retirement_cleanup::executor::Executor;
use super::*;
use nucleus_core::util::lockdep::LockClass;
use crate::multitask::irq_leaf::IrqLeaf;

struct Work {
    reservation: Option<Reservation>,
    bootstrap: UserTaskBootstrap,
    selectors: (u64, u64, u64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Vacant,
    Pending,
    Executing,
    Complete,
    Poisoned,
}
struct Row {
    nonce: u64,
    id: u64,
    phase: Phase,
    work: Option<Work>,
    succeeded: bool,
}
impl Row {
    const fn new() -> Self {
        Self {
            nonce: 0,
            id: 0,
            phase: Phase::Vacant,
            work: None,
            succeeded: false,
        }
    }
}
// Same cold-producer Class50 rank as terminal receipts; distinct per-slot rows.
// No operation nests these leaves with cleanup, identity, resource or MM leaves.
static ROWS: [IrqLeaf<Row, { LockClass::SchedulerRetirementCleanup as u8 }>; MAX_TASK] =
    [const { IrqLeaf::new(Row::new()) }; MAX_TASK];
#[derive(Clone, Copy)]
pub(in crate::multitask) struct Receipt {
    slot: usize,
    id: u64,
    nonce: u64,
}

fn enqueue(work: Work) -> Result<Receipt, Work> {
    let slot = work.reservation.as_ref().unwrap().ticket().slot();
    enqueue_into(&mut ROWS[slot].lock(), work)
}
fn enqueue_into(row: &mut Row, work: Work) -> Result<Receipt, Work> {
    let r = work.reservation.as_ref().unwrap();
    let slot = r.ticket().slot();
    let id = r.ticket().task_id();
    if !matches!(row.phase, Phase::Vacant | Phase::Complete) || row.work.is_some() {
        return Err(work);
    }
    let Some(nonce) = row.nonce.checked_add(1) else {
        return Err(work);
    };
    assert!(r.ticket().is_reserved_current());
    row.nonce = nonce;
    row.id = id;
    row.succeeded = false;
    row.work = Some(work);
    row.phase = Phase::Pending;
    Ok(Receipt { slot, id, nonce })
}
/// One non-yielding capture/transfer: never expose an actual reservation on a
/// killable caller stack at an IRQ/preemption safe point.
pub(in crate::multitask) fn submit(
    id: u64,
    bootstrap: UserTaskBootstrap,
    selectors: (u64, u64, u64),
) -> Option<Receipt> {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    #[cfg(not(test))]
    assert!(
        super::super::super::cpu_local::scheduler_initialized(),
        "thread creation before scheduler bootstrap"
    );
    if !super::super::super::cpu_local::current_cpu_task_slot_admitted() {
        return None;
    }
    let slot = super::super::super::cpu_local::current_cpu_task_slot()?;
    let creator = capture_creator(slot)?;
    let reservation = reserve_with(id, &creator)?;
    let work = Work {
        reservation: Some(reservation),
        bootstrap,
        selectors,
    };
    match enqueue(work) {
        Ok(receipt) => Some(receipt),
        Err(mut work) => {
            cancel_owned(work.reservation.take().unwrap(), &preemption);
            None
        }
    }
}
/// Observation only: neither consumption nor caller death releases ownership.
pub(in crate::multitask) fn poll(receipt: Receipt) -> Option<bool> {
    let row = ROWS[receipt.slot].lock();
    if row.nonce != receipt.nonce || row.id != receipt.id {
        return Some(false);
    }
    match row.phase {
        Phase::Complete => Some(row.succeeded),
        Phase::Poisoned | Phase::Vacant => Some(false),
        Phase::Pending | Phase::Executing => None,
    }
}
struct Claim<'a> {
    executor: &'a Executor,
    receipt: Receipt,
    work: Option<Work>,
    live: bool,
}
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let mut row = ROWS[self.receipt.slot].lock();
        assert!(
            row.nonce == self.receipt.nonce
                && row.id == self.receipt.id
                && row.phase == Phase::Executing
                && row.work.is_none()
        );
        // Preserve real Ticket/MM custody after any abandoned effect scope.
        // Never turn an unknown TEB/frame prefix into an implicit cancel ACK.
        row.work = self.work.take();
        row.phase = Phase::Poisoned;
    }
}
fn claim_next(executor: &Executor) -> Option<Claim<'_>> {
    executor.assert_current();
    for (slot, cell) in ROWS.iter().enumerate().skip(FIRST_DYNAMIC_TASK_SLOT) {
        let mut row = cell.lock();
        if row.phase != Phase::Pending {
            continue;
        }
        let receipt = Receipt {
            slot,
            id: row.id,
            nonce: row.nonce,
        };
        let work = row.work.take()
            .expect("pending producer lost actual capsule");
        row.phase = Phase::Executing;
        return Some(Claim {
            executor,
            receipt,
            work: Some(work),
            live: true,
        });
    }
    None
}
/// Qualify *after* ProcessStateLock, retaining old-MM activation and the actual
/// current full process/MM generation together through every materialization byte.
fn with_current_mm<R>(r: &Reservation, state_root: u64, f: impl FnOnce() -> R) -> Option<R> {
    let activation = r.mm.as_ref()?.try_activate()?;
    let current = process_table::pin_process_mm(r.handle, r.pid)?;
    if state_root != r.root || !activation.matches(&current) || activation.root() != state_root {
        return None;
    }
    Some(f())
}
impl Claim<'_> {
    fn complete(mut self) {
        self.executor.assert_current();
        let work = self.work.as_mut().unwrap();
        let r = work.reservation.as_ref().unwrap();
        let materialized = if let Some(thread) = work.bootstrap.windows_thread_state {
            process_table::with_process_state_mut(r.handle, |_, state| {
                let root = state.address_space().root_phys().as_u64();
                with_current_mm(r, root, || {
                    crate::user::process::initialize_windows_thread_identifiers(
                        state.address_space_mut(),
                        thread.teb_address,
                        r.pid,
                        self.receipt.id,
                    ).is_ok()
                })
                .unwrap_or(false)
            })
            .unwrap_or(false)
        } else {
            true
        };
        self.executor.assert_current();
        let r = work.reservation.take().unwrap();
        let succeeded = if materialized {
            commit(r,
                work.bootstrap.clone(),
                work.selectors.0,
                work.selectors.1,
                work.selectors.2,
            )
            .is_some()
        } else {
            cancel(r);
            false
        };
        self.executor.assert_current();
        // No successful/failed receipt escapes before the real producer ACK.
        let mut row = ROWS[self.receipt.slot].lock();
        assert!(
            row.nonce == self.receipt.nonce
                && row.id == self.receipt.id
                && row.phase == Phase::Executing
                && row.work.is_none()
        );
        row.succeeded = succeeded;
        row.phase = Phase::Complete;
        self.live = false;
    }
}
/// Bounded housekeeping work; the actual kernel Executor loan spans state-lock
/// waits and all byte effects, cancellation, Slot publication and durable ACK.
pub(in crate::multitask) fn service(executor: &Executor) -> usize {
    let Some(claim) = claim_next(executor) else {
        return 0;
    };
    claim.complete();
    1
}
#[cfg(test)]
mod tests;
