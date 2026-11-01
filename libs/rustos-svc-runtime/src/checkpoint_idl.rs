//! Canonical generated-IDL (schema 521) framing of one service-checkpoint
//! record on the rootd supervisor channel. The kernel journal keeps its own
//! `repr(C)` ABI; this is only the Ring3 service <-> rootd transport.
//!
//! A SCAN reply carries zero or more records as `[u16 len][IDL message]`
//! entries. Decoding rejects truncation, trailing bytes, unknown versions,
//! `value_len` disagreement and any non-canonical field.
use alloc::vec::Vec;
use rustos_component_model::generated_idl::SERVICE_CHECKPOINT_RECORD;
use rustos_component_model::idl::{Builder, IdlError, Message};
use rustos_user_abi::syscall::{
    ServiceCheckpointRecordWire, SERVICE_CHECKPOINT_ABI_VERSION,
    SERVICE_CHECKPOINT_VALUE_CAPACITY,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Idl(IdlError),
    Version,
    Length,
    Trailing,
}
impl From<IdlError> for Error {
    fn from(e: IdlError) -> Self { Self::Idl(e) }
}

pub fn encode(record: &ServiceCheckpointRecordWire) -> Result<Vec<u8>, Error> {
    if record.version != SERVICE_CHECKPOINT_ABI_VERSION {
        return Err(Error::Version);
    }
    let value_len = record.value_len as usize;
    if value_len > SERVICE_CHECKPOINT_VALUE_CAPACITY {
        return Err(Error::Length);
    }
    let mut b = Builder::new(SERVICE_CHECKPOINT_RECORD)?;
    b.field(1, &record.version.to_le_bytes())?;
    b.field(2, &record.flags.to_le_bytes())?;
    b.field(3, &record.key_hi.to_le_bytes())?;
    b.field(4, &record.key_lo.to_le_bytes())?;
    b.field(5, &record.parent_hi.to_le_bytes())?;
    b.field(6, &record.parent_lo.to_le_bytes())?;
    b.field(7, &record.operation_hi.to_le_bytes())?;
    b.field(8, &record.operation_lo.to_le_bytes())?;
    b.field(9, &record.revision.to_le_bytes())?;
    if value_len != 0 {
        b.field(10, &record.value[..value_len])?;
    }
    Ok(b.finish(record.revision)?)
}

pub fn decode(bytes: &[u8]) -> Result<ServiceCheckpointRecordWire, Error> {
    let m = Message::decode(SERVICE_CHECKPOINT_RECORD, bytes)?;
    let u64f = |tag: u16| -> Result<u64, Error> {
        Ok(m.field(tag).ok_or(IdlError::Missing)?.as_u64()?)
    };
    let u16f = |tag: u16| -> Result<u16, Error> {
        Ok(m.field(tag).ok_or(IdlError::Missing)?.as_u16()?)
    };
    let version = u16f(1)?;
    if version != SERVICE_CHECKPOINT_ABI_VERSION {
        return Err(Error::Version);
    }
    let revision = u64f(9)?;
    if m.header().request_id != revision {
        return Err(Error::Idl(IdlError::Malformed));
    }
    let mut record = ServiceCheckpointRecordWire {
        version,
        flags: u16f(2)?,
        key_hi: u64f(3)?,
        key_lo: u64f(4)?,
        parent_hi: u64f(5)?,
        parent_lo: u64f(6)?,
        operation_hi: u64f(7)?,
        operation_lo: u64f(8)?,
        revision,
        ..ServiceCheckpointRecordWire::default()
    };
    if let Some(value) = m.field(10) {
        if value.bytes.len() > SERVICE_CHECKPOINT_VALUE_CAPACITY {
            return Err(Error::Length);
        }
        record.value[..value.bytes.len()].copy_from_slice(value.bytes);
        record.value_len = value.bytes.len() as u32;
    }
    Ok(record)
}

/// Append one length-prefixed record to a SCAN payload buffer.
pub fn append_framed(out: &mut Vec<u8>, record: &ServiceCheckpointRecordWire) -> Result<(), Error> {
    let body = encode(record)?;
    let len = u16::try_from(body.len()).map_err(|_| Error::Length)?;
    out.try_reserve(2 + body.len()).map_err(|_| Error::Idl(IdlError::OutOfMemory))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(())
}

/// Decode every framed record of a SCAN reply payload.
pub fn decode_framed(mut bytes: &[u8]) -> Result<Vec<ServiceCheckpointRecordWire>, Error> {
    let mut records = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 2 { return Err(Error::Length); }
        let len = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        bytes = &bytes[2..];
        if len == 0 || bytes.len() < len { return Err(Error::Length); }
        records.try_reserve(1).map_err(|_| Error::Idl(IdlError::OutOfMemory))?;
        records.push(decode(&bytes[..len])?);
        bytes = &bytes[len..];
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(len: usize) -> ServiceCheckpointRecordWire {
        let mut r = ServiceCheckpointRecordWire {
            flags: 1, key_hi: 2, key_lo: 3, parent_hi: 4, parent_lo: 5,
            operation_hi: 6, operation_lo: 7, revision: 9, value_len: len as u32,
            ..ServiceCheckpointRecordWire::default()
        };
        for (i, b) in r.value[..len].iter_mut().enumerate() { *b = i as u8 + 1; }
        r
    }
    #[test]
    fn roundtrip_with_and_without_value() {
        for len in [0, 1, SERVICE_CHECKPOINT_VALUE_CAPACITY] {
            let r = sample(len);
            assert_eq!(decode(&encode(&r).unwrap()), Ok(r));
        }
    }
    #[test]
    fn malformed_records_fail_closed() {
        let good = encode(&sample(3)).unwrap();
        let mut longer = good.clone(); longer.push(0);
        assert!(decode(&longer).is_err());
        assert!(decode(&good[..good.len() - 1]).is_err());
        let mut skew = good.clone(); skew[16] ^= 1; // header request id
        assert!(decode(&skew).is_err());
        let mut bad_version = sample(0);
        bad_version.version = 2;
        assert_eq!(encode(&bad_version), Err(Error::Version));
        let mut oversize = sample(0);
        oversize.value_len = SERVICE_CHECKPOINT_VALUE_CAPACITY as u32 + 1;
        assert_eq!(encode(&oversize), Err(Error::Length));
        // A legacy 128-byte repr(C) record is not an IDL message.
        assert!(decode(&[0u8; 128]).is_err());
    }
    #[test]
    fn scan_framing_roundtrips_and_rejects_truncation() {
        let a = sample(4);
        let mut b = sample(0);
        b.key_lo = 99; b.revision = 10;
        let mut payload = Vec::new();
        append_framed(&mut payload, &a).unwrap();
        append_framed(&mut payload, &b).unwrap();
        assert_eq!(decode_framed(&payload).unwrap(), alloc::vec![a, b]);
        assert_eq!(decode_framed(&[]).unwrap().len(), 0);
        assert!(decode_framed(&payload[..payload.len() - 1]).is_err());
        assert!(decode_framed(&[1]).is_err());
        assert!(decode_framed(&[0, 0]).is_err());
    }
}
