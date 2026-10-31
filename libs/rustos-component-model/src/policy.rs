//! Qualitative permission ceilings do not mint positive authority.
//!
//! Rootd can narrow this policy; the kernel must still check the original
//! descriptor identity, generation and operation-specific handle rights.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyError {
    OutOfRange,
    WouldWiden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobCeiling {
    words: [u64; 4],
}

impl JobCeiling {
    pub const NONE: Self = Self { words: [0; 4] };
    pub const ALL: Self = Self {
        words: [u64::MAX; 4],
    };

    pub const fn words(self) -> [u64; 4] {
        self.words
    }

    pub const fn from_words(words: [u64; 4]) -> Self {
        Self { words }
    }

    pub fn permits(self, op: Operation) -> bool {
        let index = op.0 as usize;
        index < 256 && self.words[index / 64] & (1 << (index % 64)) != 0
    }

    pub fn with(self, op: Operation, allowed: bool) -> Result<Self, PolicyError> {
        let index = op.0 as usize;
        if index >= 256 {
            return Err(PolicyError::OutOfRange);
        }
        let mut next = self;
        let bit = 1u64 << (index % 64);
        if allowed {
            next.words[index / 64] |= bit;
        } else {
            next.words[index / 64] &= !bit;
        }
        Ok(next)
    }

    pub const fn intersect(self, ancestor: Self) -> Self {
        Self {
            words: [
                self.words[0] & ancestor.words[0],
                self.words[1] & ancestor.words[1],
                self.words[2] & ancestor.words[2],
                self.words[3] & ancestor.words[3],
            ],
        }
    }

    pub const fn subset_of(self, parent: Self) -> bool {
        ((self.words[0] & !parent.words[0])
            | (self.words[1] & !parent.words[1])
            | (self.words[2] & !parent.words[2])
            | (self.words[3] & !parent.words[3]))
            == 0
    }

    pub fn narrow(self, proposed: Self) -> Result<Self, PolicyError> {
        if proposed.subset_of(self) {
            Ok(proposed)
        } else {
            Err(PolicyError::WouldWiden)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_words_are_checked_and_invalid_ops_are_denied() {
        let mut mask = JobCeiling::NONE;
        for n in [0, 63, 64, 127, 128, 191, 192, 255] {
            mask = mask.with(Operation(n), true).unwrap();
            assert!(mask.permits(Operation(n)));
        }
        assert!(!mask.permits(Operation(256)));
        assert!(!mask.permits(Operation(u16::MAX)));
        assert_eq!(
            mask.with(Operation(256), true),
            Err(PolicyError::OutOfRange)
        );
        assert_eq!(mask.narrow(JobCeiling::ALL), Err(PolicyError::WouldWiden));
        assert_eq!(mask.intersect(JobCeiling::NONE), JobCeiling::NONE);
    }
}
