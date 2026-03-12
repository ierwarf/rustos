use uefi::prelude::*;

#[derive(Debug, Clone, Copy)]
pub enum BootError {
    OpenFileSystem(Status),
    ReadStage(Status),
    InvalidElf(&'static str),
    SegmentAlloc(Status),
    Graphics(Status),
    GraphicsMode(&'static str),
    BootInfoAlloc(Status),
}

impl BootError {
    pub const fn status(self) -> Status {
        match self {
            Self::OpenFileSystem(status)
            | Self::ReadStage(status)
            | Self::SegmentAlloc(status)
            | Self::Graphics(status)
            | Self::BootInfoAlloc(status) => status,
            Self::InvalidElf(_) | Self::GraphicsMode(_) => Status::LOAD_ERROR,
        }
    }

    pub const fn summary(self) -> &'static str {
        match self {
            Self::OpenFileSystem(_) => "failed to open the boot filesystem",
            Self::ReadStage(_) => "failed to read prekernel.elf",
            Self::InvalidElf(_) => "invalid ELF image",
            Self::SegmentAlloc(_) => "failed to reserve the requested load range",
            Self::Graphics(_) => "graphics initialization failed",
            Self::GraphicsMode(_) => "unsupported graphics mode",
            Self::BootInfoAlloc(_) => "failed to allocate boot info",
        }
    }
}
