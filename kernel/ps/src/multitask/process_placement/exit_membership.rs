//! Owner: Class55 exact group-exit membership census, separate from exec's epoch.
//! Boundary: freezes publication/collection; not image, CPU, MM or terminal effect permission.
//! Lifecycle: Reserved -> Effects -> Finished; explicit suspension retains original census/pin.
//! Abandonment poisons; suspension is not rollback/recollection or creation reopen.
//! Concurrency: joins must drain before capture; no leaf spans identity/table callbacks or pin Drop.
//! Failure: duplicate/foreign/exhausted admissions refuse without effects; stopped binding ACK is exact.
//! Forbidden: copied slots, metadata absence or process PID alone never grant group withdrawal.
//! Evidence: native real ledger/identity/pin witnesses; bounded lifecycle models remain projections.
use super::*;
use process_table::pin_owner::ProcessObjectPin;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Reserved,
    Effects,
    Suspended,
    Poisoned,
}
#[derive(Clone, Copy)]
pub(super) struct Record {
    nonce: u64,
    phase: Phase,
    queued: bool,
}

impl Record {
    pub(super) fn permits_captured_join(&self) -> bool {
        self.phase == Phase::Waiting
    }

    fn claim_ready(&self, s: &State) -> bool {
        self.queued
            && matches!(self.phase, Phase::Waiting | Phase::Suspended)
            && s.joins == 0
            && s.terminal.is_none()
            && s.phase == 0
    }
}

/// Short, non-yielding admission loan. Publication transfers its physical
/// process pin to the row; a caller may yield only with the Copy Receipt.
pub(in crate::multitask) struct PendingExit {
    handle: ProcessHandle,
    nonce: u64,
    live: bool,
    process: Option<ProcessObjectPin>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::multitask) enum Reason {
    Exited {
        slot: usize,
        task_id: u64,
        process_id: u64,
        logical_cpu: usize,
    },
    Terminated {
        requested_by_pid: Option<u64>,
    },
}

/// Durable row custody, never a caller-owned stack or a replacement global lock.
pub(super) struct Work {
    nonce: u64,
    process_id: u64,
    process: ProcessObjectPin,
    reason: Reason,
    captured: Option<Captured>,
}

/// The original census payload travels with its original process pin. Never
/// recollect live members after a terminal competitor forced a suspension.
struct Captured {
    members: [Member; MAX],
    len: usize,
    effects: bool,
}

/// Read-only completion credential. It owns no references across rescheduling.
/// Row nonces and their completed watermark persist across process generations;
/// only an exact queued census ACK advances the watermark, never absence/reuse.
#[derive(Clone, Copy)]
pub(in crate::multitask) struct Receipt {
    handle: ProcessHandle,
    nonce: u64,
}
impl Receipt {
    pub(in crate::multitask) fn complete(self) -> bool {
        cell(self.handle).is_some_and(|cell| cell.with(|s| s.exit_done_nonce >= self.nonce))
    }
}

pub(in crate::multitask) fn queued_for(handle: ProcessHandle) -> Option<Receipt> {
    cell(handle)?.with(|s| {
        let r = s.exit?;
        (s.handle == Some(handle)
            && s.exit_nonce == r.nonce
            && r.queued
            && r.phase != Phase::Poisoned)
            .then_some(Receipt {
                handle,
                nonce: r.nonce,
            })
    })
}

pub(in crate::multitask) fn target_by_pid(pid: u64) -> Option<(ProcessHandle, Member)> {
    for cell in &CELLS {
        if let Some(target) = cell.with(|s| {
            let h = s.handle?;
            let (&m, _) = s
                .members
                .iter()
                .zip(s.stopped)
                .find(|(m, stopped)| m.is_some_and(|(_, id)| id == pid) && !stopped)?;
            Some((h, m.unwrap()))
        }) {
            return Some(target);
        }
    }
    None
}

pub(in crate::multitask) fn claim_next() -> Option<(ExitMembership, Reason)> {
    for cell in &CELLS {
        let candidate = cell.with(|s| {
            let r = s.exit?;
            if !r.claim_ready(s) {
                return None;
            }
            let handle = s.handle.expect("queued exit lost process generation");
            let work = s
                .exit_work
                .as_ref()
                .expect("queued exit lost durable process custody");
            assert!(
                s.exit_nonce == r.nonce && work.nonce == r.nonce && work.process.handle() == handle,
                "queued exit lost exact row owner"
            );
            Some((handle, work.process_id, r.nonce))
        });
        let Some((handle, process_id, nonce)) = candidate else {
            continue;
        };
        // No Class55 leaf spans the process-table command or its diagnostic.
        // The physical exact-generation pin excludes row/PID reuse during it.
        let Some(_process) = process_table::pin_owner::pin(handle) else {
            continue;
        };
        if process_table::mark_process_exiting_for_handle(handle, process_id).is_none() {
            continue;
        }
        if let Some(claim) = cell.with(|s| {
            let r = s.exit?;
            if s.handle != Some(handle)
                || s.exit_nonce != nonce
                || r.nonce != nonce
                || !r.claim_ready(s)
            {
                return None;
            }
            let Work {
                process,
                process_id,
                reason,
                captured,
                ..
            } = s.exit_work.take().unwrap();
            let Captured { members, len, effects } = captured.unwrap_or_else(|| {
                let mut members = [(0, 0); MAX];
                let mut len = 0;
                for &m in s.members.iter().flatten() {
                    members[len] = m;
                    len += 1;
                }
                assert!(len != 0, "queued exit lost complete stopped membership");
                Captured { members, len, effects: false }
            });
            s.exit.as_mut().unwrap().phase = if effects { Phase::Effects } else { Phase::Reserved };
            Some((
                ExitMembership {
                    handle,
                    nonce,
                    members,
                    len,
                    effects,
                    live: true,
                    _process: Some(process),
                    queued_reason: Some((process_id, reason)),
                },
                reason,
            ))
        }) {
            return Some(claim);
        }
    }
    None
}

impl PendingExit {
    pub(in crate::multitask) fn begin(handle: ProcessHandle, target: Member) -> Option<Self> {
        let process = process_table::pin_owner::pin(handle)?;
        let nonce = cell(handle)?.with(|s| {
            if s.handle != Some(handle) || s.exit.is_some() || !s.members.contains(&Some(target)) {
                return None;
            }
            let nonce = s.exit_nonce.checked_add(1)?;
            s.exit_nonce = nonce;
            s.exit = Some(Record {
                nonce,
                phase: Phase::Waiting,
                queued: false,
            });
            Some(nonce)
        })?;
        Some(Self {
            handle,
            nonce,
            live: true,
            process: Some(process),
        })
    }
    fn exact(&self, s: &State) -> bool {
        self.live
            && s.handle == Some(self.handle)
            && s.exit_nonce == self.nonce
            && s.exit
                .is_some_and(|r| !r.queued && r.nonce == self.nonce && r.phase == Phase::Waiting)
    }

    pub(in crate::multitask) fn publish(mut self, process_id: u64, reason: Reason) -> Receipt {
        #[cfg(not(test))]
        assert!(
            nucleus_core::util::lockdep::preemption_disabled(),
            "exit admission may not yield before durable publication"
        );
        let process = self
            .process
            .take()
            .expect("exit publication lost process pin");
        let receipt = Receipt {
            handle: self.handle,
            nonce: self.nonce,
        };
        cell(self.handle).unwrap().with(|s| {
            assert!(
                self.exact(s) && s.exit_work.is_none(),
                "exit publication lost exact waiting seal"
            );
            s.exit_work = Some(Work {
                nonce: self.nonce,
                process_id,
                process,
                reason,
                captured: None,
            });
            s.exit.as_mut().unwrap().queued = true;
        });
        self.live = false;
        receipt
    }
    #[cfg(test)]
    pub(in crate::multitask) fn try_capture(mut self) -> Result<ExitMembership, Self> {
        let captured = cell(self.handle).unwrap().with(|s| {
            assert!(self.exact(s), "pending exit lost exact creation seal");
            if s.joins != 0 || s.terminal.is_some() {
                return None;
            }
            let mut members = [(0, 0); MAX];
            let mut len = 0;
            for &m in s.members.iter().flatten() {
                members[len] = m;
                len += 1;
            }
            s.exit.as_mut().unwrap().phase = Phase::Reserved;
            Some((members, len))
        });
        let Some((members, len)) = captured else {
            return Err(self);
        };
        let process = self.process.take().expect("pending exit lost process pin");
        self.live = false;
        Ok(ExitMembership {
            handle: self.handle,
            nonce: self.nonce,
            members,
            len,
            effects: false,
            live: true,
            _process: Some(process),
            queued_reason: None,
        })
    }
}
impl Drop for PendingExit {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        cell(self.handle).unwrap().with(|s| {
            assert!(self.exact(s), "pending exit rollback lost exact seal");
            s.exit = None;
            s.prune_exit_detached();
            s.release_empty();
        });
    }
}
pub(in crate::multitask) struct ExitMembership {
    handle: ProcessHandle,
    nonce: u64,
    members: [Member; MAX],
    len: usize,
    effects: bool,
    live: bool,
    _process: Option<ProcessObjectPin>,
    queued_reason: Option<(u64, Reason)>,
}
impl ExitMembership {
    pub(in crate::multitask) fn begin(handle: ProcessHandle, target: Member) -> Option<Self> {
        let process = process_table::pin_owner::pin(handle)?;
        let (nonce, members, len) = cell(handle)?.with(|s| {
            if s.handle != Some(handle)
                || s.exit.is_some()
                || s.terminal.is_some()
                || s.joins != 0
                || !s.members.contains(&Some(target))
            {
                return None;
            }
            let nonce = s.exit_nonce.checked_add(1)?;
            let mut members = [(0, 0); MAX];
            let mut len = 0;
            for &m in s.members.iter().flatten() {
                members[len] = m;
                len += 1;
            }
            s.exit_nonce = nonce;
            s.exit = Some(Record {
                nonce,
                phase: Phase::Reserved,
                queued: false,
            });
            Some((nonce, members, len))
        })?;
        Some(Self {
            handle,
            nonce,
            members,
            len,
            effects: false,
            live: true,
            _process: Some(process),
            queued_reason: None,
        })
    }
    fn exact(&self, s: &State) -> bool {
        self.live
            && s.handle == Some(self.handle)
            && s.exit_nonce == self.nonce
            && s.exit
                .is_some_and(|r| r.nonce == self.nonce && r.queued == self.queued_reason.is_some())
    }
    pub(in crate::multitask) fn members(&self) -> &[Member] {
        &self.members[..self.len]
    }

    pub(in crate::multitask) fn handle(&self) -> ProcessHandle {
        self.handle
    }

    pub(in crate::multitask) fn member_stopped(&self, member: Member) -> bool {
        cell(self.handle).unwrap().with(|s| {
            assert!(
                self.exact(s),
                "group exit stopped observation lost exact census"
            );
            s.members
                .iter()
                .zip(s.stopped)
                .any(|(&m, stopped)| m == Some(member) && stopped)
        })
    }
    pub(in crate::multitask) fn effects_started(&self) -> bool {
        self.effects
    }

    /// Explicit non-yielding handback, not Drop recovery. The row retains the
    /// SAME pin, nonce, captured UIDs and effect phase until the next worker.
    pub(in crate::multitask) fn suspend(mut self) {
        let (process_id, reason) = self.queued_reason.expect("unqueued exit cannot suspend");
        let work = Work {
            nonce: self.nonce,
            process_id,
            process: self._process.take().expect("exit suspension lost original process pin"),
            reason,
            captured: Some(Captured {
                members: self.members,
                len: self.len,
                effects: self.effects,
            }),
        };
        cell(self.handle).unwrap().with(|s| {
            assert!(self.exact(s) && s.exit_work.is_none(), "exit suspension lost original census");
            assert!(matches!(s.exit.unwrap().phase, Phase::Reserved | Phase::Effects));
            assert!(s.exit_work.replace(work).is_none(), "exit handback duplicated original pin");
            s.exit.as_mut().unwrap().phase = Phase::Suspended;
        });
        self.live = false;
    }

    pub(in crate::multitask) fn begin_effects(&mut self) {
        cell(self.handle).unwrap().with(|s| {
            assert!(
                self.exact(s) && !self.effects && s.exit.unwrap().phase == Phase::Reserved,
                "group exit lost exact effect admission"
            );
            s.exit.as_mut().unwrap().phase = Phase::Effects;
        });
        self.effects = true;
    }
    pub(in crate::multitask) fn finish(mut self) {
        assert!(self.effects, "group exit ACK lacks effects");
        assert!(
            self.members()
                .iter()
                .all(|&(slot, id)| self.member_stopped((slot, id))
                    || current_identity::terminal_retired_matches(slot, id)),
            "group exit ACK lacks exact desired terminal bindings"
        );
        cell(self.handle).unwrap().with(|s| {
            assert!(
                self.exact(s) && s.exit.unwrap().phase == Phase::Effects,
                "group exit ACK lost exact census"
            );
            if s.exit.unwrap().queued {
                assert!(s.exit_work.is_none(), "exit ACK still has unclaimed work");
                s.exit_done_nonce = self.nonce;
            }
            s.exit = None;
            s.prune_exit_detached();
            s.release_empty();
        });
        self.live = false;
    }
}
impl Drop for ExitMembership {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let retained = self.queued_reason.map(|(process_id, reason)| Work {
            nonce: self.nonce,
            process_id,
            process: self
                ._process
                .take()
                .expect("queued exit abandonment lost process pin"),
            reason,
            captured: Some(Captured {
                members: self.members,
                len: self.len,
                effects: self.effects,
            }),
        });
        cell(self.handle).unwrap().with(|s| {
            assert!(self.exact(s), "group exit rollback lost exact census");
            if self.effects || retained.is_some() {
                s.exit.as_mut().unwrap().phase = Phase::Poisoned;
                if let Some(work) = retained {
                    assert!(
                        s.exit_work.is_none(),
                        "abandoned exit duplicated row custody"
                    );
                    s.exit_work = Some(work);
                }
            } else {
                s.exit = None;
                s.prune_exit_detached();
                s.release_empty();
            }
        });
    }
}
#[cfg(test)]
mod tests;
