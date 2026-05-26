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

    pub const fn into_object_handle(self) -> kernel_object::api::session::ConsoleSessionHandle {
        kernel_object::api::session::ConsoleSessionHandle::from_raw(self.0)
    }

    pub const fn from_object_handle(
        handle: kernel_object::api::session::ConsoleSessionHandle,
    ) -> Self {
        Self(handle.raw())
    }

    pub const fn is_system(self) -> bool {
        self.0 == 0
    }

    pub(crate) const fn slot_index(self) -> Option<usize> {
        if self.0 == 0 {
            None
        } else {
            Some((self.0 as u32) as usize)
        }
    }

    #[cfg(test)]
    pub(crate) const fn for_tests(slot_index: u32, generation: u32) -> Self {
        Self(((generation as u64) << 32) | slot_index as u64)
    }
}

impl From<ConsoleSessionHandle> for kernel_object::api::session::ConsoleSessionHandle {
    fn from(value: ConsoleSessionHandle) -> Self {
        value.into_object_handle()
    }
}

impl From<kernel_object::api::session::ConsoleSessionHandle> for ConsoleSessionHandle {
    fn from(value: kernel_object::api::session::ConsoleSessionHandle) -> Self {
        Self::from_object_handle(value)
    }
}
