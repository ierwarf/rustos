use uefi::prelude::*;

#[derive(Debug, Clone, Copy)]
pub enum BootError {
    OpenFileSystem(Status),
    ReadKernel(Status),
    CacheBootVolume(Status),
    InvalidElf(&'static str),
    InvalidBootInfo(&'static str),
    SegmentAlloc(Status),
    Graphics(Status),
    GraphicsMode(&'static str),
    BootInfoAlloc(Status),
    BootMemoryMapAlloc(Status),
}

impl BootError {
    pub const fn status(self) -> Status {
        match self {
            Self::OpenFileSystem(status)
            | Self::ReadKernel(status)
            | Self::CacheBootVolume(status)
            | Self::SegmentAlloc(status)
            | Self::Graphics(status)
            | Self::BootInfoAlloc(status)
            | Self::BootMemoryMapAlloc(status) => status,
            Self::InvalidElf(_) | Self::InvalidBootInfo(_) | Self::GraphicsMode(_) => {
                Status::LOAD_ERROR
            }
        }
    }

    pub const fn summary(self) -> &'static str {
        match self {
            Self::OpenFileSystem(_) => "failed to open the boot filesystem",
            Self::ReadKernel(_) => "failed to read nucleus.elf",
            Self::CacheBootVolume(_) => "failed to cache the boot volume",
            Self::InvalidElf(_) => "invalid ELF image",
            Self::InvalidBootInfo(_) => "invalid boot info",
            Self::SegmentAlloc(_) => "failed to reserve the requested load range",
            Self::Graphics(_) => "graphics initialization failed",
            Self::GraphicsMode(_) => "unsupported graphics mode",
            Self::BootInfoAlloc(_) => "failed to allocate boot info",
            Self::BootMemoryMapAlloc(_) => "failed to allocate boot memory map",
        }
    }
}
