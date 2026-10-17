//! Generation-bound Linux thread-state synchronization.
//!
//! Scheduler slots are reusable, but a process-state operation may need to
//! inspect or update one thread after releasing the scheduler owner. The
//! fixed lock stays at a stable address for the kernel lifetime; `owner_tid`
//! prevents a stale binding from acquiring authority over a reused slot.

pub(in crate::multitask) mod signals;

use core::ptr::NonNull;

use nucleus_core::util::lockdep::LockClass;
use crate::multitask::irq_leaf::IrqLeaf;

use super::{
    LinuxThreadState, MAX_TASK, ProcessHandle, UserAbi, process_table, runqueue,
    stack_resources,
};

pub(in crate::multitask) struct CurrentLinuxThreadBinding {
    pub(in crate::multitask) process_handle: ProcessHandle,
    pub(in crate::multitask) tid: u64,
    pub(in crate::multitask) abi: UserAbi,
    linux_thread_state: NonNull<LinuxThreadStateLock>,
    slot: usize,
}

#[derive(Clone, Copy)]
pub(super) struct LinuxThreadStateCell {
    pub(super) owner_tid: Option<u64>,
    pub(super) state: Option<LinuxThreadState>,
}

pub(super) type LinuxThreadStateLock =
    IrqLeaf<LinuxThreadStateCell, { LockClass::LinuxThreadState as u8 }>;

pub(super) const fn empty_linux_thread_state_lock() -> LinuxThreadStateLock {
    LinuxThreadStateLock::new(LinuxThreadStateCell {
        owner_tid: None,
        state: None,
    })
}

#[cfg(test)]
mod owner_tests;

static LINUX_THREAD_STATES: [LinuxThreadStateLock; MAX_TASK] =
    [const { empty_linux_thread_state_lock() }; MAX_TASK];

/// Final physical owner withdrawal while the resource Reuse keeps admission closed.
pub(in crate::multitask::scheduler) fn clear_reclaimed(slot: usize) {
    let mut cell = LINUX_THREAD_STATES[slot].lock();
    cell.owner_tid = None;
    cell.state = None;
    super::runqueue::simd_tls::clear_tls_fs_base(slot);
}

/// Exact unpublished producer initializes Linux/TLS state before its final Slot ACK.
pub(super) fn install_reserved(
    ticket: &stack_resources::admission::Ticket,
    state: Option<LinuxThreadState>,
) {
    assert!(
        ticket.is_committing(),
        "Linux state lacks committing producer"
    );
    let slot = ticket.slot();
    let mut cell = LINUX_THREAD_STATES[slot].lock();
    assert!(
        cell.owner_tid.is_none() && cell.state.is_none(),
        "Linux state replaced without reclaim"
    );
    cell.owner_tid = state.map(|_| ticket.task_id());
    cell.state = state;
    runqueue::simd_tls::set_tls_fs_base(slot, state.map(|state| state.fs_base).unwrap_or(0));
}

/// Original exec FrameWriter authenticates both the preserved signal mask and
/// replacement Linux/TLS state; no unowned slot or copied catalog participates.
pub(super) fn prepare_exec(
    writer: &stack_resources::frame_access::FrameWriter,
    slot: usize,
    id: u64,
    incoming: &mut Option<LinuxThreadState>,
) {
    writer.exec_stack_top(slot, id);
    let cell = LINUX_THREAD_STATES[slot].lock();
    assert!(
        cell.owner_tid.is_none_or(|owner| owner == id),
        "exec Linux state changed owner"
    );
    if let Some(state) = incoming {
        state.signal_mask = cell.state.map(|state| state.signal_mask).unwrap_or(0);
        state.pending_signals = 0;
        state.pending_sigchld_events = 0;
    }
}

pub(super) fn install_exec(
    writer: &stack_resources::frame_access::FrameWriter,
    slot: usize,
    id: u64,
    state: Option<LinuxThreadState>,
) {
    writer.exec_stack_top(slot, id);
    let mut cell = LINUX_THREAD_STATES[slot].lock();
    assert!(
        cell.owner_tid.is_none_or(|owner| owner == id),
        "exec Linux state changed owner"
    );
    cell.owner_tid = state.map(|_| id);
    cell.state = state;
    runqueue::simd_tls::set_tls_fs_base(slot, state.map(|state| state.fs_base).unwrap_or(0));
}

#[cfg(test)]
pub(super) fn test_exec_state(slot: usize, id: u64) -> Option<LinuxThreadState> {
    let cell = LINUX_THREAD_STATES[slot].lock();
    (cell.owner_tid == Some(id)).then_some(cell.state).flatten()
}

#[cfg(test)]
pub(super) fn test_update_exact(slot: usize, id: u64, update: impl FnOnce(&mut LinuxThreadState)) {
    let mut cell = LINUX_THREAD_STATES[slot].lock();
    assert_eq!(cell.owner_tid, Some(id));
    update(cell.state.as_mut().unwrap());
}

/// Scalar metadata only; caller retains original Current/target UID custody
/// across this exact cell read. Never an execution or recipient admission.
pub(super) fn exact_state(slot: usize, id: u64) -> Option<LinuxThreadState> {
    let cell = LINUX_THREAD_STATES.get(slot)?.lock();
    (cell.owner_tid == Some(id)).then_some(cell.state).flatten()
}

/// Exact original terminal source, whose seal excludes fresh wake borrowers.
pub(super) fn terminal_snapshot(
    owner: &super::retirement_cleanup::producer::Producer,
) -> Option<LinuxThreadState> {
    let cell = LINUX_THREAD_STATES.get(owner.slot())?.lock();
    (cell.owner_tid == owner.task_id()).then_some(cell.state).flatten()
}

/// Move process SIGCHLD under the original source seal and target binding pin.
/// No raw Linux/identity/resource leaf spans wake or queue publication.
///
/// # Safety
/// Caller retains original same-CPU masked IRQ/preemption and has drained
/// source borrowers under the original terminal seal before effects. Each
/// recipient is separately pinned through Linux and wake effects; candidates
/// alone cannot grant resource/frame access. No whole-kernel deletion is claimed.
pub(super) unsafe fn transfer_terminal_sigchld(
    owner: &super::retirement_cleanup::producer::Producer,
    eligible_cpus: &dyn Fn(usize) -> u64,
) {
    let retiring = owner.slot();
    let Some(source) = terminal_snapshot(owner) else { return; };
    let events = source.pending_sigchld_events;
    if events == 0 { return; }
    let Some(pid) = stack_resources::context::read(retiring).and_then(|c| c.process_id) else { return; };
    // The scan identifies candidates, never grants effects. Every chosen
    // candidate must capture the exact original UID before reading/writing it.
    let mut candidates = [(usize::MAX, 0_u64); MAX_TASK];
    let mut len = 0;
    for slot in 0..MAX_TASK {
        if slot == retiring { continue; }
        if let Some(start) = stack_resources::task_start::read(slot) {
            candidates[len] = (slot, start.id);
            len += 1;
        }
    }
    candidates[..len].sort_unstable_by_key(|&(slot, id)| (id != pid, slot));
    for &(slot, id) in &candidates[..len] {
        let Some(binding) = super::wake_publication::Binding::capture(slot, id) else { continue; };
        let context = binding.context();
        if !context.user_mode || context.user_abi != Some(UserAbi::Linux) || context.process_id != Some(pid) {
            continue;
        }
        let bit = crate::user::sysops::linux::linux_signal_bit(rustos_user_abi::linux::SIGCHLD)
            .expect("SIGCHLD must have a pending-signal bit");
        let wake = {
            let mut cell = LINUX_THREAD_STATES[slot].lock();
            if cell.owner_tid != Some(binding.id()) { continue; }
            let Some(state) = cell.state.as_mut() else { continue; };
            state.pending_sigchld_events |= events;
            state.pending_signals |= bit;
            crate::multitask::current_identity::raise_signal_pending(slot);
            state.signal_mask & bit == 0
        };
        // SAFETY: original caller custody; same original target loan survives
        // signal bits through queue effects, even if terminal begins meanwhile.
        if wake && !unsafe { super::wake_publication::guarded_owned(binding, eligible_cpus) } {
            return;
        }
        let mut source = LINUX_THREAD_STATES[retiring].lock();
        assert_eq!(source.owner_tid, owner.task_id(), "terminal SIGCHLD lost original source");
        if let Some(state) = source.state.as_mut() {
            state.pending_sigchld_events = 0;
            state.pending_signals &= !bit;
        }
        return;
    }
}

pub(in crate::multitask) fn current_binding() -> Option<CurrentLinuxThreadBinding> {
    let (tid, abi, process_handle, _) = super::stack_resources::context::current_user_binding()?;
    let slot = crate::multitask::cpu_local::current_cpu_task_slot()?;
    Some(CurrentLinuxThreadBinding {
        process_handle,
        tid,
        abi,
        linux_thread_state: NonNull::from(LINUX_THREAD_STATES.get(slot)?),
        slot,
    })
}

pub(in crate::multitask) fn current_state() -> Option<LinuxThreadState> {
    let binding = current_binding()?;
    if binding.abi != UserAbi::Linux {
        return None;
    }
    let lock = unsafe { binding.linux_thread_state.as_ref() };
    let cell = lock.lock();
    if cell.owner_tid != Some(binding.tid) {
        return None;
    }
    super::super::current_identity::sync_signal_pending(
        binding.slot,
        cell.state
            .is_some_and(|state| state.pending_signals != 0 || state.pending_sigchld_events != 0),
    );
    cell.state
}

impl CurrentLinuxThreadBinding {
    pub(in crate::multitask) fn with_thread_state_mut<R>(
        &self,
        f: impl FnOnce(&mut Option<LinuxThreadState>) -> R,
    ) -> Option<R> {
        // SAFETY: production storage is static, independent of Scheduler. Host
        // fixtures keep their directory alive. A reused TID cannot claim this cell.
        let lock = unsafe { self.linux_thread_state.as_ref() };
        let access = || {
            let mut cell = lock.lock();
            if cell.owner_tid != Some(self.tid) {
                return None;
            }
            let result = f(&mut cell.state);
            super::super::current_identity::sync_signal_pending(
                self.slot,
                cell.state.is_some_and(|state| {
                    state.pending_signals != 0 || state.pending_sigchld_events != 0
                }),
            );
            Some(result)
        };
        // IRQ exclusion lasts through the bounded callback, not just acquisition.
        // No IPC, user copy, blocking allocation or lower-ranked owner here.
        #[cfg(not(test))]
        {
            x86_64::instructions::interrupts::without_interrupts(access)
        }
        #[cfg(test)]
        {
            access()
        }
    }
}
