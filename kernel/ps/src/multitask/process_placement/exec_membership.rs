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
    members: Vec<Member>,
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
        let cell = cell(handle)?;
        let mut cap = 8usize;
        let (nonce, members) = loop {
            let mut members_buf = Vec::with_capacity(cap);
            let attempt = cell.with(|state| {
                if state.handle != Some(handle)
                    || state.phase != 0
                    || state.terminal.is_some()
                    || state.exit.is_some()
                    || state.joins != 0
                    || state.rows.iter().any(|row| row.reclaiming.is_some())
                    || !state
                        .rows
                        .iter()
                        .any(|row| row.member == target && !row.stopped)
                {
                    return Ok(None);
                }
                if state.rows.len() > members_buf.capacity() {
                    return Err((state.rows.len(), members_buf));
                }
                let Some(nonce) = state.nonce.checked_add(1) else {
                    return Ok(None);
                };
                for row in &state.rows {
                    members_buf.push(row.member);
                }
                state.nonce = nonce;
                state.phase = EXEC;
                Ok(Some((nonce, members_buf)))
            });
            match attempt {
                Ok(res) => break res?,
                Err((need, buf)) => {
                    drop(buf);
                    cap = need.next_power_of_two();
                }
            }
        };
        let owner = Self {
            handle,
            reservation,
            target,
            key: intent.credential(),
            nonce,
            members,
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
        &self.members
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
                        .rows
                        .iter()
                        .all(|row| self.members().contains(&row.member)),
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
                let mut index = 0;
                while index < state.rows.len() {
                    if state.rows[index].exec_detached {
                        if state.exit.is_some() {
                            state.rows[index].exit_detached = true;
                            state.rows[index].exec_detached = false;
                            index += 1;
                        } else if state.terminal.is_some() {
                            state.rows[index].terminal_detached = true;
                            state.rows[index].exec_detached = false;
                            index += 1;
                        } else {
                            state.remove_member(index);
                        }
                    } else {
                        index += 1;
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
