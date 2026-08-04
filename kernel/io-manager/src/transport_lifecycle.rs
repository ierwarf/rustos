//! Generation-bound claim/revoke primitive for cross-domain transports.
//!
//! Operations may publish shared cursors or slots only while holding a claim
//! for the exact active epoch. Revocation first closes admission, then waits
//! for all existing claims to leave before the owner resets or unmaps shared
//! state. IRQ/NMI callers publish a request bit elsewhere and never wait here.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const STATE_BITS: u32 = 3;
const STATE_MASK: u64 = (1 << STATE_BITS) - 1;
const MAX_EPOCH: u64 = u64::MAX >> STATE_BITS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum TransportState {
    Detached = 0,
    Active = 1,
    Draining = 2,
    Revoked = 3,
    Activating = 4,
}

impl TransportState {
    fn from_word(word: u64) -> Self {
        match (word & STATE_MASK) as u8 {
            0 => Self::Detached,
            1 => Self::Active,
            2 => Self::Draining,
            3 => Self::Revoked,
            4 => Self::Activating,
            _ => panic!("invalid packed transport lifecycle state"),
        }
    }
}

const fn pack(epoch: u64, state: TransportState) -> u64 {
    (epoch << STATE_BITS) | state as u64
}

const fn unpack_epoch(word: u64) -> u64 {
    word >> STATE_BITS
}

pub(crate) struct TransportLifecycle {
    /// The epoch and state have one linearization word. In particular, drain
    /// can replace `Activating(e)` with `Draining(e)` and activation commit
    /// must then fail its exact compare-exchange rather than resurrecting the
    /// transport with an unconditional `ACTIVE` store.
    state_epoch: AtomicU64,
    in_flight: AtomicU32,
    revoke_seq: AtomicU64,
}

impl TransportLifecycle {
    pub(crate) const fn detached() -> Self {
        Self {
            state_epoch: AtomicU64::new(pack(0, TransportState::Detached)),
            in_flight: AtomicU32::new(0),
            revoke_seq: AtomicU64::new(0),
        }
    }

    pub(crate) fn activate_begin(&self, epoch: u64) -> Option<ActivationToken<'_>> {
        if epoch == 0 || epoch > MAX_EPOCH || self.in_flight.load(Ordering::Acquire) != 0 {
            return None;
        }
        loop {
            let current = self.state_epoch.load(Ordering::Acquire);
            let state = TransportState::from_word(current);
            let current_epoch = unpack_epoch(current);
            if state == TransportState::Active && current_epoch == epoch {
                return Some(ActivationToken {
                    lifecycle: self,
                    activating_word: current,
                    committed: true,
                });
            }
            if !matches!(state, TransportState::Detached | TransportState::Revoked) {
                return None;
            }
            let activating = pack(epoch, TransportState::Activating);
            match self.state_epoch.compare_exchange_weak(
                current,
                activating,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ActivationToken {
                        lifecycle: self,
                        activating_word: activating,
                        committed: false,
                    });
                }
                Err(_) => continue,
            }
        }
    }

    pub(crate) fn activate(&self, epoch: u64) -> bool {
        self.activate_begin(epoch)
            .is_some_and(ActivationToken::commit)
    }

    pub(crate) fn try_claim(&self, expected_epoch: u64) -> Option<TransportClaim<'_>> {
        let expected = pack(expected_epoch, TransportState::Active);
        if expected_epoch == 0 || self.state_epoch.load(Ordering::Acquire) != expected {
            return None;
        }
        self.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .ok()?;
        if self.state_epoch.load(Ordering::Acquire) != expected {
            self.release_claim();
            return None;
        }
        Some(TransportClaim {
            lifecycle: self,
            epoch: expected_epoch,
        })
    }

    pub(crate) fn request_drain(&self) -> u64 {
        let sequence = self
            .revoke_seq
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("transport lifecycle revoke sequence exhausted"))
            .checked_add(1)
            .expect("transport lifecycle revoke sequence wrapped");
        loop {
            let current = self.state_epoch.load(Ordering::Acquire);
            let state = TransportState::from_word(current);
            if !matches!(state, TransportState::Active | TransportState::Activating) {
                break;
            }
            let draining = pack(unpack_epoch(current), TransportState::Draining);
            if self
                .state_epoch
                .compare_exchange_weak(current, draining, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        sequence
    }

    /// Returns the retired epoch only after every admitted operation left.
    pub(crate) fn finish_drain(&self) -> Option<u64> {
        let current = self.state_epoch.load(Ordering::Acquire);
        let state = TransportState::from_word(current);
        if self.in_flight.load(Ordering::Acquire) != 0 {
            return None;
        }
        if state == TransportState::Revoked {
            return Some(unpack_epoch(current));
        }
        if state != TransportState::Draining {
            return None;
        }
        let epoch = unpack_epoch(current);
        self.state_epoch
            .compare_exchange(
                current,
                pack(epoch, TransportState::Revoked),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            .map(|_| epoch)
    }

    pub(crate) fn epoch(&self) -> u64 {
        unpack_epoch(self.state_epoch.load(Ordering::Acquire))
    }

    pub(crate) fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Acquire)
    }

    fn release_claim(&self) {
        let prior = self.in_flight.fetch_sub(1, Ordering::Release);
        assert_ne!(prior, 0, "transport lifecycle claim underflow");
    }
}

pub(crate) struct ActivationToken<'a> {
    lifecycle: &'a TransportLifecycle,
    activating_word: u64,
    committed: bool,
}

impl ActivationToken<'_> {
    /// Publish all mapping/header initialization before committing this token.
    /// A drain that wins while activation is in progress changes the exact
    /// packed word, so this operation fails closed and can never reactivate it.
    pub(crate) fn commit(mut self) -> bool {
        if self.committed {
            return true;
        }
        let epoch = unpack_epoch(self.activating_word);
        if self
            .lifecycle
            .state_epoch
            .compare_exchange(
                self.activating_word,
                pack(epoch, TransportState::Active),
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.committed = true;
            true
        } else {
            false
        }
    }
}

impl Drop for ActivationToken<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let epoch = unpack_epoch(self.activating_word);
        let _ = self.lifecycle.state_epoch.compare_exchange(
            self.activating_word,
            pack(epoch, TransportState::Revoked),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

pub(crate) struct TransportClaim<'a> {
    lifecycle: &'a TransportLifecycle,
    epoch: u64,
}

impl TransportClaim<'_> {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn validate_current(&self) -> bool {
        self.lifecycle.state_epoch.load(Ordering::Acquire)
            == pack(self.epoch, TransportState::Active)
    }
}

impl Drop for TransportClaim<'_> {
    fn drop(&mut self) {
        self.lifecycle.release_claim();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_closes_admission_and_waits_for_exact_claim() {
        let lifecycle = TransportLifecycle::detached();
        assert!(lifecycle.activate(7));
        let claim = lifecycle.try_claim(7).expect("active claim");
        lifecycle.request_drain();
        assert!(lifecycle.try_claim(7).is_none());
        assert_eq!(lifecycle.finish_drain(), None);
        assert!(!claim.validate_current());
        drop(claim);
        assert_eq!(lifecycle.finish_drain(), Some(7));
        assert!(lifecycle.activate(8));
        assert_eq!(lifecycle.epoch(), 8);
    }

    #[test]
    fn drain_during_activation_cannot_be_overwritten_by_commit() {
        let lifecycle = TransportLifecycle::detached();
        let activation = lifecycle.activate_begin(11).expect("activation token");
        lifecycle.request_drain();
        assert!(!activation.commit());
        assert_eq!(lifecycle.finish_drain(), Some(11));
        assert!(lifecycle.activate(12));
    }
}
