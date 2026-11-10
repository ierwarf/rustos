//! Bounded replay records for authenticated runtime-control mutations.
use runtime_control::protocol::{
    MAX_TRANSACTION_LIFETIME_NS, RuntimeRequest, TRANSACTION_REPLAY_RETENTION_NS,
};

const MAX_TRANSACTIONS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransactionKey {
    pub(crate) peer_pid: i32,
    pub(crate) id_hi: u64,
    pub(crate) id_lo: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Queued,
    Admitted,
    Finished(Result<(), i32>),
    /// The worker or a downstream service stopped without a trustworthy result.
    Unknown,
}

struct Transaction {
    key: TransactionKey,
    request: RuntimeRequest,
    outcome: Outcome,
}

pub(crate) struct Transactions {
    started_ns: Option<u64>,
    records: Vec<Transaction>,
}

impl Default for Transactions {
    fn default() -> Self {
        Self {
            started_ns: runtime_control::monotonic_nanos().ok(),
            records: Vec::new(),
        }
    }
}

impl Transactions {
    pub(crate) fn admit(
        &mut self,
        peer_pid: i32,
        request: RuntimeRequest,
        now: u64,
        query: bool,
    ) -> Result<(TransactionKey, Outcome, bool), i32> {
        let key = TransactionKey {
            peer_pid,
            id_hi: request.transaction_id_hi,
            id_lo: request.transaction_id_lo,
        };
        // Every admitted id predates or equals admission. A later server
        // incarnation starts after that point and must reject it rather than
        // execute an old transaction whose volatile outcome has been lost.
        let started = self.started_ns.ok_or(libc::EIO)?;
        if key.id_lo == 0 || key.id_hi <= started || key.id_hi > now {
            return Err(libc::ESTALE);
        }
        let mut fingerprint = request;
        fingerprint.flags = 0;
        fingerprint.deadline_ns = 0;
        // The immutable creation half of the identity bounds BOTH admission
        // and replay. A caller cannot resurrect an evicted ID by supplying a
        // later deadline. Expired unknown metadata may then be reclaimed
        // without asserting that its side effects were rolled back.
        let admission_end = key
            .id_hi
            .checked_add(MAX_TRANSACTION_LIFETIME_NS)
            .ok_or(libc::EINVAL)?;
        let retention_end = admission_end
            .checked_add(TRANSACTION_REPLAY_RETENTION_NS)
            .ok_or(libc::EINVAL)?;
        self.records.retain(|record| {
            let end = record
                .key
                .id_hi
                .saturating_add(MAX_TRANSACTION_LIFETIME_NS)
                .saturating_add(TRANSACTION_REPLAY_RETENTION_NS);
            now < end
        });
        if now >= retention_end {
            return Err(libc::ESTALE);
        }
        if request.transaction_deadline_ns <= key.id_hi
            || request.transaction_deadline_ns > admission_end
        {
            return Err(libc::EINVAL);
        }
        if let Some(record) = self.records.iter().find(|record| record.key == key) {
            if record.request != fingerprint {
                return Err(libc::EINVAL);
            }
            return Ok((key, record.outcome, false));
        }
        if query {
            return Err(libc::ENOENT);
        }
        if request.transaction_deadline_ns <= now {
            return Err(libc::ETIMEDOUT);
        }
        if self.records.len() == MAX_TRANSACTIONS {
            return Err(libc::EBUSY);
        }
        self.records.push(Transaction {
            key,
            request: fingerprint,
            outcome: Outcome::Queued,
        });
        Ok((key, Outcome::Queued, true))
    }

    pub(crate) fn finish(&mut self, key: TransactionKey, outcome: Outcome) {
        if let Some(record) = self.records.iter_mut().find(|record| record.key == key) {
            record.outcome = outcome;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime_control::protocol::{OP_REQUEST_LAUNCH_PATH, REQUEST_FLAG_QUERY};

    fn request(id: u64) -> RuntimeRequest {
        RuntimeRequest {
            op: OP_REQUEST_LAUNCH_PATH,
            transaction_id_hi: 1,
            transaction_id_lo: id,
            transaction_deadline_ns: 100,
            ..RuntimeRequest::default()
        }
    }

    #[test]
    fn replay_returns_original_late_commit_without_a_second_admission() {
        let mut table = Transactions {
            started_ns: Some(0),
            records: Vec::new(),
        };
        let req = request(1);
        let (key, outcome, fresh) = table.admit(41, req, 1, false).unwrap();
        assert_eq!((outcome, fresh), (Outcome::Queued, true));
        table.finish(key, Outcome::Admitted);
        assert_eq!(table.admit(41, req, 2, false).unwrap().1, Outcome::Admitted);
        table.finish(key, Outcome::Finished(Ok(())));
        let mut query = req;
        query.flags = REQUEST_FLAG_QUERY;
        query.deadline_ns = 999;
        let (_, outcome, fresh) = table.admit(41, query, 101, true).unwrap();
        assert_eq!((outcome, fresh), (Outcome::Finished(Ok(())), false));
        assert_eq!(table.admit(41, req, 101, false).unwrap().1, outcome);
        assert_eq!(
            table.admit(
                41,
                req,
                1 + MAX_TRANSACTION_LIFETIME_NS + TRANSACTION_REPLAY_RETENTION_NS,
                false
            ),
            Err(libc::ESTALE)
        );
    }

    #[test]
    fn peer_and_complete_request_fingerprint_are_bound() {
        let mut table = Transactions {
            started_ns: Some(0),
            records: Vec::new(),
        };
        let req = request(1);
        table.admit(41, req, 1, false).unwrap();
        assert_eq!(table.admit(42, req, 1, true), Err(libc::ENOENT));
        let mut changed = req;
        changed.target_value = 1;
        assert_eq!(table.admit(41, changed, 1, true), Err(libc::EINVAL));
        changed = req;
        changed.transaction_deadline_ns += 1;
        assert_eq!(table.admit(41, changed, 1, false), Err(libc::EINVAL));
    }

    #[test]
    fn evicted_finished_identity_cannot_acquire_an_extended_deadline() {
        let mut table = Transactions {
            started_ns: Some(0),
            records: Vec::new(),
        };
        let req = request(1);
        let (key, _, _) = table.admit(41, req, 1, false).unwrap();
        table.finish(key, Outcome::Finished(Ok(())));
        let end = 1 + MAX_TRANSACTION_LIFETIME_NS + TRANSACTION_REPLAY_RETENTION_NS;
        let mut forged = req;
        forged.transaction_deadline_ns = end + 100;
        assert_eq!(table.admit(41, forged, end, false), Err(libc::ESTALE));
        assert!(table.records.is_empty());
        forged.transaction_deadline_ns = 2 + MAX_TRANSACTION_LIFETIME_NS;
        assert_eq!(table.admit(41, forged, 2, false), Err(libc::EINVAL));
    }

    #[test]
    fn restart_never_readmits_an_old_transaction() {
        let req = request(1);
        let mut first = Transactions {
            started_ns: Some(0),
            records: Vec::new(),
        };
        first.admit(41, req, 2, false).unwrap();
        let mut restarted = Transactions {
            started_ns: Some(2),
            records: Vec::new(),
        };
        assert_eq!(restarted.admit(41, req, 3, false), Err(libc::ESTALE));
        assert_eq!(restarted.admit(41, req, 3, true), Err(libc::ESTALE));
    }

    #[test]
    fn unknown_records_remain_bounded_and_expire_without_readmitting_old_ids() {
        let mut table = Transactions {
            started_ns: Some(0),
            records: Vec::new(),
        };
        for id in 1..=MAX_TRANSACTIONS as u64 {
            let (key, _, _) = table.admit(41, request(id), 1, false).unwrap();
            table.finish(key, Outcome::Unknown);
        }
        assert_eq!(table.admit(41, request(999), 2, false), Err(libc::EBUSY));
        let later = 1 + MAX_TRANSACTION_LIFETIME_NS + TRANSACTION_REPLAY_RETENTION_NS;
        let mut new_request = request(999);
        new_request.transaction_id_hi = later;
        new_request.transaction_deadline_ns = later + 100;
        assert!(table.admit(41, new_request, later, false).unwrap().2);
        assert_eq!(table.records.len(), 1);
        let mut forged_old = request(1);
        forged_old.transaction_deadline_ns = later + 100;
        assert_eq!(table.admit(41, forged_old, later, false), Err(libc::ESTALE));
    }
}
