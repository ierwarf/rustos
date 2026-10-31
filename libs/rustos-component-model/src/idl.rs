//! Canonical typed-field IDL codec for future Ring3 control interfaces.
//!
//! This validation is intentionally stricter than a generic TLV parser:
//! unknown tags, duplicates, out-of-order tags, noncanonical integers,
//! excessive body sizes and missing required fields all fail closed. Nothing
//! here replaces production `repr(C)` RustOS ABIs until migration gates pass.

use crate::wire::{Header, WireError, CURRENT_VERSION, MAX_BODY_BYTES};
use alloc::vec::Vec;

pub const FIELD_HEADER_LEN: usize = 4;
pub const MAX_FIELDS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Bytes,
    Utf8,
    Bool,
    U16,
    U32,
    U64,
}

impl FieldKind {
    const fn fixed_len(self) -> Option<usize> {
        match self {
            Self::Bool => Some(1),
            Self::U16 => Some(2),
            Self::U32 => Some(4),
            Self::U64 => Some(8),
            Self::Bytes | Self::Utf8 => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldSpec {
    pub tag: u16,
    pub kind: FieldKind,
    pub required: bool,
    pub max_len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdlError {
    Schema,
    Missing,
    Duplicate,
    Order,
    UnknownField,
    WrongType,
    TooLong,
    Malformed,
    EmptyText,
    OutOfMemory,
    Wire(WireError),
}

impl From<WireError> for IdlError {
    fn from(e: WireError) -> Self {
        Self::Wire(e)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Schema<'a> {
    pub opcode: u16,
    pub fields: &'a [FieldSpec],
}

#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    pub tag: u16,
    pub kind: FieldKind,
    pub bytes: &'a [u8],
}

impl<'a> Field<'a> {
    pub fn as_u64(self) -> Result<u64, IdlError> {
        if self.kind != FieldKind::U64 {
            return Err(IdlError::WrongType);
        }
        let arr: [u8; 8] = self.bytes.try_into().map_err(|_| IdlError::Malformed)?;
        Ok(u64::from_le_bytes(arr))
    }
    pub fn as_u32(self) -> Result<u32, IdlError> {
        if self.kind != FieldKind::U32 {
            return Err(IdlError::WrongType);
        }
        let arr: [u8; 4] = self.bytes.try_into().map_err(|_| IdlError::Malformed)?;
        Ok(u32::from_le_bytes(arr))
    }
    pub fn as_u16(self) -> Result<u16, IdlError> {
        if self.kind != FieldKind::U16 {
            return Err(IdlError::WrongType);
        }
        let arr: [u8; 2] = self.bytes.try_into().map_err(|_| IdlError::Malformed)?;
        Ok(u16::from_le_bytes(arr))
    }
    pub fn as_bool(self) -> Result<bool, IdlError> {
        if self.kind != FieldKind::Bool {
            return Err(IdlError::WrongType);
        }
        match self.bytes {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(IdlError::Malformed),
        }
    }
    pub fn as_str(self) -> Result<&'a str, IdlError> {
        if self.kind != FieldKind::Utf8 {
            return Err(IdlError::WrongType);
        }
        core::str::from_utf8(self.bytes).map_err(|_| IdlError::Malformed)
    }
}

impl<'a> Schema<'a> {
    pub fn validate(self) -> Result<Self, IdlError> {
        if self.fields.len() > MAX_FIELDS {
            return Err(IdlError::Schema);
        }
        let mut previous = 0;
        for (index, field) in self.fields.iter().enumerate() {
            if field.tag == 0
                || (index != 0 && field.tag <= previous)
                || field.max_len > MAX_BODY_BYTES
            {
                return Err(IdlError::Schema);
            }
            if let Some(len) = field.kind.fixed_len() {
                if field.max_len != len {
                    return Err(IdlError::Schema);
                }
            }
            previous = field.tag;
        }
        Ok(self)
    }

    fn specification(self, tag: u16) -> Option<(usize, FieldSpec)> {
        self.fields
            .iter()
            .copied()
            .enumerate()
            .find(|(_, f)| f.tag == tag)
    }
}

fn validate_field(spec: FieldSpec, value: &[u8]) -> Result<(), IdlError> {
    if value.len() > spec.max_len {
        return Err(IdlError::TooLong);
    }
    if let Some(expected) = spec.kind.fixed_len() {
        if value.len() != expected {
            return Err(IdlError::WrongType);
        }
    }
    match spec.kind {
        FieldKind::Bool if value != [0] && value != [1] => Err(IdlError::Malformed),
        FieldKind::Utf8 => {
            if value.is_empty() {
                return Err(IdlError::EmptyText);
            }
            let text = core::str::from_utf8(value).map_err(|_| IdlError::Malformed)?;
            if text.as_bytes().contains(&0) {
                return Err(IdlError::Malformed);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub struct Message<'a> {
    header: Header,
    schema: Schema<'a>,
    body: &'a [u8],
}

impl<'a> Message<'a> {
    pub fn decode(schema: Schema<'a>, wire: &'a [u8]) -> Result<Self, IdlError> {
        let schema = schema.validate()?;
        let (header, body) = Header::decode(wire)?;
        if header.version != CURRENT_VERSION || header.opcode != schema.opcode {
            return Err(IdlError::Wire(WireError::UnsupportedVersion));
        }
        // Ordered schema lets us validate exact field presence without
        // allocating a set or copying untrusted guest-supplied bytes.
        let mut cursor = 0;
        let mut previous = 0u16;
        let mut found = 0u64;
        while cursor < body.len() {
            if body.len() - cursor < FIELD_HEADER_LEN {
                return Err(IdlError::Malformed);
            }
            let tag = u16::from_le_bytes([body[cursor], body[cursor + 1]]);
            let len = u16::from_le_bytes([body[cursor + 2], body[cursor + 3]]) as usize;
            if tag == 0 {
                return Err(IdlError::UnknownField);
            }
            if tag <= previous {
                return Err(if tag == previous {
                    IdlError::Duplicate
                } else {
                    IdlError::Order
                });
            }
            cursor += FIELD_HEADER_LEN;
            let end = cursor.checked_add(len).ok_or(IdlError::TooLong)?;
            if end > body.len() {
                return Err(IdlError::Malformed);
            }
            let (index, spec) = schema.specification(tag).ok_or(IdlError::UnknownField)?;
            validate_field(spec, &body[cursor..end])?;
            found |= 1u64 << index;
            previous = tag;
            cursor = end;
        }
        for (index, spec) in schema.fields.iter().enumerate() {
            if spec.required && (found & (1u64 << index)) == 0 {
                return Err(IdlError::Missing);
            }
        }
        Ok(Self {
            header,
            schema,
            body,
        })
    }

    pub fn header(&self) -> Header {
        self.header
    }

    pub fn field(&self, tag: u16) -> Option<Field<'a>> {
        let (_, spec) = self.schema.specification(tag)?;
        let mut cursor = 0;
        while cursor < self.body.len() {
            let current = u16::from_le_bytes([self.body[cursor], self.body[cursor + 1]]);
            let len = u16::from_le_bytes([self.body[cursor + 2], self.body[cursor + 3]]) as usize;
            cursor += FIELD_HEADER_LEN;
            if current == tag {
                return Some(Field {
                    tag,
                    kind: spec.kind,
                    bytes: &self.body[cursor..cursor + len],
                });
            }
            if current > tag {
                return None;
            }
            cursor += len;
        }
        None
    }
}

pub struct Builder<'a> {
    schema: Schema<'a>,
    body: Vec<u8>,
    last_tag: u16,
    present: u64,
}

impl<'a> Builder<'a> {
    pub fn new(schema: Schema<'a>) -> Result<Self, IdlError> {
        Ok(Self {
            schema: schema.validate()?,
            body: Vec::new(),
            last_tag: 0,
            present: 0,
        })
    }

    pub fn field(&mut self, tag: u16, value: &[u8]) -> Result<(), IdlError> {
        let (index, spec) = self
            .schema
            .specification(tag)
            .ok_or(IdlError::UnknownField)?;
        if tag <= self.last_tag {
            return Err(if tag == self.last_tag {
                IdlError::Duplicate
            } else {
                IdlError::Order
            });
        }
        validate_field(spec, value)?;
        let len = u16::try_from(value.len()).map_err(|_| IdlError::TooLong)?;
        let added = FIELD_HEADER_LEN
            .checked_add(value.len())
            .ok_or(IdlError::TooLong)?;
        let total = self
            .body
            .len()
            .checked_add(added)
            .ok_or(IdlError::TooLong)?;
        if total > MAX_BODY_BYTES {
            return Err(IdlError::TooLong);
        }
        self.body
            .try_reserve(added)
            .map_err(|_| IdlError::OutOfMemory)?;
        self.body.extend_from_slice(&tag.to_le_bytes());
        self.body.extend_from_slice(&len.to_le_bytes());
        self.body.extend_from_slice(value);
        self.last_tag = tag;
        self.present |= 1u64 << index;
        Ok(())
    }

    pub fn finish(self, request_id: u64) -> Result<Vec<u8>, IdlError> {
        for (index, spec) in self.schema.fields.iter().enumerate() {
            if spec.required && (self.present & (1u64 << index)) == 0 {
                return Err(IdlError::Missing);
            }
        }
        let header = Header {
            version: CURRENT_VERSION,
            opcode: self.schema.opcode,
            body_len: self.body.len() as u32,
            request_id,
        };
        header.encode(&self.body).map_err(IdlError::Wire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELDS: [FieldSpec; 3] = [
        FieldSpec {
            tag: 1,
            kind: FieldKind::U64,
            required: true,
            max_len: 8,
        },
        FieldSpec {
            tag: 2,
            kind: FieldKind::Utf8,
            required: true,
            max_len: 32,
        },
        FieldSpec {
            tag: 3,
            kind: FieldKind::Bool,
            required: false,
            max_len: 1,
        },
    ];
    const S: Schema<'static> = Schema {
        opcode: 7,
        fields: &FIELDS,
    };

    #[test]
    fn canonical_encoding_roundtrip_is_strict() {
        let mut b = Builder::new(S).unwrap();
        b.field(1, &12u64.to_le_bytes()).unwrap();
        b.field(2, b"hello").unwrap();
        b.field(3, &[1]).unwrap();
        let bytes = b.finish(98).unwrap();
        let msg = Message::decode(S, &bytes).unwrap();
        assert_eq!(msg.header().request_id, 98);
        assert_eq!(msg.field(1).unwrap().as_u64(), Ok(12));
        assert_eq!(msg.field(2).unwrap().as_str(), Ok("hello"));
        assert_eq!(msg.field(3).unwrap().as_bool(), Ok(true));
        let mut invalid = bytes;
        invalid[crate::wire::HEADER_SIZE] = 0;
        assert_eq!(
            Message::decode(S, &invalid).err(),
            Some(IdlError::UnknownField)
        );
    }

    #[test]
    fn missing_duplicate_and_out_of_order_are_refused() {
        let mut b = Builder::new(S).unwrap();
        assert_eq!(b.field(3, &[2]), Err(IdlError::Malformed));
        b.field(1, &7u64.to_le_bytes()).unwrap();
        assert_eq!(b.field(1, &7u64.to_le_bytes()), Err(IdlError::Duplicate));
        assert_eq!(b.finish(2), Err(IdlError::Missing));
        let mut b = Builder::new(S).unwrap();
        b.field(2, b"x").unwrap();
        assert_eq!(b.field(1, &1u64.to_le_bytes()), Err(IdlError::Order));
    }

    #[test]
    fn schemas_must_be_sorted_and_fixed_widths_exact() {
        const BAD: [FieldSpec; 2] = [
            FieldSpec {
                tag: 2,
                kind: FieldKind::U32,
                required: true,
                max_len: 4,
            },
            FieldSpec {
                tag: 1,
                kind: FieldKind::U64,
                required: true,
                max_len: 8,
            },
        ];
        assert!(Builder::new(Schema {
            opcode: 1,
            fields: &BAD
        })
        .is_err());
        const BAD_WIDTH: [FieldSpec; 1] = [FieldSpec {
            tag: 1,
            kind: FieldKind::U32,
            required: false,
            max_len: 8,
        }];
        assert!(Builder::new(Schema {
            opcode: 1,
            fields: &BAD_WIDTH
        })
        .is_err());
    }
}
