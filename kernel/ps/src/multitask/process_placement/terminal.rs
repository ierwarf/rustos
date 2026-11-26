//! Owner: Class55 exact process membership/last-live terminal turn.
//! Boundary: membership/creation exclusion and stop election, not CPU/MM/image or process policy.
//! Lifecycle: Reserved -> Stopped -> Published; row removal retains turn through producer ACK.
//! Concurrency: ProcessObjectPin first; scalar leaves never span Identity51 or callbacks/destruction.
//! Failure: untouched rollback exact; effect abandonment retains poisoned occupancy; epochs checked.
//! Forbidden: unknown bindings, desired stops, empty descriptors or copied booleans are not final election.
//! Evidence: real membership/join/pin/terminal-publication witnesses; no full lifecycle/hardware refinement.
use super::*;
use crate::multitask::scheduler::retirement_cleanup::producer::Publication;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Reserved,
    Stopped,
    Poisoned,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Record {
    member: Member,
    nonce: u64,
    last: bool,
    phase: Phase,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::multitask) struct Credential {
    handle: ProcessHandle,
    member: Member,
    nonce: u64,
}
impl Credential {
    pub(in crate::multitask) fn matches_member(&self, slot: usize, id: Option<u64>) -> bool {
        self.member.0 == slot && Some(self.member.1) == id
    }

    pub(in crate::multitask) fn admits_publication(&self) -> bool {
        cell(self.handle).is_some_and(|cell| {
            cell.with(|state| {
                state.handle == Some(self.handle)
                    && state.terminal_nonce == self.nonce
                    && state.terminal.is_some_and(|record| {
                        record.member == self.member
                            && record.nonce == self.nonce
                            && record.phase == Phase::Stopped
                    })
            })
        })
    }
}
pub(in crate::multitask) struct Turn {
    handle: ProcessHandle,
    member: Member,
    nonce: u64,
    last: bool,
    members: Vec<Member>,
    stopped: Vec<bool>,
    effects: bool,
    live: bool,
    _process: process_table::pin_owner::ProcessObjectPin,
}
impl Turn {
    pub(in crate::multitask) fn begin(handle: ProcessHandle, member: Member) -> Option<Self> {
        let process = process_table::pin_owner::pin(handle)?;
        let (nonce, last, members, stopped) = cell(handle)?.with(|state| {
            if state.handle != Some(handle) || state.terminal.is_some() || state.joins != 0
                || state.rows.iter().any(|row| row.reclaiming.is_some())
            {
                return None;
            }
            let index = state.rows.iter().position(|row| row.member == member)?;
            if state.rows[index].stopped {
                return None;
            }
            let nonce = state.terminal_nonce.checked_add(1)?;
            let last = state
                .rows
                .iter()
                .all(|row| row.member == member || row.stopped);
            let (mut members, mut stopped) = state
                .spare_terminal
                .take()
                .expect("terminal snapshot buffer must be reserved at join");
            members.clear();
            stopped.clear();
            assert!(
                members.capacity() >= state.rows.len() && stopped.capacity() >= state.rows.len(),
                "terminal snapshot buffer smaller than live rows"
            );
            for row in &state.rows {
                members.push(row.member);
                stopped.push(row.stopped);
            }
            state.terminal_nonce = nonce;
            state.terminal = Some(Record {
                member,
                nonce,
                last,
                phase: Phase::Reserved,
            });
            Some((nonce, last, members, stopped))
        })?;
        Some(Self {
            handle,
            member,
            nonce,
            last,
            members,
            stopped,
            effects: false,
            live: true,
            _process: process,
        })
    }
    fn exact(&self, state: &State) -> bool {
        self.live
            && state.handle == Some(self.handle)
            && state.terminal_nonce == self.nonce
            && state.terminal.is_some_and(|r| {
                r.member == self.member && r.nonce == self.nonce && r.last == self.last
            })
    }
    pub(in crate::multitask) fn credential(&self) -> Credential {
        assert!(
            cell(self.handle).unwrap().with(|s| self.exact(s)),
            "terminal credential lost live turn"
        );
        Credential {
            handle: self.handle,
            member: self.member,
            nonce: self.nonce,
        }
    }
    pub(in crate::multitask) fn last(&self) -> bool {
        self.last
    }
    /// Pre-withdraw exit authorization only: a complete desired-stop census
    /// can seal process authority, but never replaces committed final election.
    pub(in crate::multitask) fn last_desired(&self) -> bool {
        assert!(
            cell(self.handle).unwrap().with(|s| self.exact(s)),
            "terminal desired census lost membership turn"
        );
        self.members
            .iter()
            .zip(self.stopped.iter().copied())
            .all(|(&m, stopped)| {
                m == self.member || stopped || current_identity::terminal_retired_matches(m.0, m.1)
            })
    }
    pub(in crate::multitask) fn mark_stopped(&mut self) {
        assert!(
            current_identity::terminal_retired_matches(self.member.0, self.member.1),
            "terminal election preceded exact lifecycle stop"
        );
        cell(self.handle).unwrap().with(|state| {
            assert!(
                self.exact(state)
                    && !self.effects
                    && state.terminal.unwrap().phase == Phase::Reserved,
                "terminal election lost exact turn"
            );
            let index = state
                .rows
                .iter()
                .position(|row| row.member == self.member)
                .expect("terminal election lost member");
            assert!(!state.rows[index].stopped, "terminal member stopped twice");
            state.rows[index].stopped = true;
            state.terminal.as_mut().unwrap().phase = Phase::Stopped;
        });
        self.effects = true;
    }
    /// Original stopped final-member census retains every captured UID against
    /// physical reclaim. The caller withdraws donations OUTSIDE Class55 while
    /// this borrow prevents consuming the original terminal turn.
    pub(in crate::multitask) fn final_donation_members(&self) -> &[Member] {
        assert!(self.live && self.effects && self.last, "process donation withdrawal lacks stopped final census");
        cell(self.handle).unwrap().with(|state| {
            assert!(self.exact(state) && state.terminal.unwrap().phase == Phase::Stopped,
                "process donation withdrawal lost original terminal epoch");
        });
        &self.members
    }
    pub(in crate::multitask) fn finish(mut self, publication: Publication) {
        assert!(
            self.effects && publication.matches_turn(self.credential()),
            "terminal membership ACK lacks actual producer publication"
        );
        let mut members = core::mem::take(&mut self.members);
        let mut stopped = core::mem::take(&mut self.stopped);
        cell(self.handle).unwrap().with(|state| {
            assert!(
                self.exact(state) && state.terminal.unwrap().phase == Phase::Stopped,
                "terminal membership ACK lost exact turn"
            );
            state.terminal = None;
            members.clear();
            stopped.clear();
            state.spare_terminal = Some((members, stopped));
            state.prune_terminal_detached();
            state.release_empty();
        });
        self.live = false;
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let mut members = core::mem::take(&mut self.members);
        let mut stopped = core::mem::take(&mut self.stopped);
        cell(self.handle).unwrap().with(|state| {
            assert!(
                self.exact(state),
                "terminal membership rollback lost exact epoch"
            );
            if self.effects {
                state.terminal.as_mut().unwrap().phase = Phase::Poisoned;
                // Wait: poisoned state never permits another Turn, so return spare anyway
                // rather than dropping/deallocating under Class55 or masked IRQs.
                members.clear();
                stopped.clear();
                state.spare_terminal = Some((members, stopped));
            } else {
                state.terminal = None;
                members.clear();
                stopped.clear();
                state.spare_terminal = Some((members, stopped));
                state.prune_terminal_detached();
                state.release_empty();
            }
        }); // process pin destroyed after the leaf
    }
}
#[cfg(test)]
mod tests;
