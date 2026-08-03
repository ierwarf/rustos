//! Generation-bound claim/revoke primitive for cross-domain transports.
//!
//! Operations may publish shared cursors or slots only while holding a claim
//! for the exact active epoch. Revocation first closes admission, then waits
//! for all existing claims to leave before the owner resets or unmaps shared
//! state. IRQ/NMI callers publish a request bit elsewhere and never wait here.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

const DETACHED: u8 = 0;
const ACTIVE: u8 = 1;
const DRAINING: u8 = 2;
const REVOKED: u8 = 3;
const ACTIVATING: u8 = 4;

pub(crate) struct TransportLifecycle {
    state: AtomicU8,
    epoch: AtomicU64,
    in_flight: AtomicU32,
    revoke_seq: AtomicU64,
}

impl TransportLifecycle {
    pub(crate) const fn detached() -> Self {
        Self {
            state: AtomicU8::new(DETACHED),
            epoch: AtomicU64::new(0),
            in_flight: AtomicU32::new(0),
            revoke_seq: AtomicU64::new(0),
        }
    }

    pub(crate) fn activate(&self, epoch: u64) -> bool {
        if epoch == 0 || self.in_flight.load(Ordering::Acquire) != 0 {
            return false;
        }
        let state = self.state.load(Ordering::Acquire);
        if !matches!(state, DETACHED | REVOKED) {
            return state == ACTIVE && self.epoch.load(Ordering::Acquire) == epoch;
        }
        if self
            .state
            .compare_exchange(state, ACTIVATING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.epoch.store(epoch, Ordering::Release);
        self.state.store(ACTIVE, Ordering::Release);
        true
    }

    pub(crate) fn try_claim(&self, expected_epoch: u64) -> Option<TransportClaim<'_>> {
        if expected_epoch == 0
            || self.state.load(Ordering::Acquire) != ACTIVE
            || self.epoch.load(Ordering::Acquire) != expected_epoch
        {
            return None;
        }
        self.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .ok()?;
        if self.state.load(Ordering::Acquire) != ACTIVE
            || self.epoch.load(Ordering::Acquire) != expected_epoch
        {
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
        let _ = self
            .state
            .compare_exchange(ACTIVE, DRAINING, Ordering::AcqRel, Ordering::Acquire);
        sequence
    }

    /// Returns the retired epoch only after every admitted operation left.
    pub(crate) fn finish_drain(&self) -> Option<u64> {
        let state = self.state.load(Ordering::Acquire);
        if self.in_flight.load(Ordering::Acquire) != 0 {
            return None;
        }
        if state == REVOKED {
            return Some(self.epoch.load(Ordering::Acquire));
        }
        if state != DRAINING {
            return None;
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        self.state
            .compare_exchange(DRAINING, REVOKED, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| epoch)
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub(crate) fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Acquire)
    }

    fn release_claim(&self) {
        let prior = self.in_flight.fetch_sub(1, Ordering::Release);
        assert_ne!(prior, 0, "transport lifecycle claim underflow");
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
        self.lifecycle.state.load(Ordering::Acquire) == ACTIVE
            && self.lifecycle.epoch.load(Ordering::Acquire) == self.epoch
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
}
