//! Exact cold exec transaction custody, separate from CPU execution references.
//! Writer51 owns commands; the atomic word only authenticates resource access.
use super::*;
use crate::multitask::process_table::ExecReservation;
use core::{cell::Cell, sync::atomic::AtomicU64};
const SEALED: u64 = 1;
const IMAGE: u64 = 2;
const PUBLISHED: u64 = 3;
const POISONED: u64 = 4;
const CANCELLING: u64 = 5;
static WORDS: [AtomicU64; MAX_SCHEDULER_TASKS] = [const { AtomicU64::new(0) }; MAX_SCHEDULER_TASKS];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::multitask) struct Credential {
    slot: usize,
    task_id: u64,
    reservation: ExecReservation,
    generation: u64,
}
#[derive(Clone, Copy)]
pub(super) struct Record {
    key: Credential,
    phase: u64,
    current: bool,
}
pub(super) fn check_unowned_gate(writer: &IdentityWriter, gate: LifecycleGate, value: bool) {
    if matches!(gate, LifecycleGate::ExecQuiesced) && writer.exec_intent.is_some() {
        assert_eq!(
            value, writer.lifecycle.exec_quiesced,
            "unqualified writer changed owned exec gate"
        );
    }
}
impl Credential {
    fn word(self, phase: u64) -> u64 {
        self.generation * 8 + phase
    }
    pub(in crate::multitask) fn admits_image(self, slot: usize, task_id: u64) -> bool {
        self.slot == slot
            && self.task_id == task_id
            && matches!(WORDS[slot].load(Ordering::Acquire), word if word == self.word(SEALED) || word == self.word(IMAGE))
    }
    pub(in crate::multitask) fn published(self, reservation: ExecReservation) -> bool {
        self.reservation == reservation
            && WORDS[self.slot].load(Ordering::Acquire) == self.word(PUBLISHED)
    }
}
#[must_use]
pub(in crate::multitask) struct Intent {
    key: Credential,
    live: Cell<bool>,
}
#[must_use]
pub(in crate::multitask) struct Image<'a> {
    intent: &'a Intent,
    live: bool,
    published: bool,
}
pub(in crate::multitask) fn busy(slot: usize) -> bool {
    WORDS
        .get(slot)
        .is_none_or(|word| word.load(Ordering::Acquire) != 0)
}
fn exact(writer: &IdentityWriter, key: Credential) -> bool {
    writer.exec_intent.is_some_and(|record| record.key == key)
        && writer.bound_task_id == Some(key.task_id)
}
fn publish(slot: usize, writer: &mut IdentityWriter) {
    let word = writer
        .exec_intent
        .map_or(0, |record| record.key.word(record.phase));
    WORDS[slot].store(word, Ordering::Release);
    commit_writer(slot, writer);
}
pub(in crate::multitask) fn begin(
    slot: usize,
    task_id: u64,
    reservation: ExecReservation,
    current: bool,
) -> Option<Intent> {
    with_writer(slot, |writer| {
        let identity = writer.identity?;
        if current && crate::multitask::cpu_local::current_cpu_task_slot() != Some(slot) {
            return None;
        }
        if writer.exec_intent.is_some()
            || super::cohort::busy(slot)
            || writer.lifecycle.retired
            || writer.lifecycle.exec_quiesced
            || identity.task_id != Some(task_id)
            || identity.process_handle != Some(reservation.process_handle())
            || identity.process_id != Some(task_id)
            || !identity.user_mode
            || identity.user_binding().is_none()
        {
            return None;
        }
        let generation = writer
            .exec_generation
            .checked_add(1)
            .filter(|g| *g <= u64::MAX / 8)?;
        let key = Credential {
            slot,
            task_id,
            reservation,
            generation,
        };
        // Join descriptor/reservation admission to the intent publication;
        // no teardown/rebind can slip between a copied test and the seal.
        crate::multitask::scheduler::stack_resources::with_cold_binding(slot, task_id, || {
            writer.exec_generation = generation;
            writer.exec_intent = Some(Record {
                key,
                phase: SEALED,
                current,
            });
            writer.lifecycle.exec_quiesced = !current;
            publish(slot, writer);
            Some(Intent {
                key,
                live: Cell::new(true),
            })
        })?
    })?
}
impl Intent {
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.key.slot
    }
    pub(in crate::multitask) fn mode_matches(&self, current: bool) -> bool {
        with_writer(self.key.slot, |writer| {
            exact(writer, self.key)
                && writer
                    .exec_intent
                    .is_some_and(|record| record.current == current)
        })
        .unwrap_or(false)
    }
    pub(in crate::multitask) fn reservation(&self) -> ExecReservation {
        self.key.reservation
    }
    pub(in crate::multitask) fn credential(&self) -> Credential {
        self.key
    }
    pub(in crate::multitask) fn task_id(&self) -> u64 {
        self.key.task_id
    }
    pub(in crate::multitask) fn matches(&self, slot: usize, reservation: ExecReservation) -> bool {
        self.live.get()
            && self.key.slot == slot
            && self.key.reservation == reservation
            && with_writer(slot, |writer| {
                exact(writer, self.key)
                    && writer.exec_intent.is_some_and(|r| r.phase == SEALED)
                    && (writer.lifecycle.exec_quiesced
                        || writer.exec_intent.is_some_and(|r| r.current))
            })
            .unwrap_or(false)
    }
    pub(in crate::multitask) fn image(&self, reservation: ExecReservation) -> Option<Image<'_>> {
        if !self.matches(self.key.slot, reservation) {
            return None;
        }
        with_writer(self.key.slot, |writer| {
            if !exact(writer, self.key) || writer.exec_intent?.phase != SEALED {
                return None;
            }
            writer.exec_intent.as_mut()?.phase = IMAGE;
            writer.lifecycle.exec_quiesced = true;
            publish(self.key.slot, writer);
            Some(Image {
                intent: self,
                live: true,
                published: false,
            })
        })?
    }
    /// No owner lock or caller-enabled IRQ mask spans the process cancellation
    /// callback; the standalone unit prevents the self gate from stranding its CPU.
    pub(in crate::multitask) fn cancel_with(
        self,
        preemption: nucleus_core::util::lockdep::DispatchPreemption<'static>,
        cancel: impl FnOnce(
            ExecReservation,
        ) -> Option<crate::multitask::process_table::ExecCancellation>,
    ) -> (usize, u64, bool) {
        let preemption = Some(preemption);
        self.prepare_cancel();
        let cancelled = cancel(self.key.reservation)
            .expect("cold exec intent lost exact process cancellation ownership");
        let outcome = (self.key.slot, self.key.task_id, cancelled.terminal());
        self.cancel(cancelled);
        drop(preemption);
        outcome
    }

    /// Close before process cancellation; caller retains CPU preemption without
    /// masking caller-enabled IPIs during the process-owner wait.
    pub(in crate::multitask) fn prepare_cancel(&self) {
        assert!(
            with_writer(self.key.slot, |writer| {
                if !exact(writer, self.key) || writer.exec_intent.is_none_or(|r| r.phase != SEALED)
                {
                    return false;
                }
                writer.exec_intent.as_mut().unwrap().phase = CANCELLING;
                writer.lifecycle.exec_quiesced = true;
                publish(self.key.slot, writer);
                true
            })
            .unwrap_or(false),
            "exec cancellation lacks sealed intent custody"
        );
    }

    /// Consume only after the exact process reservation was cancelled. Owned
    /// IMAGE/PUBLISHED transactions cannot be rolled back or reopened.
    pub(in crate::multitask) fn cancel(
        self,
        cancellation: crate::multitask::process_table::ExecCancellation,
    ) {
        assert!(
            cancellation.matches(self.key.reservation),
            "foreign exec cancellation receipt"
        );
        assert!(
            with_writer(self.key.slot, |writer| {
                if !exact(writer, self.key)
                    || writer.exec_intent.is_none_or(|r| r.phase != CANCELLING)
                {
                    return false;
                }
                writer.exec_intent = None;
                writer.lifecycle.exec_quiesced = cancellation.terminal();
                publish(self.key.slot, writer);
                true
            })
            .unwrap_or(false),
            "exec intent cancellation lost exact sealed transaction"
        );
        self.live.set(false);
    }
    /// Process visibility finalized and the image writer has left its scope.
    pub(in crate::multitask) fn complete(
        &self,
        finalized: &crate::multitask::process_table::ExecFinalize,
    ) {
        assert_eq!(
            finalized.intent(),
            Some(self.key),
            "exec intent completion lacks exact process finalization"
        );
        assert!(
            with_writer(self.key.slot, |writer| {
                if !exact(writer, self.key)
                    || writer.exec_intent.is_none_or(|r| r.phase != PUBLISHED)
                    || crate::multitask::scheduler::stack_resources::image_writing(self.key.slot)
                {
                    return false;
                }
                writer.exec_intent = None;
                writer.lifecycle.exec_quiesced = finalized.exit_pending();
                publish(self.key.slot, writer);
                true
            })
            .unwrap_or(false),
            "exec intent completion lacks exact published transaction"
        );
        self.live.set(false);
    }
}
impl Image<'_> {
    pub(in crate::multitask) fn credential(&self) -> Credential {
        self.intent.key
    }
    pub(in crate::multitask) fn publication(&self, reservation: ExecReservation) {
        assert_eq!(
            self.intent.key.reservation, reservation,
            "foreign exec intent receipt"
        );
        assert!(
            with_writer(self.intent.key.slot, |writer| {
                if !exact(writer, self.intent.key)
                    || writer.exec_intent.is_none_or(|r| r.phase != IMAGE)
                {
                    return false;
                }
                writer.exec_intent.as_mut().unwrap().phase = PUBLISHED;
                publish(self.intent.key.slot, writer);
                true
            })
            .unwrap_or(false),
            "exec publication lost cold intent custody"
        );
    }
    pub(in crate::multitask) fn finish(mut self, published: bool) {
        if published {
            assert!(
                self.intent.key.published(self.intent.key.reservation),
                "exec image completion lost publication"
            );
            self.published = true;
        } else {
            assert!(
                with_writer(self.intent.key.slot, |writer| {
                    if !exact(writer, self.intent.key)
                        || writer.exec_intent.is_none_or(|r| r.phase != IMAGE)
                    {
                        return false;
                    }
                    writer.exec_intent.as_mut().unwrap().phase = SEALED;
                    writer.lifecycle.exec_quiesced = !writer.exec_intent.unwrap().current;
                    publish(self.intent.key.slot, writer);
                    true
                })
                .unwrap_or(false),
                "published exec image cannot be cancelled"
            );
        }
        self.live = false;
    }
}
fn poison(key: Credential) {
    with_writer(key.slot, |writer| {
        if exact(writer, key) {
            writer.exec_intent.as_mut().unwrap().phase = POISONED;
            writer.lifecycle.exec_quiesced = true;
            publish(key.slot, writer);
        }
    });
}
impl Drop for Intent {
    fn drop(&mut self) {
        if self.live.get() {
            poison(self.key);
        }
    }
}
impl Drop for Image<'_> {
    fn drop(&mut self) {
        if self.live {
            poison(self.intent.key);
        }
    }
}
#[cfg(test)]
mod tests;
