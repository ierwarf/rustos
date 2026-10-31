//! Generated-IDL slow client-Job session admission and settlement.
//! The IDL only validates canonical bytes; kernel-stamped caller identity
//! and rootd sessiond principal policy still authorize every operation.
use crate::generated_idl::CLIENT_JOB_SESSION_ACCOUNTING;
use crate::idl::{Builder, IdlError, Message};
use alloc::vec::Vec;

pub const CHARGE: u16 = 1;
pub const RELEASE: u16 = 2;
const REQUEST: u16 = 1;
const ACK: u16 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionControl {
    pub token: u64,
    pub client_pid: u64,
    pub operation: u16,
}

impl SessionControl {
    pub fn new(token: u64, client_pid: u64, operation: u16) -> Result<Self, IdlError> {
        if token == 0
            || (operation != CHARGE && operation != RELEASE)
            || (operation == CHARGE && client_pid == 0)
            || (operation == RELEASE && client_pid != 0)
        {
            return Err(IdlError::Malformed);
        }
        Ok(Self { token, client_pid, operation })
    }
}

fn encode(value: SessionControl, job: u64, phase: u16) -> Result<Vec<u8>, IdlError> {
    SessionControl::new(value.token, value.client_pid, value.operation)?;
    if phase != REQUEST && phase != ACK || (phase == REQUEST && job != 0) {
        return Err(IdlError::Malformed);
    }
    let mut builder = Builder::new(CLIENT_JOB_SESSION_ACCOUNTING)?;
    builder.field(1, &value.token.to_le_bytes())?;
    builder.field(2, &value.client_pid.to_le_bytes())?;
    builder.field(3, &value.operation.to_le_bytes())?;
    builder.field(4, &job.to_le_bytes())?;
    builder.field(5, &phase.to_le_bytes())?;
    builder.finish(value.token)
}

fn decode(bytes: &[u8], expected_phase: u16) -> Result<(SessionControl, u64), IdlError> {
    let msg = Message::decode(CLIENT_JOB_SESSION_ACCOUNTING, bytes)?;
    let token = msg.field(1).ok_or(IdlError::Missing)?.as_u64()?;
    let pid = msg.field(2).ok_or(IdlError::Missing)?.as_u64()?;
    let operation = msg.field(3).ok_or(IdlError::Missing)?.as_u16()?;
    let job = msg.field(4).ok_or(IdlError::Missing)?.as_u64()?;
    let phase = msg.field(5).ok_or(IdlError::Missing)?.as_u16()?;
    let request = SessionControl::new(token, pid, operation)?;
    if msg.header().request_id != token || phase != expected_phase
        || (phase == REQUEST && job != 0)
        || (phase == ACK && operation == CHARGE && job == 0)
    {
        return Err(IdlError::Malformed);
    }
    Ok((request, job))
}

pub fn encode_request(request: SessionControl) -> Result<Vec<u8>, IdlError> {
    encode(request, 0, REQUEST)
}

pub fn decode_request(wire: &[u8]) -> Result<SessionControl, IdlError> {
    decode(wire, REQUEST).map(|(request, _)| request)
}

pub fn encode_ack(request: SessionControl, job: u64) -> Result<Vec<u8>, IdlError> {
    encode(request, job, ACK)
}

pub fn decode_ack(wire: &[u8], request: SessionControl) -> Result<u64, IdlError> {
    let (actual, job) = decode(wire, ACK)?;
    if actual != request {
        return Err(IdlError::Malformed);
    }
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_and_cancel_have_exact_nonreusable_idl_challenges() {
        let charge = SessionControl::new(9, 104, CHARGE).unwrap();
        let query = encode_request(charge).unwrap();
        assert_eq!(decode_request(&query), Ok(charge));
        let ack = encode_ack(charge, 1337).unwrap();
        assert_eq!(decode_ack(&ack, charge), Ok(1337));
        assert!(decode_ack(&ack, SessionControl { token: 10, ..charge }).is_err());
        assert!(decode_ack(&ack, SessionControl { client_pid: 105, ..charge }).is_err());
        assert!(decode_request(&ack).is_err());
        let cancel = SessionControl::new(10, 0, RELEASE).unwrap();
        assert_eq!(decode_ack(&encode_ack(cancel, 0).unwrap(), cancel), Ok(0));
        assert!(encode_ack(charge, 0).is_err());
        assert!(SessionControl::new(0, 2, CHARGE).is_err());
        assert!(SessionControl::new(1, 2, RELEASE).is_err());
    }
}
