#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ConsoleSessionHandle(u64);

impl ConsoleSessionHandle {
    pub const SYSTEM: Self = Self(0);

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn is_system(self) -> bool {
        self.0 == 0
    }

    pub const fn slot_index(self) -> Option<usize> {
        if self.0 == 0 {
            None
        } else {
            Some((self.0 as u32) as usize)
        }
    }

    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub const fn from_parts(slot_index: u32, generation: u32) -> Self {
        Self(((generation as u64) << 32) | slot_index as u64)
    }

    #[cfg(test)]
    pub(crate) const fn for_tests(slot_index: u32, generation: u32) -> Self {
        Self::from_parts(slot_index, generation)
    }
}
