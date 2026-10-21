//! Owner: exact wait-payload epoch and original typed wake publication phase.
//! Boundary: original Binding loan -> per-slot Wait58 scalar leaf -> queue after unlock.
//! Lifecycle: checked epoch arm -> exact match -> pending wake -> clear/publish -> release.
//! Concurrency: pending wake excludes new arms without holding a raw leaf through work.
//! Failure: exhausted nonce, changed/cleared wait or terminal owner cannot match or rearm.
//! Forbidden: RunOwner generation changes, copied payload grants, leaves across frame/IRQ.
//! Evidence: actual shared wait CAS and original physical Binding; native entry is modeled.
use super::*;
use crate::multitask::scheduler::wake_publication::Binding;
use nucleus_core::util::lockdep::LockClass;
use crate::multitask::irq_leaf::IrqLeaf;

pub(super) struct State {
    serial: u64,
    pending: Option<u64>,
}
impl State {
    const fn new() -> Self {
        Self {
            serial: 0,
            pending: None,
        }
    }
    pub(super) fn advance(&mut self) -> bool {
        let Some(next) = self.serial.checked_add(1) else {
            return false;
        };
        self.serial = next;
        true
    }
    pub(super) fn can_arm(&self) -> bool {
        self.pending.is_none()
    }
}
static EPOCHS: [IrqLeaf<State, { LockClass::TaskWaitEpoch as u8 }>; MAX_TASK] =
    [const { IrqLeaf::new(State::new()) }; MAX_TASK];

pub(super) fn with<R>(slot: usize, f: impl FnOnce(&mut State) -> R) -> R {
    f(&mut EPOCHS[slot].lock())
}

/// What a refused wake observed about the target's wait, for a later retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask::scheduler) enum WaitInstance {
    /// No wait is published (or the owner is terminal): nothing to wake.
    Idle,
    /// Another typed wake currently owns this wait's publication phase.
    WakePending,
    /// The exact wait instance: arm epoch plus reason kind and object.
    Waiting { serial: u64, kind: u8, id: u64 },
}

/// Read-only observation of the slot's current wait instance. The epoch serial
/// advances on every arm, cancel and wake, so an equal serial with the same
/// reason is the same, still-uncompleted wait. Grants no wake or custody.
pub(in crate::multitask::scheduler) fn wait_instance(slot: usize) -> WaitInstance {
    with(slot, |state| {
        let (kind, id) = super::reason_locked(slot);
        if kind == REASON_NONE || owner(slot).state.is_terminal() {
            WaitInstance::Idle
        } else if matches!(state.pending, Some(_)) {
            WaitInstance::WakePending
        } else {
            WaitInstance::Waiting { serial: state.serial, kind, id }
        }
    })
}

/// Non-Copy pending publication borrows the ORIGINAL actual wake binding.
/// A nonce/payload alone is not wake, frame, execution or reuse permission.
pub(in crate::multitask::scheduler) struct TypedWake<'a> {
    binding: &'a Binding,
    serial: u64,
    kind: u8,
    id: u64,
    cleared: core::cell::Cell<bool>,
}
impl<'a> TypedWake<'a> {
    /// Negative receive observation only: a timer/other original wake already
    /// withdrew the endpoint arm while its runtime waiter was still published.
    /// This does NOT wake, bind funding or grant execution. The caller can keep
    /// its exact queued reply AwaitingReceiver for a later actual receive.
    pub(in crate::multitask::scheduler) fn receive_was_withdrawn(binding: &Binding) -> bool {
        with(binding.slot(), |state| {
            let owner = owner(binding.slot());
            state.pending.is_none()
                && super::reason_locked(binding.slot()) == (REASON_NONE, 0)
                && !owner.wait_armed && !super::super::wait_blocked(owner)
                && owner.runnable && !owner.state.is_terminal()
        })
    }

    pub(in crate::multitask::scheduler) fn capture(
        binding: &'a Binding,
        kind: u8,
        id: u64,
    ) -> Option<Self> {
        if kind == REASON_NONE {
            return None;
        }
        with(binding.slot(), |state| {
            if state.pending.is_some()
                || super::reason_locked(binding.slot()) != (kind, id)
                || owner(binding.slot()).state.is_terminal()
            {
                return None;
            }
            // Reserve the clear's successor before any publication ownership.
            // Exhaustion refuses capture, never panics after taking a wake loan.
            state.serial.checked_add(1)?;
            state.pending = Some(state.serial);
            Some(Self {
                binding,
                serial: state.serial,
                kind,
                id,
                cleared: core::cell::Cell::new(false),
            })
        })
    }
    pub(in crate::multitask::scheduler) fn clear(&self) -> bool {
        self.publish_and_clear(|| Some(())).is_some()
    }

    /// Join only a scalar owner CAS to this ORIGINAL pending wait. No queue,
    /// resource, donation, frame, allocation or notification work enters `publish`.
    /// A refused CAS leaves both the wait epoch and payload untouched.
    pub(in crate::multitask::scheduler) fn publish_and_clear<R>(
        &self,
        publish: impl FnOnce() -> Option<R>,
    ) -> Option<R> {
        if self.cleared.get() {
            return None;
        }
        let result = with(self.binding.slot(), |state| {
            if state.pending != Some(self.serial)
                || state.serial != self.serial
                || super::reason_locked(self.binding.slot()) != (self.kind, self.id)
                || owner(self.binding.slot()).state.is_terminal()
            {
                return None;
            }
            let result = publish()?;
            assert!(
                state.advance(),
                "scheduler wait epoch exhausted during matched wake"
            );
            super::clear_wake_locked(self.binding.slot());
            Some(result)
        });
        if result.is_some() {
            self.cleared.set(true);
        }
        result
    }
}
impl Drop for TypedWake<'_> {
    fn drop(&mut self) {
        with(self.binding.slot(), |state| {
            assert_eq!(
                state.pending,
                Some(self.serial),
                "original typed wake publication lost its owner"
            );
            state.pending = None;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multitask::scheduler::{root_creation, runqueue};
    #[test]
    fn actual_exhausted_wait_epoch_refuses_capture_and_arm_before_payload_or_pending_effects() {
        let id = 81700;
        root_creation::tests::exec_root(
            id,
            true,
            root_creation::tests::bootstrap(crate::user::abi::UserAbi::Linux),
            |slot, _| {
                runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
                assert!(try_publish_arm(slot, REASON_PAGER_FAULT, 901));
                let binding = Binding::capture(slot, id).unwrap();
                let owner = owner(slot);
                // Explicit native exhausted-nonce input; physical original loan is real.
                let original = with(slot, |state| {
                    let original = state.serial;
                    state.serial = u64::MAX;
                    original
                });
                assert!(TypedWake::capture(&binding, REASON_PAGER_FAULT, 901).is_none());
                assert!(!try_publish_arm(slot, REASON_PAGER_FAULT, 902));
                assert_eq!(reason(slot), (REASON_PAGER_FAULT, 901));
                assert_eq!(runqueue::owner(slot), owner);
                with(slot, |state| {
                    assert!(state.pending.is_none());
                    // Fixture-only negative input recovery, never a production nonce reset.
                    state.serial = original;
                });
            },
        );
    }
}
