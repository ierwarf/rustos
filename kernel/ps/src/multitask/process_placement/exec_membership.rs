//! Owner: exact process exec membership census, independent of Scheduler enumeration.
//! Boundary: binding/creation/mask exclusion only, not terminal producer or CPU grant.
//! Lifecycle: capture while joins drained -> sealed census -> staged image -> exact finish.
//! Concurrency: Class55 scalar leaf; ProcessObjectPin construction/destruction outside it.
//! Failure: pre-image Drop reopens only own epoch; staged-image abandonment poisons.
//! Forbidden: allocation, callbacks, Identity/Resources/queue leaves or hardware under55.
//! Evidence: native census/nonce/custody witnesses; not full group exit/MM/RSP refinement.
use super::*;
use crate::multitask::current_identity::exec_intent::Intent;
const EXEC: u8 = 4;
pub(in crate::multitask) struct ExecMembership {
    handle: ProcessHandle,
    reservation: process_table::ExecReservation,
    target: Member,
    key: current_identity::exec_intent::Credential,
    nonce: u64,
    members: [Member; MAX],
    len: usize,
    staged: core::cell::Cell<bool>,
    live: bool,
    _process: process_table::pin_owner::ProcessObjectPin,
}
impl ExecMembership {
    pub(in crate::multitask) fn begin(intent: &Intent) -> Option<Self> {
        let reservation = intent.reservation();
        let target = (intent.slot(), intent.task_id());
        if !intent.matches(target.0, reservation) {
            return None;
        }
        let handle = reservation.process_handle();
        let process = process_table::pin_owner::pin(handle)?;
        let (nonce, members, len) = cell(handle)?.with(|state| {
            if state.handle != Some(handle)
                || state.phase != 0
                || state.terminal.is_some()
                || state.exit.is_some()
                || state.joins != 0
                || state.reclaiming.iter().any(Option::is_some)
                || !state
                    .members
                    .iter()
                    .zip(state.stopped)
                    .any(|(&row, stopped)| row == Some(target) && !stopped)
            {
                return None;
            }
            let nonce = state.nonce.checked_add(1)?;
            let mut members = [(0, 0); MAX];
            let mut len = 0;
            for &member in state.members.iter().flatten() {
                members[len] = member;
                len += 1;
            }
            state.nonce = nonce;
            state.phase = EXEC;
            Some((nonce, members, len))
        })?;
        let owner = Self {
            handle,
            reservation,
            target,
            key: intent.credential(),
            nonce,
            members,
            len,
            staged: core::cell::Cell::new(false),
            live: true,
            _process: process,
        };
        if !intent.matches(target.0, reservation) {
            return None;
        }
        Some(owner)
    }
    pub(in crate::multitask) fn members(&self) -> &[Member] {
        &self.members[..self.len]
    }
    pub(in crate::multitask) fn assert_matches(&self, intent: &Intent) {
        assert!(
            self.live
                && self.target == (intent.slot(), intent.task_id())
                && self.reservation == intent.reservation()
                && intent.matches(self.target.0, self.reservation),
            "exec census lacks exact live intent"
        );
        cell(self.handle)
            .expect("exec census lost process ledger")
            .with(|state| {
                assert!(
                    state.handle == Some(self.handle)
                        && state.nonce == self.nonce
                        && state.phase == EXEC,
                    "exec census lost exact epoch"
                );
                assert!(
                    state
                        .members
                        .iter()
                        .flatten()
                        .all(|member| self.members().contains(member)),
                    "exec census admitted unpublished member"
                );
            });
    }
    pub(in crate::multitask) fn assert_image(
        &self,
        key: current_identity::exec_intent::Credential,
    ) {
        assert!(
            self.live && self.key == key && key.admits_image(self.target.0, self.target.1),
            "exec image lacks exact census credential"
        );
        cell(self.handle)
            .expect("exec image lost census ledger")
            .with(|state| {
                assert!(
                    state.handle == Some(self.handle)
                        && state.nonce == self.nonce
                        && state.phase == EXEC,
                    "exec image lost sealed census"
                )
            });
    }
    pub(in crate::multitask) fn mark_staged(&self) {
        assert!(
            self.live && !self.staged.replace(true),
            "exec census staged twice"
        );
    }
    pub(in crate::multitask) fn assert_finalized(&self, finalized: &process_table::ExecFinalize) {
        assert!(
            self.live && self.staged.get() && finalized.intent() == Some(self.key),
            "exec census ACK lacks exact finalized image"
        );
        assert!(
            !crate::multitask::scheduler::stack_resources::image_writing(self.target.0),
            "exec census ACK preceded image writer release"
        );
    }
    pub(in crate::multitask) fn finish(mut self, finalized: &process_table::ExecFinalize) {
        self.assert_finalized(finalized);
        self.release(false);
        self.live = false;
    }
    /// Native census-only projection; not a production image ACK.
    #[cfg(test)]
    pub(in crate::multitask) fn finish_for_test(mut self) {
        self.release(false);
        self.live = false;
    }
    fn release(&self, poison: bool) {
        cell(self.handle)
            .expect("exec census release lost ledger")
            .with(|state| {
                assert!(
                    state.handle == Some(self.handle)
                        && state.nonce == self.nonce
                        && state.phase == EXEC,
                    "exec census release lost exact epoch"
                );
                state.phase = if poison { 3 } else { 0 };
                for index in 0..MAX {
                    if state.exec_detached[index] {
                        if state.exit.is_some() {
                            state.exit_detached[index] = true;
                            state.exec_detached[index] = false;
                        } else if state.terminal.is_some() {
                            state.terminal_detached[index] = true;
                            state.exec_detached[index] = false;
                        } else {
                            state.remove_member(index);
                        }
                    }
                }
                state.release_empty();
            });
    }
}
impl Drop for ExecMembership {
    fn drop(&mut self) {
        if self.live {
            self.release(self.staged.get());
        }
    }
}

#[cfg(test)]
mod tests;
