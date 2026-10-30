//! Stable names and generation-bound component identities.
//! Slot indices, PIDs and Job IDs are not interchangeable.

use core::fmt;

pub const MAX_MONIKER_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonikerError {
    Empty,
    TooLong,
    InvalidCharacter,
    Reserved,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Moniker {
    bytes: [u8; MAX_MONIKER_BYTES],
    len: u8,
}

impl Moniker {
    /// Accepts a single *component* name, not a filesystem path.
    /// A slash, NUL, colon, space or a dot-only path component is forbidden.
    pub fn new(text: &str) -> Result<Self, MonikerError> {
        let raw = text.as_bytes();
        if raw.is_empty() {
            return Err(MonikerError::Empty);
        }
        if raw.len() > MAX_MONIKER_BYTES {
            return Err(MonikerError::TooLong);
        }
        if text == "." || text == ".." {
            return Err(MonikerError::Reserved);
        }
        if !raw
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-' || *c == b'.')
        {
            return Err(MonikerError::InvalidCharacter);
        }
        let mut bytes = [0; MAX_MONIKER_BYTES];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Self {
            bytes,
            len: raw.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        // Constructor accepts only ASCII characters.
        core::str::from_utf8(&self.bytes[..self.len as usize])
            .expect("validated ASCII component name")
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for Moniker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Moniker").field(&self.as_str()).finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentId {
    slot: u32,
    generation: u32,
}

impl ComponentId {
    pub fn new(slot: u32, generation: u32) -> Option<Self> {
        if generation == 0 {
            None
        } else {
            Some(Self { slot, generation })
        }
    }

    pub const fn slot(self) -> usize {
        self.slot as usize
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }

    pub const fn packed(self) -> u64 {
        (self.generation as u64) << 32 | (self.slot as u64)
    }

    pub fn unpack(value: u64) -> Option<Self> {
        Self::new(value as u32, (value >> 32) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_refuse_path_injection_and_unknown_bytes() {
        for bad in [
            "", ".", "..", "../root", "a/b", "a\\b", "a:b", "a\0b", "a b", "한글",
        ] {
            assert!(Moniker::new(bad).is_err(), "{bad:?}");
        }
        let n = Moniker::new("compositor.v2").unwrap();
        assert_eq!(n.as_str(), "compositor.v2");
        assert_eq!(n.len(), 13);
    }

    #[test]
    fn id_roundtrip_requires_nonzero_generation() {
        assert_eq!(ComponentId::unpack(0), None);
        let id = ComponentId::new(13, u32::MAX).unwrap();
        assert_eq!(ComponentId::unpack(id.packed()), Some(id));
        assert_eq!(id.slot(), 13);
    }
}
