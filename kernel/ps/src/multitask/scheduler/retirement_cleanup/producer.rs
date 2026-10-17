//! Owner: exact terminal producer loan, before withdrawal through final consumer publication.
//! Boundary: Class50 metadata custody, not process-last-live, exec, CPU/MM or IRQ grant.
//! Lifecycle: Reserved -> Effects -> Deferred -> Claimed -> Effects -> Published.
//! Concurrency: scalar leaves only; no leaf spans scheduler/process/queue callbacks or Drop.
//! Failure: untouched rollback exact; effect abandonment quarantines; all epochs checked.
//! Forbidden: copied reasons/UIDs, descriptor absence or empty deferred queue are not producer ACKs.
//! Evidence: native real row/consumer/reset witnesses, not physical retirement/complete lifecycle proof.
use super::*;
use crate::multitask::process_placement::terminal::Credential;
/// Producer publication receipt; not external cleanup/kernel/hardware completion.
pub(in crate::multitask) struct Publication {
    turn: Option<Credential>,
}
impl Publication {
    #[cfg(test)]
    pub(in crate::multitask) fn fixture() -> Self {
        Self { turn: None }
    }

    pub(in crate::multitask) fn matches_turn(&self, turn: Credential) -> bool {
        self.turn == Some(turn)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Key {
    generation: u64,
    nonce: u64,
    task_id: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Reserved,
    WaitingExecutor,
    Effects,
    Deferred,
    Claimed,
    Poisoned,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Record {
    key: Key,
    phase: Phase,
    reason: TaskRetireReason,
    detach: bool,
    turn: Option<Credential>,
}
pub(in crate::multitask::scheduler) struct Producer {
    slot: usize,
    key: Key,
    reason: TaskRetireReason,
    detach: bool,
    turn: Option<Credential>,
    claimed: bool,
    waited_for_executor: bool,
    effects: bool,
    live: bool,
}
impl Producer {
    pub(in crate::multitask::scheduler) fn begin(
        slot: usize,
        task_id: Option<u64>,
        reason: TaskRetireReason,
        detach: bool,
    ) -> Option<Self> {
        let key = {
            let mut row = CELLS.get(slot)?.state.lock();
            if row.reset_seal.is_some()
                || row.producer.is_some()
                || row.producing
                || row.deferred.is_some()
                || row.reason.is_some()
                || row.cleanup != CleanupState::Vacant
                || row.kernel != KernelState::Vacant
                || (row.task_id.is_some() && row.task_id != task_id)
            {
                return None;
            }
            let generation = row.generation.checked_add(1)?;
            let nonce = row.producer_nonce.checked_add(1)?;
            let key = Key {
                generation,
                nonce,
                task_id,
            };
            row.generation = generation;
            row.producer_nonce = nonce;
            row.task_id = task_id;
            row.producing = true;
            row.producer = Some(Record {
                key,
                phase: Phase::Reserved,
                reason,
                detach,
                turn: None,
            });
            key
        };
        Some(Self {
            slot,
            key,
            reason,
            detach,
            turn: None,
            claimed: false,
            waited_for_executor: false,
            effects: false,
            live: true,
        })
    }
    pub(in crate::multitask::scheduler) fn claim(
        slot: usize,
        task_id: Option<u64>,
    ) -> Option<Self> {
        let record = {
            let mut row = CELLS.get(slot)?.state.lock();
            let record = row.producer?;
            if row.reset_seal.is_some()
                || !matches!(record.phase, Phase::Deferred | Phase::WaitingExecutor)
                || record.key.task_id != task_id
                || row.task_id != task_id
                || row.generation != record.key.generation
                || row.producer_nonce != record.key.nonce
                || row.deferred != Some(record.reason)
                || row.producing
                || row.cleanup != CleanupState::Vacant
                || row.kernel != KernelState::Vacant
            {
                return None;
            }
            if record.phase == Phase::WaitingExecutor
                && !super::super::stack_resources::terminal_binding_matches(slot, task_id)
            {
                return None;
            }
            let nonce = row.producer_nonce.checked_add(1)?;
            let mut admitted = record;
            admitted.key.nonce = nonce;
            admitted.phase = Phase::Claimed;
            row.producer_nonce = nonce;
            row.producer = Some(admitted);
            row.deferred = None;
            row.producing = true;
            (admitted, record.phase == Phase::WaitingExecutor)
        };
        let (record, waited_for_executor) = record;
        Some(Self {
            slot,
            key: record.key,
            reason: record.reason,
            detach: record.detach,
            turn: record.turn,
            claimed: true,
            waited_for_executor,
            effects: false,
            live: true,
        })
    }
    /// Preserve the first exact stop request without denying this kernel caller's
    /// resume. Only existing executor scopes may extend the loan until it drains.
    pub(in crate::multitask::scheduler) fn wait_for_executor(mut self) {
        let mut row = CELLS[self.slot].state.lock();
        if self.exact(&row)
            && !self.effects
            && !self.claimed
            && row.producer.unwrap().phase == Phase::Reserved
            && crate::multitask::current_identity::execution::kernel_executor::admit_stop_wait(
                self.slot,
                self.key.task_id,
            )
        {
            row.producer.as_mut().unwrap().phase = Phase::WaitingExecutor;
            row.reason = Some(self.reason);
            row.deferred = Some(self.reason);
            row.producing = false;
            self.live = false;
            mark_reap_pending();
        }
        drop(row);
        // Otherwise the untouched producer rolls back; no descriptor inference
        // or user continuation is converted into executor retirement custody.
    }
    pub(in crate::multitask::scheduler) fn waited_for_executor(&self) -> bool {
        self.waited_for_executor
    }
    pub(super) fn waiting_executor(record: Record) -> bool {
        record.phase == Phase::WaitingExecutor
    }

    /// Acknowledgement of the retained ORIGINAL first producer, not admission
    /// or a replacement lifetime. Caller still owns its original Current loan.
    pub(in crate::multitask::scheduler) fn waiting_for_original(slot: usize, id: u64) -> bool {
        let Some(cell) = CELLS.get(slot) else { return false; };
        let row = cell.state.lock();
        let Some(record) = row.producer else { return false; };
        record.phase == Phase::WaitingExecutor
            && record.key.task_id == Some(id) && row.task_id == Some(id)
            && row.generation == record.key.generation
            && row.producer_nonce == record.key.nonce
            && row.deferred == Some(record.reason) && row.reason == Some(record.reason)
            && !row.producing && row.reset_seal.is_none()
            && row.cleanup == CleanupState::Vacant && row.kernel == KernelState::Vacant
    }
    pub(in crate::multitask::scheduler) fn slot(&self) -> usize {
        self.slot
    }
    pub(in crate::multitask::scheduler) fn task_id(&self) -> Option<u64> {
        self.key.task_id
    }
    pub(in crate::multitask::scheduler) fn reason(&self) -> TaskRetireReason {
        self.reason
    }
    pub(in crate::multitask::scheduler) fn detach(&self) -> bool {
        self.detach
    }
    fn exact(&self, row: &SlotState) -> bool {
        self.live
            && row.generation == self.key.generation
            && row.producer_nonce == self.key.nonce
            && row.task_id == self.key.task_id
            && row.producer.is_some_and(|record| {
                record.key == self.key
                    && record.reason == self.reason
                    && record.detach == self.detach
                    && record.turn == self.turn
            })
    }
    pub(in crate::multitask::scheduler) fn bind_process_turn(&mut self, turn: Credential) {
        assert!(
            turn.matches_member(self.slot, self.key.task_id),
            "terminal process turn changed slot UID"
        );
        let mut row = CELLS[self.slot].state.lock();
        assert!(
            self.exact(&row)
                && !self.effects
                && self.turn.is_none()
                && row
                    .producer
                    .is_some_and(|r| matches!(r.phase, Phase::Reserved | Phase::Claimed)),
            "terminal process turn bound after effects"
        );
        row.producer.as_mut().unwrap().turn = Some(turn);
        self.turn = Some(turn);
    }

    pub(in crate::multitask::scheduler) fn begin_effects(&mut self) {
        let mut row = CELLS[self.slot].state.lock();
        assert!(
            self.exact(&row)
                && !self.effects
                && row.producing
                && row
                    .producer
                    .is_some_and(|r| matches!(r.phase, Phase::Reserved | Phase::Claimed)),
            "terminal producer lost effect admission"
        );
        row.producer.as_mut().unwrap().phase = Phase::Effects;
        self.effects = true;
    }
    /// Retain the SAME original loan after complete stop withdrawal, before
    /// borrower drain/process election. No handoff gap, new nonce or consumer
    /// ACK: untouched abandonment restores the original deferred stop.
    pub(in crate::multitask::scheduler) fn retain_stopped(mut self) -> Self {
        {
            let mut row = CELLS[self.slot].state.lock();
            assert!(
                self.exact(&row) && self.effects && self.turn.is_none()
                    && row.producing && row.producer.unwrap().phase == Phase::Effects,
                "terminal stop retention lost original effects owner"
            );
            row.reason = Some(self.reason);
            row.deferred = Some(self.reason);
            row.producer.as_mut().unwrap().phase = Phase::Claimed;
        }
        self.effects = false;
        self.claimed = true;
        self
    }

    pub(in crate::multitask::scheduler) fn defer(mut self) {
        {
            let mut row = CELLS[self.slot].state.lock();
            assert!(
                self.exact(&row) && self.effects && row.producer.unwrap().phase == Phase::Effects,
                "terminal deferral lacks exact effects owner"
            );
            row.producer.as_mut().unwrap().phase = Phase::Deferred;
            row.reason = Some(self.reason);
            row.deferred = Some(self.reason);
            row.producing = false;
        }
        self.live = false;
        mark_reap_pending();
    }
    pub(in crate::multitask::scheduler) fn publish(
        mut self,
        cleanup: Option<RetiredTaskCleanup>,
        effects: RetirementSideEffect,
    ) -> Publication {
        assert!(
            self.turn.is_none_or(|turn| turn.admits_publication()),
            "terminal publication lacks exact stopped process turn"
        );
        {
            let mut row = CELLS[self.slot].state.lock();
            assert!(
                self.exact(&row)
                    && self.effects
                    && row.reset_seal.is_none()
                    && row.producing
                    && row.producer.unwrap().phase == Phase::Effects
                    && row.cleanup == CleanupState::Vacant
                    && row.kernel == KernelState::Vacant,
                "terminal publication lacks exact producer"
            );
            assert!(
                cleanup.is_none_or(|c| Some(c.task_id) == self.key.task_id)
                    && effects.task_id() == self.key.task_id,
                "terminal payload changed producer UID"
            );
            row.reason = Some(self.reason);
            row.deferred = None;
            row.cleanup = cleanup
                .map(CleanupState::Pending)
                .unwrap_or(CleanupState::Complete);
            row.kernel = KernelState::Pending(effects);
            row.producer = None;
            row.producing = false;
        }
        self.live = false;
        mark_reap_pending();
        Publication { turn: self.turn }
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let mut row = CELLS[self.slot].state.lock();
        assert!(self.exact(&row), "terminal rollback lost exact producer");
        if self.effects {
            row.producer.as_mut().unwrap().phase = Phase::Poisoned;
            row.producing = true;
        } else if self.claimed {
            row.producer.as_mut().unwrap().phase = if self.waited_for_executor {
                Phase::WaitingExecutor
            } else {
                Phase::Deferred
            };
            row.deferred = Some(self.reason);
            row.producing = false;
        } else {
            row.producer = None;
            row.producing = false;
            row.task_id = None;
        }
    }
}
/// Actual producer/consumer/reset path for native process-turn witnesses only.
#[cfg(test)]
pub(in crate::multitask) fn publish_process_turn_fixture(
    slot: usize,
    id: u64,
    turn: Credential,
) -> Publication {
    let mut owner = Producer::begin(slot, Some(id), TaskRetireReason::Exited, true).unwrap();
    owner.bind_process_turn(turn);
    owner.begin_effects();
    let publication = owner.publish(None, RetirementSideEffect::new(Some(id), None));
    let (generation, effects) = CELLS[slot].claim_kernel().unwrap();
    CELLS[slot].complete_kernel(generation, effects);
    CELLS[slot].reset();
    publication
}
/// Fixture isolation/poison recovery only, never production reopening.
#[cfg(test)]
pub(in crate::multitask) fn reset_process_turn_fixture(slot: usize) {
    let mut row = CELLS[slot].state.lock();
    let generation = row.generation;
    let nonce = row.producer_nonce;
    *row = SlotState::new();
    row.generation = generation;
    row.producer_nonce = nonce;
}
#[cfg(test)]
mod tests;
