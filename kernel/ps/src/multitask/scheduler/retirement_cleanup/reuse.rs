//! Owner: exact drained terminal row seal through producer-last slot reset.
//! Boundary: no resource/process/queue work under Cleanup50; not process-exit policy.
//! Lifecycle: checked seal nonce, owned reset, finish; post-effect abandonment quarantines.
//! Concurrency: row sealed before resource closure; row release precedes physical reuse ACK.
//! Failure: pending producer/deferred/external/kernel custody refuses without spending nonce.
//! Forbidden: unowned reset or new terminal producer cannot consume a sealed row.
//! Evidence: actual row tests and independent resource/reuse witnesses; no hardware proof.
use super::*;
pub(in crate::multitask::scheduler) struct ResetSeal {
    slot: usize,
    nonce: u64,
    generation: u64,
    effects: bool,
    reset: bool,
    live: bool,
}
pub(in crate::multitask::scheduler) fn begin(slot: usize, id: Option<u64>) -> Option<ResetSeal> {
    let cell = CELLS.get(slot)?;
    let result = {
        let mut row = cell.state.lock();
        if row.reset_seal.is_some()
            || row.task_id != id
            || row.producer.is_some()
            || row.producing
            || row.deferred.is_some()
            || !matches!(row.cleanup, CleanupState::Vacant | CleanupState::Complete)
            || !matches!(row.kernel, KernelState::Vacant | KernelState::Complete)
        {
            return None;
        }
        let nonce = row.reset_nonce.checked_add(1)?;
        row.reset_nonce = nonce;
        row.reset_seal = Some(nonce);
        (nonce, row.generation)
    };
    Some(ResetSeal {
        slot,
        nonce: result.0,
        generation: result.1,
        effects: false,
        reset: false,
        live: true,
    })
}

/// Native fixture recovery only: no external-effect ACK or production reopen.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn recover_detached_fixture(slot: usize) {
    let mut row = CELLS[slot].state.lock();
    assert!(row.reset_seal.is_some() && row.task_id.is_none());
    assert!(row.producer.is_none() && !row.producing && row.deferred.is_none());
    assert!(matches!(row.cleanup, CleanupState::Vacant));
    assert!(matches!(row.kernel, KernelState::Vacant));
    row.reset_seal = None;
}
impl ResetSeal {
    pub(in crate::multitask::scheduler) fn mark_effects(&mut self) {
        self.effects = true;
    }
    pub(in crate::multitask::scheduler) fn reset(&mut self) {
        assert!(
            self.live && self.effects && !self.reset,
            "terminal reset lacks sole producer"
        );
        let mut row = CELLS[self.slot].state.lock();
        assert_eq!(
            row.reset_seal,
            Some(self.nonce),
            "foreign terminal reset seal"
        );
        assert_eq!(
            row.generation, self.generation,
            "terminal reset lost exact generation"
        );
        assert!(
            row.producer.is_none()
                && !row.producing
                && row.deferred.is_none()
                && matches!(row.cleanup, CleanupState::Vacant | CleanupState::Complete)
                && matches!(row.kernel, KernelState::Vacant | KernelState::Complete),
            "terminal reset acquired new custody"
        );
        row.cleanup = CleanupState::Vacant;
        row.kernel = KernelState::Vacant;
        row.task_id = None;
        row.reason = None;
        self.reset = true;
    }
    pub(in crate::multitask::scheduler) fn finish(mut self) {
        assert!(
            self.live && self.reset,
            "terminal seal released before owned reset"
        );
        let mut row = CELLS[self.slot].state.lock();
        assert_eq!(
            row.reset_seal,
            Some(self.nonce),
            "foreign terminal reset ACK"
        );
        assert_eq!(
            row.generation, self.generation,
            "terminal reset ACK lost generation"
        );
        row.reset_seal = None;
        self.live = false;
    }
}
impl Drop for ResetSeal {
    fn drop(&mut self) {
        if self.live && !self.effects {
            let mut row = CELLS[self.slot].state.lock();
            if row.reset_seal == Some(self.nonce) && row.generation == self.generation {
                row.reset_seal = None;
            }
        }
    }
}
#[cfg(test)]
mod tests;
