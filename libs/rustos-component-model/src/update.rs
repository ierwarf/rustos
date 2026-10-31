//! Staged user-space service release decisions and rollback receipts.
//!
//! Artifact bytes and signature verification are owned by a separate trusted
//! image-admission service. A digest received from a client does *not* grant
//! permission to load it; this is only a policy transaction. Rootd has to
//! perform the actual loader/lease swap after validating its signer and Job.

use crate::identity::ComponentId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseId {
    pub epoch: u64,
    pub digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Staged,
    Activating,
    Trial,
    Committed,
    RollingBack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateError {
    Invalid,
    NotReady,
    Stale,
    WrongIncarnation,
    Overflow,
    Downgrade,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateReceipt {
    pub component: ComponentId,
    pub transaction: u64,
    pub previous: ReleaseId,
    pub candidate: ReleaseId,
    pub expected_incarnation: u64,
    pub activate_before: u64,
    pub trial_deadline: u64,
}

pub struct Update {
    component: ComponentId,
    current: ReleaseId,
    state: UpdateState,
    transaction: u64,
    pending: Option<UpdateReceipt>,
    trial_incarnation: u64,
}

impl Update {
    pub fn new(component: ComponentId, installed: ReleaseId) -> Result<Self, UpdateError> {
        if installed.epoch == 0 || installed.digest == [0; 32] {
            return Err(UpdateError::Invalid);
        }
        Ok(Self {
            component,
            current: installed,
            state: UpdateState::Idle,
            transaction: 0,
            pending: None,
            trial_incarnation: 0,
        })
    }

    pub fn installed(&self) -> ReleaseId {
        self.current
    }
    pub fn state(&self) -> UpdateState {
        self.state
    }

    pub fn stage(
        &mut self,
        candidate: ReleaseId,
        expected_incarnation: u64,
        activate_before: u64,
        trial_deadline: u64,
    ) -> Result<UpdateReceipt, UpdateError> {
        if !matches!(self.state, UpdateState::Idle | UpdateState::Committed)
            || expected_incarnation == 0
            || activate_before >= trial_deadline
            || candidate.digest == [0; 32]
        {
            return Err(UpdateError::Invalid);
        }
        if candidate.epoch <= self.current.epoch {
            return Err(UpdateError::Downgrade);
        }
        let transaction = self
            .transaction
            .checked_add(1)
            .ok_or(UpdateError::Overflow)?;
        let receipt = UpdateReceipt {
            component: self.component,
            transaction,
            previous: self.current,
            candidate,
            expected_incarnation,
            activate_before,
            trial_deadline,
        };
        self.transaction = transaction;
        self.pending = Some(receipt);
        self.trial_incarnation = 0;
        self.state = UpdateState::Staged;
        Ok(receipt)
    }

    pub fn begin_activation(
        &mut self,
        receipt: UpdateReceipt,
        now: u64,
    ) -> Result<(), UpdateError> {
        if self.pending != Some(receipt) || self.state != UpdateState::Staged {
            return Err(UpdateError::Stale);
        }
        if now > receipt.activate_before {
            return Err(UpdateError::NotReady);
        }
        self.state = UpdateState::Activating;
        Ok(())
    }

    pub fn report_activated(
        &mut self,
        receipt: UpdateReceipt,
        previous_incarnation: u64,
        new_incarnation: u64,
        now: u64,
    ) -> Result<(), UpdateError> {
        if self.pending != Some(receipt) || self.state != UpdateState::Activating {
            return Err(UpdateError::Stale);
        }
        if previous_incarnation != receipt.expected_incarnation
            || new_incarnation <= previous_incarnation
        {
            return Err(UpdateError::WrongIncarnation);
        }
        if now >= receipt.trial_deadline {
            return Err(UpdateError::NotReady);
        }
        self.trial_incarnation = new_incarnation;
        self.state = UpdateState::Trial;
        Ok(())
    }

    /// Trial commit requires exact active incarnation and a fresh healthy
    /// supervisor observation. Neither an ordinary IPC reply nor a stale
    /// checkpoint can stand in for the owning health witness.
    pub fn finish_trial(
        &mut self,
        receipt: UpdateReceipt,
        incarnation: u64,
        now: u64,
        healthy: bool,
    ) -> Result<(), UpdateError> {
        if self.pending != Some(receipt) || self.state != UpdateState::Trial {
            return Err(UpdateError::Stale);
        }
        if incarnation != self.trial_incarnation {
            return Err(UpdateError::WrongIncarnation);
        }
        if now >= receipt.trial_deadline || !healthy {
            return Err(UpdateError::NotReady);
        }
        self.current = receipt.candidate;
        self.pending = None;
        self.state = UpdateState::Committed;
        Ok(())
    }

    pub fn request_rollback(&mut self, receipt: UpdateReceipt) -> Result<ReleaseId, UpdateError> {
        if self.pending != Some(receipt)
            || !matches!(
                self.state,
                UpdateState::Staged | UpdateState::Activating | UpdateState::Trial
            )
        {
            return Err(UpdateError::Stale);
        }
        self.state = UpdateState::RollingBack;
        Ok(receipt.previous)
    }

    /// Restore the old artifact only after a separate verified loader step.
    pub fn rollback_complete(
        &mut self,
        receipt: UpdateReceipt,
        restored: ReleaseId,
    ) -> Result<(), UpdateError> {
        if self.pending != Some(receipt)
            || self.state != UpdateState::RollingBack
            || restored != receipt.previous
        {
            return Err(UpdateError::Stale);
        }
        self.current = restored;
        self.pending = None;
        self.state = UpdateState::Idle;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ComponentId;

    #[test]
    fn staged_update_requires_exact_trial_incarnation() {
        let id = ComponentId::new(1, 1).unwrap();
        let base = ReleaseId {
            epoch: 1,
            digest: [1; 32],
        };
        let next = ReleaseId {
            epoch: 2,
            digest: [2; 32],
        };
        let mut u = Update::new(id, base).unwrap();
        assert_eq!(u.stage(base, 1, 10, 100), Err(UpdateError::Downgrade));
        let receipt = u.stage(next, 1, 10, 100).unwrap();
        u.begin_activation(receipt, 3).unwrap();
        assert_eq!(
            u.report_activated(receipt, 1, 1, 5),
            Err(UpdateError::WrongIncarnation)
        );
        u.report_activated(receipt, 1, 2, 5).unwrap();
        assert_eq!(
            u.finish_trial(receipt, 1, 20, true),
            Err(UpdateError::WrongIncarnation)
        );
        assert_eq!(
            u.finish_trial(receipt, 2, 100, true),
            Err(UpdateError::NotReady)
        );
        u.finish_trial(receipt, 2, 30, true).unwrap();
        assert_eq!(u.installed(), next);
    }

    #[test]
    fn rollback_retains_exact_old_release() {
        let id = ComponentId::new(1, 1).unwrap();
        let old = ReleaseId {
            epoch: 3,
            digest: [3; 32],
        };
        let proposed = ReleaseId {
            epoch: 4,
            digest: [4; 32],
        };
        let mut u = Update::new(id, old).unwrap();
        let receipt = u.stage(proposed, 10, 20, 100).unwrap();
        assert_eq!(u.request_rollback(receipt), Ok(old));
        assert_eq!(
            u.rollback_complete(receipt, proposed),
            Err(UpdateError::Stale)
        );
        u.rollback_complete(receipt, old).unwrap();
        assert_eq!(u.installed(), old);
        assert_eq!(u.state(), UpdateState::Idle);
    }
}
