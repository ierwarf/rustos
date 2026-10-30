//! Exact process membership and committed placement across exec ABI changes. Leaves never span
//! frame work, callbacks, queues, process-table acquisition or destruction.
use super::{current_identity, process_table};
use crate::user::abi::UserAbi;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use process_table::ProcessHandle;
use crate::multitask::slot_dir::AtomicDir;
const MAX: usize = process_table::MAX_THREADS_PER_PROCESS;
type Member = (usize, u64);
struct State {
    handle: Option<ProcessHandle>,
    mask: u64,
    nonce: u64,
    phase: u8, // 0 open, 1 metadata sealed, 2 published, 3 poisoned, 4 exec census
    joins: u32,
    members: [Option<Member>; MAX],
    stopped: [bool; MAX],
    exit_detached: [bool; MAX],
    exec_detached: [bool; MAX],
    terminal_detached: [bool; MAX],
    reclaiming: [Option<u64>; MAX],
    terminal: Option<terminal::Record>,
    terminal_nonce: u64,
    exit: Option<exit_membership::Record>,
    exit_nonce: u64,
    exit_work: Option<exit_membership::Work>,
    exit_done_nonce: u64,
    root: Option<root_membership::Record>,
    root_nonce: u64,
    root_process: Option<process_table::pin_owner::ProcessObjectPin>,
}
impl State {
    const fn new() -> Self {
        Self {
            handle: None,
            mask: 0,
            nonce: 0,
            phase: 0,
            joins: 0,
            members: [None; MAX],
            stopped: [false; MAX],
            exit_detached: [false; MAX],
            exec_detached: [false; MAX],
            terminal_detached: [false; MAX],
            reclaiming: [None; MAX],
            terminal: None,
            terminal_nonce: 0,
            exit: None,
            exit_nonce: 0,
            exit_work: None,
            exit_done_nonce: 0,
            root: None,
            root_nonce: 0,
            root_process: None,
        }
    }
    fn insert(&mut self, member: Member) -> Option<()> {
        if member.0 >= super::MAX_SCHEDULER_TASKS {
            return None;
        }
        if self
            .members
            .iter()
            .flatten()
            .any(|&(slot, id)| slot == member.0 || id == member.1)
        {
            return None;
        }
        let index = self.members.iter().position(Option::is_none)?;
        self.members[index] = Some(member);
        self.stopped[index] = false;
        self.exit_detached[index] = false;
        self.exec_detached[index] = false;
        self.terminal_detached[index] = false;
        self.reclaiming[index] = None;
        Some(())
    }
    fn prune_exit_detached(&mut self) {
        assert!(
            self.exit.is_none(),
            "detached exit rows pruned before exact release"
        );
        for i in 0..MAX {
            if self.exit_detached[i] {
                assert!(self.stopped[i], "exit tombstone lacks committed stop");
                if self.phase == 4 {
                    self.exec_detached[i] = true;
                    self.exit_detached[i] = false;
                } else if self.terminal.is_some() {
                    self.terminal_detached[i] = true;
                    self.exit_detached[i] = false;
                } else {
                    self.remove_member(i);
                }
            }
        }
    }
    fn remove_member(&mut self, index: usize) {
        self.members[index] = None;
        self.stopped[index] = false;
        self.exit_detached[index] = false;
        self.exec_detached[index] = false;
        self.terminal_detached[index] = false;
        self.reclaiming[index] = None;
    }
    fn unregister_member(&mut self, index: usize) {
        let state = self;
        assert!(
            state.root.is_none()
                && ((state.terminal.is_none() && state.exit.is_none()) || state.stopped[index]),
            "live process member detached across terminal turn"
        );
        if state.exit.is_some() {
            state.exit_detached[index] = true;
        } else if state.phase == 4 {
            // Keep the exact frozen UID as a reclaim fence even after logical detach.
            state.exec_detached[index] = true;
        } else if state.terminal.is_some() {
            state.terminal_detached[index] = true;
        } else {
            state.remove_member(index);
        }
    }
    fn prune_terminal_detached(&mut self) {
        assert!(self.terminal.is_none(), "terminal census pruned before original release");
        for index in 0..MAX {
            if self.terminal_detached[index] {
                if self.exit.is_some() {
                    self.exit_detached[index] = true;
                    self.terminal_detached[index] = false;
                } else if self.phase == 4 {
                    self.exec_detached[index] = true;
                    self.terminal_detached[index] = false;
                } else {
                    self.remove_member(index);
                }
            }
        }
    }
    fn release_empty(&mut self) {
        if self.root.is_none()
            && self.root_process.is_none()
            && self.terminal.is_none()
            && self.exit.is_none()
            && self.exit_work.is_none()
            && self.phase == 0
            && self.joins == 0
            && self.members.iter().all(Option::is_none)
        {
            self.handle = None;
            self.mask = 0;
        }
    }
}
struct Cell(TrackedSpinLock<State, { LockClass::ProcessPlacement as u8 }>);
impl Cell {
    const fn new() -> Self {
        Self(TrackedSpinLock::new(State::new()))
    }
    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        // This bounded scalar leaf is called WITH an original dispatch/Current
        // owner as well as from cold paths. It must never acquire a second
        // exclusive dispatch unit. Mask local IRQs before the tracked raw leaf;
        // its existing preemption accounting and rank still apply. Only the
        // privileged mask differs in native tests, not the transaction owner.
        let protected = || f(&mut self.0.lock());
        #[cfg(rustos_boot_image)]
        {
            x86_64::instructions::interrupts::without_interrupts(protected)
        }
        #[cfg(not(rustos_boot_image))]
        {
            protected()
        }
    }
}
static CELLS: AtomicDir<Cell> = AtomicDir::new();

/// Publishes the placement cells covering `index` before that slot can exist.
pub(super) fn ensure_slot_storage(index: usize) -> bool {
    CELLS.ensure(index, Cell::new)
}
#[cfg(test)]
pub(super) fn cell_exists_for_tests(handle: ProcessHandle) -> bool {
    cell(handle).is_some()
}
fn cell(handle: ProcessHandle) -> Option<&'static Cell> {
    CELLS.get(handle.index())
}
/// Native already-published catalog projection; production holds RootJoin through Ticket ACK.
#[cfg(test)]
pub(super) fn register_root(handle: ProcessHandle, slot: usize, id: u64, mask: u64) -> Option<()> {
    cell(handle)?.with(|state| {
        if mask == 0
            || state.phase != 0
            || state.joins != 0
            || state.terminal.is_some()
            || state.exit.is_some()
        {
            return None;
        }
        if state.handle.is_some() && state.handle != Some(handle) {
            return None;
        }
        #[cfg(not(test))]
        if state.members.iter().any(Option::is_some) {
            return None;
        }
        if state.handle == Some(handle) && state.mask != mask {
            return None;
        }
        state.insert((slot, id))?;
        state.handle = Some(handle);
        state.mask = mask;
        Some(())
    })
}
#[cfg(test)]
pub(super) fn unregister(handle: ProcessHandle, slot: usize, id: u64) {
    let Some(cell) = cell(handle) else {
        return;
    };
    cell.with(|state| {
        if state.handle != Some(handle) {
            return;
        }
        if let Some(index) = state
            .members
            .iter()
            .position(|&row| row == Some((slot, id)))
        {
            state.unregister_member(index);
        }
        state.release_empty();
    });
}
pub(super) fn mask(handle: ProcessHandle) -> Option<u64> {
    cell(handle)?.with(|state| {
        (state.handle == Some(handle) && state.phase != 3 && state.mask != 0).then_some(state.mask)
    })
}
/// Physical process pin is independent of the logical group transaction.
pub(super) struct Change {
    handle: ProcessHandle,
    caller: Member,
    nonce: u64,
    members: [Member; MAX],
    stopped: [bool; MAX],
    len: usize,
    previous: u64,
    effects: bool,
    live: bool,
    _process: Option<process_table::pin_owner::ProcessObjectPin>,
}
impl Change {
    pub(super) fn begin(handle: ProcessHandle, caller: Member) -> Option<Self> {
        let process = process_table::pin_owner::pin(handle);
        #[cfg(not(test))]
        if process.is_none() {
            return None;
        }
        let (nonce, members, stopped, len, previous) = cell(handle)?.with(|state| {
            if state.handle != Some(handle)
                || state.phase != 0
                || state.terminal.is_some()
                || state.exit.is_some()
                || state.joins != 0
                || !state
                    .members
                    .iter()
                    .zip(state.stopped)
                    .any(|(&row, stopped)| row == Some(caller) && !stopped)
            {
                return None;
            }
            let nonce = state.nonce.checked_add(1)?;
            let mut members = [(0, 0); MAX];
            let mut stopped = [false; MAX];
            let mut len = 0;
            for (&member, is_stopped) in state.members.iter().zip(state.stopped) {
                if let Some(member) = member {
                    members[len] = member;
                    stopped[len] = is_stopped;
                    len += 1;
                }
            }
            state.nonce = nonce;
            state.phase = 1;
            Some((nonce, members, stopped, len, state.mask))
        })?;
        Some(Self {
            handle,
            caller,
            nonce,
            members,
            stopped,
            len,
            previous,
            effects: false,
            live: true,
            _process: process,
        })
    }
    #[cfg(test)]
    pub(super) fn members(&self) -> &[Member] {
        &self.members[..self.len]
    }
    /// Exact terminal ACK captured in the same Class55 turn as membership.
    /// A missing descriptor or a replacement UID is never negative evidence.
    pub(super) fn live_members(&self) -> impl Iterator<Item = Member> + '_ {
        self.members[..self.len]
            .iter()
            .copied()
            .zip(self.stopped)
            .filter_map(|(member, stopped)| (!stopped).then_some(member))
    }
    pub(super) fn previous(&self) -> u64 {
        self.previous
    }
    pub(super) fn process_pin(&self) -> Option<&process_table::pin_owner::ProcessObjectPin> {
        self._process.as_ref()
    }
    pub(super) fn begin_effects(&mut self) {
        self.effects = true;
    }
    /// The release callback is exactly Cohort's atomic publication, no leaves.
    pub(super) fn publish(&self, mask: u64, cohort: &mut current_identity::cohort::Cohort) {
        self.publish_metadata(Some(mask), cohort);
    }
    /// Job control shares the original census, not affinity write authority.
    pub(super) fn publish_job_control(&self, cohort: &mut current_identity::cohort::Cohort) {
        self.publish_metadata(None, cohort);
    }
    fn publish_metadata(&self, mask: Option<u64>, cohort: &mut current_identity::cohort::Cohort) {
        cell(self.handle).unwrap().with(|state| {
            assert!(
                self.live
                    && self.effects
                    && state.handle == Some(self.handle)
                    && state.nonce == self.nonce
                    && state.phase == 1
            );
            if let Some(mask) = mask {
                assert_ne!(mask, 0);
                assert!(
                    cohort.owns_affinity(self.caller.0, self.caller.1),
                    "process publication lacks exact affinity cohort"
                );
                state.mask = mask;
            } else {
                assert!(cohort.owns_job_control(self.caller.0, self.caller.1),
                    "process publication lacks exact job-control cohort");
            }
            cohort.publish_admission();
            state.phase = 2;
        });
    }
    pub(super) fn finish(mut self) {
        cell(self.handle).unwrap().with(|state| {
            assert!(
                state.handle == Some(self.handle) && state.nonce == self.nonce && state.phase == 2
            );
            state.phase = 0;
            state.release_empty();
        });
        self.live = false;
    }
}
impl Drop for Change {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        cell(self.handle).unwrap().with(|state| {
            assert!(state.handle == Some(self.handle) && state.nonce == self.nonce);
            state.phase = if self.effects { 3 } else { 0 };
            state.release_empty();
        });
        if self.effects {
            core::mem::forget(self._process.take());
        }
    }
}
/// Captured creation admission spans first frame effects through membership and
/// suspended publication; a policy writer cannot overtake the inherited mask.
pub(super) struct Join {
    handle: ProcessHandle,
    mask: u64,
    _parent: current_identity::execution::BindingPin,
}
impl Join {
    pub(super) fn begin(handle: ProcessHandle, parent: Member, abi: UserAbi) -> Option<Self> {
        let pin = current_identity::execution::pin_creation(parent.0, parent.1, handle, abi)?;
        let mask = cell(handle)?.with(|state| {
            if state.handle != Some(handle)
                || state.phase != 0
                || state.terminal.is_some()
                || state.exit.is_some()
                || state.root.is_some()
                || !state
                    .members
                    .iter()
                    .zip(state.stopped)
                    .any(|(&row, stopped)| row == Some(parent) && !stopped)
            {
                return None;
            }
            state.joins = state.joins.checked_add(1)?;
            Some(state.mask)
        })?;
        Some(Self {
            handle,
            mask,
            _parent: pin,
        })
    }
    pub(super) fn mask(&self) -> u64 {
        self.mask
    }
    pub(super) fn register(&self, slot: usize, id: u64) -> Option<()> {
        cell(self.handle)?.with(|state| {
            if state.handle != Some(self.handle)
                || state.phase != 0
                || state.terminal.is_some()
                || state.exit.is_some_and(|exit| !exit.permits_captured_join())
                || state.joins == 0
                || state.mask != self.mask
            {
                return None;
            }
            state.insert((slot, id))
        })
    }
}
impl Drop for Join {
    fn drop(&mut self) {
        cell(self.handle).unwrap().with(|state| {
            assert!(state.handle == Some(self.handle) && state.joins != 0);
            state.joins -= 1;
            state.release_empty();
        }); // parent binding pin drops after the leaf, never CPU permission
    }
}
#[cfg(test)]
pub(super) fn reset_for_tests() {
    for cell in CELLS.iter() {
        cell.with(|state| {
            assert_eq!(state.phase, 0);
            assert_eq!(state.joins, 0);
            assert!(state.terminal.is_none() && state.exit.is_none() && state.exit_work.is_none());
            assert!(state.root.is_none() && state.root_process.is_none());
            let root_nonce = state.root_nonce;
            let exit_nonce = state.exit_nonce;
            let exit_done_nonce = state.exit_done_nonce;
            let nonce = state.nonce;
            let terminal_nonce = state.terminal_nonce;
            *state = State::new();
            state.nonce = nonce;
            state.root_nonce = root_nonce;
            state.terminal_nonce = terminal_nonce;
            state.exit_nonce = exit_nonce;
            state.exit_done_nonce = exit_done_nonce;
        });
    }
}
pub(in crate::multitask) mod reclaim;
pub(in crate::multitask) mod exec_membership;
pub(in crate::multitask) mod exit_membership;
pub(in crate::multitask) mod root_membership;
pub(in crate::multitask) mod terminal;
#[cfg(test)]
mod tests;
