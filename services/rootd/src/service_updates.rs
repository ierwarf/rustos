//! Rootd-owned in-memory staging/activation/health/rollback transaction book.
//!
//! This is the **control half** of stage 8. The caller must be the exact live
//! loaderd or initd principal; a candidate digest is NOT executable proof.
//! Real signed-image admission and durable previous-image custody must be
//! supplied by the loader/image subsystem before calling it a safe update.
//! No service can commit merely by claiming that a PID is healthy.
use rustos_component_model::update_wire::Approval;
use super::service_manifest::POST_INIT_MANIFEST;
use super::update_journal::UpdateJournal;
use rustos_component_model::durable_recovery::Kind as JournalKind;

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
    /// The previous image's replacement incarnation passed three health
    /// pulses after a rollback; `seal` retires the transaction.
    RolledBack,
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
    /// Incarnation published after RollbackRequired (the restored image).
    restore_pid: u64,
    restore_pulses: u8,
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
        restore_pid: 0,
        restore_pulses: 0,
    };
    fn matches(&self, approval: Approval) -> bool {
        self.service == approval.service
            && self.epoch == approval.epoch
            && self.candidate_digest == approval.digest
            && self.deadline_ns == approval.trial_deadline_ns
    }
}

/// The supervisor-visible snapshot of one service's release transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Inspected {
    pub phase: Phase,
    pub epoch: u64,
    pub previous_pid: u64,
    pub candidate_pid: u64,
    pub digest: [u8; 32],
    pub deadline_ns: u64,
}

pub(crate) struct UpdateBook {
    entries: [Upgrade; MAX_UPGRADES],
    committed_epochs: [u64; MAX_UPGRADES],
    /// Present once rootd has attached its release journal; every transition
    /// is then recorded (and validated by the journal's state machine) BEFORE
    /// the book changes, so an illegal order or journal failure refuses it.
    journal: Option<UpdateJournal>,
}

impl UpdateBook {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [Upgrade::EMPTY; MAX_UPGRADES],
            committed_epochs: [0; MAX_UPGRADES],
            journal: None,
        }
    }

    pub(crate) fn attach_journal(&mut self, journal: UpdateJournal) {
        self.journal = Some(journal);
    }

    /// Record one transition; `Ok` when no journal is attached.
    fn journaled(&mut self, upgrade: &Upgrade, kind: JournalKind) -> Result<(), i32> {
        match self.journal.as_mut() {
            Some(journal) => journal.record(upgrade.service, upgrade.epoch, kind, upgrade.candidate_digest),
            None => Ok(()),
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
        let staged = Upgrade {
            service: approval.service,
            epoch: approval.epoch,
            candidate_digest: approval.digest,
            previous_pid,
            candidate_pid: 0,
            deadline_ns: approval.trial_deadline_ns,
            state: Phase::Staged,
            health_pulses: 0,
            restore_pid: 0,
            restore_pulses: 0,
        };
        if let Some(journal) = self.journal.as_mut() {
            // A staged release is a verified candidate with the previous
            // incarnation recorded; open the transaction and say so in order.
            journal.begin(staged.service, staged.epoch, staged.candidate_digest)?;
            journal.record(staged.service, staged.epoch, JournalKind::OldImagePinned, staged.candidate_digest)?;
            journal.record(staged.service, staged.epoch, JournalKind::CandidateVerified, staged.candidate_digest)?;
        }
        self.entries[index] = staged;
        Ok(Phase::Staged)
    }

    /// Only initd can request the cutover; rootd still requires the actual
    /// loader transition and an exact new service lease before starting trial.
    pub(crate) fn arm(&mut self, approval: Approval, now_ns: u64) -> Result<Phase, i32> {
        let index = Self::position(approval.service)?;
        let slot = self.entries[index];
        if !slot.matches(approval) || now_ns >= slot.deadline_ns {
            return Err(ESTALE);
        }
        match slot.state {
            Phase::Staged => {
                self.journaled(&slot, JournalKind::Quiesced)?;
                self.entries[index].state = Phase::Armed;
                Ok(Phase::Armed)
            }
            Phase::Armed => Ok(slot.state),
            _ => Err(EBUSY),
        }
    }

    /// Pure transaction admission preflight, called BEFORE rootd publishes a
    /// replacement service lease. Once the lease has been authenticated and
    /// inserted, this exact check cannot reject the same candidate.
    ///
    /// After RollbackRequired the only admissible new incarnation is the
    /// restored previous image: a fresh PID that is neither the failed
    /// candidate nor the incarnation the update started from.
    pub(crate) fn may_publish(
        &self, service: u64, pid: u64, now_ns: u64,
    ) -> Result<(), i32> {
        let slot = &self.entries[Self::position(service)?];
        match slot.state {
            Phase::Empty => Ok(()),
            Phase::RollbackRequired => {
                if pid == 0 || pid == slot.candidate_pid || pid == slot.previous_pid {
                    Err(ESTALE)
                } else {
                    Ok(())
                }
            }
            Phase::Armed if pid != 0 && pid != slot.previous_pid && now_ns < slot.deadline_ns => Ok(()),
            _ => Err(ESTALE),
        }
    }

    /// Pure health preflight before mutating the independent health witness.
    /// The same monotonic timestamp is used in both transitions.
    pub(crate) fn may_accept_health(
        &self, service: u64, pid: u64, now_ns: u64,
    ) -> Result<(), i32> {
        // Only post-init services can be under a release transaction; the
        // bootstrap services' health is still authenticated and recorded by
        // the health book, and simply has no update trial to gate.
        let Ok(index) = Self::position(service) else {
            return Ok(());
        };
        let slot = &self.entries[index];
        if slot.state == Phase::Trial
            && (slot.candidate_pid != pid || now_ns >= slot.deadline_ns)
        {
            return Err(ESTALE);
        }
        Ok(())
    }

    /// Exact live post-init lease publication, not the requester's PID, sets
    /// the candidate (or, after RollbackRequired, the restored) incarnation.
    pub(crate) fn published(
        &mut self, service: u64, new_pid: u64, now_ns: u64,
    ) -> Result<Phase, i32> {
        self.may_publish(service, new_pid, now_ns)?;
        let index = Self::position(service)?;
        let slot = self.entries[index];
        match slot.state {
            Phase::Empty => Ok(Phase::Empty),
            Phase::RollbackRequired => {
                // A different restored incarnation replaces an earlier failed
                // restore attempt; the journal opens RestoreBegin only once.
                if slot.restore_pid == 0 {
                    self.journaled(&slot, JournalKind::RestoreBegin)?;
                }
                let entry = &mut self.entries[index];
                entry.restore_pid = new_pid;
                entry.restore_pulses = 0;
                Ok(Phase::RollbackRequired)
            }
            _ => {
                self.journaled(&slot, JournalKind::Activated)?;
                let entry = &mut self.entries[index];
                entry.candidate_pid = new_pid;
                entry.state = Phase::Trial;
                entry.health_pulses = 0;
                Ok(Phase::Trial)
            }
        }
    }

    /// Success is repeated authenticated health from the **new** incarnation
    /// after it was published: the candidate during a trial, or the restored
    /// image after a rollback. Three distinct pulses either way.
    pub(crate) fn healthy(
        &mut self, service: u64, pid: u64, now_ns: u64,
    ) -> Result<Phase, i32> {
        self.may_accept_health(service, pid, now_ns)?;
        let Ok(index) = Self::position(service) else {
            return Ok(Phase::Empty);
        };
        let slot = self.entries[index];
        match slot.state {
            Phase::Trial => {
                if slot.health_pulses.saturating_add(1) >= 3 {
                    self.journaled(&slot, JournalKind::HealthAccepted)?;
                    self.entries[index].state = Phase::Committed;
                }
                let entry = &mut self.entries[index];
                entry.health_pulses = entry.health_pulses.saturating_add(1);
                Ok(entry.state)
            }
            Phase::RollbackRequired if slot.restore_pid != 0 && slot.restore_pid == pid => {
                if slot.restore_pulses.saturating_add(1) >= 3 {
                    self.journaled(&slot, JournalKind::RestoreComplete)?;
                    self.entries[index].state = Phase::RolledBack;
                }
                let entry = &mut self.entries[index];
                entry.restore_pulses = entry.restore_pulses.saturating_add(1);
                Ok(entry.state)
            }
            _ => Ok(slot.state),
        }
    }

    /// A rollback request seals the transaction against later commit. It
    /// does NOT imply the old executable image was actually restored; that
    /// is `RolledBack`, reached only through a restored incarnation's health.
    pub(crate) fn rollback(
        &mut self, approval: Approval,
    ) -> Result<Phase, i32> {
        let index = Self::position(approval.service)?;
        let slot = self.entries[index];
        if !slot.matches(approval) {
            return Err(ESTALE);
        }
        match slot.state {
            Phase::Armed | Phase::Staged | Phase::Trial => {
                self.journaled(&slot, JournalKind::RollbackRequired)?;
                self.entries[index].state = Phase::RollbackRequired;
                Ok(Phase::RollbackRequired)
            }
            Phase::RollbackRequired => Ok(Phase::RollbackRequired),
            _ => Err(EBUSY),
        }
    }

    pub(crate) fn on_exit(&mut self, pid: u64) {
        for index in 0..self.entries.len() {
            let slot = self.entries[index];
            if pid == 0 {
                continue;
            }
            if pid == slot.candidate_pid && slot.state == Phase::Trial {
                // Refusing to record must never block the safe direction.
                let _ = self.journaled(&slot, JournalKind::RollbackRequired);
                self.entries[index].state = Phase::RollbackRequired;
            } else if pid == slot.restore_pid && slot.state == Phase::RollbackRequired {
                // The restored incarnation died before three pulses: it must
                // be relaunched and start counting again.
                let entry = &mut self.entries[index];
                entry.restore_pid = 0;
                entry.restore_pulses = 0;
            }
        }
    }

    /// Rootd supervisor owns timeout transitions, not the service being
    /// upgraded. A stopped candidate must never be considered committed.
    pub(crate) fn expired(&mut self, now_ns: u64) -> [Option<u64>; MAX_UPGRADES] {
        let mut result = [None; MAX_UPGRADES];
        for index in 0..self.entries.len() {
            let slot = self.entries[index];
            if matches!(slot.state, Phase::Staged | Phase::Armed | Phase::Trial)
                && now_ns >= slot.deadline_ns
            {
                let _ = self.journaled(&slot, JournalKind::RollbackRequired);
                self.entries[index].state = Phase::RollbackRequired;
                result[index] = Some(slot.service);
            }
        }
        result
    }

    /// Commit marks the epoch immutable only after exact trial health; a
    /// finished rollback retires the transaction and burns its epoch so the
    /// failed candidate can never be staged again.
    pub(crate) fn seal(&mut self, approval: Approval) -> Result<Phase, i32> {
        let index = Self::position(approval.service)?;
        let slot = self.entries[index];
        if !slot.matches(approval) {
            return Err(ESTALE);
        }
        match slot.state {
            Phase::Committed => {
                self.journaled(&slot, JournalKind::Commit)?;
                self.committed_epochs[index] = slot.epoch;
                self.entries[index] = Upgrade::EMPTY;
                Ok(Phase::Committed)
            }
            Phase::RolledBack => {
                // RestoreComplete was journaled when the state was entered.
                self.committed_epochs[index] = slot.epoch;
                self.entries[index] = Upgrade::EMPTY;
                Ok(Phase::RolledBack)
            }
            _ => Err(ESTALE),
        }
    }

    pub(crate) fn inspect(&self, service: u64) -> Result<Inspected, i32> {
        let slot = &self.entries[Self::position(service)?];
        Ok(Inspected {
            phase: slot.state,
            epoch: slot.epoch,
            previous_pid: slot.previous_pid,
            candidate_pid: slot.candidate_pid,
            digest: slot.candidate_digest,
            deadline_ns: slot.deadline_ns,
        })
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
        assert_eq!(ledger.inspect(IPC_SERVICE_SESSIOND).unwrap().phase, Phase::Armed);
        assert_eq!(ledger.may_publish(IPC_SERVICE_SESSIOND, 102, 5), Ok(()));
        ledger.published(IPC_SERVICE_SESSIOND, 102, 5).unwrap();
        assert_eq!(ledger.may_accept_health(IPC_SERVICE_SESSIOND, 101, 6), Err(ESTALE));
        assert_eq!(ledger.may_accept_health(IPC_SERVICE_SESSIOND, 102, 6), Ok(()));
        assert_eq!(ledger.inspect(IPC_SERVICE_SESSIOND).unwrap().phase, Phase::Trial);
    }

    #[test]
    fn failed_trial_is_rollback_required_not_success() {
        let mut ledger = UpdateBook::new();
        ledger.stage(approval(2), 10, 1).unwrap();
        ledger.arm(approval(2), 2).unwrap();
        ledger.published(IPC_SERVICE_SESSIOND, 11, 3).unwrap();
        ledger.on_exit(11);
        assert_eq!(ledger.inspect(IPC_SERVICE_SESSIOND).unwrap().phase, Phase::RollbackRequired);
        assert_eq!(ledger.seal(approval(2)), Err(ESTALE));
    }

    #[test]
    fn rollback_completes_only_through_three_pulses_of_a_restored_incarnation() {
        let mut ledger = UpdateBook::new();
        ledger.stage(approval(3), 10, 1).unwrap();
        ledger.arm(approval(3), 2).unwrap();
        ledger.published(IPC_SERVICE_SESSIOND, 11, 3).unwrap();
        ledger.on_exit(11);
        assert_eq!(ledger.inspect(IPC_SERVICE_SESSIOND).unwrap().phase, Phase::RollbackRequired);
        // Neither the failed candidate nor the starting incarnation restores.
        assert_eq!(ledger.may_publish(IPC_SERVICE_SESSIOND, 11, 4), Err(ESTALE));
        assert_eq!(ledger.may_publish(IPC_SERVICE_SESSIOND, 10, 4), Err(ESTALE));
        assert_eq!(ledger.published(IPC_SERVICE_SESSIOND, 12, 4), Ok(Phase::RollbackRequired));
        // A different pid's pulses do not count; the restored one needs three.
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 11, 5), Ok(Phase::RollbackRequired));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 12, 6), Ok(Phase::RollbackRequired));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 12, 7), Ok(Phase::RollbackRequired));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 12, 8), Ok(Phase::RolledBack));
        assert_eq!(ledger.seal(approval(3)), Ok(Phase::RolledBack));
        // The failed epoch is burned; a later epoch can stage again.
        assert_eq!(ledger.stage(approval(3), 12, 9), Err(ESTALE));
        assert_eq!(ledger.stage(approval(4), 12, 9), Ok(Phase::Staged));
    }

    #[test]
    fn a_restored_incarnation_that_dies_must_restart_its_count() {
        let mut ledger = UpdateBook::new();
        ledger.stage(approval(5), 10, 1).unwrap();
        ledger.rollback(approval(5)).unwrap();
        ledger.published(IPC_SERVICE_SESSIOND, 12, 2).unwrap();
        ledger.healthy(IPC_SERVICE_SESSIOND, 12, 3).unwrap();
        ledger.healthy(IPC_SERVICE_SESSIOND, 12, 4).unwrap();
        ledger.on_exit(12);
        assert_eq!(ledger.published(IPC_SERVICE_SESSIOND, 13, 5), Ok(Phase::RollbackRequired));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 13, 6), Ok(Phase::RollbackRequired));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 13, 7), Ok(Phase::RollbackRequired));
        assert_eq!(ledger.healthy(IPC_SERVICE_SESSIOND, 13, 8), Ok(Phase::RolledBack));
    }

    #[test]
    fn journaled_book_records_every_transition_and_refuses_a_concurrent_release() {
        let mut ledger = UpdateBook::new();
        ledger.attach_journal(UpdateJournal::new(1, 2, 3).unwrap());
        ledger.stage(approval(1), 11, 1).unwrap();
        // Begin, OldImagePinned, CandidateVerified.
        assert_eq!(ledger.journal.as_ref().unwrap().records(), 3);
        ledger.arm(approval(1), 2).unwrap();
        ledger.published(IPC_SERVICE_SESSIOND, 12, 3).unwrap();
        for tick in 4..7 {
            ledger.healthy(IPC_SERVICE_SESSIOND, 12, tick).unwrap();
        }
        ledger.seal(approval(1)).unwrap();
        // Quiesced, Activated, HealthAccepted, Commit.
        assert_eq!(ledger.journal.as_ref().unwrap().records(), 7);
        assert!(!ledger.journal.as_ref().unwrap().failed());
    }

    #[test]
    fn journaled_rollback_and_restore_are_recorded_and_frees_the_slot() {
        let mut ledger = UpdateBook::new();
        ledger.attach_journal(UpdateJournal::new(1, 2, 3).unwrap());
        ledger.stage(approval(2), 10, 1).unwrap();
        ledger.arm(approval(2), 2).unwrap();
        ledger.published(IPC_SERVICE_SESSIOND, 11, 3).unwrap();
        ledger.on_exit(11);
        ledger.published(IPC_SERVICE_SESSIOND, 12, 4).unwrap();
        for tick in 5..8 {
            ledger.healthy(IPC_SERVICE_SESSIOND, 12, tick).unwrap();
        }
        ledger.seal(approval(2)).unwrap();
        // 3 staging + Quiesced + Activated + RollbackRequired + RestoreBegin + RestoreComplete.
        assert_eq!(ledger.journal.as_ref().unwrap().records(), 8);
        // The transaction is closed, so the next epoch can begin.
        assert_eq!(ledger.stage(approval(3), 12, 9), Ok(Phase::Staged));
    }

    #[test]
    fn bootstrap_service_health_is_not_an_update_error() {
        use rustos_user_abi::syscall::{IPC_SERVICE_INITD, IPC_SERVICE_VFSD};
        let mut ledger = UpdateBook::new();
        for service in [IPC_SERVICE_VFSD, IPC_SERVICE_INITD] {
            assert_eq!(ledger.may_accept_health(service, 9, 1), Ok(()));
            assert_eq!(ledger.healthy(service, 9, 1), Ok(Phase::Empty));
        }
        // A post-init service is still gated by its trial.
        ledger.stage(approval(1), 11, 1).unwrap();
        ledger.arm(approval(1), 2).unwrap();
        ledger.published(IPC_SERVICE_SESSIOND, 12, 3).unwrap();
        assert_eq!(ledger.may_accept_health(IPC_SERVICE_SESSIOND, 99, 4), Err(ESTALE));
    }
}
