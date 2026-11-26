//! Exact process membership and committed placement across exec ABI changes. Leaves never span
//! frame work, callbacks, queues, process-table acquisition or destruction.
use super::{current_identity, process_table};
use crate::user::abi::UserAbi;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use process_table::ProcessHandle;
use crate::multitask::slot_dir::AtomicDir;
use alloc::vec::Vec;
type Member = (usize, u64);

#[derive(Clone, Copy)]
struct MemberRow {
    member: Member,
    stopped: bool,
    exit_detached: bool,
    exec_detached: bool,
    terminal_detached: bool,
    reclaiming: Option<u64>,
}

impl MemberRow {
    const fn new(member: Member) -> Self {
        Self {
            member,
            stopped: false,
            exit_detached: false,
            exec_detached: false,
            terminal_detached: false,
            reclaiming: None,
        }
    }
}

struct State {
    handle: Option<ProcessHandle>,
    mask: u64,
    nonce: u64,
    phase: u8, // 0 open, 1 metadata sealed, 2 published, 3 poisoned, 4 exec census
    joins: u32,
    rows: Vec<MemberRow>,
    spare_terminal: Option<(Vec<Member>, Vec<bool>)>,
    spare_exit: Option<Vec<Member>>,
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
            rows: Vec::new(),
            spare_terminal: None,
            spare_exit: None,
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
        if self
            .rows
            .iter()
            .any(|row| row.member.0 == member.0 || row.member.1 == member.1)
        {
            return None;
        }
        if self.rows.len() == self.rows.capacity() {
            return None;
        }
        self.rows.push(MemberRow::new(member));
        Some(())
    }
    fn prune_exit_detached(&mut self) {
        assert!(
            self.exit.is_none(),
            "detached exit rows pruned before exact release"
        );
        let mut index = 0;
        while index < self.rows.len() {
            if self.rows[index].exit_detached {
                assert!(
                    self.rows[index].stopped,
                    "exit tombstone lacks committed stop"
                );
                if self.phase == 4 {
                    self.rows[index].exec_detached = true;
                    self.rows[index].exit_detached = false;
                    index += 1;
                } else if self.terminal.is_some() {
                    self.rows[index].terminal_detached = true;
                    self.rows[index].exit_detached = false;
                    index += 1;
                } else {
                    self.remove_member(index);
                }
            } else {
                index += 1;
            }
        }
    }
    fn remove_member(&mut self, index: usize) {
        self.rows.remove(index);
    }
    fn unregister_member(&mut self, index: usize) {
        assert!(
            self.root.is_none()
                && ((self.terminal.is_none() && self.exit.is_none()) || self.rows[index].stopped),
            "live process member detached across terminal turn"
        );
        if self.exit.is_some() {
            self.rows[index].exit_detached = true;
        } else if self.phase == 4 {
            // Keep the exact frozen UID as a reclaim fence even after logical detach.
            self.rows[index].exec_detached = true;
        } else if self.terminal.is_some() {
            self.rows[index].terminal_detached = true;
        } else {
            self.remove_member(index);
        }
    }
    fn prune_terminal_detached(&mut self) {
        assert!(self.terminal.is_none(), "terminal census pruned before original release");
        let mut index = 0;
        while index < self.rows.len() {
            if self.rows[index].terminal_detached {
                if self.exit.is_some() {
                    self.rows[index].exit_detached = true;
                    self.rows[index].terminal_detached = false;
                    index += 1;
                } else if self.phase == 4 {
                    self.rows[index].exec_detached = true;
                    self.rows[index].terminal_detached = false;
                    index += 1;
                } else {
                    self.remove_member(index);
                }
            } else {
                index += 1;
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
            && self.rows.is_empty()
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
    /// Pre-reserves member rows and IRQ-masked scratch capacity outside Class55
    /// so `Turn` / `ExitMembership` never allocate inside Class55 or masked IRQs.
    fn ensure_spare(&self, extra_rows: usize) {
        loop {
            let need = self.with(|s| {
                let target = s.rows.len().saturating_add(extra_rows);
                let term_ok = s.spare_terminal.as_ref().is_some_and(|(m, st)| {
                    m.capacity() >= target && st.capacity() >= target
                });
                let exit_ok = s.spare_exit.as_ref().is_some_and(|m| m.capacity() >= target);
                if s.rows.capacity() >= target && term_ok && exit_ok {
                    None
                } else {
                    Some(target.next_power_of_two().max(8))
                }
            });
            let Some(cap) = need else {
                break;
            };
            let mut new_rows = Vec::with_capacity(cap);
            let new_t_members = Vec::with_capacity(cap);
            let new_t_stopped = Vec::with_capacity(cap);
            let new_exit = Vec::with_capacity(cap);
            let (old_rows, old_term, old_exit) = self.with(|s| {
                let old_rows = if s.rows.capacity() < new_rows.capacity() {
                    new_rows.extend_from_slice(&s.rows);
                    core::mem::replace(&mut s.rows, new_rows)
                } else {
                    new_rows
                };
                let old_term = if s
                    .spare_terminal
                    .as_ref()
                    .is_none_or(|(m, st)| m.capacity() < cap || st.capacity() < cap)
                {
                    s.spare_terminal.replace((new_t_members, new_t_stopped))
                } else {
                    Some((new_t_members, new_t_stopped))
                };
                let old_exit = if s.spare_exit.as_ref().is_none_or(|m| m.capacity() < cap) {
                    s.spare_exit.replace(new_exit)
                } else {
                    Some(new_exit)
                };
                (old_rows, old_term, old_exit)
            });
            drop((old_rows, old_term, old_exit));
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
    let cell = cell(handle)?;
    cell.ensure_spare(1);
    cell.with(|state| {
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
        if !state.rows.is_empty() {
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
        if let Some(index) = state.rows.iter().position(|row| row.member == (slot, id)) {
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
    members: Vec<Member>,
    stopped: Vec<bool>,
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
        let cell = cell(handle)?;
        let mut cap = 8usize;
        let (nonce, members, stopped, previous) = loop {
            let mut members_buf = Vec::with_capacity(cap);
            let mut stopped_buf = Vec::with_capacity(cap);
            let attempt = cell.with(|state| {
                if state.handle != Some(handle)
                    || state.phase != 0
                    || state.terminal.is_some()
                    || state.exit.is_some()
                    || state.joins != 0
                    || !state
                        .rows
                        .iter()
                        .any(|row| row.member == caller && !row.stopped)
                {
                    return Ok(None);
                }
                if state.rows.len() > members_buf.capacity() {
                    return Err((state.rows.len(), members_buf, stopped_buf));
                }
                let Some(nonce) = state.nonce.checked_add(1) else {
                    return Ok(None);
                };
                for row in &state.rows {
                    members_buf.push(row.member);
                    stopped_buf.push(row.stopped);
                }
                state.nonce = nonce;
                state.phase = 1;
                Ok(Some((nonce, members_buf, stopped_buf, state.mask)))
            });
            match attempt {
                Ok(result) => break result?,
                Err((need, m, s)) => {
                    drop((m, s));
                    cap = need.next_power_of_two();
                }
            }
        };
        Some(Self {
            handle,
            caller,
            nonce,
            members,
            stopped,
            previous,
            effects: false,
            live: true,
            _process: process,
        })
    }
    #[cfg(test)]
    pub(super) fn members(&self) -> &[Member] {
        &self.members
    }
    /// Exact terminal ACK captured in the same Class55 turn as membership.
    /// A missing descriptor or a replacement UID is never negative evidence.
    pub(super) fn live_members(&self) -> impl Iterator<Item = Member> + '_ {
        self.members
            .iter()
            .copied()
            .zip(self.stopped.iter().copied())
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
        let cell = cell(handle)?;
        cell.ensure_spare(1);
        let mask = cell.with(|state| {
            if state.handle != Some(handle)
                || state.phase != 0
                || state.terminal.is_some()
                || state.exit.is_some()
                || state.root.is_some()
                || !state
                    .rows
                    .iter()
                    .any(|row| row.member == parent && !row.stopped)
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
        let cell = cell(self.handle)?;
        cell.ensure_spare(1);
        cell.with(|state| {
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
        let retired = cell.with(|state| {
            assert_eq!(state.phase, 0);
            assert_eq!(state.joins, 0);
            assert!(state.terminal.is_none() && state.exit.is_none() && state.exit_work.is_none());
            assert!(state.root.is_none() && state.root_process.is_none());
            let root_nonce = state.root_nonce;
            let exit_nonce = state.exit_nonce;
            let exit_done_nonce = state.exit_done_nonce;
            let nonce = state.nonce;
            let terminal_nonce = state.terminal_nonce;
            let old = core::mem::replace(state, State::new());
            state.nonce = nonce;
            state.root_nonce = root_nonce;
            state.terminal_nonce = terminal_nonce;
            state.exit_nonce = exit_nonce;
            state.exit_done_nonce = exit_done_nonce;
            old
        });
        drop(retired);
    }
}
pub(in crate::multitask) mod reclaim;
pub(in crate::multitask) mod exec_membership;
pub(in crate::multitask) mod exit_membership;
pub(in crate::multitask) mod root_membership;
pub(in crate::multitask) mod terminal;
#[cfg(test)]
mod tests;
