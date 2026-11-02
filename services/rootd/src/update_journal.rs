//! Journal of release-transaction transitions, replay-validated by the
//! component model's `DurableJournal` state machine.
//!
//! The backing device here is rootd RAM. It makes every `UpdateBook`
//! transition pass the same canonical sequence/order/owner/readback checks a
//! persistent device will, and refuses a second concurrent release
//! transaction (the journal cursor has one active transaction). It is NOT
//! power-loss durable; a storaged-backed `JournalDevice` replaces `MemoryDevice`
//! when a writable persistent volume exists.
use alloc::vec::Vec;
use rustos_component_model::durable_recovery::{Kind, Record, RECORD_BYTES};
use rustos_component_model::storage_replay::{DurableJournal, IoError, JournalDevice, JournalOwner};

/// Bounded so a stuck transaction cannot grow rootd's heap without limit.
const MAX_RECORDS: u64 = 4096;

struct MemoryDevice(Vec<[u8; RECORD_BYTES]>);

impl JournalDevice for MemoryDevice {
    fn read(&mut self, index: u64, out: &mut [u8; RECORD_BYTES]) -> Result<bool, IoError> {
        match self.0.get(index as usize) {
            Some(record) => {
                *out = *record;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    fn append(&mut self, index: u64, bytes: &[u8; RECORD_BYTES]) -> Result<(), IoError> {
        if index as usize != self.0.len() {
            return Err(IoError::Conflict);
        }
        if self.0.len() as u64 >= MAX_RECORDS {
            return Err(IoError::Full);
        }
        self.0.try_reserve(1).map_err(|_| IoError::Full)?;
        self.0.push(*bytes);
        Ok(())
    }
    fn sync(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

pub(crate) struct UpdateJournal {
    device: MemoryDevice,
    journal: DurableJournal,
    owner: JournalOwner,
    transaction: u64,
}

impl UpdateJournal {
    pub(crate) fn new(rootd_pid: u64, lease_generation: u64, boot_epoch: u64) -> Option<Self> {
        let owner = JournalOwner { rootd_pid, lease_generation, boot_epoch };
        if !owner.valid() {
            return None;
        }
        let mut device = MemoryDevice(Vec::new());
        let mut journal = DurableJournal::new(owner);
        journal.load(&mut device, owner, MAX_RECORDS).ok()?;
        Some(Self { device, journal, owner, transaction: 0 })
    }

    /// Open a new release transaction. Refused while another is in flight.
    pub(crate) fn begin(&mut self, service: u64, epoch: u64, digest: [u8; 32]) -> Result<(), i32> {
        let cursor = self.journal.cursor();
        let idle = cursor.active_transaction == 0
            || matches!(cursor.last_kind, Some(Kind::Commit) | Some(Kind::RestoreComplete));
        if !idle {
            return Err(16);
        }
        self.transaction = self.transaction.max(cursor.active_transaction).checked_add(1).ok_or(75)?;
        self.append(service, epoch, Kind::Begin, digest)
    }

    /// Append the next transition of the open transaction.
    pub(crate) fn record(
        &mut self, service: u64, epoch: u64, kind: Kind, digest: [u8; 32],
    ) -> Result<(), i32> {
        let cursor = self.journal.cursor();
        if cursor.active_service != service || cursor.generation != epoch {
            return Err(116);
        }
        self.append(service, epoch, kind, digest)
    }

    fn append(&mut self, service: u64, epoch: u64, kind: Kind, digest: [u8; 32]) -> Result<(), i32> {
        let cursor = self.journal.cursor();
        let record = Record {
            sequence: cursor.last_sequence + 1,
            transaction: if kind == Kind::Begin { self.transaction } else { cursor.active_transaction },
            service,
            kind,
            generation: epoch,
            payload_hash: digest,
        };
        self.journal
            .append_verified(&mut self.device, self.owner, record)
            .map(|_| ())
            .map_err(|error| match error {
                IoError::Sequence | IoError::Conflict => 116,
                IoError::Full => 28,
                _ => 5,
            })
    }
}

#[cfg(test)]
impl UpdateJournal {
    // Observation helpers for tests; production never reads these.
    pub(crate) fn failed(&self) -> bool {
        self.journal.failed()
    }
    /// Whether `kind` would be a legal next record, without writing it.
    pub(crate) fn admits(&self, kind: Kind) -> bool {
        let cursor = self.journal.cursor();
        let probe = Record {
            sequence: cursor.last_sequence + 1,
            transaction: cursor.active_transaction.max(1),
            service: cursor.active_service.max(1),
            kind,
            generation: cursor.generation.max(1),
            payload_hash: [1; 32],
        };
        cursor.admit(probe).is_ok()
    }
    pub(crate) fn records(&self) -> usize {
        self.device.0.len()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> UpdateJournal {
        UpdateJournal::new(1, 2, 3).unwrap()
    }

    #[test]
    fn a_full_release_sequence_is_accepted_in_order_and_commit_frees_the_slot() {
        let mut j = journal();
        j.begin(11, 5, [1; 32]).unwrap();
        for kind in [
            Kind::OldImagePinned, Kind::CandidateVerified, Kind::Quiesced,
            Kind::Activated, Kind::HealthAccepted, Kind::Commit,
        ] {
            j.record(11, 5, kind, [1; 32]).unwrap();
        }
        assert_eq!(j.records(), 7);
        // Commit ends the transaction; another service may now begin.
        j.begin(3, 6, [2; 32]).unwrap();
    }

    #[test]
    fn out_of_order_and_foreign_transitions_are_refused_without_writing() {
        let mut j = journal();
        j.begin(11, 5, [1; 32]).unwrap();
        assert_eq!(j.record(11, 5, Kind::Activated, [1; 32]), Err(116));
        assert_eq!(j.record(3, 5, Kind::OldImagePinned, [1; 32]), Err(116));
        assert_eq!(j.record(11, 6, Kind::OldImagePinned, [1; 32]), Err(116));
        assert_eq!(j.records(), 1);
        assert!(j.admits(Kind::OldImagePinned));
        assert!(!j.admits(Kind::Commit));
    }

    #[test]
    fn a_second_concurrent_release_transaction_is_refused() {
        let mut j = journal();
        j.begin(11, 5, [1; 32]).unwrap();
        assert_eq!(j.begin(3, 6, [2; 32]), Err(16));
    }

    #[test]
    fn rollback_then_restore_completes_and_cannot_commit() {
        let mut j = journal();
        j.begin(11, 5, [1; 32]).unwrap();
        j.record(11, 5, Kind::OldImagePinned, [1; 32]).unwrap();
        j.record(11, 5, Kind::RollbackRequired, [1; 32]).unwrap();
        assert!(!j.admits(Kind::Commit));
        j.record(11, 5, Kind::RestoreBegin, [1; 32]).unwrap();
        j.record(11, 5, Kind::RestoreComplete, [1; 32]).unwrap();
        j.begin(11, 6, [3; 32]).unwrap();
    }

    #[test]
    fn invalid_owner_cannot_open_a_journal() {
        assert!(UpdateJournal::new(0, 2, 3).is_none());
    }
}
