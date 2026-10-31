//! Canonical slow-control lease reclamation with exact PID and challenge.
//! The IDL is a data format, not authorization. Rootd still requires the
//! kernel-stamped current initd identity and a matching live lease before
//! revoking any endpoint or terminating a process.
use crate::generated_idl::POST_INIT_LEASE_RECLAIM;
use crate::idl::{Builder, IdlError, Message};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reclaim {
    pub service: u64,
    pub pid: u64,
    pub challenge: u64,
}

const REQUEST: u16 = 1;
const ACK: u16 = 2;

fn encode(value: Reclaim, phase: u16) -> Result<Vec<u8>, IdlError> {
    if value.service == 0 || value.pid == 0 || value.challenge == 0 {
        return Err(IdlError::Malformed);
    }
    let mut builder = Builder::new(POST_INIT_LEASE_RECLAIM)?;
    builder.field(1, &value.service.to_le_bytes())?;
    builder.field(2, &value.pid.to_le_bytes())?;
    builder.field(3, &value.challenge.to_le_bytes())?;
    builder.field(4, &phase.to_le_bytes())?;
    builder.finish(value.challenge)
}

fn decode(frame: &[u8], phase: u16) -> Result<Reclaim, IdlError> {
    let msg = Message::decode(POST_INIT_LEASE_RECLAIM, frame)?;
    let service = msg.field(1).ok_or(IdlError::Missing)?.as_u64()?;
    let pid = msg.field(2).ok_or(IdlError::Missing)?.as_u64()?;
    let challenge = msg.field(3).ok_or(IdlError::Missing)?.as_u64()?;
    let found_phase = msg.field(4).ok_or(IdlError::Missing)?.as_u16()?;
    if found_phase != phase
        || service == 0 || pid == 0 || challenge == 0
        || msg.header().request_id != challenge
    {
        return Err(IdlError::Malformed);
    }
    Ok(Reclaim { service, pid, challenge })
}

pub fn encode_request(value: Reclaim) -> Result<Vec<u8>, IdlError> {
    encode(value, REQUEST)
}

pub fn decode_request(frame: &[u8]) -> Result<Reclaim, IdlError> {
    decode(frame, REQUEST)
}

pub fn encode_ack(value: Reclaim) -> Result<Vec<u8>, IdlError> {
    encode(value, ACK)
}

pub fn decode_ack(frame: &[u8], request: Reclaim) -> Result<(), IdlError> {
    if decode(frame, ACK)? != request {
        return Err(IdlError::Malformed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_reclaim_idl_binds_service_pid_and_nonce() {
        let ticket = Reclaim { service: 11, pid: 77, challenge: 19 };
        let wire = encode_request(ticket).unwrap();
        assert_eq!(decode_request(&wire), Ok(ticket));
        assert!(decode_ack(&wire, ticket).is_err());
        let reply = encode_ack(ticket).unwrap();
        assert_eq!(decode_ack(&reply, ticket), Ok(()));
        assert_eq!(decode_ack(&reply, Reclaim { pid: 78, ..ticket }), Err(IdlError::Malformed));
        assert_eq!(decode_ack(&reply, Reclaim { challenge: 20, ..ticket }), Err(IdlError::Malformed));
        assert_eq!(decode_ack(&reply, Reclaim { service: 12, ..ticket }), Err(IdlError::Malformed));
        assert!(decode_request(&reply).is_err());
    }

    #[test]
    fn malformed_reclaim_writes_nothing() {
        assert!(encode_request(Reclaim { service: 0, pid: 1, challenge: 1 }).is_err());
        assert!(encode_request(Reclaim { service: 1, pid: 0, challenge: 1 }).is_err());
        assert!(encode_request(Reclaim { service: 1, pid: 1, challenge: 0 }).is_err());
        let ticket = Reclaim { service: 11, pid: 77, challenge: 19 };
        let mut wire = encode_request(ticket).unwrap();
        wire.push(0);
        assert!(decode_request(&wire).is_err());
    }
}
