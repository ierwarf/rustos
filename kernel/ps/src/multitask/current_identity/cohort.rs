//! Exact binding custody and one release point for multi-slot cold publication.
//! No owner leaf spans callbacks, queues, allocation, hardware or destruction.
use super::{execution, with_writer};
use crate::multitask::MAX_SCHEDULER_TASKS as MAX;
use core::sync::atomic::{AtomicU64, Ordering};
const PENDING: u64 = 1;
const COMPLETE: u64 = 2;
const POISON: u64 = 3;
const _: () = assert!(MAX <= 255);
static BATCHES: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];
static MEMBERS: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];

pub(in crate::multitask) struct Cohort {
    root: usize,
    epoch: u64,
    cookie: u64,
    pins: [Option<execution::BindingPin>; MAX],
    len: usize,
    effects: bool,
    activation: bool,
    job_control: bool,
    live: bool,
}
/// Busy for mutation/lifetime, including the brief complete-but-not-unsealed phase.
pub(in crate::multitask) fn busy(slot: usize) -> bool {
    MEMBERS
        .get(slot)
        .is_some_and(|word| word.load(Ordering::Acquire) != 0)
}
pub(super) fn check_unowned_gate(slot: usize, gate: super::LifecycleGate) {
    assert!(
        !matches!(gate, super::LifecycleGate::Suspended | super::LifecycleGate::JobStopped) || !busy(slot),
        "lifecycle command lacks cold cohort authority"
    );
}

pub(super) fn admission_ready(slot: usize) -> bool {
    let Some(member) = MEMBERS.get(slot) else {
        return false;
    };
    let cookie = member.load(Ordering::Acquire);
    if cookie == 0 {
        return true;
    }
    let root = ((cookie & 255) - 1) as usize;
    BATCHES
        .get(root)
        .is_some_and(|word| word.load(Ordering::Acquire) == ((cookie >> 8) << 2) | COMPLETE)
}
impl Cohort {
    pub(in crate::multitask) fn begin(members: &[(usize, u64)]) -> Option<Self> {
        Self::begin_kind(members, false)
    }
    fn begin_kind(members: &[(usize, u64)], activation: bool) -> Option<Self> {
        let &(root, _) = members.first()?;
        if members.len() > MAX || root >= MAX {
            return None;
        }
        for (i, &(slot, _)) in members.iter().enumerate() {
            if slot >= MAX || members[..i].iter().any(|&(old, _)| old == slot) {
                return None;
            }
        }
        let ledger = &BATCHES[root];
        let old = ledger.load(Ordering::Acquire);
        if old & 3 != 0 {
            return None;
        }
        let epoch = (old >> 2)
            .checked_add(1)
            .filter(|epoch| *epoch <= u64::MAX >> 8)?;
        ledger
            .compare_exchange(
                old,
                epoch << 2 | PENDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;
        let cookie = epoch << 8 | (root as u64 + 1);
        let mut owner = Self {
            root,
            epoch,
            cookie,
            pins: [const { None }; MAX],
            len: 0,
            effects: false,
            activation,
            job_control: false,
            live: true,
        };
        for &(slot, id) in members {
            let pin = with_writer(slot, |writer| {
                if busy(slot)
                    || writer.kernel_executors != 0
                    || writer.exec_intent.is_some()
                    || writer.lifecycle.retired
                    || (activation && !writer.lifecycle.suspended)
                    || writer.identity.and_then(|identity| identity.task_id) != Some(id)
                {
                    return None;
                }
                let pin = execution::pin_binding(slot, writer, id)?;
                MEMBERS[slot].store(cookie, Ordering::Release);
                Some(pin)
            })??;
            owner.pins[owner.len] = Some(pin);
            owner.len += 1;
        }
        Some(owner)
    }
    pub(in crate::multitask) fn begin_activation(members: &[(usize, u64)]) -> Option<Self> {
        Self::begin_kind(members, true)
    }
    pub(in crate::multitask) fn begin_job_control(members: &[(usize, u64)]) -> Option<Self> {
        let mut owner = Self::begin_kind(members, false)?;
        owner.job_control = true;
        Some(owner)
    }
    pub(in crate::multitask) fn owns_affinity(&self, slot: usize, id: u64) -> bool {
        !self.activation && !self.job_control && self.owns_member(slot, id)
    }
    pub(in crate::multitask) fn owns_job_control(&self, slot: usize, id: u64) -> bool {
        self.job_control && !self.activation && self.owns_member(slot, id)
    }
    /// The captured original binding survives terminal close; never resurrect it.
    pub(in crate::multitask) fn set_job_stopped(&self, slot: usize, id: u64, stopped: bool) -> Option<bool> {
        with_writer(slot, |writer| {
            if !self.owns_job_control(slot, id) || writer.bound_task_id != Some(id) {
                return None;
            }
            if writer.lifecycle.retired { return Some(false); }
            if writer.identity.and_then(|identity| identity.task_id) != Some(id) {
                return None;
            }
            let changed = writer.lifecycle.job_stopped != stopped;
            writer.lifecycle.job_stopped = stopped;
            super::commit_writer(slot, writer);
            Some(changed)
        })?
    }
    pub(in crate::multitask) fn unsuspend(&self, slot: usize, id: u64) -> bool {
        with_writer(slot, |writer| {
            if !self.activation
                || !self.owns_member(slot, id)
                || writer.bound_task_id != Some(id)
                || !writer.lifecycle.suspended
            {
                return false;
            }
            // A concurrent terminal stop may hide metadata, but the captured
            // binding stays pinned. It must never be reawakened by activation.
            if writer.lifecycle.retired {
                return true;
            }
            if writer.identity.and_then(|identity| identity.task_id) != Some(id) {
                return false;
            }
            writer.lifecycle.suspended = false;
            super::commit_writer(slot, writer);
            true
        })
        .unwrap_or(false)
    }

    pub(in crate::multitask) fn owns_member(&self, slot: usize, id: u64) -> bool {
        self.live
            && self.effects
            && MEMBERS
                .get(slot)
                .is_some_and(|word| word.load(Ordering::Acquire) == self.cookie)
            && BATCHES[self.root].load(Ordering::Acquire) == (self.epoch << 2 | PENDING)
            && self.pins[..self.len]
                .iter()
                .flatten()
                .any(|pin| pin.slot() == slot && pin.id() == id)
    }

    /// All refusal/preflight precedes this irreversible publication boundary.
    pub(in crate::multitask) fn begin_effects(&mut self) {
        self.effects = true;
    }
    pub(in crate::multitask) fn complete(mut self) {
        self.publish_admission();
        self.unseal();
    }
    /// Pure atomic release; may join a higher-ranked process publication leaf.
    pub(in crate::multitask) fn publish_admission(&mut self) {
        assert!(self.effects, "cohort completed without owned publication");
        BATCHES[self.root]
            .compare_exchange(
                self.epoch << 2 | PENDING,
                self.epoch << 2 | COMPLETE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .expect("cohort lost exact completion epoch");
    }
    pub(in crate::multitask) fn finish_published(mut self) {
        assert_eq!(
            BATCHES[self.root].load(Ordering::Acquire),
            self.epoch << 2 | COMPLETE
        );
        self.unseal();
    }
    fn unseal(&mut self) {
        for pin in self.pins[..self.len].iter_mut() {
            let pin = pin.take().expect("cohort lost binding pin");
            with_writer(pin.slot(), |_| {
                MEMBERS[pin.slot()]
                    .compare_exchange(self.cookie, 0, Ordering::AcqRel, Ordering::Acquire)
                    .expect("cohort unsealed a foreign slot epoch");
            })
            .expect("cohort lost slot");
            drop(pin); // outside Identity51; last release may notify reaper
        }
        let phase = BATCHES[self.root].load(Ordering::Acquire);
        assert!(phase == (self.epoch << 2 | PENDING) || phase == (self.epoch << 2 | COMPLETE));
        BATCHES[self.root]
            .compare_exchange(phase, self.epoch << 2, Ordering::AcqRel, Ordering::Acquire)
            .expect("cohort release lost producer");
        self.live = false;
    }
}
impl Drop for Cohort {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        if !self.effects {
            self.unseal();
            return;
        }
        let phase = BATCHES[self.root].load(Ordering::Acquire);
        assert!(phase == (self.epoch << 2 | PENDING) || phase == (self.epoch << 2 | COMPLETE));
        BATCHES[self.root]
            .compare_exchange(
                phase,
                self.epoch << 2 | POISON,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .expect("cohort poison lost producer");
        for pin in self.pins[..self.len].iter_mut() {
            core::mem::forget(pin.take());
        }
        // Quarantine exact UID refs and member words. Partial effects cannot reopen.
    }
}
#[cfg(test)]
mod tests;
