//! Generated IDL supervisor health report/ack protocol.
//! Kernel-stamped sender and exact sealed service lease authorize the IPC;
//! canonical bytes alone never confer component authority.
use crate::generated_idl::SUPERVISOR_HEALTH;
use crate::idl::{Builder, IdlError, Message};
use alloc::vec::Vec;

pub const PULSE: u16 = 1;
pub const ACCEPT: u16 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pulse {
    pub service: u64,
    pub incarnation_pid: u64,
    pub sequence: u64,
}

fn encode(pulse: Pulse, phase: u16) -> Result<Vec<u8>, IdlError> {
    if pulse.service == 0 || pulse.incarnation_pid == 0 || pulse.sequence == 0
        || !(phase == PULSE || phase == ACCEPT)
    {
        return Err(IdlError::Malformed);
    }
    let mut builder = Builder::new(SUPERVISOR_HEALTH)?;
    builder.field(1, &pulse.service.to_le_bytes())?;
    builder.field(2, &pulse.incarnation_pid.to_le_bytes())?;
    builder.field(3, &pulse.sequence.to_le_bytes())?;
    builder.field(4, &phase.to_le_bytes())?;
    builder.finish(pulse.sequence)
}

fn decode(bytes: &[u8], phase: u16) -> Result<Pulse, IdlError> {
    let message = Message::decode(SUPERVISOR_HEALTH, bytes)?;
    let service = message.field(1).ok_or(IdlError::Missing)?.as_u64()?;
    let incarnation_pid = message.field(2).ok_or(IdlError::Missing)?.as_u64()?;
    let sequence = message.field(3).ok_or(IdlError::Missing)?.as_u64()?;
    let actual = message.field(4).ok_or(IdlError::Missing)?.as_u16()?;
    if service == 0 || incarnation_pid == 0 || sequence == 0
        || actual != phase || message.header().request_id != sequence
    {
        return Err(IdlError::Malformed);
    }
    Ok(Pulse { service, incarnation_pid, sequence })
}

pub fn encode_pulse(pulse: Pulse) -> Result<Vec<u8>, IdlError> {
    encode(pulse, PULSE)
}

pub fn decode_pulse(bytes: &[u8]) -> Result<Pulse, IdlError> {
    decode(bytes, PULSE)
}

pub fn encode_accept(pulse: Pulse) -> Result<Vec<u8>, IdlError> {
    encode(pulse, ACCEPT)
}

pub fn decode_accept(bytes: &[u8], expected: Pulse) -> Result<(), IdlError> {
    if decode(bytes, ACCEPT)? != expected {
        return Err(IdlError::Malformed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_pulse_is_challenge_and_incarnation_bound() {
        let ticket = Pulse { service: 11, incarnation_pid: 41, sequence: 7 };
        let req = encode_pulse(ticket).unwrap();
        assert_eq!(decode_pulse(&req), Ok(ticket));
        let ack = encode_accept(ticket).unwrap();
        assert_eq!(decode_accept(&ack, ticket), Ok(()));
        assert!(decode_accept(&ack, Pulse { sequence: 8, ..ticket }).is_err());
        assert!(decode_accept(&ack, Pulse { incarnation_pid: 42, ..ticket }).is_err());
        assert!(decode_pulse(&ack).is_err());
    }
}
