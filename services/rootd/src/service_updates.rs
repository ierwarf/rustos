//! Rootd-owned in-memory staging/activation/health/rollback transaction book.
//!
//! This is the **control half** of stage 8. The caller must be the exact live
//! loaderd or initd principal; a candidate digest is NOT executable proof.
//! Real signed-image admission and durable previous-image custody must be
//! supplied by the loader/image subsystem before calling it a safe update.
//! No service can commit merely by claiming that a PID is healthy.
use rustos_component_model::update_wire::Approval;
use super::service_manifest::POST_INIT_MANIFEST;

const EINVAL: i32 = 22;
const EACCES: i32 = 13;
const ESTALE: i32 = 116;
const EBUSY: i32 = 16;
const MAX_UPGRADES: usize = POST_INIT_MANIFEST.len();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Phase {
    Empty,
    Staged,
    Armed,
    Trial,
    Committed,
    RollbackRequired,
}

#[derive(Clone, Copy)]
struct Upgrade {
    service: u64,
    epoch: u64,
    candidate_digest: [u8; 32],
    previous_pid: u64,
    candidate_pid: u64,
    deadline_ns: u64,
    state: Phase,
    health_pulses: u8,
}
impl Upgrade {
    const EMPTY: Self = Self {
        service: 0,
        epoch: 0,
        candidate_digest: [0; 32],
        previous_pid: 0,
        candidate_pid: 0,
        deadline_ns: 0,
        state: Phase::Empty,
        health_pulses: 0,
    };
    fn matches(&self, approval: Approval) -> bool {
        self.service == approval.service
            && self.epoch == approval.epoch
            && self.candidate_digest == approval.digest
            && self.deadline_ns == approval.trial_deadline_ns
    }
}

pub(crate) struct UpdateBook {
    entries: [Upgrade; MAX_UPGRADES],
    committed_epochs: [u64; MAX_UPGRADES],
}

impl UpdateBook {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [Upgrade::EMPTY; MAX_UPGRADES],
            committed_epochs: [0; MAX_UPGRADES],
        }
    }

    fn position(service: u64) -> Result<usize, i32> {
        POST_INIT_MANIFEST.iter().position(|spec| spec.service_id == service)
            .ok_or(EACCES)
    }

    /// Only loaderd may stage after its independently verified image
    /// admission; rootd never trusts the digest alone as signature proof.
    pub(crate) fn stage(
        &mut self, approval: Approval, previous_pid: u64, now_ns: u64,
    ) -> Result<Phase, i32> {
        let index = Self::position(approval.service)?;
        if previous_pid == 0 || approval.epoch <= self.committed_epochs[index]
            || now_ns == 0 || approval.trial_deadline_ns <= now_ns
        {
            return Err(ESTALE);
        }
        let slot = &mut self.entries[index];
        if slot.state != Phase::Empty {
            if slot.state == Phase::Staged && slot.matches(approval)
                && slot.previous_pid == previous_pid
            {
                return Ok(Phase::Staged);
            }
            return Err(EBUSY);
        }
        *slot = Upgrade {
            service: approval.service,
            epoch: approval.epoch,
            candidate_digest: approval.digest,
            previous_pid,
            candidate_pid: 0,
            deadline_ns: approval.trial_deadline_ns,
            state: Phase::Staged,
            health_pulses: 0,
        };
        Ok(slot.state)
    }

    /// Only initd can request the cutover; rootd still requires the actual
    /// loader transition and an exact new service lease before starting trial.
    pub(crate) fn arm(&mut self, approval: Approval, now_ns: u64) -> Result<Phase, i32> {
        let slot = &mut self.entries[Self::position(approval.service)?];
        if !slot.matches(approval) || now_ns >= slot.deadline_ns {
            return Err(ESTALE);
        }
        match slot.state {
            Phase::Staged => {
                slot.state = Phase::Armed;
                Ok(slot.state)
            }
            Phase::Armed => Ok(slot.state),
            _ => Err(EBUSY),
        }
    }

    /// Pure transaction admission preflight, called BEFORE rootd publishes a
    /// replacement service lease. Once the lease has been authenticated and
    /// inserted, this exact check cannot reject the same candidate.
    pub(crate) fn may_publish(
        &self, service: u64, pid: u64, now_ns: u64,
    ) -> Result<(), i32> {
        let slot = &self.entries[Self::position(service)?];
        if slot.state == Phase::Empty {
            return Ok(());
        }
        if slot.state != Phase::Armed || pid == 0 || pid == slot.previous_pid
            || now_ns >= slot.deadline_ns
        {
            return Err(ESTALE);
        }
        Ok(())
    }

    /// Pure health preflight before mutating the independent health witness.
    /// The same monotonic timestamp is used in both transitions.
    pub(crate) fn may_accept_health(
        &self, service: u64, pid: u64, now_ns: u64,
    ) -> Result<(), i32> {
        let slot = &self.entries[Self::position(service)?];
        if slot.state == Phase::Trial
            && (slot.candidate_pid != pid || now_ns >= slot.deadline_ns)
        {
            return Err(ESTALE);
        }
        Ok(())
    }

    /// Exact live post-init lease publication, not the requester's PID, sets
    /// the candidate incarnation. This cannot invent a process.
    pub(crate) fn published(
        &mut self, service: u64, new_pid: u64, now_ns: u64,
    ) -> Result<Phase, i32> {
        self.may_publish(service, new_pid, now_ns)?;
        let slot = &mut self.entries[Self::position(service)?];
        if slot.state == Phase::Empty {
            return Ok(Phase::Empty);
        }
        slot.candidate_pid = new_pid;
        slot.state = Phase::Trial;
        slot.health_pulses = 0;
        Ok(slot.state)
    }

    /// The only automatic success path is repeated authenticated health
    /// from the **new** incarnation after it was published.
    pub(crate) fn healthy(
        &mut self, service: u64, pid: u64, now_ns: u64,
    ) -> Result<Phase, i32> {
        self.may_accept_health(service, pid, now_ns)?;
        let slot = &mut self.entries[Self::position(service)?];
        if slot.state != Phase::Trial {
            return Ok(slot.state);
        }
        slot.health_pulses = slot.health_pulses.saturating_add(1);
        if slot.health_pulses >= 3 {
            slot.state = Phase::Committed;
        }
        Ok(slot.state)
    }

    /// A rollback request seals the transaction against later commit. It
    /// does NOT imply the old executable image was actually restored.
    pub(crate) fn rollback(
        &mut self, approval: Approval,
    ) -> Result<Phase, i32> {
        let slot = &mut self.entries[Self::position(approval.service)?];
        if !slot.matches(approval) {
            return Err(ESTALE);
        }
        match slot.state {
            Phase::Armed | Phase::Staged | Phase::Trial | Phase::RollbackRequired => {
                slot.state = Phase::RollbackRequired;
                Ok(slot.state)
            }
            _ => Err(EBUSY),
        }
    }

    pub(crate) fn on_exit(&mut self, pid: u64) {
        for slot in &mut self.entries {
            if pid != 0 && pid == slot.candidate_pid && slot.state == Phase::Trial {
                slot.state = Phase::RollbackRequired;
            }
        }
    }

    /// Rootd supervisor owns timeout transitions, not the service being
    /// upgraded. A stopped candidate must never be considered committed.
    pub(crate) fn expired(&mut self, now_ns: u64) -> [Option<u64>; MAX_UPGRADES] {
        let mut result = [None; MAX_UPGRADES];
        for (i, slot) in self.entries.iter_mut().enumerate() {
            if matches!(slot.state, Phase::Staged | Phase::Armed | Phase::Trial)
                && now_ns >= slot.deadline_ns
            {
                slot.state = Phase::RollbackRequired;
                result[i] = Some(slot.service);
            }
        }
        result
    }

    /// Commit marks the epoch immutable only after exact trial health.
    /// Storage-safe irreversible commit awaits durable image custody.
    pub(crate) fn seal(&mut self, approval: Approval) -> Result<Phase, i32> {
        let i = Self::position(approval.service)?;
        let slot = &mut self.entries[i];
        if !slot.matches(approval) || slot.state != Phase::Committed {
            return Err(ESTALE);
        }
        self.committed_epochs[i] = slot.epoch;
        *slot = Upgrade::EMPTY;
        Ok(Phase::Committed)
    }

    pub(crate) fn inspect(&self, service: u64) -> Result<(Phase, u64, u64, u64), i32> {
        let slot = &self.entries[Self::position(service)?];
        Ok((slot.state, slot.epoch, slot.previous_pid, slot.candidate_pid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::IPC_SERVICE_SESSIOND;
    fn approval(epoch: u64) -> Approval {
        Approval { service: IPC_SERVICE_SESSIOND, epoch, digest: [7; 32],
            trial_deadline_ns: 1_000_000_000 }
    }
    #[test]
    fn upgrade_never_commits_without_real_new_pid_and_three_health_pulses() {
        let mut ledger = UpdateBook::new();
        assert_eq!(ledger.stage(approval(1), 11, 20), Ok(Phase::Staged));
        assert_eq!(ledger.arm(approval(1), 21), Ok(Phase::Armed));
        assert_eq!(ledger.published(IPC_SERVICE_SESSIOND, 11, 22), Err(ESTALE));
        assert_eq!(ledger.published(IPC_SERVICE_SESSIOND, 12, 23), Ok(Phase::Trial));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 12, 24), Ok(Phase::Trial));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 11, 25), Err(ESTALE));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 12, 26), Ok(Phase::Trial));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 12, 27), Ok(Phase::Committed));
        assert_eq!(ledger.seal(approval(1)), Ok(Phase::Committed));
        assert_eq!(ledger.stage(approval(1), 12, 30), Err(ESTALE));
    }
    #[test]
    fn side_effecting_lease_and_health_transitions_have_pure_admission() {
        let mut ledger = UpdateBook::new();
        ledger.stage(approval(7), 101, 1).unwrap();
        assert_eq!(ledger.may_publish(IPC_SERVICE_SESSIOND, 102, 2), Err(ESTALE));
        ledger.arm(approval(7), 3).unwrap();
        assert_eq!(ledger.may_publish(IPC_SERVICE_SESSIOND, 101, 4), Err(ESTALE));
        assert_eq!(ledger.inspect(IPC_SERVICE_SESSIOND).unwrap().0, Phase::Armed);
        assert_eq!(ledger.may_publish(IPC_SERVICE_SESSIOND, 102, 5), Ok(()));
        ledger.published(IPC_SERVICE_SESSIOND, 102, 5).unwrap();
        assert_eq!(ledger.may_accept_health(IPC_SERVICE_SESSIOND, 101, 6), Err(ESTALE));
        assert_eq!(ledger.may_accept_health(IPC_SERVICE_SESSIOND, 102, 6), Ok(()));
        assert_eq!(ledger.inspect(IPC_SERVICE_SESSIOND).unwrap().0, Phase::Trial);
    }

    #[test]
    fn failed_trial_is_rollback_required_not_success() {
        let mut ledger = UpdateBook::new();
        ledger.stage(approval(2), 10, 1).unwrap();
        ledger.arm(approval(2), 2).unwrap();
        ledger.published(IPC_SERVICE_SESSIOND, 11, 3).unwrap();
        ledger.on_exit(11);
        assert_eq!(ledger.inspect(IPC_SERVICE_SESSIOND).unwrap().0, Phase::RollbackRequired);
        assert_eq!(ledger.seal(approval(2)), Err(ESTALE));
    }
}
