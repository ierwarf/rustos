//! Ring3-only restart/rollback intent state machine.
//! This does not attest signatures, fsync data or restore images. The caller
//! must supply independently verified evidence before each transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageIdentity {
    pub generation: u64,
    pub digest: [u8; 32],
}
impl ImageIdentity {
    pub fn valid(self) -> bool {
        self.generation != 0 && self.digest.iter().any(|b| *b != 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Intent {
    pub transaction: u64,
    pub service: u64,
    pub previous: ImageIdentity,
    pub candidate: ImageIdentity,
    pub previous_verified: bool,
    pub candidate_verified: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Idle, Prepared, Journaled, Quiesced, Activated, Healthy,
    Committed, RollbackRequired, Restoring, Restored,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid, Conflict, Order, MissingDurability, WrongWitness,
    RollbackRequired, MissingImageCustody,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Witness {
    pub service: u64,
    pub transaction: u64,
    pub candidate_generation: u64,
    pub pid: u64,
    pub lease_generation: u64,
    pub nonce: u64,
}
pub struct RestartTransaction {
    intent: Option<Intent>,
    phase: Phase,
    witness: Option<Witness>,
}
impl RestartTransaction {
    pub const fn new() -> Self {
        Self { intent: None, phase: Phase::Idle, witness: None }
    }
    pub fn phase(&self) -> Phase { self.phase }
    pub fn intent(&self) -> Option<Intent> { self.intent }
    pub fn prepare(&mut self, intent: Intent) -> Result<(), Error> {
        if self.phase != Phase::Idle && self.phase != Phase::Restored
            && self.phase != Phase::Committed { return Err(Error::Conflict); }
        if intent.transaction == 0 || intent.service == 0
            || !intent.previous.valid() || !intent.candidate.valid()
            || intent.previous == intent.candidate
            || !intent.previous_verified || !intent.candidate_verified {
            return Err(Error::Invalid);
        }
        self.intent = Some(intent);
        self.witness = None;
        self.phase = Phase::Prepared;
        Ok(())
    }
    /// Called only after durable intent + previous image custody are
    /// independently confirmed by the storage/image authority.
    pub fn journal_durable(&mut self, transaction: u64, durable: bool)
        -> Result<(), Error> {
        self.require(transaction, Phase::Prepared)?;
        if !durable { return Err(Error::MissingDurability); }
        self.phase = Phase::Journaled;
        Ok(())
    }
    pub fn quiesced(&mut self, transaction: u64) -> Result<(), Error> {
        self.require(transaction, Phase::Journaled)?;
        self.phase = Phase::Quiesced;
        Ok(())
    }
    pub fn activated(&mut self, transaction: u64) -> Result<(), Error> {
        self.require(transaction, Phase::Quiesced)?;
        self.phase = Phase::Activated;
        Ok(())
    }
    pub fn healthy(&mut self, witness: Witness) -> Result<(), Error> {
        let intent = self.intent.ok_or(Error::Order)?;
        self.require(intent.transaction, Phase::Activated)?;
        if witness.service != intent.service || witness.transaction != intent.transaction
            || witness.candidate_generation != intent.candidate.generation
            || witness.pid == 0 || witness.lease_generation == 0 || witness.nonce == 0 {
            return Err(Error::WrongWitness);
        }
        self.witness = Some(witness);
        self.phase = Phase::Healthy;
        Ok(())
    }
    pub fn commit(&mut self, transaction: u64, durable: bool)
        -> Result<(), Error> {
        self.require(transaction, Phase::Healthy)?;
        if !durable { return Err(Error::MissingDurability); }
        self.phase = Phase::Committed;
        Ok(())
    }
    pub fn require_rollback(&mut self, transaction: u64) -> Result<(), Error> {
        if self.intent.map(|i| i.transaction) != Some(transaction)
            || matches!(self.phase, Phase::Idle | Phase::Committed | Phase::Restored) {
            return Err(Error::Order);
        }
        self.witness = None;
        self.phase = Phase::RollbackRequired;
        Ok(())
    }
    pub fn start_restore(&mut self, transaction: u64, image_in_custody: bool)
        -> Result<(), Error> {
        self.require(transaction, Phase::RollbackRequired)?;
        if !image_in_custody { return Err(Error::MissingImageCustody); }
        self.phase = Phase::Restoring;
        Ok(())
    }
    pub fn finish_restore(&mut self, transaction: u64, image_verified: bool,
                          durable: bool) -> Result<(), Error> {
        self.require(transaction, Phase::Restoring)?;
        if !image_verified { return Err(Error::MissingImageCustody); }
        if !durable { return Err(Error::MissingDurability); }
        self.phase = Phase::Restored;
        Ok(())
    }
    fn require(&self, transaction: u64, expected: Phase) -> Result<(), Error> {
        if self.intent.map(|i| i.transaction) != Some(transaction) ||
           self.phase != expected { Err(Error::Order) } else { Ok(()) }
    }
}
impl Default for RestartTransaction {
    fn default() -> Self { Self::new() }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rollback_is_not_restoration() {
        let old = ImageIdentity { generation: 1, digest: [1; 32] };
        let new = ImageIdentity { generation: 2, digest: [2; 32] };
        let mut tx = RestartTransaction::new();
        tx.prepare(Intent { transaction: 3, service: 4, previous: old,
            candidate: new, previous_verified: true, candidate_verified: true }).unwrap();
        assert_eq!(tx.journal_durable(3, false), Err(Error::MissingDurability));
        tx.journal_durable(3, true).unwrap();
        tx.require_rollback(3).unwrap();
        assert_eq!(tx.start_restore(3, false), Err(Error::MissingImageCustody));
        assert_eq!(tx.phase(), Phase::RollbackRequired);
    }
}
