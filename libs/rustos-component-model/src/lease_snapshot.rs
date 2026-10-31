//! Canonical generated-IDL post-init service lease snapshot.
//!
//! Production initd and rootd use this codec on the slow supervisor IPC.
//! The message cannot authorize a lease: rootd separately authenticates the
//! kernel-stamped sender and exact live initd principal before replying.
//! The request-id and challenge match to reject a mismatched/stale reply.
use alloc::vec::Vec;
use crate::generated_idl::POST_INIT_LEASE_SNAPSHOT;
use crate::idl::{Builder, IdlError, Message};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Query {
    pub service: u64,
    pub challenge: u64,
}

fn encode_fields(service: u64, pid: u64, challenge: u64, state: u16)
    -> Result<Vec<u8>, IdlError>
{
    if service == 0 || challenge == 0 {
        return Err(IdlError::Malformed);
    }
    let mut builder = Builder::new(POST_INIT_LEASE_SNAPSHOT)?;
    builder.field(1, &service.to_le_bytes())?;
    builder.field(2, &pid.to_le_bytes())?;
    builder.field(3, &challenge.to_le_bytes())?;
    builder.field(4, &state.to_le_bytes())?;
    builder.finish(challenge)
}

fn decode_fields(frame: &[u8]) -> Result<(u64, u64, u64, u16), IdlError> {
    let message = Message::decode(POST_INIT_LEASE_SNAPSHOT, frame)?;
    let service = message.field(1).ok_or(IdlError::Missing)?.as_u64()?;
    let pid = message.field(2).ok_or(IdlError::Missing)?.as_u64()?;
    let challenge = message.field(3).ok_or(IdlError::Missing)?.as_u64()?;
    let state = message.field(4).ok_or(IdlError::Missing)?.as_u16()?;
    if service == 0 || challenge == 0 || message.header().request_id != challenge {
        return Err(IdlError::Malformed);
    }
    Ok((service, pid, challenge, state))
}

pub fn encode_query(service: u64, challenge: u64) -> Result<Vec<u8>, IdlError> {
    encode_fields(service, 0, challenge, 0)
}

pub fn decode_query(frame: &[u8]) -> Result<Query, IdlError> {
    let (service, pid, challenge, state) = decode_fields(frame)?;
    if pid != 0 || state != 0 {
        return Err(IdlError::Malformed);
    }
    Ok(Query { service, challenge })
}

pub fn encode_reply(query: Query, pid: u64, state: u16)
    -> Result<Vec<u8>, IdlError>
{
    encode_fields(query.service, pid, query.challenge, state)
}

pub fn decode_reply(
    frame: &[u8],
    expected_service: u64,
    expected_challenge: u64,
) -> Result<(u64, u16), IdlError> {
    let (service, pid, challenge, state) = decode_fields(frame)?;
    if service != expected_service || challenge != expected_challenge {
        return Err(IdlError::Malformed);
    }
    Ok((pid, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_lease_snapshot_roundtrips_exact_query_and_reply() {
        let request = encode_query(11, 27).unwrap();
        let query = decode_query(&request).unwrap();
        assert_eq!(query, Query { service: 11, challenge: 27 });
        let reply = encode_reply(query, 901, 1).unwrap();
        assert_eq!(decode_reply(&reply, 11, 27), Ok((901, 1)));
        assert_eq!(decode_reply(&reply, 11, 28), Err(IdlError::Malformed));
        assert_eq!(decode_reply(&reply, 12, 27), Err(IdlError::Malformed));
        assert!(decode_query(&reply).is_err());
    }

    #[test]
    fn malformed_or_replayed_queries_never_issue_a_lease() {
        assert_eq!(encode_query(0, 1), Err(IdlError::Malformed));
        assert_eq!(encode_query(10, 0), Err(IdlError::Malformed));
        let mut query = encode_query(10, 1).unwrap();
        query.push(0);
        assert!(decode_query(&query).is_err());
        let reply = encode_reply(Query { service: 10, challenge: 1 }, 0, 0).unwrap();
        assert_eq!(decode_reply(&reply, 10, 1), Ok((0, 0)));
    }

    #[test]
    fn noncanonical_query_cannot_claim_an_incarnation_or_state() {
        let reply = encode_reply(Query { service: 10, challenge: 1 }, 50, 1).unwrap();
        assert!(decode_query(&reply).is_err());
        let mut query = encode_query(10, 1).unwrap();
        let request_id_offset = 16;
        query[request_id_offset] ^= 1;
        assert!(decode_query(&query).is_err());
    }
}
