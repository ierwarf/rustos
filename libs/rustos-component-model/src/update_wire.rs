//! Canonical generated-IDL update approval. The field "digest" is a claimed
//! candidate identity, **not** an image signature or execution permission.
//! Only trusted loaderd may attest an approved candidate, and the supervisor
//! must still stage/activate through real loaderd and Job authorities.
use crate::generated_idl::UPDATE_APPROVAL;
use crate::idl::{Builder, IdlError, Message};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Approval {
    pub service: u64,
    pub epoch: u64,
    pub digest: [u8; 32],
    pub trial_deadline_ns: u64,
}

impl Approval {
    pub fn validate(self) -> Result<Self, IdlError> {
        if self.service == 0 || self.epoch == 0
            || self.digest.iter().all(|byte| *byte == 0)
            || self.trial_deadline_ns == 0
        {
            return Err(IdlError::Malformed);
        }
        Ok(self)
    }
}

pub fn encode(approval: Approval) -> Result<Vec<u8>, IdlError> {
    approval.validate()?;
    let mut builder = Builder::new(UPDATE_APPROVAL)?;
    builder.field(1, &approval.service.to_le_bytes())?;
    builder.field(2, &approval.epoch.to_le_bytes())?;
    builder.field(3, &approval.digest)?;
    builder.field(4, &approval.trial_deadline_ns.to_le_bytes())?;
    builder.finish(approval.epoch)
}

pub fn decode(bytes: &[u8]) -> Result<Approval, IdlError> {
    let msg = Message::decode(UPDATE_APPROVAL, bytes)?;
    let service = msg.field(1).ok_or(IdlError::Missing)?.as_u64()?;
    let epoch = msg.field(2).ok_or(IdlError::Missing)?.as_u64()?;
    let data = msg.field(3).ok_or(IdlError::Missing)?.bytes;
    let digest: [u8; 32] = data.try_into().map_err(|_| IdlError::Malformed)?;
    let trial_deadline_ns = msg.field(4).ok_or(IdlError::Missing)?.as_u64()?;
    if msg.header().request_id != epoch {
        return Err(IdlError::Malformed);
    }
    Approval { service, epoch, digest, trial_deadline_ns }.validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_requires_canonical_exact_digest_epoch_and_deadline() {
        let a = Approval {
            service: 21, epoch: 5,
            digest: [1; 32], trial_deadline_ns: 100_000,
        };
        assert_eq!(decode(&encode(a).unwrap()), Ok(a));
        assert!(encode(Approval { digest: [0; 32], ..a }).is_err());
        assert!(encode(Approval { epoch: 0, ..a }).is_err());
        let mut bad = encode(a).unwrap();
        bad.push(0);
        assert!(decode(&bad).is_err());
    }
}
