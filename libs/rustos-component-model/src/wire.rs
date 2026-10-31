//! Versioned bounded envelope validation for a future generated IDL.
//!
//! This is NOT a wire-format switch in the running system. Existing RustOS
//! request/reply structures retain their ABI while the generator is adopted.
//! Avoid unchecked pointer casts and host-endian repr(C) assumptions here.

use alloc::vec::Vec;

pub const HEADER_SIZE: usize = 24;
pub const MAGIC: u32 = 0x504d_4352; // "RCMP" on the wire in LE
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const CURRENT_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u16,
    pub opcode: u16,
    pub body_len: u32,
    pub request_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    InvalidMagic,
    UnsupportedVersion,
    InvalidReserved,
    InvalidLength,
    ExcessData,
    Allocation,
}

impl Header {
    pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), WireError> {
        if bytes.len() < HEADER_SIZE {
            return Err(WireError::Truncated);
        }
        if u32::from_le_bytes(bytes[0..4].try_into().expect("fixed range")) != MAGIC {
            return Err(WireError::InvalidMagic);
        }
        let version = u16::from_le_bytes(bytes[4..6].try_into().expect("fixed range"));
        if version != CURRENT_VERSION {
            return Err(WireError::UnsupportedVersion);
        }
        let opcode = u16::from_le_bytes(bytes[6..8].try_into().expect("fixed range"));
        let body_len = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed range"));
        if body_len as usize > MAX_BODY_BYTES {
            return Err(WireError::InvalidLength);
        }
        if bytes[12..16] != [0, 0, 0, 0] {
            return Err(WireError::InvalidReserved);
        }
        let request_id = u64::from_le_bytes(bytes[16..24].try_into().expect("fixed range"));
        let total = HEADER_SIZE
            .checked_add(body_len as usize)
            .ok_or(WireError::InvalidLength)?;
        if bytes.len() < total {
            return Err(WireError::Truncated);
        }
        if bytes.len() > total {
            return Err(WireError::ExcessData);
        }
        Ok((
            Self {
                version,
                opcode,
                body_len,
                request_id,
            },
            &bytes[HEADER_SIZE..],
        ))
    }

    pub fn encode(self, body: &[u8]) -> Result<Vec<u8>, WireError> {
        if self.version != CURRENT_VERSION {
            return Err(WireError::UnsupportedVersion);
        }
        if body.len() > MAX_BODY_BYTES || body.len() != self.body_len as usize {
            return Err(WireError::InvalidLength);
        }
        let mut out = Vec::new();
        out.try_reserve_exact(HEADER_SIZE + body.len())
            .map_err(|_| WireError::Allocation)?;
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&self.opcode.to_le_bytes());
        out.extend_from_slice(&self.body_len.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&self.request_id.to_le_bytes());
        out.extend_from_slice(body);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_wire_roundtrip_rejects_padded_or_malformed_frames() {
        let h = Header {
            version: 1,
            opcode: 15,
            body_len: 3,
            request_id: 12,
        };
        let bytes = h.encode(&[1, 2, 3]).unwrap();
        let (parsed, body) = Header::decode(&bytes).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(body, &[1, 2, 3]);
        let mut padded = bytes.clone();
        padded.push(0);
        assert_eq!(Header::decode(&padded), Err(WireError::ExcessData));
        let mut wrong = bytes;
        wrong[12] = 1;
        assert_eq!(Header::decode(&wrong), Err(WireError::InvalidReserved));
    }
}
