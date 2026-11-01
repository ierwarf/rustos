//! Generated-IDL (schema 523) slow-request authorization query: may the
//! caller's service act for `personality` on `namespace` for this class of slow
//! operation? The decision is rootd's (`personality::PolicySnapshot`); this
//! module is only the canonical wire. It is never used from syscall entry, IPC
//! or scheduling paths.
use crate::generated_idl::SLOW_REQUEST_AUTHORIZATION;
use crate::idl::{Builder, IdlError, Message};
use crate::personality::{Personality, SlowRequest};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Query {
    pub personality: Personality,
    pub request: SlowRequest,
    pub namespace: u64,
}

pub const fn personality_id(personality: Personality) -> u16 {
    match personality {
        Personality::Native => 1,
        Personality::Linux => 2,
        Personality::Windows => 3,
    }
}

pub const fn personality_from_id(id: u16) -> Option<Personality> {
    match id {
        1 => Some(Personality::Native),
        2 => Some(Personality::Linux),
        3 => Some(Personality::Windows),
        _ => None,
    }
}

pub const fn request_id(request: SlowRequest) -> u16 {
    match request {
        SlowRequest::ExecutableImage => 1,
        SlowRequest::FileAssociation => 2,
        SlowRequest::MountPolicy => 3,
        SlowRequest::NetworkPolicy => 4,
        SlowRequest::CredentialPolicy => 5,
        SlowRequest::NamespacePolicy => 6,
        SlowRequest::CompatibilityProfile => 7,
    }
}

pub const fn request_from_id(id: u16) -> Option<SlowRequest> {
    match id {
        1 => Some(SlowRequest::ExecutableImage),
        2 => Some(SlowRequest::FileAssociation),
        3 => Some(SlowRequest::MountPolicy),
        4 => Some(SlowRequest::NetworkPolicy),
        5 => Some(SlowRequest::CredentialPolicy),
        6 => Some(SlowRequest::NamespacePolicy),
        7 => Some(SlowRequest::CompatibilityProfile),
        _ => None,
    }
}

pub fn encode(query: Query, request_id_value: u64) -> Result<Vec<u8>, IdlError> {
    if query.namespace == 0 || request_id_value == 0 {
        return Err(IdlError::Malformed);
    }
    let mut b = Builder::new(SLOW_REQUEST_AUTHORIZATION)?;
    b.field(1, &personality_id(query.personality).to_le_bytes())?;
    b.field(2, &request_id(query.request).to_le_bytes())?;
    b.field(3, &query.namespace.to_le_bytes())?;
    b.finish(request_id_value)
}

pub fn decode(bytes: &[u8]) -> Result<Query, IdlError> {
    let m = Message::decode(SLOW_REQUEST_AUTHORIZATION, bytes)?;
    let personality = personality_from_id(m.field(1).ok_or(IdlError::Missing)?.as_u16()?)
        .ok_or(IdlError::Malformed)?;
    let request = request_from_id(m.field(2).ok_or(IdlError::Missing)?.as_u16()?)
        .ok_or(IdlError::Malformed)?;
    let namespace = m.field(3).ok_or(IdlError::Missing)?.as_u64()?;
    if namespace == 0 || m.header().request_id == 0 {
        return Err(IdlError::Malformed);
    }
    Ok(Query { personality, request, namespace })
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: Query = Query {
        personality: Personality::Windows,
        request: SlowRequest::ExecutableImage,
        namespace: 1,
    };

    #[test]
    fn roundtrip_and_every_enum_value_has_one_stable_id() {
        let wire = encode(QUERY, 9).unwrap();
        assert_eq!(decode(&wire).unwrap(), QUERY);
        for id in 1..=3 {
            assert_eq!(personality_id(personality_from_id(id).unwrap()), id);
        }
        for id in 1..=7 {
            assert_eq!(request_id(request_from_id(id).unwrap()), id);
        }
        assert!(personality_from_id(0).is_none() && request_from_id(8).is_none());
    }

    #[test]
    fn zero_namespace_unknown_ids_and_truncation_fail_closed() {
        assert!(encode(Query { namespace: 0, ..QUERY }, 9).is_err());
        assert!(encode(QUERY, 0).is_err());
        let wire = encode(QUERY, 9).unwrap();
        assert!(decode(&wire[..wire.len() - 1]).is_err());
        assert!(decode(&[]).is_err());
    }
}
