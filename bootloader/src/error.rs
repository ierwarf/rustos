use uefi::prelude::*;

#[derive(Debug, Clone, Copy)]
pub enum BootError {
    OpenFileSystem(Status),
    ReadKernel(Status),
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
            | Self::ReadKernel(status)
            | Self::SegmentAlloc(status)
            | Self::Graphics(status)
            | Self::BootInfoAlloc(status) => status,
            Self::InvalidElf(_) | Self::GraphicsMode(_) => Status::LOAD_ERROR,
        }
    }
}
